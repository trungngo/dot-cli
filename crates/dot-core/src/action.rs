//! Actions: the units of work a directive plans. Planning is pure; only `apply` changes anything.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::event::{Event, EventSink, StepId, Stream, Unit};
use crate::resource::{Diff, Resource};
use crate::state::{Restore, StateStore};

/// Whether an action has anything to do. Computed without side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pending,
    Satisfied,
    /// Cannot be decided yet without side effects: the input may be produced by an earlier step
    /// (a link source a `shell` step downloads, a `cwd` a `create` step makes), or deciding needs
    /// resolved secrets, which `dot plan` never unlocks. `describe` says why. The engine checks
    /// again right before applying, when earlier steps have run.
    Unknown,
    /// Applying would fail without changing anything: something the step must not touch is in
    /// the way (a file where a link goes and `on_conflict: fail`, a checkout with local changes,
    /// an executable `dot` did not install). `describe` says what. Plans and dry runs report it
    /// as a would-fail; a real run still calls `apply`, which must fail with the reason.
    Conflict,
}

impl Status {
    /// Whether applying would do something: pending, or possibly so (unknown). Satisfied and
    /// conflicting actions change nothing.
    pub fn has_work(self) -> bool {
        matches!(self, Status::Pending | Status::Unknown)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Satisfied => "satisfied",
            Status::Unknown => "unknown",
            Status::Conflict => "conflict",
        }
    }
}

/// What applying an action, step or bundle did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Something was changed (in a dry run: would be changed).
    Changed,
    /// Already in the desired state.
    Ok,
    /// Not run: `when:` did not match, or an earlier step of the bundle failed.
    Skipped,
    Failed,
}

/// One unit of work, e.g. "link ~/.npmrc -> npmrc" or "run `install.sh`".
///
/// `describe` and `status` must not change anything: they back `dot plan`, `--dry-run` and (later)
/// drift detection. The engine re-plans each step right before running it and calls `status`
/// again right before `apply`, since earlier steps may have changed the machine in between.
///
/// `Send + Sync` so a plan can be shared by the worker threads of `--jobs N`.
pub trait Action: Send + Sync {
    fn describe(&self) -> String;
    fn status(&self) -> Result<Status>;
    fn apply(&self, ctx: &RunCtx) -> Result<Outcome>;

    /// Whether applying needs root (through `sudo` when `dot` is not root), so the run can ask
    /// for the password once, up front, instead of in the middle of a live view. Only asked of
    /// actions that are not satisfied, after `status`; an action may use what `status` found.
    fn needs_sudo(&self) -> bool {
        false
    }

    /// What the action manages (see [`crate::resource`]), with how to undo each. Must not
    /// change anything. Asked while planning (for identity: `dot status` orphans) and again
    /// right after a real run applied the action, when the engine records them in the state
    /// store: the undo data may then use what applying left (the SHA-256 of a written file).
    /// Default: nothing (a `shell` step, a `clean`).
    fn resources(&self) -> Vec<Resource> {
        Vec::new()
    }

    /// How the machine differs from what the action wants, for `dot diff`; asked after
    /// `status`. `None`: [`Action::describe`] says it all. Never holds a secret value.
    fn diff(&self) -> Result<Option<Diff>> {
        Ok(None)
    }

    /// Whether applying hands the terminal to a child (an interactive command, a login
    /// prompt): under `--jobs N` such a step runs alone, like one that needs root.
    fn needs_tty(&self) -> bool {
        false
    }

    /// Whether applying runs a program that asks for the sudo password by itself, whatever
    /// sudo timestamp `dot` holds (Homebrew discards cached sudo credentials before it runs a
    /// cask's `.pkg` installer). Implies [`Action::needs_sudo`]. The run does not ask for sudo
    /// up front for it: an interactive run hands such a program the terminal (the action says
    /// [`Action::needs_tty`]); a non-interactive one is allowed only with passwordless sudo,
    /// checked before anything is applied. Only asked of actions that are not satisfied.
    fn asks_for_sudo(&self) -> bool {
        false
    }
}

/// Keeps `dot`'s own `sudo -n` working while a run applies (`dot_platform::sudo`). Other
/// programs may discard the sudo timestamp `dot` asked for up front (every `brew` command
/// does), so it is checked again right before each action that needs it.
pub trait SudoCheck: Send + Sync {
    /// Right before the action `what` of step `step`, which runs commands through `sudo -n`
    /// ([`Action::needs_sudo`]), is applied: make sure `sudo -n` works, asking for the
    /// password again on the terminal (the console paused around it) when it no longer does
    /// and the run is interactive. `Err`: it does not; the action fails with that.
    fn before_sudo(&self, sink: &EventSink, step: StepId, what: &str) -> Result<()>;
}

/// What an action may do while applying besides touching the machine: report output and ask for
/// the terminal. Owns a handle to the (shared, thread-safe) event sink, so several steps can run
/// at once.
#[derive(Clone)]
pub struct RunCtx {
    sink: EventSink,
    step: StepId,
    notices: Notices,
    backups: Arc<Mutex<Vec<Restore>>>,
    /// The run's state store (real runs), for [`RunCtx::preserve_root`].
    store: Option<StateStore>,
}

/// The run's notices (see [`RunCtx::notice`]); cloning shares them.
#[derive(Debug, Clone, Default)]
pub struct Notices(Arc<Mutex<Vec<String>>>);

impl Notices {
    /// Adds `text` unless it is already there.
    pub fn add(&self, text: impl Into<String>) {
        let text = text.into();
        let mut v = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !v.contains(&text) {
            v.push(text);
        }
    }

    pub fn list(&self) -> Vec<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl RunCtx {
    pub fn new(sink: &EventSink, step: StepId) -> Self {
        Self {
            sink: sink.clone(),
            step,
            notices: Notices::default(),
            backups: Arc::default(),
            store: None,
        }
    }

    /// The same context saving originals into `store` ([`RunCtx::preserve_root`]).
    pub fn with_state(mut self, store: StateStore) -> Self {
        self.store = Some(store);
        self
    }

    /// The same context adding its notices to `notices` (the run's).
    pub fn with_notices(mut self, notices: Notices) -> Self {
        self.notices = notices;
        self
    }

    /// Something the user has to do once the run is over, e.g. "log out and back in: new
    /// membership of group docker". Collected (each text once) into
    /// [`crate::Summary::notices`], which every renderer prints with the summary.
    pub fn notice(&self, text: impl Into<String>) {
        self.notices.add(text);
    }

    /// The notices raised so far (this context's collection).
    pub fn notices(&self) -> Vec<String> {
        self.notices.list()
    }

    /// The action moved what was in its way to `backup` (a `.dot-backup`): recorded with its
    /// resource, so `dot remove` can move it back.
    pub fn backed_up(&self, backup: &Path) {
        self.backups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Restore::Backup {
                path: backup.to_path_buf(),
            });
    }

    /// The action is about to write over the root-owned file at `path` (resource `key`), which
    /// holds `bytes` with `mode` (read through sudo when need be): the first time `dot` writes
    /// it, they are saved in the state directory, so `dot remove` can put them back as root.
    /// The engine's own saving ([`crate::Resource::preserve`]) reads as this user, which a
    /// root-owned file may not allow. Does nothing outside a real run.
    pub fn preserve_root(&self, key: &str, path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        if !store.read().first_write(key) {
            return Ok(());
        }
        let saved = store.save_backup(path, bytes)?;
        self.backups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Restore::RootBackup { path: saved, mode });
        Ok(())
    }

    /// What was saved or moved aside since the last call (the engine takes them after each
    /// action and records them with its first resource).
    pub fn take_backups(&self) -> Vec<Restore> {
        std::mem::take(&mut *self.backups.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn step(&self) -> StepId {
        self.step
    }

    /// One line of child output.
    pub fn output(&self, stream: Stream, line: impl Into<String>) {
        self.sink.emit(Event::Output {
            step: self.step,
            stream,
            line: line.into(),
        });
    }

    pub fn progress(&self, done: u64, total: Option<u64>, unit: Unit) {
        self.sink.emit(Event::Progress {
            step: self.step,
            done,
            total,
            unit,
        });
    }

    /// Call before handing the terminal to a child; renderers pause live output.
    pub fn needs_tty(&self, reason: impl Into<String>) {
        self.sink.emit(Event::NeedsTty {
            step: Some(self.step),
            reason: reason.into(),
        });
    }

    pub fn tty_released(&self) {
        self.sink.emit(Event::TtyReleased {
            step: Some(self.step),
        });
    }
}
