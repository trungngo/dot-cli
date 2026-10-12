//! Linux package managers: apt, dnf, zypper, pacman, apk and snap. All install as root.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use dot_core::config::PkgManager;
use dot_core::sections::APT;
use dot_core::{StateStore, Stream};

use super::{
    InstallOpts, Outdated, PackageManager, check_root, first_words, membership, pick_outdated,
    query, run,
};
use crate::exec::{Cmd, OutFn, Runner};

/// apt's refresh stamp, in `dot`'s state store: the time of the last successful `apt-get
/// update`, in seconds since the epoch.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct AptStamp {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refreshed: Option<u64>,
}

/// How often apt's package lists are refreshed.
pub const APT_REFRESH_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// Present while snapd runs.
pub const SNAPD_SOCKET: &str = "/run/snapd.socket";

pub struct Apt {
    pub(super) runner: Arc<dyn Runner>,
    /// `None`: no state directory; refresh on every run that installs.
    pub(super) state: Option<StateStore>,
    /// `apt-get update` already ran (or failed) in this run.
    pub(super) attempted: AtomicBool,
}

impl Apt {
    fn root(&self, cmd: Cmd) -> Cmd {
        cmd.env("DEBIAN_FRONTEND", "noninteractive")
            .as_root(self.runner.is_root())
    }

    /// Age of the last successful refresh, from the stamp.
    fn stamp_age(&self) -> Option<Duration> {
        let stamp: AptStamp = self.state.as_ref()?.section(APT);
        let secs = stamp.refreshed?;
        SystemTime::now()
            .duration_since(UNIX_EPOCH + Duration::from_secs(secs))
            .ok()
    }

    /// `apt-get update`, whatever the stamp says. A failure is a warning.
    fn update(&self, out: &mut OutFn<'_>) -> bool {
        self.attempted.store(true, Ordering::SeqCst);
        let cmd = self.root(Cmd::new("apt-get").args(["update", "-q"]));
        let problem = match self.runner.stream(&cmd, out) {
            Ok(o) if o.success() => None,
            Ok(o) => Some(format!(
                "{}{}",
                o.describe_exit(),
                o.last_line().map(|l| format!(": {l}")).unwrap_or_default()
            )),
            Err(e) => Some(format!("could not run: {e}")),
        };
        if let Some(p) = problem {
            out(
                Stream::Stderr,
                &format!(
                    "warning: `apt-get update` {p}; continuing with the existing package lists"
                ),
            );
            return false;
        }
        if let Some(state) = &self.state {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let written = state.update_section(APT, |s: &mut AptStamp| s.refreshed = Some(now));
            if let Err(e) = written {
                out(
                    Stream::Stderr,
                    &format!("warning: {e:#}; apt will refresh again next run"),
                );
            }
        }
        true
    }

    /// `(name, status)` for each of `packages` dpkg knows (`ii`, `hi`, `rc`, ...).
    fn dpkg_status(&self, packages: &[String]) -> Result<Vec<(String, String)>> {
        let cmd = Cmd::new("dpkg-query")
            .args(["-W", "-f", "${Package}\t${db:Status-Abbrev}\n"])
            .args(packages.iter().cloned());
        let out = query(self.runner.as_ref(), &cmd, &[0, 1])?;
        Ok(out
            .stdout
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(n, st)| (n.to_string(), st.trim().to_string()))
            .collect())
    }

    /// The held packages among `packages` (dpkg's names, without an architecture).
    fn held(&self, packages: &[String]) -> Result<Vec<String>> {
        Ok(self
            .dpkg_status(packages)?
            .into_iter()
            .filter(|(_, st)| st.starts_with('h'))
            .map(|(n, _)| n)
            .collect())
    }

    fn install_cmd(&self, packages: &[String]) -> Cmd {
        self.root(
            Cmd::new("apt-get")
                .args(APT_INSTALL)
                .args(packages.iter().cloned()),
        )
    }
}

/// `fd-find` for `fd-find:amd64`.
fn base_name(p: &str) -> &str {
    p.split(':').next().unwrap_or(p)
}

/// `apt-get install` without questions, keeping changed config files.
const APT_INSTALL: [&str; 7] = [
    "install",
    "-y",
    "-q",
    "-o",
    "Dpkg::Options::=--force-confdef",
    "-o",
    "Dpkg::Options::=--force-confold",
];

/// `apt-cache policy <names>`: `(name, installed, candidate)` per package block (`(none)` when
/// not installed or without a candidate).
pub fn parse_apt_policy(text: &str) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) && line.trim_end().ends_with(':') {
            let name = line.trim_end().trim_end_matches(':').to_string();
            out.push((name, String::new(), String::new()));
            continue;
        }
        let Some(cur) = out.last_mut() else { continue };
        let t = line.trim();
        if let Some(v) = t.strip_prefix("Installed:") {
            cur.1 = v.trim().to_string();
        } else if let Some(v) = t.strip_prefix("Candidate:") {
            cur.2 = v.trim().to_string();
        }
    }
    out
}

/// apt's words for "the package lists do not know this package (yet)".
fn no_candidate(text: &str) -> bool {
    text.contains("Unable to locate package")
        || text.contains("has no installation candidate")
        || text.contains("is not available, but is referred to")
}

impl PackageManager for Apt {
    fn kind(&self) -> PkgManager {
        PkgManager::Apt
    }

    /// `apt-cache pkgnames <prefix>` (the pattern up to its first wildcard), filtered.
    fn names_matching(&self, pattern: &str) -> Result<Option<Vec<String>>> {
        let prefix = pattern.split(['*', '?']).next().unwrap_or("");
        let cmd = Cmd::new("apt-cache").args(["pkgnames", prefix]);
        let out = query(self.runner.as_ref(), &cmd, &[0])?;
        Ok(Some(
            out.stdout
                .lines()
                .map(str::trim)
                .filter(|n| super::name_matches(pattern, n))
                .map(str::to_string)
                .collect(),
        ))
    }

    fn available(&self) -> bool {
        self.runner.on_path("apt-get") && self.runner.on_path("dpkg-query")
    }

    /// `dpkg-query -W -f '${Package}\t${db:Status-Abbrev}\n' <names>`: installed when the status
    /// starts with `ii` (or `hi`, held). Exits 1 when some name is unknown to dpkg.
    fn installed(&self, packages: &[String]) -> Result<Vec<bool>> {
        let found: Vec<String> = self
            .dpkg_status(packages)?
            .into_iter()
            .filter(|(_, st)| st.starts_with("ii") || st.starts_with("hi"))
            .map(|(name, _)| name)
            .collect();
        // `fd-find:amd64` is installed when dpkg lists `fd-find`.
        let names: Vec<String> = packages
            .iter()
            .map(|p| p.split(':').next().unwrap_or(p).to_string())
            .collect();
        Ok(membership(&names, &found))
    }

    /// `apt-get install -y` with the existing config files kept. When apt knows no candidate
    /// for a package and the lists were not refreshed in this run (the stamp said they were
    /// fresh), they are refreshed and the install is tried once more.
    fn install(&self, packages: &[String], _: InstallOpts, out: &mut OutFn<'_>) -> Result<()> {
        check_root(self.runner.as_ref(), "installing apt packages")?;
        let cmd = self.install_cmd(packages);
        match run(self.runner.as_ref(), &cmd, out) {
            Ok(_) => Ok(()),
            Err(e) if !self.attempted.load(Ordering::SeqCst) && no_candidate(&format!("{e:#}")) => {
                out(
                    Stream::Stderr,
                    "apt has no candidate for a package: refreshing the package lists and \
                     trying again",
                );
                self.update(out);
                run(self.runner.as_ref(), &cmd, out).map(|_| ())
            }
            Err(e) => Err(e),
        }
    }

    /// `apt-cache policy <names>`: outdated when the candidate is not the installed version,
    /// except held packages (dpkg status `h...`, `apt-mark hold`): apt refuses to upgrade those
    /// with `-y`, so they are never offered.
    fn outdated(&self, packages: &[String]) -> Result<Vec<Outdated>> {
        let held = self.held(packages)?;
        let packages: Vec<String> = packages
            .iter()
            .filter(|p| !held.iter().any(|h| h == base_name(p)))
            .cloned()
            .collect();
        if packages.is_empty() {
            return Ok(Vec::new());
        }
        let packages = &packages[..];
        let cmd = Cmd::new("apt-cache")
            .arg("policy")
            .env("LC_ALL", "C")
            .args(packages.iter().cloned());
        let out = query(self.runner.as_ref(), &cmd, &[0])?;
        let found = parse_apt_policy(&out.stdout)
            .into_iter()
            .filter(|(_, inst, cand)| {
                !inst.is_empty()
                    && !cand.is_empty()
                    && inst != "(none)"
                    && cand != "(none)"
                    && inst != cand
            })
            .map(|(n, inst, cand)| {
                // `fd-find:amd64` is asked for; apt prints it back as is or without the arch.
                (n.split(':').next().unwrap_or(&n).to_string(), inst, cand)
            })
            .collect();
        let names: Vec<String> = packages
            .iter()
            .map(|p| p.split(':').next().unwrap_or(p).to_string())
            .collect();
        Ok(pick_outdated(&names, found)
            .into_iter()
            .map(|mut o| {
                if let Some(p) = packages
                    .iter()
                    .find(|p| p.split(':').next() == Some(&o.name))
                {
                    o.name.clone_from(p);
                }
                o
            })
            .collect())
    }

    /// `apt-get install --only-upgrade`.
    fn upgrade(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        check_root(self.runner.as_ref(), "upgrading apt packages")?;
        let cmd = self.root(
            Cmd::new("apt-get")
                .args(APT_INSTALL)
                .arg("--only-upgrade")
                .args(packages.iter().cloned()),
        );
        run(self.runner.as_ref(), &cmd, out).map(|_| ())
    }

    /// The lists were not refreshed in this run and the stamp is missing or a day old.
    fn stale(&self) -> bool {
        !self.attempted.load(Ordering::SeqCst)
            && self.stamp_age().is_none_or(|age| age >= APT_REFRESH_EVERY)
    }

    /// Forgets the stamp (and that this run refreshed), so the next install refreshes: a
    /// repository was added.
    fn invalidate(&self) {
        self.attempted.store(false, Ordering::SeqCst);
        if let Some(state) = &self.state {
            let _ = state.update_section(APT, |s: &mut AptStamp| s.refreshed = None);
        }
    }

    /// `apt-get update`, at most once a day (and once per run).
    fn refresh(&self, out: &mut OutFn<'_>) -> Result<()> {
        if self.attempted.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(age) = self.stamp_age()
            && age < APT_REFRESH_EVERY
        {
            let hours = age.as_secs() / 3600;
            out(
                Stream::Stdout,
                &format!(
                    "apt package lists were refreshed {} ago; not refreshing",
                    if hours == 0 {
                        "less than an hour".to_string()
                    } else {
                        format!("{hours}h")
                    }
                ),
            );
            return Ok(());
        }
        check_root(self.runner.as_ref(), "refreshing apt's package lists")?;
        self.update(out);
        Ok(())
    }

    /// `apt-get remove -y <names>` (configuration files are kept, as `apt remove` does).
    fn uninstall(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        check_root(self.runner.as_ref(), "removing apt packages")?;
        let cmd = self.root(
            Cmd::new("apt-get")
                .args(["remove", "-y", "-q"])
                .args(packages.iter().cloned()),
        );
        run(self.runner.as_ref(), &cmd, out).map(|_| ())
    }

    fn needs_root(&self) -> bool {
        true
    }
}

/// dnf and zypper: queried through `rpm`.
pub struct Rpm {
    runner: Arc<dyn Runner>,
    kind: PkgManager,
}

impl Rpm {
    pub fn dnf(runner: Arc<dyn Runner>) -> Self {
        Self {
            runner,
            kind: PkgManager::Dnf,
        }
    }

    pub fn zypper(runner: Arc<dyn Runner>) -> Self {
        Self {
            runner,
            kind: PkgManager::Zypper,
        }
    }

    fn program(&self) -> &'static str {
        match self.kind {
            PkgManager::Zypper => "zypper",
            _ => "dnf",
        }
    }
}

impl PackageManager for Rpm {
    fn kind(&self) -> PkgManager {
        self.kind
    }

    /// dnf: `dnf repoquery --queryformat '%{name}\n' <pattern>`; zypper cannot.
    fn names_matching(&self, pattern: &str) -> Result<Option<Vec<String>>> {
        if self.kind != PkgManager::Dnf {
            return Ok(None);
        }
        let cmd = Cmd::new("dnf").args(["repoquery", "--queryformat", "%{name}\n", pattern]);
        let out = query(self.runner.as_ref(), &cmd, &[0])?;
        let mut names: Vec<String> = out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|n| super::name_matches(pattern, n))
            .map(str::to_string)
            .collect();
        names.dedup();
        Ok(Some(names))
    }

    fn available(&self) -> bool {
        self.runner.on_path(self.program()) && self.runner.on_path("rpm")
    }

    /// `rpm -q --qf '%{NAME}\n' <names>`: the name of each installed one, `package x is not
    /// installed` for the others (exit code: how many are missing).
    fn installed(&self, packages: &[String]) -> Result<Vec<bool>> {
        let cmd = Cmd::new("rpm")
            .args(["-q", "--qf", "%{NAME}\n"])
            .args(packages.iter().cloned());
        let ok: Vec<i32> = (0..=packages.len().min(255) as i32).collect();
        let out = query(self.runner.as_ref(), &cmd, &ok)?;
        let found: Vec<String> = out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.contains(' '))
            .map(str::to_string)
            .collect();
        Ok(membership(packages, &found))
    }

    /// `dnf install -y` / `zypper --non-interactive install --auto-agree-with-licenses`.
    fn install(&self, packages: &[String], _: InstallOpts, out: &mut OutFn<'_>) -> Result<()> {
        check_root(
            self.runner.as_ref(),
            &format!("installing {} packages", self.program()),
        )?;
        let cmd = match self.kind {
            PkgManager::Zypper => Cmd::new("zypper").args([
                "--non-interactive",
                "install",
                "--auto-agree-with-licenses",
            ]),
            _ => Cmd::new("dnf").args(["install", "-y"]),
        };
        let cmd = cmd
            .args(packages.iter().cloned())
            .as_root(self.runner.is_root());
        run(self.runner.as_ref(), &cmd, out).map(|_| ())
    }

    /// dnf: `dnf check-update -q <names>` (exit 100: updates, `name.arch version repo` lines),
    /// with the installed versions from `rpm -q`. zypper: `zypper list-updates` (a `|` table).
    fn outdated(&self, packages: &[String]) -> Result<Vec<Outdated>> {
        let r = self.runner.as_ref();
        let found = match self.kind {
            PkgManager::Zypper => {
                let cmd = Cmd::new("zypper").args(["--non-interactive", "--quiet", "list-updates"]);
                parse_zypper_updates(&query(r, &cmd, &[0])?.stdout)
            }
            _ => {
                let cmd = Cmd::new("dnf")
                    .args(["check-update", "-q"])
                    .args(packages.iter().cloned());
                let latest = parse_dnf_check_update(&query(r, &cmd, &[0, 100])?.stdout);
                if latest.is_empty() {
                    return Ok(Vec::new());
                }
                let cmd = Cmd::new("rpm")
                    .args(["-q", "--qf", "%{NAME} %{VERSION}-%{RELEASE}\n"])
                    .args(packages.iter().cloned());
                let ok: Vec<i32> = (0..=packages.len().min(255) as i32).collect();
                let current = query(r, &cmd, &ok)?.stdout;
                latest
                    .into_iter()
                    .filter_map(|(name, new)| {
                        let cur = current.lines().find_map(|l| {
                            let (n, v) = l.trim().split_once(' ')?;
                            (n == name && !v.contains(' ')).then(|| v.to_string())
                        })?;
                        Some((name, cur, new))
                    })
                    .collect()
            }
        };
        Ok(pick_outdated(packages, found))
    }

    /// `dnf upgrade -y` / `zypper --non-interactive update --auto-agree-with-licenses`.
    fn upgrade(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        check_root(
            self.runner.as_ref(),
            &format!("upgrading {} packages", self.program()),
        )?;
        let cmd = match self.kind {
            PkgManager::Zypper => Cmd::new("zypper").args([
                "--non-interactive",
                "update",
                "--auto-agree-with-licenses",
            ]),
            _ => Cmd::new("dnf").args(["upgrade", "-y"]),
        };
        let cmd = cmd
            .args(packages.iter().cloned())
            .as_root(self.runner.is_root());
        run(self.runner.as_ref(), &cmd, out).map(|_| ())
    }

    /// `dnf remove -y` / `zypper --non-interactive remove`.
    fn uninstall(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        check_root(
            self.runner.as_ref(),
            &format!("removing {} packages", self.program()),
        )?;
        let cmd = match self.kind {
            PkgManager::Zypper => Cmd::new("zypper").args(["--non-interactive", "remove"]),
            _ => Cmd::new("dnf").args(["remove", "-y"]),
        };
        let cmd = cmd
            .args(packages.iter().cloned())
            .as_root(self.runner.is_root());
        run(self.runner.as_ref(), &cmd, out).map(|_| ())
    }

    fn needs_root(&self) -> bool {
        true
    }
}

/// `dnf check-update -q`: `(name, version)` from `name.arch  version  repo` lines.
pub fn parse_dnf_check_update(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            let [pkg, version, _repo] = w.as_slice() else {
                return None;
            };
            let name = pkg.rsplit_once('.').map_or(*pkg, |(n, _)| n);
            Some((name.to_string(), version.to_string()))
        })
        .collect()
}

/// `zypper list-updates`: `(name, current, available)` from its `|`-separated table (`S |
/// Repository | Name | Current Version | Available Version | Arch`).
pub fn parse_zypper_updates(text: &str) -> Vec<(String, String, String)> {
    text.lines()
        .filter_map(|l| {
            let cols: Vec<&str> = l.split('|').map(str::trim).collect();
            if cols.len() < 6 || cols[2] == "Name" || cols[2].is_empty() {
                return None;
            }
            Some((
                cols[2].to_string(),
                cols[3].to_string(),
                cols[4].to_string(),
            ))
        })
        .collect()
}

pub struct Pacman(pub(super) Arc<dyn Runner>);

impl PackageManager for Pacman {
    fn kind(&self) -> PkgManager {
        PkgManager::Pacman
    }

    fn available(&self) -> bool {
        self.0.on_path("pacman")
    }

    /// `pacman -Q <names>`: `name version` for each installed one; exits 1 when any is not.
    fn installed(&self, packages: &[String]) -> Result<Vec<bool>> {
        let cmd = Cmd::new("pacman").arg("-Q").args(packages.iter().cloned());
        let out = query(self.0.as_ref(), &cmd, &[0, 1])?;
        Ok(membership(packages, &first_words(&out.stdout)))
    }

    /// `pacman -S --needed --noconfirm`.
    fn install(&self, packages: &[String], _: InstallOpts, out: &mut OutFn<'_>) -> Result<()> {
        check_root(self.0.as_ref(), "installing pacman packages")?;
        let cmd = Cmd::new("pacman")
            .args(["-S", "--needed", "--noconfirm"])
            .args(packages.iter().cloned())
            .as_root(self.0.is_root());
        run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    /// `pacman -Qu <names>`: `name old -> new` lines; exits 1 when there are none. Against the
    /// sync databases as they are (refreshing them alone would be a partial upgrade).
    fn outdated(&self, packages: &[String]) -> Result<Vec<Outdated>> {
        let cmd = Cmd::new("pacman").arg("-Qu").args(packages.iter().cloned());
        let out = query(self.0.as_ref(), &cmd, &[0, 1])?;
        let found = out
            .stdout
            .lines()
            .filter_map(|l| {
                let w: Vec<&str> = l.split_whitespace().collect();
                match w.as_slice() {
                    [name, old, "->", new, ..] => {
                        Some((name.to_string(), old.to_string(), new.to_string()))
                    }
                    _ => None,
                }
            })
            .collect();
        Ok(pick_outdated(packages, found))
    }

    /// `pacman -S --noconfirm <names>`: the versions of the sync databases.
    fn upgrade(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        check_root(self.0.as_ref(), "upgrading pacman packages")?;
        let cmd = Cmd::new("pacman")
            .args(["-S", "--noconfirm"])
            .args(packages.iter().cloned())
            .as_root(self.0.is_root());
        run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    /// `pacman -R --noconfirm <names>`.
    fn uninstall(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        check_root(self.0.as_ref(), "removing pacman packages")?;
        let cmd = Cmd::new("pacman")
            .args(["-R", "--noconfirm"])
            .args(packages.iter().cloned())
            .as_root(self.0.is_root());
        run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    fn needs_root(&self) -> bool {
        true
    }
}

pub struct Apk(pub(super) Arc<dyn Runner>);

impl PackageManager for Apk {
    fn kind(&self) -> PkgManager {
        PkgManager::Apk
    }

    fn available(&self) -> bool {
        self.0.on_path("apk")
    }

    /// `apk info -e <names>`: prints the installed ones; the exit code counts the others.
    fn installed(&self, packages: &[String]) -> Result<Vec<bool>> {
        let cmd = Cmd::new("apk")
            .args(["info", "-e"])
            .args(packages.iter().cloned());
        let ok: Vec<i32> = (0..=packages.len().min(255) as i32).collect();
        let out = query(self.0.as_ref(), &cmd, &ok)?;
        Ok(membership(packages, &first_words(&out.stdout)))
    }

    /// `apk add --no-cache`: fetches fresh indexes every time, so there is nothing to refresh.
    fn install(&self, packages: &[String], _: InstallOpts, out: &mut OutFn<'_>) -> Result<()> {
        check_root(self.0.as_ref(), "installing apk packages")?;
        let cmd = Cmd::new("apk")
            .args(["add", "--no-cache"])
            .args(packages.iter().cloned())
            .as_root(self.0.is_root());
        run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    /// `apk version -l '<' <names>`: `name-version < newversion` lines.
    fn outdated(&self, packages: &[String]) -> Result<Vec<Outdated>> {
        let cmd = Cmd::new("apk")
            .args(["version", "-l", "<"])
            .args(packages.iter().cloned());
        let out = query(self.0.as_ref(), &cmd, &[0])?;
        Ok(pick_outdated(packages, parse_apk_version(&out.stdout)))
    }

    /// `apk add --no-cache --upgrade <names>`.
    fn upgrade(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        check_root(self.0.as_ref(), "upgrading apk packages")?;
        let cmd = Cmd::new("apk")
            .args(["add", "--no-cache", "--upgrade"])
            .args(packages.iter().cloned())
            .as_root(self.0.is_root());
        run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    /// `apk del <names>`.
    fn uninstall(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        check_root(self.0.as_ref(), "removing apk packages")?;
        let cmd = Cmd::new("apk")
            .arg("del")
            .args(packages.iter().cloned())
            .as_root(self.0.is_root());
        run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    fn needs_root(&self) -> bool {
        true
    }
}

pub struct Snap(pub(super) Arc<dyn Runner>);

impl PackageManager for Snap {
    fn kind(&self) -> PkgManager {
        PkgManager::Snap
    }

    fn available(&self) -> bool {
        self.0.on_path("snap")
    }

    /// `snap list`: a table (`Name Version Rev ...`) of every installed snap.
    fn installed(&self, packages: &[String]) -> Result<Vec<bool>> {
        let out = query(self.0.as_ref(), &Cmd::new("snap").arg("list"), &[0])?;
        let found: Vec<String> = first_words(&out.stdout)
            .into_iter()
            .skip_while(|w| w != "Name")
            .skip(1)
            .collect();
        Ok(membership(packages, &found))
    }

    /// `snap install <names>`; a `--classic` snap is installed on its own (snap takes mode
    /// flags for a single snap only).
    fn install(&self, packages: &[String], opts: InstallOpts, out: &mut OutFn<'_>) -> Result<()> {
        check_root(self.0.as_ref(), "installing snaps")?;
        let root = self.0.is_root();
        if opts.classic {
            for p in packages {
                let cmd = Cmd::new("snap")
                    .args(["install", "--classic", p.as_str()])
                    .as_root(root);
                run(self.0.as_ref(), &cmd, out)?;
            }
            return Ok(());
        }
        let cmd = Cmd::new("snap")
            .arg("install")
            .args(packages.iter().cloned())
            .as_root(root);
        run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    /// `snap refresh --list` (the new versions) against `snap list` (the installed ones).
    fn outdated(&self, packages: &[String]) -> Result<Vec<Outdated>> {
        let r = self.0.as_ref();
        let pending = query(r, &Cmd::new("snap").args(["refresh", "--list"]), &[0])?.stdout;
        let latest = snap_table(&pending);
        if latest.is_empty() {
            return Ok(Vec::new());
        }
        let installed = snap_table(&query(r, &Cmd::new("snap").arg("list"), &[0])?.stdout);
        let found = latest
            .into_iter()
            .map(|(n, new)| {
                let cur = installed
                    .iter()
                    .find(|(i, _)| *i == n)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                (n, cur, new)
            })
            .collect();
        Ok(pick_outdated(packages, found))
    }

    /// `snap refresh <names>`.
    fn upgrade(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        check_root(self.0.as_ref(), "refreshing snaps")?;
        let cmd = Cmd::new("snap")
            .arg("refresh")
            .args(packages.iter().cloned())
            .as_root(self.0.is_root());
        run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    /// `snap remove <names>`.
    fn uninstall(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        check_root(self.0.as_ref(), "removing snaps")?;
        let cmd = Cmd::new("snap")
            .arg("remove")
            .args(packages.iter().cloned())
            .as_root(self.0.is_root());
        run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    fn needs_root(&self) -> bool {
        true
    }
}

/// `(name, version)` rows of a snap table (`Name  Version  Rev ...` header).
fn snap_table(text: &str) -> Vec<(String, String)> {
    text.lines()
        .skip_while(|l| !l.starts_with("Name"))
        .skip(1)
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            Some((w.next()?.to_string(), w.next()?.to_string()))
        })
        .collect()
}

/// `apk version -l '<'`: `(name, current, available)` from `name-1.2-r0  <  1.3-r0` lines. The
/// version is the last two `-` parts of the first word (`<version>-r<n>`).
pub fn parse_apk_version(text: &str) -> Vec<(String, String, String)> {
    text.lines()
        .filter_map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            let [pkg, "<", new, ..] = w.as_slice() else {
                return None;
            };
            let (rest, rel) = pkg.rsplit_once('-')?;
            if !rel.starts_with('r') {
                return None;
            }
            let (name, ver) = rest.rsplit_once('-')?;
            Some((name.to_string(), format!("{ver}-{rel}"), new.to_string()))
        })
        .collect()
}
