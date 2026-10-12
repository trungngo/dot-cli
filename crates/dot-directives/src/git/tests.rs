//! `git` against local bare repositories over `file://`. Git's global and system config are
//! switched off for every command, so the user's settings never leak in.

use std::path::Path;
use std::process::Command;

use dot_core::{EventSink, Options, SecretRefs, engine};

use super::*;
use crate::Builtins;
use crate::testutil::Fixture;

/// The isolation every git command here gets.
fn isolated(home: &Path) -> Vec<(OsString, OsString)> {
    [
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_AUTHOR_NAME", "dot test"),
        ("GIT_AUTHOR_EMAIL", "dot@example.invalid"),
        ("GIT_COMMITTER_NAME", "dot test"),
        ("GIT_COMMITTER_EMAIL", "dot@example.invalid"),
    ]
    .iter()
    .map(|(k, v)| (OsString::from(k), OsString::from(v)))
    .chain([(OsString::from("HOME"), home.as_os_str().to_owned())])
    .collect()
}

/// A bare "remote" with one commit on `main`, and a scratch clone to push more from.
struct Remote {
    _tmp: tempfile::TempDir,
    bare: PathBuf,
    work: PathBuf,
    env: Vec<(OsString, OsString)>,
}

impl Remote {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let env = isolated(tmp.path());
        let bare = tmp.path().join("origin.git");
        let work = tmp.path().join("work");
        let r = Remote {
            bare,
            work,
            env,
            _tmp: tmp,
        };
        r.git(
            None,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                r.bare.to_str().unwrap(),
            ],
        );
        r.git(
            None,
            &["init", "-q", "-b", "main", r.work.to_str().unwrap()],
        );
        r.git(Some(&r.work), &["remote", "add", "origin", &r.url()]);
        r.commit("README", "one");
        r
    }

    fn url(&self) -> String {
        format!("file://{}", self.bare.display())
    }

    fn git(&self, dir: Option<&Path>, args: &[&str]) -> String {
        run_git(&self.env, dir, args)
    }

    /// Commits `file` = `content` on the current branch and pushes it.
    fn commit(&self, file: &str, content: &str) -> String {
        std::fs::write(self.work.join(file), content).unwrap();
        self.git(Some(&self.work), &["add", "."]);
        self.git(Some(&self.work), &["commit", "-q", "-m", content]);
        let branch = self.git(Some(&self.work), &["symbolic-ref", "--short", "HEAD"]);
        self.git(Some(&self.work), &["push", "-q", "origin", &branch]);
        self.git(Some(&self.work), &["rev-parse", "HEAD"])
    }
}

fn run_git(env: &[(OsString, OsString)], dir: Option<&Path>, args: &[&str]) -> String {
    let mut c = Command::new("git");
    c.envs(env.iter().map(|(k, v)| (k, v)));
    if let Some(d) = dir {
        c.arg("-C").arg(d);
    }
    let out = c.args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn builtins(remote: &Remote) -> Builtins {
    Builtins {
        git: Git {
            program: PathBuf::from("git"),
            env: remote.env.clone(),
        },
        ..Builtins::default()
    }
}

fn step(url: &str, extra: &str) -> String {
    format!("steps:\n  - git: {{ repo: '{url}', dest: ~/src/x{extra} }}\n")
}

fn plan(f: &Fixture, b: &Builtins) -> Vec<Box<dyn Action>> {
    let step = &f.bundle.steps[0];
    let secrets = SecretRefs::default();
    let ctx = f.context();
    let pctx = PlanCtx::new(&ctx, &f.bundle, step, &secrets);
    dot_core::Planner::plan(b, &step.directive, &pctx).unwrap_or_else(|e| panic!("{e:#}"))
}

fn head(r: &Remote, dir: &Path) -> String {
    r.git(Some(dir), &["rev-parse", "HEAD"])
}

#[test]
fn clones_then_fast_forwards() {
    let r = Remote::new();
    let f = Fixture::new(&step(&r.url(), ""));
    let b = builtins(&r);
    let dest = f.home().join("src/x");

    let actions = plan(&f, &b);
    assert_eq!(
        actions[0].describe(),
        format!("clone {} -> ~/src/x", r.url())
    );
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert_eq!(f.apply(&actions), [Outcome::Changed]);
    assert_eq!(std::fs::read_to_string(dest.join("README")).unwrap(), "one");
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    let actions = plan(&f, &b);
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    assert!(
        actions[0].describe().ends_with("(up to date)"),
        "{}",
        actions[0].describe()
    );

    // The remote moves on: the plan names both commits and applying fast-forwards.
    let old = head(&r, &dest);
    let new = r.commit("README", "two");
    let actions = plan(&f, &b);
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert_eq!(
        actions[0].describe(),
        format!(
            "update ~/src/x from {} ({} -> {})",
            r.url(),
            &old[..7],
            &new[..7]
        )
    );
    assert_eq!(f.apply(&actions), [Outcome::Changed]);
    assert_eq!(head(&r, &dest), new);
    assert_eq!(std::fs::read_to_string(dest.join("README")).unwrap(), "two");
    assert_eq!(plan(&f, &b)[0].status().unwrap(), Status::Satisfied);
}

#[test]
fn a_shallow_clone_takes_the_last_commits_and_still_fast_forwards() {
    let r = Remote::new();
    r.commit("README", "two");
    r.commit("README", "three");
    let f = Fixture::new(&step(&r.url(), ", depth: 1"));
    let b = builtins(&r);
    let dest = f.home().join("src/x");
    assert_eq!(f.apply(&plan(&f, &b)), [Outcome::Changed]);
    let count = |d: &Path| r.git(Some(d), &["rev-list", "--count", "HEAD"]);
    assert_eq!(count(&dest), "1", "only the last commit");
    assert_eq!(
        r.git(Some(&dest), &["rev-parse", "--is-shallow-repository"]),
        "true"
    );
    let new = r.commit("README", "four");
    assert_eq!(f.apply(&plan(&f, &b)), [Outcome::Changed]);
    assert_eq!(head(&r, &dest), new);
    assert_eq!(
        std::fs::read_to_string(dest.join("README")).unwrap(),
        "four"
    );
    assert_eq!(plan(&f, &b)[0].status().unwrap(), Status::Satisfied);
}

#[test]
fn local_changes_are_reported_and_left_alone() {
    let r = Remote::new();
    let f = Fixture::new(&step(&r.url(), ""));
    let b = builtins(&r);
    let dest = f.home().join("src/x");
    f.apply(&plan(&f, &b));
    std::fs::write(dest.join("README"), "edited").unwrap();

    // Nothing to pull: a dirty checkout is still satisfied.
    assert_eq!(plan(&f, &b)[0].status().unwrap(), Status::Satisfied);

    let before = head(&r, &dest);
    r.commit("README", "two");
    let actions = plan(&f, &b);
    assert_eq!(actions[0].status().unwrap(), Status::Conflict);
    let d = actions[0].describe();
    assert!(d.contains("conflict: ~/src/x has local changes"), "{d}");
    let e = f.apply_err(actions[0].as_ref());
    assert!(e.contains("has local changes"), "{e}");
    assert!(e.contains("left untouched"), "{e}");
    assert_eq!(head(&r, &dest), before);
    assert_eq!(
        std::fs::read_to_string(dest.join("README")).unwrap(),
        "edited"
    );
}

#[test]
fn local_commits_ahead_are_satisfied_and_divergence_fails() {
    let r = Remote::new();
    let f = Fixture::new(&step(&r.url(), ""));
    let b = builtins(&r);
    let dest = f.home().join("src/x");
    f.apply(&plan(&f, &b));
    std::fs::write(dest.join("local"), "mine").unwrap();
    r.git(Some(&dest), &["add", "."]);
    r.git(Some(&dest), &["commit", "-q", "-m", "local"]);
    let actions = plan(&f, &b);
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    assert!(actions[0].describe().contains("ahead of the remote"));

    // Now the remote moves too: a fast-forward is impossible, and nothing changes.
    r.commit("other", "theirs");
    let before = head(&r, &dest);
    let actions = plan(&f, &b);
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    let e = f.apply_err(actions[0].as_ref());
    assert!(e.contains("fast-forward"), "{e}");
    assert_eq!(head(&r, &dest), before);
    assert!(!dest.join("other").exists());
}

#[test]
fn a_different_remote_or_a_plain_directory_is_a_conflict() {
    let r = Remote::new();
    let other = Remote::new();
    let f = Fixture::new(&step(&r.url(), ""));
    let dest = f.home().join("src/x");
    run_git(
        &other.env,
        None,
        &["clone", "-q", &other.url(), dest.to_str().unwrap()],
    );
    let actions = plan(&f, &builtins(&r));
    assert_eq!(actions[0].status().unwrap(), Status::Conflict);
    let d = actions[0].describe();
    assert!(
        d.contains(&format!("is a checkout of {}", other.url())),
        "{d}"
    );
    let e = f.apply_err(actions[0].as_ref());
    assert!(e.contains("left untouched"), "{e}");
    assert_eq!(head(&r, &dest), head(&other, &other.work));

    std::fs::remove_dir_all(&dest).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("keep"), "precious").unwrap();
    let actions = plan(&f, &builtins(&r));
    assert_eq!(actions[0].status().unwrap(), Status::Conflict);
    assert!(
        actions[0]
            .describe()
            .contains("exists and is not a git checkout")
    );
    f.apply_err(actions[0].as_ref());
    assert_eq!(
        std::fs::read_to_string(dest.join("keep")).unwrap(),
        "precious"
    );

    // An empty directory is simply cloned into.
    std::fs::remove_file(dest.join("keep")).unwrap();
    let actions = plan(&f, &builtins(&r));
    assert_eq!(f.apply(&actions), [Outcome::Changed]);
    assert!(dest.join("README").exists());
}

#[test]
fn branches() {
    let r = Remote::new();
    r.git(Some(&r.work), &["checkout", "-q", "-b", "dev"]);
    let dev = r.commit("DEV", "dev");
    let f = Fixture::new(&step(&r.url(), ", branch: dev"));
    let b = builtins(&r);
    let dest = f.home().join("src/x");
    let actions = plan(&f, &b);
    assert!(
        actions[0].describe().ends_with("(branch dev)"),
        "{}",
        actions[0].describe()
    );
    f.apply(&actions);
    assert_eq!(head(&r, &dest), dev);
    assert_eq!(plan(&f, &b)[0].status().unwrap(), Status::Satisfied);

    // A checkout switched to another branch is not switched back.
    r.git(Some(&dest), &["checkout", "-q", "-b", "mine"]);
    let d = plan(&f, &b)[0].describe();
    assert!(d.contains("is on branch mine, not dev"), "{d}");

    // A branch the remote lacks.
    let f = Fixture::new(&step(&r.url(), ", branch: nope"));
    let dest = f.home().join("src/x");
    run_git(
        &r.env,
        None,
        &["clone", "-q", &r.url(), dest.to_str().unwrap()],
    );
    r.git(Some(&dest), &["checkout", "-q", "-b", "nope"]);
    let actions = plan(&f, &b);
    assert_eq!(actions[0].status().unwrap(), Status::Unknown);
    assert!(actions[0].describe().contains("branch nope does not exist"));
}

#[test]
fn update_false_leaves_an_existing_checkout() {
    let r = Remote::new();
    let f = Fixture::new(&step(&r.url(), ", update: false"));
    let b = builtins(&r);
    f.apply(&plan(&f, &b));
    r.commit("README", "two");
    let actions = plan(&f, &b);
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    assert!(actions[0].describe().contains("(update: false)"));
}

#[test]
fn unreachable_remote_is_unknown() {
    let r = Remote::new();
    let f = Fixture::new(&step(&r.url(), ""));
    let b = builtins(&r);
    f.apply(&plan(&f, &b));
    std::fs::remove_dir_all(&r.bare).unwrap();
    let actions = plan(&f, &b);
    assert_eq!(actions[0].status().unwrap(), Status::Unknown);
    assert!(
        actions[0].describe().contains("cannot reach"),
        "{}",
        actions[0].describe()
    );
    let e = f.apply_err(actions[0].as_ref());
    assert!(e.contains("cannot reach"), "{e}");
}

#[test]
fn missing_git_fails_when_applied() {
    let r = Remote::new();
    let f = Fixture::new(&step(&r.url(), ""));
    let mut b = builtins(&r);
    b.git.program = PathBuf::from("/nonexistent/git");
    let actions = plan(&f, &b);
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    let e = f.apply_err(actions[0].as_ref());
    assert!(e.contains("is git installed?"), "{e}");

    // With a checkout present, the answer is unknown until git exists.
    f.apply(&plan(&f, &builtins(&r)));
    let actions = plan(&f, &b);
    assert_eq!(actions[0].status().unwrap(), Status::Unknown);
    assert!(actions[0].describe().contains("git is not installed yet"));
    assert!(actions[0].describe().contains("`package: [git]`"));
}

#[test]
fn clone_failure_reports_gits_message() {
    let r = Remote::new();
    let url = format!("file://{}", r.bare.with_file_name("missing.git").display());
    let f = Fixture::new(&step(&url, ""));
    let actions = plan(&f, &builtins(&r));
    let e = f.apply_err(actions[0].as_ref());
    assert!(e.contains("git clone failed"), "{e}");
    assert!(!f.home().join("src/x").exists());
}

#[test]
fn dry_run_clones_nothing() {
    let r = Remote::new();
    let f = Fixture::new(&step(&r.url(), ""));
    let summary = engine::run(
        std::slice::from_ref(&f.bundle),
        &builtins(&r),
        &f.context(),
        &EventSink::new(),
        Options {
            dry_run: true,
            ..Default::default()
        },
    );
    assert_eq!((summary.changed, summary.failed), (1, 0), "{summary:?}");
    assert!(!f.home().join("src").exists());
    let summary = engine::run(
        std::slice::from_ref(&f.bundle),
        &builtins(&r),
        &f.context(),
        &EventSink::new(),
        Options::default(),
    );
    assert_eq!((summary.changed, summary.failed), (1, 0), "{summary:?}");
    assert!(f.home().join("src/x/README").exists());
}

#[test]
fn urls_compare_loosely() {
    assert!(same_remote(
        "https://github.com/a/b.git",
        "https://github.com/a/b"
    ));
    assert!(same_remote(
        "https://github.com/a/b/",
        "https://github.com/a/b.git"
    ));
    assert!(!same_remote(
        "https://github.com/a/b",
        "https://github.com/a/c"
    ));
}

#[test]
fn secrets_and_credentials_never_reach_the_url() {
    let r = Remote::new();
    let b = builtins(&r);
    for (repo, want) in [
        (
            "https://oauth2:{{ secret('gl_token') }}@127.0.0.1:9/me/priv.git",
            "secret() cannot be used here",
        ),
        (
            "https://{{ secret('gl_token') }}/x",
            "secret() cannot be used here",
        ),
        (
            "https://{{ user }}:{{ pw }}@127.0.0.1:9/x",
            "has credentials in it",
        ),
    ] {
        let f = Fixture::new(&format!(
            "vars: {{ user: me, pw: hunter2 }}\nsteps:\n  - git: {{ repo: \"{repo}\", dest: ~/src/x }}\n"
        ));
        let mut ctx = f.context();
        ctx.vars = ctx.vars.with_bundle(&f.bundle);
        let step = &f.bundle.steps[0];
        let secrets = SecretRefs::default();
        let pctx = PlanCtx::new(&ctx, &f.bundle, step, &secrets);
        let e = match dot_core::Planner::plan(&b, &step.directive, &pctx) {
            Ok(_) => panic!("{repo}: planned"),
            Err(e) => format!("{e:#}"),
        };
        assert!(e.contains(want), "{repo}: {e}");
        assert!(!e.contains("hunter2"), "{e}");
    }
    let secret_branch = Fixture::new(
        "steps:\n  - git: { repo: 'https://x/y', dest: ~/src/x, branch: \"{{ secret('b') }}\" }\n",
    );
    let step = &secret_branch.bundle.steps[0];
    let secrets = SecretRefs::default();
    let ctx = secret_branch.context();
    let pctx = PlanCtx::new(&ctx, &secret_branch.bundle, step, &secrets);
    let e = dot_core::Planner::plan(&b, &step.directive, &pctx)
        .err()
        .expect("refused");
    assert!(format!("{e:#}").contains("git branch: secret()"), "{e:#}");
}
