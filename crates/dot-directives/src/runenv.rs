//! `path` and `env`: add directories to `PATH` and set environment variables for the later
//! steps of the same run (see [`dot_core::runenv`]), and record them for `dot env --shell`.
//!
//! Entries are recorded while the step is planned, so later steps see them both in the plan and
//! while applying. An entry is satisfied when an earlier real run recorded it with the same value
//! (`env.json`, what `dot env --shell` writes out; see [`RunEnvCell::recorded`]), whatever the
//! environment `dot` was started with; otherwise it is reported as a change, and the run records
//! it.
//!
//! `persist: user` on a `path:` entry also writes it to the Windows user `Path` in the registry
//! (resource `user_path:<dir>`, taken out again by `dot remove` when `dot` added it); there is
//! nothing to persist on other systems, which the plan says.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result};
use dot_core::config::{PathDirEntry, PathPersist};
use dot_core::facts::Os;
use dot_core::runenv::{EnvVar, PathEntry, PathPosition, RunEnvCell};
use dot_core::{Action, Outcome, PlanCtx, Resource, RunCtx, Status, Undo};

pub fn plan_path(entries: &[PathDirEntry], ctx: &PlanCtx<'_>) -> Result<Vec<Box<dyn Action>>> {
    let dirs: Vec<(PathBuf, PathPosition)> = entries
        .iter()
        .map(|e| Ok((ctx.target(&e.dir)?, e.position)))
        .collect::<Result<_>>()?;
    let windows = ctx.facts.os == Os::Windows;
    let bundle = &ctx.bundle.name;
    ctx.run_env().update(|env| {
        // Prepends in reverse, so the first listed ends up first.
        for (dir, pos) in dirs
            .iter()
            .rev()
            .filter(|(_, p)| *p == PathPosition::Prepend)
        {
            env.add_path(PathEntry {
                dir: dir.clone(),
                position: *pos,
                bundle: bundle.clone(),
            });
        }
        for (dir, pos) in dirs.iter().filter(|(_, p)| *p == PathPosition::Append) {
            env.add_path(PathEntry {
                dir: dir.clone(),
                position: *pos,
                bundle: bundle.clone(),
            });
        }
    });
    Ok(dirs
        .into_iter()
        .zip(entries)
        .map(|((dir, position), e)| {
            let persist = match e.persist {
                None => Persist::No,
                Some(PathPersist::User) if windows => Persist::User {
                    added: AtomicBool::new(false),
                },
                Some(PathPersist::User) => Persist::Unsupported,
            };
            Box::new(PathAction {
                show: ctx.show(&dir),
                dir,
                position,
                env: ctx.run_env().clone(),
                persist,
            }) as Box<dyn Action>
        })
        .collect())
}

pub fn plan_env(vars: &[(String, String)], ctx: &PlanCtx<'_>) -> Result<Vec<Box<dyn Action>>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, value) in vars {
        let r = ctx.render_text(value, &format!("env `{name}`"))?;
        if !r.secrets.is_empty() {
            return Err(ctx.config_error(format!(
                "env `{name}`: secret() cannot be used here: the value would be written to \
                 `dot env --shell` output; use a `shell` step's `export_sensitive` or a \
                 `secret_file` instead"
            )));
        }
        out.push((name.clone(), r.text));
    }
    let bundle = &ctx.bundle.name;
    ctx.run_env().update(|env| {
        for (name, value) in &out {
            env.set_var(EnvVar {
                name: name.clone(),
                value: value.clone(),
                bundle: bundle.clone(),
            });
        }
    });
    Ok(out
        .into_iter()
        .map(|(name, value)| {
            Box::new(EnvAction {
                name,
                value,
                env: ctx.run_env().clone(),
            }) as Box<dyn Action>
        })
        .collect())
}

pub struct PathAction {
    pub dir: PathBuf,
    pub position: PathPosition,
    env: RunEnvCell,
    show: String,
    persist: Persist,
}

/// `persist:` of a `path:` entry, as it applies on this system.
enum Persist {
    No,
    /// Windows: the user `Path` in the registry. `added`: this run added it there.
    User {
        added: AtomicBool,
    },
    /// `persist: user` anywhere but Windows: nothing to do.
    Unsupported,
}

impl PathAction {
    /// The registry is in step (or there is nothing to persist).
    fn persisted(&self) -> bool {
        match self.persist {
            Persist::User { .. } => dot_platform::winpath::user_path_has(&self.dir),
            Persist::No | Persist::Unsupported => true,
        }
    }
}

impl Action for PathAction {
    fn describe(&self) -> String {
        let verb = match self.position {
            PathPosition::Prepend => "prepend",
            PathPosition::Append => "append",
        };
        let mut d = format!("PATH: {verb} {}", self.show);
        let mut notes = Vec::new();
        if !self.dir.is_dir() {
            notes.push("does not exist yet");
        }
        match self.persist {
            Persist::User { .. } if self.persisted() => notes.push("in the user Path"),
            Persist::User { .. } => notes.push("and the user Path in the registry"),
            Persist::Unsupported => {
                notes.push("persist: user is Windows-only: nothing to persist here")
            }
            Persist::No => {}
        }
        if !notes.is_empty() {
            d.push_str(&format!(" ({})", notes.join("; ")));
        }
        d
    }

    fn status(&self) -> Result<Status> {
        Ok(
            if self.env.recorded().has_path(&self.dir, self.position) && self.persisted() {
                Status::Satisfied
            } else {
                Status::Pending
            },
        )
    }

    /// Recorded when planned; with `persist: user` on Windows, written to the registry too.
    fn apply(&self, _ctx: &RunCtx) -> Result<Outcome> {
        if let Persist::User { added } = &self.persist {
            let append = self.position == PathPosition::Append;
            let did = dot_platform::winpath::add_user_path(&self.dir, append)
                .with_context(|| format!("adding {} to the user Path", self.show))?;
            added.store(did, Ordering::Relaxed);
        }
        Ok(Outcome::Changed)
    }

    /// `path:<dir>`: `dot remove` drops it from the record. `user_path:<dir>` (`persist: user`
    /// on Windows): `dot remove` takes it out of the registry, when `dot` put it there.
    fn resources(&self) -> Vec<Resource> {
        let mut out = vec![Resource::new("path", self.show.clone()).with_undo(Undo::Forget)];
        if let Persist::User { added } = &self.persist {
            out.push(
                Resource::new("user_path", self.show.clone())
                    .with_undo(Undo::RemoveUserPath {
                        dir: self.dir.clone(),
                    })
                    .changed(added.load(Ordering::Relaxed)),
            );
        }
        out
    }
}

pub struct EnvAction {
    pub name: String,
    pub value: String,
    env: RunEnvCell,
}

impl Action for EnvAction {
    fn describe(&self) -> String {
        format!("env {}={}", self.name, self.value)
    }

    fn status(&self) -> Result<Status> {
        Ok(
            if self.env.recorded().var(&self.name) == Some(self.value.as_str()) {
                Status::Satisfied
            } else {
                Status::Pending
            },
        )
    }

    /// Recorded when planned; nothing else to do.
    fn apply(&self, _ctx: &RunCtx) -> Result<Outcome> {
        Ok(Outcome::Changed)
    }

    /// `env:<NAME>`: `dot remove` drops it from the record.
    fn resources(&self) -> Vec<Resource> {
        vec![Resource::new("env", self.name.clone()).with_undo(Undo::Forget)]
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;

    use dot_core::runenv::{BaseEnv, EnvRecord};
    use dot_core::shellenv::{self, Shell};
    use dot_core::{EventSink, Options, engine};

    use super::*;
    use crate::Builtins;
    use crate::testutil::Fixture;

    fn ctx_with_base(f: &Fixture, path: &str, vars: &[(&str, &str)]) -> dot_core::Context {
        let mut ctx = f.context();
        ctx.run_env = RunEnvCell::new(BaseEnv {
            path: Some(path.into()),
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..BaseEnv::default()
        });
        ctx
    }

    #[test]
    fn later_steps_see_path_and_env_and_the_run_records_them() {
        let f = Fixture::new(concat!(
            "steps:\n",
            "  - path: [~/.local/bin, ~/bin]\n",
            "  - path: { /opt/late: { position: append } }\n",
            "  - env: { EDITOR: nvim, GREETING: \"hi {{ os }}\" }\n",
            "  - create: [~/.local/bin]\n",
            // `{{ home }}` is the test's home; `~` in a command is the shell's (the real one).
            "  - shell: printf '#!/bin/sh\\necho from-tool\\n' > {{ home }}/.local/bin/mytool && chmod +x {{ home }}/.local/bin/mytool\n",
            "  - shell: { cmd: 'mytool > out; echo \"$PATH|$EDITOR|$GREETING\" >> out', unless: 'command -v mytool && test -e out' }\n",
        ));
        let ctx = ctx_with_base(&f, "/usr/bin:/bin", &[("EDITOR", "nvim")]);
        let rec = dot_core::event::Recorder::default();
        let sink = EventSink::new().with(Box::new(rec.clone()));
        let summary = engine::run(
            std::slice::from_ref(&f.bundle),
            &Builtins::default(),
            &ctx,
            &sink,
            Options::default(),
        );
        assert!(summary.success(), "{summary:?} {:?}", rec.events());
        let home = f.home().display().to_string();
        let out = fs::read_to_string(f.repo().join("out")).unwrap();
        assert_eq!(
            out,
            format!(
                "from-tool\n{home}/.local/bin:{home}/bin:/usr/bin:/bin:/opt/late|nvim|hi linux\n"
            )
        );
        assert_eq!((summary.changed, summary.ok), (6, 0), "{summary:?}");
        let recorded = ctx.run_env.snapshot();
        let dirs: Vec<String> = recorded
            .path
            .iter()
            .map(|e| e.dir.display().to_string())
            .collect();
        assert_eq!(
            dirs,
            [
                format!("{home}/.local/bin"),
                format!("{home}/bin"),
                "/opt/late".into()
            ]
        );
        assert!(recorded.path.iter().all(|e| e.bundle == "demo"));
        let vars: std::collections::BTreeMap<String, String> = recorded
            .vars
            .iter()
            .map(|v| (v.name.clone(), v.value.clone()))
            .collect();
        let script = shellenv::render(Shell::Zsh, &vars, &recorded);
        assert!(script.contains("export GREETING='hi linux'"), "{script}");
        assert!(script.contains(&format!("PATH='{home}/bin'")), "{script}");

        // A second run with that record has nothing to do for these steps, whatever the
        // environment dot is started with (the variables are not exported in this one).
        let mut record = EnvRecord::default();
        record.replace_bundles(&["demo".into()], &recorded);
        let statuses = |ctx: &dot_core::Context| -> Vec<Status> {
            let plan = engine::plan(std::slice::from_ref(&f.bundle), &Builtins::default(), ctx);
            plan.bundles[0].steps[..3]
                .iter()
                .flat_map(|s| match &s.state {
                    dot_core::engine::PlannedState::Ready(a) => {
                        a.iter().map(|a| a.status).collect()
                    }
                    _ => Vec::new(),
                })
                .collect()
        };
        let mut ctx2 = ctx_with_base(&f, "/usr/bin:/bin", &[]);
        ctx2.run_env = ctx2.run_env.clone().with_recorded(record.env.clone());
        assert_eq!(statuses(&ctx2), [Status::Satisfied; 5]);

        // The live environment alone does not count: without the record, every entry is a
        // change (it is recorded again).
        let path = recorded
            .path_value(Some(std::ffi::OsStr::new("/usr/bin:/bin")))
            .unwrap();
        let live = ctx_with_base(
            &f,
            path.to_str().unwrap(),
            &[("EDITOR", "nvim"), ("GREETING", "hi linux")],
        );
        assert_eq!(statuses(&live), [Status::Pending; 5]);

        // A changed value, or a directory moved to the other end, is a change again.
        let mut changed = record.env.clone();
        changed.set_var(dot_core::runenv::EnvVar {
            name: "GREETING".into(),
            value: "hi macos".into(),
            bundle: "demo".into(),
        });
        changed.add_path(dot_core::runenv::PathEntry {
            dir: "/opt/late".into(),
            position: PathPosition::Prepend,
            bundle: "demo".into(),
        });
        let mut ctx3 = ctx_with_base(&f, "/usr/bin:/bin", &[]);
        ctx3.run_env = ctx3.run_env.clone().with_recorded(changed);
        assert_eq!(
            statuses(&ctx3),
            [
                Status::Satisfied,
                Status::Satisfied,
                Status::Pending,
                Status::Satisfied,
                Status::Pending
            ]
        );
    }

    #[test]
    fn persist_user_adds_a_registry_resource_on_windows_only() {
        let f = Fixture::new("steps:\n  - path: { /opt/x: { persist: user } }\n");
        let actions = f.plan(0).unwrap();
        assert_eq!(
            actions[0].describe(),
            "PATH: prepend /opt/x (does not exist yet; persist: user is Windows-only: nothing to \
             persist here)"
        );
        assert_eq!(actions[0].resources().len(), 1);

        let mut ctx = f.context();
        ctx.facts = dot_core::Facts::new(
            dot_core::facts::Os::Windows,
            "x86_64",
            dot_core::facts::Role::Desktop,
        );
        let actions = f.plan_with(0, &ctx).unwrap();
        // Planned for Windows on this host: the registry never lists it, so it is pending.
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert!(
            actions[0]
                .describe()
                .ends_with("and the user Path in the registry)"),
            "{}",
            actions[0].describe()
        );
        let r = actions[0].resources();
        assert_eq!(r[1].key(), "user_path:/opt/x");
        assert_eq!(
            r[1].undo,
            Some(Undo::RemoveUserPath {
                dir: "/opt/x".into()
            })
        );
        assert_eq!(r[1].changed, Some(false), "nothing added yet");
    }

    #[test]
    fn dry_runs_record_nothing_on_the_context() {
        let f = Fixture::new("steps:\n  - path: [~/bin]\n");
        let ctx = ctx_with_base(&f, "/bin", &[]);
        let plan = engine::plan(std::slice::from_ref(&f.bundle), &Builtins::default(), &ctx);
        assert!(!plan.has_errors());
        assert!(ctx.run_env.snapshot().is_empty());
        engine::run(
            std::slice::from_ref(&f.bundle),
            &Builtins::default(),
            &ctx,
            &EventSink::new(),
            Options {
                dry_run: true,
                ..Default::default()
            },
        );
        assert!(ctx.run_env.snapshot().is_empty());
    }

    #[test]
    fn env_rejects_secrets_and_bad_names() {
        let f = Fixture::new("steps:\n  - env: { TOKEN: \"{{ secret('t') }}\" }\n");
        let err = format!("{:#}", f.plan(0).err().unwrap());
        assert!(
            err.contains("bundle.yml:2:5: steps[0]: env `TOKEN`: secret()"),
            "{err}"
        );
        for (yaml, want) in [
            (
                "steps:\n  - env: { 1X: a }\n",
                "not a valid environment variable name",
            ),
            (
                "steps:\n  - env: { Path: a }\n",
                "set PATH with the `path:` directive",
            ),
            ("steps:\n  - env: {}\n", "expected at least one variable"),
            (
                "steps:\n  - path: [relative/bin]\n",
                "must be an absolute path",
            ),
        ] {
            let file = f.root().join("bad.yml");
            fs::write(&file, yaml).unwrap();
            let err = dot_core::config::load_bundle(&file)
                .unwrap_err()
                .to_string();
            assert!(err.contains(want), "{yaml}: {err}");
            assert!(err.contains("bad.yml:2:"), "{err}");
        }
    }
}
