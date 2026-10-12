//! Repo mode end to end: a temp repository with `dot.yml`, bundles and `.env` files, a temp
//! home, config and state directory, and a fixed role. Nothing touches the real home or
//! `~/.config/dot`. Unix-only: the bundles run `sh` steps.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

struct Repo {
    _tmp: TempDir,
    root: PathBuf,
}

const DOT_YML: &str = r#"
vars:
  greeting: hello
  email: { required: true, pattern: ".+@.+" }
profiles:
  server:
    match: { role: server }
    bundles: [app, gui]
    vars: { greeting: "hi from {{ profile }}" }
  laptop:
    match: { role: desktop }
    bundles: [gui]
"#;

impl Repo {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["home", "config", "state", "logs", "repo"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let r = Self { _tmp: tmp, root };
        r.write("repo/dot.yml", DOT_YML);
        r.write("repo/.env", "email=me@example.com\nGH_TOKEN=tok-${email}\n");
        r.write(
            "repo/bundles/common/bundle.yml",
            "implicit: true\nsteps:\n  - shell: echo common >> ~/order\n",
        );
        r.write(
            "repo/bundles/lib/bundle.yml",
            "steps:\n  - shell: echo lib >> ~/order\n",
        );
        r.write(
            "repo/bundles/app/bundle.yml",
            "requires: [lib]\nvars: { port: { type: int, default: 8080 } }\nsteps:\n  - shell: echo \"app $greeting $port {{ email }} [$GH_TOKEN]\" >> ~/order\n",
        );
        r.write(
            "repo/bundles/gui/bundle.yml",
            "when: { role: desktop }\nsteps:\n  - shell: echo gui >> ~/order\n",
        );
        r
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_dot"));
        c.args(args)
            .current_dir(cwd)
            .env("HOME", self.home())
            .env_remove("DOT_STATE_DIR")
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("DOT_ROLE", "server")
            .env("LC_ALL", "C")
            .env_remove("DOT_REPO")
            .env_remove("DOT_PROFILE")
            .env_remove("GH_TOKEN")
            .env_remove("email")
            .env_remove("greeting");
        c
    }

    /// Runs `dot` from inside the repository.
    fn dot(&self, args: &[&str]) -> Output {
        self.command(&self.repo(), args).output().unwrap()
    }

    fn apply(&self, extra: &[&str]) -> Output {
        let logs = self.root.join("logs");
        let mut args = vec!["apply", "--yes", "--log-dir", logs.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.dot(&args)
    }

    fn order(&self) -> Vec<String> {
        fs::read_to_string(self.home().join("order"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn both(o: &Output) -> String {
    format!("{}\n{}", stdout(o), stderr(o))
}

#[test]
fn plan_shows_order_reasons_and_exclusions() {
    let r = Repo::new();
    let out = r.dot(&["plan"]);
    assert!(out.status.success(), "{}", both(&out));
    let text = stdout(&out);
    assert!(
        text.contains("profile: server (matched role=server)"),
        "{text}"
    );
    assert!(text.contains("1. common  implicit"), "{text}");
    assert!(text.contains("2. lib     required by app"), "{text}");
    assert!(
        text.contains("3. app     selected by profile server"),
        "{text}"
    );
    assert!(
        text.contains("gui (selected by profile server): when: role is server, not desktop"),
        "{text}"
    );
    assert!(
        text.contains("run `echo \"app $greeting $port me@example.com [$GH_TOKEN]\""),
        "{text}"
    );
    assert!(!r.home().join("order").exists(), "plan changes nothing");
}

#[test]
fn apply_runs_in_dependency_order_with_vars_exported() {
    let r = Repo::new();
    let out = r.apply(&[]);
    assert!(out.status.success(), "{}", both(&out));
    let order = r.order();
    assert_eq!(order.len(), 3, "{order:?}");
    assert_eq!(&order[..2], ["common", "lib"]);
    // Profile vars override dot.yml, bundle declarations supply defaults, templates see .env;
    // the sensitive GH_TOKEN is not exported to the step.
    assert_eq!(order[2], "app hi from server 8080 me@example.com []");
}

#[test]
fn var_precedence_on_the_command_line() {
    let r = Repo::new();
    r.write("config/dot/local.env", "greeting=from-local\n");
    let out = r.apply(&["app", "--var", "greeting=from-cli"]);
    assert!(out.status.success(), "{}", both(&out));
    assert_eq!(
        r.order().last().unwrap(),
        "app from-cli 8080 me@example.com []"
    );

    fs::remove_file(r.home().join("order")).unwrap();
    let out = r.apply(&["app"]);
    assert!(out.status.success(), "{}", both(&out));
    assert_eq!(
        r.order().last().unwrap(),
        "app from-local 8080 me@example.com []"
    );

    fs::remove_file(r.home().join("order")).unwrap();
    let mut cmd = r.command(&r.repo(), &["apply", "--yes", "app", "--log-dir"]);
    cmd.arg(r.root.join("logs"))
        .env("greeting", "from-environment");
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", both(&out));
    assert_eq!(
        r.order().last().unwrap(),
        "app from-environment 8080 me@example.com []"
    );
}

#[test]
fn no_deps_applies_only_the_named_bundle() {
    let r = Repo::new();
    let out = r.apply(&["app", "--no-deps"]);
    assert!(out.status.success(), "{}", both(&out));
    assert_eq!(r.order(), ["app hi from server 8080 me@example.com []"]);
}

#[test]
fn a_failed_bundle_skips_its_dependents_only() {
    let r = Repo::new();
    r.write("repo/bundles/lib/bundle.yml", "steps:\n  - shell: exit 3\n");
    r.write(
        "repo/bundles/other/bundle.yml",
        "steps:\n  - shell: echo other >> ~/order\n",
    );
    let out = r.apply(&["app", "other"]);
    assert_eq!(out.status.code(), Some(1), "{}", both(&out));
    assert_eq!(r.order(), ["common", "other"]);
    let text = stdout(&out);
    assert!(text.contains("x lib: shell: exit 3"), "{text}");
    assert!(text.contains("app: shell: echo \"app $greeting"), "{text}");
    assert!(text.contains("(skipped: dependency lib failed)"), "{text}");
}

#[test]
fn check_satisfied_bundles_are_skipped_unless_forced() {
    let r = Repo::new();
    r.write(
        "repo/bundles/lib/bundle.yml",
        "check: test -e ~/lib-done\nsteps:\n  - shell: echo lib >> ~/order\n",
    );
    fs::write(r.home().join("lib-done"), "").unwrap();
    let out = r.apply(&["lib"]);
    assert!(out.status.success(), "{}", both(&out));
    assert_eq!(r.order(), ["common"]);
    let out = r.apply(&["lib", "--force"]);
    assert!(out.status.success(), "{}", both(&out));
    assert_eq!(r.order(), ["common", "common", "lib"]);
}

#[test]
fn missing_required_vars_are_listed_up_front() {
    let r = Repo::new();
    r.write("repo/.env", "");
    r.write(
        "repo/bundles/app/bundle.yml",
        "requires: [lib]\nvars: { api: { required: true } }\nsteps:\n  - shell: echo app >> ~/order\n",
    );
    let out = r.apply(&[]);
    assert_eq!(out.status.code(), Some(2), "{}", both(&out));
    assert!(
        stderr(&out).contains("missing required variables: api, email"),
        "{}",
        stderr(&out)
    );
    assert!(r.order().is_empty(), "nothing ran");
}

#[test]
fn profile_from_flag_and_local_pin() {
    let r = Repo::new();
    let out = r.dot(&["plan", "--profile", "laptop"]);
    assert!(out.status.success(), "{}", both(&out));
    assert!(
        stdout(&out).contains("profile: laptop (--profile)"),
        "{}",
        stdout(&out)
    );

    r.write("config/dot/local.env", "DOT_PROFILE=laptop\n");
    let out = r.dot(&["plan"]);
    assert!(
        stdout(&out).contains("profile: laptop (pinned in"),
        "{}",
        stdout(&out)
    );

    r.write(
        "repo/dot.yml",
        "profiles:\n  a: { match: {} }\n  b: { match: {} }\n",
    );
    r.write("config/dot/local.env", "");
    let out = r.dot(&["plan"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("several profiles match"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn repo_is_found_by_flag_env_walk_and_local_config() {
    let r = Repo::new();
    let sub = r.repo().join("bundles/app");
    let out = r.command(&sub, &["plan"]).output().unwrap();
    assert!(
        stdout(&out).contains("(current directory)"),
        "{}",
        both(&out)
    );

    let home = r.home();
    let out = r.command(&home, &["plan"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("no dotfiles repository found"));

    let repo = r.repo();
    let out = r
        .command(&home, &["plan", "--repo", repo.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(stdout(&out).contains("(--repo)"), "{}", both(&out));

    let out = r
        .command(&home, &["plan"])
        .env("DOT_REPO", &repo)
        .output()
        .unwrap();
    assert!(stdout(&out).contains("(DOT_REPO)"), "{}", both(&out));

    r.write(
        "config/dot/local.env",
        &format!("DOT_REPO={}\n", repo.display()),
    );
    let out = r.command(&home, &["plan"]).output().unwrap();
    assert!(
        stdout(&out).contains("(DOT_REPO in local.env)"),
        "{}",
        both(&out)
    );
}

#[test]
fn graph_and_why() {
    let r = Repo::new();
    let out = r.dot(&["graph"]);
    assert!(out.status.success(), "{}", both(&out));
    assert_eq!(stdout(&out), "common\nlib\napp\n  requires lib\n");
    let out = r.dot(&["graph", "--dot"]);
    let text = stdout(&out);
    assert!(text.starts_with("digraph dot {"), "{text}");
    assert!(text.contains("\"app\" -> \"lib\";"), "{text}");

    let out = r.dot(&["why", "lib"]);
    assert_eq!(
        stdout(&out),
        "lib is included (profile server):\n  - required by app\n    - selected by profile server\n"
    );
    let out = r.dot(&["why", "gui"]);
    assert!(
        stdout(&out).contains("gui is not included (profile server): it is selected by profile server, but its when: role is server, not desktop"),
        "{}",
        stdout(&out)
    );
    let out = r.dot(&["why", "nope"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn vars_show_sources_and_redact_sensitive_values() {
    let r = Repo::new();
    let out = r.dot(&["vars", "app"]);
    assert!(out.status.success(), "{}", both(&out));
    let text = stdout(&out);
    let line = |name: &str| {
        text.lines()
            .find(|l| l.starts_with(&format!("{name} ")))
            .unwrap_or_else(|| panic!("no {name} in {text}"))
            .to_string()
    };
    assert!(
        line("greeting").contains("hi from server")
            && line("greeting").ends_with("(profile server)")
    );
    assert!(line("port").contains("8080") && line("port").ends_with("(bundle app)"));
    assert!(line("email").ends_with("(.env)"));
    assert!(line("GH_TOKEN").contains("***"), "{}", line("GH_TOKEN"));
    assert!(!text.contains("tok-me@example.com"), "{text}");
    assert!(line("os").ends_with("(fact)"));
}

#[test]
fn check_validates_the_whole_repo() {
    let r = Repo::new();
    let out = r.dot(&["check"]);
    assert!(out.status.success(), "{}", both(&out));
    assert!(
        stdout(&out).contains("checked dot.yml and 4 bundles: ok"),
        "{}",
        stdout(&out)
    );

    r.write(
        "repo/bundles/app/bundle.yml",
        "requires: [lib, ghost, cap:node]\nsteps: []\n",
    );
    r.write(
        "repo/bundles/lib/bundle.yml",
        "after: [app]\nrequires: [app]\nsteps: []\n",
    );
    r.write(
        "repo/bundles/bad/bundle.yml",
        "steps:\n  - shell: x\n    lable: y\n  - linkk: {}\n",
    );
    let out = r.dot(&["check"]);
    assert_eq!(out.status.code(), Some(1), "{}", both(&out));
    let err = stderr(&out);
    assert!(err.contains("bad/bundle.yml:3:5:"), "{err}");
    assert!(
        err.contains("bad/bundle.yml:4:5: steps[1]: unknown field `linkk`"),
        "{err}"
    );
    assert!(stdout(&out).contains("2 errors"), "{}", stdout(&out));

    r.write("repo/bundles/bad/bundle.yml", "steps: []\n");
    let out = r.dot(&["check"]);
    let err = stderr(&out);
    assert!(
        err.contains("unknown bundle `ghost` (required via app)"),
        "{err}"
    );
    assert!(
        err.contains("capability `node` (required via app): no bundle provides it"),
        "{err}"
    );
    assert!(err.contains("dependency cycle: app -> lib -> app"), "{err}");
}

#[test]
fn graph_errors_exit_2_and_name_the_cycle() {
    let r = Repo::new();
    r.write(
        "repo/bundles/lib/bundle.yml",
        "requires: [app]\nsteps: []\n",
    );
    let out = r.dot(&["plan"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("dependency cycle: app -> lib -> app"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn facts_and_repo_schema() {
    let r = Repo::new();
    let out = r.dot(&["facts"]);
    let text = stdout(&out);
    for f in [
        "os", "arch", "distro", "family", "version", "role", "env", "pkg",
    ] {
        assert!(
            text.contains(&format!("\n{f}: ")) || text.starts_with(&format!("{f}: ")),
            "{f} in {text}"
        );
    }
    assert!(text.contains("role: server"), "DOT_ROLE override: {text}");
    let out = r.dot(&["schema", "repo"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["title"], "dot repository (dot.yml)");
    assert!(v["properties"]["profiles"].is_object());
    let out = r.dot(&["schema", "secrets"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["title"], "dot secrets");
    assert!(v["properties"]["secrets"].is_object());
}

#[test]
fn a_named_bundle_that_does_not_match_is_an_error() {
    let r = Repo::new();
    let out = r.apply(&["--plain", "gui"]);
    assert_eq!(out.status.code(), Some(2), "{}", both(&out));
    assert!(
        stderr(&out).contains(
            "bundle `gui` does not match this machine: when: role is server, not desktop"
        ),
        "{}",
        both(&out)
    );
    assert!(r.order().is_empty());
}

#[test]
fn plan_json_reports_variable_errors_on_stderr() {
    let r = Repo::new();
    r.write("repo/.env", "");
    let out = r.dot(&["plan", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", both(&out));
    let _: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        stderr(&out).contains("error: missing required variables: email"),
        "{}",
        both(&out)
    );
}

#[test]
fn check_validates_env_files() {
    let r = Repo::new();
    r.write("repo/.env", "email=me@example.com\nBAD LINE\n");
    let out = r.dot(&["check"]);
    assert_eq!(out.status.code(), Some(1), "{}", both(&out));
    assert!(stderr(&out).contains(".env:2:"), "{}", both(&out));
    r.write("repo/.env", "email=nope\n");
    let out = r.dot(&["check"]);
    assert_eq!(out.status.code(), Some(1), "{}", both(&out));
    assert!(
        stderr(&out).contains("`nope` does not match pattern"),
        "{}",
        both(&out)
    );
}

#[test]
fn repo_vars_may_use_bundle_vars_and_derived_secrets_stay_hidden() {
    let r = Repo::new();
    r.write(
        "repo/dot.yml",
        "vars:\n  url: \"http://localhost:{{ port }}\"\n  clone_url: \"https://oauth:{{ GH_TOKEN }}@gitlab.com\"\nbundles: [b]\n",
    );
    r.write(
        "repo/bundles/b/bundle.yml",
        "vars: { port: 8080 }\nsteps:\n  - shell: echo \"{{ url }} [$clone_url]\" >> ~/order\n",
    );
    let out = r.apply(&["--plain"]);
    assert!(out.status.success(), "{}", both(&out));
    assert_eq!(r.order(), ["http://localhost:8080 []", "common"]);
    let out = r.dot(&["vars", "b"]);
    let text = stdout(&out);
    assert!(text.contains("url "), "{text}");
    assert!(!text.contains("tok-me@example.com"), "{text}");
}

#[test]
fn jobs_run_bundles_together_but_report_them_in_plan_order_with_hints() {
    let r = Repo::new();
    // `a-wait` only succeeds once `b-fail` (later in plan order, no edge between them) has
    // run: one bundle at a time, it would time out.
    r.write(
        "repo/bundles/a-wait/bundle.yml",
        "steps:\n  - shell: |\n      i=0\n      while [ ! -e ~/b-done ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done\n      test -e ~/b-done && echo a-waited\n",
    );
    r.write(
        "repo/bundles/b-fail/bundle.yml",
        "steps:\n  - shell: |\n      touch ~/b-done\n      echo 'curl: (6) Could not resolve host: example.invalid' >&2\n      exit 6\n",
    );
    let out = r.apply(&["a-wait", "b-fail", "--jobs", "3", "--plain", "-v"]);
    assert_eq!(out.status.code(), Some(1), "{}", both(&out));
    let text = stdout(&out);
    let a = text.find("+ a-wait: shell:").expect(&text);
    let b = text.find("b-fail: shell:").expect(&text);
    assert!(a < b, "plan order, although b-fail finished first:\n{text}");
    assert!(
        text.contains("    error: command exited with code 6"),
        "{text}"
    );
    assert!(
        text.contains("    hint: a host name could not be resolved"),
        "{text}"
    );
    let logs: Vec<PathBuf> = fs::read_dir(r.root.join("logs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let log = fs::read_to_string(&logs[0]).unwrap();
    let a = log.find("step a-wait[0]").expect(&log);
    let b = log.find("step b-fail[0]").expect(&log);
    assert!(a < b, "{log}");
    assert!(log.contains("  hint (dns): a host name"), "{log}");

    // --json carries the hint on the failed step.
    let out = r.apply(&["b-fail", "--no-deps", "--json"]);
    let finished: Vec<serde_json::Value> = stdout(&out)
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|v| v["event"] == "step_finished")
        .collect();
    assert_eq!(finished.len(), 1, "{}", both(&out));
    assert!(
        finished[0]["hint"]
            .as_str()
            .is_some_and(|h| h.contains("could not be resolved")),
        "{}",
        finished[0]
    );
}
