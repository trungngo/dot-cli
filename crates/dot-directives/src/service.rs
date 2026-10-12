//! `service`: a service installed (optionally from a templated unit file or launchd plist),
//! enabled at boot / login and running now, or disabled and stopped. One action per concern, in
//! order: the unit file, lingering (systemd user services), enabled, running. Every system
//! command goes through the injected [`System`], so tests assert argv.
//!
//! - `systemd`: system units in `/etc/systemd/system`, managed through `sudo` unless root.
//! - `systemd-user`: `~/.config/systemd/user`, `systemctl --user`; `linger` keeps them running
//!   without a login session (`loginctl enable-linger`, through `sudo`).
//! - `launchd`: user agents in `~/Library/LaunchAgents/<name>.plist`, domain `gui/<uid>`.
//! - `windows`: the service control manager (`sc.exe`). Changes need an elevated terminal:
//!   the actions say so ([`Action::needs_sudo`]) and the run fails up front, listing them, when
//!   it is not elevated.
//!
//! Each manager is a [`ServiceManager`] (see [`dot_platform::service`]); this directive only
//! talks to the trait.
//!
//! A changed unit file is followed by a reload (systemd) and a restart when the service is
//! started ([`ServiceManager::started`]), so the change takes effect. Before the unit file is
//! written, a marker is left in `dot`'s state store (`services` section, `<manager>-<unit>`), removed
//! only once the reload and restart have succeeded: if either fails (or the run is interrupted),
//! the next run still sees the unit as pending and reloads and restarts, though the file on disk
//! is already up to date.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use dot_core::config::{ServiceManagerKind, ServiceSpec};
use dot_core::facts::Os;
use dot_core::sections::SERVICES;
use dot_core::{Action, Outcome, PlanCtx, Resource, RunCtx, StateStore, Status, Undo};
use dot_platform::exec::System;
use dot_platform::service::{self as platform, ManagerKind, ServiceManager};

use crate::fileutil::{Existing, atomic_write, change_note, new_file_note, with_notes};
use crate::syscmd::run_all;

/// The manager for `kind` on `os`, or why it cannot be used there.
pub fn resolve_manager(kind: ServiceManagerKind, os: Os) -> Result<ManagerKind, String> {
    let (resolved, needs) = match kind {
        ServiceManagerKind::Auto => {
            return match os {
                Os::Linux => Ok(ManagerKind::Systemd),
                Os::Macos => Ok(ManagerKind::Launchd),
                Os::Windows => Ok(ManagerKind::Windows),
            };
        }
        ServiceManagerKind::Systemd => (ManagerKind::Systemd, Os::Linux),
        ServiceManagerKind::SystemdUser => (ManagerKind::SystemdUser, Os::Linux),
        ServiceManagerKind::Launchd => (ManagerKind::Launchd, Os::Macos),
        ServiceManagerKind::Windows => (ManagerKind::Windows, Os::Windows),
    };
    if os != needs {
        return Err(format!(
            "service manager `{}` needs {} (this is {}); add a `when:` to the step",
            kind.as_str(),
            needs.as_str(),
            os.as_str()
        ));
    }
    Ok(resolved)
}

pub fn plan(spec: &ServiceSpec, ctx: &PlanCtx<'_>, sys: &System) -> Result<Vec<Box<dyn Action>>> {
    let kind = resolve_manager(spec.manager, ctx.facts.os).map_err(|e| ctx.config_error(e))?;
    let name = ctx.render(&spec.name)?;
    if let Some(why) = systemd_missing(kind, ctx.facts) {
        return Ok(vec![Box::new(NoManager {
            name,
            kind: kind.as_str(),
            why,
        })]);
    }
    let manager = platform::manager(kind, sys.clone(), ctx.home);
    let mut actions: Vec<Box<dyn Action>> = Vec::new();
    if let Some(unit) = &spec.unit {
        let src = ctx.source(unit)?;
        let show_src = ctx.show_source(&src);
        let content = match fs::read(&src) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes).map_err(|_| {
                    ctx.config_error(format!("unit source `{show_src}` is not UTF-8 text"))
                })?;
                let r = ctx.render_text(&text, &format!("unit `{show_src}`"))?;
                if !r.secrets.is_empty() {
                    return Err(ctx.config_error(format!(
                        "unit `{show_src}`: secret() cannot be used in a unit file, which \
                         others can read; write the secret with `secret_file` and point the \
                         unit at it (EnvironmentFile=, ...)"
                    )));
                }
                Some(r.text)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(ctx.config_error(format!("cannot read unit source `{show_src}`: {e}")));
            }
        };
        let dest = manager
            .unit_path(&name)
            .context("this service manager has no unit files")?;
        let marker = format!("{}-{}", kind.as_str(), manager.unit_name(&name));
        actions.push(Box::new(UnitAction {
            show: ctx.show(&dest),
            dest,
            marker,
            state: StateStore::at(ctx.state_dir),
            show_src,
            content,
            name: name.clone(),
            restart: spec.start,
            manager: manager.clone(),
        }));
    }
    if spec.linger {
        let user = sys.user.clone().ok_or_else(|| {
            ctx.config_error("linger: the user name is unknown ($USER is not set)")
        })?;
        actions.push(Box::new(LingerAction {
            user,
            manager: manager.clone(),
        }));
    }
    actions.push(Box::new(EnableAction {
        name: name.clone(),
        want: spec.enable,
        manager: manager.clone(),
    }));
    actions.push(Box::new(StartAction {
        name: name.clone(),
        want: spec.start,
        manager: manager.clone(),
    }));
    if spec.restart {
        actions.push(Box::new(RestartAction { name, manager }));
    }
    Ok(actions)
}

/// Why a systemd service cannot be managed here: systemd is not the running init (the `init`
/// fact is known and is not `systemd`): a container, or WSL without `[boot] systemd=true`.
/// `systemctl` may well be installed there, and then fails with "System has not been booted
/// with systemd".
fn systemd_missing(kind: ManagerKind, facts: &dot_core::Facts) -> Option<String> {
    if !matches!(kind, ManagerKind::Systemd | ManagerKind::SystemdUser) {
        return None;
    }
    let init = facts.init.as_deref()?;
    if init == "systemd" {
        return None;
    }
    let hint = if facts.env.iter().any(|e| e == "wsl") {
        "; enable it in /etc/wsl.conf (`[boot]` `systemd=true`) and restart WSL, or add \
         `when: { init: systemd }` to the step"
    } else {
        "; add `when: { init: systemd }` to the step"
    };
    Some(format!(
        "systemd is not running here (init: {init}; no {}){hint}",
        dot_core::facts::SYSTEMD_RUNNING
    ))
}

/// A `service` step for a manager that is not running: a conflict, never applied.
pub struct NoManager {
    name: String,
    kind: &'static str,
    why: String,
}

impl Action for NoManager {
    fn describe(&self) -> String {
        format!(
            "service {} ({}) (conflict: {})",
            self.name, self.kind, self.why
        )
    }

    fn status(&self) -> Result<Status> {
        Ok(Status::Conflict)
    }

    fn apply(&self, _ctx: &RunCtx) -> Result<Outcome> {
        bail!("cannot manage service {}: {}", self.name, self.why)
    }
}

/// `restart: true`: restart the service when it is running (a handler's usual job). Always
/// pending: a restart is an event, not a state.
pub struct RestartAction {
    name: String,
    manager: Arc<dyn ServiceManager>,
}

impl Action for RestartAction {
    fn needs_sudo(&self) -> bool {
        self.manager.changes_need_sudo()
    }

    fn describe(&self) -> String {
        format!(
            "restart {} ({})",
            self.manager.unit_name(&self.name),
            self.manager.kind().as_str()
        )
    }

    fn status(&self) -> Result<Status> {
        Ok(Status::Pending)
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let running = query(
            self.manager.started(&self.name),
            "whether the service is running",
        )?;
        // Not running (a `start` step failed silently, or the manager is missing): there is
        // nothing to restart.
        if running != Some(true) {
            return Ok(Outcome::Ok);
        }
        let cmds = self.manager.restart(&self.name)?;
        run_all(self.manager.system(), &cmds, ctx)?;
        Ok(Outcome::Changed)
    }
}

/// A query's answer, with "the manager is not installed" as `None` (an earlier step may install
/// it, so that is unknown rather than an error).
fn query<T>(r: io::Result<T>, what: &str) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("checking {what}")),
    }
}

/// Unit changes whose reload / restart has not happened yet, by `<manager>-<unit>`: the unit
/// file's path.
type Markers = BTreeMap<String, String>;

pub struct UnitAction {
    pub dest: PathBuf,
    /// Recorded while a written unit still needs its reload / restart.
    marker: String,
    state: StateStore,
    /// `None`: the source does not exist (yet).
    content: Option<String>,
    name: String,
    /// Restart the service after a change when it is running.
    restart: bool,
    manager: Arc<dyn ServiceManager>,
    show: String,
    show_src: String,
}

impl UnitAction {
    fn existing(&self) -> io::Result<Existing> {
        Existing::read(&self.dest)
    }

    /// An earlier run wrote the unit but did not get through its reload / restart.
    fn follow_up_pending(&self) -> bool {
        self.state
            .section::<Markers>(SERVICES)
            .contains_key(&self.marker)
    }

    fn mark(&self) -> Result<()> {
        let dest = self.dest.display().to_string();
        self.state.update_section(SERVICES, |m: &mut Markers| {
            m.insert(self.marker.clone(), dest);
        })
    }

    /// Reloads the manager and restarts the service when it is started, then clears the mark.
    fn take_effect(&self, ctx: &RunCtx) -> Result<()> {
        let sys = self.manager.system();
        run_all(sys, &self.manager.reload(), ctx)?;
        if self.restart && self.manager.started(&self.name).unwrap_or(false) {
            run_all(sys, &self.manager.restart(&self.name)?, ctx)?;
        }
        if !self.follow_up_pending() {
            return Ok(());
        }
        self.state.update_section(SERVICES, |m: &mut Markers| {
            m.remove(&self.marker);
        })
    }

    fn write(&self, content: &str, ctx: &RunCtx) -> Result<()> {
        if !self.manager.unit_needs_privilege() {
            return atomic_write(&self.dest, content.as_bytes(), 0o644);
        }
        // Written as the user, then installed as root.
        let tmp = std::env::temp_dir().join(format!(
            "dot-unit-{}-{}",
            std::process::id(),
            self.manager.unit_name(&self.name)
        ));
        let _ = fs::remove_file(&tmp);
        atomic_write(&tmp, content.as_bytes(), 0o644)?;
        let install = self.manager.install_unit(&tmp, &self.dest);
        let out = crate::syscmd::run(self.manager.system(), &install, ctx);
        let _ = fs::remove_file(&tmp);
        out.map(|_| ())
    }
}

impl Action for UnitAction {
    fn needs_sudo(&self) -> bool {
        self.manager.unit_needs_privilege()
    }

    fn describe(&self) -> String {
        let base = format!("install unit {} <- {}", self.show, self.show_src);
        let Some(content) = &self.content else {
            return with_notes(
                base,
                &["source does not exist yet; an earlier step may create it".into()],
            );
        };
        let note = match self.existing() {
            Ok(Existing::File { bytes, .. })
                if bytes == content.as_bytes() && self.follow_up_pending() =>
            {
                Some("written; reload and restart still pending from an earlier run".into())
            }
            Ok(Existing::File { bytes, .. }) if bytes == content.as_bytes() => None,
            Ok(Existing::File { bytes, .. }) => Some(change_note(&bytes, content.as_bytes())),
            Ok(Existing::Missing) => Some(new_file_note(content.as_bytes())),
            Ok(_) => Some("replaces what is there".into()),
            Err(e) => Some(format!("cannot inspect: {e}")),
        };
        let mut notes: Vec<String> = note.into_iter().collect();
        if self.manager.unit_needs_privilege() && !notes.is_empty() {
            notes.push("as root".into());
        }
        with_notes(base, &notes)
    }

    fn status(&self) -> Result<Status> {
        let Some(content) = &self.content else {
            return Ok(Status::Unknown);
        };
        Ok(match self.existing()? {
            Existing::File { bytes, .. }
                if bytes == content.as_bytes() && !self.follow_up_pending() =>
            {
                Status::Satisfied
            }
            Existing::Dir => Status::Conflict,
            _ => Status::Pending,
        })
    }

    /// `file:<unit>`: removed while it holds what `dot` wrote (as root for a system unit).
    fn resources(&self) -> Vec<Resource> {
        let root = self.manager.unit_needs_privilege();
        // A system unit is written as root: what `dot` writes is hashed.
        let file = match &self.content {
            Some(c) if root => crate::fileutil::written_bytes(&self.dest, c.as_bytes()),
            _ => crate::fileutil::written(&self.dest),
        };
        let r = Resource::new("file", self.show.clone()).with_undo(Undo::RemoveFiles {
            files: vec![file],
            root,
        });
        vec![if root { r } else { r.preserving(&self.dest) }]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let Some(content) = &self.content else {
            bail!("unit source not found: `{}`", self.show_src);
        };
        let existing = self.existing()?;
        if matches!(existing, Existing::Dir) {
            bail!("{} is a directory", self.dest.display());
        }
        let written = existing.bytes() == Some(content.as_bytes());
        if written && !self.follow_up_pending() {
            return Ok(Outcome::Ok);
        }
        if !written {
            // A system unit already there is saved before `dot` first writes over it (a user
            // unit is saved by the engine: [`Resource::preserve`]).
            if self.manager.unit_needs_privilege()
                && let Existing::File { bytes, mode } = &existing
            {
                ctx.preserve_root(
                    &format!("file:{}", self.show),
                    &self.dest,
                    bytes,
                    mode.unwrap_or(0o644),
                )?;
            }
            // Marked first: whatever fails from here on, the next run finishes the job.
            self.mark()?;
            self.write(content, ctx)?;
        }
        self.take_effect(ctx)?;
        Ok(Outcome::Changed)
    }
}

pub struct LingerAction {
    user: String,
    manager: Arc<dyn ServiceManager>,
}

impl Action for LingerAction {
    fn needs_sudo(&self) -> bool {
        self.manager.linger_needs_sudo()
    }

    fn describe(&self) -> String {
        format!("enable lingering for {}", self.user)
    }

    fn status(&self) -> Result<Status> {
        Ok(match query(self.manager.linger(&self.user), "lingering")? {
            None => Status::Unknown,
            Some(true) => Status::Satisfied,
            Some(false) => Status::Pending,
        })
    }

    /// `linger:<user>`: turned off by hand.
    fn resources(&self) -> Vec<Resource> {
        vec![
            Resource::new("linger", self.user.clone()).with_undo(Undo::Manual {
                note: format!("sudo loginctl disable-linger {}", self.user),
            }),
        ]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        run_all(
            self.manager.system(),
            &[self.manager.enable_linger(&self.user)],
            ctx,
        )?;
        Ok(Outcome::Changed)
    }
}

pub struct EnableAction {
    name: String,
    want: bool,
    manager: Arc<dyn ServiceManager>,
}

impl Action for EnableAction {
    fn needs_sudo(&self) -> bool {
        self.manager.changes_need_sudo()
    }

    fn describe(&self) -> String {
        let verb = if self.want { "enable" } else { "disable" };
        format!(
            "{verb} {} ({})",
            self.manager.unit_name(&self.name),
            self.manager.kind().as_str()
        )
    }

    fn status(&self) -> Result<Status> {
        let state = query(
            self.manager.enabled(&self.name),
            "whether the service is enabled",
        )?;
        Ok(match state {
            None => Status::Unknown,
            Some(Some(on)) if on == self.want => Status::Satisfied,
            // Unknown to the manager: nothing to disable. To enable, it must be installed first
            // (by this step's unit or an earlier step); applying says so if it still is not.
            Some(None) if !self.want => Status::Satisfied,
            Some(_) => Status::Pending,
        })
    }

    /// `service:<manager>:<unit>` when enabled: disabled and stopped by hand, with the commands
    /// the manager would run.
    fn resources(&self) -> Vec<Resource> {
        if !self.want {
            return Vec::new();
        }
        let unit = self.manager.unit_name(&self.name);
        let mut cmds: Vec<String> = Vec::new();
        for c in self
            .manager
            .set_enabled(&self.name, false)
            .into_iter()
            .chain(self.manager.stop(&self.name))
            .flatten()
        {
            cmds.push(c.display());
        }
        let note = if cmds.is_empty() {
            format!("disable and stop {unit} by hand")
        } else {
            format!("disable and stop it: {}", cmds.join("; "))
        };
        vec![
            Resource::new(
                "service",
                format!("{}:{unit}", self.manager.kind().as_str()),
            )
            .with_undo(Undo::Manual { note }),
        ]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let cmds = self.manager.set_enabled(&self.name, self.want)?;
        run_all(self.manager.system(), &cmds, ctx)?;
        Ok(Outcome::Changed)
    }
}

pub struct StartAction {
    name: String,
    want: bool,
    manager: Arc<dyn ServiceManager>,
}

impl Action for StartAction {
    fn needs_sudo(&self) -> bool {
        self.manager.changes_need_sudo()
    }

    fn describe(&self) -> String {
        let verb = if self.want { "start" } else { "stop" };
        format!(
            "{verb} {} ({})",
            self.manager.unit_name(&self.name),
            self.manager.kind().as_str()
        )
    }

    fn status(&self) -> Result<Status> {
        let active = query(
            self.manager.started(&self.name),
            "whether the service is running",
        )?;
        Ok(match active {
            None => Status::Unknown,
            Some(on) if on == self.want => Status::Satisfied,
            Some(_) => Status::Pending,
        })
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let cmds = if self.want {
            self.manager.start(&self.name)?
        } else {
            self.manager.stop(&self.name)?
        };
        run_all(self.manager.system(), &cmds, ctx)?;
        Ok(Outcome::Changed)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use dot_platform::exec::Output;

    use super::*;
    use crate::testutil::Fixture;
    use crate::testutil_sys::{fake_builtins, plan_on};

    #[test]
    fn systemd_user_unit_linger_enable_start() {
        let f = Fixture::new(concat!(
            "steps:\n",
            "  - service:\n",
            "      name: syncthing\n",
            "      manager: systemd-user\n",
            "      unit: syncthing.service.j2\n",
            "      linger: true\n",
        ));
        f.write_repo(
            "syncthing.service.j2",
            "[Service]\nExecStart=/usr/bin/syncthing --home={{ home }}/.config/syncthing\n",
        );
        let (b, fake) = fake_builtins();
        fake.on(
            "systemctl --user is-enabled",
            Output::exit(1, "not-found\n"),
        );
        fake.on("systemctl --user is-active", Output::exit(3, "inactive\n"));
        fake.on("loginctl show-user", Output::ok("no\n"));
        let actions = plan_on(&f, &b, Os::Linux, 0).unwrap();
        let described: Vec<String> = actions.iter().map(|a| a.describe()).collect();
        assert_eq!(
            described,
            [
                "install unit ~/.config/systemd/user/syncthing.service <- syncthing.service.j2 (new file, 2 lines)",
                "enable lingering for user",
                "enable syncthing.service (systemd-user)",
                "start syncthing.service (systemd-user)",
            ]
        );
        let statuses: Vec<Status> = actions.iter().map(|a| a.status().unwrap()).collect();
        assert_eq!(statuses, [Status::Pending; 4]);
        // Only lingering needs root: the run asks for sudo up front because of it.
        let sudo: Vec<bool> = actions.iter().map(|a| a.needs_sudo()).collect();
        assert_eq!(sudo, [false, true, false, false]);
        fake.clear_calls();
        f.apply(&actions);
        let unit = f.home().join(".config/systemd/user/syncthing.service");
        let home = f.home().display();
        assert_eq!(
            fs::read_to_string(&unit).unwrap(),
            format!("[Service]\nExecStart=/usr/bin/syncthing --home={home}/.config/syncthing\n")
        );
        let changes: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| !c.contains(" is-") && !c.contains("show-user"))
            .collect();
        assert_eq!(
            changes,
            [
                "systemctl --user daemon-reload",
                "sudo -n loginctl enable-linger user",
                "systemctl --user enable syncthing.service",
                "systemctl --user start syncthing.service",
            ]
        );
        // Converged.
        fake.on("systemctl --user is-enabled", Output::ok("enabled\n"));
        fake.on("systemctl --user is-active", Output::ok("active\n"));
        fake.on("loginctl show-user", Output::ok("yes\n"));
        let actions = plan_on(&f, &b, Os::Linux, 0).unwrap();
        assert!(
            actions
                .iter()
                .all(|a| a.status().unwrap() == Status::Satisfied)
        );
        assert_eq!(
            actions[0].describe(),
            "install unit ~/.config/systemd/user/syncthing.service <- syncthing.service.j2"
        );

        // A changed unit restarts the running service.
        fs::write(&unit, "old\n").unwrap();
        fake.clear_calls();
        f.apply(&actions[..1]);
        assert_eq!(
            fake.calls(),
            [
                "systemctl --user daemon-reload",
                "systemctl --user is-active syncthing.service",
                "systemctl --user restart syncthing.service",
            ]
        );
    }

    #[test]
    fn a_failed_reload_is_finished_by_the_next_run() {
        let f = Fixture::new(
            "steps:\n  - service: { name: app, manager: systemd-user, unit: app.service }\n",
        );
        f.write_repo("app.service", "[Service]\nExecStart=/new\n");
        let unit = f.home().join(".config/systemd/user/app.service");
        fs::create_dir_all(unit.parent().unwrap()).unwrap();
        fs::write(&unit, "[Service]\nExecStart=/old\n").unwrap();
        let (b, fake) = fake_builtins();
        fake.on("systemctl --user is-enabled", Output::ok("enabled\n"));
        fake.on("systemctl --user is-active", Output::ok("active\n"));
        fake.on(
            "systemctl --user daemon-reload",
            Output {
                code: Some(1),
                stdout: String::new(),
                stderr: "Failed to connect to bus\n".into(),
            },
        );
        let actions = plan_on(&f, &b, Os::Linux, 0).unwrap();
        let err = f.apply_err(actions[0].as_ref());
        assert!(err.contains("Failed to connect to bus"), "{err}");
        assert_eq!(
            fs::read_to_string(&unit).unwrap(),
            "[Service]\nExecStart=/new\n"
        );

        // The file is up to date, but the reload and restart never happened.
        fake.on("systemctl --user daemon-reload", Output::ok(""));
        let actions = plan_on(&f, &b, Os::Linux, 0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert!(
            actions[0]
                .describe()
                .contains("reload and restart still pending from an earlier run"),
            "{}",
            actions[0].describe()
        );
        fake.clear_calls();
        f.apply(&actions[..1]);
        assert_eq!(
            fake.calls(),
            [
                "systemctl --user daemon-reload",
                "systemctl --user is-active app.service",
                "systemctl --user restart app.service",
            ]
        );
        let actions = plan_on(&f, &b, Os::Linux, 0).unwrap();
        let statuses: Vec<Status> = actions.iter().map(|a| a.status().unwrap()).collect();
        assert_eq!(statuses, [Status::Satisfied; 3]);
    }

    #[test]
    fn a_changed_plist_reloads_a_loaded_launchd_job_that_is_not_running() {
        let f = Fixture::new(
            "steps:\n  - service: { name: com.x.agent, manager: launchd, unit: agent.plist }\n",
        );
        f.write_repo("agent.plist", "<plist>new</plist>\n");
        let plist = f.home().join("Library/LaunchAgents/com.x.agent.plist");
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        fs::write(&plist, "<plist>old</plist>\n").unwrap();
        let (b, fake) = fake_builtins();
        fake.on(
            "launchctl print-disabled",
            Output::ok("disabled services = {\n}\n"),
        );
        fake.on(
            "launchctl print gui/501/com.x.agent",
            Output::ok("com.x.agent = {\n\tstate = not running\n}\n"),
        );
        let actions = plan_on(&f, &b, Os::Macos, 0).unwrap();
        let statuses: Vec<Status> = actions.iter().map(|a| a.status().unwrap()).collect();
        // Loaded counts as started: an interval job between runs is not kickstarted every run.
        assert_eq!(
            statuses,
            [Status::Pending, Status::Satisfied, Status::Satisfied]
        );
        fake.clear_calls();
        f.apply(&actions[..1]);
        assert_eq!(
            fake.calls(),
            [
                "launchctl print gui/501/com.x.agent".to_string(),
                "launchctl bootout gui/501/com.x.agent".to_string(),
                format!("launchctl bootstrap gui/501 {}", plist.display()),
            ]
        );
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    }

    #[test]
    fn system_units_are_installed_and_managed_through_sudo() {
        let f = Fixture::new(
            "steps:\n  - service: { name: dot-test-unit, manager: systemd, unit: u.service, start: false }\n",
        );
        f.write_repo("u.service", "[Service]\n");
        let (b, fake) = fake_builtins();
        fake.on("systemctl is-enabled", Output::ok("disabled\n"));
        fake.on("systemctl is-active", Output::ok("active\n"));
        let actions = plan_on(&f, &b, Os::Linux, 0).unwrap();
        assert!(actions.iter().all(|a| a.needs_sudo()));
        assert!(
            actions[0]
                .describe()
                .ends_with("(new file, 1 line; as root)"),
            "{}",
            actions[0].describe()
        );
        fake.clear_calls();
        f.apply(&actions);
        let calls = fake.calls();
        assert!(
            calls[0].starts_with("sudo -n install -m 0644 ")
                && calls[0].ends_with(" /etc/systemd/system/dot-test-unit.service"),
            "{calls:?}"
        );
        assert_eq!(
            &calls[1..],
            [
                "sudo -n systemctl daemon-reload",
                "sudo -n systemctl enable dot-test-unit.service",
                "sudo -n systemctl stop dot-test-unit.service",
            ]
        );
    }

    #[test]
    fn launchd_and_windows() {
        let f = Fixture::new(
            "steps:\n  - service: com.example.agent\n  - service: { name: sshd, manager: windows, enable: false }\n",
        );
        let (b, fake) = fake_builtins();
        fake.on(
            "launchctl print gui/501/com.example.agent",
            Output::exit(113, ""),
        );
        let actions = plan_on(&f, &b, Os::Macos, 0).unwrap();
        assert_eq!(actions[0].describe(), "enable com.example.agent (launchd)");
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
        assert_eq!(actions[1].status().unwrap(), Status::Pending);
        fake.clear_calls();
        f.apply(&actions[1..]);
        let plist = f
            .home()
            .join("Library/LaunchAgents/com.example.agent.plist");
        assert_eq!(
            fake.calls(),
            [
                "launchctl print gui/501/com.example.agent".to_string(),
                format!("launchctl bootstrap gui/501 {}", plist.display()),
                "launchctl kickstart gui/501/com.example.agent".to_string(),
            ]
        );

        fake.on(
            "sc.exe qc sshd",
            Output::ok("  START_TYPE : 2 AUTO_START\n"),
        );
        let actions = plan_on(&f, &b, Os::Windows, 1).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        // Service changes need an elevated terminal: the run checks up front.
        assert!(actions.iter().all(|a| a.needs_sudo()));
        fake.clear_calls();
        f.apply(&actions[..1]);
        assert_eq!(fake.calls(), ["sc.exe config sshd start= demand"]);
    }

    #[test]
    fn wrong_system_or_missing_tools() {
        let f =
            Fixture::new("steps:\n  - service: { name: x, manager: launchd }\n  - service: x\n");
        let (b, fake) = fake_builtins();
        let err = format!("{:#}", plan_on(&f, &b, Os::Linux, 0).err().unwrap());
        assert!(
            err.contains("`launchd` needs macos (this is linux)"),
            "{err}"
        );
        assert!(err.contains("bundle.yml:2:5"), "{err}");
        // systemctl not installed (yet): unknown, not an error.
        fake.fail_to_start("systemctl", io::ErrorKind::NotFound);
        let actions = plan_on(&f, &b, Os::Linux, 1).unwrap();
        assert!(
            actions
                .iter()
                .all(|a| a.status().unwrap() == Status::Unknown)
        );
        // And a failing command fails the step with its message.
        fake.on(
            "sudo -n systemctl enable",
            Output {
                code: Some(1),
                stdout: String::new(),
                stderr: "Failed to enable unit: Unit file x.service does not exist.\n".into(),
            },
        );
        let err = f.apply_err(actions[0].as_ref());
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn restart_restarts_a_running_service() {
        let f = Fixture::new("steps:\n  - service: { name: ssh, restart: true }\n");
        let (b, fake) = fake_builtins();
        fake.on("systemctl is-enabled", Output::ok("enabled\n"));
        fake.on("systemctl is-active", Output::ok("active\n"));
        let actions = plan_on(&f, &b, Os::Linux, 0).unwrap();
        let last = actions.last().unwrap();
        assert_eq!(last.describe(), "restart ssh.service (systemd)");
        assert_eq!(last.status().unwrap(), Status::Pending);
        assert!(last.needs_sudo());
        fake.clear_calls();
        assert_eq!(f.apply_one(last.as_ref()).unwrap(), Outcome::Changed);
        assert_eq!(
            fake.calls(),
            [
                "systemctl is-active ssh.service",
                "sudo -n systemctl restart ssh.service"
            ]
        );
        // Not running: nothing to restart.
        fake.on("systemctl is-active", Output::exit(3, "inactive\n"));
        assert_eq!(f.apply_one(last.as_ref()).unwrap(), Outcome::Ok);
    }

    #[test]
    fn systemd_that_is_not_running_is_a_clear_conflict() {
        let f = Fixture::new(
            "steps:\n  - service: ssh\n  - service: { name: s, manager: systemd-user }\n",
        );
        let (b, fake) = fake_builtins();
        let mut ctx = f.context();
        ctx.facts.env = vec!["container".into()];
        ctx.facts.init = Some("other".into());
        for step in 0..2 {
            let actions = f.plan_by(step, &ctx, &b).unwrap();
            assert_eq!(actions.len(), 1);
            assert_eq!(actions[0].status().unwrap(), Status::Conflict);
            let d = actions[0].describe();
            assert!(
                d.contains("systemd is not running here (init: other"),
                "{d}"
            );
            assert!(d.contains("when: { init: systemd }"), "{d}");
            let err = f.apply_err(actions[0].as_ref());
            assert!(err.contains("cannot manage service"), "{err}");
        }
        ctx.facts.env = vec!["wsl".into()];
        let d = f.plan_by(0, &ctx, &b).unwrap()[0].describe();
        assert!(d.contains("/etc/wsl.conf"), "{d}");
        // Nothing was asked of the (missing) systemctl.
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        // Running systemd, or an unknown init: planned as usual.
        ctx.facts.init = Some("systemd".into());
        assert_eq!(f.plan_by(0, &ctx, &b).unwrap().len(), 2);
    }

    #[test]
    fn parse_errors() {
        let f = Fixture::new("steps: []\n");
        for (yaml, want) in [
            (
                "{ name: x, linger: true }",
                "`linger` needs `manager: systemd-user`",
            ),
            (
                "{ name: x, manager: windows, unit: u }",
                "`unit` is not supported",
            ),
            ("{ name: x, manager: upstart }", "unknown variant `upstart`"),
            ("'a b'", "not a valid service name"),
            ("{ name: x, enabled: true }", "unknown field `enabled`"),
            (
                "{ name: x, restart: true, start: false }",
                "`restart: true` needs the service started",
            ),
        ] {
            let file = f.root().join("bad.yml");
            fs::write(&file, format!("steps:\n  - service: {yaml}\n")).unwrap();
            let err = dot_core::config::load_bundle(&file)
                .unwrap_err()
                .to_string();
            assert!(err.contains(want), "{yaml}: {err}");
            assert!(err.contains("bad.yml:2:"), "{err}");
        }
    }
}
