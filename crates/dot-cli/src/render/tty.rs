//! The tty renderer: an inline live view for interactive terminals.
//!
//! Finished lines (bundle headers, step results, failure blocks, the summary) are printed above
//! the live area and stay in scrollback. The live area, redrawn in place, holds:
//!
//! ```text
//! nodejs                        <- the bundle, until its header is printed for good
//!   ⠹ Install fnm  4s           <- the running step: spinner, or a bar from `Progress`
//!     │ Downloading fnm...      <- the last few output lines; dropped when the step succeeds
//! ```
//!
//! Each running step has its own spinner, window and output tail, keyed by its [`StepId`], so
//! steps that run at the same time (`--jobs`) each keep their live view and their
//! failure report.
//!
//! `NeedsTty` clears the live area and stops all drawing until `TtyReleased`, so sudo and login
//! prompts own the terminal; lines produced meanwhile are held back and printed on resume.
//!
//! Every piece of text goes through the shared [`Redactor`] before it is drawn.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use console::Style;
use dot_core::event::{PlanView, StepViewState};
use dot_core::{Event, Outcome, Redactor, Renderer, StepId, Summary, Unit};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use super::logfile::LogLocator;
use super::plain::TAIL_LINES;
use super::{Glyphs, Verbosity};
use crate::prompt::Prompter;

/// Output lines shown live under the running step.
pub const WINDOW_LINES: usize = 5;

/// Width assumed when the terminal does not report one.
const FALLBACK_WIDTH: usize = 80;

/// Columns kept free on the spinner line for the spinner, elapsed time and (with `Progress`) the
/// bar and counts.
const SPINNER_RESERVE: usize = 12;
const PROGRESS_RESERVE: usize = 52;

/// Makes the draw target for the live area; called again when drawing resumes after `NeedsTty`.
pub type TargetFactory = Box<dyn Fn() -> ProgressDrawTarget + Send>;

/// Colours for the pieces of the view. All plain when colour is off.
#[derive(Clone)]
struct Styles {
    ok: Style,
    changed: Style,
    failed: Style,
    skipped: Style,
    dim: Style,
    bold: Style,
}

impl Styles {
    fn new(color: bool) -> Self {
        let s = |st: Style| st.force_styling(color);
        Self {
            ok: s(Style::new().dim()),
            changed: s(Style::new().green()),
            failed: s(Style::new().red().bold()),
            skipped: s(Style::new().yellow()),
            dim: s(Style::new().dim()),
            bold: s(Style::new().bold()),
        }
    }

    fn outcome(&self, outcome: Outcome) -> &Style {
        match outcome {
            Outcome::Changed => &self.changed,
            Outcome::Ok => &self.ok,
            Outcome::Skipped => &self.skipped,
            Outcome::Failed => &self.failed,
        }
    }
}

/// A step being applied and its live bars.
struct Running {
    label: String,
    spinner: ProgressBar,
    window: Option<ProgressBar>,
    /// Last [`TAIL_LINES`] output lines (redacted, sanitised), for the window and failure block.
    tail: VecDeque<String>,
    dropped: usize,
    actions: Vec<(Outcome, String)>,
    progress: Option<Unit>,
}

pub struct TtyRenderer {
    mp: MultiProgress,
    make_target: TargetFactory,
    verbosity: Verbosity,
    glyphs: Glyphs,
    styles: Styles,
    redactor: Redactor,
    log: Option<LogLocator>,
    /// Fixed terminal width (tests); `None` asks the terminal on every draw.
    width: Option<usize>,
    /// Animate spinners from a background ticker (off in tests, for deterministic frames).
    tick: bool,
    plan: PlanView,
    dry_run: bool,
    /// The live bundle line, while that bundle's header has not been printed for good.
    bundle_bar: Option<(usize, ProgressBar)>,
    /// Bundle whose header is already in scrollback.
    header_printed: Option<usize>,
    running: BTreeMap<StepId, Running>,
    /// Between `NeedsTty` and `TtyReleased`: nothing is drawn, printed lines wait here.
    suspended: bool,
    held: Vec<String>,
}

impl TtyRenderer {
    /// Draws to stdout.
    pub fn stdout(verbosity: Verbosity, color: bool, redactor: Redactor) -> Self {
        Self::with_target(
            Box::new(ProgressDrawTarget::stdout),
            verbosity,
            color,
            redactor,
        )
        .tick(true)
    }

    pub fn with_target(
        make_target: TargetFactory,
        verbosity: Verbosity,
        color: bool,
        redactor: Redactor,
    ) -> Self {
        let mp = MultiProgress::with_draw_target(make_target());
        Self {
            mp,
            make_target,
            verbosity,
            glyphs: Glyphs::detect(),
            styles: Styles::new(color),
            redactor,
            log: None,
            width: None,
            tick: false,
            plan: PlanView::default(),
            dry_run: false,
            bundle_bar: None,
            header_printed: None,
            running: BTreeMap::new(),
            suspended: false,
            held: Vec::new(),
        }
    }

    // Used by tests; a builder option the CLI does not need yet.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn glyphs(mut self, glyphs: Glyphs) -> Self {
        self.glyphs = glyphs;
        self
    }

    /// The run log, for `log: <path>:<line>` under a failure.
    pub fn log_path(mut self, log: Option<LogLocator>) -> Self {
        self.log = log;
        self
    }

    /// Truncate live lines to this width instead of the terminal's.
    // Used by tests; a builder option the CLI does not need yet.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn width(mut self, width: usize) -> Self {
        self.width = Some(width);
        self
    }

    /// Animate spinners in the background.
    pub fn tick(mut self, yes: bool) -> Self {
        self.tick = yes;
        self
    }

    fn term_width(&self) -> usize {
        self.width.unwrap_or_else(|| {
            console::Term::stdout()
                .size_checked()
                .map(|(_, w)| usize::from(w))
                .filter(|w| *w > 0)
                .unwrap_or(FALLBACK_WIDTH)
        })
    }

    /// Whether the ASCII glyph set is in use (non-UTF-8 locale); spinners, bars and ellipses
    /// follow it.
    fn ascii(&self) -> bool {
        self.glyphs.failed == Glyphs::ASCII.failed
    }

    fn ellipsis(&self) -> &'static str {
        if self.ascii() { "..." } else { "…" }
    }

    /// Redacts and fits `text` into `width` columns.
    fn fit(&self, text: &str, width: usize) -> String {
        let text = self.redactor.redact(text);
        console::truncate_str(&text, width.max(1), self.ellipsis()).into_owned()
    }

    /// Prints a finished line above the live area (held back while suspended).
    fn println(&mut self, text: &str) {
        let text = self.redactor.redact(text).into_owned();
        if self.suspended {
            self.held.push(text);
        } else {
            // Output errors (a closed terminal) must not abort the run.
            let _ = self.mp.println(text);
        }
    }

    fn spinner_style(&self) -> ProgressStyle {
        let ticks: &[&str] = if self.ascii() {
            &["|", "/", "-", "\\", " "]
        } else {
            &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", " "]
        };
        ProgressStyle::with_template("  {spinner} {msg}  {elapsed}")
            .expect("valid template")
            .tick_strings(ticks)
    }

    fn progress_style(&self, unit: Unit, total: bool) -> ProgressStyle {
        let counts = match (unit, total) {
            (Unit::Bytes, true) => "{binary_bytes}/{binary_total_bytes}",
            (Unit::Bytes, false) => "{binary_bytes}",
            (Unit::Items, true) => "{pos}/{len}",
            (Unit::Items, false) => "{pos}",
        };
        let bar = if total { " [{bar:20}]" } else { "" };
        let chars = if self.ascii() { "=> " } else { "━╸ " };
        ProgressStyle::with_template(&format!("  {{spinner}} {{msg}}{bar} {counts}  {{elapsed}}"))
            .expect("valid template")
            .progress_chars(chars)
            .tick_strings(&["-", "-"])
    }

    fn step_label(&self, id: StepId) -> String {
        match self.plan.find_step(id) {
            Some((_, s)) => s.label.clone(),
            None => format!("step {}.{}", id.bundle, id.step),
        }
    }

    fn bundle_name(&self, bundle: usize) -> String {
        self.plan
            .bundles
            .get(bundle)
            .map(|b| b.name.clone())
            .unwrap_or_else(|| format!("bundle {bundle}"))
    }

    /// Prints the bundle's header for good (once) and drops its live line.
    fn ensure_header(&mut self, bundle: usize) {
        if self.header_printed == Some(bundle) {
            return;
        }
        if let Some((b, bar)) = self.bundle_bar.take() {
            if b == bundle {
                bar.finish_and_clear();
                self.mp.remove(&bar);
            } else {
                self.bundle_bar = Some((b, bar));
            }
        }
        self.header_printed = Some(bundle);
        let name = self.bundle_name(bundle);
        let text = self.styles.bold.apply_to(name).to_string();
        self.println(&text);
    }

    fn bundle_started(&mut self, bundle: usize, name: &str) {
        self.clear_bundle_bar();
        let skip = self.plan.bundles.get(bundle).and_then(|b| b.skip.clone());
        if let Some(reason) = skip {
            if self.verbosity >= Verbosity::Verbose {
                let g = self.styles.skipped.apply_to(self.glyphs.skipped);
                self.println(&format!("{g} {name} (skipped: {reason})"));
            }
            return;
        }
        if self.verbosity >= Verbosity::Verbose {
            self.ensure_header(bundle);
            return;
        }
        let bar = self.mp.add(ProgressBar::new_spinner());
        bar.set_style(ProgressStyle::with_template("{msg}").expect("valid template"));
        let width = self.term_width();
        let text = self.fit(name, width);
        bar.set_message(self.styles.bold.apply_to(text).to_string());
        self.bundle_bar = Some((bundle, bar));
    }

    fn clear_bundle_bar(&mut self) {
        if let Some((_, bar)) = self.bundle_bar.take() {
            bar.finish_and_clear();
            self.mp.remove(&bar);
        }
    }

    fn step_started(&mut self, step: StepId, label: &str) {
        self.drop_running(step);
        let spinner = self.mp.add(ProgressBar::new_spinner());
        spinner.set_style(self.spinner_style());
        let width = self.term_width().saturating_sub(SPINNER_RESERVE);
        spinner.set_message(self.fit(label, width));
        if self.tick && !self.suspended {
            spinner.enable_steady_tick(Duration::from_millis(100));
        }
        spinner.tick();
        self.running.insert(
            step,
            Running {
                label: label.to_string(),
                spinner,
                window: None,
                tail: VecDeque::new(),
                dropped: 0,
                actions: Vec::new(),
                progress: None,
            },
        );
        if self.verbosity >= Verbosity::Debug {
            self.ensure_header(step.bundle);
            let text = format!("  > {label}");
            self.println(&text);
        }
    }

    fn output(&mut self, step: StepId, line: &str) {
        let line = sanitize(&self.redactor.redact(line));
        if self.verbosity >= Verbosity::Debug {
            let bar = self.styles.dim.apply_to(self.window_bar()).to_string();
            self.println(&format!("    {bar} {line}"));
        }
        let Some(r) = self.running.get_mut(&step) else {
            return;
        };
        if r.tail.len() == TAIL_LINES {
            r.tail.pop_front();
            r.dropped += 1;
        }
        r.tail.push_back(line);
        if self.verbosity < Verbosity::Debug {
            self.draw_window(step);
        }
    }

    fn window_bar(&self) -> &'static str {
        if self.ascii() { "|" } else { "│" }
    }

    fn draw_window(&mut self, step: StepId) {
        let width = self.term_width().saturating_sub(6);
        let bar = self.styles.dim.apply_to(self.window_bar()).to_string();
        let Some(r) = self.running.get(&step) else {
            return;
        };
        let skip = r.tail.len().saturating_sub(WINDOW_LINES);
        let text: Vec<String> = r
            .tail
            .iter()
            .skip(skip)
            .map(|l| {
                let l = console::truncate_str(l, width.max(1), self.ellipsis());
                format!("    {bar} {}", self.styles.dim.apply_to(l))
            })
            .collect();
        let text = text.join("\n");
        let window = match &r.window {
            Some(w) => w.clone(),
            None => {
                let w = self.mp.insert_after(&r.spinner, ProgressBar::new_spinner());
                w.set_style(ProgressStyle::with_template("{msg}").expect("valid template"));
                if let Some(r) = self.running.get_mut(&step) {
                    r.window = Some(w.clone());
                }
                w
            }
        };
        window.set_message(text);
    }

    fn progress(&mut self, step: StepId, done: u64, total: Option<u64>, unit: Unit) {
        let width = self.term_width().saturating_sub(PROGRESS_RESERVE);
        let style = self.progress_style(unit, total.is_some());
        let Some(r) = self.running.get_mut(&step) else {
            return;
        };
        let label = r.label.clone();
        if r.progress != Some(unit) || r.spinner.length().is_some() != total.is_some() {
            r.progress = Some(unit);
            r.spinner.set_style(style);
        }
        match total {
            Some(t) => r.spinner.set_length(t),
            None => r.spinner.unset_length(),
        }
        r.spinner.set_position(done);
        let text = self.fit(&label, width);
        if let Some(r) = self.running.get(&step) {
            r.spinner.set_message(text);
        }
    }

    /// Removes a running step's live bars.
    fn drop_running(&mut self, step: StepId) -> Option<Running> {
        let r = self.running.remove(&step)?;
        r.spinner.finish_and_clear();
        self.mp.remove(&r.spinner);
        if let Some(w) = &r.window {
            w.finish_and_clear();
            self.mp.remove(w);
        }
        Some(r)
    }

    /// Removes every running step's live bars.
    fn drop_all_running(&mut self) {
        let steps: Vec<StepId> = self.running.keys().copied().collect();
        for s in steps {
            self.drop_running(s);
        }
    }

    fn step_finished(
        &mut self,
        step: StepId,
        outcome: Outcome,
        detail: Option<&str>,
        elapsed: Duration,
    ) {
        let running = self.drop_running(step);
        // A step skipped because a dependency failed is part of the failure report.
        let dependency_failed = outcome == Outcome::Skipped
            && detail.is_some_and(|d| d.starts_with(dot_core::engine::DEPENDENCY_FAILED));
        let show = match outcome {
            Outcome::Failed => true,
            Outcome::Changed => self.verbosity >= Verbosity::Normal,
            Outcome::Skipped if dependency_failed => self.verbosity >= Verbosity::Normal,
            Outcome::Ok | Outcome::Skipped => self.verbosity >= Verbosity::Verbose,
        };
        if !show {
            return;
        }
        self.ensure_header(step.bundle);
        let label = match &running {
            Some(r) => r.label.clone(),
            None => self.step_label(step),
        };
        let style = self.styles.outcome(outcome).clone();
        let mark = style.apply_to(self.glyphs.outcome(outcome, self.dry_run));
        let mut suffix = String::new();
        if outcome == Outcome::Skipped {
            if let Some(d) = detail {
                suffix = format!(" (skipped: {d})");
            }
        } else if elapsed >= Duration::from_secs(1) {
            suffix = format!(" ({:.1}s)", elapsed.as_secs_f64());
        }
        let width = self
            .term_width()
            .saturating_sub(4 + console::measure_text_width(&suffix));
        let label = self.fit(&label, width);
        let label = if outcome == Outcome::Failed {
            style.apply_to(label).to_string()
        } else {
            label
        };
        let suffix = self.styles.dim.apply_to(suffix);
        self.println(&format!("  {mark} {label}{suffix}"));

        let actions = running
            .as_ref()
            .map(|r| r.actions.clone())
            .unwrap_or_default();
        if actions.len() > 1 || self.verbosity >= Verbosity::Verbose {
            for (o, desc) in &actions {
                let interesting = matches!(o, Outcome::Changed | Outcome::Failed);
                if interesting || self.verbosity >= Verbosity::Verbose {
                    let m = self
                        .styles
                        .outcome(*o)
                        .apply_to(self.glyphs.outcome(*o, self.dry_run));
                    self.println(&format!("      {m} {desc}"));
                }
            }
        }

        if outcome == Outcome::Failed {
            self.failure_block(step, detail, running.as_ref());
        }
    }

    fn failure_block(&mut self, step: StepId, detail: Option<&str>, running: Option<&Running>) {
        if let Some(d) = detail {
            let text = self
                .styles
                .failed
                .apply_to(format!("error: {d}"))
                .to_string();
            self.println(&format!("      {text}"));
        }
        if let Some(r) = running
            && !r.tail.is_empty()
            && self.verbosity < Verbosity::Debug
        {
            let note = if r.dropped > 0 {
                format!("last {} lines of output:", r.tail.len())
            } else {
                "output:".to_string()
            };
            let note = self.styles.dim.apply_to(note).to_string();
            self.println(&format!("      {note}"));
            let bar = self.styles.dim.apply_to(self.window_bar()).to_string();
            for l in &r.tail {
                self.println(&format!("      {bar} {l}"));
            }
        }
        if let Some(log) = &self.log {
            // Under `--jobs N` the log may not have written this step yet (it keeps plan
            // order): then the step's heading says where to look.
            let name = match self.plan.find_step(step) {
                Some((b, s)) => format!("{}[{}] {}", b.name, step.step, s.label),
                None => format!("{}.{}", step.bundle, step.step),
            };
            let text = format!("log: {}", log.at_named(step, &name));
            let text = self.styles.dim.apply_to(text).to_string();
            self.println(&format!("      {text}"));
        }
        let tail = running
            .into_iter()
            .flat_map(|r| r.tail.iter().map(String::as_str));
        if let Some(h) = crate::hints::hint(detail, tail) {
            let label = self.styles.skipped.apply_to("hint:");
            self.println(&format!("      {label} {h}"));
        }
    }

    fn needs_tty(&mut self, step: Option<StepId>, reason: &str) {
        if self.suspended {
            return;
        }
        let what = match step {
            Some(s) => format!("{}: {reason}", self.step_label(s)),
            None => reason.to_string(),
        };
        let text = self.styles.dim.apply_to(format!("  > {what}")).to_string();
        self.println(&text);
        for r in self.running.values() {
            r.spinner.disable_steady_tick();
        }
        let _ = self.mp.clear();
        self.mp.set_draw_target(ProgressDrawTarget::hidden());
        self.suspended = true;
    }

    fn tty_released(&mut self) {
        if !self.suspended {
            return;
        }
        self.suspended = false;
        self.mp.set_draw_target((self.make_target)());
        for line in std::mem::take(&mut self.held) {
            let _ = self.mp.println(line);
        }
        if let Some((_, bar)) = &self.bundle_bar {
            bar.tick();
        }
        for r in self.running.values() {
            if self.tick {
                r.spinner.enable_steady_tick(Duration::from_millis(100));
            }
            r.spinner.tick();
            if let Some(w) = &r.window {
                w.tick();
            }
        }
    }

    fn summary(&mut self, s: &Summary) {
        self.drop_all_running();
        self.clear_bundle_bar();
        let secs = s.elapsed.as_secs_f64();
        let st = &self.styles;
        let failed = format!("{} failed", s.failed);
        let failed = if s.failed > 0 {
            st.failed.apply_to(failed).to_string()
        } else {
            failed
        };
        let changed = st.changed.apply_to(format!(
            "{} {}",
            s.changed,
            if s.dry_run { "would change" } else { "changed" }
        ));
        let head = if s.dry_run { "Dry run:" } else { "Summary:" };
        let head = st.bold.apply_to(head);
        let text = format!(
            "{head} {changed}, {} ok, {failed}, {} skipped ({secs:.1}s)",
            s.ok, s.skipped
        );
        self.println(&text);
        for n in &s.notices {
            let text = format!("{} {n}", self.styles.skipped.apply_to("Notice:"));
            self.println(&text);
        }
        if !s.success()
            && let Some(log) = &self.log
        {
            let text = format!("Full log: {}", log.path().display());
            self.println(&text);
        }
    }
}

/// A handle on the live view for what happens outside the event stream: the plan preview and
/// the "Apply?" question. Both run with the live area hidden, then it is redrawn.
#[derive(Clone)]
pub struct TtyConsole {
    mp: MultiProgress,
    bold: Style,
}

impl TtyRenderer {
    /// A handle that can take the terminal over from this renderer for a moment.
    pub fn console(&self) -> TtyConsole {
        TtyConsole {
            mp: self.mp.clone(),
            bold: self.styles.bold.clone(),
        }
    }
}

impl TtyConsole {
    /// Runs `f` with the live area cleared; whatever `f` prints stays above it.
    pub fn suspend<R>(&self, f: impl FnOnce() -> R) -> R {
        self.mp.suspend(f)
    }
}

/// The tty renderer's side of the confirmation: the question is drawn in the renderer's style,
/// with the live view out of the way, and answered through `inner`.
pub struct TtyPrompter<P> {
    console: TtyConsole,
    inner: P,
}

impl<P: Prompter> TtyPrompter<P> {
    pub fn new(console: TtyConsole, inner: P) -> Self {
        Self { console, inner }
    }
}

impl<P: Prompter> Prompter for TtyPrompter<P> {
    fn confirm(&mut self, question: &str, default: bool) -> std::io::Result<bool> {
        let question = self.console.bold.apply_to(question).to_string();
        let inner = &mut self.inner;
        self.console.suspend(|| inner.confirm(&question, default))
    }
}

impl Renderer for TtyRenderer {
    fn handle(&mut self, event: &Event) {
        match event {
            Event::RunStarted { dry_run, .. } => {
                self.dry_run = *dry_run;
                if *dry_run && self.verbosity >= Verbosity::Normal {
                    let text = self
                        .styles
                        .dim
                        .apply_to("Dry run: nothing will be changed.")
                        .to_string();
                    self.println(&text);
                }
            }
            Event::PlanReady(view) => {
                self.plan = view.clone();
                // Plan errors surface as failed steps; list them up front too.
                for b in &view.bundles {
                    for s in &b.steps {
                        if let StepViewState::Error(e) = &s.state
                            && self.verbosity >= Verbosity::Verbose
                        {
                            let text = format!("plan error in {}: {}: {e}", b.name, s.label);
                            self.println(&text);
                        }
                    }
                }
            }
            Event::BundleStarted { bundle, name } => self.bundle_started(*bundle, name),
            Event::StepStarted { step, label, .. } => self.step_started(*step, label),
            Event::Output { step, line, .. } => self.output(*step, line),
            Event::Progress {
                step,
                done,
                total,
                unit,
            } => self.progress(*step, *done, *total, *unit),
            Event::NeedsTty { step, reason } => self.needs_tty(*step, reason),
            Event::TtyReleased { .. } => self.tty_released(),
            Event::ActionFinished {
                step,
                description,
                outcome,
                ..
            } => {
                if let Some(r) = self.running.get_mut(step) {
                    r.actions.push((*outcome, description.clone()));
                }
            }
            Event::StepFinished {
                step,
                outcome,
                detail,
                elapsed,
            } => self.step_finished(*step, *outcome, detail.as_deref(), *elapsed),
            Event::BundleFinished { bundle, .. } => {
                if self.bundle_bar.as_ref().is_some_and(|(b, _)| b == bundle) {
                    self.clear_bundle_bar();
                }
            }
            Event::RunError(e) => {
                let text = self
                    .styles
                    .failed
                    .apply_to(format!("error: {e}"))
                    .to_string();
                self.println(&text);
            }
            Event::RunFinished(s) => self.summary(s),
        }
    }

    fn finish(&mut self) {
        self.tty_released();
        self.drop_all_running();
        self.clear_bundle_bar();
        let _ = self.mp.clear();
    }
}

/// Makes a child's output line safe to draw inside the live area: no escape sequences, no
/// carriage-return redraws (only the final state of the line is kept), no tabs or other control
/// characters that would move the cursor.
fn sanitize(line: &str) -> String {
    let line = line.rsplit('\r').find(|s| !s.is_empty()).unwrap_or("");
    let line = console::strip_ansi_codes(line);
    line.chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use dot_core::Stream;
    use dot_core::event::{BundleView, StepView};
    use indicatif::InMemoryTerm;

    use super::*;

    const S0: StepId = StepId { bundle: 0, step: 0 };
    const S1: StepId = StepId { bundle: 0, step: 1 };

    fn plan() -> PlanView {
        let step = |id: StepId, label: &str| StepView {
            id,
            kind: "shell",
            label: label.into(),
            state: StepViewState::Ready(vec![]),
        };
        PlanView {
            bundles: vec![BundleView {
                name: "nodejs".into(),
                file: "/r/bundle.yml".into(),
                skip: None,
                steps: vec![step(S0, "Install fnm"), step(S1, "link ~/.npmrc")],
            }],
            secrets: vec![],
            sudo: vec![],
            sudo_steps: vec![],
            tty_sudo_steps: vec![],
        }
    }

    struct Harness {
        term: InMemoryTerm,
        r: TtyRenderer,
    }

    impl Harness {
        fn new(verbosity: Verbosity) -> Self {
            Self::sized(verbosity, 80, false)
        }

        fn sized(verbosity: Verbosity, cols: u16, color: bool) -> Self {
            let term = InMemoryTerm::new(60, cols);
            let t = term.clone();
            let redactor = Redactor::new();
            redactor.add("s3cret");
            let r = TtyRenderer::with_target(
                Box::new(move || ProgressDrawTarget::term_like(Box::new(t.clone()))),
                verbosity,
                color,
                redactor,
            )
            .glyphs(Glyphs::ASCII)
            .width(usize::from(cols))
            .log_path(Some("/logs/run.log".into()));
            let mut h = Self { term, r };
            h.send(Event::PlanReady(plan()));
            h
        }

        fn send(&mut self, e: Event) {
            self.r.handle(&e);
        }

        fn start(&mut self, step: StepId) {
            if step.step == 0 {
                self.send(Event::BundleStarted {
                    bundle: step.bundle,
                    name: "nodejs".into(),
                });
            }
            let label = plan().find_step(step).unwrap().1.label.clone();
            self.send(Event::StepStarted {
                step,
                bundle: "nodejs".into(),
                kind: "shell",
                label,
            });
        }

        fn out(&mut self, step: StepId, n: std::ops::RangeInclusive<usize>) {
            for i in n {
                self.send(Event::Output {
                    step,
                    stream: Stream::Stdout,
                    line: format!("line {i} s3cret"),
                });
            }
        }

        fn finish_step(&mut self, step: StepId, outcome: Outcome, secs: u64) {
            self.send(Event::StepFinished {
                step,
                outcome,
                detail: (outcome == Outcome::Failed).then(|| "command exited with code 2".into()),
                elapsed: Duration::from_secs(secs),
            });
        }

        fn screen(&self) -> String {
            self.term.contents()
        }
    }

    #[test]
    fn prompter_asks_with_the_live_area_out_of_the_way() {
        let mut h = Harness::new(Verbosity::Normal);
        h.start(S0);
        assert!(h.screen().contains("Install fnm"), "{}", h.screen());
        let mut out = Vec::new();
        let answer = TtyPrompter::new(
            h.r.console(),
            crate::prompt::LinePrompter::new(&b"n\n"[..], &mut out),
        )
        .confirm("Apply?", true)
        .unwrap();
        assert!(!answer);
        assert_eq!(String::from_utf8(out).unwrap(), "Apply? [Y/n] ");
        assert!(
            h.screen().contains("Install fnm"),
            "redrawn: {}",
            h.screen()
        );
    }

    #[test]
    fn running_step_shows_spinner_and_rolling_window() {
        let mut h = Harness::new(Verbosity::Normal);
        h.start(S0);
        h.out(S0, 1..=8);
        let s = h.screen();
        assert!(s.contains("nodejs"), "{s}");
        assert!(s.contains("Install fnm"), "{s}");
        assert!(!s.contains("line 3 "), "only the last 5 lines: {s}");
        assert!(s.contains("| line 4 ***"), "{s}");
        assert!(s.contains("| line 8 ***"), "{s}");
        assert!(!s.contains("s3cret"), "redacted: {s}");
    }

    #[test]
    fn window_collapses_on_success() {
        let mut h = Harness::new(Verbosity::Normal);
        h.start(S0);
        h.out(S0, 1..=3);
        h.finish_step(S0, Outcome::Changed, 2);
        h.start(S1);
        h.finish_step(S1, Outcome::Ok, 0);
        h.send(Event::BundleFinished {
            bundle: 0,
            name: "nodejs".into(),
            outcome: Outcome::Changed,
        });
        h.send(Event::RunFinished(Summary {
            changed: 1,
            ok: 1,
            elapsed: Duration::from_secs(2),
            ..Summary::default()
        }));
        h.r.finish();
        let s = h.screen();
        assert_eq!(
            s,
            "nodejs\n  + Install fnm (2.0s)\nSummary: 1 changed, 1 ok, 0 failed, 0 skipped (2.0s)"
        );
    }

    #[test]
    fn failure_block_has_last_20_lines_and_log() {
        let mut h = Harness::new(Verbosity::Normal);
        h.start(S0);
        h.out(S0, 1..=30);
        h.finish_step(S0, Outcome::Failed, 3);
        h.send(Event::RunFinished(Summary {
            failed: 1,
            ..Summary::default()
        }));
        h.r.finish();
        let s = h.screen();
        assert!(s.contains("nodejs\n  x Install fnm (3.0s)\n"), "{s}");
        assert!(s.contains("      error: command exited with code 2"), "{s}");
        assert!(s.contains("last 20 lines of output:"), "{s}");
        assert!(!s.contains("line 10 "), "{s}");
        assert!(s.contains("      | line 11 ***"), "{s}");
        assert!(s.contains("      | line 30 ***"), "{s}");
        // The log has not written the step (it holds it back under `--jobs`): its heading
        // there says where to look.
        assert!(
            s.contains("      log: /logs/run.log (at \"step nodejs[0] Install fnm: started\")"),
            "{s}"
        );
        assert!(s.contains("Full log: /logs/run.log"), "{s}");
        assert!(!s.contains("s3cret"), "{s}");
    }

    #[test]
    fn a_known_failure_gets_a_hint() {
        let mut h = Harness::new(Verbosity::Normal);
        h.start(S0);
        h.send(Event::Output {
            step: S0,
            stream: Stream::Stderr,
            line: "fatal: unable to access 'https://x/': Could not resolve host: x".into(),
        });
        h.finish_step(S0, Outcome::Failed, 0);
        h.r.finish();
        let s = h.screen();
        assert!(
            s.contains("started\")\n      hint: a host name could not be resolved"),
            "{s}"
        );
    }

    #[test]
    fn quiet_run_prints_only_the_summary() {
        let mut h = Harness::new(Verbosity::Quiet);
        h.start(S0);
        h.finish_step(S0, Outcome::Changed, 0);
        h.send(Event::RunFinished(Summary {
            changed: 1,
            ..Summary::default()
        }));
        h.r.finish();
        assert_eq!(
            h.screen(),
            "Summary: 1 changed, 0 ok, 0 failed, 0 skipped (0.0s)"
        );
    }

    #[test]
    fn notices_follow_the_summary() {
        let mut h = Harness::new(Verbosity::Quiet);
        h.send(Event::RunFinished(Summary {
            changed: 1,
            notices: vec!["log out and back in: user was added to group docker".into()],
            ..Summary::default()
        }));
        h.r.finish();
        assert_eq!(
            h.screen(),
            "Summary: 1 changed, 0 ok, 0 failed, 0 skipped (0.0s)\n\
             Notice: log out and back in: user was added to group docker"
        );
    }

    #[test]
    fn verbose_lists_every_step() {
        let mut h = Harness::new(Verbosity::Verbose);
        h.start(S0);
        h.send(Event::ActionFinished {
            step: S0,
            description: "run `install.sh`".into(),
            outcome: Outcome::Ok,
            error: None,
        });
        h.finish_step(S0, Outcome::Ok, 0);
        h.r.finish();
        assert_eq!(
            h.screen(),
            "nodejs\n  = Install fnm\n      = run `install.sh`"
        );
    }

    #[test]
    fn debug_streams_output_for_good() {
        let mut h = Harness::new(Verbosity::Debug);
        h.start(S0);
        h.out(S0, 1..=2);
        h.finish_step(S0, Outcome::Changed, 0);
        h.r.finish();
        assert_eq!(
            h.screen(),
            "nodejs\n  > Install fnm\n    | line 1 ***\n    | line 2 ***\n  + Install fnm"
        );
    }

    #[test]
    fn needs_tty_suspends_drawing_until_released() {
        let mut h = Harness::new(Verbosity::Normal);
        h.start(S0);
        h.out(S0, 1..=2);
        h.send(Event::NeedsTty {
            step: None,
            reason: "sudo password".into(),
        });
        assert_eq!(h.screen(), "  > sudo password", "live area cleared");
        // Nothing is drawn while the prompt owns the terminal.
        h.term.reset();
        h.out(S0, 3..=4);
        h.finish_step(S0, Outcome::Changed, 0);
        h.start(S1);
        assert_eq!(h.screen(), "");
        h.send(Event::TtyReleased { step: None });
        let s = h.screen();
        assert!(
            s.starts_with("nodejs\n  + Install fnm\n"),
            "held lines: {s}"
        );
        assert!(s.contains("link ~/.npmrc"), "live again: {s}");
    }

    #[test]
    fn progress_shows_a_byte_bar() {
        let mut h = Harness::new(Verbosity::Normal);
        h.start(S0);
        h.send(Event::Progress {
            step: S0,
            done: 1024,
            total: Some(4096),
            unit: Unit::Bytes,
        });
        let s = h.screen();
        assert!(s.contains("Install fnm ["), "{s}");
        assert!(s.contains("1.00 KiB/4.00 KiB"), "{s}");
        h.send(Event::Progress {
            step: S0,
            done: 3,
            total: Some(7),
            unit: Unit::Items,
        });
        assert!(h.screen().contains("3/7"), "{}", h.screen());
    }

    #[test]
    fn long_lines_are_truncated_to_the_width() {
        let mut h = Harness::sized(Verbosity::Normal, 40, false);
        h.start(S0);
        h.send(Event::Output {
            step: S0,
            stream: Stream::Stdout,
            line: format!("\x1b[31m{}\x1b[0m\r", "y".repeat(100)),
        });
        let s = h.screen();
        assert!(s.contains("y..."), "{s}");
        for l in s.lines() {
            assert!(console::measure_text_width(l) <= 40, "{l:?} in {s}");
        }
    }

    #[test]
    fn colour_only_when_enabled() {
        let run = |color| {
            let mut h = Harness::sized(Verbosity::Normal, 80, color);
            h.start(S0);
            h.finish_step(S0, Outcome::Changed, 0);
            h.r.finish();
            h.term.contents_formatted()
        };
        let has_esc = |b: Vec<u8>| {
            // The formatted dump always carries cursor/reset codes; look for an SGR colour.
            String::from_utf8_lossy(&b).contains("\x1b[32m")
        };
        assert!(has_esc(run(true)));
        assert!(!has_esc(run(false)));
    }

    #[test]
    fn steps_skipped_for_a_failed_dependency_are_shown() {
        let mut h = Harness::new(Verbosity::Normal);
        h.send(Event::BundleStarted {
            bundle: 0,
            name: "nodejs".into(),
        });
        h.send(Event::StepFinished {
            step: S0,
            outcome: Outcome::Skipped,
            detail: Some(format!(
                "{} fail failed",
                dot_core::engine::DEPENDENCY_FAILED
            )),
            elapsed: Duration::ZERO,
        });
        // Other skips stay hidden at default verbosity.
        h.send(Event::StepFinished {
            step: S1,
            outcome: Outcome::Skipped,
            detail: Some("when: os is linux, not macos".into()),
            elapsed: Duration::ZERO,
        });
        h.r.finish();
        let s = h.screen();
        assert!(
            s.contains("  - Install fnm (skipped: dependency fail failed)"),
            "{s}"
        );
        assert!(!s.contains("link ~/.npmrc"), "{s}");
    }

    #[test]
    fn concurrent_steps_keep_their_own_live_view_and_failure_output() {
        let mut h = Harness::new(Verbosity::Normal);
        h.start(S0);
        h.start(S1);
        h.out(S0, 1..=2);
        h.send(Event::Output {
            step: S1,
            stream: Stream::Stdout,
            line: "from step one".into(),
        });
        let s = h.screen();
        assert!(
            s.contains("Install fnm") && s.contains("link ~/.npmrc"),
            "{s}"
        );
        h.finish_step(S0, Outcome::Failed, 0);
        let s = h.screen();
        assert!(s.contains("      | line 1 ***"), "{s}");
        assert!(s.contains("      | line 2 ***"), "{s}");
        assert!(!s.contains("      | from step one"), "{s}");
        assert!(
            s.contains("link ~/.npmrc"),
            "the other step is still live: {s}"
        );
        h.finish_step(S1, Outcome::Changed, 0);
        h.r.finish();
        assert!(h.screen().contains("  + link ~/.npmrc"), "{}", h.screen());
    }

    #[test]
    fn sanitize_keeps_the_final_redraw() {
        assert_eq!(sanitize("10%\r50%\r100%"), "100%");
        assert_eq!(sanitize("done\r"), "done");
        assert_eq!(sanitize("\x1b[1ma\tb\x07\x1b[0m"), "a b");
    }
}
