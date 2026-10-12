//! Events the engine emits. Renderers (plain, log file; later tty and json) subscribe through an
//! [`EventSink`]; the engine never prints directly.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::action::{Outcome, Status};
use crate::facts::Facts;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    pub fn as_str(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
    }
}

/// Identifies a step within a run: bundle position in the plan, step position in the bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StepId {
    pub bundle: usize,
    pub step: usize,
}

#[derive(Debug, Clone)]
pub enum Event {
    RunStarted {
        facts: Facts,
        dry_run: bool,
        /// Selected profile; `None` without one (`-c` bundle files, a repository without profiles).
        profile: Option<String>,
    },
    /// The whole plan, before anything is applied.
    PlanReady(PlanView),
    BundleStarted {
        bundle: usize,
        name: String,
    },
    StepStarted {
        step: StepId,
        bundle: String,
        kind: &'static str,
        label: String,
    },
    /// One line of a child process's output, without the line terminator.
    Output {
        step: StepId,
        stream: Stream,
        line: String,
    },
    Progress {
        step: StepId,
        done: u64,
        total: Option<u64>,
        unit: Unit,
    },
    /// Something needs the real terminal (sudo, a login prompt); live rendering must pause until
    /// the matching `TtyReleased`. `step` is `None` for run-level prompts: `sudo -v` before the
    /// first step, or a secret provider's `unlock`.
    NeedsTty {
        step: Option<StepId>,
        reason: String,
    },
    TtyReleased {
        step: Option<StepId>,
    },
    /// One action of a step finished (in a dry run: was evaluated).
    ActionFinished {
        step: StepId,
        description: String,
        outcome: Outcome,
        error: Option<String>,
    },
    StepFinished {
        step: StepId,
        outcome: Outcome,
        /// Why it was skipped or failed.
        detail: Option<String>,
        elapsed: Duration,
    },
    BundleFinished {
        bundle: usize,
        name: String,
        outcome: Outcome,
    },
    /// The run could not proceed past planning (e.g. unlocking secrets failed); no step ran.
    RunError(String),
    RunFinished(Summary),
}

/// What a `Progress` count measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Bytes,
    Items,
}

/// Step counts for a run. In a dry run, `changed` counts steps that would change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub changed: usize,
    pub ok: usize,
    pub failed: usize,
    pub skipped: usize,
    pub elapsed: Duration,
    pub dry_run: bool,
    /// Set when the run stopped before applying anything (see [`Event::RunError`]).
    pub run_error: Option<String>,
    /// The bundles whose steps all ran to the end in a real run: not failed, skipped, or
    /// satisfied by their `check:`. What they recorded (`path:` / `env:`) replaces what they
    /// recorded before; nothing else of theirs is known to have run.
    pub completed: Vec<String>,
    /// Things the user has to do after the run, in the order steps raised them, each once
    /// ("log out and back in: new membership of group docker"). Every renderer prints them
    /// with the summary. See [`crate::RunCtx::notice`].
    pub notices: Vec<String>,
}

impl Summary {
    pub fn record(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Changed => self.changed += 1,
            Outcome::Ok => self.ok += 1,
            Outcome::Failed => self.failed += 1,
            Outcome::Skipped => self.skipped += 1,
        }
    }

    pub fn success(&self) -> bool {
        self.failed == 0 && self.run_error.is_none()
    }
}

/// A printable snapshot of a [`crate::engine::Plan`].
#[derive(Debug, Clone, Default)]
pub struct PlanView {
    pub bundles: Vec<BundleView>,
    /// Logical secret names the run needs; resolved after planning, never by `dot plan`.
    pub secrets: Vec<String>,
    /// Bundles with an action that needs root: sudo is asked for once, before applying.
    pub sudo: Vec<String>,
    /// The steps behind `sudo`, as `bundle: step label`.
    pub sudo_steps: Vec<String>,
    /// The steps whose program asks for the sudo password by itself, on the terminal, while
    /// applying (a Homebrew cask's `.pkg` installer), as `bundle: step label`.
    pub tty_sudo_steps: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BundleView {
    pub name: String,
    pub file: PathBuf,
    /// Why the whole bundle is skipped (its `when:` did not match).
    pub skip: Option<String>,
    pub steps: Vec<StepView>,
}

#[derive(Debug, Clone)]
pub struct StepView {
    pub id: StepId,
    pub kind: &'static str,
    pub label: String,
    pub state: StepViewState,
}

#[derive(Debug, Clone)]
pub enum StepViewState {
    Skipped(String),
    /// Planning failed (e.g. a missing link source); the bundle will not be applied.
    Error(String),
    Ready(Vec<ActionView>),
}

#[derive(Debug, Clone)]
pub struct ActionView {
    pub description: String,
    pub status: Status,
}

impl PlanView {
    pub fn has_errors(&self) -> bool {
        self.bundles.iter().any(|b| {
            b.steps
                .iter()
                .any(|s| matches!(s.state, StepViewState::Error(_)))
        })
    }

    pub fn find_step(&self, id: StepId) -> Option<(&BundleView, &StepView)> {
        let b = self.bundles.get(id.bundle)?;
        Some((b, b.steps.get(id.step)?))
    }
}

/// Receives every event. `Send` so a sink can be shared across the threads of `--jobs N`.
pub trait Renderer: Send {
    fn handle(&mut self, event: &Event);
    /// An event that happened at `at` and is handled only now (held back to keep plan order
    /// under `--jobs N`): renderers that print times print `at`. Default: [`Renderer::handle`].
    fn handle_at(&mut self, event: &Event, at: std::time::SystemTime) {
        let _ = at;
        self.handle(event);
    }
    /// Called once when the run is over, to flush.
    fn finish(&mut self) {}
}

/// Fans each event out to every renderer, in the order they were added. A cheap, cloneable,
/// thread-safe handle: every clone feeds the same renderers, and events from concurrent emitters
/// are delivered whole, one at a time.
#[derive(Clone, Default)]
pub struct EventSink {
    renderers: Arc<Mutex<Vec<Box<dyn Renderer>>>>,
}

impl EventSink {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Box<dyn Renderer>>> {
        // A renderer that panicked mid-event must not silence the rest of the run.
        self.renderers.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn add(&self, r: Box<dyn Renderer>) {
        self.lock().push(r);
    }

    pub fn with(self, r: Box<dyn Renderer>) -> Self {
        self.add(r);
        self
    }

    pub fn emit(&self, event: Event) {
        for r in self.lock().iter_mut() {
            r.handle(&event);
        }
    }

    pub fn finish(&self) {
        for r in self.lock().iter_mut() {
            r.finish();
        }
    }
}

/// A renderer that records every event; for tests.
#[derive(Clone, Default)]
pub struct Recorder(pub Arc<Mutex<Vec<Event>>>);

impl Recorder {
    pub fn events(&self) -> Vec<Event> {
        self.0.lock().expect("recorder lock").clone()
    }
}

impl Renderer for Recorder {
    fn handle(&mut self, event: &Event) {
        self.0.lock().expect("recorder lock").push(event.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn sink_is_shareable_across_threads() {
        assert_send_sync::<EventSink>();
        assert_send_sync::<crate::RunCtx>();
        let rec = Recorder::default();
        let sink = EventSink::new().with(Box::new(rec.clone()));
        std::thread::scope(|s| {
            for b in 0..4 {
                let ctx = crate::RunCtx::new(&sink, StepId { bundle: b, step: 0 });
                s.spawn(move || {
                    for i in 0..50 {
                        ctx.output(Stream::Stdout, format!("{b}:{i}"));
                    }
                });
            }
        });
        let events = rec.events();
        assert_eq!(events.len(), 200);
        // Each emitter's own events stay in order.
        for b in 0..4 {
            let mine: Vec<String> = events
                .iter()
                .filter_map(|e| match e {
                    Event::Output { step, line, .. } if step.bundle == b => Some(line.clone()),
                    _ => None,
                })
                .collect();
            let want: Vec<String> = (0..50).map(|i| format!("{b}:{i}")).collect();
            assert_eq!(mine, want);
        }
    }
}
