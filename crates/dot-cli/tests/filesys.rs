//! The file and system directives through the built binary: `template`, `copy`, `block`,
//! `path` / `env` (and the record `dot env --shell` reads), and `dot check` / `dot schema`.
//! HOME and XDG_STATE_HOME point into a temp dir; nothing touches the real home directory.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use tempfile::TempDir;

struct Env {
    _tmp: TempDir,
    root: PathBuf,
}

impl Env {
    fn new(yaml: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["home", "logs", "repo/bundles/demo", "state"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("repo/dot.yml"), "").unwrap();
        fs::write(root.join("repo/bundles/demo/bundle.yml"), yaml).unwrap();
        Self { _tmp: tmp, root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn write_repo(&self, rel: &str, text: &str) {
        let p = self.root.join("repo/bundles/demo").join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn dot(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dot"))
            .args(args)
            .current_dir(&self.root)
            .env("HOME", self.home())
            .env_remove("DOT_STATE_DIR")
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("PATH", "/usr/bin:/bin")
            .env_remove("DOT_REPO")
            .env("LC_ALL", "C")
            .output()
            .unwrap()
    }

    fn apply(&self, extra: &[&str]) -> Output {
        let bundle = self.root.join("repo/bundles/demo/bundle.yml");
        let logs = self.root.join("logs");
        let mut args = vec![
            "apply",
            "-c",
            bundle.to_str().unwrap(),
            "--log-dir",
            logs.to_str().unwrap(),
            "--plain",
            "-y",
        ];
        args.extend_from_slice(extra);
        self.dot(&args)
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

const BUNDLE: &str = r#"name: demo
steps:
  - template:
      ~/.config/app/conf: conf.j2
  - copy:
      ~/.config/app/plain: plain
  - block:
      file: ~/.bashrc
      marker: tools
      content: export TOOLS={{ home }}/tools
  - path: [~/tools/bin]
  - env: { DOT_DEMO: "on {{ os }}" }
  - shell: echo "$DOT_DEMO $PATH" > {{ home }}/seen
"#;

#[test]
fn applies_converges_and_records_the_environment() {
    let env = Env::new(BUNDLE);
    env.write_repo("conf.j2", "user={{ home }}\n");
    env.write_repo("plain", "bytes\n");
    let home = env.home().display().to_string();

    let dry = env.apply(&["--dry-run"]);
    assert!(dry.status.success(), "{}", text(&dry));
    assert!(!env.home().join(".config").exists());
    assert!(!env.root.join("state/dot/state.json").exists());

    let out = env.apply(&[]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        fs::read_to_string(env.home().join(".config/app/conf")).unwrap(),
        format!("user={home}\n")
    );
    assert_eq!(
        fs::read_to_string(env.home().join(".config/app/plain")).unwrap(),
        "bytes\n"
    );
    assert_eq!(
        fs::read_to_string(env.home().join(".bashrc")).unwrap(),
        format!("# >>> dot:demo:tools >>>\nexport TOOLS={home}/tools\n# <<< dot:demo:tools <<<\n")
    );
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    assert_eq!(
        fs::read_to_string(env.home().join("seen")).unwrap(),
        format!("on {os} {home}/tools/bin:/usr/bin:/bin\n")
    );
    let record = fs::read_to_string(env.root.join("state/dot/state.json")).unwrap();
    assert!(record.contains(&format!("{home}/tools/bin")), "{record}");
    assert!(record.contains("DOT_DEMO"), "{record}");

    // Second run: the files are satisfied, and so are path/env (the record has them, though
    // the calling shell does not); only the shell step runs again.
    let out = env.apply(&["-v"]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("template ~/.config/app/conf <- conf.j2"), "{t}");
    assert!(t.contains("Summary: 1 changed, 5 ok, 0 failed"), "{t}");
}

#[test]
fn dot_state_dir_moves_every_state_file() {
    let env = Env::new("name: demo\nsteps:\n  - path: [~/tools/bin]\n");
    let state = env.root.join("elsewhere");
    let bundle = env.root.join("repo/bundles/demo/bundle.yml");
    let out = Command::new(env!("CARGO_BIN_EXE_dot"))
        .args(["apply", "-c", bundle.to_str().unwrap(), "--plain", "-y"])
        .current_dir(&env.root)
        .env("HOME", env.home())
        .env("DOT_STATE_DIR", &state)
        .env("XDG_STATE_HOME", env.root.join("state"))
        .env("PATH", "/usr/bin:/bin")
        .env_remove("DOT_REPO")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(state.join("state.json").is_file());
    assert_eq!(fs::read_dir(state.join("logs")).unwrap().count(), 1);
    assert!(!env.root.join("state/dot").exists());
}

#[test]
fn a_run_that_stops_before_the_path_steps_keeps_the_record() {
    let env = Env::new(
        "name: demo\nsteps:\n  - shell: test ! -e {{ home }}/fail\n  - path: [~/tools/bin]\n  - env: { DOT_DEMO: on }\n",
    );
    let out = env.apply(&[]);
    assert!(out.status.success(), "{}", text(&out));
    let file = env.root.join("state/dot/state.json");
    let env_record = |f: &std::path::Path| -> String {
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(f).unwrap()).unwrap();
        v["env"].to_string()
    };
    let before = env_record(&file);
    assert!(
        before.contains("tools/bin") && before.contains("DOT_DEMO"),
        "{before}"
    );

    // The first step fails, so the path/env steps never run: their record stays.
    fs::write(env.home().join("fail"), "").unwrap();
    let out = env.apply(&[]);
    assert!(!out.status.success(), "{}", text(&out));
    assert_eq!(env_record(&file), before);
}

#[test]
fn a_conflict_is_a_would_fail_in_plan_and_dry_run() {
    let env = Env::new("name: demo\nsteps:\n  - link: { ~/.rc: { src: rc, on_conflict: fail } }\n");
    env.write_repo("rc", "repo\n");
    fs::write(env.home().join(".rc"), "mine\n").unwrap();
    let bundle = env.root.join("repo/bundles/demo/bundle.yml");
    let out = env.dot(&["plan", "-c", bundle.to_str().unwrap()]);
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(
        t.contains("conflict: existing file, on_conflict is fail) [conflict]"),
        "{t}"
    );

    let out = env.apply(&["--dry-run"]);
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("would fail: link ~/.rc"), "{t}");
    assert_eq!(
        fs::read_to_string(env.home().join(".rc")).unwrap(),
        "mine\n"
    );
}

#[test]
fn check_and_schema_know_the_new_directives() {
    let env = Env::new(
        "steps:\n  - template:\n      ~/x: missing.j2\n  - copy:\n      ~/y: { src: y, mode: 644 }\n",
    );
    let out = env.dot(&["check", env.root.join("repo").to_str().unwrap()]);
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("bundle.yml:5:"), "{t}");
    assert!(t.contains("file mode must be a quoted octal string"), "{t}");

    let env = Env::new("steps:\n  - template:\n      ~/x: missing.j2\n");
    let out = env.dot(&["check", env.root.join("repo").to_str().unwrap()]);
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(
        t.contains("template source `missing.j2` for `~/x` does not exist"),
        "{t}"
    );

    let out = env.dot(&["schema"]);
    let schema = String::from_utf8_lossy(&out.stdout);
    for key in [
        "\"template\"",
        "\"copy\"",
        "\"secret_file\"",
        "\"block\"",
        "\"path\"",
        "\"env\"",
        "\"macos_defaults\"",
        "\"service\"",
    ] {
        assert!(schema.contains(key), "schema lacks {key}");
    }
}

#[test]
fn a_directory_copy_prunes_what_its_source_lost() {
    let env = Env::new("name: demo\nsteps:\n  - copy:\n      ~/.app: app\n");
    env.write_repo("app/keep", "k\n");
    env.write_repo("app/gone", "g\n");
    env.write_repo("app/edited", "e\n");
    let dest = env.home().join(".app");
    let repo = env.root.join("repo");
    let bundle = env.root.join("repo/bundles/demo/bundle.yml");
    let out = env.apply(&[]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(fs::read_to_string(dest.join("gone")).unwrap(), "g\n");
    let status = |args: &[&str]| {
        let mut a = vec!["status", "demo", "--repo", repo.to_str().unwrap()];
        a.extend_from_slice(args);
        env.dot(&a)
    };
    let st = status(&[]);
    assert!(st.status.success(), "{}", text(&st));

    // The source loses two files; the user edits one of the copies first.
    for f in ["app/gone", "app/edited"] {
        fs::remove_file(env.root.join("repo/bundles/demo").join(f)).unwrap();
    }
    fs::write(dest.join("edited"), "mine\n").unwrap();
    let plan = env.dot(&["plan", "-c", bundle.to_str().unwrap()]);
    let t = text(&plan);
    assert!(plan.status.success(), "{t}");
    assert!(
        t.contains(
            "copy ~/.app <- app (1 stale file to remove; 1 stale file kept: changed since copied)"
        ),
        "{t}"
    );
    let st = status(&[]);
    let t = text(&st);
    assert!(!st.status.success(), "{t}");
    assert!(t.contains("1 stale file to remove"), "{t}");

    let out = env.apply(&[]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("kept ~/.app/edited"), "{t}");
    assert!(!dest.join("gone").exists());
    assert_eq!(fs::read_to_string(dest.join("edited")).unwrap(), "mine\n");
    assert_eq!(fs::read_to_string(dest.join("keep")).unwrap(), "k\n");

    // Converged: in sync, and a third run changes nothing.
    let st = status(&[]);
    assert!(st.status.success(), "{}", text(&st));
    let out = env.apply(&[]);
    let t = text(&out);
    assert!(t.contains("Summary: 0 changed, 1 ok, 0 failed"), "{t}");

    // `dot remove` takes out only what dot still owns: the edited file stays.
    let out = env.dot(&["remove", "demo", "--yes", "--repo", repo.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!dest.join("keep").exists());
    assert_eq!(fs::read_to_string(dest.join("edited")).unwrap(), "mine\n");
}

#[test]
fn path_persist_user_is_a_no_op_off_windows() {
    let env = Env::new(
        "name: demo\nsteps:\n  - path:\n      ~/tools/bin: { persist: user }\n      ~/late: { position: append, persist: user }\n",
    );
    let bundle = env.root.join("repo/bundles/demo/bundle.yml");
    let plan = env.dot(&["plan", "-c", bundle.to_str().unwrap()]);
    let t = text(&plan);
    assert!(plan.status.success(), "{t}");
    assert!(
        t.contains(
            "PATH: prepend ~/tools/bin (does not exist yet; persist: user is Windows-only: nothing to persist here)"
        ),
        "{t}"
    );
    let out = env.apply(&[]);
    assert!(out.status.success(), "{}", text(&out));
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env.root.join("state/dot/state.json")).unwrap())
            .unwrap();
    let keys: Vec<&String> = state["resources"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["path:~/late", "path:~/tools/bin"], "{state}");
    let out = env.apply(&[]);
    let t = text(&out);
    assert!(t.contains("Summary: 0 changed, 1 ok, 0 failed"), "{t}");

    let out = env.dot(&["schema"]);
    let schema = String::from_utf8_lossy(&out.stdout);
    assert!(schema.contains("\"persist\""), "schema lacks persist");
    assert!(schema.contains("PathPersist"), "schema lacks PathPersist");

    let bad = Env::new("name: demo\nsteps:\n  - path: { ~/x: { persist: machine } }\n");
    let out = bad.dot(&["check", bad.root.join("repo").to_str().unwrap()]);
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("unknown variant `machine`"), "{t}");
}
