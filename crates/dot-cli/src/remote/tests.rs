//! The remote flow against a scripted runner: every ssh, scp, rsync and git argv asserted,
//! nothing run for real.

use std::fs;
use std::io::{ErrorKind, Read};
use std::path::Path;

use dot_platform::exec::fake::FakeExec;

use super::*;
use crate::handoff;

const TOKEN: &str = "tok-remote-very-secret";

const LINUX: &str = "Linux x86_64\n/home/me\nno-rsync\n";

fn remote(fake: &Arc<FakeExec>) -> Remote {
    Remote::with_runner("me@box", fake.clone(), PathBuf::from("/opt/dot/bin/dot"))
}

/// A repository with a tracked file, an ignored one, and `.git`.
fn repo(dir: &Path) -> PathBuf {
    let root = dir.join("repo");
    for (p, text) in [
        ("dot.yml", ""),
        ("bundles/app/bundle.yml", "steps: []\n"),
        (".env.local", "LOCAL=only\n"),
        (".git/HEAD", "ref: refs/heads/main\n"),
    ] {
        let f = root.join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, text).unwrap();
    }
    root
}

fn job<'a>(r: &'a Remote, root: &Path) -> Job<'a> {
    Job {
        remote: r,
        command: "apply",
        root: root.to_path_buf(),
        remote_repo: None,
        args: vec![
            "--profile".into(),
            "server".into(),
            "--yes".into(),
            "app".into(),
        ],
        dry_run: false,
        prepare: None,
    }
}

const HOST_INFO: &str = concat!(
    "{\"version\":\"",
    env!("CARGO_PKG_VERSION"),
    "\",\"home\":\"/home/me\",\"facts\":\"os=linux,arch=x86_64\",\"profile\":null,\"local_env\":[\"EMAIL\"]}\n"
);

/// A handoff of one secret and one variable.
fn handoff() -> handoff::Handoff {
    handoff::Handoff {
        secrets: vec![handoff::SentSecret {
            name: "tok".into(),
            file: false,
            target: Some("envfile:API_TOKEN".into()),
            value: dot_secrets::SecretValue::from(TOKEN.to_string()),
        }],
        vars: vec![handoff::SentVar {
            name: "EMAIL".into(),
            value: zeroize::Zeroizing::new("me@example.com".into()),
            persist: true,
            keep: false,
            sensitive: false,
        }],
        ..handoff::Handoff::default()
    }
}

fn code(c: ExitCode) -> String {
    format!("{c:?}")
}

fn untar(bytes: &[u8]) -> Vec<String> {
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut names: Vec<String> = a
        .entries()
        .unwrap()
        .map(|e| {
            let mut e = e.unwrap();
            let mut s = String::new();
            let _ = e.read_to_string(&mut s);
            e.path().unwrap().display().to_string()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn probe_output_and_targets() {
    let p = Probe::parse(concat!(
        "Linux aarch64\n/home/me\nrsync\n/usr/local/bin/dot\ndot ",
        env!("CARGO_PKG_VERSION"),
        "\n"
    ))
    .unwrap();
    assert_eq!(
        p,
        Probe {
            target: Target {
                os: "linux",
                arch: "aarch64"
            },
            home: "/home/me".into(),
            rsync: true,
            dot: Some((
                "/usr/local/bin/dot".into(),
                concat!("dot ", env!("CARGO_PKG_VERSION"), "").into()
            )),
        }
    );
    let p = Probe::parse("Darwin arm64\n/Users/me\nno-rsync\n").unwrap();
    assert_eq!(
        (p.target.os, p.target.arch, p.rsync, p.dot),
        ("macos", "aarch64", false, None)
    );
    for bad in [
        "",
        "FreeBSD amd64\n/home\n",
        "Linux riscv64\n/h\n",
        "Linux x86_64\nrel\n",
    ] {
        assert!(Probe::parse(bad).is_err(), "{bad:?}");
    }
    for bad in ["", "-oProxyCommand=x", "a b"] {
        assert!(check_host(bad).is_err(), "{bad}");
    }
}

#[test]
fn a_new_machine_gets_dot_from_install_sh_the_repo_by_tar_and_what_was_resolved_here() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    let fake = FakeExec::new();
    fake.on("ssh me@box 'sh -c", Output::ok(LINUX));
    fake.on(
        &format!("git -C {} ls-files", root.display()),
        Output::ok("dot.yml\0bundles/app/bundle.yml\0gone.txt\0"),
    );
    fake.on(
        "ssh me@box '/home/me/.local/bin/dot host-info'",
        Output::ok(HOST_INFO),
    );
    fake.on(
        "ssh -T -o EscapeChar=none me@box '/home/me/.local/bin/dot apply",
        Output::exit(3, ""),
    );
    let r = remote(&fake);
    let asked = std::sync::Mutex::new(Vec::new());
    let mut j = job(&r, &root);
    j.prepare = Some(Box::new(|info: &HostInfo, dry| {
        asked.lock().unwrap().push((info.clone(), dry));
        Ok(send::Prepared {
            handoff: Some(handoff()),
            profile: Some("ignored".into()),
        })
    }));
    let c = j.run().unwrap();
    drop(j);
    assert_eq!(code(c), code(ExitCode::from(3)), "the remote's exit code");
    // What host-info reported was what the sending side planned with.
    let asked = asked.into_inner().unwrap();
    assert_eq!(asked.len(), 1);
    assert!(!asked[0].1);
    assert_eq!(asked[0].0.local_env, ["EMAIL"]);
    assert_eq!(asked[0].0.facts().unwrap().0.os, dot_core::facts::Os::Linux);

    let cmds = fake.commands();
    let lines: Vec<String> = cmds.iter().map(Cmd::line).collect();
    // install.sh on the remote's sh, the script itself on stdin.
    let install = cmds
        .iter()
        .find(|c| c.args.last().unwrap() == "sh -s")
        .unwrap();
    assert_eq!(install.stdin.as_deref(), Some(INSTALL_SH.as_bytes()));
    // Every ssh fed bytes gets no terminal there and no escape character, whatever
    // ~/.ssh/config says.
    for c in cmds
        .iter()
        .filter(|c| c.program == "ssh" && c.stdin.is_some())
    {
        assert_eq!(c.args[..3], ["-T", "-o", "EscapeChar=none"], "{}", c.line());
    }
    // The repository as a tar stream: tracked and untracked-but-not-ignored files and `.git`;
    // not `.env.local` (ignored, so git did not list it) or a deleted file.
    let tar = cmds
        .iter()
        .find(|c| c.args.last().unwrap().contains("tar -xzf"))
        .unwrap();
    assert_eq!(
        tar.args.last().unwrap(),
        "mkdir -p /home/me/.dot && tar -xzf - -C /home/me/.dot"
    );
    assert_eq!(
        untar(tar.stdin.as_deref().unwrap()),
        [".git/HEAD", "bundles/app/bundle.yml", "dot.yml"]
    );
    // Asked before anything is synced; then the run, which reads the handoff on its stdin.
    let from = |prefix: &str| lines.iter().position(|l| l.starts_with(prefix)).unwrap();
    assert!(
        from("ssh me@box '/home/me/.local/bin/dot host-info'")
            < from("ssh -T -o EscapeChar=none me@box 'mkdir -p")
    );
    // The profile given here (not the one planned) goes to the run.
    assert_eq!(
        lines.last().unwrap(),
        "ssh -T -o EscapeChar=none me@box '/home/me/.local/bin/dot apply --secrets-from - --repo \
         /home/me/.dot --profile server --yes app'"
    );
    let run = cmds.last().unwrap();
    let sent = handoff::Handoff::decode(run.stdin.as_deref().unwrap()).unwrap();
    assert_eq!(sent.secret_names(), ["tok"]);
    assert_eq!(sent.secrets[0].value.expose(), TOKEN.as_bytes());
    assert_eq!(sent.var_names(), ["EMAIL"]);
    // The value is on no command line, and fed to nothing but that run.
    for c in &cmds[..cmds.len() - 1] {
        assert!(!c.line().contains(TOKEN), "{}", c.line());
        let stdin = String::from_utf8_lossy(c.stdin.as_deref().unwrap_or_default()).into_owned();
        assert!(!stdin.contains(TOKEN), "{}", c.line());
    }
    assert!(!run.line().contains(TOKEN));
    assert!(!run.line().contains("me@example.com"));
    assert!(run.env.is_empty(), "{:?}", run.env);
}

#[test]
fn nothing_to_hand_over_runs_as_before_and_an_old_dot_there_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    let fake = FakeExec::new();
    fake.on(
        "ssh me@box 'sh -c",
        Output::ok(concat!(
            "Linux x86_64\n/home/me\nno-rsync\n/usr/bin/dot\ndot ",
            env!("CARGO_PKG_VERSION"),
            "\n"
        )),
    );
    fake.on("ssh me@box '/usr/bin/dot host-info'", Output::ok(HOST_INFO));
    let r = remote(&fake);
    let mut j = job(&r, &root);
    j.args.drain(..2);
    j.prepare = Some(Box::new(|_: &HostInfo, _| {
        Ok(send::Prepared {
            handoff: None,
            profile: Some("work".into()),
        })
    }));
    assert_eq!(code(j.run().unwrap()), code(ExitCode::SUCCESS));
    let last = fake.commands().pop().unwrap();
    // No --profile here: the run is given the one planned here.
    assert_eq!(
        last.line(),
        "ssh me@box '/usr/bin/dot apply --repo /home/me/.dot --profile work --yes app'"
    );
    assert!(last.stdin.is_none());

    // This version without `host-info` (an older build): its own error is not shown as such.
    let fake = FakeExec::new();
    fake.on(
        "ssh me@box 'sh -c",
        Output::ok(&*format!(
            "Linux x86_64\n/home/me\nno-rsync\n/usr/bin/dot\ndot {}\n",
            env!("CARGO_PKG_VERSION")
        )),
    );
    fake.on(
        "ssh me@box '/usr/bin/dot host-info'",
        Output {
            code: Some(2),
            stdout: String::new(),
            stderr: "error: unrecognized subcommand 'host-info'\n".into(),
        },
    );
    let r = remote(&fake);
    let mut j = job(&r, &root);
    j.prepare = Some(Box::new(|_: &HostInfo, _| panic!("not asked")));
    let e = format!("{:#}", j.run().unwrap_err());
    assert!(
        e.contains("does not know `host-info`") && e.contains("/usr/bin/dot self-update"),
        "{e}"
    );
    assert!(!e.contains("--no-secrets"), "{e}");

    // host-info failing for another reason: the remote's own error, not a version.
    let fake = FakeExec::new();
    fake.on(
        "ssh me@box 'sh -c",
        Output::ok(&*format!(
            "Linux x86_64\n/home/me\nno-rsync\n/usr/bin/dot\ndot {}\n",
            env!("CARGO_PKG_VERSION")
        )),
    );
    fake.on(
        "ssh me@box '/usr/bin/dot host-info'",
        Output {
            code: Some(1),
            stdout: String::new(),
            stderr: "error: /home/me/.config/dot/local.env: invalid dotenv syntax in the value \
                     of `EMAIL`\n"
                .into(),
        },
    );
    let r = remote(&fake);
    let mut j = job(&r, &root);
    j.prepare = Some(Box::new(|_: &HostInfo, _| panic!("not asked")));
    let e = format!("{:#}", j.run().unwrap_err());
    assert!(
        e.contains("host-info` failed on me@box") && e.contains("invalid dotenv syntax"),
        "{e}"
    );
    assert!(!e.contains("older"), "{e}");

    // An older dot there: stops before anything is synced or run, or host-info asked.
    let fake = FakeExec::new();
    fake.on(
        "ssh me@box 'sh -c",
        Output::ok("Linux x86_64\n/home/me\nno-rsync\n/usr/bin/dot\ndot 0.0.9\n"),
    );
    fake.on(
        "ssh me@box '/usr/bin/dot host-info'",
        Output::exit(2, "error: unrecognized subcommand 'host-info'"),
    );
    let r = remote(&fake);
    let mut j = job(&r, &root);
    j.prepare = Some(Box::new(|_: &HostInfo, _| panic!("not asked")));
    let e = format!("{:#}", j.run().unwrap_err());
    assert!(
        e.contains("dot 0.0.9 at /usr/bin/dot on me@box is older than this one")
            && e.contains("`/usr/bin/dot self-update`"),
        "{e}"
    );
    assert!(!e.contains("curl"), "one instruction: {e}");
    assert!(
        !fake
            .calls()
            .iter()
            .any(|l| l.contains("tar") || l.contains("apply") || l.contains("host-info"))
    );
}

#[test]
fn rsync_the_local_binary_and_an_existing_dot() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    let fake = FakeExec::new();
    fake.on(
        "ssh -o ControlPath=/tmp/x/%C me@box 'sh -c",
        Output::ok("Linux x86_64\n/home/me\nrsync\n"),
    );
    // Ignored by any rule (here, say, `.git/info/exclude`): excluded by name.
    fake.on(
        &format!("git -C {} ls-files -z --others --ignored", root.display()),
        Output::ok(".env.local\0build/\0odd*name\0"),
    );
    let mut r = remote(&fake);
    r.local = Some(Target {
        os: "linux",
        arch: "x86_64",
    });
    r.ssh_opts = vec!["-o".into(), "ControlPath=/tmp/x/%C".into()];
    r.tty = true;
    let c = job(&r, &root).run().unwrap();
    assert_eq!(code(c), code(ExitCode::SUCCESS));
    let lines = fake.calls();
    // Same platform: this binary is copied over, not downloaded.
    assert!(!lines.iter().any(|l| l.ends_with("'sh -s'")), "{lines:?}");
    assert!(
        lines.contains(
            &"scp -o ControlPath=/tmp/x/%C -q /opt/dot/bin/dot me@box:/home/me/.local/bin/.dot.tmp"
                .to_string()
        ),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains(
        "chmod 755 /home/me/.local/bin/.dot.tmp && mv -f /home/me/.local/bin/.dot.tmp /home/me/.local/bin/dot"
    )));
    // rsync on both ends, through the same shared connection; .gitignore filters.
    let rsync = fake
        .commands()
        .into_iter()
        .find(|c| c.program == "rsync")
        .unwrap();
    assert_eq!(rsync.args.len(), 8, "{:?}", rsync.args);
    assert_eq!(
        rsync.args[..3],
        ["-az", "--delete", "--filter=:- .gitignore"]
    );
    assert!(
        rsync.args[3].starts_with("--exclude-from="),
        "{:?}",
        rsync.args
    );
    assert_eq!(
        rsync.args[4..],
        [
            "-e",
            "ssh -o ControlPath=/tmp/x/%C",
            &format!("{}/", root.display()),
            "me@box:/home/me/.dot/",
        ]
    );
    assert_eq!(
        exclude_list(&[".env.local".into(), "build/".into(), "odd*name".into()]),
        "/.env.local\n/build/\n/odd\\*name\n"
    );
    // Nothing to push without secrets (a plan); the run gets a terminal.
    assert!(
        !lines.iter().any(|l| l.contains("plan --json")),
        "{lines:?}"
    );
    assert!(
        lines
            .last()
            .unwrap()
            .starts_with("ssh -t -o ControlPath=/tmp/x/%C me@box '/home/me/.local/bin/dot apply"),
        "{lines:?}"
    );

    // Already installed: used as it is; rsync missing locally: tar.
    let fake = FakeExec::new();
    fake.on(
        "ssh me@box 'sh -c",
        Output::ok("Linux x86_64\n/home/me\nrsync\n/usr/bin/dot\ndot 99.0.0\n"),
    );
    fake.fail_to_start("rsync", ErrorKind::NotFound);
    let r = remote(&fake);
    job(&r, &root).run().unwrap();
    let lines = fake.calls();
    assert!(!lines.iter().any(|l| l.starts_with("scp")), "{lines:?}");
    assert!(!lines.iter().any(|l| l.contains("sh -s")), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("tar -xzf")), "{lines:?}");
    assert!(
        lines
            .last()
            .unwrap()
            .starts_with("ssh me@box '/usr/bin/dot apply")
    );
}

#[test]
fn failures_stop_before_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    let fake = FakeExec::new();
    fake.on(
        "ssh me@box 'sh -c",
        Output::exit(255, "ssh: connect: refused"),
    );
    let r = remote(&fake);
    let e = format!("{:#}", job(&r, &root).run().unwrap_err());
    assert_eq!(e, "cannot reach me@box: ssh: connect: refused");
    assert_eq!(fake.calls().len(), 1);
    // A remote whose shell is not sh (Windows' cmd.exe): said briefly, without the script.
    let fake = FakeExec::new();
    fake.on(
        "ssh me@box 'sh -c",
        Output {
            code: Some(1),
            stdout: String::new(),
            stderr: "'sh' is not recognized as an internal or external command,\noperable \
                     program or batch file.\n"
                .into(),
        },
    );
    let r = remote(&fake);
    let e = format!("{:#}", job(&r, &root).run().unwrap_err());
    assert!(
        e.starts_with("could not run the probe on me@box (")
            && e.contains("needs a POSIX `sh`: --host supports Linux and macOS remotes"),
        "{e}"
    );
    assert!(!e.contains("uname") && !e.contains("operable"), "{e}");
    // An unusual remote repository path is refused before anything is copied.
    let fake = FakeExec::new();
    fake.on("ssh me@box 'sh -c", Output::ok(LINUX));
    fake.on("ssh -T -o EscapeChar=none me@box 'sh -s'", Output::ok(""));
    let r = remote(&fake);
    let mut j = job(&r, &root);
    j.remote_repo = Some("my dots".into());
    let e = format!("{:#}", j.run().unwrap_err());
    assert!(e.contains("cannot sync to /home/me/my dots"), "{e}");
    // Something other than a dotfiles repository there: left alone, nothing synced or run.
    let fake = FakeExec::new();
    fake.on("ssh me@box 'sh -c", Output::ok(LINUX));
    fake.on("ssh me@box 'if [ -e", Output::ok("dot:not-a-dot-repo\n"));
    let r = remote(&fake);
    let e = format!("{:#}", job(&r, &root).run().unwrap_err());
    assert_eq!(
        e,
        "/home/me/.dot there exists and holds no dot.yml: not overwriting it; pass \
         --remote-repo to sync somewhere else"
    );
    let lines = fake.calls();
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("tar -xzf") || l.contains(" apply")),
        "{lines:?}"
    );
}

/// The check run there, run here by `sh`: only a missing or empty directory, or one holding
/// `dot.yml`, may be synced into.
#[cfg(unix)]
#[test]
fn the_sync_target_check_refuses_anything_else() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    fs::create_dir_all(t.join("empty")).unwrap();
    fs::create_dir_all(t.join("synced")).unwrap();
    fs::write(t.join("synced/dot.yml"), "").unwrap();
    fs::create_dir_all(t.join("dotbot/src")).unwrap();
    fs::write(t.join("dotbot/install"), "").unwrap();
    fs::create_dir_all(t.join("hidden")).unwrap();
    fs::write(t.join("hidden/.git"), "").unwrap();
    fs::write(t.join("file"), "").unwrap();
    for (name, refused) in [
        ("missing", false),
        ("empty", false),
        ("synced", false),
        ("dotbot", true),
        ("hidden", true),
        ("file", true),
    ] {
        let dir = t.join(name).display().to_string();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(sync_target_check(&dir))
            .output()
            .unwrap();
        assert!(out.status.success(), "{name}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim() == NOT_A_DOT_REPO,
            refused,
            "{name}"
        );
    }
}

fn exclude_list(paths: &[String]) -> String {
    fs::read_to_string(exclude_file(paths).unwrap().path()).unwrap()
}

#[test]
fn a_dry_run_installs_syncs_and_sends_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    // dot is there: asked what it is, for the plan made here.
    let fake = FakeExec::new();
    fake.on(
        "ssh me@box 'sh -c",
        Output::ok(concat!(
            "Linux x86_64\n/home/me\nno-rsync\n/usr/bin/dot\ndot ",
            env!("CARGO_PKG_VERSION"),
            "\n"
        )),
    );
    fake.on("ssh me@box '/usr/bin/dot host-info'", Output::ok(HOST_INFO));
    let r = remote(&fake);
    let dry = std::sync::Mutex::new(Vec::new());
    let mut j = job(&r, &root);
    j.dry_run = true;
    j.prepare = Some(Box::new(|info: &HostInfo, d| {
        dry.lock().unwrap().push((info.local_env.clone(), d));
        Ok(send::Prepared::default())
    }));
    assert_eq!(code(j.run().unwrap()), code(ExitCode::SUCCESS));
    drop(j);
    assert_eq!(
        dry.into_inner().unwrap(),
        [(vec!["EMAIL".to_string()], true)]
    );
    assert_eq!(
        fake.calls().len(),
        2,
        "the probe and host-info only: {:?}",
        fake.calls()
    );

    // No dot there: nothing installed; planned from the probe alone.
    let fake = FakeExec::new();
    fake.on("ssh me@box 'sh -c", Output::ok(LINUX));
    let r = remote(&fake);
    let seen = std::sync::Mutex::new(None);
    let mut j = job(&r, &root);
    j.dry_run = true;
    j.prepare = Some(Box::new(|info: &HostInfo, _| {
        *seen.lock().unwrap() = Some(info.clone());
        Ok(send::Prepared::default())
    }));
    j.run().unwrap();
    drop(j);
    assert_eq!(fake.calls().len(), 1, "{:?}", fake.calls());
    let info = seen.into_inner().unwrap().unwrap();
    assert_eq!(info.facts, "os=linux,arch=x86_64");
    assert_eq!(info.home, "/home/me");
}

#[test]
fn facts_travel_as_dot_facts_overrides() {
    use dot_core::facts::{Os, Role};
    let mut f = dot_core::Facts::new(Os::Linux, "aarch64", Role::Server);
    f.distro = Some("ubuntu".into());
    f.family = Some("debian".into());
    f.version = Some("24.04".into());
    f.codename = Some("noble".into());
    f.env = vec!["ssh".into(), "wsl".into()];
    f.pkg = Some("apt".into());
    f.init = Some("systemd".into());
    let s = facts_string(&f);
    assert_eq!(
        s,
        "os=linux,arch=aarch64,distro=ubuntu,family=debian,version=24.04,codename=noble,\
         role=server,env=ssh+wsl,pkg=apt,init=systemd"
    );
    let back = HostInfo {
        facts: s.clone(),
        ..HostInfo::default()
    }
    .facts()
    .unwrap();
    assert_eq!(back, (f.clone(), Vec::new()));
    // A newer dot there may report facts this one does not know: left out, named.
    let back = HostInfo {
        facts: format!("{s},newfact=1,odd\u{1b}[31m=x"),
        ..HostInfo::default()
    }
    .facts()
    .unwrap();
    assert_eq!(
        back,
        (f, vec!["newfact".to_string(), "odd?[31m".to_string()])
    );
}

#[test]
fn versions_compare_as_numbers() {
    assert_eq!(parse_version("dot 0.10.2"), Some((0, 10, 2)));
    assert_eq!(parse_version("dot 1.2.3-rc.1"), Some((1, 2, 3)));
    assert!(parse_version("dot 0.10.0") > parse_version("dot 0.9.9"));
    assert_eq!(parse_version("dot - graphviz version 2.43.0"), None);
    assert_eq!(parse_version("unknown"), None);
}

#[test]
fn the_probe_skips_a_dot_that_is_not_this_tool() {
    // The probe script itself, run by this machine's sh with a PATH whose `dot` is graphviz's.
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let local = tmp.path().join("home/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&local).unwrap();
    let script = |p: &Path, body: &str| {
        fs::write(p, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
    };
    script(&bin.join("dot"), "echo 'dot - graphviz version 2.43.0'");
    let out = |home: &Path| {
        let o = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(PROBE)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", home)
            .output()
            .unwrap();
        Probe::parse(&String::from_utf8_lossy(&o.stdout)).unwrap()
    };
    assert_eq!(out(&tmp.path().join("home")).dot, None);
    script(
        &local.join("dot"),
        concat!("echo 'dot ", env!("CARGO_PKG_VERSION"), "'"),
    );
    assert_eq!(
        out(&tmp.path().join("home")).dot,
        Some((
            local.join("dot").display().to_string(),
            concat!("dot ", env!("CARGO_PKG_VERSION"), "").into()
        ))
    );
}

/// Serves dot's release for Linux x86_64 through the API (asset 11 redirects to a storage
/// host) and records what was asked for.
#[derive(Default)]
struct Releases {
    asked: std::sync::Mutex<Vec<dot_platform::http::Request>>,
}

const API: &str = "https://api.test";

fn release_archive() -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(3);
    h.set_mode(0o755);
    h.set_entry_type(tar::EntryType::Regular);
    b.append_data(&mut h, "dot", &b"new"[..]).unwrap();
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut e, &b.into_inner().unwrap()).unwrap();
    e.finish().unwrap()
}

impl dot_platform::http::HttpClient for Releases {
    fn get(&self, req: &dot_platform::http::Request) -> Result<dot_platform::http::Response> {
        use sha2::Digest as _;
        self.asked.lock().unwrap().push(req.clone());
        let archive = release_archive();
        let name = "dot-x86_64-unknown-linux-musl.tar.gz";
        let base = format!("{API}/repos/trungngo/dot-cli/releases");
        let body = if req.url == format!("{base}/latest") {
            serde_json::json!({
                "tag_name": "v9.9.9",
                "assets": [
                    {"name": name, "id": 11, "browser_download_url": "https://github.test/a"},
                    {"name": "SHA256SUMS", "id": 12, "browser_download_url": "https://github.test/s"},
                ]
            })
            .to_string()
            .into_bytes()
        } else if req.url == format!("{base}/assets/11") {
            archive
        } else if req.url == format!("{base}/assets/12") {
            let sum: String = sha2::Sha256::digest(&archive)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("{sum}  {name}\n").into_bytes()
        } else {
            bail!("no fixture for {}", req.url);
        };
        Ok(dot_platform::http::Response {
            status: 200,
            headers: Vec::new(),
            content_length: Some(body.len() as u64),
            body: Box::new(std::io::Cursor::new(body)),
        })
    }
}

fn a_new_linux_box(root: &Path) -> Arc<FakeExec> {
    let fake = FakeExec::new();
    fake.on("ssh me@box 'sh -c", Output::ok(LINUX));
    fake.on(
        &format!("git -C {} ls-files", root.display()),
        Output::ok("dot.yml\0"),
    );
    fake.on("ssh me@box '/home/me/.local/bin/dot apply", Output::ok(""));
    fake.on("ssh -T -o EscapeChar=none me@box 'sh -s'", Output::ok(""));
    fake
}

const GH_TOKEN: &str = "ghp_FAKEremote123";

#[test]
fn with_a_token_dot_is_downloaded_here_and_the_token_never_leaves() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    let fake = a_new_linux_box(&root);
    let releases = Arc::new(Releases::default());
    let mut r = remote(&fake);
    r.github = Github::new(releases.clone(), API);
    r.github_token = || Some(SecretBytes::new(GH_TOKEN));
    job(&r, &root).run().unwrap();

    // The release for the remote's target, downloaded here: the token went to the API only.
    let asked = releases.asked.lock().unwrap().clone();
    let base = format!("{API}/repos/trungngo/dot-cli/releases");
    let urls: Vec<&str> = asked.iter().map(|r| r.url.as_str()).collect();
    assert_eq!(
        urls,
        [
            format!("{base}/latest"),
            format!("{base}/assets/12"),
            format!("{base}/assets/11")
        ]
    );
    for req in &asked {
        assert_eq!(
            req.get_header("authorization"),
            Some(format!("Bearer {GH_TOKEN}").as_str())
        );
    }
    // Copied over as the local binary would be; install.sh never ran there.
    let lines = fake.calls();
    assert!(!lines.iter().any(|l| l.contains("sh -s")), "{lines:?}");
    let scp = lines
        .iter()
        .find(|l| l.starts_with("scp -q "))
        .unwrap_or_else(|| panic!("{lines:?}"));
    let (src, dest) = scp["scp -q ".len()..].split_once(' ').unwrap();
    assert!(src.ends_with("/out/dot.new"), "{scp}");
    assert_eq!(dest, "me@box:/home/me/.local/bin/.dot.tmp");
    assert!(
        !Path::new(src).exists(),
        "the download is cleaned up: {src}"
    );
    assert!(lines.iter().any(|l| l.contains(
        "chmod 755 /home/me/.local/bin/.dot.tmp && mv -f /home/me/.local/bin/.dot.tmp /home/me/.local/bin/dot"
    )));
    // Nothing sent there holds the token.
    for c in fake.commands() {
        assert!(!c.line().contains(GH_TOKEN), "{}", c.line());
        let stdin = String::from_utf8_lossy(c.stdin.as_deref().unwrap_or_default()).into_owned();
        assert!(!stdin.contains(GH_TOKEN), "{}", c.line());
    }
}

#[test]
fn without_a_token_or_with_a_mirror_install_sh_runs_there_without_one() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());

    // A mirror needs no token: none is even looked up, none is sent.
    let fake = a_new_linux_box(&root);
    let mut r = remote(&fake);
    r.release_base = Some("https://mirror.test/dot".into());
    r.github_token = || panic!("no token is needed for a mirror");
    job(&r, &root).run().unwrap();
    let install = fake
        .commands()
        .into_iter()
        .find(|c| c.args.last().unwrap().ends_with("sh -s"))
        .unwrap();
    assert_eq!(
        install.args.last().unwrap(),
        "env DOT_RELEASE_BASE=https://mirror.test/dot sh -s"
    );
    assert_eq!(install.stdin.as_deref(), Some(INSTALL_SH.as_bytes()));

    // A fork's repository goes along too.
    let fake = a_new_linux_box(&root);
    let mut r = remote(&fake);
    r.release_repo = "someone/dot-fork".into();
    job(&r, &root).run().unwrap();
    let install = fake
        .commands()
        .into_iter()
        .find(|c| c.args.last().unwrap().ends_with("sh -s"))
        .unwrap();
    assert_eq!(
        install.args.last().unwrap(),
        "env DOT_RELEASE_REPO=someone/dot-fork sh -s"
    );

    // No token: install.sh there; when it fails, its explanation is shown, and what to do here.
    let fake = FakeExec::new();
    fake.on("ssh me@box 'sh -c", Output::ok(LINUX));
    fake.on(
        "ssh -T -o EscapeChar=none me@box 'sh -s'",
        Output {
            code: Some(1),
            stdout: String::new(),
            stderr: "install.sh: error: cannot download\n".into(),
        },
    );
    let r = remote(&fake);
    let err = format!("{:#}", job(&r, &root).run().unwrap_err());
    assert!(err.contains("install.sh failed there"), "{err}");
    assert!(err.contains("GITHUB_TOKEN set here"), "{err}");
    let install = fake
        .commands()
        .into_iter()
        .find(|c| c.args.last().unwrap() == "sh -s")
        .unwrap();
    assert_eq!(install.stdin.as_deref(), Some(INSTALL_SH.as_bytes()));
    assert!(
        !fake.calls().iter().any(|l| l.contains("dot apply")),
        "nothing runs after a failed install"
    );
}

// ---------------------------------------------------------------------------------------------
// --dot-binary: a file here, copied over; never a download.

const OURS: &str = concat!("dot ", env!("CARGO_PKG_VERSION"));
const TMP: &str = "/home/me/.local/bin/.dot.tmp";
const VERIFY: &str =
    "ssh me@box 'chmod 755 /home/me/.local/bin/.dot.tmp && /home/me/.local/bin/.dot.tmp --version'";

/// An HTTP client that fails the test when anything is asked of it.
struct NoNetwork;

impl dot_platform::http::HttpClient for NoNetwork {
    fn get(&self, req: &dot_platform::http::Request) -> Result<dot_platform::http::Response> {
        panic!("--dot-binary fetches nothing: {}", req.url);
    }
}

/// A remote that would download from GitHub with a token, or could copy this binary, but is
/// given `--dot-binary` (a file holding `header`): neither may happen.
fn with_binary(fake: &Arc<FakeExec>, dir: &Path, header: &[u8]) -> Remote {
    let file = dir.join("build/dot");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, header).unwrap();
    let mut r = remote(fake);
    r.github = Github::new(Arc::new(NoNetwork), API);
    r.github_token = || panic!("--dot-binary needs no token");
    r.local = Some(Target {
        os: "linux",
        arch: "x86_64",
    });
    r.dot_binary = Some(HostBinary::open(&file).unwrap());
    r
}

fn linux_x86() -> Vec<u8> {
    binary::fixtures::elf(62, false)
}

#[test]
fn dot_binary_is_copied_under_a_temporary_name_checked_there_then_renamed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    let fake = a_new_linux_box(&root);
    fake.on(VERIFY, Output::ok(format!("{OURS}\n")));
    let r = with_binary(&fake, tmp.path(), &linux_x86());
    assert_eq!(code(job(&r, &root).run().unwrap()), code(ExitCode::SUCCESS));
    let lines = fake.calls();
    let file = tmp.path().join("build/dot");
    let at = |l: &str| {
        lines
            .iter()
            .position(|x| x == l)
            .unwrap_or_else(|| panic!("{l}: {lines:#?}"))
    };
    let steps = [
        at("ssh me@box 'mkdir -p /home/me/.local/bin'"),
        at(&format!("scp -q {} me@box:{TMP}", file.display())),
        at(VERIFY),
        at(&format!("ssh me@box 'mv -f {TMP} /home/me/.local/bin/dot'")),
    ];
    assert!(steps.windows(2).all(|w| w[0] < w[1]), "{lines:#?}");
    assert_eq!(steps[0], 1, "right after the probe: {lines:#?}");
    // Nothing else installs it: no install.sh, no copy of this binary, no download.
    assert!(!lines.iter().any(|l| l.contains("sh -s")), "{lines:#?}");
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("scp")).count(),
        1,
        "{lines:#?}"
    );
    assert!(!lines.iter().any(|l| l.contains("rm -f")), "{lines:#?}");
    assert!(
        lines
            .last()
            .unwrap()
            .starts_with("ssh me@box '/home/me/.local/bin/dot apply"),
        "{lines:#?}"
    );
}

#[test]
fn a_dot_binary_for_another_platform_is_refused_before_anything_is_copied() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    for (header, says) in [
        (
            binary::fixtures::elf(183, false),
            "is a linux/aarch64 binary; me@box is linux/x86_64",
        ),
        (
            binary::fixtures::macho(0x0100_0007),
            "is a macos/x86_64 binary; me@box is linux/x86_64",
        ),
        (
            binary::fixtures::fat(&[0x0100_0007, 0x0100_000C]),
            "is a macos/x86_64+aarch64 (universal) binary",
        ),
    ] {
        let fake = a_new_linux_box(&root);
        let r = with_binary(&fake, tmp.path(), &header);
        let err = format!("{:#}", job(&r, &root).run().unwrap_err());
        assert!(err.contains(says), "{err}");
        assert!(err.contains("scripts/build-linux.sh x86_64"), "{err}");
        assert_eq!(fake.calls().len(), 1, "the probe only: {:?}", fake.calls());
    }
    // An aarch64 box takes the aarch64 build.
    let fake = a_new_linux_box(&root);
    fake.on(
        "ssh me@box 'sh -c",
        Output::ok("Linux aarch64\n/home/me\nno-rsync\n"),
    );
    fake.on(VERIFY, Output::ok(OURS));
    let r = with_binary(&fake, tmp.path(), &binary::fixtures::elf(183, false));
    job(&r, &root).run().unwrap();
    assert!(fake.calls().iter().any(|l| l.starts_with("scp -q")));
}

#[test]
fn a_dot_binary_of_another_version_or_that_does_not_run_there_replaces_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    for (out, says) in [
        (
            Output::ok("dot 0.0.9\n"),
            format!("says `dot 0.0.9`, not `{OURS}` (this dot)"),
        ),
        (
            Output::ok("dot - graphviz version 2.43.0\n"),
            "says `dot - graphviz version 2.43.0`".to_string(),
        ),
        (
            Output {
                code: Some(126),
                stdout: String::new(),
                stderr: "sh: 1: .dot.tmp: Exec format error\n".into(),
            },
            "does not run there (`--version` exited with code 126: sh: 1: .dot.tmp: Exec format \
             error); a Linux server needs a static (musl) build"
                .to_string(),
        ),
    ] {
        // An older dot there is not replaced either.
        let fake = a_new_linux_box(&root);
        fake.on(
            "ssh me@box 'sh -c",
            Output::ok("Linux x86_64\n/home/me\nno-rsync\n/home/me/.local/bin/dot\ndot 0.0.1\n"),
        );
        fake.on(VERIFY, out);
        let r = with_binary(&fake, tmp.path(), &linux_x86());
        let err = format!("{:#}", job(&r, &root).run().unwrap_err());
        assert!(err.contains(&says), "{err}");
        assert!(err.contains("nothing was replaced on me@box"), "{err}");
        let lines = fake.calls();
        let verify = lines.iter().position(|l| l == VERIFY).unwrap();
        assert_eq!(
            lines[verify + 1..],
            [format!("ssh me@box 'rm -f {TMP}'")],
            "the upload is removed, and nothing else runs"
        );
        assert!(!lines.iter().any(|l| l.contains("mv -f")), "{lines:#?}");
    }
}

#[test]
fn a_dot_of_this_version_or_newer_there_is_kept_an_older_one_upgraded() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    let probe = |dot: &str| Output::ok(format!("Linux x86_64\n/home/me\nno-rsync\n{dot}\n"));

    // This version, or a newer one: nothing copied.
    for (found, run) in [
        (
            format!("/home/me/.local/bin/dot\n{OURS}"),
            "/home/me/.local/bin/dot",
        ),
        ("/usr/bin/dot\ndot 99.0.0".to_string(), "/usr/bin/dot"),
    ] {
        let fake = a_new_linux_box(&root);
        fake.on("ssh me@box 'sh -c", probe(&found));
        let r = with_binary(&fake, tmp.path(), &linux_x86());
        job(&r, &root).run().unwrap();
        let lines = fake.calls();
        assert!(!lines.iter().any(|l| l.starts_with("scp")), "{lines:#?}");
        assert!(
            lines
                .last()
                .unwrap()
                .starts_with(&format!("ssh me@box '{run} apply")),
            "{lines:#?}"
        );
    }

    // An older one in ~/.local/bin: upgraded in place (no "update it there" refusal).
    let fake = a_new_linux_box(&root);
    fake.on(
        "ssh me@box 'sh -c",
        probe("/home/me/.local/bin/dot\ndot 0.0.1"),
    );
    fake.on(VERIFY, Output::ok(OURS));
    let r = with_binary(&fake, tmp.path(), &linux_x86());
    job(&r, &root).run().unwrap();
    assert!(
        fake.calls()
            .contains(&format!("ssh me@box 'mv -f {TMP} /home/me/.local/bin/dot'"))
    );

    // An older one first on the PATH, and an earlier copy in ~/.local/bin: that one is used.
    let fake = a_new_linux_box(&root);
    fake.on("ssh me@box 'sh -c", probe("/usr/bin/dot\ndot 0.0.1"));
    fake.on(
        "ssh me@box '/home/me/.local/bin/dot --version 2>/dev/null'",
        Output::ok(format!("{OURS}\n")),
    );
    let r = with_binary(&fake, tmp.path(), &linux_x86());
    job(&r, &root).run().unwrap();
    let lines = fake.calls();
    assert!(!lines.iter().any(|l| l.starts_with("scp")), "{lines:#?}");
    assert!(
        lines
            .last()
            .unwrap()
            .starts_with("ssh me@box '/home/me/.local/bin/dot apply"),
        "{lines:#?}"
    );

    // ... and without one there, it is copied there and used.
    let fake = a_new_linux_box(&root);
    fake.on("ssh me@box 'sh -c", probe("/usr/bin/dot\ndot 0.0.1"));
    fake.on(
        "ssh me@box '/home/me/.local/bin/dot --version",
        Output::exit(127, ""),
    );
    fake.on(VERIFY, Output::ok(OURS));
    let r = with_binary(&fake, tmp.path(), &linux_x86());
    job(&r, &root).run().unwrap();
    let lines = fake.calls();
    assert!(lines.iter().any(|l| l.starts_with("scp")), "{lines:#?}");
    assert!(
        lines
            .last()
            .unwrap()
            .starts_with("ssh me@box '/home/me/.local/bin/dot apply"),
        "{lines:#?}"
    );
}

#[test]
fn a_dry_run_with_a_dot_binary_checks_it_and_copies_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    let fake = a_new_linux_box(&root);
    let r = with_binary(&fake, tmp.path(), &linux_x86());
    let mut j = job(&r, &root);
    j.dry_run = true;
    j.run().unwrap();
    assert_eq!(fake.calls().len(), 1, "the probe only: {:?}", fake.calls());

    let fake = a_new_linux_box(&root);
    let r = with_binary(&fake, tmp.path(), &binary::fixtures::elf(183, false));
    let mut j = job(&r, &root);
    j.dry_run = true;
    let err = format!("{:#}", j.run().unwrap_err());
    assert!(err.contains("linux/aarch64 binary"), "{err}");
}

#[test]
fn without_a_dot_binary_an_older_dot_there_says_it_is_an_option() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    let fake = a_new_linux_box(&root);
    fake.on(
        "ssh me@box 'sh -c",
        Output::ok("Linux x86_64\n/home/me\nno-rsync\n/usr/bin/dot\ndot 0.0.1\n"),
    );
    let r = remote(&fake);
    let err = format!("{:#}", job(&r, &root).run().unwrap_err());
    assert!(err.contains("`/usr/bin/dot self-update`"), "{err}");
    assert!(err.contains("or pass --dot-binary"), "{err}");
    assert!(!fake.calls().iter().any(|l| l.starts_with("scp")));
}

#[test]
fn a_dot_of_this_version_there_is_kept_only_when_it_is_the_same_build() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    let probe = |dot: &str| Output::ok(format!("Linux x86_64\n/home/me\nno-rsync\n{dot}\n"));
    let sum_of = |f: &str| format!("ssh me@box 'sha256sum {f} 2>/dev/null || shasum -a 256 {f}");
    let local = "/home/me/.local/bin/dot";

    // The same build (sha256sum says so): nothing copied.
    let fake = a_new_linux_box(&root);
    fake.on("ssh me@box 'sh -c", probe(&format!("{local}\n{OURS}")));
    let r = with_binary(&fake, tmp.path(), &linux_x86());
    let sum = r.dot_binary.as_ref().unwrap().sha256.clone();
    fake.on(&sum_of(local), Output::ok(format!("{sum}  {local}\n")));
    job(&r, &root).run().unwrap();
    let lines = fake.calls();
    assert!(
        lines.iter().any(|l| l.starts_with(&sum_of(local))),
        "{lines:#?}"
    );
    assert!(!lines.iter().any(|l| l.starts_with("scp")), "{lines:#?}");

    // Another build of the same version (every build from source says it): replaced.
    let fake = a_new_linux_box(&root);
    fake.on("ssh me@box 'sh -c", probe(&format!("{local}\n{OURS}")));
    fake.on(
        &sum_of(local),
        Output::ok(format!("{}  {local}\n", "0".repeat(64))),
    );
    fake.on(VERIFY, Output::ok(OURS));
    let r = with_binary(&fake, tmp.path(), &linux_x86());
    job(&r, &root).run().unwrap();
    let lines = fake.calls();
    let at = |p: &str| lines.iter().position(|l| l.starts_with(p));
    assert!(at(&sum_of(local)) < at("scp -q"), "{lines:#?}");
    assert!(
        lines.contains(&format!("ssh me@box 'mv -f {TMP} {local}'")),
        "{lines:#?}"
    );

    // This version first on the PATH but another build, and another build in ~/.local/bin too:
    // that one is replaced, and used.
    let fake = a_new_linux_box(&root);
    fake.on("ssh me@box 'sh -c", probe(&format!("/usr/bin/dot\n{OURS}")));
    fake.on(&sum_of("/usr/bin/dot"), Output::ok("1".repeat(64)));
    fake.on(
        &format!("ssh me@box '{local} --version 2>/dev/null'"),
        Output::ok(format!("{OURS}\n")),
    );
    fake.on(&sum_of(local), Output::ok("2".repeat(64)));
    fake.on(VERIFY, Output::ok(OURS));
    let r = with_binary(&fake, tmp.path(), &linux_x86());
    job(&r, &root).run().unwrap();
    let lines = fake.calls();
    assert!(lines.iter().any(|l| l.starts_with("scp -q")), "{lines:#?}");
    assert!(
        lines
            .last()
            .unwrap()
            .starts_with(&format!("ssh me@box '{local} apply")),
        "{lines:#?}"
    );

    // ... and the same build there: used, nothing copied.
    let fake = a_new_linux_box(&root);
    fake.on("ssh me@box 'sh -c", probe(&format!("/usr/bin/dot\n{OURS}")));
    fake.on(&sum_of("/usr/bin/dot"), Output::ok("1".repeat(64)));
    fake.on(
        &format!("ssh me@box '{local} --version 2>/dev/null'"),
        Output::ok(format!("{OURS}\n")),
    );
    let r = with_binary(&fake, tmp.path(), &linux_x86());
    let sum = r.dot_binary.as_ref().unwrap().sha256.clone();
    fake.on(&sum_of(local), Output::ok(sum));
    job(&r, &root).run().unwrap();
    let lines = fake.calls();
    assert!(!lines.iter().any(|l| l.starts_with("scp")), "{lines:#?}");
    assert!(
        lines
            .last()
            .unwrap()
            .starts_with(&format!("ssh me@box '{local} apply")),
        "{lines:#?}"
    );
}

#[test]
fn a_dot_binary_check_that_loses_the_connection_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    let fake = a_new_linux_box(&root);
    fake.on(
        VERIFY,
        Output {
            code: Some(255),
            stdout: String::new(),
            stderr: "Connection to box closed by remote host.\n".into(),
        },
    );
    let r = with_binary(&fake, tmp.path(), &linux_x86());
    let err = format!("{:#}", job(&r, &root).run().unwrap_err());
    assert!(err.contains("cannot reach me@box"), "{err}");
    assert!(err.contains("Connection to box closed"), "{err}");
    assert!(err.contains("nothing was replaced"), "{err}");
    assert!(!err.contains("musl"), "{err}");
    assert!(!fake.calls().iter().any(|l| l.contains("mv -f")));
}

#[test]
fn a_dry_run_refuses_what_the_run_would_refuse() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(tmp.path());
    let fake = a_new_linux_box(&root);
    fake.on(
        "ssh me@box 'sh -c",
        Output::ok("Linux x86_64\n/home/me;x\nno-rsync\n"),
    );
    let r = with_binary(&fake, tmp.path(), &linux_x86());
    let mut j = job(&r, &root);
    j.dry_run = true;
    let err = format!("{:#}", j.run().unwrap_err());
    assert!(err.contains("an unusual path for scp"), "{err}");
}
