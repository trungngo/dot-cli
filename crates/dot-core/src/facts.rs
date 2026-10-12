//! Facts detected about the machine at startup; every `when:` condition matches against these.
//!
//! Detection goes through a [`Probe`] (files, environment variables, commands on `PATH`), so tests
//! feed fixture `/etc/os-release` files and environments instead of depending on the host.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Macos,
    Windows,
}

impl Os {
    pub const ALL: [&'static str; 3] = ["linux", "macos", "windows"];

    pub fn as_str(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Macos => "macos",
            Os::Windows => "windows",
        }
    }

    /// The OS this binary was built for.
    pub fn host() -> Os {
        if cfg!(target_os = "macos") {
            Os::Macos
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Desktop,
    Server,
}

impl Role {
    pub const ALL: [&'static str; 2] = ["desktop", "server"];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Desktop => "desktop",
            Role::Server => "server",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s.trim().to_ascii_lowercase().as_str() {
            "desktop" => Some(Role::Desktop),
            "server" => Some(Role::Server),
            _ => None,
        }
    }
}

/// The `arch` values `DOT_FACTS` may simulate (Rust's names, as the fact reports them).
pub const ARCHES: [&str; 8] = [
    "x86_64",
    "aarch64",
    "arm",
    "x86",
    "riscv64",
    "powerpc64",
    "s390x",
    "loongarch64",
];

/// Values of the `env` fact. Several can hold at once (an SSH session into a WSL distro).
pub const ENVS: [&str; 4] = ["wsl", "container", "vm", "ssh"];

/// The `when: { env: none }` value: none of [`ENVS`] applies (a physical machine, used locally).
pub const ENV_NONE: &str = "none";

/// Values of the `init` fact: the service manager running as PID 1. `systemd` only when it is
/// really running (`/run/systemd/system` exists): containers and WSL without `[boot]
/// systemd=true` have `systemctl` but no systemd, and are `other`.
pub const INITS: [&str; 3] = ["systemd", "launchd", "other"];

/// Where systemd says it is running (see `sd_booted(3)`).
pub const SYSTEMD_RUNNING: &str = "/run/systemd/system";

/// Every package manager the `pkg` fact can name. Which one wins depends on the OS and distro:
/// see [`pkg_priority`].
pub const PKGS: [&str; 8] = [
    "brew", "apt", "dnf", "pacman", "zypper", "apk", "winget", "scoop",
];

/// The order in which package managers are tried for the `pkg` fact: the first one on `PATH`
/// wins. On Linux the distro family's own manager comes first and Homebrew (linuxbrew) last, so
/// an Ubuntu machine with `brew` installed is still `pkg: apt`.
pub fn pkg_priority(os: Os, family: Option<&str>) -> Vec<&'static str> {
    match os {
        Os::Macos => vec!["brew"],
        Os::Windows => vec!["winget", "scoop"],
        Os::Linux => {
            let native = match family {
                Some("debian") => Some("apt"),
                Some("rhel") => Some("dnf"),
                Some("arch") => Some("pacman"),
                Some("suse") => Some("zypper"),
                Some("alpine") => Some("apk"),
                _ => None,
            };
            let mut out: Vec<&str> = native.into_iter().collect();
            for p in ["apt", "dnf", "pacman", "zypper", "apk", "brew"] {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
            out
        }
    }
}

/// The fact names a `when:` condition may test. Adding a variant here is all it takes for the
/// config parser and JSON Schema to accept it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Fact {
    Os,
    Arch,
    Distro,
    Family,
    Version,
    Codename,
    Role,
    Env,
    Pkg,
    Init,
}

impl Fact {
    pub const ALL: [Fact; 10] = [
        Fact::Os,
        Fact::Arch,
        Fact::Distro,
        Fact::Family,
        Fact::Version,
        Fact::Codename,
        Fact::Role,
        Fact::Env,
        Fact::Pkg,
        Fact::Init,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Fact::Os => "os",
            Fact::Arch => "arch",
            Fact::Distro => "distro",
            Fact::Family => "family",
            Fact::Version => "version",
            Fact::Codename => "codename",
            Fact::Role => "role",
            Fact::Env => "env",
            Fact::Pkg => "pkg",
            Fact::Init => "init",
        }
    }

    /// The closed set of values this fact can take, when there is one. A `when:` naming any other
    /// value is a config error rather than a condition that silently never matches.
    pub fn known_values(self) -> Option<&'static [&'static str]> {
        match self {
            Fact::Os => Some(&Os::ALL),
            Fact::Role => Some(&Role::ALL),
            Fact::Env => Some(&ENVS),
            Fact::Pkg => Some(&PKGS),
            Fact::Init => Some(&INITS),
            Fact::Arch | Fact::Distro | Fact::Family | Fact::Version | Fact::Codename => None,
        }
    }

    /// Checks a literal (untemplated) `when:` value for this fact. A leading `!` negates it
    /// (`env: "!container"`); `env: none` means no special environment.
    pub fn check_value(self, value: &str) -> Result<(), String> {
        let value = value.strip_prefix('!').unwrap_or(value);
        if self == Fact::Env && value == ENV_NONE {
            return Ok(());
        }
        if let Some(known) = self.known_values()
            && !known.contains(&value)
        {
            return Err(format!(
                "unknown value `{value}` for fact `{self}`, expected one of: {}",
                known.join(", ")
            ));
        }
        if self == Fact::Version {
            VersionReq::parse(value)?;
        }
        Ok(())
    }
}

impl fmt::Display for Fact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub os: Os,
    pub arch: &'static str,
    /// `/etc/os-release` `ID`, e.g. `ubuntu`. `None` off Linux.
    pub distro: Option<String>,
    /// Distro family from `ID_LIKE` / `ID`: `debian`, `rhel`, `arch`, `alpine`, `suse`, ...
    pub family: Option<String>,
    /// `VERSION_ID` on Linux, the product version on macOS and Windows.
    pub version: Option<String>,
    /// The release's code name on Linux (`VERSION_CODENAME`, else `UBUNTU_CODENAME`), e.g.
    /// `noble`: the apt suite of vendor repositories.
    pub codename: Option<String>,
    pub role: Role,
    /// Any of [`ENVS`], sorted.
    pub env: Vec<String>,
    /// The first manager of [`pkg_priority`] found on `PATH` (in WSL, never a Windows program
    /// reached through interop).
    pub pkg: Option<String>,
    /// WSL: the Windows user's profile directory as a Linux path (`/mnt/c/Users/me`), for the
    /// `windows_home` variable. `None` elsewhere, or when interop is off.
    pub windows_home: Option<String>,
    /// The running init / service manager, one of [`INITS`]; `None` when not detected (facts
    /// built by hand).
    pub init: Option<String>,
}

impl Facts {
    /// Facts with only `os`, `arch` and `role` known; for tests and callers that fill the rest.
    pub fn new(os: Os, arch: &'static str, role: Role) -> Self {
        Self {
            os,
            arch,
            distro: None,
            family: None,
            version: None,
            codename: None,
            role,
            env: Vec::new(),
            pkg: None,
            windows_home: None,
            init: None,
        }
    }

    /// Detects everything about the host. `DOT_ROLE` in the process environment overrides the
    /// detected role; callers with a local config pass their override to [`Facts::detect_with`].
    pub fn detect() -> Self {
        let role = std::env::var("DOT_ROLE").ok().and_then(|r| Role::parse(&r));
        Self::detect_with(&HostProbe, role)
    }

    /// Detection through `probe`; `role` (from local config) wins over the detected role.
    pub fn detect_with(probe: &dyn Probe, role: Option<Role>) -> Self {
        let os = probe.os();
        let mut facts = Facts::new(os, probe.arch(), Role::Desktop);
        match os {
            Os::Linux => {
                let release = probe
                    .read_file("/etc/os-release")
                    .or_else(|| probe.read_file("/usr/lib/os-release"))
                    .map(|t| parse_os_release(&t))
                    .unwrap_or_default();
                let id = release.get("ID").map(|s| s.to_ascii_lowercase());
                let like: Vec<String> = release
                    .get("ID_LIKE")
                    .map(|s| {
                        s.split_whitespace()
                            .map(|w| w.to_ascii_lowercase())
                            .collect()
                    })
                    .unwrap_or_default();
                facts.family = id.as_deref().and_then(|id| family(id, &like));
                facts.distro = id;
                facts.version = release.get("VERSION_ID").cloned();
                facts.codename = ["VERSION_CODENAME", "UBUNTU_CODENAME"]
                    .iter()
                    .filter_map(|k| release.get(*k))
                    .map(|v| v.trim().to_ascii_lowercase())
                    .find(|v| !v.is_empty());
                facts.role = role.unwrap_or_else(|| linux_role(probe));
            }
            Os::Macos | Os::Windows => {
                facts.version = probe.os_version();
                facts.role = role.unwrap_or(Role::Desktop);
            }
        }
        facts.env = detect_env(os, probe);
        let wsl = facts.env.iter().any(|e| e == "wsl");
        // In WSL, Windows' own PATH is appended (interop): a Windows program found there must
        // not make this Linux machine's `pkg` a Windows manager.
        facts.pkg = pkg_priority(os, facts.family.as_deref())
            .into_iter()
            .find(|p| probe.on_path(p) && !(wsl && probe.is_windows_program(p)))
            .map(str::to_string);
        if wsl {
            facts.windows_home = windows_home(probe);
        }
        facts.init = Some(
            match os {
                Os::Linux if probe.exists(SYSTEMD_RUNNING) => "systemd",
                Os::Macos => "launchd",
                _ => "other",
            }
            .to_string(),
        );
        facts
    }

    /// The value(s) of one fact, empty when it is unknown on this machine (e.g. `distro` on
    /// macOS). Only `env` can have more than one.
    pub fn values(&self, fact: Fact) -> Vec<&str> {
        match fact {
            Fact::Os => vec![self.os.as_str()],
            Fact::Arch => vec![self.arch],
            Fact::Distro => self.distro.as_deref().into_iter().collect(),
            Fact::Family => self.family.as_deref().into_iter().collect(),
            Fact::Version => self.version.as_deref().into_iter().collect(),
            Fact::Codename => self.codename.as_deref().into_iter().collect(),
            Fact::Role => vec![self.role.as_str()],
            Fact::Env => self.env.iter().map(String::as_str).collect(),
            Fact::Pkg => self.pkg.as_deref().into_iter().collect(),
            Fact::Init => self.init.as_deref().into_iter().collect(),
        }
    }

    /// The value of a single-valued fact (`None` for `env`).
    pub fn get(&self, fact: Fact) -> Option<&str> {
        match fact {
            Fact::Env => None,
            f => self.values(f).into_iter().next(),
        }
    }

    /// Whether one `when:` value holds. `version` values may be comparisons (`>=22.04`).
    pub fn test(&self, fact: Fact, wanted: &str) -> bool {
        match fact {
            Fact::Env if wanted == ENV_NONE => self.env.is_empty(),
            Fact::Version => match (&self.version, VersionReq::parse(wanted)) {
                (Some(v), Ok(req)) => req.matches(v),
                _ => false,
            },
            f => self.values(f).contains(&wanted),
        }
    }

    /// The fact's value for messages: `ubuntu`, `unknown`, or for `env` `wsl, ssh` / `none`.
    pub fn describe(&self, fact: Fact) -> String {
        let v = self.values(fact);
        match (fact, v.is_empty()) {
            (Fact::Env, true) => "none".into(),
            (_, true) => "unknown".into(),
            _ => v.join(", "),
        }
    }

    /// `(name, value)` pairs of the facts that are known, for display.
    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        Fact::ALL
            .iter()
            .filter_map(|f| {
                let v = self.values(*f);
                (!v.is_empty()).then(|| (f.as_str(), v.join(",")))
            })
            .collect()
    }
}

impl Facts {
    /// Replaces detected facts with `overrides`: comma-separated `fact=value` pairs, e.g.
    /// `os=linux,family=debian,distro=ubuntu,codename=noble,pkg=apt` (`env` takes `+`-separated
    /// values, an empty value clears a fact). `dot` reads them from `DOT_FACTS`: for trying a
    /// configuration for another platform with `dot plan`, and for end-to-end tests against fake
    /// tools. `arch` takes one of [`ARCHES`]: it picks release assets and `when:` branches, so
    /// a simulated machine of another CPU plans (and, against fakes, applies) as that one would.
    ///
    /// Simulating another machine (an `os`, or a non-empty `distro`, other than the detected
    /// one) first drops what was detected about this one (`family`, `distro`, `version`,
    /// `codename`, `env`, `pkg`, `init`), then derives what the overrides leave out: `family`
    /// from `distro`, `pkg` and `init` from `os` and `family` (`os=linux,distro=ubuntu` is an
    /// apt, systemd machine; `version` and `codename` stay unknown unless given).
    pub fn apply_overrides(&mut self, overrides: &str) -> Result<(), String> {
        let mut pairs = Vec::new();
        for pair in overrides
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            let Some((k, v)) = pair.split_once('=') else {
                return Err(format!("`{pair}` is not fact=value"));
            };
            let (k, v) = (k.trim(), v.trim());
            let fact = Fact::ALL
                .into_iter()
                .find(|f| f.as_str() == k)
                .ok_or_else(|| format!("unknown fact `{k}`"))?;
            pairs.push((fact, v));
        }
        let given = |f: Fact| pairs.iter().any(|(g, _)| *g == f);
        let other_machine = pairs.iter().any(|(f, v)| match f {
            Fact::Os => *v != self.os.as_str(),
            Fact::Distro => !v.is_empty() && Some(*v) != self.distro.as_deref(),
            _ => false,
        });
        if other_machine {
            self.family = None;
            self.distro = None;
            self.version = None;
            self.codename = None;
            self.env.clear();
            self.pkg = None;
            self.init = None;
            self.windows_home = None;
        }
        for &(fact, v) in &pairs {
            let opt = || (!v.is_empty()).then(|| v.to_string());
            match fact {
                Fact::Os => {
                    self.os = match v {
                        "linux" => Os::Linux,
                        "macos" => Os::Macos,
                        "windows" => Os::Windows,
                        _ => return Err(format!("unknown value `{v}` for fact `os`")),
                    }
                }
                Fact::Arch => {
                    self.arch = ARCHES.iter().copied().find(|a| *a == v).ok_or_else(|| {
                        format!(
                            "unknown value `{v}` for fact `arch`, expected one of: {}",
                            ARCHES.join(", ")
                        )
                    })?
                }
                Fact::Distro => self.distro = opt(),
                Fact::Family => self.family = opt(),
                Fact::Version => self.version = opt(),
                Fact::Codename => self.codename = opt(),
                Fact::Role => {
                    self.role = Role::parse(v)
                        .ok_or_else(|| format!("unknown value `{v}` for fact `role`"))?
                }
                Fact::Env => {
                    let mut env: Vec<String> = v
                        .split('+')
                        .filter(|e| !e.is_empty() && *e != ENV_NONE)
                        .map(str::to_string)
                        .collect();
                    for e in &env {
                        if e.starts_with('!') || !ENVS.contains(&e.as_str()) {
                            return Err(format!(
                                "unknown value `{e}` for fact `env`, expected one of: {}",
                                ENVS.join(", ")
                            ));
                        }
                    }
                    env.sort();
                    self.env = env;
                }
                Fact::Pkg => {
                    if !v.is_empty() {
                        fact.check_value(v)?;
                    }
                    self.pkg = opt();
                }
                Fact::Init => {
                    if !v.is_empty() {
                        fact.check_value(v)?;
                    }
                    self.init = opt();
                }
            }
        }
        if other_machine {
            if !given(Fact::Family) {
                self.family = self.distro.as_deref().and_then(|d| family(d, &[]));
            }
            if !given(Fact::Pkg) {
                self.pkg = match (self.os, self.family.as_deref()) {
                    (Os::Macos, _) => Some("brew"),
                    (Os::Windows, _) => Some("winget"),
                    (Os::Linux, Some("debian")) => Some("apt"),
                    (Os::Linux, Some("rhel")) => Some("dnf"),
                    (Os::Linux, Some("arch")) => Some("pacman"),
                    (Os::Linux, Some("suse")) => Some("zypper"),
                    (Os::Linux, Some("alpine")) => Some("apk"),
                    (Os::Linux, _) => None,
                }
                .map(str::to_string);
            }
            if !given(Fact::Init) {
                let no_systemd = self.env.iter().any(|e| e == "container" || e == "wsl");
                self.init = Some(
                    match self.os {
                        Os::Linux if !no_systemd => "systemd",
                        Os::Macos => "launchd",
                        _ => "other",
                    }
                    .to_string(),
                );
            }
        }
        Ok(())
    }
}

/// Where detection reads the machine from.
pub trait Probe {
    fn os(&self) -> Os {
        Os::host()
    }
    fn arch(&self) -> &'static str {
        std::env::consts::ARCH
    }
    fn read_file(&self, path: &str) -> Option<String>;
    fn exists(&self, path: &str) -> bool;
    fn env(&self, name: &str) -> Option<String>;
    /// Trimmed stdout of `program args...` when it ran and exited 0.
    fn run(&self, program: &str, args: &[&str]) -> Option<String>;
    /// Whether `program` is an executable on `PATH`.
    fn on_path(&self, program: &str) -> bool;
    /// The OS product version on macOS and Windows.
    fn os_version(&self) -> Option<String>;
    /// WSL: whether `program` on `PATH` is a Windows program reached through interop (on a
    /// `/mnt/<drive>` mount).
    fn is_windows_program(&self, program: &str) -> bool {
        let _ = program;
        false
    }
}

/// WSL: the Windows user's profile directory as a Linux path. `cmd.exe /c echo %USERPROFILE%`
/// through interop, then `wslpath -u` (or, without it, the default `/mnt/<drive>` mapping).
fn windows_home(probe: &dyn Probe) -> Option<String> {
    let win = probe.run("cmd.exe", &["/c", "echo %USERPROFILE%"])?;
    let win = win.lines().next()?.trim().to_string();
    if win.is_empty() || win.contains('%') {
        return None;
    }
    probe
        .run("wslpath", &["-u", &win])
        .filter(|p| p.starts_with('/'))
        .or_else(|| crate::folders::wsl_path(&win))
}

/// The real machine.
pub struct HostProbe;

impl Probe for HostProbe {
    fn read_file(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }

    fn run(&self, program: &str, args: &[&str]) -> Option<String> {
        if !self.on_path(program) {
            return None;
        }
        let out = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn on_path(&self, program: &str) -> bool {
        self.find(program).is_some()
    }

    fn is_windows_program(&self, program: &str) -> bool {
        self.find(program)
            .is_some_and(|p| crate::folders::is_windows_mount(&p.to_string_lossy()))
    }

    fn os_version(&self) -> Option<String> {
        if cfg!(target_os = "macos")
            && let Some(v) = self.run("sw_vers", &["-productVersion"])
            && !v.is_empty()
        {
            return Some(v);
        }
        match os_info::get().version() {
            os_info::Version::Unknown => None,
            v => Some(v.to_string()),
        }
    }
}

impl HostProbe {
    /// Where `program` is on `PATH`.
    fn find(&self, program: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        let pathext = std::env::var("PATHEXT").ok();
        let names = crate::paths::exe_candidates(program, pathext.as_deref(), cfg!(windows));
        std::env::split_paths(&path).find_map(|dir| {
            names.iter().find_map(|name| {
                let p = dir.join(name);
                is_executable(&p).then_some(p)
            })
        })
    }
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &std::path::Path) -> bool {
    p.is_file()
}

/// `KEY=value` lines of an os-release file, with shell-style quotes removed.
pub fn parse_os_release(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_string(), v.replace("\\\"", "\"")))
        })
        .collect()
}

/// The family of a distro: the first of `ID_LIKE`, then `ID`, that names a known family.
fn family(id: &str, like: &[String]) -> Option<String> {
    fn canonical(s: &str) -> Option<&'static str> {
        Some(match s {
            "debian" | "ubuntu" => "debian",
            "rhel" | "centos" | "fedora" => "rhel",
            "arch" | "archlinux" => "arch",
            "alpine" => "alpine",
            "suse" | "opensuse" | "sles" => "suse",
            "gentoo" => "gentoo",
            "nixos" => "nixos",
            _ => return None,
        })
    }
    like.iter()
        .map(String::as_str)
        .chain(std::iter::once(id))
        .find_map(canonical)
        .map(str::to_string)
        .or_else(|| Some(like.first().map(String::as_str).unwrap_or(id).to_string()))
}

/// Desktop when a local graphical session is running, or when the machine boots to a graphical
/// target *and* has a display manager. `graphical.target` alone is not enough: stock Debian and
/// Ubuntu Server images default to it too. A `DISPLAY` forwarded over `ssh -X`
/// (`localhost:10.0`, or any `DISPLAY` inside an SSH session) says nothing about the machine.
fn linux_role(probe: &dyn Probe) -> Role {
    let session = probe
        .env("XDG_SESSION_TYPE")
        .is_some_and(|t| t == "x11" || t == "wayland");
    let over_ssh = probe.env("SSH_CONNECTION").is_some() || probe.env("SSH_TTY").is_some();
    let local_display = probe
        .env("DISPLAY")
        .is_some_and(|d| !over_ssh && !is_remote_display(&d));
    if session || local_display || probe.env("WAYLAND_DISPLAY").is_some() {
        return Role::Desktop;
    }
    let graphical = probe
        .run("systemctl", &["get-default"])
        .is_some_and(|t| t.trim() == "graphical.target");
    let display_manager = probe
        .run("systemctl", &["is-enabled", "display-manager.service"])
        .is_some()
        || probe.exists("/etc/systemd/system/display-manager.service");
    if graphical && display_manager {
        Role::Desktop
    } else {
        Role::Server
    }
}

/// `host:display` with a host part (`localhost:10.0`, `10.0.0.5:0`) is a network display, as
/// SSH X11 forwarding sets; `:0` and `unix:0` are local.
fn is_remote_display(display: &str) -> bool {
    match display.rsplit_once(':') {
        Some((host, _)) => !host.is_empty() && host != "unix" && !host.starts_with('/'),
        None => false,
    }
}

fn detect_env(os: Os, probe: &dyn Probe) -> Vec<String> {
    let mut env = Vec::new();
    if os == Os::Linux {
        let wsl = probe.env("WSL_DISTRO_NAME").is_some()
            || probe
                .read_file("/proc/version")
                .is_some_and(|v| v.to_ascii_lowercase().contains("microsoft"));
        if wsl {
            env.push("wsl");
        }
        if probe.exists("/.dockerenv")
            || probe.exists("/run/.containerenv")
            || probe.env("container").is_some()
        {
            env.push("container");
        }
        if let Some(v) = probe.run("systemd-detect-virt", &["--vm"])
            && !matches!(v.as_str(), "" | "none" | "wsl")
        {
            env.push("vm");
        }
    }
    if os == Os::Macos
        && probe
            .run("sysctl", &["-n", "kern.hv_vmm_present"])
            .as_deref()
            == Some("1")
    {
        env.push("vm");
    }
    if probe.env("SSH_CONNECTION").is_some() || probe.env("SSH_TTY").is_some() {
        env.push("ssh");
    }
    let mut env: Vec<String> = env.into_iter().map(str::to_string).collect();
    env.sort();
    env
}

/// A `version:` condition: a bare version (exact match) or comma-separated comparisons that must
/// all hold, e.g. `>=22.04, <25`. Versions compare numerically component by component; missing
/// components count as 0 (`24` == `24.0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionReq(Vec<(Cmp, String)>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Exact,
    Eq,
    Ne,
    Ge,
    Gt,
    Le,
    Lt,
}

impl VersionReq {
    pub fn parse(s: &str) -> Result<VersionReq, String> {
        let mut out = Vec::new();
        for part in s.split(',') {
            let part = part.trim();
            let (cmp, rest) = [
                (">=", Cmp::Ge),
                ("<=", Cmp::Le),
                ("==", Cmp::Eq),
                ("!=", Cmp::Ne),
                (">", Cmp::Gt),
                ("<", Cmp::Lt),
                ("=", Cmp::Eq),
            ]
            .iter()
            .find_map(|(p, c)| part.strip_prefix(p).map(|r| (*c, r.trim())))
            .unwrap_or((Cmp::Exact, part));
            if rest.is_empty() {
                return Err(format!("invalid version condition `{s}`: missing version"));
            }
            if cmp != Cmp::Exact && numeric(rest).is_none() {
                return Err(format!(
                    "invalid version condition `{s}`: `{rest}` is not a numeric version like 22.04"
                ));
            }
            if cmp == Cmp::Exact && s.contains(',') {
                return Err(format!(
                    "invalid version condition `{s}`: each part needs an operator (>=, >, <=, <, ==, !=)"
                ));
            }
            out.push((cmp, rest.to_string()));
        }
        Ok(VersionReq(out))
    }

    pub fn matches(&self, version: &str) -> bool {
        self.0.iter().all(|(cmp, want)| {
            if *cmp == Cmp::Exact {
                return version == want;
            }
            let (Some(have), Some(want)) = (numeric(version), numeric(want)) else {
                return false;
            };
            let ord = compare(&have, &want);
            match cmp {
                Cmp::Exact | Cmp::Eq => ord == Ordering::Equal,
                Cmp::Ne => ord != Ordering::Equal,
                Cmp::Ge => ord != Ordering::Less,
                Cmp::Gt => ord == Ordering::Greater,
                Cmp::Le => ord != Ordering::Greater,
                Cmp::Lt => ord == Ordering::Less,
            }
        })
    }
}

fn numeric(v: &str) -> Option<Vec<u64>> {
    v.split('.').map(|p| p.trim().parse().ok()).collect()
}

fn compare(a: &[u64], b: &[u64]) -> Ordering {
    let n = a.len().max(b.len());
    (0..n)
        .map(|i| {
            a.get(i)
                .copied()
                .unwrap_or(0)
                .cmp(&b.get(i).copied().unwrap_or(0))
        })
        .find(|o| *o != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    /// A scripted machine.
    #[derive(Default)]
    pub(crate) struct FakeProbe {
        pub os: Option<Os>,
        pub files: BTreeMap<String, String>,
        pub env: BTreeMap<String, String>,
        pub commands: BTreeMap<String, String>,
        pub path: BTreeSet<String>,
        /// Programs on `path` that live on a Windows drive (WSL interop).
        pub windows_path: BTreeSet<String>,
        pub os_version: Option<String>,
    }

    impl Probe for FakeProbe {
        fn os(&self) -> Os {
            self.os.unwrap_or(Os::Linux)
        }
        fn arch(&self) -> &'static str {
            "x86_64"
        }
        fn read_file(&self, path: &str) -> Option<String> {
            self.files.get(path).cloned()
        }
        fn exists(&self, path: &str) -> bool {
            self.files.contains_key(path)
        }
        fn env(&self, name: &str) -> Option<String> {
            self.env.get(name).cloned()
        }
        fn run(&self, program: &str, args: &[&str]) -> Option<String> {
            let key = std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.commands.get(&key).cloned()
        }
        fn on_path(&self, program: &str) -> bool {
            self.path.contains(program)
        }
        fn os_version(&self) -> Option<String> {
            self.os_version.clone()
        }
        fn is_windows_program(&self, program: &str) -> bool {
            self.windows_path.contains(program)
        }
    }

    const UBUNTU: &str = r#"PRETTY_NAME="Ubuntu 24.04.1 LTS"
NAME="Ubuntu"
VERSION_ID="24.04"
VERSION="24.04.1 LTS (Noble Numbat)"
ID=ubuntu
ID_LIKE=debian
"#;
    const DEBIAN: &str = "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nNAME=\"Debian GNU/Linux\"\nVERSION_ID=\"12\"\nID=debian\n";
    const ROCKY: &str =
        "NAME=\"Rocky Linux\"\nID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\nVERSION_ID=\"9.4\"\n";
    const ARCH: &str = "NAME=\"Arch Linux\"\nID=arch\nBUILD_ID=rolling\n";
    const ALPINE: &str = "NAME=\"Alpine Linux\"\nID=alpine\nVERSION_ID=3.20.3\n";
    const FEDORA: &str = "NAME=\"Fedora Linux\"\nID=fedora\nVERSION_ID=40\n";
    const MINT: &str =
        "NAME=\"Linux Mint\"\nID=linuxmint\nID_LIKE=\"ubuntu debian\"\nVERSION_ID=\"22\"\n";
    const SUSE: &str = "NAME=\"openSUSE Tumbleweed\"\nID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\nVERSION_ID=\"20240901\"\n";

    fn linux(release: &str) -> FakeProbe {
        let mut p = FakeProbe::default();
        p.files.insert("/etc/os-release".into(), release.into());
        p
    }

    #[test]
    fn os_release_fixtures() {
        let cases = [
            (UBUNTU, "ubuntu", Some("debian"), Some("24.04")),
            (ROCKY, "rocky", Some("rhel"), Some("9.4")),
            (ARCH, "arch", Some("arch"), None),
            (ALPINE, "alpine", Some("alpine"), Some("3.20.3")),
            (FEDORA, "fedora", Some("rhel"), Some("40")),
            (MINT, "linuxmint", Some("debian"), Some("22")),
            (SUSE, "opensuse-tumbleweed", Some("suse"), Some("20240901")),
        ];
        for (text, distro, fam, version) in cases {
            let f = Facts::detect_with(&linux(text), None);
            assert_eq!(f.distro.as_deref(), Some(distro), "{text}");
            assert_eq!(f.family.as_deref(), fam, "{text}");
            assert_eq!(f.version.as_deref(), version, "{text}");
        }
    }

    #[test]
    fn falls_back_to_usr_lib_os_release() {
        let mut p = FakeProbe::default();
        p.files
            .insert("/usr/lib/os-release".into(), ALPINE.to_string());
        assert_eq!(
            Facts::detect_with(&p, None).distro.as_deref(),
            Some("alpine")
        );
        let f = Facts::detect_with(&FakeProbe::default(), None);
        assert_eq!((f.distro, f.family, f.version), (None, None, None));
    }

    #[test]
    fn linux_role_detection() {
        let mut p = linux(UBUNTU);
        assert_eq!(Facts::detect_with(&p, None).role, Role::Server);
        p.commands
            .insert("systemctl get-default".into(), "graphical.target".into());
        assert_eq!(
            Facts::detect_with(&p, None).role,
            Role::Server,
            "graphical.target without a display manager (stock Ubuntu Server)"
        );
        p.commands.insert(
            "systemctl is-enabled display-manager.service".into(),
            "enabled".into(),
        );
        assert_eq!(Facts::detect_with(&p, None).role, Role::Desktop);
        let mut p = linux(DEBIAN);
        p.commands
            .insert("systemctl get-default".into(), "graphical.target".into());
        p.env.insert("SSH_CONNECTION".into(), "1 2 3 4".into());
        p.env.insert("XDG_SESSION_TYPE".into(), "tty".into());
        assert_eq!(Facts::detect_with(&p, None).role, Role::Server, "Debian 12");
        p.files.insert(
            "/etc/systemd/system/display-manager.service".into(),
            String::new(),
        );
        p.env.remove("SSH_CONNECTION");
        assert_eq!(Facts::detect_with(&p, None).role, Role::Desktop);
        p.commands
            .insert("systemctl get-default".into(), "multi-user.target".into());
        assert_eq!(Facts::detect_with(&p, None).role, Role::Server);
        for (k, v) in [
            ("XDG_SESSION_TYPE", "wayland"),
            ("DISPLAY", ":0"),
            ("WAYLAND_DISPLAY", "wayland-0"),
        ] {
            let mut p = linux(UBUNTU);
            p.env.insert(k.into(), v.into());
            assert_eq!(Facts::detect_with(&p, None).role, Role::Desktop, "{k}");
        }
        // A display forwarded over `ssh -X` is not this machine's.
        for (k, v) in [
            ("DISPLAY", "localhost:10.0"),
            ("SSH_CONNECTION", "10.0.0.2 5555 10.0.0.1 22"),
        ] {
            let mut p = linux(UBUNTU);
            p.commands
                .insert("systemctl get-default".into(), "multi-user.target".into());
            p.env.insert("DISPLAY".into(), "localhost:10.0".into());
            p.env.insert(k.into(), v.into());
            assert_eq!(Facts::detect_with(&p, None).role, Role::Server, "{k}");
        }
        let mut p = linux(UBUNTU);
        p.env.insert("DISPLAY".into(), ":0".into());
        p.env.insert("SSH_TTY".into(), "/dev/pts/0".into());
        assert_eq!(
            Facts::detect_with(&p, None).role,
            Role::Server,
            "any DISPLAY inside an SSH session"
        );
        assert!(!is_remote_display(":0") && !is_remote_display("unix:0"));
        assert!(is_remote_display("localhost:10.0"));
        // A tty session is not graphical by itself.
        let mut p = linux(UBUNTU);
        p.env.insert("XDG_SESSION_TYPE".into(), "tty".into());
        assert_eq!(Facts::detect_with(&p, None).role, Role::Server);
        // The local override wins.
        let mut p = linux(UBUNTU);
        p.env.insert("DISPLAY".into(), ":0".into());
        assert_eq!(
            Facts::detect_with(&p, Some(Role::Server)).role,
            Role::Server
        );
    }

    #[test]
    fn macos_and_windows() {
        let p = FakeProbe {
            os: Some(Os::Macos),
            os_version: Some("15.1".into()),
            path: ["brew".to_string()].into(),
            ..Default::default()
        };
        let f = Facts::detect_with(&p, None);
        assert_eq!(f.role, Role::Desktop);
        assert_eq!(f.version.as_deref(), Some("15.1"));
        assert_eq!(f.pkg.as_deref(), Some("brew"));
        assert_eq!(f.distro, None);

        let p = FakeProbe {
            os: Some(Os::Windows),
            path: ["scoop".to_string(), "winget".to_string()].into(),
            ..Default::default()
        };
        let f = Facts::detect_with(&p, None);
        assert_eq!(f.pkg.as_deref(), Some("winget"), "priority order");
        assert!(f.env.is_empty());
    }

    #[test]
    fn env_detection() {
        let mut p = linux(UBUNTU);
        p.files.insert(
            "/proc/version".into(),
            "Linux version 5.15.153.1-microsoft-standard-WSL2".into(),
        );
        p.files.insert("/.dockerenv".into(), String::new());
        p.env.insert("SSH_CONNECTION".into(), "1 2 3 4".into());
        p.commands
            .insert("systemd-detect-virt --vm".into(), "kvm".into());
        let f = Facts::detect_with(&p, None);
        assert_eq!(f.env, ["container", "ssh", "vm", "wsl"]);

        let mut p = linux(UBUNTU);
        p.files.insert("/run/.containerenv".into(), String::new());
        p.commands
            .insert("systemd-detect-virt --vm".into(), "none".into());
        assert_eq!(Facts::detect_with(&p, None).env, ["container"]);
    }

    fn wsl(release: &str) -> FakeProbe {
        let mut p = linux(release);
        p.env.insert("WSL_DISTRO_NAME".into(), "Ubuntu".into());
        p
    }

    #[test]
    fn wsl_finds_the_windows_home_through_interop() {
        let mut p = wsl(UBUNTU);
        p.commands.insert(
            "cmd.exe /c echo %USERPROFILE%".into(),
            "C:\\Users\\Me\r".into(),
        );
        p.commands
            .insert("wslpath -u C:\\Users\\Me".into(), "/mnt/c/Users/Me".into());
        let f = Facts::detect_with(&p, None);
        assert_eq!(f.env, ["wsl"]);
        assert_eq!(f.windows_home.as_deref(), Some("/mnt/c/Users/Me"));
        // Without wslpath: the default automount mapping.
        p.commands.remove("wslpath -u C:\\Users\\Me");
        assert_eq!(
            Facts::detect_with(&p, None).windows_home.as_deref(),
            Some("/mnt/c/Users/Me")
        );
        // Interop off (cmd.exe cannot run), or not WSL at all: none.
        p.commands.clear();
        assert_eq!(Facts::detect_with(&p, None).windows_home, None);
        let mut p = linux(UBUNTU);
        p.commands.insert(
            "cmd.exe /c echo %USERPROFILE%".into(),
            "C:\\Users\\Me".into(),
        );
        assert_eq!(Facts::detect_with(&p, None).windows_home, None);
    }

    #[test]
    fn wsl_picks_the_linux_package_manager() {
        let mut p = wsl(UBUNTU);
        // Windows' PATH is appended in WSL: winget.exe is there, and a shim named like a Linux
        // manager could be too.
        p.path
            .extend(["winget".to_string(), "apt".to_string(), "brew".to_string()]);
        p.windows_path.insert("winget".into());
        assert_eq!(Facts::detect_with(&p, None).pkg.as_deref(), Some("apt"));
        let mut p = wsl(FEDORA);
        p.path.extend(["dnf".to_string(), "apt".to_string()]);
        p.windows_path.insert("dnf".into());
        assert_eq!(
            Facts::detect_with(&p, None).pkg.as_deref(),
            Some("apt"),
            "a Windows-side program never counts"
        );
        assert!(!pkg_priority(Os::Linux, Some("debian")).contains(&"winget"));
    }

    #[test]
    fn pkg_priority_follows_the_os_and_distro() {
        let mut p = linux(UBUNTU);
        assert_eq!(Facts::detect_with(&p, None).pkg, None);
        p.path.insert("apk".into());
        p.path.insert("apt".into());
        assert_eq!(Facts::detect_with(&p, None).pkg.as_deref(), Some("apt"));
        // Homebrew on Linux (linuxbrew) does not replace the distro's own manager.
        p.path.insert("brew".into());
        assert_eq!(Facts::detect_with(&p, None).pkg.as_deref(), Some("apt"));
        let mut p = linux(FEDORA);
        p.path
            .extend(["brew".to_string(), "dnf".to_string(), "apt".to_string()]);
        assert_eq!(Facts::detect_with(&p, None).pkg.as_deref(), Some("dnf"));
        let mut p = linux(UBUNTU);
        p.path.insert("brew".into());
        assert_eq!(
            Facts::detect_with(&p, None).pkg.as_deref(),
            Some("brew"),
            "the only one there"
        );
        let p = FakeProbe {
            os: Some(Os::Macos),
            path: ["apt".to_string(), "brew".to_string()].into(),
            ..Default::default()
        };
        assert_eq!(Facts::detect_with(&p, None).pkg.as_deref(), Some("brew"));
    }

    #[test]
    fn version_requirements() {
        let ok = |req: &str, v: &str| VersionReq::parse(req).unwrap().matches(v);
        assert!(ok("24.04", "24.04"));
        assert!(!ok("24.04", "24.4"), "a bare version is an exact match");
        assert!(ok(">=22.04", "24.04"));
        assert!(ok(">=22.04", "22.04"));
        assert!(!ok(">=22.04", "20.04"));
        assert!(ok("<24", "22.04"));
        assert!(!ok("<24", "24.0"));
        assert!(ok(">=22.04, <25", "24.10"));
        assert!(!ok(">=22.04, <25", "25.04"));
        assert!(ok("==24", "24.0.0"));
        assert!(ok("!=24", "24.1"));
        assert!(!ok(">1", "rolling"));
        assert!(VersionReq::parse(">=").is_err());
        assert!(VersionReq::parse(">=abc").is_err());
        assert!(VersionReq::parse("22, <25").is_err());
    }

    #[test]
    fn codename_from_os_release() {
        let f = Facts::detect_with(&linux(UBUNTU), None);
        assert_eq!(f.codename, None);
        let f = Facts::detect_with(
            &linux(&format!(
                "{UBUNTU}VERSION_CODENAME=noble\nUBUNTU_CODENAME=noble\n"
            )),
            None,
        );
        assert_eq!(f.codename.as_deref(), Some("noble"));
        assert!(f.test(Fact::Codename, "noble"));
        assert_eq!(f.get(Fact::Codename), Some("noble"));
        // Mint: its own codename first; Ubuntu's when that is empty.
        let f = Facts::detect_with(
            &linux(&format!("{MINT}VERSION_CODENAME=\nUBUNTU_CODENAME=noble\n")),
            None,
        );
        assert_eq!(f.codename.as_deref(), Some("noble"));
        assert!(f.pairs().contains(&("codename", "noble".to_string())));
    }

    #[test]
    fn overrides_replace_detected_facts() {
        let mut f = Facts::new(Os::Macos, "aarch64", Role::Desktop);
        f.pkg = Some("brew".into());
        f.apply_overrides(
            "os=linux, family=debian,distro=ubuntu,version=24.04,codename=noble,pkg=apt,env=ssh+wsl,role=server",
        )
        .unwrap();
        assert_eq!(f.os, Os::Linux);
        assert_eq!(f.codename.as_deref(), Some("noble"));
        assert_eq!(f.pkg.as_deref(), Some("apt"));
        assert_eq!(f.env, ["ssh", "wsl"]);
        assert_eq!(f.role, Role::Server);
        f.apply_overrides("pkg=,distro=").unwrap();
        assert_eq!((f.pkg.as_deref(), f.distro.as_deref()), (None, None));
        f.apply_overrides("arch=x86_64").unwrap();
        assert_eq!(f.arch, "x86_64");
        assert!(f.test(Fact::Arch, "x86_64"));
        for bad in ["os=beos", "arch=x", "nope=1", "pkg=yum", "env=moon", "junk"] {
            assert!(f.apply_overrides(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn simulating_another_machine_drops_the_hosts_facts_and_derives_the_rest() {
        let mac = || {
            let mut f = Facts::new(Os::Macos, "aarch64", Role::Desktop);
            f.version = Some("26.5".into());
            f.pkg = Some("brew".into());
            f.init = Some("launchd".into());
            f.env = vec!["ssh".into()];
            f
        };
        let mut f = mac();
        f.apply_overrides("os=linux,distro=ubuntu").unwrap();
        assert_eq!(f.family.as_deref(), Some("debian"));
        assert_eq!(f.pkg.as_deref(), Some("apt"));
        assert_eq!(f.init.as_deref(), Some("systemd"));
        assert_eq!((f.version.as_deref(), f.codename.as_deref()), (None, None));
        assert!(f.env.is_empty());

        // Given facts win over derived ones.
        let mut f = mac();
        f.apply_overrides("os=linux,distro=fedora,version=41,env=container,pkg=")
            .unwrap();
        assert_eq!(f.family.as_deref(), Some("rhel"));
        assert_eq!(f.version.as_deref(), Some("41"));
        assert_eq!((f.pkg.as_deref(), f.init.as_deref()), (None, Some("other")));

        let mut f = mac();
        f.apply_overrides("os=windows").unwrap();
        assert_eq!(
            (f.pkg.as_deref(), f.init.as_deref()),
            (Some("winget"), Some("other"))
        );
        assert_eq!(f.version, None);

        // The same machine, only some facts changed: the rest stay as detected.
        let mut f = mac();
        f.apply_overrides("os=macos,role=server").unwrap();
        assert_eq!(
            f,
            Facts {
                role: Role::Server,
                ..mac()
            }
        );
    }

    #[test]
    fn init_is_the_running_service_manager() {
        let mut p = linux(UBUNTU);
        p.path.insert("systemctl".into());
        // systemctl alone (a container, WSL without systemd) is not a running systemd.
        assert_eq!(Facts::detect_with(&p, None).init.as_deref(), Some("other"));
        p.files.insert(SYSTEMD_RUNNING.into(), String::new());
        let f = Facts::detect_with(&p, None);
        assert_eq!(f.init.as_deref(), Some("systemd"));
        assert!(f.test(Fact::Init, "systemd"));
        let mac = FakeProbe {
            os: Some(Os::Macos),
            ..Default::default()
        };
        assert_eq!(
            Facts::detect_with(&mac, None).init.as_deref(),
            Some("launchd")
        );
        let win = FakeProbe {
            os: Some(Os::Windows),
            ..Default::default()
        };
        assert_eq!(
            Facts::detect_with(&win, None).init.as_deref(),
            Some("other")
        );
        let mut f = Facts::new(Os::Linux, "x86_64", Role::Server);
        assert_eq!(f.init, None);
        f.apply_overrides("init=systemd").unwrap();
        assert_eq!(f.init.as_deref(), Some("systemd"));
        assert!(f.apply_overrides("init=upstart").is_err());
        assert!(Fact::Init.check_value("!systemd").is_ok());
        assert!(Fact::Init.check_value("runit").is_err());
    }

    #[test]
    fn facts_test_and_describe() {
        let mut f = Facts::new(Os::Linux, "x86_64", Role::Server);
        f.version = Some("24.04".into());
        f.env = vec!["ssh".into(), "wsl".into()];
        assert!(f.test(Fact::Version, ">=22"));
        assert!(f.test(Fact::Env, "wsl"));
        assert!(!f.test(Fact::Env, "vm"));
        assert_eq!(f.describe(Fact::Env), "ssh, wsl");
        assert_eq!(f.describe(Fact::Distro), "unknown");
        assert!(Fact::Pkg.check_value("apt").is_ok());
        assert!(Fact::Pkg.check_value("yum").is_err());
        assert!(Fact::Version.check_value(">=x").is_err());
    }
}
