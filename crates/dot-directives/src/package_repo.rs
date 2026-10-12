//! `package_repo`: a vendor's package repository, so that later `package` steps can install
//! from it. Only the entry for this machine's package manager is used (the `pkg` fact, or the
//! one the OS and distro call for), so one step covers several platforms:
//!
//! - apt: the signing key, downloaded, checked against `fingerprint` when one is given (before
//!   anything is installed), dearmored when it is ASCII-armored, installed as
//!   `/etc/apt/keyrings/<name>.gpg`; and a deb822 `/etc/apt/sources.list.d/<name>.sources` with
//!   `Signed-By:` that keyring. A single templated suite (`"{{ codename }}"`) may have a
//!   `fallback_suite`, used when the vendor does not publish the suite (an HTTP HEAD of
//!   `<url>/dists/<suite>/Release` finds nothing): new releases often come before the vendor's
//!   packages for them.
//! - dnf / yum: `/etc/yum.repos.d/<name>.repo`, `gpgcheck` against `gpgkey`.
//! - zypper: `zypper addrepo` (the key imported with `rpm --import` first).
//! - Homebrew: `brew tap`, then `brew trust --tap` where Homebrew has it (untrusted taps are not
//!   loaded).
//! - scoop: `scoop bucket add`; winget: `winget source add` (an elevated terminal).
//!
//! Root-owned files are written to a private temporary directory, then installed with `sudo
//! install -m 0644` (sudo is asked for up front). Applying holds the package-manager lock, and a
//! changed apt repository resets apt's refresh stamp, so the next `package` step refreshes the
//! package lists and sees the new packages.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, bail};
use dot_core::config::{
    AptRepo, BrewTap, DnfRepo, PackageRepoSpec, PkgManager, ScoopBucket, WingetSource,
};
use dot_core::facts::Os;
use dot_core::{Action, Outcome, PlanCtx, Resource, RunCtx, Status, Undo};
use dot_platform::exec::{Cmd, Output, Runner, SystemRunner};
use dot_platform::pkg::repo::{
    self, APT_KEYRING_DIR, APT_SOURCES_DIR, CurlRepoFetcher, Deb822, KeyFormat, RepoFetcher,
    SourceFormat, YUM_REPOS_DIR,
};
use dot_platform::pkg::{self, Packages};

use crate::fileutil::{Existing, change_note, new_file_note, with_notes};
use crate::package::native_manager;

/// How keys are downloaded and suites probed ([`RepoFetcher`]); `curl` by default.
#[derive(Clone)]
pub struct RepoNet(pub Arc<dyn RepoFetcher>);

impl std::fmt::Debug for RepoNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RepoNet")
    }
}

impl Default for RepoNet {
    fn default() -> Self {
        Self(Arc::new(CurlRepoFetcher::new(Arc::new(SystemRunner))))
    }
}

pub fn plan(
    spec: &PackageRepoSpec,
    ctx: &PlanCtx<'_>,
    packages: &Packages,
    net: &RepoNet,
    sysroot: Option<&Path>,
) -> Result<Vec<Box<dyn Action>>> {
    let native = native_manager(ctx.facts);
    let mut actions: Vec<Box<dyn Action>> = Vec::new();
    let env = Env {
        name: spec.name.clone(),
        packages: packages.clone(),
        net: net.clone(),
        sysroot: sysroot.map(Path::to_path_buf),
    };
    match ctx.facts.os {
        Os::Linux => match native {
            Some(PkgManager::Apt) => {
                if let Some(a) = &spec.apt {
                    plan_apt(a, ctx, &env, &mut actions)?;
                }
            }
            Some(PkgManager::Dnf) => {
                if let Some(d) = &spec.dnf {
                    actions.push(plan_dnf(d, ctx, &env)?);
                }
            }
            Some(PkgManager::Zypper) => {
                if let Some(z) = &spec.zypper {
                    let gpgkey = z.gpgkey.as_deref().map(|k| ctx.render(k)).transpose()?;
                    if let Some(k) = &gpgkey {
                        dot_core::config::check_key_url("package_repo: zypper gpgkey", k, true)
                            .map_err(|e| ctx.config_error(e))?;
                    }
                    actions.push(Box::new(ZypperAction {
                        url: ctx.render(&z.url)?,
                        gpgkey,
                        env,
                    }));
                }
            }
            Some(PkgManager::Brew) => {
                if let Some(b) = &spec.brew {
                    actions.push(plan_brew(b, ctx, env)?);
                }
            }
            _ => {}
        },
        Os::Macos => {
            if let Some(b) = &spec.brew {
                actions.push(plan_brew(b, ctx, env)?);
            }
        }
        Os::Windows => {
            if let Some(s) = &spec.scoop {
                actions.push(plan_scoop(s, ctx, env.clone())?);
            }
            if let Some(w) = &spec.winget {
                actions.push(plan_winget(w, ctx, env)?);
            }
        }
    }
    Ok(actions)
}

/// A root-owned file a repository action writes: `file:<path>`, removed through sudo while it
/// holds what `dot` wrote.
fn root_file(path: &Path, show: &str) -> Resource {
    Resource::new("file", show.to_string()).with_undo(Undo::RemoveFiles {
        files: vec![crate::fileutil::written(path)],
        root: true,
    })
}

/// What every repository action shares.
#[derive(Clone)]
struct Env {
    /// The step's `name`.
    name: String,
    packages: Packages,
    net: RepoNet,
    /// `DOT_SYSROOT`: the repository's files are written under it, as this user.
    sysroot: Option<PathBuf>,
}

impl Env {
    fn runner(&self) -> &dyn Runner {
        self.packages.runner()
    }

    /// Where a root-owned file really is: under the sysroot when there is one.
    fn map(&self, path: PathBuf) -> PathBuf {
        match &self.sysroot {
            Some(root) => dot_core::paths::rebase(root, &path),
            None => path,
        }
    }

    fn is_root(&self) -> bool {
        if self.sysroot.is_some() {
            // Under a sysroot everything is this user's: nothing goes through sudo.
            return true;
        }
        self.runner().is_root()
    }

    fn root(&self, cmd: Cmd) -> Cmd {
        cmd.as_root(self.is_root())
    }

    /// Runs `cmd`, its output shown as the step's; fails unless it exits 0.
    fn run(&self, cmd: &Cmd, ctx: &RunCtx) -> Result<Output> {
        let o = self
            .runner()
            .stream(cmd, &mut |s, l| ctx.output(s, l))
            .with_context(|| format!("cannot run `{}`", cmd.line()))?;
        if !o.success() {
            return Err(o.failure(cmd));
        }
        Ok(o)
    }

    /// A read-only query; `None` when its program is not installed (yet).
    fn query(&self, cmd: &Cmd) -> Result<Option<Output>> {
        match self.runner().output(cmd) {
            Ok(o) => Ok(Some(o)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("cannot run `{}`", cmd.line())),
        }
    }

    /// Writes `content` to root-owned `dest` (mode 0644, resource `file:<show>`): into a
    /// private temporary directory, then `install` as root, creating the directory first when
    /// it is missing.
    fn install_root_file(
        &self,
        content: &[u8],
        dest: &Path,
        show: &str,
        ctx: &RunCtx,
    ) -> Result<()> {
        let dir = tempfile::tempdir().context("creating a temporary directory")?;
        let tmp = dir.path().join("file");
        std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
        self.install_root_copy(&tmp, dest, show, ctx)
    }

    /// Installs `src` as root-owned `dest` (see [`Env::install_root_file`]). A file already
    /// there is saved first, the first time `dot` writes over it, so `dot remove` puts it back.
    fn install_root_copy(&self, src: &Path, dest: &Path, show: &str, ctx: &RunCtx) -> Result<()> {
        self.preserve(dest, show, ctx)?;
        if let Some(parent) = dest.parent()
            && !parent.is_dir()
        {
            self.run(&self.root(repo::install_dir(parent)), ctx)?;
        }
        self.run(&self.root(repo::install_file(src, dest, 0o644)), ctx)?;
        Ok(())
    }

    /// Hands what is at root-owned `dest` (read through `sudo -n cat` when this user may not
    /// read it) to [`RunCtx::preserve_root`].
    fn preserve(&self, dest: &Path, show: &str, ctx: &RunCtx) -> Result<()> {
        let (bytes, mode) = match Existing::read(dest) {
            Ok(Existing::File { bytes, mode }) => (bytes, mode),
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let cat = self.root(Cmd::new("cat").arg(dest.display().to_string()));
                match self.query(&cat)? {
                    Some(o) if o.success() => (
                        o.stdout.into_bytes(),
                        std::fs::symlink_metadata(dest)
                            .ok()
                            .and_then(|m| crate::fileutil::mode_of(&m)),
                    ),
                    _ => bail!("cannot read {show} (even through sudo) to save it first"),
                }
            }
            Err(e) => return Err(e).with_context(|| format!("reading {show}")),
        };
        ctx.preserve_root(&format!("file:{show}"), dest, &bytes, mode.unwrap_or(0o644))
    }

    fn sudo_note(&self) -> Option<String> {
        (!self.is_root()).then(|| "sudo".to_string())
    }

    /// After a change to an apt (or dnf) repository: the next `package` step refreshes.
    fn invalidate(&self, m: PkgManager) {
        self.packages.manager(m).invalidate();
    }
}

// ---------------------------------------------------------------------------------------------
// apt

/// The machine's apt architecture: what `dpkg --print-architecture` says (an amd64 userland
/// on an arm64 kernel, or `dot` built for another CPU than the packages), else the `arch`
/// fact's Debian name. A simulated `arch` (`DOT_FACTS=arch=...`, not this binary's) wins over
/// the host's dpkg.
fn apt_architecture(ctx: &PlanCtx<'_>, env: &Env) -> Option<String> {
    let from_fact = || repo::dpkg_arch(ctx.facts.arch).map(str::to_string);
    if ctx.facts.arch != std::env::consts::ARCH || !env.runner().on_path("dpkg") {
        return from_fact();
    }
    let asked = env
        .query(&Cmd::new("dpkg").arg("--print-architecture"))
        .ok()
        .flatten()
        .filter(Output::success)
        .map(|o| o.stdout.trim().to_string())
        .filter(|a| {
            !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        });
    asked.or_else(from_fact)
}

fn plan_apt(
    a: &AptRepo,
    ctx: &PlanCtx<'_>,
    env: &Env,
    actions: &mut Vec<Box<dyn Action>>,
) -> Result<()> {
    let name = &env.name;
    let codename_unknown = |t: &str| t.contains("codename") && ctx.facts.codename.is_none();
    let mut suites = Vec::new();
    for s in &a.suites.0 {
        if codename_unknown(s) {
            return Err(ctx.config_error(format!(
                "package_repo {name}: the `codename` fact is unknown on this machine (no \
                 VERSION_CODENAME in /etc/os-release); name the suite"
            )));
        }
        suites.push(word(ctx, &format!("apt suite `{s}`"), s)?);
    }
    let fallback = a
        .fallback_suite
        .as_deref()
        .map(|f| word(ctx, "apt fallback_suite", f))
        .transpose()?
        .filter(|f| suites.first() != Some(f));
    let keyring = match &a.keyring {
        Some(k) => ctx.target(k)?,
        None => Path::new(APT_KEYRING_DIR).join(format!("{name}.gpg")),
    };
    let file = match &a.file {
        Some(f) => ctx.target(f)?,
        None => Path::new(APT_SOURCES_DIR).join(format!("{name}.sources")),
    };
    // apt reads the format from the extension (checked as written unless templated).
    let key_format = KeyFormat::of(&keyring).ok_or_else(|| {
        ctx.config_error(format!(
            "package_repo {name}: apt keyring {} must end in `.gpg` or `.asc`",
            keyring.display()
        ))
    })?;
    let format = SourceFormat::of(&file).ok_or_else(|| {
        ctx.config_error(format!(
            "package_repo {name}: apt file {} must end in `.sources` or `.list`",
            file.display()
        ))
    })?;
    let mut replaces = Vec::new();
    for r in &a.replaces {
        let path = ctx.target(r)?;
        if path.parent() != file.parent() || SourceFormat::of(&path).is_none() || path == file {
            return Err(ctx.config_error(format!(
                "package_repo {name}: apt `replaces` may only name other `.list` / `.sources` \
                 files in {} (got {})",
                file.parent().unwrap_or(Path::new("/")).display(),
                path.display()
            )));
        }
        replaces.push(path);
    }
    let architectures = match &a.architectures {
        Some(list) => list
            .iter()
            .map(|x| word(ctx, "apt architecture", x))
            .collect::<Result<_>>()?,
        None => apt_architecture(ctx, env).into_iter().collect(),
    };
    let components = match &a.components {
        Some(list) => list
            .iter()
            .map(|c| word(ctx, "apt component", c))
            .collect::<Result<_>>()?,
        None => vec!["main".to_string()],
    };
    let source = Deb822 {
        name: name.clone(),
        url: word(ctx, "apt url", &a.url)?,
        suites,
        components,
        architectures,
        signed_by: a.key.is_some().then(|| keyring.display().to_string()),
    };
    // Written where they really go: under `DOT_SYSROOT` when set (the source still names the
    // keyring's real path, as apt will read it).
    let keyring = env.map(keyring);
    let file = env.map(file);
    let replaces: Vec<PathBuf> = replaces.into_iter().map(|r| env.map(r)).collect();
    if let Some(key) = &a.key {
        let url = word(ctx, "apt key", key)?;
        dot_core::config::check_key_url("package_repo: apt key", &url, false)
            .map_err(|e| ctx.config_error(e))?;
        let mut fingerprints = Vec::new();
        for f in a.fingerprint.iter().flat_map(|f| &f.0) {
            let f = ctx.render(f)?;
            dot_core::config::check_fingerprint(&f).map_err(|e| ctx.config_error(e))?;
            fingerprints.push(repo::normalize_fingerprint(&f));
        }
        let fingerprint = (!fingerprints.is_empty()).then_some(fingerprints);
        actions.push(Box::new(KeyringAction {
            url,
            fingerprint,
            format: key_format,
            show: ctx.show(&keyring),
            path: keyring,
            env: env.clone(),
        }));
    }
    actions.push(Box::new(AptSourceAction {
        show: ctx.show(&file),
        path: file,
        source,
        format,
        fallback,
        if_missing: a.if_missing,
        replaces,
        env: env.clone(),
        probe: Mutex::new(None),
    }));
    Ok(())
}

/// A rendered value that must be one word (a URL, a suite).
fn word(ctx: &PlanCtx<'_>, what: &str, text: &str) -> Result<String> {
    let v = ctx.render(text)?;
    if v.trim().is_empty() || v.chars().any(char::is_whitespace) {
        return Err(ctx.config_error(format!(
            "package_repo: {what} renders to `{v}`, which is not one word"
        )));
    }
    Ok(v)
}

/// The repository's signing key in `/etc/apt/keyrings`.
pub struct KeyringAction {
    url: String,
    /// The pinned keys, normalized.
    fingerprint: Option<Vec<String>>,
    /// What the keyring's extension tells apt it holds.
    format: KeyFormat,
    path: PathBuf,
    show: String,
    env: Env,
}

/// What is known about the installed keyring.
enum KeyState {
    Missing,
    Good,
    /// Present, but not exactly the pinned keys: why ([`repo::check_pinned_keys`]).
    Wrong(String),
    /// Present; its fingerprint cannot be checked (no gpg yet).
    Unchecked,
}

/// A private directory for `gpg --homedir`.
fn gpg_home(dir: &Path) -> Result<PathBuf> {
    let home = dir.join("gnupg");
    std::fs::create_dir(&home).with_context(|| format!("creating {}", home.display()))?;
    crate::fileutil::set_mode(&home, 0o700).ok();
    Ok(home)
}

impl KeyringAction {
    fn state(&self) -> Result<KeyState> {
        if !self.path.exists() {
            return Ok(KeyState::Missing);
        }
        let Some(want) = &self.fingerprint else {
            return Ok(KeyState::Good);
        };
        if !self.env.runner().on_path("gpg") {
            return Ok(KeyState::Unchecked);
        }
        let dir = tempfile::tempdir().context("creating a temporary directory")?;
        let keys = repo::key_list(self.env.runner(), &gpg_home(dir.path())?, &self.path)
            .unwrap_or_default();
        Ok(match repo::check_pinned_keys(want, &keys) {
            Ok(()) => KeyState::Good,
            Err(why) => KeyState::Wrong(why),
        })
    }
}

impl Action for KeyringAction {
    fn describe(&self) -> String {
        let base = format!("apt keyring {} <- {}", self.show, self.url);
        let mut notes: Vec<String> = Vec::new();
        let pending = match self.state() {
            Ok(KeyState::Good) => return format!("apt keyring {}", self.show),
            Ok(KeyState::Unchecked) => {
                return format!(
                    "apt keyring {} (fingerprint not checked: gpg is not installed)",
                    self.show
                );
            }
            Ok(KeyState::Missing) => true,
            Ok(KeyState::Wrong(why)) => {
                notes.push(format!(
                    "the installed keyring is not the pinned key ({why}): replaced"
                ));
                true
            }
            Err(e) => {
                notes.push(format!("cannot inspect: {e:#}"));
                true
            }
        };
        if self.fingerprint.is_some() {
            notes.push("fingerprint checked before installing".into());
        } else {
            notes.push("not pinned: no `fingerprint`".into());
        }
        if pending {
            notes.extend(self.env.sudo_note());
        }
        with_notes(base, &notes)
    }

    fn status(&self) -> Result<Status> {
        Ok(match self.state()? {
            KeyState::Good | KeyState::Unchecked => Status::Satisfied,
            KeyState::Missing | KeyState::Wrong(_) => Status::Pending,
        })
    }

    /// `file:<keyring>`, removed as root.
    fn resources(&self) -> Vec<Resource> {
        vec![root_file(&self.path, &self.show)]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let _one_at_a_time = pkg::lock();
        let dir = tempfile::tempdir().context("creating a temporary directory")?;
        let downloaded = dir.path().join("key.download");
        self.env.net.0.download(&self.url, &downloaded)?;
        let bytes = std::fs::read(&downloaded)
            .with_context(|| format!("reading the key downloaded from {}", self.url))?;
        let gpg = self.env.runner().on_path("gpg");
        let armored = repo::is_armored(&bytes);
        let key = if self.format == KeyFormat::Armored {
            // apt reads a `.asc` keyring as armored: installed as downloaded.
            if !armored {
                bail!(
                    "the key at {} is binary, and apt reads {} as ASCII-armored; name the \
                     keyring `.gpg` instead",
                    self.url,
                    self.show
                );
            }
            downloaded
        } else if armored {
            if !gpg {
                bail!(
                    "the key at {} is ASCII-armored and gpg is not installed to convert it; \
                     install gnupg first (`package: [gnupg]`)",
                    self.url
                );
            }
            let out = dir.path().join("key.gpg");
            let home = gpg_home(dir.path())?;
            self.env
                .run(&repo::gpg_dearmor(&home, &downloaded, &out), ctx)?;
            out
        } else {
            downloaded
        };
        if let Some(want) = &self.fingerprint {
            if !gpg {
                bail!(
                    "gpg is needed to check the fingerprint of the key at {}; install gnupg \
                     first (`package: [gnupg]`)",
                    self.url
                );
            }
            let home = dir.path().join("gnupg");
            if !home.exists() {
                gpg_home(dir.path())?;
            }
            let keys = repo::key_list(self.env.runner(), &home, &key)?;
            if let Err(why) = repo::check_pinned_keys(want, &keys) {
                bail!(
                    "the key at {} is not the pinned key {}: {why}; refusing to install it",
                    self.url,
                    want.join(" / ")
                );
            }
        }
        self.env
            .install_root_copy(&key, &self.path, &self.show, ctx)?;
        self.env.invalidate(PkgManager::Apt);
        Ok(Outcome::Changed)
    }

    fn needs_sudo(&self) -> bool {
        !self.env.is_root()
    }
}

/// The repository's sources file: deb822 (`.sources`) or one-line (`.list`).
pub struct AptSourceAction {
    path: PathBuf,
    show: String,
    source: Deb822,
    format: SourceFormat,
    /// Used when the vendor does not publish the (single) suite.
    fallback: Option<String>,
    if_missing: bool,
    /// Other sources files (in the same directory) removed when the source is written.
    replaces: Vec<PathBuf>,
    env: Env,
    /// Whether the suite is published, once asked.
    probe: Mutex<Option<Result<bool, String>>>,
}

/// The content the sources file should have, and why that suite.
struct Wanted {
    content: Option<String>,
    note: Option<String>,
}

impl AptSourceAction {
    fn published(&self, suite: &str) -> Result<bool, String> {
        let mut p = self.probe.lock().unwrap_or_else(|e| e.into_inner());
        p.get_or_insert_with(|| {
            self.env
                .net
                .0
                .exists(&repo::release_url(&self.source.url, suite))
        })
        .clone()
    }

    /// What the file should hold, given what it holds now. `content: None`: cannot be told
    /// (the suite probe failed and the file is not already the fallback).
    fn wanted(&self, existing: Option<&[u8]>) -> Wanted {
        let primary = self.source.render_as(self.format);
        let Some(fallback) = &self.fallback else {
            return Wanted {
                content: Some(primary),
                note: None,
            };
        };
        if existing == Some(primary.as_bytes()) {
            return Wanted {
                content: Some(primary),
                note: None,
            };
        }
        let suite = &self.source.suites[0];
        let fb = self.source.with_suite(fallback).render_as(self.format);
        match self.published(suite) {
            Ok(true) => Wanted {
                content: Some(primary),
                note: None,
            },
            Ok(false) => Wanted {
                content: Some(fb),
                note: Some(format!(
                    "suite {fallback}: the repository has nothing for {suite}"
                )),
            },
            Err(e) if existing == Some(fb.as_bytes()) => Wanted {
                content: Some(fb),
                note: Some(format!(
                    "suite {fallback}; whether the repository has {suite} yet is unknown ({e})"
                )),
            },
            Err(e) => Wanted {
                content: None,
                note: Some(format!(
                    "cannot tell whether the repository has suite {suite} ({e})"
                )),
            },
        }
    }

    fn existing(&self) -> Result<Existing> {
        Existing::read(&self.path).with_context(|| format!("inspecting {}", self.show))
    }

    /// Other sources files that list this repository (URI and a suite, the fallback included),
    /// split into those `replaces:` removes and those in the way.
    fn others(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut suites = self.source.suites.clone();
        suites.extend(self.fallback.iter().cloned());
        let dups = repo::duplicate_sources(
            &self.source.url,
            &suites,
            &repo::other_source_files(&self.path),
        );
        dups.into_iter().partition(|p| self.replaces.contains(p))
    }

    /// The `replaces:` files that are there, to remove.
    fn to_remove(&self) -> Vec<&PathBuf> {
        self.replaces
            .iter()
            .filter(|p| std::fs::symlink_metadata(p).is_ok())
            .collect()
    }

    fn duplicate_note(&self, blocking: &[PathBuf]) -> String {
        format!(
            "conflict: {} also list{} {}; list {} under `replaces:` to remove {}",
            blocking
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            if blocking.len() == 1 { "s" } else { "" },
            self.source.url,
            if blocking.len() == 1 { "it" } else { "them" },
            if blocking.len() == 1 { "it" } else { "them" },
        )
    }
}

impl Action for AptSourceAction {
    fn describe(&self) -> String {
        let base = format!("apt source {}", self.show);
        let existing = match self.existing() {
            Ok(e) => e,
            Err(e) => return with_notes(base, &[format!("{e:#}")]),
        };
        if self.if_missing && matches!(existing, Existing::File { .. }) {
            return format!("{base} (exists; kept as it is)");
        }
        let w = self.wanted(existing.bytes());
        let mut notes: Vec<String> = w.note.into_iter().collect();
        let (_, blocking) = self.others();
        if !blocking.is_empty() {
            notes.push(self.duplicate_note(&blocking));
        }
        let removes = self.to_remove();
        if !removes.is_empty() {
            notes.push(format!(
                "removes {}",
                removes
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let pending = !removes.is_empty();
        let pending = pending
            | match (&existing, &w.content) {
                (_, None) => true,
                (Existing::File { bytes, .. }, Some(c)) if bytes == c.as_bytes() => false,
                (Existing::File { bytes, .. }, Some(c)) => {
                    notes.push(change_note(bytes, c.as_bytes()));
                    true
                }
                (Existing::Missing, Some(c)) => {
                    notes.push(new_file_note(c.as_bytes()));
                    true
                }
                (Existing::Dir, _) => {
                    notes.push("conflict: a directory is in the way".into());
                    false
                }
                (_, Some(_)) => {
                    notes.push("replaces what is there".into());
                    true
                }
            };
        if pending && blocking.is_empty() {
            notes.extend(self.env.sudo_note());
        }
        with_notes(base, &notes)
    }

    fn status(&self) -> Result<Status> {
        let existing = self.existing()?;
        if matches!(existing, Existing::Dir) {
            return Ok(Status::Conflict);
        }
        if self.if_missing && matches!(existing, Existing::File { .. }) {
            return Ok(Status::Satisfied);
        }
        if !self.others().1.is_empty() {
            return Ok(Status::Conflict);
        }
        let removes = !self.to_remove().is_empty();
        Ok(match self.wanted(existing.bytes()).content {
            None => Status::Unknown,
            Some(c) if existing.bytes() == Some(c.as_bytes()) && !removes => Status::Satisfied,
            Some(_) => Status::Pending,
        })
    }

    /// `file:<sources file>`, removed as root.
    fn resources(&self) -> Vec<Resource> {
        vec![root_file(&self.path, &self.show)]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let _one_at_a_time = pkg::lock();
        let existing = self.existing()?;
        if matches!(existing, Existing::Dir) {
            bail!("{} is a directory", self.show);
        }
        if self.if_missing && matches!(existing, Existing::File { .. }) {
            return Ok(Outcome::Ok);
        }
        let (_, blocking) = self.others();
        if !blocking.is_empty() {
            bail!("{}", self.duplicate_note(&blocking));
        }
        let w = self.wanted(existing.bytes());
        let Some(content) = w.content else {
            bail!(
                "{}",
                w.note
                    .unwrap_or_else(|| "cannot tell which suite to use".to_string())
            );
        };
        let removes = self.to_remove();
        if existing.bytes() == Some(content.as_bytes()) && removes.is_empty() {
            return Ok(Outcome::Ok);
        }
        if let Some(n) = &w.note {
            ctx.output(dot_core::Stream::Stdout, n.clone());
        }
        // The old entries go first: with both in place apt reads no sources at all.
        for p in removes {
            let rm = Cmd::new("rm").args(["-f".to_string(), p.display().to_string()]);
            self.env.run(&self.env.root(rm), ctx)?;
        }
        if existing.bytes() != Some(content.as_bytes()) {
            self.env
                .install_root_file(content.as_bytes(), &self.path, &self.show, ctx)?;
        }
        self.env.invalidate(PkgManager::Apt);
        Ok(Outcome::Changed)
    }

    fn needs_sudo(&self) -> bool {
        !self.env.is_root()
    }
}

// ---------------------------------------------------------------------------------------------
// dnf

fn plan_dnf(d: &DnfRepo, ctx: &PlanCtx<'_>, env: &Env) -> Result<Box<dyn Action>> {
    let name = &env.name;
    let path = env.map(match &d.file {
        Some(f) => ctx.target(f)?,
        None => Path::new(YUM_REPOS_DIR).join(format!("{name}.repo")),
    });
    let description = match &d.description {
        Some(t) => ctx.render(t)?,
        None => name.clone(),
    };
    let gpgkey = d
        .gpgkey
        .as_deref()
        .map(|k| word(ctx, "dnf gpgkey", k))
        .transpose()?;
    if let Some(k) = &gpgkey {
        dot_core::config::check_key_url("package_repo: dnf gpgkey", k, true)
            .map_err(|e| ctx.config_error(e))?;
    }
    let content = repo::dnf_repo_file(
        name,
        &description,
        &word(ctx, "dnf baseurl", &d.baseurl)?,
        gpgkey.as_deref(),
    );
    Ok(Box::new(DnfRepoAction {
        show: ctx.show(&path),
        path,
        content,
        unsigned: gpgkey.is_none(),
        if_missing: d.if_missing,
        env: env.clone(),
    }))
}

pub struct DnfRepoAction {
    path: PathBuf,
    show: String,
    content: String,
    unsigned: bool,
    if_missing: bool,
    env: Env,
}

impl DnfRepoAction {
    fn existing(&self) -> Result<Existing> {
        Existing::read(&self.path).with_context(|| format!("inspecting {}", self.show))
    }
}

impl Action for DnfRepoAction {
    fn describe(&self) -> String {
        let base = format!("dnf repository {}", self.show);
        let mut notes = Vec::new();
        if self.unsigned {
            notes.push("unsigned: no gpgkey".to_string());
        }
        match self.existing() {
            Ok(Existing::File { .. }) if self.if_missing => {
                return format!("{base} (exists; kept as it is)");
            }
            Ok(Existing::File { bytes, .. }) if bytes == self.content.as_bytes() => {}
            Ok(Existing::File { bytes, .. }) => {
                notes.push(change_note(&bytes, self.content.as_bytes()));
                notes.extend(self.env.sudo_note());
            }
            Ok(Existing::Missing) => {
                notes.push(new_file_note(self.content.as_bytes()));
                notes.extend(self.env.sudo_note());
            }
            Ok(_) => notes.push("conflict: something else is in the way".into()),
            Err(e) => notes.push(format!("{e:#}")),
        }
        with_notes(base, &notes)
    }

    fn status(&self) -> Result<Status> {
        Ok(match self.existing()? {
            Existing::File { .. } if self.if_missing => Status::Satisfied,
            Existing::File { bytes, .. } if bytes == self.content.as_bytes() => Status::Satisfied,
            Existing::File { .. } | Existing::Missing => Status::Pending,
            _ => Status::Conflict,
        })
    }

    /// `file:<.repo file>`, removed as root.
    fn resources(&self) -> Vec<Resource> {
        vec![root_file(&self.path, &self.show)]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let _one_at_a_time = pkg::lock();
        match self.existing()? {
            Existing::File { .. } if self.if_missing => return Ok(Outcome::Ok),
            Existing::File { bytes, .. } if bytes == self.content.as_bytes() => {
                return Ok(Outcome::Ok);
            }
            Existing::File { .. } | Existing::Missing => {}
            _ => bail!("{} is not a regular file", self.show),
        }
        self.env
            .install_root_file(self.content.as_bytes(), &self.path, &self.show, ctx)?;
        self.env.invalidate(PkgManager::Dnf);
        Ok(Outcome::Changed)
    }

    fn needs_sudo(&self) -> bool {
        !self.env.is_root()
    }
}

// ---------------------------------------------------------------------------------------------
// zypper

pub struct ZypperAction {
    url: String,
    gpgkey: Option<String>,
    env: Env,
}

impl ZypperAction {
    /// The URL the alias points at now (`Some(None)`: no such repository); `None` when zypper
    /// is not installed.
    fn current(&self) -> Result<Option<Option<String>>> {
        let cmd = Cmd::new("zypper").args(["--non-interactive", "repos", "--uri"]);
        let Some(out) = self.env.query(&cmd)? else {
            return Ok(None);
        };
        if !out.success() {
            return Err(out.failure(&cmd));
        }
        Ok(Some(
            repo::parse_zypper_repos(&out.stdout)
                .into_iter()
                .find(|(alias, _)| *alias == self.env.name)
                .map(|(_, uri)| uri),
        ))
    }
}

impl Action for ZypperAction {
    fn describe(&self) -> String {
        let base = format!("zypper repository {} ({})", self.env.name, self.url);
        match self.current() {
            Ok(Some(Some(u))) if u.trim_end_matches('/') == self.url.trim_end_matches('/') => base,
            Ok(Some(Some(u))) => with_notes(
                base,
                &[format!("replaces {u}")]
                    .into_iter()
                    .chain(self.env.sudo_note())
                    .collect::<Vec<_>>(),
            ),
            Ok(Some(None)) => {
                let mut notes = vec!["add".to_string()];
                if self.gpgkey.is_some() {
                    notes.push("imports its key".into());
                }
                notes.extend(self.env.sudo_note());
                with_notes(base, &notes)
            }
            Ok(None) => with_notes(base, &["zypper is not installed".into()]),
            Err(e) => with_notes(base, &[format!("cannot check: {e:#}")]),
        }
    }

    fn status(&self) -> Result<Status> {
        Ok(match self.current()? {
            None => Status::Unknown,
            Some(Some(u)) if u.trim_end_matches('/') == self.url.trim_end_matches('/') => {
                Status::Satisfied
            }
            Some(_) => Status::Pending,
        })
    }

    /// `repo:zypper:<alias>`: removed by hand.
    fn resources(&self) -> Vec<Resource> {
        vec![
            Resource::new("repo", format!("zypper:{}", self.env.name)).with_undo(Undo::Manual {
                note: format!("sudo zypper removerepo {}", self.env.name),
            }),
        ]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let _one_at_a_time = pkg::lock();
        let current = self.current()?.context("zypper is not installed")?;
        if let Some(k) = &self.gpgkey {
            self.env.run(
                &self
                    .env
                    .root(Cmd::new("rpm").args(["--import", k.as_str()])),
                ctx,
            )?;
        }
        if current.is_some() {
            let cmd = Cmd::new("zypper").args([
                "--non-interactive",
                "removerepo",
                self.env.name.as_str(),
            ]);
            self.env.run(&self.env.root(cmd), ctx)?;
        }
        let cmd = Cmd::new("zypper").args([
            "--non-interactive",
            "addrepo",
            "--refresh",
            self.url.as_str(),
            self.env.name.as_str(),
        ]);
        self.env.run(&self.env.root(cmd), ctx)?;
        Ok(Outcome::Changed)
    }

    fn needs_sudo(&self) -> bool {
        !self.env.is_root()
    }
}

// ---------------------------------------------------------------------------------------------
// Homebrew, scoop, winget

fn plan_brew(b: &BrewTap, ctx: &PlanCtx<'_>, env: Env) -> Result<Box<dyn Action>> {
    Ok(Box::new(BrewTapAction {
        tap: ctx.render(&b.tap)?,
        url: b.url.as_deref().map(|u| ctx.render(u)).transpose()?,
        trust: b.trust.unwrap_or(true),
        env,
    }))
}

pub struct BrewTapAction {
    tap: String,
    url: Option<String>,
    trust: bool,
    env: Env,
}

/// Where a tap stands.
struct TapState {
    tapped: bool,
    /// `None`: not asked, or this Homebrew has no `brew trust`.
    trusted: Option<bool>,
}

impl BrewTapAction {
    fn brew(&self) -> Cmd {
        // Tapping never needs root: Homebrew (7.0.7 and later) then leaves the sudo timestamp
        // alone.
        Cmd::new("brew")
            .env("NONINTERACTIVE", "1")
            .env("HOMEBREW_NO_ENV_HINTS", "1")
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .env("HOMEBREW_NO_SUDO", "1")
    }

    /// `None`: Homebrew is not installed (yet).
    fn state(&self) -> Result<Option<TapState>> {
        let cmd = self.brew().arg("tap");
        let Some(out) = self.env.query(&cmd)? else {
            return Ok(None);
        };
        if !out.success() {
            return Err(out.failure(&cmd));
        }
        let want = self.tap.to_ascii_lowercase();
        let tapped = repo::parse_brew_taps(&out.stdout).contains(&want);
        let trusted = if self.trust {
            let cmd = self.brew().args(["trust", "--json", "v1"]);
            match self.env.query(&cmd)? {
                Some(o) if o.success() => repo::parse_trusted_taps(&o.stdout)
                    .ok()
                    .map(|t| t.contains(&want)),
                // No `brew trust` in this Homebrew: nothing to trust.
                _ => None,
            }
        } else {
            None
        };
        Ok(Some(TapState { tapped, trusted }))
    }
}

impl Action for BrewTapAction {
    fn describe(&self) -> String {
        let base = format!("brew tap {}", self.tap);
        match self.state() {
            Ok(Some(s)) => {
                let mut todo = Vec::new();
                if !s.tapped {
                    todo.push(match &self.url {
                        Some(u) => format!("tap from {u}"),
                        None => "tap".to_string(),
                    });
                }
                if s.trusted == Some(false) {
                    todo.push("brew trust --tap".into());
                }
                if todo.is_empty() {
                    base
                } else {
                    with_notes(base, &todo)
                }
            }
            Ok(None) => with_notes(base, &["Homebrew is not installed yet".into()]),
            Err(e) => with_notes(base, &[format!("cannot check: {e:#}")]),
        }
    }

    fn status(&self) -> Result<Status> {
        Ok(match self.state()? {
            None => Status::Unknown,
            Some(s) if s.tapped && s.trusted != Some(false) => Status::Satisfied,
            Some(_) => Status::Pending,
        })
    }

    /// `repo:brew:<tap>`: untapped by hand (formulae from it may still be installed).
    fn resources(&self) -> Vec<Resource> {
        vec![
            Resource::new("repo", format!("brew:{}", self.tap)).with_undo(Undo::Manual {
                note: format!("brew untap {}", self.tap),
            }),
        ]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        if self.env.is_root() {
            bail!("Homebrew does not run as root: run dot as your own user");
        }
        let _one_at_a_time = pkg::lock();
        let s = self.state()?.context("Homebrew is not installed")?;
        let mut changed = false;
        if !s.tapped {
            let cmd = self
                .brew()
                .args(["tap", self.tap.as_str()])
                .args(self.url.iter().cloned());
            self.env.run(&cmd, ctx)?;
            changed = true;
        }
        // Asked again after tapping: a new tap is not trusted.
        let trusted = if changed {
            self.state()?.and_then(|s| s.trusted)
        } else {
            s.trusted
        };
        if trusted == Some(false) {
            self.env.run(
                &self.brew().args(["trust", "--tap", self.tap.as_str()]),
                ctx,
            )?;
            changed = true;
        }
        Ok(if changed {
            Outcome::Changed
        } else {
            Outcome::Ok
        })
    }
}

fn plan_scoop(s: &ScoopBucket, ctx: &PlanCtx<'_>, env: Env) -> Result<Box<dyn Action>> {
    Ok(Box::new(ListedAction {
        what: "scoop bucket",
        item: ctx.render(&s.bucket)?,
        list: Cmd::new("scoop").args(["bucket", "list"]),
        add: {
            let bucket = ctx.render(&s.bucket)?;
            Cmd::new("scoop")
                .args(["bucket", "add", bucket.as_str()])
                .args(s.url.as_deref().map(|u| ctx.render(u)).transpose()?)
        },
        hint: None,
        admin: false,
        env,
    }))
}

fn plan_winget(w: &WingetSource, ctx: &PlanCtx<'_>, env: Env) -> Result<Box<dyn Action>> {
    let source = ctx.render(&w.source)?;
    let mut add = Cmd::new("winget").args([
        "source".to_string(),
        "add".to_string(),
        "--name".to_string(),
        source.clone(),
        "--arg".to_string(),
        ctx.render(&w.url)?,
    ]);
    if let Some(t) = &w.kind {
        add = add.args(["--type".to_string(), ctx.render(t)?]);
    }
    add = add.args(["--accept-source-agreements", "--disable-interactivity"]);
    Ok(Box::new(ListedAction {
        what: "winget source",
        item: source,
        // `source list` takes no `--accept-source-agreements` (only `source add` and the
        // install/search commands do): winget rejects it as an unknown argument.
        list: Cmd::new("winget").args(["source", "list", "--disable-interactivity"]),
        add,
        hint: Some("adding a winget source needs an elevated (Administrator) terminal"),
        admin: true,
        env,
    }))
}

/// Something a manager lists by name in a table (scoop buckets, winget sources) and adds with
/// one command.
pub struct ListedAction {
    what: &'static str,
    item: String,
    list: Cmd,
    add: Cmd,
    hint: Option<&'static str>,
    /// Adding needs administrator rights (winget sources): an elevated terminal, checked up
    /// front ([`Action::needs_sudo`]).
    admin: bool,
    env: Env,
}

impl ListedAction {
    /// `None`: the manager is not installed (yet).
    fn present(&self) -> Result<Option<bool>> {
        let Some(out) = self.env.query(&self.list)? else {
            return Ok(None);
        };
        if !out.success() {
            return Err(out.failure(&self.list));
        }
        Ok(Some(
            repo::parse_table_names(&out.stdout).contains(&self.item.to_ascii_lowercase()),
        ))
    }
}

impl Action for ListedAction {
    fn describe(&self) -> String {
        let base = format!("{} {}", self.what, self.item);
        match self.present() {
            Ok(Some(true)) => base,
            Ok(Some(false)) => {
                let mut notes = vec!["add".to_string()];
                if self.needs_sudo() {
                    notes.push("needs an elevated terminal".into());
                }
                with_notes(base, &notes)
            }
            Ok(None) => with_notes(
                base,
                &[format!("{} is not installed yet", self.list.program)],
            ),
            Err(e) => with_notes(base, &[format!("cannot check: {e:#}")]),
        }
    }

    fn status(&self) -> Result<Status> {
        Ok(match self.present()? {
            None => Status::Unknown,
            Some(true) => Status::Satisfied,
            Some(false) => Status::Pending,
        })
    }

    /// `repo:<what>:<name>`: removed by hand.
    fn resources(&self) -> Vec<Resource> {
        vec![
            Resource::new("repo", format!("{}:{}", self.what, self.item)).with_undo(Undo::Manual {
                note: format!("remove the {} {} by hand", self.what, self.item),
            }),
        ]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let _one_at_a_time = pkg::lock();
        if self.present()? == Some(true) {
            return Ok(Outcome::Ok);
        }
        self.env.run(&self.add, ctx).map_err(|e| match self.hint {
            Some(h) => e.context(h),
            None => e,
        })?;
        Ok(Outcome::Changed)
    }

    /// winget sources need an elevated terminal, unless this one is.
    fn needs_sudo(&self) -> bool {
        self.admin && !self.env.is_root()
    }
}

#[cfg(all(test, unix))]
mod tests;
