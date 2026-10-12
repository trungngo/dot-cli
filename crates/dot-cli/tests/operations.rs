//! Operations end to end: `dot status`, `dot diff`, `dot remove`, `dot adopt`, `dot
//! doctor`, the run lock and the version-0 state migration. A temp repository, home, config and
//! state directory; `PATH` holds fake `sudo` / `git` / `apt-get` scripts and the system
//! directories. Nothing touches the real home, runs sudo or a package manager, or uses the
//! network.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

const SUDO: &str = "#!/bin/sh\n[ \"$1\" = \"-n\" ] && shift\ncase \"$1\" in true|-v) exit 0 ;; esac\nexec \"$@\"\n";
const GIT: &str = "#!/bin/sh\necho 'git version 2.99.0'\n";

const DOT_YML: &str =
    "bundles: [zsh, extra]\nvars:\n  api_pin: { default: \"4455-secret\", sensitive: true }\n";

const ZSH: &str = "# the shell\nname: zsh\nsteps:\n  # links\n  - link:\n      ~/.zshrc: zshrc\n  - template:\n      ~/.config/app.conf: app.conf.j2\n  - shell: echo hi\n";

const EXTRA: &str =
    "requires: [zsh]\nsteps:\n  - create: [~/extra-dir]\n  - link: { ~/.extrarc: extrarc }\n";

struct Env {
    _tmp: TempDir,
    root: PathBuf,
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["home/.config", "config/dot", "state", "logs", "bin"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let e = Self { _tmp: tmp, root };
        e.write("repo/dot.yml", DOT_YML);
        e.write("repo/bundles/zsh/bundle.yml", ZSH);
        e.write("repo/bundles/zsh/zshrc", "# repo zshrc\n");
        e.write(
            "repo/bundles/zsh/app.conf.j2",
            "name=app\npin={{ api_pin }}\nmode=fast\n",
        );
        e.write("repo/bundles/extra/bundle.yml", EXTRA);
        e.write("repo/bundles/extra/extrarc", "extra\n");
        e.script("sudo", SUDO);
        e.script("git", GIT);
        e
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn script(&self, name: &str, body: &str) {
        let p = self.root.join("bin").join(name);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn state(&self) -> PathBuf {
        self.root.join("state/dot")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_dot"));
        c.args(args)
            .current_dir(self.root.join("repo"))
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("HOME", self.home())
            .env("USER", "tester")
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("DOT_ROLE", "server")
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        c
    }

    fn dot(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn apply(&self, extra: &[&str]) -> Output {
        let logs = self.root.join("logs");
        let mut args = vec![
            "apply",
            "--yes",
            "--plain",
            "--log-dir",
            logs.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.dot(&args)
    }

    fn state_json(&self) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(self.state().join("state.json")).unwrap()).unwrap()
    }
}

fn ok(o: &Output) -> String {
    let t = text(o);
    assert!(o.status.success(), "{t}");
    t
}

fn fails(o: &Output) -> String {
    let t = text(o);
    assert!(!o.status.success(), "{t}");
    t
}

#[test]
fn status_and_diff_report_drift_orphans_and_never_show_secrets() {
    let e = Env::new();
    fs::write(e.home().join(".zshrc"), "# my old zshrc\n").unwrap();
    ok(&e.apply(&[]));

    // Everything recorded, with the bundle that manages it.
    let st = e.state_json();
    assert_eq!(st["version"], 1);
    let res = &st["resources"];
    assert_eq!(res["link:~/.zshrc"]["bundle"], "zsh");
    assert_eq!(res["link:~/.zshrc"]["owned"], true);
    assert_eq!(res["file:~/.config/app.conf"]["bundle"], "zsh");
    assert_eq!(res["dir:~/extra-dir"]["bundle"], "extra");
    assert!(
        res["link:~/.zshrc"]["restore"]["path"]
            .as_str()
            .unwrap()
            .ends_with(".zshrc.dot-backup")
    );

    let t = ok(&e.dot(&["status"]));
    assert!(t.contains("status: in sync"), "{t}");
    assert!(t.contains("run on every apply: zsh: shell: echo hi"), "{t}");
    assert!(ok(&e.dot(&["diff"])).contains("no differences"));

    // Drift: a deleted link, an edited file.
    fs::remove_file(e.home().join(".zshrc")).unwrap();
    fs::write(
        e.home().join(".config/app.conf"),
        "name=app\npin=4455-secret\nmode=slow\n",
    )
    .unwrap();
    let t = fails(&e.dot(&["status"]));
    assert!(t.contains("drift:"), "{t}");
    assert!(t.contains("zsh: link ~/.zshrc -> zshrc [pending]"), "{t}");
    assert!(
        t.contains("zsh: template ~/.config/app.conf <- app.conf.j2"),
        "{t}"
    );
    assert!(t.contains("status: 2 drifted, 0 orphans"), "{t}");

    let t = fails(&e.dot(&["diff"]));
    assert!(t.contains("# zsh: link ~/.zshrc -> zshrc [pending]"), "{t}");
    assert!(t.contains("link ~/.zshrc\n- (nothing)\n+ link to "), "{t}");
    assert!(t.contains("--- ~/.config/app.conf\n"), "{t}");
    assert!(
        t.contains("+++ ~/.config/app.conf (rendered from app.conf.j2)\n"),
        "{t}"
    );
    assert!(t.contains("-mode=slow\n+mode=fast\n"), "{t}");
    assert!(t.contains(" pin=***\n"), "{t}");
    assert!(!t.contains("4455-secret"), "a sensitive value leaked: {t}");
    // One bundle (and what it requires) only.
    let t = fails(&e.dot(&["diff", "zsh"]));
    assert!(!t.contains("extra"), "{t}");

    // Orphans: a deselected bundle, and a step removed from a bundle.
    ok(&e.apply(&[]));
    e.write("repo/dot.yml", &DOT_YML.replace("[zsh, extra]", "[zsh]"));
    e.write(
        "repo/bundles/zsh/bundle.yml",
        &ZSH.replace("  - template:\n      ~/.config/app.conf: app.conf.j2\n", ""),
    );
    let t = fails(&e.dot(&["status"]));
    assert!(
        t.contains("dir:~/extra-dir (bundle extra is not selected)"),
        "{t}"
    );
    assert!(
        t.contains("link:~/.extrarc (bundle extra is not selected)"),
        "{t}"
    );
    assert!(
        t.contains("file:~/.config/app.conf (no longer in bundle zsh)"),
        "{t}"
    );
    assert!(t.contains("status: 0 drifted, 3 orphans"), "{t}");
    // A deleted bundle.
    fs::remove_dir_all(e.root.join("repo/bundles/extra")).unwrap();
    let t = fails(&e.dot(&["status"]));
    assert!(t.contains("(bundle extra no longer exists)"), "{t}");

    // An unreadable state store: orphans are unknown, never "in sync".
    e.write("repo/dot.yml", DOT_YML);
    e.write("repo/bundles/zsh/bundle.yml", ZSH);
    e.write("repo/bundles/extra/bundle.yml", EXTRA);
    e.write("repo/bundles/extra/extrarc", "extra\n");
    ok(&e.apply(&[]));
    let store = e.state().join("state.json");
    fs::write(&store, "{bad").unwrap();
    let t = fails(&e.dot(&["status"]));
    assert!(
        t.contains("orphans unknown (the state store cannot be read"),
        "{t}"
    );
    assert!(!t.contains("in sync"), "{t}");
}

#[test]
fn a_bundle_applied_by_name_is_part_of_the_machine_until_removed() {
    let e = Env::new();
    // An opt-in bundle: not selected, applied on purpose.
    e.write("repo/dot.yml", &DOT_YML.replace("[zsh, extra]", "[zsh]"));
    ok(&e.apply(&[]));
    ok(&e.apply(&["extra"]));
    let t = ok(&e.dot(&["status"]));
    assert!(t.contains("status: in sync"), "{t}");
    // Its drift is reported like any other bundle's.
    fs::remove_dir(e.home().join("extra-dir")).unwrap();
    let t = fails(&e.dot(&["status"]));
    assert!(t.contains("extra: create ~/extra-dir [pending]"), "{t}");
    ok(&e.apply(&["extra"]));
    // Removed: neither checked nor an orphan.
    ok(&e.dot(&["remove", "extra", "--yes"]));
    let t = ok(&e.dot(&["status"]));
    assert!(t.contains("status: in sync"), "{t}");
    assert!(!t.contains("extra"), "{t}");
    // `status --no-deps`, as `apply --no-deps`: only the named bundle.
    let t = ok(&e.dot(&["status", "--no-deps", "zsh"]));
    assert!(t.contains("status: in sync"), "{t}");
}

#[test]
fn a_sysroot_takes_root_owned_files_so_a_sandbox_converges_them() {
    let e = Env::new();
    e.write("repo/dot.yml", "bundles: [sys]\n");
    e.write(
        "repo/bundles/sys/bundle.yml",
        concat!(
            "steps:\n",
            "  - template: { /etc/sysctl.d/60-dot.conf: { src: s.j2, root: true } }\n",
            "  - create: { /opt/dot-test: { root: true } }\n",
            "  - block: { file: /etc/hosts.dot-test, content: \"10.0.0.1 box\", root: true }\n",
            "  - absent: { /etc/old-dot-test.conf: { root: true } }\n",
        ),
    );
    e.write("repo/bundles/sys/s.j2", "vm.swappiness = 10\n");
    // No sudo at all: the sandbox is not root, and nothing may reach the real /etc.
    fs::remove_file(e.root.join("bin/sudo")).unwrap();
    let sys = e.root.join("sysroot");
    fs::create_dir_all(sys.join("etc")).unwrap();
    fs::write(sys.join("etc/old-dot-test.conf"), "old\n").unwrap();
    let run = |args: &[&str]| {
        let mut c = e.command(args);
        c.env("DOT_SYSROOT", &sys);
        c.output().unwrap()
    };
    let logs = e.root.join("logs");
    let apply = [
        "apply",
        "--yes",
        "--plain",
        "--log-dir",
        logs.to_str().unwrap(),
    ];
    let t = ok(&run(&apply));
    assert!(!t.contains("sudo"), "{t}");
    assert_eq!(
        fs::read_to_string(sys.join("etc/sysctl.d/60-dot.conf")).unwrap(),
        "vm.swappiness = 10\n"
    );
    assert!(sys.join("opt/dot-test").is_dir());
    assert!(
        fs::read_to_string(sys.join("etc/hosts.dot-test"))
            .unwrap()
            .contains("10.0.0.1 box")
    );
    assert!(!sys.join("etc/old-dot-test.conf").exists());
    // Converged: a second run changes nothing, and status is in sync.
    let t = ok(&run(&apply));
    assert!(t.contains("0 changed"), "{t}");
    let t = ok(&run(&["status"]));
    assert!(t.contains("status: in sync"), "{t}");
}

#[test]
fn remove_refuses_dependents_then_cascades_and_puts_things_back() {
    let e = Env::new();
    fs::write(e.home().join(".zshrc"), "# my old zshrc\n").unwrap();
    ok(&e.apply(&[]));
    assert!(e.home().join("extra-dir").is_dir());

    let t = fails(&e.dot(&["remove", "zsh", "--yes"]));
    assert!(t.contains("extra requires zsh"), "{t}");
    assert!(t.contains("--cascade"), "{t}");
    assert!(e.home().join(".zshrc").is_symlink());

    let t = ok(&e.dot(&["remove", "zsh", "--cascade", "--dry-run"]));
    assert!(t.contains("would remove bundle extra:"), "{t}");
    assert!(t.contains("would remove bundle zsh:"), "{t}");
    assert!(t.contains("remove link ~/.zshrc -> "), "{t}");
    assert!(
        t.contains("put back ~/.zshrc from ~/.zshrc.dot-backup"),
        "{t}"
    );
    assert!(t.contains("remove directory ~/extra-dir"), "{t}");
    assert!(t.contains("note: extra, zsh still selected"), "{t}");
    assert!(
        e.home().join(".zshrc").is_symlink(),
        "dry run changed nothing"
    );

    let t = ok(&e.dot(&["remove", "zsh", "--cascade", "--yes"]));
    // Nothing to confirm: each removal is printed once, as it is done.
    assert!(!t.contains("remove bundle"), "{t}");
    assert_eq!(t.matches("remove link ~/.zshrc -> ").count(), 1, "{t}");
    // Dependents first.
    let (x, z) = (t.find("extra: ").unwrap(), t.find("zsh: ").unwrap());
    assert!(x < z, "{t}");
    assert_eq!(
        fs::read_to_string(e.home().join(".zshrc")).unwrap(),
        "# my old zshrc\n"
    );
    assert!(!e.home().join(".zshrc").is_symlink());
    assert!(!e.home().join(".zshrc.dot-backup").exists());
    assert!(!e.home().join(".extrarc").exists());
    assert!(!e.home().join("extra-dir").exists());
    assert!(!e.home().join(".config/app.conf").exists());
    let st = e.state_json();
    assert_eq!(st["resources"], serde_json::json!({}), "{st}");

    // Nothing recorded any more.
    let t = ok(&e.dot(&["remove", "extra"]));
    assert!(t.contains("nothing recorded for extra"), "{t}");
    let t = fails(&e.dot(&["remove", "nope"]));
    assert!(t.contains("unknown bundle `nope`"), "{t}");
}

#[test]
fn a_file_changed_since_dot_wrote_it_is_left_and_the_removal_fails() {
    let e = Env::new();
    ok(&e.apply(&[]));
    fs::write(e.home().join(".config/app.conf"), "mine now\n").unwrap();
    let t = ok(&e.dot(&["remove", "extra", "--yes"]));
    assert!(t.contains("removed extra"), "{t}");
    let t = fails(&e.dot(&["remove", "zsh", "--cascade", "--yes"]));
    assert!(
        t.contains("conflict: ~/.config/app.conf changed since dot wrote it"),
        "{t}"
    );
    assert_eq!(
        fs::read_to_string(e.home().join(".config/app.conf")).unwrap(),
        "mine now\n"
    );
    assert!(!e.home().join(".zshrc").exists(), "the rest was removed");
    let st = e.state_json();
    let left: Vec<&String> = st["resources"].as_object().unwrap().keys().collect();
    assert_eq!(left, ["file:~/.config/app.conf"]);
}

#[test]
fn adopt_moves_a_file_into_a_bundle_links_it_and_remove_puts_a_copy_back() {
    let e = Env::new();
    fs::write(e.home().join(".gitconfig"), "[user]\n  name = me\n").unwrap();
    fs::write(e.home().join(".vimrc"), "set nu\n").unwrap();

    let t = ok(&e.dot(&["adopt", "~/.gitconfig", "--dry-run"]));
    assert!(t.contains("would move file ~/.gitconfig to "), "{t}");
    assert!(t.contains("would create bundle gitconfig"), "{t}");
    assert!(!e.home().join(".gitconfig").is_symlink());

    // A new bundle.
    let gc = e.home().join(".gitconfig");
    ok(&e.dot(&["adopt", gc.to_str().unwrap(), "--bundle", "git"]));
    let moved = e.root.join("repo/bundles/git/gitconfig");
    assert_eq!(fs::read_to_string(&moved).unwrap(), "[user]\n  name = me\n");
    assert_eq!(fs::read_link(&gc).unwrap(), moved);
    assert_eq!(
        fs::read_to_string(e.root.join("repo/bundles/git/bundle.yml")).unwrap(),
        "steps:\n  - link:\n      ~/.gitconfig: gitconfig\n"
    );

    // An existing bundle: the step is appended, comments and layout kept.
    let vimrc = e.home().join(".vimrc");
    ok(&e.dot(&["adopt", vimrc.to_str().unwrap(), "--bundle", "zsh"]));
    let yml = fs::read_to_string(e.root.join("repo/bundles/zsh/bundle.yml")).unwrap();
    assert_eq!(
        yml,
        format!("{ZSH}  - link:\n      ~/.vimrc: vimrc\n"),
        "{yml}"
    );
    ok(&e.dot(&["check"]));
    // Refused: a link already, outside home, taken name.
    let t = fails(&e.dot(&["adopt", vimrc.to_str().unwrap(), "--bundle", "zsh"]));
    assert!(t.contains("is a link already"), "{t}");
    let outside = e.root.join("outside");
    fs::write(&outside, "x").unwrap();
    let t = fails(&e.dot(&["adopt", outside.to_str().unwrap()]));
    assert!(t.contains("not inside the home directory"), "{t}");
    fs::write(e.home().join(".zshrc2"), "x").unwrap();
    let t = fails(&e.dot(&[
        "adopt",
        e.home().join(".zshrc2").to_str().unwrap(),
        "--bundle",
        "zsh",
        "--name",
        "zshrc",
    ]));
    assert!(t.contains("already exists in bundle zsh"), "{t}");

    // The adopted link is dot's: removing the bundle puts a copy of the file back.
    let st = e.state_json();
    assert_eq!(st["resources"]["link:~/.gitconfig"]["bundle"], "git");
    ok(&e.dot(&["remove", "git", "--yes"]));
    assert!(!gc.is_symlink());
    assert_eq!(fs::read_to_string(&gc).unwrap(), "[user]\n  name = me\n");
    assert!(moved.exists(), "the repository keeps its copy");
}

#[test]
fn doctor_checks_the_machine_and_clears_a_stale_lock() {
    let e = Env::new();
    e.write(
        "repo/secrets.yml",
        "default_provider: envfile\nsecrets:\n  tok: { ref: \"envfile:TOK\" }\n",
    );
    e.write("config/dot/secrets.env", "TOK=x\n");
    let secrets = e.root.join("config/dot/secrets.env");
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o644)).unwrap();
    fs::create_dir_all(e.state()).unwrap();
    fs::write(
        e.state().join("run.json"),
        r#"{"pid":999999,"command":"apply","started":1}"#,
    )
    .unwrap();

    // Only the fakes on PATH: whether this machine has apt (an Ubuntu runner does) must not
    // matter. The fakes are `#!/bin/sh` scripts, which need no PATH.
    let out = e
        .command(&["doctor"])
        .env("DOT_FACTS", "os=linux,family=debian,distro=ubuntu,pkg=apt")
        .env("PATH", e.root.join("bin"))
        .output()
        .unwrap();
    let t = fails(&out);
    assert!(t.contains("ok    repo: "), "{t}");
    assert!(t.contains("ok    profile: none"), "{t}");
    assert!(
        t.contains("warn  package manager: apt is not on PATH"),
        "{t}"
    );
    assert!(
        t.contains("ok    sudo: available without a password"),
        "{t}"
    );
    assert!(
        t.contains("ok    secrets provider: envfile available"),
        "{t}"
    );
    assert!(t.contains("FAIL  secrets file: "), "{t}");
    assert!(t.contains("ok    git: git version 2.99.0"), "{t}");
    assert!(t.contains("ok    clock: sane"), "{t}");
    assert!(t.contains("ok    state: "), "{t}");
    assert!(t.contains("warn  locks: `dot apply`, pid 999999"), "{t}");
    assert!(
        t.contains("warn  PATH: does not contain ~/.local/bin"),
        "{t}"
    );
    assert!(
        !e.state().join("run.json").exists(),
        "the stale record is gone"
    );

    // Fixed: private secrets, apt present, ~/.local/bin on PATH.
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o600)).unwrap();
    e.script("apt-get", "#!/bin/sh\nexit 0\n");
    e.script("dpkg-query", "#!/bin/sh\nexit 0\n");
    let out = e
        .command(&["doctor"])
        .env("DOT_FACTS", "os=linux,family=debian,distro=ubuntu,pkg=apt")
        .env(
            "PATH",
            format!(
                "{}:{}",
                e.home().join(".local/bin").display(),
                e.root.join("bin").display()
            ),
        )
        .output()
        .unwrap();
    let t = ok(&out);
    assert!(t.contains("ok    secrets file: "), "{t}");
    assert!(t.contains("secrets.env (private, nothing missing)"), "{t}");
    assert!(t.contains("ok    package manager: apt found"), "{t}");
    assert!(t.contains("ok    locks: no dot run in progress"), "{t}");
    assert!(t.contains("ok    PATH: contains ~/.local/bin"), "{t}");
    assert!(t.contains("doctor: 0 problems"), "{t}");

    // A broken repository is a failure, not a crash.
    e.write("repo/dot.yml", "bundles: [\n");
    let t = fails(&e.dot(&["doctor"]));
    assert!(t.contains("FAIL  repo: "), "{t}");
}

#[test]
fn two_runs_never_interleave() {
    let e = Env::new();
    e.write(
        "repo/bundles/zsh/bundle.yml",
        // Holds the run (and its lock) until the test says so, at most a minute.
        "steps:\n  - shell: 'i=0; while [ ! -e \"$HOME/release\" ] && [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done'\n  - link: { ~/.zshrc: zshrc }\n",
    );
    e.write("repo/dot.yml", "bundles: [zsh]\n");
    let logs = e.root.join("logs");
    let first = e
        .command(&[
            "apply",
            "--yes",
            "--plain",
            "--log-dir",
            logs.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Wait until the first run holds the lock.
    let info = e.state().join("run.json");
    for _ in 0..3000 {
        if info.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !info.exists() {
        let out = first.wait_with_output().unwrap();
        panic!("the first run never took the lock: {}", text(&out));
    }
    let t = fails(&e.apply(&[]));
    assert!(t.contains("another dot run is in progress"), "{t}");
    assert!(t.contains("`dot apply`"), "{t}");
    // Read-only commands and dry runs do not wait.
    ok(&e.apply(&["--dry-run"]));
    let status = e.dot(&["status"]);
    assert!(text(&status).contains("zsh"), "{}", text(&status));
    assert!(
        !e.home().join(".zshrc").exists(),
        "the first run is still waiting"
    );
    fs::write(e.home().join("release"), "").unwrap();
    let out = first.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(!info.exists(), "the lock record goes with the run");
    ok(&e.apply(&[]));
}

#[test]
fn version_0_state_files_are_migrated_on_first_use() {
    let e = Env::new();
    fs::create_dir_all(e.state()).unwrap();
    let rc = e.home().join(".zshrc");
    let src = e.root.join("repo/bundles/zsh/zshrc");
    fs::write(
        e.state().join("links.json"),
        format!(
            r#"{{"links":{{"{0}":{{"dest":"{0}","src":"{1}","kind":"junction"}}}}}}"#,
            rc.display(),
            src.display()
        ),
    )
    .unwrap();
    fs::write(
        e.state().join("env.json"),
        r#"{"path":[{"dir":"/opt/tool/bin","position":"prepend","bundle":"zsh"}],"vars":[]}"#,
    )
    .unwrap();
    fs::write(e.state().join("apt-update.stamp"), "1700000000\n").unwrap();
    fs::create_dir_all(e.state().join("services")).unwrap();
    fs::write(
        e.state().join("services/systemd-user-x.service"),
        "/h/x.service",
    )
    .unwrap();

    let t = ok(&e.dot(&["env", "--shell", "bash"]));
    assert!(t.contains("/opt/tool/bin"), "{t}");
    let st = e.state_json();
    assert_eq!(st["version"], 1);
    assert_eq!(
        st["sections"]["links"][rc.to_str().unwrap()]["kind"],
        "junction"
    );
    assert_eq!(st["sections"]["apt"]["refreshed"], 1_700_000_000u64);
    assert_eq!(
        st["sections"]["services"]["systemd-user-x.service"],
        "/h/x.service"
    );
    assert_eq!(st["env"]["path"][0]["dir"], "/opt/tool/bin");
    for old in ["links.json", "env.json", "apt-update.stamp", "services"] {
        assert!(!e.state().join(old).exists(), "{old}");
        assert!(e.state().join("legacy").join(old).exists(), "{old}");
    }
    let t = ok(&e.dot(&["doctor"]));
    assert!(t.contains("migrated from the version-0 files"), "{t}");
}

/// Four bundles, two of them requiring a third, and an unrelated one.
const JOBS_DOT_YML: &str = "bundles: [base, app, tools, solo]\n";
const JOBS_BASE: &str = "steps:\n  - link: { ~/.baserc: baserc }\n  - create: [~/base-dir]\n";
const JOBS_APP: &str = r#"requires: [base]
steps:
  - template:
      ~/.config/app/app.conf: app.conf.j2
    notify: [reload]
  - copy:
      ~/.config/app/extra.conf: extra.conf
    notify: [reload]
handlers:
  reload:
    shell: echo reload >> "$HOME/handler.log"
"#;
const JOBS_TOOLS: &str = r#"requires: [base]
steps:
  - link: { ~/.toolsrc: toolsrc }
  - block:
      file: ~/.profile
      marker: tools
      content: export TOOLS={{ home }}/tools
"#;
const JOBS_SOLO: &str = "steps:\n  - link: { ~/.solorc: solorc }\n";

fn handler_runs(e: &Env) -> usize {
    fs::read_to_string(e.home().join("handler.log"))
        .unwrap_or_default()
        .lines()
        .count()
}

#[test]
fn parallel_bundles_handlers_status_diff_adopt_and_cascading_removal() {
    let e = Env::new();
    fs::remove_dir_all(e.root.join("repo/bundles")).unwrap();
    e.write("repo/dot.yml", JOBS_DOT_YML);
    e.write("repo/bundles/base/bundle.yml", JOBS_BASE);
    e.write("repo/bundles/base/baserc", "base\n");
    e.write("repo/bundles/app/bundle.yml", JOBS_APP);
    e.write(
        "repo/bundles/app/app.conf.j2",
        "name=app\nhome={{ home }}\n",
    );
    e.write("repo/bundles/app/extra.conf", "extra=1\n");
    e.write("repo/bundles/tools/bundle.yml", JOBS_TOOLS);
    e.write("repo/bundles/tools/toolsrc", "tools\n");
    e.write("repo/bundles/solo/bundle.yml", JOBS_SOLO);
    e.write("repo/bundles/solo/solorc", "solo\n");
    fs::write(e.home().join(".profile"), "export MINE=1\n").unwrap();

    // Three at a time: base first, then app, tools and solo together. The handler both app
    // steps notify runs once, after them.
    let t = ok(&e.apply(&["--jobs", "3"]));
    assert_eq!(handler_runs(&e), 1, "{t}");
    for f in [".baserc", ".toolsrc", ".solorc"] {
        assert!(e.home().join(f).is_symlink(), "{f}: {t}");
    }
    assert!(e.home().join("base-dir").is_dir());
    assert_eq!(
        fs::read_to_string(e.home().join(".config/app/extra.conf")).unwrap(),
        "extra=1\n"
    );
    let profile = fs::read_to_string(e.home().join(".profile")).unwrap();
    assert!(profile.starts_with("export MINE=1\n"), "{profile}");
    assert!(profile.contains("export TOOLS="), "{profile}");
    // Plain output reads as a sequential run, whatever finished first: each bundle's lines
    // together, in plan order.
    let order: Vec<usize> = ["base", "app", "solo", "tools"]
        .iter()
        .map(|b| t.find(&format!("{b}:")).unwrap_or(usize::MAX))
        .collect();
    assert!(order.iter().all(|&i| i != usize::MAX), "{t}");

    // Every bundle's resources were recorded by concurrent workers, none lost; the handler's
    // run is an event, not a resource.
    let st = e.state_json();
    let res = st["resources"].as_object().unwrap();
    for (key, bundle) in [
        ("link:~/.baserc", "base"),
        ("dir:~/base-dir", "base"),
        ("file:~/.config/app/app.conf", "app"),
        ("file:~/.config/app/extra.conf", "app"),
        ("link:~/.toolsrc", "tools"),
        ("block:~/.profile [dot:tools:tools]", "tools"),
        ("link:~/.solorc", "solo"),
    ] {
        assert_eq!(res[key]["bundle"], bundle, "{key}: {st}");
    }
    assert_eq!(res.len(), 7, "{st}");
    assert!(
        st["sections"].get("handlers").is_none(),
        "nothing due: {st}"
    );

    // Converged: nothing changes, the handler does not run again.
    ok(&e.apply(&["-j", "3"]));
    assert_eq!(handler_runs(&e), 1);
    let t = ok(&e.dot(&["status"]));
    assert!(t.contains("status: in sync"), "{t}");

    // Manual drift: status and diff see it, applying puts it back and notifies once more.
    let conf = e.home().join(".config/app/app.conf");
    fs::write(&conf, "name=mine\n").unwrap();
    let t = fails(&e.dot(&["status"]));
    assert!(
        t.contains("app: template ~/.config/app/app.conf <- app.conf.j2"),
        "{t}"
    );
    // The handler it would notify is due too.
    assert!(t.contains("app: handler reload: run `echo reload"), "{t}");
    assert!(t.contains("status: 2 drifted, 0 orphans"), "{t}");
    let t = fails(&e.dot(&["diff", "app"]));
    assert!(t.contains("-name=mine\n+name=app\n"), "{t}");
    ok(&e.apply(&["-j", "3"]));
    assert_eq!(handler_runs(&e), 2);
    assert!(ok(&e.dot(&["status"])).contains("status: in sync"));

    // A handler that fails stays due in the state store: `dot status` shows it, and the next
    // run fires it although nothing notifies it any more.
    let app_yml = e.root.join("repo/bundles/app/bundle.yml");
    e.write(
        "repo/bundles/app/bundle.yml",
        &JOBS_APP.replace("echo reload >>", "exit 3; echo reload >>"),
    );
    fs::write(&conf, "name=mine\n").unwrap();
    fails(&e.apply(&["-j", "3"]));
    assert_eq!(handler_runs(&e), 2);
    assert_eq!(
        e.state_json()["sections"]["handlers"],
        serde_json::json!({ "app": ["reload"] })
    );
    fs::write(&app_yml, JOBS_APP).unwrap();
    let t = fails(&e.dot(&["status"]));
    assert!(t.contains("app: handler reload: run `echo reload"), "{t}");
    assert!(t.contains("status: 1 drifted, 0 orphans"), "{t}");
    ok(&e.apply(&["-j", "3"]));
    assert_eq!(handler_runs(&e), 3);
    assert!(e.state_json()["sections"].get("handlers").is_none());
    assert!(ok(&e.dot(&["status"])).contains("status: in sync"));

    // Adopt into tools, which base's removal cascades to.
    let gc = e.home().join(".gitconfig");
    fs::write(&gc, "[user]\n  name = me\n").unwrap();
    ok(&e.dot(&["adopt", gc.to_str().unwrap(), "--bundle", "tools"]));
    assert!(gc.is_symlink());
    assert!(ok(&e.dot(&["status"])).contains("status: in sync"));

    // Removing base takes app and tools with it (dependents first); solo stays.
    let t = fails(&e.dot(&["remove", "base", "--yes"]));
    assert!(t.contains("--cascade"), "{t}");
    let t = ok(&e.dot(&["remove", "base", "--cascade", "--yes"]));
    let (app, tools, base) = (
        t.find("app: ").unwrap(),
        t.find("tools: ").unwrap(),
        t.rfind("base: ").unwrap(),
    );
    assert!(app < base && tools < base, "{t}");
    for f in [".baserc", ".toolsrc", "base-dir", ".config/app/app.conf"] {
        assert!(!e.home().join(f).exists(), "{f}: {t}");
    }
    assert_eq!(
        fs::read_to_string(e.home().join(".profile")).unwrap(),
        "export MINE=1\n",
        "the block is gone, the rest kept"
    );
    assert!(!gc.is_symlink());
    assert_eq!(fs::read_to_string(&gc).unwrap(), "[user]\n  name = me\n");
    assert!(e.home().join(".solorc").is_symlink());
    let st = e.state_json();
    let left: Vec<&String> = st["resources"].as_object().unwrap().keys().collect();
    assert_eq!(left, ["link:~/.solorc"], "{st}");
    assert_eq!(handler_runs(&e), 3, "removing runs no handler");
}
