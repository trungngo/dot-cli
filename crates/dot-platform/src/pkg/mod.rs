//! Package managers: Homebrew formulae and casks, apt, dnf, pacman, zypper, apk, snap, winget and
//! scoop. Each answers "which of these are installed" in one query where the tool allows it,
//! installs a batch non-interactively, and refreshes its metadata when that is needed.
//!
//! Everything goes through a [`Runner`], so tests assert the exact argv without running a real
//! package manager. Installs take [`lock`], so only one package manager runs at a time even when
//! bundles run in parallel (`--jobs`).
//!
//! Refreshing: apt's package lists are updated at most once a day (a stamp in `dot`'s state
//! store), with a warning, not a failure, when that fails; and an install that finds no
//! candidate for a package refreshes once more and retries before giving up, so a stale list is
//! never trusted. The other managers refresh by themselves as part of an install (Homebrew's
//! auto-update, dnf's `metadata_expire`, zypper's autorefresh, `apk add --no-cache`, snapd,
//! winget's source cache, scoop's self-update); their `refresh` does nothing. pacman is not
//! refreshed on its own on purpose: `pacman -Sy` followed by `pacman -S` is a partial upgrade,
//! which Arch does not support.
//!
//! Upgrading: by default a `package` step only installs what is missing. With `upgrade: true`
//! (or `dot apply --upgrade`) it also asks the manager which of the installed packages are
//! outdated ([`PackageManager::outdated`], read-only) and upgrades those
//! ([`PackageManager::upgrade`]).

mod brew;
mod linux;
pub mod repo;
mod vscode;
mod windows;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use anyhow::{Context as _, Result, bail};
use dot_core::StateStore;
use dot_core::config::PkgManager;

use crate::exec::{Cmd, OutFn, Output, Runner, SystemRunner};
use crate::sudo::Sudo;

pub use brew::{CaskFetcher, CaskInfo, CurlFetcher, parse_cask_info};

/// Options for one install batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InstallOpts {
    /// brew casks: `--adopt` (take over an app already in `/Applications`).
    pub adopt: bool,
    /// snap: `--classic`.
    pub classic: bool,
    /// winget: `--scope machine` (for every user; needs an elevated terminal).
    pub machine: bool,
    /// The installer runs sudo (a cask's `.pkg`), left free to ask for the password: it asks on
    /// the controlling terminal whatever its stdio (the output is still streamed), so the caller
    /// pauses the live view around it, or made sure sudo is passwordless. Only Homebrew casks
    /// take it; any other cask's sudo fails at once instead of asking.
    pub sudo: bool,
}

/// An installed package with a newer version available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outdated {
    /// The name as it was asked about.
    pub name: String,
    pub current: String,
    pub latest: String,
}

impl Outdated {
    pub fn new(
        name: impl Into<String>,
        current: impl Into<String>,
        latest: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            current: current.into(),
            latest: latest.into(),
        }
    }
}

pub trait PackageManager: Send + Sync {
    fn kind(&self) -> PkgManager;

    /// Whether its program is installed. It may not be yet when an earlier bundle of the run
    /// installs it (Homebrew on a new Mac).
    fn available(&self) -> bool;

    /// For each package, whether it is installed. Read-only.
    fn installed(&self, packages: &[String]) -> Result<Vec<bool>>;

    /// Installs `packages` (all missing) without asking anything, streaming the output to `out`.
    fn install(&self, packages: &[String], opts: InstallOpts, out: &mut OutFn<'_>) -> Result<()>;

    /// Brings the package metadata up to date if the manager needs that before installing.
    /// A failure is a warning on `out`, not an error: the install may still work.
    fn refresh(&self, out: &mut OutFn<'_>) -> Result<()> {
        let _ = out;
        Ok(())
    }

    /// Which of `packages` (all installed) have a newer version available, by the metadata the
    /// manager has now (see [`PackageManager::stale`]). Read-only.
    fn outdated(&self, packages: &[String]) -> Result<Vec<Outdated>>;

    /// Upgrades `packages` (installed and outdated) without asking anything.
    fn upgrade(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()>;

    /// Upgrades `packages`, whose installer runs sudo (see [`InstallOpts::sudo`]). Only
    /// Homebrew casks do.
    fn upgrade_with_sudo(&self, packages: &[String], _out: &mut OutFn<'_>) -> Result<()> {
        bail!(
            "{} cannot upgrade {} with sudo free to ask",
            self.kind().as_str(),
            packages.join(", ")
        )
    }

    /// Removes `packages` (installed) without asking anything (`dot remove --packages`).
    fn uninstall(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()>;

    /// Whether [`PackageManager::refresh`] would refresh the metadata now, so that what
    /// [`PackageManager::outdated`] says before it may be out of date (apt's lists are older
    /// than a day). Managers that refresh by themselves say no.
    fn stale(&self) -> bool {
        false
    }

    /// Forgets that the metadata is fresh, so the next [`PackageManager::refresh`] really
    /// refreshes: a repository was added or changed.
    fn invalidate(&self) {}

    /// Whether installing runs as root (through `sudo` when `dot` is not root).
    fn needs_root(&self) -> bool;

    /// The packages the manager knows (installed or not, by its local metadata) whose names
    /// match `pattern` (`*`, `?`; see [`name_matches`]). `None`: this manager cannot list them.
    fn names_matching(&self, pattern: &str) -> Result<Option<Vec<String>>> {
        let _ = pattern;
        Ok(None)
    }
}

/// Whether a package name `name` matches `pattern`: `*` is any run of characters, `?` one.
pub fn name_matches(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some(('*', rest)) => (0..=n.len()).any(|i| go(rest, &n[i..])),
            Some(('?', rest)) => !n.is_empty() && go(rest, &n[1..]),
            Some((c, rest)) => n.first() == Some(c) && go(rest, &n[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
}

/// Whether `name` is a pattern for [`PackageManager::names_matching`].
pub fn is_name_pattern(name: &str) -> bool {
    name.contains(['*', '?'])
}

/// The newest of `names` (which match `pattern`) as versions sort: what the pattern's
/// wildcards matched is compared, runs of digits as numbers (`dotnet-sdk-10.0` after
/// `dotnet-sdk-9.0`). A version (starting with a digit) beats anything else
/// (`dotnet-sdk-10.0` over `dotnet-sdk-dbg-10.0`), and a bare version (digits and dots only)
/// beats a version with more after it, which is another package (`dotnet-sdk-10.0` over
/// `dotnet-sdk-10.0-source-built-artifacts`, and over `dotnet-sdk-11.0-source-built-...` too).
pub fn newest<'a>(pattern: &str, names: &'a [String]) -> Option<&'a String> {
    let prefix = pattern.split(['*', '?']).next().unwrap_or("");
    let suffix = pattern.rsplit(['*', '?']).next().unwrap_or("");
    let key = |n: &'a String| {
        let rest = n.strip_prefix(prefix).unwrap_or(n);
        // What the wildcards matched (a suffix that would overlap the prefix is left).
        let rest = match rest.strip_suffix(suffix) {
            Some(r) if is_name_pattern(pattern) => r,
            _ => rest,
        };
        let version = rest.starts_with(|c: char| c.is_ascii_digit());
        let bare = version && rest.chars().all(|c| c.is_ascii_digit() || c == '.');
        (version, bare, rest)
    };
    names.iter().max_by(|a, b| {
        let ((da, pa, ra), (db, pb, rb)) = (key(a), key(b));
        da.cmp(&db)
            .then(pa.cmp(&pb))
            .then_with(|| natural_cmp(ra, rb))
    })
}

fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn chunks(s: &str) -> Vec<(bool, &str)> {
        let mut out = Vec::new();
        let mut start = 0;
        let bytes = s.as_bytes();
        for i in 1..=bytes.len() {
            if i == bytes.len() || bytes[i].is_ascii_digit() != bytes[start].is_ascii_digit() {
                out.push((bytes[start].is_ascii_digit(), &s[start..i]));
                start = i;
            }
        }
        out
    }
    let (ca, cb) = (chunks(a), chunks(b));
    for (x, y) in ca.iter().zip(&cb) {
        let o = match (x, y) {
            ((true, x), (true, y)) => {
                let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
                x.len().cmp(&y.len()).then_with(|| x.cmp(y))
            }
            ((_, x), (_, y)) => x.cmp(y),
        };
        if o != std::cmp::Ordering::Equal {
            return o;
        }
    }
    ca.len().cmp(&cb.len())
}

/// Overrides the snapd check ([`Packages::snapd_available`]): `1` / `yes` / `true` or `0` /
/// `no` / `false`.
pub const SNAPD_ENV: &str = "DOT_SNAPD";

static LOCK: Mutex<()> = Mutex::new(());

/// The global package-manager lock: hold it while refreshing or installing, so only one package
/// manager runs at a time (apt and dpkg have their own locks and fail rather than wait; brew
/// serialises badly).
pub fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The package managers of a run, sharing a [`Runner`], the state directory (apt's refresh
/// stamp) and a per-run cache of Homebrew cask metadata. Cheap to clone.
#[derive(Clone)]
pub struct Packages {
    inner: Arc<Inner>,
}

struct Inner {
    runner: Arc<dyn Runner>,
    managers: Vec<Arc<dyn PackageManager>>,
    casks: brew::CaskCache,
    /// The run's sudo ([`Packages::with_sudo`]); unset: nobody to ask.
    sudo: OnceLock<Arc<Sudo>>,
}

impl fmt::Debug for Packages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Packages").finish_non_exhaustive()
    }
}

impl Default for Packages {
    fn default() -> Self {
        Self::system(None)
    }
}

impl Packages {
    /// The real machine. `state_dir`: `dot`'s state directory (`~/.local/state/dot`), for apt's
    /// refresh stamp; without one apt refreshes on every run that installs something.
    pub fn system(state_dir: Option<PathBuf>) -> Self {
        let runner: Arc<dyn Runner> = Arc::new(SystemRunner);
        let fetcher = Arc::new(CurlFetcher::new(runner.clone()));
        Self::new(runner, state_dir, fetcher)
    }

    pub fn new(
        runner: Arc<dyn Runner>,
        state_dir: Option<PathBuf>,
        fetcher: Arc<dyn CaskFetcher>,
    ) -> Self {
        let r = || runner.clone();
        let managers: Vec<Arc<dyn PackageManager>> = vec![
            Arc::new(brew::Brew::formula(r())),
            Arc::new(brew::Brew::cask(r())),
            Arc::new(linux::Apt {
                runner: r(),
                state: state_dir.map(StateStore::at),
                attempted: AtomicBool::new(false),
            }),
            Arc::new(linux::Rpm::dnf(r())),
            Arc::new(linux::Pacman(r())),
            Arc::new(linux::Rpm::zypper(r())),
            Arc::new(linux::Apk(r())),
            Arc::new(linux::Snap(r())),
            Arc::new(windows::Winget(r())),
            Arc::new(windows::Scoop(r())),
            Arc::new(vscode::Vscode(r())),
        ];
        Self {
            inner: Arc::new(Inner {
                runner,
                managers,
                casks: brew::CaskCache::new(fetcher),
                sudo: OnceLock::new(),
            }),
        }
    }

    pub fn runner(&self) -> &dyn Runner {
        self.inner.runner.as_ref()
    }

    /// The same packages using the run's sudo (whether a person is there to type a password, and
    /// the terminal's turn). Only the first call counts.
    pub fn with_sudo(self, sudo: Arc<Sudo>) -> Self {
        let _ = self.inner.sudo.set(sudo);
        self
    }

    /// The run's sudo; without [`Packages::with_sudo`], a non-interactive one.
    pub fn sudo(&self) -> Arc<Sudo> {
        self.inner
            .sudo
            .get_or_init(|| Sudo::new(self.inner.runner.clone(), false))
            .clone()
    }

    pub fn manager(&self, m: PkgManager) -> Arc<dyn PackageManager> {
        self.inner
            .managers
            .iter()
            .find(|x| x.kind() == m)
            .cloned()
            .expect("every manager is registered")
    }

    /// Whether snaps can be installed: `snap` is on `PATH` and `snapd` is running (containers
    /// and WSL often have the binary but no daemon). [`SNAPD_ENV`] overrides it (tests, and
    /// sandboxes simulating a machine with or without snapd).
    pub fn snapd_available(&self) -> bool {
        match std::env::var(SNAPD_ENV).as_deref() {
            Ok("1" | "yes" | "true") => return true,
            Ok("0" | "no" | "false") => return false,
            _ => {}
        }
        self.runner().on_path("snap") && self.runner().exists(Path::new(linux::SNAPD_SOCKET))
    }

    /// What Homebrew's API says about cask `token`; fetched once per run. `Err`: why it could
    /// not be found out (offline, a tap cask).
    pub fn cask_info(&self, token: &str) -> Result<CaskInfo, String> {
        self.inner.casks.get(token)
    }
}

/// Runs a read-only query; `ok` lists the exit codes that still mean "answered" (many tools exit
/// 1 when some package is not installed).
fn query(runner: &dyn Runner, cmd: &Cmd, ok: &[i32]) -> Result<Output> {
    let out = runner
        .output(cmd)
        .with_context(|| format!("cannot run `{}`", cmd.display()))?;
    match out.code {
        Some(c) if ok.contains(&c) => Ok(out),
        _ => bail!(
            "`{}` {}{}",
            cmd.display(),
            out.describe_exit(),
            out.last_line()
                .map(|l| format!(": {l}"))
                .unwrap_or_default()
        ),
    }
}

/// Runs an install command, streaming its output; fails unless it exits 0.
fn run(runner: &dyn Runner, cmd: &Cmd, out: &mut OutFn<'_>) -> Result<Output> {
    let o = runner
        .stream(cmd, out)
        .with_context(|| format!("cannot run `{}`", cmd.display()))?;
    if !o.success() {
        bail!(
            "`{}` {}{}",
            cmd.display(),
            o.describe_exit(),
            o.last_line().map(|l| format!(": {l}")).unwrap_or_default()
        );
    }
    Ok(o)
}

/// Fails with a clear message when root is needed and neither we are root nor `sudo` exists.
fn check_root(runner: &dyn Runner, what: &str) -> Result<()> {
    if !runner.is_root() && !runner.on_path("sudo") {
        bail!("{what} needs root: run dot as root, or install sudo");
    }
    Ok(())
}

/// The entries of `found` (`(name, current, latest)` as the tool reports them) that are among
/// `packages`, under the name asked for.
fn pick_outdated(packages: &[String], found: Vec<(String, String, String)>) -> Vec<Outdated> {
    packages
        .iter()
        .filter_map(|p| {
            found
                .iter()
                .find(|(n, _, _)| same_name(p, n))
                .map(|(_, cur, new)| Outdated::new(p.clone(), cur.clone(), new.clone()))
        })
        .collect()
}

/// The rows of a table printed under a line of dashes (scoop, winget), split on whitespace.
fn table_rows(text: &str) -> Vec<Vec<&str>> {
    text.lines()
        .skip_while(|l| !l.trim_start().starts_with("----"))
        .skip(1)
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Whether the tool's name `found` is the package `wanted`: equal, or `wanted`'s last path
/// segment (`owner/tap/fd`, `extras/vscode`), ignoring case.
fn same_name(wanted: &str, found: &str) -> bool {
    let last = wanted.rsplit('/').next().unwrap_or(wanted);
    wanted.eq_ignore_ascii_case(found) || last.eq_ignore_ascii_case(found)
}

/// For each of `packages`, whether it is among `found`.
fn membership(packages: &[String], found: &[String]) -> Vec<bool> {
    packages
        .iter()
        .map(|p| found.iter().any(|f| same_name(p, f)))
        .collect()
}

/// The first whitespace-separated word of each non-empty line.
fn first_words(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests;
