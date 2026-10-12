//! Crate-level tests: the provider contract against envfile and the mock, the resolver's
//! lifecycle, and the envfile guardrails and check. Everything lives in temp dirs; git runs
//! with the user's configuration shut out.

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use crate::contract::{self, Harness, Seed};
use crate::git::testing::{init, isolated};
use crate::mock::MemoryProvider;
use crate::*;

// ---------------------------------------------------------------------------------------------
// Harnesses

struct EnvfileHarness {
    tmp: TempDir,
    git: Git,
    n: Cell<usize>,
}

impl EnvfileHarness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // Without git on the machine the guardrail treats everything as outside a repo.
        let git = isolated(tmp.path())
            .unwrap_or_else(|| Git::system().with_program(tmp.path().join("no-git")));
        Self {
            tmp,
            git,
            n: Cell::new(0),
        }
    }
}

impl Harness for EnvfileHarness {
    fn provider(&self, seed: &Seed) -> Box<dyn SecretProvider> {
        self.n.set(self.n.get() + 1);
        let dir = self.tmp.path().join(format!("p{}", self.n.get()));
        let path = dir.join("secrets.env");
        let mut text = String::from("# seeded by the contract\n");
        for (k, v) in &seed.values {
            text = dotenv::upsert(&text, k, v);
        }
        fsutil::write_private(&path, text.as_bytes()).unwrap();
        for (rel, bytes) in &seed.files {
            fsutil::write_private(&dir.join("files").join(rel), bytes).unwrap();
        }
        Box::new(EnvfileProvider::new(path, None).with_git(self.git.clone()))
    }

    fn value_ref(&self, key: &str) -> SecretRef {
        SecretRef::envfile(key).unwrap()
    }

    fn file_ref(&self, path: &str) -> Option<SecretRef> {
        Some(SecretRef::envfile_file(path).unwrap())
    }
}

struct MockHarness {
    caps: Caps,
}

impl Harness for MockHarness {
    fn provider(&self, seed: &Seed) -> Box<dyn SecretProvider> {
        let mut p = MemoryProvider::new("mock").with_caps(self.caps);
        for (k, v) in &seed.values {
            p = p.with_value(k, v);
        }
        for (path, bytes) in &seed.files {
            p = p.with_file(path, bytes);
        }
        p.boxed()
    }

    fn value_ref(&self, key: &str) -> SecretRef {
        SecretRef::parse(&format!("mock:{key}")).unwrap()
    }

    fn file_ref(&self, path: &str) -> Option<SecretRef> {
        Some(SecretRef::parse(&format!("mock-file:{path}")).unwrap())
    }
}

#[test]
fn contract_envfile() {
    contract::run(&EnvfileHarness::new());
}

#[test]
fn contract_mock() {
    contract::run(&MockHarness {
        caps: MemoryProvider::new("mock").capabilities(),
    });
}

#[test]
fn contract_mock_read_only_unbatched_without_files() {
    contract::run(&MockHarness {
        caps: Caps {
            read: true,
            ..Caps::default()
        },
    });
}

// ---------------------------------------------------------------------------------------------
// SecretValue

#[test]
fn secret_value_hides_itself() {
    let v = SecretValue::from("hunter2".to_string());
    assert_eq!(format!("{v:?}"), "***");
    assert_eq!(v.expose(), b"hunter2");
    assert_eq!(v.expose_str(), Some("hunter2"));
    assert_eq!(v.len(), 7);
    assert!(SecretValue::new(vec![0xff]).expose_str().is_none());
    assert!(SecretValue::new(Vec::new()).is_empty());
}

// ---------------------------------------------------------------------------------------------
// Resolver

/// A tty that records suspend/resume calls.
#[derive(Default)]
struct RecordingTty(Vec<String>);

impl Interactive for RecordingTty {
    fn is_interactive(&self) -> bool {
        true
    }
    fn suspend(&mut self, reason: &str) {
        self.0.push(format!("suspend: {reason}"));
    }
    fn resume(&mut self) {
        self.0.push("resume".into());
    }
}

fn config(yaml: &str) -> SecretsConfig {
    SecretsConfig::from_yaml(yaml, Path::new("secrets.yml")).unwrap()
}

fn registry_with(providers: &[&MemoryProvider]) -> Registry {
    let mut reg = Registry::new();
    for p in providers {
        let p = (*p).clone();
        reg.register(p.scheme(), move |_| Ok(p.boxed()));
    }
    reg
}

fn val<'a>(r: &'a Resolved, name: &str) -> &'a str {
    r.get(name).and_then(SecretValue::expose_str).unwrap()
}

#[test]
fn resolver_groups_by_provider_and_unlocks_once() {
    let a = MemoryProvider::new("a")
        .with_value("X", "ax")
        .with_value("Y", "ay");
    let b = MemoryProvider::new("b")
        .with_value("Z", "bz")
        .with_caps(Caps {
            read: true,
            needs_unlock: true,
            batch: true,
            ..Caps::default()
        });
    let cfg = config(
        "default_provider: a\nsecrets:\n  x: { ref: X }\n  y: { ref: \"a:Y\" }\n  y2: { ref: \"a:Y\" }\n  z: { ref: \"b:Z\" }\n",
    );
    let mut resolver = Resolver::new(registry_with(&[&a, &b]), cfg);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    resolver.on_resolved(move |v| sink.lock().unwrap().push(v.expose().to_vec()));
    let mut tty = RecordingTty::default();

    let r = resolver
        .resolve(["z", "x", "y", "y2", "x"], &mut tty)
        .unwrap();
    assert_eq!(r.names().collect::<Vec<_>>(), ["x", "y", "y2", "z"]);
    assert_eq!(
        (val(&r, "x"), val(&r, "y"), val(&r, "y2"), val(&r, "z")),
        ("ax", "ay", "ay", "bz")
    );
    assert_eq!(
        (a.unlocks(), a.resolve_calls()),
        (1, 1),
        "one unlock, one batch for `a`"
    );
    assert_eq!((b.unlocks(), b.resolve_calls()), (1, 1));
    assert_eq!(
        tty.0,
        ["suspend: unlocking secrets provider `b`", "resume"],
        "only needs_unlock pauses"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        4,
        "every handed-out value reaches the hook"
    );

    // A second resolve in the same run does not unlock again.
    let r2 = resolver.resolve(["x"], &mut tty).unwrap();
    assert_eq!(val(&r2, "x"), "ax");
    assert_eq!((a.unlocks(), a.resolve_calls()), (1, 2));
}

#[test]
fn resolver_without_batch_calls_once_per_ref() {
    let a = MemoryProvider::new("a")
        .with_value("X", "1")
        .with_value("Y", "2")
        .with_caps(Caps {
            read: true,
            ..Caps::default()
        });
    let cfg = config("default_provider: a\nsecrets:\n  x: { ref: X }\n  y: { ref: Y }\n");
    let mut resolver = Resolver::new(registry_with(&[&a]), cfg);
    resolver.resolve(["x", "y"], &mut NonInteractive).unwrap();
    assert_eq!(a.resolve_calls(), 2);
}

#[test]
fn resolver_reports_every_missing_name_at_once() {
    let a = MemoryProvider::new("a").with_value("X", "ax");
    let cfg = config(
        "default_provider: a\nsecrets:\n  x: { ref: X }\n  gone: { ref: GONE }\n  also_gone: { ref: \"a:ALSO\" }\n  nowhere: { ref: \"zz:K\" }\n",
    );
    let mut resolver = Resolver::new(registry_with(&[&a]), cfg);
    let err = resolver
        .resolve(
            ["x", "gone", "also_gone", "nowhere", "undeclared"],
            &mut NonInteractive,
        )
        .unwrap_err();
    assert_eq!(
        err.names().collect::<Vec<_>>(),
        ["also_gone", "gone", "nowhere", "undeclared"]
    );
    let msg = err.to_string();
    assert!(msg.starts_with("4 secrets could not be resolved:"), "{msg}");
    assert!(
        msg.contains("gone: `a:GONE` not found in memory provider `a`"),
        "{msg}"
    );
    assert!(
        msg.contains("nowhere: provider `zz` is not a known provider"),
        "{msg}"
    );
    assert!(
        msg.contains("undeclared: not declared in secrets.yml"),
        "{msg}"
    );
    assert!(!msg.contains("ax"), "{msg}");
}

#[test]
fn fallback_applies_when_missing_or_unavailable() {
    let primary = MemoryProvider::new("p").with_value("HAS", "primary");
    let down = MemoryProvider::new("down")
        .with_value("K", "never")
        .unavailable();
    let backup = MemoryProvider::new("backup")
        .with_value("A", "backup-a")
        .with_value("B", "backup-b")
        .with_value("HAS", "backup-has");
    let cfg = config(
        r#"
default_provider: p
secrets:
  has:       { ref: HAS, fallback: "backup:HAS" }
  missing:   { ref: MISSING, fallback: "backup:A" }
  down:      { ref: "down:K", fallback: "backup:B" }
  both_gone: { ref: NOPE, fallback: "backup:NOPE" }
"#,
    );
    let mut resolver = Resolver::new(registry_with(&[&primary, &down, &backup]), cfg);
    let r = resolver
        .resolve(["has", "missing", "down"], &mut NonInteractive)
        .unwrap();
    assert_eq!(
        (val(&r, "has"), val(&r, "missing"), val(&r, "down")),
        ("primary", "backup-a", "backup-b")
    );
    assert_eq!(
        down.unlocks(),
        0,
        "an unavailable provider is never unlocked"
    );
    assert_eq!(backup.resolve_calls(), 1, "fallbacks are batched too");

    let err = resolver
        .resolve(["both_gone"], &mut NonInteractive)
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "1 secret could not be resolved:\n  both_gone: `p:NOPE` not found in memory provider `p`; fallback: `backup:NOPE` not found in memory provider `backup`"
    );
}

#[test]
fn provider_failure_does_not_fall_back() {
    let broken = MemoryProvider::new("broken")
        .with_value("K", "v")
        .failing_unlock("MFA declined");
    let backup = MemoryProvider::new("backup").with_value("K", "backup");
    let cfg = config("secrets:\n  k: { ref: \"broken:K\", fallback: \"backup:K\" }\n");
    let mut resolver = Resolver::new(registry_with(&[&broken, &backup]), cfg);
    let err = resolver.resolve(["k"], &mut NonInteractive).unwrap_err();
    assert!(
        err.to_string()
            .contains("provider `broken` failed: MFA declined"),
        "{err}"
    );
    assert_eq!(err.failures[0].fallback, None);
    assert_eq!(backup.resolve_calls(), 0);
    // The failure is remembered: no second unlock attempt in the same run.
    resolver.resolve(["k"], &mut NonInteractive).unwrap_err();
    assert_eq!(broken.unlocks(), 1);
}

#[test]
fn file_refs_need_file_support() {
    let nofiles = MemoryProvider::new("nf")
        .with_file("a.pem", b"x")
        .with_caps(Caps {
            read: true,
            batch: true,
            ..Caps::default()
        });
    let files = MemoryProvider::new("f").with_file("a.pem", b"pem");
    let cfg = config(
        "secrets:\n  cert: { ref: \"nf-file:a.pem\", fallback: \"f-file:a.pem\", kind: file }\n",
    );
    let mut resolver = Resolver::new(registry_with(&[&nofiles, &files]), cfg);
    let r = resolver.resolve(["cert"], &mut NonInteractive).unwrap();
    assert_eq!(r.get("cert").unwrap().expose(), b"pem");
    assert_eq!(nofiles.resolve_calls(), 0);
}

#[test]
fn bare_refs_follow_the_profile_override() {
    let a = MemoryProvider::new("a").with_value("TOKEN", "from-a");
    let b = MemoryProvider::new("b").with_value("TOKEN", "from-b");
    let base = config("default_provider: a\nsecrets:\n  token: { ref: TOKEN }\n");
    let mut r = Resolver::new(registry_with(&[&a, &b]), base.clone());
    assert_eq!(
        val(&r.resolve(["token"], &mut NonInteractive).unwrap(), "token"),
        "from-a"
    );
    let mut r = Resolver::new(
        registry_with(&[&a, &b]),
        base.clone().with_provider_override(Some("b")),
    );
    assert_eq!(
        val(&r.resolve(["token"], &mut NonInteractive).unwrap(), "token"),
        "from-b"
    );
    let mut r = Resolver::new(
        registry_with(&[&a, &b]),
        base.with_provider_override(Some("lastpass")),
    );
    let err = r.resolve(["token"], &mut NonInteractive).unwrap_err();
    assert!(err.to_string().contains("full ref"), "{err}");
}

#[cfg(feature = "secrets-lastpass")]
#[test]
fn lastpass_without_lpass_is_unavailable_so_fallback_is_used() {
    let tmp = tempfile::tempdir().unwrap();
    // An empty search path: no `lpass`, and never the real one.
    let env = ProviderEnv::with_home(tmp.path())
        .with_git(Git::system().with_program(tmp.path().join("no-git")))
        .with_search_path(tmp.path().join("empty-bin"));
    let secrets = env.config_dir.join("secrets.env");
    fsutil::write_private(&secrets, b"GITLAB_TOKEN=glpat-1\n").unwrap();
    let cfg = config(
        "providers:\n  lastpass: { username: me@example.com }\nsecrets:\n  glab: { ref: \"lastpass://Dev/GitLab#password\", fallback: \"envfile:GITLAB_TOKEN\" }\n  only_lp: { ref: \"lastpass://Dev/Other#password\" }\n",
    );
    let mut r = Resolver::new(Registry::builtin(&env), cfg);
    assert_eq!(
        val(&r.resolve(["glab"], &mut NonInteractive).unwrap(), "glab"),
        "glpat-1"
    );
    let err = r.resolve(["only_lp"], &mut NonInteractive).unwrap_err();
    assert!(
        err.to_string()
            .contains("provider `lastpass` is not available"),
        "{err}"
    );
}

#[cfg(feature = "secrets-1password")]
#[test]
fn onepassword_stub_is_unavailable() {
    let p = OnePasswordProvider::new(OnePasswordConfig {
        account: Some("my.1password.com".into()),
    });
    assert_eq!(p.account(), Some("my.1password.com"));
    assert!(!p.available().unwrap());
    let r = SecretRef::parse("op://Private/GitLab/token").unwrap();
    assert!(p.resolve(&[r]).is_err());
}

// ---------------------------------------------------------------------------------------------
// envfile: defaults, guardrails, check

fn no_git(tmp: &Path) -> Git {
    Git::system().with_program(tmp.join("no-git"))
}

#[test]
fn envfile_defaults_come_from_the_injected_home() {
    let env = ProviderEnv::with_home("/home/u");
    let p = EnvfileProvider::from_config(None, &env);
    assert_eq!(p.path(), Path::new("/home/u/.config/dot/secrets.env"));
    assert_eq!(p.files_dir(), Path::new("/home/u/.config/dot/files"));
    let cfg = EnvfileConfig {
        path: Some("~/private/s.env".into()),
        files_dir: None,
    };
    let p = EnvfileProvider::from_config(Some(&cfg), &env);
    assert_eq!(p.path(), Path::new("/home/u/private/s.env"));
    assert_eq!(p.files_dir(), Path::new("/home/u/private/files"));
    let cfg = EnvfileConfig {
        path: None,
        files_dir: Some("~/blobs".into()),
    };
    assert_eq!(
        EnvfileProvider::from_config(Some(&cfg), &env).files_dir(),
        Path::new("/home/u/blobs")
    );
    // Shown as the path is: `~/private/s.env` joined onto the injected home, which on Windows
    // puts this platform's separator after the home.
    let shown = if cfg!(windows) {
        r"/home/u\private/s.env"
    } else {
        "/home/u/private/s.env"
    };
    assert_eq!(p.location().as_deref(), Some(shown));
}

#[test]
fn envfile_missing_file_means_missing_keys_and_store_creates_it() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("cfg/secrets.env");
    let mut p = EnvfileProvider::new(&path, None).with_git(no_git(tmp.path()));
    p.unlock(&mut NonInteractive).unwrap();
    let r = SecretRef::envfile("TOKEN").unwrap();
    assert!(p.resolve(std::slice::from_ref(&r)).unwrap()[0].is_none());
    p.store(&r, &SecretValue::from("t0k".to_string())).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "TOKEN=\"t0k\"\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(
        p.store(&r, &SecretValue::new(vec![0xff, 0xfe]))
            .unwrap_err()
            .to_string()
            .contains("UTF-8")
    );
}

#[test]
fn envfile_keeps_comments_and_treats_empty_as_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("secrets.env");
    fsutil::write_private(&path, b"# my tokens\nA=1\nEMPTY=\nB=${A}-x\n").unwrap();
    let p = EnvfileProvider::new(&path, None).with_git(no_git(tmp.path()));
    let refs = ["A", "EMPTY", "B"].map(|k| SecretRef::envfile(k).unwrap());
    let got = p.resolve(&refs).unwrap();
    assert_eq!(got[0].as_ref().unwrap().expose(), b"1");
    assert!(got[1].is_none());
    assert_eq!(
        got[2].as_ref().unwrap().expose(),
        b"1-x",
        "dotenv interpolation"
    );
    p.store(&refs[1], &SecretValue::from("now".to_string()))
        .unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "# my tokens\nA=1\nEMPTY=\"now\"\nB=${A}-x\n"
    );
}

#[test]
fn envfile_errors_never_show_the_line() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("secrets.env");
    fsutil::write_private(&path, b"GOOD=1\nBROKEN=sup3r secret value\n").unwrap();
    let mut p = EnvfileProvider::new(&path, None).with_git(no_git(tmp.path()));
    let err = format!("{:#}", p.unlock(&mut NonInteractive).unwrap_err());
    assert!(err.contains("malformed") && !err.contains("sup3r"), "{err}");
    let report = p.check(&Expected::default());
    assert!(
        report.problems.iter().any(|m| m.contains("malformed")),
        "{report:?}"
    );
    assert!(!format!("{report:?}").contains("sup3r"));
}

#[test]
fn envfile_rejects_foreign_refs() {
    let tmp = tempfile::tempdir().unwrap();
    let p = EnvfileProvider::new(tmp.path().join("s.env"), None).with_git(no_git(tmp.path()));
    let r = SecretRef::parse("mock:K").unwrap();
    assert!(
        p.resolve(std::slice::from_ref(&r))
            .unwrap_err()
            .to_string()
            .contains("not `envfile`")
    );
    assert!(p.store(&r, &SecretValue::from("v".to_string())).is_err());
}

#[cfg(unix)]
#[test]
fn envfile_refuses_loose_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("secrets.env");
    fs::write(&path, "A=1\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let mut p = EnvfileProvider::new(&path, None).with_git(no_git(tmp.path()));
    let err = format!("{:#}", p.unlock(&mut NonInteractive).unwrap_err());
    assert!(err.contains("chmod 600") && err.contains("0644"), "{err}");
    assert!(p.resolve(&[SecretRef::envfile("A").unwrap()]).is_err());
    assert!(
        p.store(
            &SecretRef::envfile("B").unwrap(),
            &SecretValue::from("b".to_string())
        )
        .is_err()
    );

    // File secrets are held to the same rule.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let pem = tmp.path().join("files/k.pem");
    fs::create_dir_all(pem.parent().unwrap()).unwrap();
    fs::write(&pem, "key").unwrap();
    fs::set_permissions(&pem, fs::Permissions::from_mode(0o640)).unwrap();
    let err = format!(
        "{:#}",
        p.resolve(&[SecretRef::envfile_file("k.pem").unwrap()])
            .unwrap_err()
    );
    assert!(
        err.contains("file secret") && err.contains("chmod 600"),
        "{err}"
    );

    // Through the resolver the guardrail fails that secret alone, naming it and its ref.
    let cfg = config("secrets:\n  k: { ref: \"envfile-file:k.pem\", kind: file }\n");
    let mut reg = Registry::new();
    let prov = p.clone();
    reg.register("envfile", move |_| {
        Ok(Box::new(prov.clone()) as Box<dyn SecretProvider>)
    });
    let err = Resolver::new(reg, cfg)
        .resolve(["k"], &mut NonInteractive)
        .unwrap_err();
    assert!(
        err.to_string().contains("k: `envfile-file:k.pem`")
            && err.to_string().contains("chmod 600"),
        "{err}"
    );
    assert!(err.failures[0].is_scoped(), "{err}");
}

/// Regression: one file secret others can read used to fail the provider's whole batch, so
/// every secret of the run (values and the other files alike) was reported unresolved.
#[cfg(unix)]
#[test]
fn a_loose_file_secret_fails_only_itself() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("secrets.env");
    fsutil::write_private(&path, b"TOKEN=tok\n").unwrap();
    fsutil::write_private(&tmp.path().join("files/vpn/a.ovpn"), b"a\n").unwrap();
    let loose = tmp.path().join("files/vpn/b.ovpn");
    fsutil::write_private(&loose, b"b\n").unwrap();
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o644)).unwrap();
    let p = EnvfileProvider::new(&path, None).with_git(no_git(tmp.path()));

    // The provider: per ref.
    let refs = [
        SecretRef::envfile("TOKEN").unwrap(),
        SecretRef::envfile_file("vpn/a.ovpn").unwrap(),
        SecretRef::envfile_file("vpn/b.ovpn").unwrap(),
    ];
    let each = p.resolve_each(&refs).unwrap();
    assert_eq!(each[0].as_ref().unwrap().as_ref().unwrap().expose(), b"tok");
    assert_eq!(each[1].as_ref().unwrap().as_ref().unwrap().expose(), b"a\n");
    let why = each[2].as_ref().unwrap_err();
    assert!(why.contains("b.ovpn") && why.contains("chmod 600"), "{why}");
    // `resolve` keeps its all-or-nothing contract.
    assert!(p.resolve(&refs).is_err());

    // The resolver: the other two resolve; the loose one fails, scoped to itself.
    let cfg = config(concat!(
        "secrets:\n",
        "  token: { ref: \"envfile:TOKEN\" }\n",
        "  a: { ref: \"envfile-file:vpn/a.ovpn\", kind: file }\n",
        "  b: { ref: \"envfile-file:vpn/b.ovpn\", kind: file }\n",
    ));
    let mut reg = Registry::new();
    let prov = p.clone();
    reg.register("envfile", move |_| {
        Ok(Box::new(prov.clone()) as Box<dyn SecretProvider>)
    });
    let got = Resolver::new(reg, cfg).resolve_partial(["token", "a", "b"], &mut NonInteractive);
    assert_eq!(val(&got.resolved, "token"), "tok");
    assert_eq!(got.resolved.get("a").unwrap().expose(), b"a\n");
    assert_eq!(got.failed.len(), 1, "{:?}", got.failed);
    let f = &got.failed[0];
    assert_eq!(f.name, "b");
    assert!(matches!(f.problem, Problem::RefFailed { .. }), "{f:?}");
    assert!(f.is_scoped() && !f.is_missing(), "{f:?}");

    // The secrets file itself stays a provider-wide failure, which is not scoped.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let cfg = config("secrets:\n  token: { ref: \"envfile:TOKEN\" }\n");
    let mut reg = Registry::new();
    let prov = p.clone();
    reg.register("envfile", move |_| {
        Ok(Box::new(prov.clone()) as Box<dyn SecretProvider>)
    });
    let got = Resolver::new(reg, cfg).resolve_partial(["token"], &mut NonInteractive);
    assert!(!got.failed[0].is_scoped(), "{:?}", got.failed);
}

#[test]
fn envfile_refuses_a_file_inside_a_git_work_tree_unless_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(git) = isolated(tmp.path()) else {
        eprintln!("git not installed; skipping");
        return;
    };
    let repo = tmp.path().join("repo");
    init(&git, &repo);
    let path = repo.join("secrets.env");
    fsutil::write_private(&path, b"A=1\n").unwrap();
    let mut p = EnvfileProvider::new(&path, None).with_git(git.clone());
    let err = format!("{:#}", p.unlock(&mut NonInteractive).unwrap_err());
    assert!(
        err.contains("inside a git work tree") && err.contains(".gitignore"),
        "{err}"
    );
    let err = p.store(
        &SecretRef::envfile_file("x.pem").unwrap(),
        &SecretValue::from("x".to_string()),
    );
    assert!(format!("{:#}", err.unwrap_err()).contains("git work tree"));

    fs::write(repo.join(".gitignore"), "secrets.env\nfiles/\n").unwrap();
    p.unlock(&mut NonInteractive).unwrap();
    let r = SecretRef::envfile("A").unwrap();
    assert_eq!(p.resolve(&[r]).unwrap()[0].as_ref().unwrap().expose(), b"1");
    p.store(
        &SecretRef::envfile_file("x.pem").unwrap(),
        &SecretValue::from("x".to_string()),
    )
    .unwrap();

    // With git absent the file counts as outside any repository.
    fs::remove_file(repo.join(".gitignore")).unwrap();
    let mut p = EnvfileProvider::new(&path, None).with_git(no_git(tmp.path()));
    p.unlock(&mut NonInteractive).unwrap();
}

#[test]
fn envfile_check_reports_names_only() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("secrets.env");
    fsutil::write_private(
        &path,
        b"GITLAB_TOKEN=glpat-secret\nEMPTY=\nSTALE=old-secret\n",
    )
    .unwrap();
    fsutil::write_private(&tmp.path().join("files/vpn/work.ovpn"), b"client\n").unwrap();
    let example = tmp.path().join(".env.example");
    fs::write(
        &example,
        "HOST=gitlab.com\nGITLAB_TOKEN= # secret\nEMPTY= # secret\nNPM_TOKEN= # secret\n",
    )
    .unwrap();

    let cfg = config(
        "secrets:\n  gl: { ref: GITLAB_TOKEN }\n  vpn: { ref: \"envfile-file:vpn/work.ovpn\", kind: file }\n  ssh: { ref: ssh/id.pem, kind: file }\n  lp: { ref: \"lastpass://A/B#password\", fallback: \"envfile:LP_FALLBACK\" }\n",
    );
    let expected = Expected::from_example(&example)
        .unwrap()
        .merge(Expected::from_config(&cfg));
    let names = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        expected.keys.iter().cloned().collect::<Vec<_>>(),
        names(&["EMPTY", "GITLAB_TOKEN", "NPM_TOKEN"])
    );
    // A fallback is optional; on a profile whose provider is envfile it is tried first, so
    // it is expected there.
    assert_eq!(
        expected.optional_keys.iter().cloned().collect::<Vec<_>>(),
        names(&["LP_FALLBACK"])
    );
    let on_envfile = Expected::from_config(&cfg.clone().with_provider_override(Some("envfile")));
    assert!(on_envfile.keys.contains("LP_FALLBACK"), "{on_envfile:?}");

    let p = EnvfileProvider::new(&path, None).with_git(no_git(tmp.path()));
    let report = p.check(&expected);
    assert_eq!(report.missing_keys, names(&["EMPTY", "NPM_TOKEN"]));
    assert_eq!(report.missing_files, names(&["ssh/id.pem"]));
    assert_eq!(report.unused_keys, names(&["STALE"]));
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert!(!report.is_ok());
    let shown = format!("{report:?}");
    assert!(
        !shown.contains("glpat") && !shown.contains("old-secret") && !shown.contains("client"),
        "{shown}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let report = p.check(&expected);
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(report.problems[0].contains("chmod 600"));
    }

    let absent =
        EnvfileProvider::new(tmp.path().join("nope.env"), None).with_git(no_git(tmp.path()));
    let report = absent.check(&expected);
    assert!(report.problems[0].contains("does not exist"));
    assert_eq!(report.missing_keys.len(), 3);
}

#[test]
fn builtin_registry_resolves_envfile_under_injected_home() {
    let tmp = tempfile::tempdir().unwrap();
    let home: PathBuf = tmp.path().join("home");
    let env = ProviderEnv::with_home(&home).with_git(no_git(tmp.path()));
    fsutil::write_private(&home.join(".config/dot/secrets.env"), b"GITLAB_TOKEN=abc\n").unwrap();
    fsutil::write_private(
        &home.join(".config/dot/files/vpn/work.ovpn"),
        b"remote vpn\n",
    )
    .unwrap();
    let cfg = config(
        "secrets:\n  gitlab_token: { ref: \"envfile:GITLAB_TOKEN\" }\n  vpn: { ref: \"envfile-file:vpn/work.ovpn\", kind: file }\n",
    );
    let reg = Registry::builtin(&env);
    assert!(cfg.validate(&reg).is_empty());
    let mut r = Resolver::new(reg, cfg);
    let got = r
        .resolve(["gitlab_token", "vpn"], &mut NonInteractive)
        .unwrap();
    assert_eq!(val(&got, "gitlab_token"), "abc");
    assert_eq!(got.get("vpn").unwrap().expose(), b"remote vpn\n");
    assert_eq!(got.len(), 2);
}

#[test]
fn the_profile_provider_is_tried_first() {
    let lp = MemoryProvider::new("lp").with_value("GL", "from-lp");
    let env = MemoryProvider::new("ef").with_value("GL", "from-ef");
    let base =
        config("default_provider: lp\nsecrets:\n  glab: { ref: \"lp:GL\", fallback: \"ef:GL\" }\n");
    let entry = |c: &SecretsConfig| c.entry("glab").unwrap().unwrap();
    assert_eq!(entry(&base).reference.provider(), "lp");
    let server = base.clone().with_provider_override(Some("ef"));
    assert_eq!(entry(&server).reference.provider(), "ef");
    assert_eq!(entry(&server).fallback.unwrap().provider(), "lp");
    let mut r = Resolver::new(registry_with(&[&lp, &env]), server);
    assert_eq!(
        val(&r.resolve(["glab"], &mut NonInteractive).unwrap(), "glab"),
        "from-ef"
    );
    assert_eq!(lp.resolve_calls(), 0, "the other provider is not touched");
}

#[test]
fn a_locked_provider_falls_back_when_nobody_can_unlock_it() {
    let locked = MemoryProvider::new("lp")
        .with_value("GL", "never")
        .with_caps(Caps {
            read: true,
            batch: true,
            needs_unlock: true,
            ..Caps::default()
        })
        .failing_unlock("not logged in; run `lpass login`");
    let env = MemoryProvider::new("ef").with_value("GL", "from-ef");
    let cfg = config(
        "secrets:\n  glab: { ref: \"lp:GL\", fallback: \"ef:GL\" }\n  only: { ref: \"lp:GL\" }\n",
    );
    let mut r = Resolver::new(registry_with(&[&locked, &env]), cfg.clone());
    assert_eq!(
        val(&r.resolve(["glab"], &mut NonInteractive).unwrap(), "glab"),
        "from-ef"
    );
    let err = r
        .resolve(["only"], &mut NonInteractive)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("locked and cannot be unlocked without a terminal"),
        "{err}"
    );
    // Interactively, a refused unlock is a real failure.
    let mut r = Resolver::new(registry_with(&[&locked, &env]), cfg);
    let err = r
        .resolve(["glab"], &mut RecordingTty::default())
        .unwrap_err();
    assert!(err.to_string().contains("provider `lp` failed"), "{err}");
}

#[test]
fn an_unavailable_provider_is_deferred_and_asked_again() {
    let lp = MemoryProvider::new("lp")
        .with_value("GL", "from-lp")
        .unavailable();
    let env = MemoryProvider::new("ef").with_value("A", "a");
    let cfg = config(
        "secrets:\n  glab: { ref: \"lp:GL\" }\n  a: { ref: \"ef:A\" }\n  gone: { ref: \"ef:NOPE\" }\n",
    );
    let mut r = Resolver::new(registry_with(&[&lp, &env]), cfg);
    let (now, later) = r
        .resolve_available(["glab", "a"], &mut NonInteractive)
        .unwrap();
    assert_eq!(val(&now, "a"), "a");
    assert_eq!(later, ["glab"]);
    // Any other failure is still an error up front.
    let err = r
        .resolve_available(["glab", "gone"], &mut NonInteractive)
        .unwrap_err();
    assert_eq!(err.names().collect::<Vec<_>>(), ["gone"]);
    // Installed since: the same resolver finds it now.
    lp.set_available(true);
    assert_eq!(
        val(&r.resolve(["glab"], &mut NonInteractive).unwrap(), "glab"),
        "from-lp"
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_secrets_file_is_checked_where_it_points() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(git) = isolated(tmp.path()) else {
        eprintln!("git not installed; skipping");
        return;
    };
    let other = tmp.path().join("other");
    init(&git, &other);
    let target = other.join("secrets.env");
    fsutil::write_private(&target, b"TOK=supersecretvalue123\n").unwrap();
    let dir = tmp.path().join("config");
    fs::create_dir_all(&dir).unwrap();
    let link = dir.join("secrets.env");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let mut p = EnvfileProvider::new(&link, None).with_git(git.clone());
    let err = format!("{:#}", p.unlock(&mut NonInteractive).unwrap_err());
    assert!(err.contains("the target of"), "{err}");
    let report = p.check(&Expected::default());
    assert!(
        report.problems.iter().any(|e| e.contains("git work tree")),
        "{:?}",
        report.problems
    );
    fs::write(other.join(".gitignore"), "secrets.env\n").unwrap();
    p.unlock(&mut NonInteractive).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Prompting for missing secrets

/// Interactive, answering from a script and recording every prompt.
#[derive(Default)]
struct AnsweringTty {
    answers: Vec<&'static str>,
    prompts: Vec<String>,
    events: Vec<String>,
}

impl Interactive for AnsweringTty {
    fn is_interactive(&self) -> bool {
        true
    }
    fn suspend(&mut self, reason: &str) {
        self.events.push(format!("suspend: {reason}"));
    }
    fn resume(&mut self) {
        self.events.push("resume".into());
    }
    fn read_secret(&mut self, prompt: &str) -> anyhow::Result<SecretValue> {
        self.prompts.push(prompt.to_string());
        if self.answers.is_empty() {
            anyhow::bail!("no more answers");
        }
        Ok(SecretValue::from(self.answers.remove(0).to_string()))
    }
}

#[test]
fn a_missing_secret_is_asked_for_and_stored() {
    let p = MemoryProvider::new("m").with_value("HAVE", "had");
    let cfg = config("secrets:\n  have: { ref: \"m:HAVE\" }\n  want: { ref: \"m:WANT\" }\n");
    let hooked = Arc::new(Mutex::new(Vec::new()));
    let h = hooked.clone();
    let mut r = Resolver::new(registry_with(&[&p]), cfg);
    r.on_resolved(move |v| h.lock().unwrap().push(v.expose_str().unwrap().to_string()));
    let mut tty = AnsweringTty {
        answers: vec!["typed"],
        ..Default::default()
    };
    let got = r.resolve(["have", "want"], &mut tty).unwrap();
    assert_eq!(val(&got, "want"), "typed");
    assert_eq!(val(&got, "have"), "had");
    assert_eq!(got.stored().collect::<Vec<_>>(), [("want", "m:WANT")]);
    assert_eq!(got.prompted().collect::<Vec<_>>(), ["want"]);
    assert_eq!(tty.prompts.len(), 1);
    assert!(
        tty.prompts[0].contains("`want` (m:WANT)")
            && tty.prompts[0].contains("saved to memory provider `m`"),
        "{:?}",
        tty.prompts
    );
    assert!(
        tty.events
            .contains(&"suspend: asking for secret `want`".to_string())
    );
    assert!(
        hooked.lock().unwrap().contains(&"typed".to_string()),
        "redacted too"
    );
    // Stored: the next run finds it without asking.
    let got = p.resolve(&[SecretRef::parse("m:WANT").unwrap()]).unwrap();
    assert_eq!(got[0].as_ref().unwrap().expose_str(), Some("typed"));
}

#[test]
fn nothing_is_asked_without_a_terminal_or_for_what_cannot_be_typed() {
    let p = MemoryProvider::new("m").with_value("A", "a");
    let failing = MemoryProvider::new("bad").failing_unlock("broken");
    let cfg = config(concat!(
        "secrets:\n",
        "  want: { ref: \"m:WANT\" }\n",
        "  cert: { ref: \"m-file:c.pem\", kind: file }\n",
        "  broken: { ref: \"bad:X\" }\n",
    ));
    let mut r = Resolver::new(registry_with(&[&p, &failing]), cfg.clone());
    let err = r.resolve(["want"], &mut NonInteractive).unwrap_err();
    assert_eq!(err.names().collect::<Vec<_>>(), ["want"]);

    let mut tty = AnsweringTty {
        answers: vec!["x", "y", "z"],
        ..Default::default()
    };
    let mut r = Resolver::new(registry_with(&[&p, &failing]), cfg);
    let err = r
        .resolve(["cert", "broken", "undeclared"], &mut tty)
        .unwrap_err();
    assert_eq!(err.failures.len(), 3);
    assert!(tty.prompts.is_empty(), "{:?}", tty.prompts);

    // An empty answer (or none) leaves it missing.
    let mut tty = AnsweringTty {
        answers: vec![""],
        ..Default::default()
    };
    let err = r.resolve(["want"], &mut tty).unwrap_err();
    assert_eq!(err.names().collect::<Vec<_>>(), ["want"]);
    assert_eq!(tty.prompts.len(), 1);
    let err = r
        .resolve(["want"], &mut AnsweringTty::default())
        .unwrap_err();
    assert!(err.to_string().contains("`m:WANT` not found"), "{err}");
}

#[test]
fn an_answer_goes_where_the_value_was_looked_for() {
    // Primary provider down, fallback reachable but without the key: stored in the fallback.
    let lp = MemoryProvider::new("lp").unavailable();
    let ef = MemoryProvider::new("ef");
    let ro = MemoryProvider::new("ro").with_caps(Caps {
        read: true,
        batch: true,
        ..Caps::default()
    });
    let cfg = config(concat!(
        "secrets:\n",
        "  glab: { ref: \"lp:GL\", fallback: \"ef:GL\" }\n",
        "  only_lp: { ref: \"lp:X\" }\n",
        "  readonly: { ref: \"ro:R\" }\n",
    ));
    let mut r = Resolver::new(registry_with(&[&lp, &ef, &ro]), cfg);
    let mut tty = AnsweringTty {
        answers: vec!["typed-glab", "typed-ro"],
        ..Default::default()
    };
    let got = r.resolve(["glab", "readonly"], &mut tty).unwrap();
    assert_eq!(val(&got, "glab"), "typed-glab");
    assert_eq!(val(&got, "readonly"), "typed-ro");
    assert_eq!(got.stored().collect::<Vec<_>>(), [("glab", "ef:GL")]);
    assert!(
        tty.prompts[1].contains("used for this run only"),
        "{:?}",
        tty.prompts
    );
    let stored = ef.resolve(&[SecretRef::parse("ef:GL").unwrap()]).unwrap();
    assert_eq!(stored[0].as_ref().unwrap().expose_str(), Some("typed-glab"));

    // A provider that is not there cannot be asked: the name stays deferrable.
    let mut tty = AnsweringTty {
        answers: vec!["never"],
        ..Default::default()
    };
    let (_, deferred) = r.resolve_available(["only_lp"], &mut tty).unwrap();
    assert_eq!(deferred, ["only_lp"]);
    assert!(tty.prompts.is_empty());
}

#[test]
fn an_answer_is_appended_to_the_envfile_privately() {
    let tmp = tempfile::tempdir().unwrap();
    let env = ProviderEnv::with_home(tmp.path()).with_git(no_git(tmp.path()));
    let path = env.config_dir.join("secrets.env");
    fsutil::write_private(&path, b"# mine\nKEEP=1\n").unwrap();
    let cfg = config("secrets:\n  tok: { ref: \"envfile:NEW_TOKEN\" }\n");
    let mut r = Resolver::new(Registry::builtin(&env), cfg);
    let mut tty = AnsweringTty {
        answers: vec!["s3cr3t value"],
        ..Default::default()
    };
    let got = r.resolve(["tok"], &mut tty).unwrap();
    assert_eq!(val(&got, "tok"), "s3cr3t value");
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text, "# mine\nKEEP=1\nNEW_TOKEN=\"s3cr3t value\"\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
