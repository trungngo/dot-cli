//! The run environment: `PATH` entries and variables that `path:` / `env:` steps add for the
//! later steps of the same run, and the record of them that `dot env --shell` turns into a
//! sourceable script ([`crate::shellenv`]).
//!
//! A `path:` or `env:` step records its entries while it is *planned* (an in-memory change, not
//! a change to the machine), so later steps see them both in the up-front plan (a `shell`
//! `unless:` check that needs the new `PATH`) and while applying (the engine plans each step again
//! right before it runs, in order). Each pass of the engine gets its own [`RunEnvCell`] from
//! [`RunEnvCell::fresh`]; the real run records into [`crate::Context::run_env`], which the CLI
//! saves afterwards ([`EnvRecord`]).
//!
//! Status is judged against what earlier real runs recorded ([`EnvRecord`], in the state store, which
//! `dot env --shell` reads): an entry recorded with the same value is satisfied, whatever the
//! environment `dot` happens to be started with. The environment `dot` was started with
//! ([`BaseEnv`]) is only what later steps' commands inherit underneath the run's entries.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::state::StateStore;

/// Where a `path:` directory goes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathPosition {
    /// In front of the existing `PATH` (wins over it).
    #[default]
    Prepend,
    /// After the existing `PATH`.
    Append,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathEntry {
    pub dir: PathBuf,
    pub position: PathPosition,
    /// The bundle whose step added it.
    pub bundle: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
    pub bundle: String,
}

/// `PATH` entries and variables, in the order they take effect.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEnv {
    /// Prepended directories first (the first one wins), then appended ones.
    #[serde(default)]
    pub path: Vec<PathEntry>,
    #[serde(default)]
    pub vars: Vec<EnvVar>,
}

impl RunEnv {
    pub fn is_empty(&self) -> bool {
        self.path.is_empty() && self.vars.is_empty()
    }

    /// Adds a directory. A prepended one goes in front of every earlier one (as a later
    /// `export PATH=dir:$PATH` would), an appended one after. A directory already listed moves.
    pub fn add_path(&mut self, entry: PathEntry) {
        self.path.retain(|e| e.dir != entry.dir);
        match entry.position {
            PathPosition::Prepend => self.path.insert(0, entry),
            PathPosition::Append => self.path.push(entry),
        }
    }

    /// Whether `dir` is listed at `position`.
    pub fn has_path(&self, dir: &Path, position: PathPosition) -> bool {
        let want = crate::paths::normalize(dir);
        self.path
            .iter()
            .any(|e| e.position == position && crate::paths::normalize(&e.dir) == want)
    }

    /// The value of variable `name`, if set.
    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|v| v.name == name)
            .map(|v| v.value.as_str())
    }

    /// Sets a variable; a later value for the same name replaces the earlier one.
    pub fn set_var(&mut self, var: EnvVar) {
        match self.vars.iter_mut().find(|v| v.name == var.name) {
            Some(v) => *v = var,
            None => self.vars.push(var),
        }
    }

    /// The `PATH` a child process gets: our prepended directories, the base `PATH` (without
    /// entries we list ourselves), then our appended ones. `None` when we add nothing.
    pub fn path_value(&self, base: Option<&OsStr>) -> Option<OsString> {
        if self.path.is_empty() {
            return None;
        }
        let ours: Vec<&Path> = self.path.iter().map(|e| e.dir.as_path()).collect();
        let mut dirs: Vec<PathBuf> = self
            .path
            .iter()
            .filter(|e| e.position == PathPosition::Prepend)
            .map(|e| e.dir.clone())
            .collect();
        if let Some(base) = base {
            dirs.extend(std::env::split_paths(base).filter(|d| !ours.contains(&d.as_path())));
        }
        dirs.extend(
            self.path
                .iter()
                .filter(|e| e.position == PathPosition::Append)
                .map(|e| e.dir.clone()),
        );
        Some(std::env::join_paths(&dirs).unwrap_or_else(|_| {
            // A directory containing the separator cannot be listed; leave it out rather than
            // corrupt the rest.
            let ok: Vec<&PathBuf> = dirs
                .iter()
                .filter(|d| std::env::join_paths([d]).is_ok())
                .collect();
            std::env::join_paths(ok).unwrap_or_default()
        }))
    }

    /// The variables a child process gets on top of what it inherits: ours, then `PATH` when
    /// directories were added.
    pub fn child_env(&self, base_path: Option<&OsStr>) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .vars
            .iter()
            .map(|v| (v.name.clone(), v.value.clone()))
            .collect();
        if let Some(p) = self.path_value(base_path) {
            out.push(("PATH".into(), p.to_string_lossy().into_owned()));
        }
        out
    }
}

/// The environment variable listing directories (`PATH` syntax) that stay ahead of every
/// `path:` entry in the `PATH` of the commands a run starts: a test harness's fake tools
/// (`test/fakebin`) are never shadowed by a real `/opt/homebrew/bin` a bundle prepends.
pub const PATH_FIRST_ENV: &str = "DOT_PATH_FIRST";

/// The environment `dot` was started with. Statuses are judged against it; nothing here is ever
/// changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BaseEnv {
    pub path: Option<OsString>,
    pub vars: BTreeMap<String, String>,
    /// [`PATH_FIRST_ENV`]: directories kept first in every child's `PATH`.
    pub first: Vec<PathBuf>,
}

impl BaseEnv {
    /// A snapshot of this process's environment.
    pub fn from_process() -> Self {
        Self {
            path: std::env::var_os("PATH"),
            vars: std::env::vars().collect(),
            first: std::env::var_os(PATH_FIRST_ENV)
                .map(|v| {
                    std::env::split_paths(&v)
                        .filter(|d| !d.as_os_str().is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Whether `dir` is one of the `PATH` entries.
    pub fn has_path_dir(&self, dir: &Path) -> bool {
        let want = crate::paths::normalize(dir);
        self.path
            .as_deref()
            .is_some_and(|p| std::env::split_paths(p).any(|d| crate::paths::normalize(&d) == want))
    }

    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }
}

/// A shared, thread-safe [`RunEnv`] plus the [`BaseEnv`] it is layered on, and what earlier runs
/// recorded (statuses are judged against that). Cloning shares it.
#[derive(Clone)]
pub struct RunEnvCell {
    base: Arc<BaseEnv>,
    recorded: Arc<RunEnv>,
    env: Arc<Mutex<RunEnv>>,
    /// Directories an installer added to the system's `PATH` during the run (Windows: winget
    /// and scoop write the registry), appended for later commands; never recorded.
    system: Arc<Mutex<Vec<PathBuf>>>,
    /// The bundles whose entries [`RunEnvCell::child_env`] passes on; `None`: every bundle's.
    visible: Option<Arc<BTreeSet<String>>>,
}

impl Default for RunEnvCell {
    /// Based on this process's environment.
    fn default() -> Self {
        Self::new(BaseEnv::from_process())
    }
}

impl std::fmt::Debug for RunEnvCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunEnvCell")
            .field("env", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl RunEnvCell {
    pub fn new(base: BaseEnv) -> Self {
        Self {
            base: Arc::new(base),
            recorded: Arc::default(),
            env: Arc::default(),
            system: Arc::default(),
            visible: None,
        }
    }

    /// The same cell with `recorded` as what earlier runs recorded (the `env.json` record).
    pub fn with_recorded(mut self, recorded: RunEnv) -> Self {
        self.recorded = Arc::new(recorded);
        self
    }

    /// An empty run environment over the same base and record (one per engine pass).
    pub fn fresh(&self) -> Self {
        Self {
            base: self.base.clone(),
            recorded: self.recorded.clone(),
            env: Arc::default(),
            system: self.system.clone(),
            visible: None,
        }
    }

    /// The system's `PATH` as it is now (`dirs`, e.g. re-read from the Windows registry after a
    /// winget install): the ones this run does not know yet are appended to later commands'
    /// `PATH`, in every bundle, and never recorded (the system keeps them itself).
    pub fn add_system_path(&self, dirs: Vec<PathBuf>) {
        let known = |d: &PathBuf| {
            self.base.has_path_dir(d)
                || self
                    .system
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(d)
        };
        let new: Vec<PathBuf> = dirs.into_iter().filter(|d| !known(d)).collect();
        self.system
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(new);
    }

    /// The same run environment (entries still go to the shared record), whose commands only
    /// see what `bundles` added: a bundle sees the `path:` / `env:` entries of the bundles it
    /// `requires` or runs `after` (transitively) and its own, so what it sees does not depend
    /// on which unrelated bundles `--jobs N` happened to finish first.
    pub fn visible_to(&self, bundles: BTreeSet<String>) -> Self {
        Self {
            visible: Some(Arc::new(bundles)),
            ..self.clone()
        }
    }

    pub fn base(&self) -> &BaseEnv {
        &self.base
    }

    /// What earlier real runs recorded ([`EnvRecord`]); empty when nothing was loaded.
    pub fn recorded(&self) -> &RunEnv {
        &self.recorded
    }

    pub fn snapshot(&self) -> RunEnv {
        self.env.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn update(&self, f: impl FnOnce(&mut RunEnv)) {
        f(&mut self.env.lock().unwrap_or_else(|e| e.into_inner()));
    }

    pub fn clear(&self) {
        self.update(|e| *e = RunEnv::default());
    }

    /// What a child process started now should get on top of what it inherits.
    pub fn child_env(&self) -> Vec<(String, String)> {
        let mut env = self.snapshot();
        if let Some(visible) = &self.visible {
            env.path.retain(|e| visible.contains(&e.bundle));
            env.vars.retain(|v| visible.contains(&v.bundle));
        }
        let mut out = env.child_env(self.base.path.as_deref());
        let system = self
            .system
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !system.is_empty() {
            let current = match out.iter().find(|(k, _)| k == "PATH") {
                Some((_, p)) => Some(OsString::from(p)),
                None => self.base.path.clone(),
            };
            let mut dirs: Vec<PathBuf> = current
                .as_deref()
                .map(|p| std::env::split_paths(p).collect())
                .unwrap_or_default();
            for d in system {
                if !dirs.contains(&d) {
                    dirs.push(d);
                }
            }
            if let Ok(v) = std::env::join_paths(&dirs) {
                let v = v.to_string_lossy().into_owned();
                out.retain(|(k, _)| k != "PATH");
                out.push(("PATH".into(), v));
            }
        }
        if !self.base.first.is_empty()
            && let Some((_, path)) = out.iter_mut().find(|(k, _)| k == "PATH")
        {
            let rest = std::env::split_paths(OsStr::new(path.as_str()))
                .filter(|d| !self.base.first.contains(d))
                .collect::<Vec<_>>();
            if let Ok(v) = std::env::join_paths(self.base.first.iter().chain(&rest)) {
                *path = v.to_string_lossy().into_owned();
            }
        }
        out
    }
}

thread_local! {
    /// The run environment of the bundle this thread is planning or applying
    /// ([`RunEnvCell::enter`]).
    static CURRENT: RefCell<Option<RunEnvCell>> = const { RefCell::new(None) };
}

/// Restores the thread's previous run environment when dropped ([`RunEnvCell::enter`]).
#[must_use = "the run environment is left again when the guard is dropped"]
pub struct Entered(Option<RunEnvCell>);

impl Drop for Entered {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|c| *c.borrow_mut() = previous);
    }
}

impl RunEnvCell {
    /// Makes this the run environment of the commands the current thread looks up and starts
    /// (see [`current_path`]) until the guard is dropped: the engine enters each bundle's while
    /// planning and applying it, so a program a `path:` step made reachable (Homebrew's
    /// `/opt/homebrew/bin/brew` right after its installer ran) is found by the directives that
    /// run it themselves (`package`, `package_repo`, ...), not only by `shell` steps.
    pub fn enter(&self) -> Entered {
        Entered(CURRENT.with(|c| c.borrow_mut().replace(self.clone())))
    }
}

/// The `PATH` to look programs up in on this thread: what [`RunEnvCell::child_env`] gives a
/// child of the entered run environment, `None` when none is entered or it adds no directory
/// (then this process's own `PATH` applies).
pub fn current_path() -> Option<OsString> {
    CURRENT.with(|c| {
        c.borrow().as_ref().and_then(|cell| {
            cell.child_env()
                .into_iter()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| OsString::from(v))
        })
    })
}

/// What `path:` / `env:` steps of past runs set, kept in the state store
/// ([`crate::state::State::env`]) for `dot env --shell`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvRecord {
    #[serde(flatten)]
    pub env: RunEnv,
}

impl EnvRecord {
    /// Reads the record; nothing recorded is an empty record.
    pub fn load(store: &StateStore) -> anyhow::Result<EnvRecord> {
        Ok(EnvRecord {
            env: store.load()?.env,
        })
    }

    /// Replaces what `bundles` recorded before with what they recorded in this run (`run`).
    /// Entries of other bundles are kept, and entries `run` has for them are ignored (a bundle
    /// that failed half-way keeps its old record).
    pub fn replace_bundles(&mut self, bundles: &[String], run: &RunEnv) {
        self.env.path.retain(|e| !bundles.contains(&e.bundle));
        self.env.vars.retain(|v| !bundles.contains(&v.bundle));
        let ours = |b: &String| bundles.contains(b);
        // Prepends are re-added last-first so their order is kept.
        for e in run
            .path
            .iter()
            .filter(|e| e.position == PathPosition::Prepend && ours(&e.bundle))
            .rev()
        {
            self.env.add_path(e.clone());
        }
        for e in run
            .path
            .iter()
            .filter(|e| e.position == PathPosition::Append && ours(&e.bundle))
        {
            self.env.add_path(e.clone());
        }
        for v in run.vars.iter().filter(|v| ours(&v.bundle)) {
            self.env.set_var(v.clone());
        }
    }

    /// Replaces, under the store's lock, what the `completed` bundles recorded with what they
    /// recorded in this run (see [`EnvRecord::replace_bundles`]). The state file is readable by
    /// its owner only (values may be sensitive).
    pub fn record_run(
        store: &StateStore,
        completed: &[String],
        run: &RunEnv,
    ) -> anyhow::Result<()> {
        store.update(|s| {
            let mut rec = EnvRecord {
                env: std::mem::take(&mut s.env),
            };
            rec.replace_bundles(completed, run);
            s.env = rec.env;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(dir: &str, position: PathPosition) -> PathEntry {
        PathEntry {
            dir: PathBuf::from(dir),
            position,
            bundle: "b".into(),
        }
    }

    fn var(name: &str, value: &str) -> EnvVar {
        EnvVar {
            name: name.into(),
            value: value.into(),
            bundle: "b".into(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn path_value_orders_and_dedupes() {
        let mut e = RunEnv::default();
        e.add_path(p("/b", PathPosition::Prepend));
        e.add_path(p("/a", PathPosition::Prepend));
        e.add_path(p("/z", PathPosition::Append));
        let v = e.path_value(Some(OsStr::new("/usr/bin:/a:/bin"))).unwrap();
        assert_eq!(v, "/a:/b:/usr/bin:/bin:/z");
        assert_eq!(RunEnv::default().path_value(Some(OsStr::new("/x"))), None);
        // Re-adding moves.
        e.add_path(p("/b", PathPosition::Prepend));
        assert_eq!(e.path_value(None).unwrap(), "/b:/a:/z");
    }

    #[test]
    fn later_values_replace_earlier_ones() {
        let mut e = RunEnv::default();
        e.set_var(var("A", "1"));
        e.set_var(var("B", "2"));
        e.set_var(var("A", "3"));
        assert_eq!(
            e.child_env(None),
            [("A".to_string(), "3".to_string()), ("B".into(), "2".into())]
        );
    }

    #[test]
    fn record_replaces_only_the_bundles_that_ran() {
        let mut rec = EnvRecord::default();
        let mut first = RunEnv::default();
        first.add_path(PathEntry {
            bundle: "keep".into(),
            ..p("/k", PathPosition::Prepend)
        });
        first.add_path(p("/old", PathPosition::Prepend));
        first.set_var(var("OLD", "1"));
        rec.replace_bundles(&["keep".into(), "b".into()], &first);
        let mut second = RunEnv::default();
        second.add_path(p("/new2", PathPosition::Prepend));
        second.add_path(p("/new1", PathPosition::Prepend));
        rec.replace_bundles(&["b".into()], &second);
        let dirs: Vec<_> = rec.env.path.iter().map(|e| e.dir.clone()).collect();
        assert_eq!(dirs, [PathBuf::from("/new1"), "/new2".into(), "/k".into()]);
        assert!(rec.env.vars.is_empty());

        // Only the named bundles are replaced, whatever else the run recorded: `keep` failed
        // half-way through this run.
        let mut third = RunEnv::default();
        third.add_path(PathEntry {
            bundle: "keep".into(),
            ..p("/partial", PathPosition::Prepend)
        });
        rec.replace_bundles(&[], &third);
        let dirs: Vec<_> = rec.env.path.iter().map(|e| e.dir.clone()).collect();
        assert_eq!(dirs, [PathBuf::from("/new1"), "/new2".into(), "/k".into()]);

        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::at(dir.path().join("state"));
        assert_eq!(EnvRecord::load(&store).unwrap(), EnvRecord::default());
        EnvRecord::record_run(&store, &["keep".into(), "b".into()], &rec.env).unwrap();
        assert_eq!(EnvRecord::load(&store).unwrap(), rec);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn recorded_lookups_and_fresh_cells_keep_the_record() {
        let mut rec = RunEnv::default();
        rec.add_path(p("/opt/bin/", PathPosition::Prepend));
        rec.set_var(var("A", "1"));
        assert!(rec.has_path(Path::new("/opt/bin"), PathPosition::Prepend));
        assert!(!rec.has_path(Path::new("/opt/bin"), PathPosition::Append));
        assert_eq!((rec.var("A"), rec.var("B")), (Some("1"), None));
        let cell = RunEnvCell::new(BaseEnv::default()).with_recorded(rec.clone());
        assert_eq!(cell.fresh().recorded(), &rec);
        assert!(cell.snapshot().is_empty(), "the record is not this run's");
    }

    #[cfg(unix)]
    #[test]
    fn system_path_directories_are_appended_for_every_bundle() {
        let cell = RunEnvCell::new(BaseEnv {
            path: Some("/usr/bin".into()),
            ..BaseEnv::default()
        });
        cell.add_system_path(vec![PathBuf::from("/usr/bin"), PathBuf::from("/opt/links")]);
        let view = cell.fresh().visible_to(BTreeSet::new());
        assert_eq!(
            view.child_env(),
            [("PATH".to_string(), "/usr/bin:/opt/links".to_string())]
        );
        assert!(cell.snapshot().is_empty(), "never recorded");
    }

    #[cfg(unix)]
    #[test]
    fn pinned_directories_stay_ahead_of_path_entries() {
        let cell = RunEnvCell::new(BaseEnv {
            path: Some("/fake/bin:/usr/bin".into()),
            first: vec![PathBuf::from("/fake/bin")],
            ..BaseEnv::default()
        });
        cell.update(|e| {
            e.add_path(p("/opt/homebrew/bin", PathPosition::Prepend));
            e.add_path(p("/fake/bin", PathPosition::Append));
        });
        assert_eq!(
            cell.child_env(),
            [(
                "PATH".to_string(),
                "/fake/bin:/opt/homebrew/bin:/usr/bin".to_string()
            )]
        );
    }

    #[test]
    fn base_env_lookups() {
        let base = BaseEnv {
            path: Some(std::env::join_paths(["/usr/bin", "/opt/x/"]).unwrap()),
            vars: [("A".to_string(), "1".to_string())].into(),
            ..BaseEnv::default()
        };
        assert!(base.has_path_dir(Path::new("/usr/bin")));
        assert!(!base.has_path_dir(Path::new("/usr")));
        assert_eq!(base.var("A"), Some("1"));
        assert_eq!(base.var("B"), None);
    }

    #[test]
    fn a_visible_view_passes_on_only_the_named_bundles_entries() {
        let cell = RunEnvCell::new(BaseEnv::default());
        cell.update(|e| {
            e.set_var(EnvVar {
                name: "A".into(),
                value: "1".into(),
                bundle: "a".into(),
            });
            e.set_var(EnvVar {
                name: "B".into(),
                value: "2".into(),
                bundle: "b".into(),
            });
        });
        let only_a = cell.visible_to(["a".to_string()].into());
        assert_eq!(only_a.child_env(), [("A".to_string(), "1".to_string())]);
        // Writes through a view go to the shared record.
        only_a.update(|e| {
            e.set_var(EnvVar {
                name: "C".into(),
                value: "3".into(),
                bundle: "c".into(),
            })
        });
        assert_eq!(cell.snapshot().vars.len(), 3);
        assert_eq!(cell.child_env().len(), 3);
        assert_eq!(only_a.child_env().len(), 1);
    }
}
