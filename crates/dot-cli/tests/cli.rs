//! Runs the built `dot` binary against temp bundles. HOME, XDG_STATE_HOME and the log directory
//! all point into temp dirs, so the real home directory is never touched. Tests that resolve the
//! home directory are Unix-only: on Windows `dirs::home_dir` ignores environment overrides.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use tempfile::TempDir;

struct Env {
    _tmp: TempDir,
    root: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["home", "logs", "repo/bundles/demo", "state"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("repo/dot.yml"), "").unwrap();
        Self { _tmp: tmp, root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    #[cfg(unix)]
    fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    fn bundle_dir(&self) -> PathBuf {
        self.root.join("repo/bundles/demo")
    }

    fn write_bundle(&self, yaml: &str) -> PathBuf {
        let p = self.bundle_dir().join("bundle.yml");
        fs::write(&p, yaml).unwrap();
        p
    }

    fn dot(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dot"))
            .args(args)
            .current_dir(&self.root)
            .env("HOME", self.home())
            .env_remove("DOT_STATE_DIR")
            .env("USERPROFILE", self.home())
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("LC_ALL", "C")
            .output()
            .unwrap()
    }

    #[cfg(unix)]
    fn apply(&self, extra: &[&str]) -> Output {
        let bundle = self.bundle_dir().join("bundle.yml");
        let logs = self.logs();
        let mut args = vec![
            "apply",
            "-c",
            bundle.to_str().unwrap(),
            "--log-dir",
            logs.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.dot(&args)
    }

    #[cfg(unix)]
    fn log_files(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(self.logs())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        v.sort();
        v
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Strips the plain renderer's `HH:MM:SSZ ` prefix from every line.
#[cfg(unix)]
fn untimed(text: &str) -> String {
    text.lines()
        .map(|l| match l.split_once(' ') {
            Some((t, rest)) if t.len() == 9 && t.ends_with('Z') && t.as_bytes()[2] == b':' => rest,
            _ => l,
        })
        .map(|l| format!("{l}\n"))
        .collect()
}

#[cfg(unix)]
const BUNDLE: &str = r#"
name: demo
steps:
  - create:
      ~/.ssh: { mode: "0700" }
  - link:
      ~/.demorc: files/demorc
      ~/.config/demo/conf: files/conf
  - shell: echo "configured $DEMO_VAR"; echo "to stderr" >&2
    label: Configure demo
  - shell:
      cmd: echo "$DEMO_VAR" > ~/marker
      env: { DEMO_VAR: from-env }
      unless: test -e ~/marker
  - shell: echo skipped here
    when: { os: [windows], arch: definitely-not-this }
"#;

#[cfg(unix)]
fn setup_ok() -> Env {
    let env = Env::new();
    env.write_bundle(BUNDLE);
    fs::create_dir_all(env.bundle_dir().join("files")).unwrap();
    fs::write(env.bundle_dir().join("files/demorc"), "rc").unwrap();
    fs::write(env.bundle_dir().join("files/conf"), "conf").unwrap();
    env
}

#[cfg(unix)]
#[test]
fn apply_then_reapply() {
    let env = setup_ok();
    let out = env.apply(&[]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("+ demo: Configure demo"), "{text}");
    assert!(
        text.contains("Summary: 4 changed, 0 ok, 0 failed, 1 skipped"),
        "{text}"
    );
    // Command output is hidden at default verbosity...
    assert!(!text.contains("configured"), "{text}");

    let home = env.home();
    assert_eq!(
        fs::read_link(home.join(".demorc")).unwrap(),
        env.bundle_dir().join("files/demorc")
    );
    assert_eq!(
        fs::read_to_string(home.join(".config/demo/conf")).unwrap(),
        "conf"
    );
    assert!(home.join(".ssh").is_dir());
    assert_eq!(
        fs::read_to_string(home.join("marker")).unwrap().trim(),
        "from-env"
    );

    // ...but the log file has all of it.
    let logs = env.log_files();
    assert_eq!(logs.len(), 1);
    let log = fs::read_to_string(&logs[0]).unwrap();
    assert!(log.contains("stdout| configured"), "{log}");
    assert!(log.contains("stderr| to stderr"), "{log}");
    assert!(log.contains("run finished: 4 changed"), "{log}");

    // Second run: only the plain shell step runs again.
    let out = env.apply(&["-v"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(
        text.contains("Summary: 1 changed, 3 ok, 0 failed, 1 skipped"),
        "{text}"
    );
    assert!(
        text.contains("= demo: link ~/.demorc, ~/.config/demo/conf"),
        "{text}"
    );
    assert!(text.contains("(skipped: when: os is"), "{text}");
    assert_eq!(env.log_files().len(), 2);

    // -vv streams command output live.
    let out = env.apply(&["-vv"]);
    assert!(stdout(&out).contains("  | configured"), "{}", stdout(&out));
}

#[cfg(unix)]
#[test]
fn dry_run_changes_nothing() {
    let env = setup_ok();
    let out = env.apply(&["--dry-run"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Dry run: nothing will be changed."), "{text}");
    assert!(text.contains("~ demo: link ~/.demorc"), "{text}");
    assert!(text.contains("Dry run: 4 would change"), "{text}");
    assert_eq!(fs::read_dir(env.home()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn failure_reports_tail_and_log_and_exits_nonzero() {
    let env = Env::new();
    env.write_bundle(
        "steps:\n  - shell: 'for i in $(seq 1 30); do echo out$i; done; exit 7'\n  - create: [~/never]\n",
    );
    let out = env.apply(&[]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("x demo: shell: for i in"), "{text}");
    assert!(text.contains("error: command exited with code 7"), "{text}");
    assert!(text.contains("last 20 lines of output:"), "{text}");
    assert!(
        text.contains("| out11\n") && text.contains("| out30\n"),
        "{text}"
    );
    assert!(!text.contains("| out10\n"), "{text}");
    let log = &env.log_files()[0];
    assert!(text.contains(&format!("log: {}", log.display())), "{text}");
    let log_text = fs::read_to_string(log).unwrap();
    assert!(
        log_text.contains("stdout| out1\n"),
        "full output in log: {log_text}"
    );
    assert!(!env.home().join("never").exists());
}

#[cfg(unix)]
#[test]
fn quiet_prints_only_summary_on_success() {
    let env = Env::new();
    env.write_bundle("steps:\n  - create: [~/q]\n");
    let out = env.apply(&["-q"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(untimed(&text).starts_with("Summary: 1 changed"), "{text}");
}

#[cfg(unix)]
#[test]
fn apply_lines_are_timestamped_but_plan_is_not() {
    let env = setup_ok();
    let text = stdout(&env.apply(&[]));
    for line in text.lines() {
        let (t, _) = line.split_once(' ').unwrap();
        assert!(
            t.len() == 9 && t.ends_with('Z') && t.as_bytes()[2] == b':',
            "{line}"
        );
    }
    let bundle = env.bundle_dir().join("bundle.yml");
    let text = stdout(&env.dot(&["plan", "-c", bundle.to_str().unwrap()]));
    assert!(text.starts_with("demo ("), "{text}");
}

#[cfg(unix)]
#[test]
fn log_rotation_keeps_unrelated_files() {
    let env = Env::new();
    env.write_bundle("steps: []\n");
    let mine = env.logs().join("2024-server-migration-notes.log");
    fs::write(&mine, "notes").unwrap();
    for _ in 0..11 {
        assert!(env.apply(&["-q"]).status.success());
    }
    assert_eq!(fs::read_to_string(&mine).unwrap(), "notes");
    assert_eq!(
        env.log_files().len(),
        11,
        "10 run logs plus the user's file"
    );
}

#[cfg(unix)]
#[test]
fn steps_depending_on_earlier_steps_converge() {
    let env = Env::new();
    env.write_bundle(
        "steps:\n  - shell: { cmd: 'mkdir -p oh && echo x > oh/theme', cwd: '~/' }\n  - link: { ~/.theme: ~/oh/theme }\n",
    );
    let bundle = env.bundle_dir().join("bundle.yml");
    let plan = stdout(&env.dot(&["plan", "-c", bundle.to_str().unwrap()]));
    assert!(
        plan.contains("? link ~/.theme -> ~/oh/theme (source does not exist yet"),
        "{plan}"
    );
    let out = env.apply(&[]);
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(untimed(&stdout(&out)).contains("Summary: 2 changed, 0 ok, 0 failed"));
    assert_eq!(
        fs::read_to_string(env.home().join(".theme")).unwrap(),
        "x\n"
    );
}

#[cfg(unix)]
#[test]
fn link_over_home_is_refused() {
    let env = Env::new();
    fs::create_dir_all(env.bundle_dir().join("dir")).unwrap();
    fs::write(env.home().join("keep"), "precious").unwrap();
    env.write_bundle("steps:\n  - link: { '~/': { src: dir, on_conflict: overwrite } }\n");
    let bundle = env.bundle_dir().join("bundle.yml");
    let plan = env.dot(&["plan", "-c", bundle.to_str().unwrap()]);
    assert!(!plan.status.success());
    assert!(
        stdout(&plan).contains("home directory"),
        "{}",
        stdout(&plan)
    );
    let out = env.apply(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        fs::read_to_string(env.home().join("keep")).unwrap(),
        "precious"
    );
}

#[cfg(unix)]
#[test]
fn default_log_dir_is_under_state_home() {
    let env = Env::new();
    let bundle = env.write_bundle("steps: []\n");
    let out = env.dot(&["apply", "-c", bundle.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let dir = env.root.join("state/dot/logs");
    assert_eq!(fs::read_dir(dir).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn config_error_exits_2_with_location() {
    let env = Env::new();
    let bundle = env.write_bundle("steps:\n  - shell: x\n    lable: typo\n");
    let out = env.apply(&[]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(err.contains(&format!("{}:3:5:", bundle.display())), "{err}");
    assert!(err.contains("unknown field `lable`"), "{err}");
    assert!(env.log_files().is_empty(), "nothing ran");
}

#[test]
fn check_reports_every_error() {
    let env = Env::new();
    let dir = env.root.join("repo/bundles");
    env.write_bundle("steps:\n  - link: { ~/.x: missing-source }\n");
    fs::create_dir_all(dir.join("two")).unwrap();
    fs::write(dir.join("two/bundle.yml"), "steps:\n  - label: nothing\n").unwrap();
    fs::create_dir_all(dir.join("three")).unwrap();
    fs::write(dir.join("three/bundle.yml"), "steps:\n  - create: [~/ok]\n").unwrap();

    let out = env.dot(&["check", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("`missing-source`"), "{err}");
    let two = std::path::Path::new("two").join("bundle.yml");
    assert!(err.contains(&format!("{}:2:5:", two.display())), "{err}");
    // On Windows the directory given is verbatim (`\\?\C:\...`, from `canonicalize`); what
    // `dot` says about it is in the ordinary spelling.
    assert!(!err.contains(r"\\?\"), "{err}");
    assert!(stdout(&out).contains("checked 3 bundle files: 2 errors"));

    let ok = dir.join("three/bundle.yml");
    let out = env.dot(&["check", ok.to_str().unwrap()]);
    assert!(out.status.success());
}

#[cfg(unix)]
#[test]
fn plan_lists_actions_without_applying() {
    let env = setup_ok();
    let bundle = env.bundle_dir().join("bundle.yml");
    let out = env.dot(&["plan", "-c", bundle.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("~ link ~/.demorc -> files/demorc [pending]"),
        "{text}"
    );
    assert!(!text.contains("(skipped: when: os is"), "{text}");
    let out = env.dot(&["plan", "-v", "-c", bundle.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("(skipped: when: os is"),
        "{}",
        stdout(&out)
    );
    assert_eq!(fs::read_dir(env.home()).unwrap().count(), 0);
}

#[test]
fn schema_is_json() {
    let env = Env::new();
    let out = env.dot(&["schema"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["title"], "dot bundle");
    assert!(v["properties"]["steps"].is_object());
}

#[test]
fn facts_prints_os() {
    let env = Env::new();
    let out = env.dot(&["facts"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("os: "));
}

/// A report command whose reader has gone away (`dot schema | head -1`) ends quietly instead of
/// panicking with "failed printing to stdout: Broken pipe" (exit 101).
#[cfg(unix)]
#[test]
fn report_command_survives_closed_stdout() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    const SIGPIPE: i32 = 13; // same on Linux and macOS

    let env = Env::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_dot"))
        .args(["schema"])
        .current_dir(&env.root)
        .env("HOME", env.home())
        .env_remove("DOT_STATE_DIR")
        .env("XDG_STATE_HOME", env.root.join("state"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let err = stderr(&out);
    assert!(!err.contains("panicked"), "{err}");
    assert_ne!(out.status.code(), Some(101), "{err}");
    assert!(
        out.status.success() || out.status.signal() == Some(SIGPIPE),
        "{:?} {err}",
        out.status
    );
}
