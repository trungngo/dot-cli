//! `dot`'s state store: one versioned JSON file, `state.json`, in the state directory
//! ([`crate::paths::state_dir`]).
//!
//! It holds:
//!
//! - `resources`: what the engine recorded about each [`crate::Resource`] a real run applied
//!   (the bundle that manages it, whether `dot` made or changed it, how to undo it and what to
//!   put back): `dot status` finds orphans in it, `dot remove` undoes from it;
//! - `env`: what `path:` / `env:` steps set ([`crate::runenv::EnvRecord`]);
//! - `sections`: the data of other crates, by name ([`sections`]): link fallbacks, installed
//!   GitHub releases, apt's last refresh, units whose reload / restart is still pending.
//!
//! Writes go through [`StateStore::update`]: an exclusive lock on `state.lock` (an OS file
//! lock, so it is released when a process dies), then read, change, and an atomic replace
//! (temporary file + rename, owner-only). Two `dot` processes, or two threads of one, never
//! lose each other's changes, and a reader never sees half a file. A whole run also holds the
//! run lock ([`StateStore::run_lock`]), so two `dot apply` runs never interleave.
//!
//! Schema version 1. Version 0 was one file per concern (`links.json`, `github-releases.json`,
//! `env.json`, `apt-update.stamp`, `services/`): the first read of a state directory that has
//! them and no `state.json` migrates them in and moves them to `legacy/`. A `state.json` from a
//! newer `dot` is never written to.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::resource::{Resource, Undo};
use crate::runenv::RunEnv;

pub const STATE_FILE: &str = "state.json";
pub const SCHEMA_VERSION: u32 = 1;
/// Held while the state file is read, changed and written.
pub const LOCK_FILE: &str = "state.lock";
/// Held for a whole `dot apply` / `dot remove` / `dot adopt`.
pub const RUN_LOCK_FILE: &str = "run.lock";
/// Who holds the run lock (pid, command, start time); removed when the run ends.
pub const RUN_INFO_FILE: &str = "run.json";
/// Where files are saved before `dot` first writes over them.
pub const BACKUP_DIR: &str = "backups";
/// Where version-0 files go once migrated.
pub const LEGACY_DIR: &str = "legacy";
/// Where a `state.json` that cannot be parsed is moved before a fresh one is started.
pub const CORRUPT_SUFFIX: &str = ".corrupt";

/// Section names, and the version-0 files they were migrated from.
pub mod sections {
    /// Link fallbacks (junctions, copies): `links.json`.
    pub const LINKS: &str = "links";
    /// Installed GitHub releases: `github-releases.json`.
    pub const GITHUB_RELEASES: &str = "github_releases";
    /// apt's last successful refresh: `apt-update.stamp`.
    pub const APT: &str = "apt";
    /// Units whose reload / restart is pending: `services/`.
    pub const SERVICES: &str = "services";
    /// Handlers still due, per bundle ([`crate::handlers`]).
    pub const HANDLERS: &str = "handlers";
    /// What `download` steps downloaded, by destination.
    pub const DOWNLOADS: &str = "downloads";
    /// Bundles applied by name that the profile does not select (`dot apply docker`): part of
    /// this machine's configuration for `dot status` until `dot remove`.
    pub const OPTED_IN: &str = "opted_in";

    pub const LEGACY_LINKS: &str = "links.json";
    pub const LEGACY_GITHUB_RELEASES: &str = "github-releases.json";
    pub const LEGACY_ENV: &str = "env.json";
    pub const LEGACY_APT: &str = "apt-update.stamp";
    pub const LEGACY_SERVICES: &str = "services";
}

/// Something `dot` put in place, as recorded when a real run applied it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The bundle whose step manages it (the last one to apply it).
    pub bundle: String,
    pub kind: String,
    /// When it was first recorded, in recording order: `dot remove` undoes newest first.
    pub seq: u64,
    /// `dot` made or changed it. `false`: it was already as the config wants when `dot` first
    /// met it, so `dot remove` leaves it.
    pub owned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo: Option<Undo>,
    /// What to put back once undone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<Restore>,
}

/// What was there before `dot`, to put back when a resource is removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum Restore {
    /// Moved aside (`.dot-backup`) or saved in the state directory: moved back.
    Backup { path: PathBuf },
    /// `dot adopt` moved the file into the repository: a copy is put back.
    Copy { src: PathBuf },
    /// A root-owned file saved in the state directory before `dot` first wrote over it (a
    /// `root: true` entry): put back as root, with its `mode`.
    RootBackup { path: PathBuf, mode: u32 },
}

/// The contents of `state.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    /// By resource key (`link:~/.zshrc`).
    #[serde(default)]
    pub resources: BTreeMap<String, Record>,
    #[serde(default, skip_serializing_if = "RunEnv::is_empty")]
    pub env: RunEnv,
    #[serde(default)]
    pub sections: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub next_seq: u64,
}

impl State {
    pub fn new() -> Self {
        Self {
            version: SCHEMA_VERSION,
            ..Self::default()
        }
    }

    /// A section's data; missing or malformed counts as empty (the worst outcome is that
    /// something is done once more).
    pub fn section<T: DeserializeOwned + Default>(&self, name: &str) -> T {
        self.sections
            .get(name)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    pub fn set_section<T: Serialize>(&mut self, name: &str, value: &T) {
        match serde_json::to_value(value) {
            Ok(serde_json::Value::Null) => {
                self.sections.remove(name);
            }
            Ok(v) if v.as_object().is_some_and(|o| o.is_empty()) => {
                self.sections.remove(name);
            }
            Ok(v) => {
                self.sections.insert(name.to_string(), v);
            }
            Err(_) => {}
        }
    }

    /// Records that a real run applied resource `key` for `bundle`. `changed`: applying changed
    /// it. A new record is owned when `changed`; an existing one moves to `bundle`, takes the
    /// latest `undo`, becomes owned once `dot` changes it, and keeps what it restores.
    pub fn record(
        &mut self,
        key: &str,
        bundle: &str,
        kind: &str,
        undo: Option<Undo>,
        changed: bool,
        restore: Option<Restore>,
    ) {
        match self.resources.get_mut(key) {
            Some(r) => {
                r.bundle = bundle.to_string();
                r.kind = kind.to_string();
                let mut undo = undo;
                if let (Some(new), Some(old)) = (undo.as_mut(), r.undo.as_ref()) {
                    new.carry_from(old);
                }
                r.undo = undo;
                r.owned |= changed;
                if r.restore.is_none() {
                    r.restore = restore;
                }
            }
            None => {
                let seq = self.next_seq;
                self.next_seq += 1;
                self.resources.insert(
                    key.to_string(),
                    Record {
                        bundle: bundle.to_string(),
                        kind: kind.to_string(),
                        seq,
                        owned: changed,
                        undo,
                        restore,
                    },
                );
            }
        }
    }

    /// `bundle`'s records, newest first.
    pub fn records_of(&self, bundle: &str) -> Vec<(String, Record)> {
        let mut v: Vec<(String, Record)> = self
            .resources
            .iter()
            .filter(|(_, r)| r.bundle == bundle)
            .map(|(k, r)| (k.clone(), r.clone()))
            .collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.1.seq));
        v
    }

    /// Whether `dot` has not written the resource yet, so what is there now is the user's (to
    /// save before the first write).
    pub fn first_write(&self, key: &str) -> bool {
        self.resources
            .get(key)
            .is_none_or(|r| !r.owned && r.restore.is_none())
    }
}

/// Why the state could not be read.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{path}: {message}")]
    Corrupt { path: PathBuf, message: String },
    #[error(
        "{path} has schema version {found}, but this dot knows version {SCHEMA_VERSION} at \
         most: update dot (dot self-update, or install.sh)"
    )]
    Newer { path: PathBuf, found: u32 },
    #[error("cannot read {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

/// The state store of one state directory. Cheap to clone; every call goes to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateStore {
    dir: PathBuf,
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl StateStore {
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `state.json`.
    pub fn path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }

    pub fn backup_dir(&self) -> PathBuf {
        self.dir.join(BACKUP_DIR)
    }

    /// Reads the state. No `state.json`: the migrated version-0 files when there are any (and
    /// they are migrated on disk when the directory is writable), else an empty state.
    pub fn load(&self) -> Result<State, StateError> {
        match self.read_file()? {
            Some(s) => Ok(s),
            None if !self.has_legacy() => Ok(State::new()),
            None => {
                // Migrated under the lock, unless another process just did it.
                match self.lock() {
                    Ok(_held) => match self.read_file()? {
                        Some(s) => Ok(s),
                        None => Ok(self.migrate()),
                    },
                    Err(_) => Ok(self.legacy_state()),
                }
            }
        }
    }

    /// [`StateStore::load`], with anything unreadable counting as nothing recorded.
    pub fn read(&self) -> State {
        self.load().unwrap_or_default()
    }

    /// The bundles applied by name that the profile does not select ([`sections::OPTED_IN`]).
    pub fn opted_in(&self) -> std::collections::BTreeSet<String> {
        self.section(sections::OPTED_IN)
    }

    /// Adds (`on`) or drops bundles from [`StateStore::opted_in`].
    pub fn set_opted_in(&self, bundles: &[String], on: bool) -> Result<()> {
        if bundles.is_empty() {
            return Ok(());
        }
        self.update(|s| {
            let mut set: std::collections::BTreeSet<String> = s.section(sections::OPTED_IN);
            for b in bundles {
                if on {
                    set.insert(b.clone());
                } else {
                    set.remove(b);
                }
            }
            s.set_section(sections::OPTED_IN, &set);
        })
    }

    /// A section's data (see [`State::section`]).
    pub fn section<T: DeserializeOwned + Default>(&self, name: &str) -> T {
        self.read().section(name)
    }

    /// Reads, changes and writes the state under the lock. A `state.json` that cannot be parsed
    /// is moved aside (`state.json.corrupt`) and a fresh one started; one from a newer `dot` is
    /// an error. Nothing is written when `f` changes nothing.
    pub fn update<R>(&self, f: impl FnOnce(&mut State) -> R) -> Result<R> {
        let _held = self.lock()?;
        let mut state = match self.read_file() {
            Ok(Some(s)) => s,
            Ok(None) if self.has_legacy() => self.migrate(),
            Ok(None) => State::new(),
            Err(StateError::Corrupt { .. }) => {
                let path = self.path();
                let aside = PathBuf::from(format!("{}{CORRUPT_SUFFIX}", path.display()));
                fs::rename(&path, &aside)
                    .with_context(|| format!("moving the unreadable {} aside", path.display()))?;
                State::new()
            }
            Err(e) => return Err(e.into()),
        };
        let before = state.clone();
        let r = f(&mut state);
        state.version = SCHEMA_VERSION;
        if state != before || !self.path().exists() {
            self.write(&state)?;
        }
        Ok(r)
    }

    /// [`StateStore::update`] of one section.
    pub fn update_section<T, R>(&self, name: &str, f: impl FnOnce(&mut T) -> R) -> Result<R>
    where
        T: DeserializeOwned + Serialize + Default,
    {
        self.update(|s| {
            let mut v: T = s.section(name);
            let r = f(&mut v);
            s.set_section(name, &v);
            r
        })
    }

    /// The state lock, held until the guard is dropped. Blocks while another process (or
    /// thread) holds it.
    fn lock(&self) -> Result<File> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.dir.join(LOCK_FILE);
        let f = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        f.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(f)
    }

    fn read_file(&self) -> Result<Option<State>, StateError> {
        let path = self.path();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StateError::Io { path, source }),
        };
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| StateError::Corrupt {
                path: path.clone(),
                message: e.to_string(),
            })?;
        let found = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        if found > SCHEMA_VERSION {
            return Err(StateError::Newer { path, found });
        }
        serde_json::from_value(value)
            .map(Some)
            .map_err(|e| StateError::Corrupt {
                path,
                message: e.to_string(),
            })
    }

    fn write(&self, state: &State) -> Result<()> {
        let path = self.path();
        let text = serde_json::to_string_pretty(state)?;
        // Owner-only: `env` values may be sensitive.
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)
            .with_context(|| format!("writing {}", path.display()))?;
        tmp.write_all(text.as_bytes())?;
        tmp.as_file().sync_all()?;
        let mut tmp = tmp.into_temp_path();
        // Windows refuses to replace a file another process has open for a moment.
        for attempt in 0.. {
            match tmp.persist(&path) {
                Ok(()) => return Ok(()),
                Err(e) if attempt < 20 && e.error.kind() == io::ErrorKind::PermissionDenied => {
                    tmp = e.path;
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(e) => {
                    return Err(e.error).with_context(|| format!("writing {}", path.display()));
                }
            }
        }
        unreachable!()
    }

    // -----------------------------------------------------------------------------------------
    // what the engine records

    /// Right before an action applies: saves what is at each of its `preserve` paths that `dot`
    /// has not written yet (a regular file, the user's) into `backups/`, owner-only. Returns
    /// what to restore, by resource key, for [`StateStore::record_applied`].
    pub fn preserve(&self, resources: &[Resource]) -> Result<Vec<(String, Restore)>> {
        let wanted: Vec<(&Resource, &PathBuf)> = resources
            .iter()
            .filter_map(|r| r.preserve.as_ref().map(|p| (r, p)))
            .filter(|(_, p)| fs::symlink_metadata(p).is_ok_and(|m| m.is_file()))
            .collect();
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let state = self.read();
        let mut out = Vec::new();
        for (r, path) in wanted {
            let key = r.key();
            if !state.first_write(&key) {
                continue;
            }
            let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let backup = self.save_backup(path, &bytes)?;
            out.push((key, Restore::Backup { path: backup }));
        }
        Ok(out)
    }

    /// Saves `bytes`, what is at `path` now, in `backups/` (owner-only); returns where.
    pub fn save_backup(&self, path: &Path, bytes: &[u8]) -> Result<PathBuf> {
        let dir = self.backup_dir();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let backup = dir.join(format!("{nanos}-{name}"));
        write_private(&backup, bytes)
            .with_context(|| format!("saving {} to {}", path.display(), backup.display()))?;
        Ok(backup)
    }

    /// Records the resources of an action a real run applied for `bundle`. `changed`: the
    /// action's outcome was Changed (a resource may say otherwise). `restores`: what
    /// [`StateStore::preserve`] saved or the action moved aside, by resource key.
    pub fn record_applied(
        &self,
        bundle: &str,
        resources: &[Resource],
        changed: bool,
        restores: &[(String, Restore)],
    ) -> Result<()> {
        if resources.is_empty() {
            return Ok(());
        }
        self.update(|st| {
            for r in resources {
                let key = r.key();
                let restore = restores
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, r)| r.clone());
                st.record(
                    &key,
                    bundle,
                    r.kind,
                    r.undo.clone(),
                    r.changed.unwrap_or(changed),
                    restore,
                );
            }
        })
    }

    // -----------------------------------------------------------------------------------------
    // version 0

    fn legacy_paths(&self) -> [PathBuf; 5] {
        use sections::*;
        [
            LEGACY_LINKS,
            LEGACY_GITHUB_RELEASES,
            LEGACY_ENV,
            LEGACY_APT,
            LEGACY_SERVICES,
        ]
        .map(|n| self.dir.join(n))
    }

    fn has_legacy(&self) -> bool {
        self.legacy_paths().iter().any(|p| p.exists())
    }

    /// The version-0 files as a state, without changing anything. Unreadable parts are skipped.
    fn legacy_state(&self) -> State {
        use sections::*;
        let mut s = State::new();
        let json = |name: &str| -> Option<serde_json::Value> {
            serde_json::from_str(&fs::read_to_string(self.dir.join(name)).ok()?).ok()
        };
        if let Some(links) = json(LEGACY_LINKS).and_then(|v| v.get("links").cloned()) {
            s.sections.insert(LINKS.into(), links);
        }
        if let Some(installs) =
            json(LEGACY_GITHUB_RELEASES).and_then(|v| v.get("installs").cloned())
        {
            s.sections.insert(GITHUB_RELEASES.into(), installs);
        }
        if let Some(env) = json(LEGACY_ENV).and_then(|v| serde_json::from_value(v).ok()) {
            s.env = env;
        }
        if let Some(secs) = fs::read_to_string(self.dir.join(LEGACY_APT))
            .ok()
            .and_then(|t| t.trim().parse::<u64>().ok())
        {
            s.sections
                .insert(APT.into(), serde_json::json!({ "refreshed": secs }));
        }
        if let Ok(entries) = fs::read_dir(self.dir.join(LEGACY_SERVICES)) {
            let mut markers = serde_json::Map::new();
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let dest = fs::read_to_string(e.path()).unwrap_or_default();
                markers.insert(name, serde_json::Value::String(dest));
            }
            if !markers.is_empty() {
                s.sections
                    .insert(SERVICES.into(), serde_json::Value::Object(markers));
            }
        }
        s
    }

    /// Migrates the version-0 files (the lock is held): writes `state.json`, then moves them
    /// to `legacy/`. When that cannot be written the migrated state is still returned.
    fn migrate(&self) -> State {
        let s = self.legacy_state();
        if self.write(&s).is_ok() {
            let legacy = self.dir.join(LEGACY_DIR);
            if fs::create_dir_all(&legacy).is_ok() {
                for p in self.legacy_paths() {
                    if let Some(name) = p.file_name() {
                        let to = legacy.join(name);
                        if to.exists() {
                            let _ = fs::remove_dir_all(&to).or_else(|_| fs::remove_file(&to));
                        }
                        let _ = fs::rename(&p, &to);
                    }
                }
            }
        }
        s
    }

    // -----------------------------------------------------------------------------------------
    // the run lock

    /// Takes the run lock for `command`, or says who holds it. Released when the guard drops
    /// (or the process dies).
    pub fn run_lock(&self, command: &str) -> Result<RunLock> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.dir.join(RUN_LOCK_FILE);
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                let who = self
                    .run_info()
                    .map(|i| format!(" ({})", i.describe()))
                    .unwrap_or_default();
                bail!(
                    "another dot run is in progress{who}; wait for it to finish (the lock is {})",
                    path.display()
                );
            }
            Err(fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }
        let info = RunInfo {
            pid: std::process::id(),
            command: command.to_string(),
            started: now_secs(),
        };
        let info_path = self.dir.join(RUN_INFO_FILE);
        fs::write(&info_path, serde_json::to_string(&info)?)
            .with_context(|| format!("writing {}", info_path.display()))?;
        Ok(RunLock {
            _file: file,
            info: info_path,
        })
    }

    fn run_info(&self) -> Option<RunInfo> {
        serde_json::from_str(&fs::read_to_string(self.dir.join(RUN_INFO_FILE)).ok()?).ok()
    }

    /// Whether a run holds the run lock, or one left its record behind without finishing
    /// (killed, crashed): the lock itself dies with its process, so that is only a leftover.
    pub fn run_lock_status(&self) -> RunLockStatus {
        let path = self.dir.join(RUN_LOCK_FILE);
        let Ok(file) = File::options().write(true).open(&path) else {
            return match self.run_info() {
                Some(i) => RunLockStatus::Stale(i),
                None => RunLockStatus::Free,
            };
        };
        match file.try_lock() {
            Err(fs::TryLockError::WouldBlock) => RunLockStatus::Held(self.run_info()),
            _ => match self.run_info() {
                Some(i) => RunLockStatus::Stale(i),
                None => RunLockStatus::Free,
            },
        }
    }

    /// Removes a leftover run record ([`RunLockStatus::Stale`]).
    pub fn clear_stale_run(&self) -> Result<()> {
        match fs::remove_file(self.dir.join(RUN_INFO_FILE)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(anyhow!(e)),
            _ => Ok(()),
        }
    }
}

/// Writes a new file readable by its owner only.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut opts = File::options();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Who holds (or held) the run lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunInfo {
    pub pid: u32,
    pub command: String,
    pub started: u64,
}

impl RunInfo {
    pub fn describe(&self) -> String {
        let ago = now_secs().saturating_sub(self.started);
        format!(
            "`dot {}`, pid {}, started {} ago",
            self.command,
            self.pid,
            human_secs(ago)
        )
    }
}

fn human_secs(s: u64) -> String {
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunLockStatus {
    Free,
    Held(Option<RunInfo>),
    Stale(RunInfo),
}

/// The run lock; released on drop.
#[derive(Debug)]
pub struct RunLock {
    _file: File,
    info: PathBuf,
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.info);
    }
}

#[cfg(test)]
mod tests;
