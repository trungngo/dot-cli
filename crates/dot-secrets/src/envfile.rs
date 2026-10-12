//! `envfile`: values in a dotenv file (default `~/.config/dot/secrets.env`), file secrets as
//! plain files under a sibling directory (default `~/.config/dot/files/`).
//!
//! Guardrails, applied on every read and write:
//!
//! - the secrets file and every file secret must not be accessible by group/other (unix; the
//!   error says `chmod 600`); on Windows, by anyone but the user, `SYSTEM` and `Administrators`
//!   (the file's ACL).
//! - neither may sit inside a git work tree unless `git check-ignore` says it is ignored. Without
//!   a `git` binary nothing can be committed, so the check passes.
//!
//! A missing secrets file is not an error: every key in it is simply missing. An empty value
//! (`KEY=`) counts as missing too. Values go through `dotenvy`, so `${VAR}` interpolation works;
//! values written by [`SecretProvider::store`] are quoted so they read back verbatim.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zeroize::Zeroizing;

use crate::config::{EnvfileConfig, SecretsConfig};
use crate::dotenv;
use crate::fsutil::{check_private, write_private};
use crate::git::{Git, GitPlacement};
use crate::reference::{ENVFILE, check_rel_path};
use crate::registry::ProviderEnv;
use crate::scan::read_example_secret_keys;
use crate::{Caps, Interactive, SecretError, SecretProvider, SecretRef, SecretValue};

#[derive(Debug, Clone)]
pub struct EnvfileProvider {
    path: PathBuf,
    files_dir: PathBuf,
    git: Git,
}

impl EnvfileProvider {
    /// `files_dir` defaults to `files/` next to `path`.
    pub fn new(path: impl Into<PathBuf>, files_dir: Option<PathBuf>) -> Self {
        let path = path.into();
        let files_dir = files_dir.unwrap_or_else(|| {
            path.parent()
                .map_or_else(|| PathBuf::from("files"), |p| p.join("files"))
        });
        Self {
            path,
            files_dir,
            git: Git::system(),
        }
    }

    /// From `providers.envfile` in `secrets.yml`, with defaults under `env.config_dir`.
    pub fn from_config(config: Option<&EnvfileConfig>, env: &ProviderEnv) -> Self {
        let path = config
            .and_then(|c| c.path.as_deref())
            .map_or_else(|| env.config_dir.join("secrets.env"), |p| env.expand(p));
        let files_dir = config
            .and_then(|c| c.files_dir.as_deref())
            .map(|p| env.expand(p));
        Self::new(path, files_dir).with_git(env.git.clone())
    }

    pub fn with_git(mut self, git: Git) -> Self {
        self.git = git;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn files_dir(&self) -> &Path {
        &self.files_dir
    }

    /// The git half of the guardrail, for a path that may not exist yet. A symlink is checked
    /// at both ends: the link, and the file it resolves to (a secrets file linked out of an
    /// unignored work tree is one `git add -A` away from being committed).
    fn check_outside_git(&self, path: &Path, what: &str) -> Result<(), String> {
        let target = fs::canonicalize(path).ok().filter(|t| t != path);
        for p in std::iter::once(path).chain(target.as_deref()) {
            match self.git.placement(p) {
                Ok(GitPlacement::NotIgnored) => {
                    let via = if p == path {
                        String::new()
                    } else {
                        format!(" (the target of {})", path.display())
                    };
                    return Err(format!(
                        "{what} {}{via} is inside a git work tree and not ignored; move it out of the repository or add it to .gitignore",
                        p.display()
                    ));
                }
                Ok(_) => {}
                Err(e) => return Err(format!("{what} {}: {e:#}", p.display())),
            }
        }
        Ok(())
    }

    /// Both guardrails for an existing file (for [`crate::transfer`]).
    pub(crate) fn guard_file(&self, path: &Path, what: &str) -> Result<()> {
        self.guard(path, what)
    }

    /// The git guardrail for a path that may not exist yet (for [`crate::transfer`]).
    pub(crate) fn check_git(&self, path: &Path, what: &str) -> Result<()> {
        self.check_outside_git(path, what)
            .map_err(anyhow::Error::msg)
    }

    /// Both guardrails for an existing file.
    fn guard(&self, path: &Path, what: &str) -> Result<()> {
        check_private(path, what).map_err(anyhow::Error::msg)?;
        self.check_outside_git(path, what)
            .map_err(anyhow::Error::msg)
    }

    /// Every non-empty value in the secrets file (empty map when the file does not exist).
    fn load(&self) -> Result<BTreeMap<String, SecretValue>> {
        let mut map = BTreeMap::new();
        if !self.path.exists() {
            return Ok(map);
        }
        self.guard(&self.path, "secrets file")?;
        let iter = dotenvy::from_path_iter(&self.path).map_err(|e| load_error(&self.path, e))?;
        for item in iter {
            let (key, value) = item.map_err(|e| load_error(&self.path, e))?;
            if value.is_empty() {
                map.remove(&key);
            } else {
                map.insert(key, SecretValue::from(value));
            }
        }
        Ok(map)
    }

    fn file_path(&self, rel: &str) -> Result<PathBuf> {
        check_rel_path(rel).map_err(|e| anyhow::anyhow!("envfile-file:{rel}: {e}"))?;
        Ok(self.files_dir.join(rel))
    }

    fn read_file_secret(&self, rel: &str) -> Result<Option<SecretValue>> {
        let path = self.file_path(rel)?;
        if !path.is_file() {
            return Ok(None);
        }
        self.guard(&path, "file secret")?;
        let bytes = fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        Ok(Some(SecretValue::new(bytes)))
    }

    fn check_ref(r: &SecretRef) -> Result<()> {
        if r.provider() != ENVFILE {
            return Err(SecretError::WrongProvider {
                reference: r.to_string(),
                expected: ENVFILE,
                actual: r.provider().to_string(),
            }
            .into());
        }
        Ok(())
    }

    /// `dot secrets check`: what is missing, unused or unsafe. Reports names and paths only,
    /// never values.
    pub fn check(&self, expected: &Expected) -> CheckReport {
        let mut report = CheckReport::default();
        let mut present = BTreeSet::new();
        let mut all_keys = BTreeSet::new();
        if self.path.exists() {
            if let Err(p) = check_private(&self.path, "secrets file") {
                report.problems.push(p);
            }
            if let Err(p) = self.check_outside_git(&self.path, "secrets file") {
                report.problems.push(p);
            }
            match fs::read(&self.path) {
                Ok(bytes) => {
                    let bytes = Zeroizing::new(bytes);
                    let text = Zeroizing::new(String::from_utf8_lossy(&bytes).into_owned());
                    for e in dotenv::entries(&text) {
                        all_keys.insert(e.key.clone());
                        if e.has_value {
                            present.insert(e.key);
                        }
                    }
                    if let Err(e) = dotenvy::from_read_iter(bytes.as_slice())
                        .try_for_each(|item| item.map(drop))
                    {
                        report.problems.push(load_error(&self.path, e).to_string());
                    }
                }
                Err(e) => report
                    .problems
                    .push(format!("cannot read {}: {e}", self.path.display())),
            }
        } else if !expected.keys.is_empty() {
            report.problems.push(format!(
                "secrets file {} does not exist",
                self.path.display()
            ));
        }
        report.missing_keys = expected.keys.difference(&present).cloned().collect();
        report.missing_optional = expected
            .optional_keys
            .difference(&present)
            .cloned()
            .collect();
        report.unused_keys = all_keys
            .iter()
            .filter(|k| !expected.keys.contains(*k) && !expected.optional_keys.contains(*k))
            .cloned()
            .collect();

        let files = expected
            .files
            .iter()
            .map(|f| (f, false))
            .chain(expected.optional_files.iter().map(|f| (f, true)));
        for (rel, optional) in files {
            let path = match self.file_path(rel) {
                Ok(p) => p,
                Err(e) => {
                    report.problems.push(format!("{e:#}"));
                    continue;
                }
            };
            if !path.is_file() {
                if optional {
                    report.missing_optional.push(rel.clone());
                } else {
                    report.missing_files.push(rel.clone());
                }
                continue;
            }
            if let Err(p) = check_private(&path, "file secret") {
                report.problems.push(p);
            }
            if let Err(p) = self.check_outside_git(&path, "file secret") {
                report.problems.push(p);
            }
        }
        report
    }
}

/// A dotenv error without the offending line, which may hold a secret.
fn load_error(path: &Path, e: dotenvy::Error) -> anyhow::Error {
    match e {
        dotenvy::Error::Io(io) => anyhow::anyhow!("cannot read {}: {io}", path.display()),
        dotenvy::Error::EnvVar(v) => anyhow::anyhow!("{}: {v}", path.display()),
        _ => anyhow::anyhow!(
            "{}: malformed dotenv line (contents not shown)",
            path.display()
        ),
    }
}

impl SecretProvider for EnvfileProvider {
    fn scheme(&self) -> &'static str {
        ENVFILE
    }

    fn capabilities(&self) -> Caps {
        Caps {
            read: true,
            write: true,
            files: true,
            write_files: true,
            batch: true,
            needs_unlock: false,
        }
    }

    fn available(&self) -> Result<bool> {
        Ok(true)
    }

    /// Nothing to unlock; runs the guardrails and parses the file so problems surface before
    /// any step runs.
    fn unlock(&mut self, _tty: &mut dyn Interactive) -> Result<()> {
        self.load().map(drop)
    }

    fn resolve(&self, refs: &[SecretRef]) -> Result<Vec<Option<SecretValue>>> {
        self.resolve_each(refs)?
            .into_iter()
            .map(|r| r.map_err(anyhow::Error::msg))
            .collect()
    }

    /// The secrets file is shared by every value ref, so a problem with it (loose mode, inside
    /// git, malformed) fails the whole call. A file secret is its own file: its guardrail
    /// fails only that ref, so one `.ovpn` copied with the wrong mode does not take every
    /// other secret down with it.
    fn resolve_each(&self, refs: &[SecretRef]) -> Result<Vec<crate::RefResult>> {
        refs.iter().try_for_each(Self::check_ref)?;
        let values = if refs.iter().any(|r| !r.is_file()) {
            self.load()?
        } else {
            BTreeMap::new()
        };
        Ok(refs
            .iter()
            .map(|r| {
                if r.is_file() {
                    self.read_file_secret(r.locator())
                        .map_err(|e| format!("{e:#}"))
                } else {
                    Ok(values.get(r.locator()).cloned())
                }
            })
            .collect())
    }

    fn store(&self, r: &SecretRef, v: &SecretValue) -> Result<()> {
        Self::check_ref(r)?;
        if r.is_file() {
            let path = self.file_path(r.locator())?;
            self.check_outside_git(&path, "file secret")
                .map_err(anyhow::Error::msg)?;
            return write_private(&path, v.expose());
        }
        let Some(value) = v.expose_str() else {
            bail!(
                "envfile values must be UTF-8 text; store binary data as a file secret (`envfile-file:`)"
            );
        };
        self.check_outside_git(&self.path, "secrets file")
            .map_err(anyhow::Error::msg)?;
        let current = if self.path.exists() {
            check_private(&self.path, "secrets file").map_err(anyhow::Error::msg)?;
            Zeroizing::new(
                fs::read_to_string(&self.path)
                    .with_context(|| format!("cannot read {}", self.path.display()))?,
            )
        } else {
            Zeroizing::new(String::new())
        };
        let updated = Zeroizing::new(dotenv::upsert(&current, r.locator(), value));
        write_private(&self.path, updated.as_bytes())
    }

    fn location(&self) -> Option<String> {
        Some(self.path.display().to_string())
    }
}

/// What a secrets file is expected to hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expected {
    /// Env keys that must have a value.
    pub keys: BTreeSet<String>,
    /// `envfile-file:` paths (relative to the files directory) that must exist.
    pub files: BTreeSet<String>,
    /// Keys that are only fallbacks: used when present, not missing when absent.
    pub optional_keys: BTreeSet<String>,
    /// File secrets that are only fallbacks.
    pub optional_files: BTreeSet<String>,
}

impl Expected {
    /// Keys marked `# secret` in a `.env.example`.
    pub fn from_example(path: &Path) -> Result<Self> {
        Ok(Self {
            keys: read_example_secret_keys(path)?,
            ..Self::default()
        })
    }

    /// Every `envfile` ref in `secrets.yml`, bare names read with the config's effective
    /// provider: primary refs are expected, fallback refs and the refs of an `optional: true`
    /// secret optional.
    pub fn from_config(config: &SecretsConfig) -> Self {
        let mut out = Self::default();
        for name in config.secrets.keys() {
            let Some(Ok(entry)) = config.entry(name) else {
                continue;
            };
            let refs = std::iter::once((&entry.reference, entry.optional))
                .chain(entry.fallback.as_ref().map(|f| (f, true)));
            for (r, optional) in refs {
                if r.provider() != ENVFILE {
                    continue;
                }
                let set = match (r.is_file(), optional) {
                    (true, false) => &mut out.files,
                    (true, true) => &mut out.optional_files,
                    (false, false) => &mut out.keys,
                    (false, true) => &mut out.optional_keys,
                };
                set.insert(r.locator().to_string());
            }
        }
        out.optional_keys.retain(|k| !out.keys.contains(k));
        out.optional_files.retain(|f| !out.files.contains(f));
        out
    }

    pub fn merge(mut self, other: Self) -> Self {
        self.keys.extend(other.keys);
        self.files.extend(other.files);
        self.optional_keys.extend(other.optional_keys);
        self.optional_files.extend(other.optional_files);
        self.optional_keys.retain(|k| !self.keys.contains(k));
        self.optional_files.retain(|f| !self.files.contains(f));
        self
    }
}

/// Result of [`EnvfileProvider::check`]. Holds names and messages, never values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Expected keys with no (or an empty) value.
    pub missing_keys: Vec<String>,
    /// Expected file secrets that do not exist.
    pub missing_files: Vec<String>,
    /// Keys in the secrets file that nothing expects.
    pub unused_keys: Vec<String>,
    /// Optional keys and file secrets (fallbacks, `optional: true` secrets) that are not
    /// there: a note, not a problem.
    pub missing_optional: Vec<String>,
    /// Guardrail violations and unreadable files.
    pub problems: Vec<String>,
}

impl CheckReport {
    /// No missing secrets and no problems (unused keys are only reported).
    pub fn is_ok(&self) -> bool {
        self.missing_keys.is_empty() && self.missing_files.is_empty() && self.problems.is_empty()
    }
}
