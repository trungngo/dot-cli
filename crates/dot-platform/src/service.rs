//! Service managers: systemd (system and user), launchd (user agents) and the Windows service
//! control manager. Queries are read-only; changes come back as [`Cmd`]s for the caller to run
//! (and report), so tests assert argv through a [`crate::exec::fake::FakeExec`].

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::exec::{Cmd, System};

/// A resolved service manager (no `auto`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerKind {
    Systemd,
    SystemdUser,
    Launchd,
    Windows,
}

impl ManagerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ManagerKind::Systemd => "systemd",
            ManagerKind::SystemdUser => "systemd-user",
            ManagerKind::Launchd => "launchd",
            ManagerKind::Windows => "windows",
        }
    }
}

/// Where system units go unless overridden.
pub const SYSTEMD_SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";

/// A service manager. Queries are read-only; changes come back as [`Cmd`]s for the caller to
/// run. Adding a platform means implementing this (and a detection rule), not touching the
/// `service` directive. Built by [`manager`].
pub trait ServiceManager: Send + Sync + std::fmt::Debug {
    fn kind(&self) -> ManagerKind;

    /// Who we run as, and how commands run.
    fn system(&self) -> &System;

    /// The name the manager knows the service by.
    fn unit_name(&self, name: &str) -> String {
        name.to_string()
    }

    /// Where the service's unit file (plist for launchd) lives; `None` when the manager has no
    /// unit files dot can write (Windows services).
    fn unit_path(&self, name: &str) -> Option<PathBuf> {
        let _ = name;
        None
    }

    /// Whether installing a unit file needs root (system units, when not root).
    fn unit_needs_privilege(&self) -> bool {
        false
    }

    /// Whether enabling, starting, stopping or reloading changes the whole system, and so needs
    /// root (through `sudo`) or, on Windows, an elevated terminal: see
    /// [`ServiceManager::changes_need_sudo`].
    fn changes_are_system_wide(&self) -> bool {
        false
    }

    /// Whether changes need privileges `dot` does not have: the action says
    /// [`dot_core::Action::needs_sudo`], so the run asks for sudo up front (Unix) or fails up
    /// front asking for an elevated terminal (Windows).
    fn changes_need_sudo(&self) -> bool {
        self.changes_are_system_wide() && !self.system().root
    }

    /// Whether the service starts at boot / login. `None`: the manager does not know the
    /// service (not installed yet).
    fn enabled(&self, name: &str) -> io::Result<Option<bool>>;

    /// Whether the service is running now.
    fn active(&self, name: &str) -> io::Result<bool>;

    /// Whether the service counts as started for `start: true`: running (systemd, Windows), or
    /// loaded (launchd: a loaded job runs as its plist says: at load, on an interval, kept
    /// alive; one that is not running right now has not stopped being managed). A changed unit
    /// file is only taken up by a started service through [`ServiceManager::restart`].
    fn started(&self, name: &str) -> io::Result<bool> {
        self.active(name)
    }

    /// Commands that enable (or disable) the service.
    fn set_enabled(&self, name: &str, enable: bool) -> io::Result<Vec<Cmd>>;

    /// Commands that start the service now.
    fn start(&self, name: &str) -> io::Result<Vec<Cmd>>;

    /// Commands that stop the service now.
    fn stop(&self, name: &str) -> io::Result<Vec<Cmd>>;

    /// Commands that make the manager read a changed unit file.
    fn reload(&self) -> Vec<Cmd> {
        Vec::new()
    }

    /// Commands that restart a started service ([`ServiceManager::started`]) so a changed unit
    /// file takes effect.
    fn restart(&self, name: &str) -> io::Result<Vec<Cmd>>;

    /// The command that installs a unit file written to `tmp` as `dest` (mode 0644), for when
    /// that needs root ([`ServiceManager::unit_needs_privilege`]).
    fn install_unit(&self, tmp: &Path, dest: &Path) -> Cmd {
        self.system().privileged(Cmd::new("install").args([
            "-m".to_string(),
            "0644".to_string(),
            tmp.display().to_string(),
            dest.display().to_string(),
        ]))
    }

    /// systemd-user: whether `user`'s services keep running without a login session.
    fn linger(&self, user: &str) -> io::Result<bool> {
        let _ = user;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{} has no lingering", self.kind().as_str()),
        ))
    }

    /// systemd-user: the command that enables lingering (through `sudo`).
    fn enable_linger(&self, user: &str) -> Cmd {
        self.system()
            .privileged(Cmd::new("loginctl").args(["enable-linger", user]))
    }

    /// Whether [`ServiceManager::enable_linger`] goes through `sudo`.
    fn linger_needs_sudo(&self) -> bool {
        !self.system().root && !cfg!(windows)
    }
}

/// The manager of `kind`, running commands through `sys`; per-user units live under `home`.
pub fn manager(kind: ManagerKind, sys: System, home: &Path) -> Arc<dyn ServiceManager> {
    match kind {
        ManagerKind::Systemd => Arc::new(Systemd::system(sys)),
        ManagerKind::SystemdUser => Arc::new(Systemd::user(sys, home)),
        ManagerKind::Launchd => Arc::new(Launchd {
            sys,
            home: home.to_path_buf(),
        }),
        ManagerKind::Windows => Arc::new(WindowsScm { sys }),
    }
}

/// systemd: system units in [`SYSTEMD_SYSTEM_UNIT_DIR`] (changed through `sudo` unless root),
/// or the user's units in `~/.config/systemd/user` (`systemctl --user`).
#[derive(Debug, Clone)]
pub struct Systemd {
    pub sys: System,
    /// `Some(home)`: the user's manager.
    pub user_home: Option<PathBuf>,
    /// Where system units are installed ([`SYSTEMD_SYSTEM_UNIT_DIR`]).
    pub system_unit_dir: PathBuf,
}

impl Systemd {
    pub fn system(sys: System) -> Self {
        Self {
            sys,
            user_home: None,
            system_unit_dir: PathBuf::from(SYSTEMD_SYSTEM_UNIT_DIR),
        }
    }

    pub fn user(sys: System, home: &Path) -> Self {
        Self {
            user_home: Some(home.to_path_buf()),
            ..Self::system(sys)
        }
    }

    fn systemctl(&self) -> Cmd {
        match self.user_home {
            Some(_) => Cmd::new("systemctl").arg("--user"),
            None => Cmd::new("systemctl"),
        }
    }

    /// A state-changing command: system-wide ones go through [`System::privileged`].
    fn change(&self, cmd: Cmd) -> Cmd {
        match self.user_home {
            Some(_) => cmd,
            None => self.sys.privileged(cmd),
        }
    }

    fn verb(&self, verb: &str, name: &str) -> Vec<Cmd> {
        vec![self.change(self.systemctl().args([verb, &self.unit_name(name)]))]
    }
}

impl ServiceManager for Systemd {
    fn kind(&self) -> ManagerKind {
        match self.user_home {
            Some(_) => ManagerKind::SystemdUser,
            None => ManagerKind::Systemd,
        }
    }

    fn system(&self) -> &System {
        &self.sys
    }

    /// Units get `.service` when they have no suffix.
    fn unit_name(&self, name: &str) -> String {
        if name.contains('.') {
            name.to_string()
        } else {
            format!("{name}.service")
        }
    }

    fn unit_path(&self, name: &str) -> Option<PathBuf> {
        let unit = self.unit_name(name);
        Some(match &self.user_home {
            Some(home) => home.join(".config/systemd/user").join(unit),
            None => self.system_unit_dir.join(unit),
        })
    }

    fn unit_needs_privilege(&self) -> bool {
        self.user_home.is_none() && !self.sys.root
    }

    fn changes_are_system_wide(&self) -> bool {
        self.user_home.is_none()
    }

    fn enabled(&self, name: &str) -> io::Result<Option<bool>> {
        let out = self
            .sys
            .run(&self.systemctl().args(["is-enabled", &self.unit_name(name)]))?;
        Ok(parse_systemd_enabled(&out.stdout))
    }

    fn active(&self, name: &str) -> io::Result<bool> {
        let out = self
            .sys
            .run(&self.systemctl().args(["is-active", &self.unit_name(name)]))?;
        let state = out.stdout.lines().next().unwrap_or("").trim().to_string();
        Ok(matches!(state.as_str(), "active" | "reloading"))
    }

    fn set_enabled(&self, name: &str, enable: bool) -> io::Result<Vec<Cmd>> {
        Ok(self.verb(if enable { "enable" } else { "disable" }, name))
    }

    fn start(&self, name: &str) -> io::Result<Vec<Cmd>> {
        Ok(self.verb("start", name))
    }

    fn stop(&self, name: &str) -> io::Result<Vec<Cmd>> {
        Ok(self.verb("stop", name))
    }

    fn reload(&self) -> Vec<Cmd> {
        vec![self.change(self.systemctl().arg("daemon-reload"))]
    }

    fn restart(&self, name: &str) -> io::Result<Vec<Cmd>> {
        Ok(self.verb("restart", name))
    }

    fn linger(&self, user: &str) -> io::Result<bool> {
        let out = self.sys.run(&Cmd::new("loginctl").args([
            "show-user",
            user,
            "--property=Linger",
            "--value",
        ]))?;
        Ok(out.success() && out.stdout.trim() == "yes")
    }
}

/// `systemctl is-enabled` output: `Some(true)` for the states that start the unit, `None` when
/// systemd does not know it.
pub fn parse_systemd_enabled(stdout: &str) -> Option<bool> {
    match stdout.lines().next().unwrap_or("").trim() {
        "enabled" | "enabled-runtime" | "static" | "alias" | "indirect" | "generated" => Some(true),
        "disabled" | "masked" | "masked-runtime" | "linked" | "linked-runtime" => Some(false),
        _ => None,
    }
}

/// launchd: the user's agents in `~/Library/LaunchAgents/<name>.plist`, domain `gui/<uid>`.
#[derive(Debug, Clone)]
pub struct Launchd {
    pub sys: System,
    pub home: PathBuf,
}

impl Launchd {
    fn domain(&self) -> io::Result<String> {
        match self.sys.uid {
            Some(uid) => Ok(format!("gui/{uid}")),
            None => Err(io::Error::other(
                "launchd needs the user id, which is unknown",
            )),
        }
    }

    fn target(&self, name: &str) -> io::Result<String> {
        Ok(format!("{}/{name}", self.domain()?))
    }

    /// Whether the job is loaded in the user's domain.
    fn loaded(&self, name: &str) -> io::Result<bool> {
        let out = self
            .sys
            .run(&Cmd::new("launchctl").args(["print", &self.target(name)?]))?;
        Ok(out.success())
    }

    fn bootstrap(&self, name: &str) -> io::Result<Cmd> {
        let plist = self.unit_path(name).expect("launchd jobs have a plist");
        Ok(Cmd::new("launchctl").args([
            "bootstrap".to_string(),
            self.domain()?,
            plist.display().to_string(),
        ]))
    }
}

impl ServiceManager for Launchd {
    fn kind(&self) -> ManagerKind {
        ManagerKind::Launchd
    }

    fn system(&self) -> &System {
        &self.sys
    }

    fn unit_path(&self, name: &str) -> Option<PathBuf> {
        Some(
            self.home
                .join("Library/LaunchAgents")
                .join(format!("{name}.plist")),
        )
    }

    fn enabled(&self, name: &str) -> io::Result<Option<bool>> {
        let out = self
            .sys
            .run(&Cmd::new("launchctl").args(["print-disabled", &self.domain()?]))?;
        let needle = format!("\"{name}\" =>");
        let line = out.stdout.lines().find(|l| l.trim().starts_with(&needle));
        Ok(Some(match line {
            Some(l) => {
                let v = l.trim()[needle.len()..].trim();
                !(v.starts_with("disabled") || v.starts_with("true"))
            }
            // Not listed: enabled unless its plist says otherwise.
            None => true,
        }))
    }

    fn active(&self, name: &str) -> io::Result<bool> {
        let out = self
            .sys
            .run(&Cmd::new("launchctl").args(["print", &self.target(name)?]))?;
        Ok(out.success() && out.stdout.contains("state = running"))
    }

    fn started(&self, name: &str) -> io::Result<bool> {
        self.loaded(name)
    }

    fn set_enabled(&self, name: &str, enable: bool) -> io::Result<Vec<Cmd>> {
        let verb = if enable { "enable" } else { "disable" };
        Ok(vec![
            Cmd::new("launchctl").args([verb, &self.target(name)?]),
        ])
    }

    /// Loads the job from its plist first when it is not loaded.
    fn start(&self, name: &str) -> io::Result<Vec<Cmd>> {
        let mut cmds = Vec::new();
        if !self.loaded(name)? {
            cmds.push(self.bootstrap(name)?);
        }
        cmds.push(Cmd::new("launchctl").args(["kickstart", &self.target(name)?]));
        Ok(cmds)
    }

    /// Unloads the job until the next login.
    fn stop(&self, name: &str) -> io::Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::new("launchctl").args(["bootout", &self.target(name)?]),
        ])
    }

    /// Unloads and loads the job again, which re-reads the plist.
    fn restart(&self, name: &str) -> io::Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::new("launchctl").args(["bootout", &self.target(name)?]),
            self.bootstrap(name)?,
        ])
    }
}

/// The Windows service control manager, through `sc.exe`. Changes need an elevated terminal
/// (Windows has no sudo): the actions say so, and the run fails up front when it is not
/// elevated, never half-way.
#[derive(Debug, Clone)]
pub struct WindowsScm {
    pub sys: System,
}

impl ServiceManager for WindowsScm {
    fn kind(&self) -> ManagerKind {
        ManagerKind::Windows
    }

    fn system(&self) -> &System {
        &self.sys
    }

    fn changes_are_system_wide(&self) -> bool {
        true
    }

    fn enabled(&self, name: &str) -> io::Result<Option<bool>> {
        let out = self.sys.run(&Cmd::new("sc.exe").args(["qc", name]))?;
        if !out.success() {
            return Ok(None);
        }
        Ok(Some(parse_sc_auto_start(&out.stdout)))
    }

    fn active(&self, name: &str) -> io::Result<bool> {
        let out = self.sys.run(&Cmd::new("sc.exe").args(["query", name]))?;
        Ok(out.success() && parse_sc_running(&out.stdout))
    }

    fn set_enabled(&self, name: &str, enable: bool) -> io::Result<Vec<Cmd>> {
        let start = if enable { "auto" } else { "demand" };
        Ok(vec![
            Cmd::new("sc.exe").args(["config", name, "start=", start]),
        ])
    }

    fn start(&self, name: &str) -> io::Result<Vec<Cmd>> {
        Ok(vec![Cmd::new("sc.exe").args(["start", name])])
    }

    fn stop(&self, name: &str) -> io::Result<Vec<Cmd>> {
        Ok(vec![Cmd::new("sc.exe").args(["stop", name])])
    }

    fn restart(&self, name: &str) -> io::Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::new("sc.exe").args(["stop", name]),
            Cmd::new("sc.exe").args(["start", name]),
        ])
    }
}

/// `sc.exe qc` output: whether `START_TYPE` is automatic (`AUTO_START`, also delayed).
pub fn parse_sc_auto_start(stdout: &str) -> bool {
    stdout
        .lines()
        .find(|l| l.contains("START_TYPE"))
        .is_some_and(|l| l.contains("AUTO_START"))
}

/// `sc.exe query` output: whether `STATE` is `RUNNING`.
pub fn parse_sc_running(stdout: &str) -> bool {
    stdout
        .lines()
        .any(|l| l.contains("STATE") && l.contains("RUNNING"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Output;
    use crate::exec::fake::FakeExec;

    fn manager(kind: ManagerKind) -> (Arc<dyn ServiceManager>, Arc<FakeExec>) {
        let fake = FakeExec::new();
        let m = super::manager(kind, System::fake(fake.clone()), Path::new("/h"));
        (m, fake)
    }

    fn lines(cmds: &[Cmd]) -> Vec<String> {
        cmds.iter().map(Cmd::line).collect()
    }

    #[test]
    fn systemd_queries_and_commands() {
        let (m, fake) = manager(ManagerKind::Systemd);
        assert_eq!(
            m.unit_path("syncthing"),
            Some(PathBuf::from("/etc/systemd/system/syncthing.service"))
        );
        assert_eq!(m.unit_name("x.timer"), "x.timer");
        fake.on("systemctl is-enabled", Output::exit(1, "disabled\n"));
        fake.on("systemctl is-active", Output::exit(3, "inactive\n"));
        assert_eq!(m.enabled("x").unwrap(), Some(false));
        assert!(!m.active("x").unwrap());
        fake.on("systemctl is-enabled", Output::exit(1, "not-found\n"));
        assert_eq!(m.enabled("x").unwrap(), None);
        fake.on("systemctl is-enabled", Output::ok("static\n"));
        assert_eq!(m.enabled("x").unwrap(), Some(true));
        assert_eq!(
            fake.calls()[0],
            "systemctl is-enabled x.service",
            "queries never need sudo"
        );
        if cfg!(unix) {
            assert_eq!(
                lines(&m.set_enabled("x", true).unwrap()),
                ["sudo -n systemctl enable x.service"]
            );
            assert_eq!(lines(&m.reload()), ["sudo -n systemctl daemon-reload"]);
            assert!(m.unit_needs_privilege());
        }
    }

    #[test]
    fn systemd_user_needs_no_sudo_except_for_linger() {
        let (m, fake) = manager(ManagerKind::SystemdUser);
        assert_eq!(
            m.unit_path("x"),
            Some(PathBuf::from("/h/.config/systemd/user/x.service"))
        );
        assert!(!m.unit_needs_privilege());
        assert_eq!(
            lines(&m.start("x").unwrap()),
            ["systemctl --user start x.service"]
        );
        assert_eq!(
            lines(&m.restart("x").unwrap()),
            ["systemctl --user restart x.service"]
        );
        fake.on("loginctl show-user user", Output::ok("no\n"));
        assert!(!m.linger("user").unwrap());
        fake.on("loginctl show-user user", Output::ok("yes\n"));
        assert!(m.linger("user").unwrap());
        if cfg!(unix) {
            assert_eq!(
                m.enable_linger("user").line(),
                "sudo -n loginctl enable-linger user"
            );
        }
    }

    #[test]
    fn launchd_bootstraps_unloaded_jobs() {
        let (m, fake) = manager(ManagerKind::Launchd);
        let plist = PathBuf::from("/h/Library/LaunchAgents/com.x.agent.plist");
        assert_eq!(m.unit_path("com.x.agent"), Some(plist.clone()));
        // launchd is macOS's, but its commands are planned the same on any host: the plist
        // path is joined with the host's separator (`/h\Library/LaunchAgents\...` on Windows),
        // and the command line quotes it as it would any such argument.
        let bootstrap = if cfg!(windows) {
            r"launchctl bootstrap gui/501 '/h\Library/LaunchAgents\com.x.agent.plist'"
        } else {
            "launchctl bootstrap gui/501 /h/Library/LaunchAgents/com.x.agent.plist"
        }
        .to_string();
        fake.on("launchctl print gui/501/com.x.agent", Output::exit(113, ""));
        assert!(!m.active("com.x.agent").unwrap());
        assert_eq!(
            lines(&m.start("com.x.agent").unwrap()),
            [
                bootstrap.clone(),
                "launchctl kickstart gui/501/com.x.agent".to_string()
            ]
        );
        fake.on(
            "launchctl print gui/501/com.x.agent",
            Output::ok("com.x.agent = {\n\tstate = running\n}\n"),
        );
        assert!(m.active("com.x.agent").unwrap());
        assert_eq!(
            lines(&m.start("com.x.agent").unwrap()),
            ["launchctl kickstart gui/501/com.x.agent"]
        );
        // Loaded but not running (an interval job between runs): started, not active.
        fake.on(
            "launchctl print gui/501/com.x.agent",
            Output::ok("com.x.agent = {\n\tstate = not running\n}\n"),
        );
        assert!(!m.active("com.x.agent").unwrap());
        assert!(m.started("com.x.agent").unwrap());
        assert_eq!(
            lines(&m.restart("com.x.agent").unwrap()),
            [
                "launchctl bootout gui/501/com.x.agent".to_string(),
                bootstrap.clone(),
            ]
        );
        fake.on(
            "launchctl print-disabled",
            Output::ok("disabled services = {\n\t\"com.x.agent\" => disabled\n}\n"),
        );
        assert_eq!(m.enabled("com.x.agent").unwrap(), Some(false));
        fake.on(
            "launchctl print-disabled",
            Output::ok("disabled services = {\n\t\"com.x.agent\" => enabled\n}\n"),
        );
        assert_eq!(m.enabled("com.x.agent").unwrap(), Some(true));
        assert_eq!(
            lines(&m.set_enabled("com.x.agent", false).unwrap()),
            ["launchctl disable gui/501/com.x.agent"]
        );
    }

    #[test]
    fn windows_parses_sc_output() {
        let (m, fake) = manager(ManagerKind::Windows);
        assert_eq!(m.unit_path("sshd"), None);
        fake.on(
            "sc.exe qc sshd",
            Output::ok("        START_TYPE         : 2   AUTO_START\n"),
        );
        fake.on(
            "sc.exe query sshd",
            Output::ok("        STATE              : 1  STOPPED\n"),
        );
        assert_eq!(m.enabled("sshd").unwrap(), Some(true));
        assert!(!m.active("sshd").unwrap());
        fake.on("sc.exe qc sshd", Output::exit(1060, "does not exist"));
        assert_eq!(m.enabled("sshd").unwrap(), None);
        let set: Vec<String> = lines(&m.set_enabled("sshd", true).unwrap());
        assert_eq!(
            set,
            ["sc.exe config sshd start= auto"],
            "never through sudo"
        );
        assert_eq!(
            lines(&m.restart("sshd").unwrap()),
            ["sc.exe stop sshd", "sc.exe start sshd"]
        );
    }

    #[test]
    fn system_wide_changes_need_privileges_dot_lacks() {
        for (kind, system_wide) in [
            (ManagerKind::Systemd, true),
            (ManagerKind::Windows, true),
            (ManagerKind::SystemdUser, false),
            (ManagerKind::Launchd, false),
        ] {
            let fake = FakeExec::new();
            let mut sys = System::fake(fake.clone());
            let m = super::manager(kind, sys.clone(), Path::new("/h"));
            assert_eq!(m.kind(), kind);
            assert_eq!(m.changes_need_sudo(), system_wide, "{kind:?}");
            // Root (Unix) / an elevated terminal (Windows): nothing to ask for.
            sys.root = true;
            let m = super::manager(kind, sys, Path::new("/h"));
            assert!(!m.changes_need_sudo(), "{kind:?}");
        }
        let (m, _) = manager(ManagerKind::Launchd);
        assert!(m.linger("user").is_err(), "only systemd-user lingers");
    }

    #[test]
    fn output_parsers() {
        assert_eq!(parse_systemd_enabled("enabled\n"), Some(true));
        assert_eq!(parse_systemd_enabled("masked\n"), Some(false));
        assert_eq!(parse_systemd_enabled(""), None);
        let qc = "SERVICE_NAME: sshd\r\n        TYPE               : 10  WIN32_OWN_PROCESS\r\n        START_TYPE         : 2   AUTO_START  (DELAYED)\r\n";
        assert!(parse_sc_auto_start(qc));
        assert!(!parse_sc_auto_start(
            "        START_TYPE         : 3   DEMAND_START\r\n"
        ));
        assert!(parse_sc_running(
            "        STATE              : 4  RUNNING\r\n"
        ));
        assert!(!parse_sc_running(
            "        STATE              : 1  STOPPED\r\n"
        ));
    }
}
