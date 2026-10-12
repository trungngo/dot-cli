//! `user`: a user's login shell and supplementary groups (default: the user running `dot`).
//!
//! - `shell: zsh` resolves the name through `/etc/shells` (else `PATH`), adds the path to
//!   `/etc/shells` when it is missing there, and makes it the login shell (`usermod -s` / `chsh
//!   -s` on Linux, `dscl` on macOS).
//! - `groups: [docker]` adds the user to each group it is not in yet (`usermod -aG`, or
//!   `dseditgroup` on macOS), creating a group that does not exist. Other groups are left alone.
//!
//! Every change runs as root (through `sudo -n`, asked for up front), and only takes effect at
//! the next login: applying leaves a run notice saying so, which every renderer prints with the
//! summary. Not supported on Windows.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use dot_core::config::UserSpec;
use dot_core::facts::Os;
use dot_core::{Action, Outcome, PlanCtx, Resource, RunCtx, Status, Stream, Undo};
use dot_platform::exec::System;
use dot_platform::pkg::repo::install_file;
use dot_platform::user::{Accounts, resolve_shell_for, shell_listed, with_shell};

use crate::syscmd::run;

pub fn plan(
    spec: &UserSpec,
    ctx: &PlanCtx<'_>,
    sys: &System,
    etc_shells: &Path,
) -> Result<Vec<Box<dyn Action>>> {
    if ctx.facts.os == Os::Windows {
        return Err(ctx.config_error(
            "user: not supported on Windows; add `when: { os: [linux, macos] }` to the step",
        ));
    }
    let user = match &spec.name {
        Some(n) => ctx.render(n)?,
        None => sys.user.clone().ok_or_else(|| {
            ctx.config_error("user: cannot tell who runs dot ($USER is not set); set `name:`")
        })?,
    };
    let mut actions: Vec<Box<dyn Action>> = Vec::new();
    if let Some(shell) = &spec.shell {
        actions.push(Box::new(ShellAction {
            user: user.clone(),
            wanted: ctx.render(shell)?,
            etc_shells: etc_shells.to_path_buf(),
            os: ctx.facts.os,
            sys: sys.clone(),
        }));
    }
    if !spec.groups.is_empty() {
        let groups = spec
            .groups
            .iter()
            .map(|g| ctx.render(g))
            .collect::<Result<Vec<_>>>()?;
        actions.push(Box::new(GroupsAction {
            user,
            groups,
            os: ctx.facts.os,
            sys: sys.clone(),
        }));
    }
    Ok(actions)
}

/// A query's answer, with "the tool is not installed" as `None` (unknown, not an error).
fn answered<T>(r: io::Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn root_note(sys: &System) -> Option<&'static str> {
    (!sys.root).then_some("sudo")
}

pub struct ShellAction {
    user: String,
    /// As configured: a name or a path.
    wanted: String,
    etc_shells: PathBuf,
    os: Os,
    sys: System,
}

/// What the shell step would change.
struct ShellState {
    path: PathBuf,
    current: Option<String>,
    listed: bool,
}

impl ShellAction {
    fn accounts(&self) -> Accounts<'_> {
        Accounts {
            os: self.os,
            runner: self.sys.runner.as_ref(),
        }
    }

    fn shells_text(&self) -> Result<String> {
        match std::fs::read_to_string(&self.etc_shells) {
            Ok(t) => Ok(t),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.etc_shells.display())),
        }
    }

    /// The shell's path for a user whose shell is `current` ([`resolve_shell_for`]).
    fn resolve(&self, current: Option<&str>, etc_shells: &str) -> Option<PathBuf> {
        resolve_shell_for(&self.wanted, current, etc_shells, Path::exists, |n| {
            dot_platform::exec::which(n)
        })
    }

    /// `None`: the shell is not installed (yet), or the account cannot be read (no `getent`).
    fn state(&self) -> Result<Option<ShellState>> {
        let text = self.shells_text()?;
        let Some(current) = answered(self.accounts().shell(&self.user))? else {
            return Ok(None);
        };
        let Some(path) = self.resolve(current.as_deref(), &text) else {
            return Ok(None);
        };
        Ok(Some(ShellState {
            listed: shell_listed(&text, &path),
            path,
            current,
        }))
    }
}

impl Action for ShellAction {
    fn describe(&self) -> String {
        let base = format!("login shell of {}", self.user);
        match self.state() {
            Ok(Some(s)) => {
                let path = s.path.display().to_string();
                if s.current.as_deref() == Some(path.as_str()) && s.listed {
                    return format!("{base}: {path}");
                }
                let mut d = format!(
                    "{base}: {} -> {path}",
                    s.current.as_deref().unwrap_or("(none)")
                );
                let mut notes: Vec<&str> = Vec::new();
                let add = format!("adds it to {}", self.etc_shells.display());
                if !s.listed {
                    notes.push(&add);
                }
                notes.extend(root_note(&self.sys));
                notes.push("takes effect at the next login");
                d.push_str(&format!(" [{}]", notes.join("; ")));
                d
            }
            Ok(None) => format!(
                "{base}: {} (not installed yet, or the account cannot be read)",
                self.wanted
            ),
            Err(e) => format!("{base}: {} (cannot check: {e:#})", self.wanted),
        }
    }

    fn status(&self) -> Result<Status> {
        Ok(match self.state()? {
            None => Status::Unknown,
            Some(s) if s.listed && s.current.as_deref() == Some(&*s.path.to_string_lossy()) => {
                Status::Satisfied
            }
            Some(_) => Status::Pending,
        })
    }

    /// `user:<name> shell`: changed back by hand (`chsh`).
    fn resources(&self) -> Vec<Resource> {
        vec![
            Resource::new("user", format!("{} shell", self.user)).with_undo(Undo::Manual {
                note: format!("change {}'s login shell back with chsh", self.user),
            }),
        ]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let text = self.shells_text()?;
        let current = self
            .accounts()
            .shell(&self.user)
            .with_context(|| format!("reading the login shell of {}", self.user))?;
        let Some(path) = self.resolve(current.as_deref(), &text) else {
            bail!(
                "shell `{}` is not installed: not in {} and not on PATH",
                self.wanted,
                self.etc_shells.display()
            );
        };
        let mut changed = false;
        if !shell_listed(&text, &path) {
            let dir = tempfile::tempdir().context("creating a temporary directory")?;
            let tmp = dir.path().join("shells");
            std::fs::write(&tmp, with_shell(&text, &path))
                .with_context(|| format!("writing {}", tmp.display()))?;
            ctx.output(
                Stream::Stdout,
                format!("adding {} to {}", path.display(), self.etc_shells.display()),
            );
            let cmd = self
                .sys
                .privileged(install_file(&tmp, &self.etc_shells, 0o644));
            run(&self.sys, &cmd, ctx)?;
            changed = true;
        }
        let path_s = path.display().to_string();
        if current.as_deref() != Some(path_s.as_str()) {
            let cmd = self
                .sys
                .privileged(self.accounts().set_shell(&self.user, &path));
            run(&self.sys, &cmd, ctx)?;
            ctx.notice(format!(
                "log out and back in: the login shell of {} is now {path_s}",
                self.user
            ));
            changed = true;
        }
        Ok(if changed {
            Outcome::Changed
        } else {
            Outcome::Ok
        })
    }

    fn needs_sudo(&self) -> bool {
        !self.sys.root && !cfg!(windows)
    }
}

pub struct GroupsAction {
    user: String,
    groups: Vec<String>,
    os: Os,
    sys: System,
}

impl GroupsAction {
    fn accounts(&self) -> Accounts<'_> {
        Accounts {
            os: self.os,
            runner: self.sys.runner.as_ref(),
        }
    }

    /// The configured groups the user is not in; `None` when that cannot be read (no `id`).
    fn missing(&self) -> Result<Option<Vec<String>>> {
        let Some(have) = answered(self.accounts().groups(&self.user))? else {
            return Ok(None);
        };
        Ok(Some(
            self.groups
                .iter()
                .filter(|g| !have.contains(g))
                .cloned()
                .collect(),
        ))
    }
}

impl Action for GroupsAction {
    fn describe(&self) -> String {
        let base = format!("groups of {}", self.user);
        match self.missing() {
            Ok(Some(m)) if m.is_empty() => format!("{base}: in {}", self.groups.join(", ")),
            Ok(Some(m)) => {
                let mut d = format!("{base}: add {}", m.join(", "));
                let already: Vec<&str> = self
                    .groups
                    .iter()
                    .filter(|g| !m.contains(g))
                    .map(String::as_str)
                    .collect();
                if !already.is_empty() {
                    d.push_str(&format!(" (already in: {})", already.join(", ")));
                }
                let mut notes: Vec<&str> = root_note(&self.sys).into_iter().collect();
                notes.push("takes effect at the next login");
                d.push_str(&format!(" [{}]", notes.join("; ")));
                d
            }
            Ok(None) => format!(
                "{base}: add {} (cannot read the account)",
                self.groups.join(", ")
            ),
            Err(e) => format!("{base}: {} (cannot check: {e:#})", self.groups.join(", ")),
        }
    }

    fn status(&self) -> Result<Status> {
        Ok(match self.missing()? {
            None => Status::Unknown,
            Some(m) if m.is_empty() => Status::Satisfied,
            Some(_) => Status::Pending,
        })
    }

    /// `group:<group> <user>` for each group: left by hand.
    fn resources(&self) -> Vec<Resource> {
        self.groups
            .iter()
            .map(|g| {
                Resource::new("group", format!("{g} {}", self.user)).with_undo(Undo::Manual {
                    note: format!("remove {} from group {g} by hand if needed", self.user),
                })
            })
            .collect()
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let missing = self.missing()?.with_context(|| {
            format!("cannot read the groups of {} (`id` is missing)", self.user)
        })?;
        if missing.is_empty() {
            return Ok(Outcome::Ok);
        }
        let acc = self.accounts();
        for g in &missing {
            if !acc
                .group_exists(g)
                .with_context(|| format!("checking group {g}"))?
            {
                ctx.output(Stream::Stdout, format!("creating group {g}"));
                run(&self.sys, &self.sys.privileged(acc.create_group(g)), ctx)?;
            }
            run(
                &self.sys,
                &self.sys.privileged(acc.add_to_group(&self.user, g)),
                ctx,
            )?;
        }
        ctx.notice(format!(
            "log out and back in: {} was added to group{} {}, which takes effect at the next \
             login",
            self.user,
            if missing.len() == 1 { "" } else { "s" },
            missing.join(", ")
        ));
        Ok(Outcome::Changed)
    }

    fn needs_sudo(&self) -> bool {
        !self.sys.root && !cfg!(windows)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use dot_core::event::Recorder;
    use dot_core::facts::{Facts, Role};
    use dot_core::{EventSink, Options, SecretRefs, engine};
    use dot_platform::exec::Output;
    use dot_platform::exec::fake::FakeExec;

    use super::*;
    use crate::Builtins;
    use crate::testutil::Fixture;

    struct Host {
        builtins: Builtins,
        fake: Arc<FakeExec>,
        _dir: tempfile::TempDir,
        shells: PathBuf,
        zsh: PathBuf,
    }

    /// A machine with `/etc/shells` (listing `/bin/sh`) in a temp dir and an executable `zsh`
    /// next to it; user `user`, not root.
    fn host() -> Host {
        let dir = tempfile::tempdir().unwrap();
        let shells = dir.path().join("shells");
        fs::write(&shells, "# valid login shells\n/bin/sh\n").unwrap();
        let zsh = dir.path().join("zsh");
        fs::write(&zsh, "#!/bin/sh\n").unwrap();
        let fake = FakeExec::new();
        let builtins = Builtins {
            system: System::fake(fake.clone()),
            etc_shells: shells.clone(),
            ..Builtins::default()
        };
        Host {
            builtins,
            fake,
            _dir: dir,
            shells,
            zsh,
        }
    }

    fn plan_on(f: &Fixture, h: &Host, os: Os) -> Result<Vec<Box<dyn Action>>> {
        let mut ctx = f.context();
        ctx.facts = Facts::new(os, "x86_64", Role::Desktop);
        let refs = SecretRefs::default();
        let s = &f.bundle.steps[0];
        let pctx = PlanCtx::new(&ctx, &f.bundle, s, &refs);
        dot_core::Planner::plan(&h.builtins, &s.directive, &pctx)
    }

    #[test]
    fn linux_shell_is_listed_then_set_through_sudo_with_a_notice() {
        let h = host();
        let f = Fixture::new(&format!(
            "steps:\n  - user: {{ shell: {} }}\n",
            h.zsh.display()
        ));
        h.fake.on(
            "getent passwd user",
            Output::ok("user:x:501:20::/home/user:/bin/bash\n"),
        );
        let actions = plan_on(&f, &h, Os::Linux).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        let zsh = h.zsh.display().to_string();
        assert_eq!(
            actions[0].describe(),
            format!(
                "login shell of user: /bin/bash -> {zsh} [adds it to {}; sudo; takes effect at the next login]",
                h.shells.display()
            )
        );
        assert!(actions[0].needs_sudo());
        h.fake.clear_calls();
        let rec = Recorder::default();
        let sink = EventSink::new().with(Box::new(rec.clone()));
        let ctx = RunCtx::new(&sink, dot_core::StepId { bundle: 0, step: 0 });
        assert_eq!(actions[0].apply(&ctx).unwrap(), Outcome::Changed);
        let calls = h.fake.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(calls[0], "getent passwd user");
        assert!(
            calls[1].starts_with("sudo -n install -m 0644 ")
                && calls[1].ends_with(&format!("/shells {}", h.shells.display())),
            "{calls:?}"
        );
        assert_eq!(calls[2], format!("sudo -n usermod -s {zsh} user"));
        assert_eq!(
            ctx.notices(),
            [format!(
                "log out and back in: the login shell of user is now {zsh}"
            )]
        );

        // Listed and set: satisfied.
        fs::write(&h.shells, format!("/bin/sh\n{zsh}\n")).unwrap();
        h.fake.on(
            "getent passwd user",
            Output::ok(format!("user:x:501:20::/home/user:{zsh}\n")),
        );
        let actions = plan_on(&f, &h, Os::Linux).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
        assert_eq!(actions[0].describe(), format!("login shell of user: {zsh}"));
    }

    #[test]
    fn a_listed_shell_of_the_same_name_is_left_alone() {
        // Debian lists /bin/zsh and /usr/bin/zsh; the user has the second (here: two files
        // named zsh in the temp dir, listed in that order).
        let h = host();
        let other = h.zsh.parent().unwrap().join("usr/bin/zsh");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::write(&other, "#!/bin/sh\n").unwrap();
        fs::write(
            &h.shells,
            format!("/bin/sh\n{}\n{}\n", h.zsh.display(), other.display()),
        )
        .unwrap();
        h.fake.on(
            "getent passwd user",
            Output::ok(format!("user:x:501:20::/home/user:{}\n", other.display())),
        );
        let f = Fixture::new("steps:\n  - user: { shell: zsh }\n");
        let actions = plan_on(&f, &h, Os::Linux).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
        assert_eq!(
            actions[0].describe(),
            format!("login shell of user: {}", other.display())
        );
        h.fake.clear_calls();
        let sink = EventSink::new();
        let ctx = RunCtx::new(&sink, dot_core::StepId { bundle: 0, step: 0 });
        assert_eq!(actions[0].apply(&ctx).unwrap(), Outcome::Ok);
        assert_eq!(h.fake.calls(), ["getent passwd user"], "no usermod");
        assert!(ctx.notices().is_empty());
    }

    #[test]
    fn macos_uses_dscl_and_a_missing_shell_is_unknown() {
        let h = host();
        fs::write(&h.shells, format!("/bin/sh\n{}\n", h.zsh.display())).unwrap();
        let f = Fixture::new("steps:\n  - user: { shell: zsh, name: alice }\n");
        h.fake.on(
            "dscl . -read /Users/alice UserShell",
            Output::ok("UserShell: /bin/bash\n"),
        );
        let actions = plan_on(&f, &h, Os::Macos).unwrap();
        assert_eq!(
            actions[0].status().unwrap(),
            Status::Pending,
            "zsh found via /etc/shells"
        );
        f.apply(&actions);
        assert_eq!(
            h.fake.calls().last().unwrap(),
            &format!(
                "sudo -n dscl . -create /Users/alice UserShell {}",
                h.zsh.display()
            )
        );

        let f = Fixture::new("steps:\n  - user: { shell: dot-no-such-shell-xyz }\n");
        let actions = plan_on(&f, &h, Os::Macos).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Unknown);
        assert!(actions[0].describe().contains("not installed yet"));
        let e = f.apply_err(actions[0].as_ref());
        assert!(e.contains("is not installed"), "{e}");
    }

    #[test]
    fn groups_are_added_created_when_missing_and_noticed_once() {
        let h = host();
        let f = Fixture::new("steps:\n  - user: { groups: [docker, sudo, libvirt] }\n");
        h.fake.on("id -nG user", Output::ok("user sudo\n"));
        h.fake.on("getent group libvirt", Output::exit(2, ""));
        let actions = plan_on(&f, &h, Os::Linux).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert_eq!(
            actions[0].describe(),
            "groups of user: add docker, libvirt (already in: sudo) [sudo; takes effect at the next login]"
        );
        let rec = Recorder::default();
        let sink = EventSink::new().with(Box::new(rec.clone()));
        let ctx = RunCtx::new(&sink, dot_core::StepId { bundle: 0, step: 0 });
        h.fake.clear_calls();
        actions[0].apply(&ctx).unwrap();
        assert_eq!(
            h.fake.calls(),
            [
                "id -nG user",
                "getent group docker",
                "sudo -n usermod -aG docker user",
                "getent group libvirt",
                "sudo -n groupadd libvirt",
                "sudo -n usermod -aG libvirt user",
            ]
        );
        assert_eq!(
            ctx.notices(),
            [
                "log out and back in: user was added to groups docker, libvirt, which takes effect at the next login"
            ]
        );
        h.fake
            .on("id -nG user", Output::ok("user sudo docker libvirt\n"));
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);

        // macOS: dseditgroup.
        h.fake.on("id -nG user", Output::ok("staff\n"));
        let f = Fixture::new("steps:\n  - user: { groups: [admin] }\n");
        let actions = plan_on(&f, &h, Os::Macos).unwrap();
        h.fake.clear_calls();
        f.apply(&actions);
        assert_eq!(
            h.fake.calls(),
            [
                "id -nG user",
                "dscl . -read /Groups/admin",
                "sudo -n dseditgroup -o edit -a user -t user admin"
            ]
        );
    }

    #[test]
    fn notices_reach_the_summary_and_windows_is_refused() {
        let h = host();
        let f = Fixture::new(
            "steps:\n  - user: { groups: [docker] }\n  - user: { groups: [docker] }\n",
        );
        h.fake.on("id -nG user", Output::ok("user\n"));
        let mut ctx = f.context();
        ctx.facts = Facts::new(Os::Linux, "x86_64", Role::Desktop);
        let summary = engine::run(
            std::slice::from_ref(&f.bundle),
            &h.builtins,
            &ctx,
            &EventSink::new(),
            Options::default(),
        );
        assert!(summary.success(), "{summary:?}");
        assert_eq!(
            summary.notices,
            [
                "log out and back in: user was added to group docker, which takes effect at the next login"
            ],
            "each notice once"
        );

        let e = match plan_on(&f, &h, Os::Windows) {
            Ok(_) => panic!("expected an error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(e.contains("not supported on Windows"), "{e}");
    }
}
