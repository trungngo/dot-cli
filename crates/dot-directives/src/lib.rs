//! Built-in directives: `link`, `create`, `shell`, `clean`, `package`, `package_repo`,
//! `github_release`, `download`, `git`, `template`, `copy`, `secret_file`, `block`, `path` /
//! `env`, `macos_defaults`, `service`, `user`, `absent`.
//!
//! Each directive has a pure `plan` that turns its config into [`dot_core::Action`]s; the engine
//! reaches them through [`Builtins`], which the CLI passes in as the [`Planner`].

pub mod absent;
pub mod block;
pub mod clean;
pub mod copy;
pub mod create;
pub mod download;
pub mod fileutil;
pub mod git;
pub mod github_release;
pub mod link;
pub mod linkstate;
pub mod macos_defaults;
pub mod package;
pub mod package_repo;
pub mod pkgfile;
pub mod rootfile;
pub mod runenv;
pub mod secret_file;
pub mod service;
pub mod shell;
mod syscmd;
pub mod template;
pub mod undo;
pub mod user;

#[cfg(all(test, unix))]
mod diff_tests;
#[cfg(all(test, unix))]
mod testutil;
#[cfg(all(test, unix))]
mod testutil_sys;

use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use dot_core::{Action, Bundle, Directive, PlanCtx, Planner};
use dot_platform::exec::System;
use dot_platform::link::{LinkStrategy, SystemLinks};
use dot_platform::pkg::Packages;
use dot_platform::shell::Interpreter;

/// The planner for every built-in directive.
#[derive(Debug, Clone)]
pub struct Builtins {
    /// Runs `shell` steps, `unless` checks and bundle `check:`s that do not name an interpreter
    /// (the platform's default).
    pub interpreter: Interpreter,
    /// How `link` makes links (symlinks, or the Windows fallbacks), shared with `clean` and
    /// `copy`.
    pub links: Arc<dyn LinkStrategy>,
    /// The package managers `package` steps use; one per run, so what it caches (cask
    /// metadata) lasts the run.
    pub packages: Packages,
    /// GitHub API access for `github_release` steps.
    pub github: github_release::Github,
    /// How `git` steps run git.
    pub git: git::Git,
    /// Runs the system tools of `macos_defaults`, `service` and `user` (and says who we are).
    pub system: System,
    /// `dot apply --upgrade`: every `package` step also upgrades what is outdated, and `shell`
    /// steps whose `unless` succeeds run their `upgrade:` command.
    pub upgrade: bool,
    /// The list of valid login shells `user` steps check and extend.
    pub etc_shells: std::path::PathBuf,
    /// How `package_repo` steps download keys and probe suites.
    pub repos: package_repo::RepoNet,
    /// The owner a root-owned file (`root: true`) must have: 0 (tests use their own uid).
    pub root_uid: u32,
    /// HTTP access and state for `download` steps.
    pub downloads: download::Downloads,
    /// `DOT_SYSROOT`: every root-owned file (`root: true` entries, `package_repo` keyrings,
    /// sources and `.repo` files, `root: true` `absent` paths) is read and written under this
    /// directory instead, as this user, with no sudo and no owner changes: a sandbox without
    /// root can apply and converge them.
    pub sysroot: Option<std::path::PathBuf>,
}

impl Default for Builtins {
    fn default() -> Self {
        Self {
            interpreter: Interpreter::host_default(),
            links: Arc::new(SystemLinks::new()),
            packages: Packages::default(),
            github: github_release::Github::from_env(),
            git: git::Git::default(),
            system: System::default(),
            upgrade: false,
            etc_shells: dot_platform::user::ETC_SHELLS.into(),
            repos: package_repo::RepoNet::default(),
            root_uid: 0,
            downloads: download::Downloads::default(),
            sysroot: None,
        }
    }
}

impl Builtins {
    /// How `root: true` entries read and write root-owned files on `os`.
    pub fn root_files(&self, os: dot_core::facts::Os) -> rootfile::RootFiles {
        rootfile::RootFiles::new(self.system.clone(), os, self.root_uid)
            .with_sysroot(self.sysroot.clone())
    }
}

impl Planner for Builtins {
    fn plan(&self, directive: &Directive, ctx: &PlanCtx<'_>) -> Result<Vec<Box<dyn Action>>> {
        match directive {
            Directive::Link(entries) => link::plan(entries, ctx, &self.links),
            Directive::Create(entries) => {
                create::plan(entries, ctx, &self.root_files(ctx.facts.os))
            }
            Directive::Shell(spec) => shell::plan(spec, ctx, self.interpreter, self.upgrade),
            Directive::Clean(entries) => clean::plan(entries, ctx, &self.links),
            Directive::Package(spec) => package::plan(spec, ctx, &self.packages, self.upgrade),
            Directive::GithubRelease(spec) => {
                github_release::plan(spec, ctx, &self.github, &self.system)
            }
            Directive::Git(spec) => git::plan(spec, ctx, &self.git),
            Directive::Download(spec) => download::plan(
                spec,
                ctx,
                &download::Env {
                    dl: &self.downloads,
                    sys: &self.system,
                    root_files: &self.root_files(ctx.facts.os),
                    github: &self.github,
                    links: &self.links,
                },
            ),
            Directive::Template(entries) => {
                template::plan(entries, ctx, &self.root_files(ctx.facts.os))
            }
            Directive::Copy(entries) => {
                copy::plan(entries, ctx, &self.links, &self.root_files(ctx.facts.os))
            }
            Directive::SecretFile(entries) => secret_file::plan(entries, ctx),
            Directive::Block(spec) => block::plan(spec, ctx, &self.root_files(ctx.facts.os)),
            Directive::Path(entries) => runenv::plan_path(entries, ctx),
            Directive::Env(vars) => runenv::plan_env(vars, ctx),
            Directive::MacosDefaults(entries) => macos_defaults::plan(entries, ctx, &self.system),
            Directive::Service(spec) => service::plan(spec, ctx, &self.system),
            Directive::PackageRepo(spec) => package_repo::plan(
                spec,
                ctx,
                &self.packages,
                &self.repos,
                self.sysroot.as_deref(),
            ),
            Directive::User(spec) => user::plan(spec, ctx, &self.system, &self.etc_shells),
            Directive::Absent(entries) => absent::plan(
                entries,
                ctx,
                &self.system,
                &self.links,
                self.sysroot.as_deref(),
            ),
        }
    }

    /// Runs a bundle `check:` in the bundle directory, output discarded, stdin closed, through
    /// the bundle's default interpreter (else the platform's).
    fn check(&self, command: &str, bundle: &Bundle, env: &[(String, String)]) -> Result<bool> {
        let interpreter = bundle
            .manifest
            .defaults
            .shell
            .interpreter
            .map_or(self.interpreter, shell::interpreter_of);
        interpreter.validate(command).map_err(anyhow::Error::msg)?;
        let status = interpreter
            .command(command)
            .current_dir(&bundle.dir)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("running check `{command}`"))?;
        Ok(status.success())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;

    use dot_core::event::Recorder;
    use dot_core::{EventSink, Options, engine};

    use super::*;
    use crate::testutil::Fixture;

    #[test]
    fn dry_run_changes_nothing() {
        let f = Fixture::new(
            "steps:\n  - create: [~/made]\n  - link: { ~/.rc: rc }\n  - shell: { cmd: touch ran, cwd: ~/ }\n  - clean: [~/]\n",
        );
        f.write_repo("rc", "");
        std::os::unix::fs::symlink(f.repo().join("gone"), f.home().join("dangling")).unwrap();
        let before = listing(f.home());
        let rec = Recorder::default();
        let sink = EventSink::new().with(Box::new(rec.clone()));
        let summary = engine::run(
            std::slice::from_ref(&f.bundle),
            &Builtins::default(),
            &f.context(),
            &sink,
            Options {
                dry_run: true,
                ..Default::default()
            },
        );
        assert_eq!(listing(f.home()), before);
        assert_eq!((summary.changed, summary.failed), (4, 0), "{summary:?}");

        // The same plan applied for real changes all four things.
        let summary = engine::run(
            std::slice::from_ref(&f.bundle),
            &Builtins::default(),
            &f.context(),
            &EventSink::new(),
            Options::default(),
        );
        assert_eq!((summary.changed, summary.failed), (4, 0), "{summary:?}");
        assert!(f.home().join("made").is_dir());
        assert!(f.home().join("ran").exists());
        assert!(fs::symlink_metadata(f.home().join("dangling")).is_err());

        // And a second run has nothing left to do (the shell step always runs).
        let summary = engine::run(
            std::slice::from_ref(&f.bundle),
            &Builtins::default(),
            &f.context(),
            &EventSink::new(),
            Options::default(),
        );
        assert_eq!((summary.changed, summary.ok), (1, 3), "{summary:?}");
    }

    fn run(f: &Fixture) -> (dot_core::Summary, Vec<dot_core::Event>) {
        let rec = Recorder::default();
        let sink = EventSink::new().with(Box::new(rec.clone()));
        let summary = engine::run(
            std::slice::from_ref(&f.bundle),
            &Builtins::default(),
            &f.context(),
            &sink,
            Options::default(),
        );
        (summary, rec.events())
    }

    fn failures(events: &[dot_core::Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                dot_core::Event::StepFinished {
                    outcome: dot_core::Outcome::Failed,
                    detail,
                    ..
                } => detail.clone(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn later_steps_see_what_earlier_steps_made() {
        // A download-then-link, a create-then-use and a clean of a link an earlier step broke:
        // all converge on the first run.
        let f = Fixture::new(
            r#"steps:
  - shell: { cmd: 'mkdir -p oh && echo x > oh/theme', cwd: '~/' }
  - link: { ~/.theme: ~/oh/theme }
  - create: [~/src]
  - shell: { cmd: touch made, cwd: ~/src, unless: test -e made }
  - shell: rm soon-gone
  - clean: [~/]
"#,
        );
        f.write_repo("soon-gone", "");
        std::os::unix::fs::symlink(f.repo().join("soon-gone"), f.home().join("stale")).unwrap();
        let (summary, events) = run(&f);
        assert!(summary.success(), "{summary:?} {:?}", failures(&events));
        assert_eq!(summary.changed, 6, "{summary:?}");
        assert_eq!(fs::read_to_string(f.home().join(".theme")).unwrap(), "x\n");
        assert!(f.home().join("src/made").exists());
        assert!(fs::symlink_metadata(f.home().join("stale")).is_err());
    }

    #[test]
    fn self_referencing_link_fails_the_step_and_keeps_the_source() {
        let f = Fixture::new(
            "steps:\n  - link: { ~/.config/nvim: nvim }\n  - link: { ~/.config/nvim/init.lua: nvim/init.lua }\n",
        );
        f.write_repo("nvim/init.lua", "real content");
        let (summary, events) = run(&f);
        assert_eq!((summary.changed, summary.failed), (1, 1), "{summary:?}");
        let failed = failures(&events);
        assert!(failed[0].contains("inside the repository"), "{failed:?}");
        let src = f.repo().join("nvim/init.lua");
        assert_eq!(fs::read_to_string(&src).unwrap(), "real content");
        assert!(!f.repo().join("nvim/init.lua.dot-backup").exists());
    }

    #[test]
    fn linking_over_home_is_refused_before_anything_runs() {
        let f = Fixture::new(
            "steps:\n  - create: [~/first]\n  - link: { '~/': { src: dir, on_conflict: overwrite } }\n",
        );
        f.write_repo("dir/x", "");
        fs::write(f.home().join("keep"), "precious").unwrap();
        let (summary, events) = run(&f);
        assert!(!summary.success());
        assert!(
            failures(&events)[0].contains("home directory"),
            "{events:?}"
        );
        assert_eq!(
            fs::read_to_string(f.home().join("keep")).unwrap(),
            "precious"
        );
        assert!(
            !f.home().join("first").exists(),
            "the bundle was not applied at all"
        );
    }

    #[test]
    fn bundle_check_skips_a_satisfied_bundle() {
        let f = Fixture::new("check: test -e marker\nsteps:\n  - shell: touch ran\n");
        let (summary, _) = run(&f);
        assert_eq!(summary.changed, 1);
        fs::remove_file(f.repo().join("ran")).unwrap();
        fs::write(f.repo().join("marker"), "").unwrap();
        let (summary, _) = run(&f);
        assert_eq!((summary.changed, summary.skipped), (0, 1));
        assert!(!f.repo().join("ran").exists());
        assert!(Builtins::default().check("true", &f.bundle, &[]).unwrap());
        assert!(!Builtins::default().check("false", &f.bundle, &[]).unwrap());
    }

    fn listing(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}
