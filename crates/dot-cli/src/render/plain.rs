//! The plain renderer: one timestamped line per event, no cursor movement. For non-TTY output,
//! CI, `TERM=dumb` and `--plain`.
//!
//! Timestamps are UTC wall-clock time, `HH:MM:SSZ`, so slow steps stand out in CI logs; the log
//! file has full RFC 3339 timestamps with milliseconds.

use std::collections::VecDeque;
use std::io::Write;
use std::time::{Duration, SystemTime};

use dot_core::event::{PlanView, StepViewState};
use dot_core::{Event, Outcome, Redactor, Renderer, Status, StepId, Summary};

use super::logfile::LogLocator;
use super::{Glyphs, Verbosity};

/// Output lines kept per step for the failure report.
pub const TAIL_LINES: usize = 20;

pub struct PlainRenderer {
    out: Box<dyn Write + Send>,
    verbosity: Verbosity,
    glyphs: Glyphs,
    redactor: Redactor,
    log: Option<LogLocator>,
    /// Print the whole plan when it is ready (`dot plan`).
    show_plan: bool,
    /// Where timestamps come from; `None` prints none (the `dot plan` report).
    clock: Option<fn() -> SystemTime>,
    /// When the event being handled happened, if it was held back ([`Renderer::handle_at`]).
    at: Option<SystemTime>,
    plan: PlanView,
    dry_run: bool,
    /// The step whose output `tail` holds.
    tail_step: Option<StepId>,
    tail: VecDeque<String>,
    tail_dropped: usize,
    actions: Vec<(Outcome, String, Option<String>)>,
}

impl PlainRenderer {
    pub fn new(out: Box<dyn Write + Send>, verbosity: Verbosity, redactor: Redactor) -> Self {
        Self {
            out,
            verbosity,
            glyphs: Glyphs::detect(),
            redactor,
            log: None,
            show_plan: false,
            clock: Some(SystemTime::now),
            at: None,
            plan: PlanView::default(),
            dry_run: false,
            tail_step: None,
            tail: VecDeque::new(),
            tail_dropped: 0,
            actions: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn glyphs(mut self, glyphs: Glyphs) -> Self {
        self.glyphs = glyphs;
        self
    }

    /// The run log, for `log: <path>:<line>` under a failure.
    pub fn log_path(mut self, log: Option<LogLocator>) -> Self {
        self.log = log;
        self
    }

    /// Forgets the output kept for the failure report.
    fn clear_tail(&mut self, step: Option<StepId>) {
        self.tail.clear();
        self.tail_dropped = 0;
        self.tail_step = step;
    }

    pub fn show_plan(mut self, yes: bool) -> Self {
        self.show_plan = yes;
        self
    }

    /// Prefix each line with the time (on by default).
    pub fn timestamps(mut self, yes: bool) -> Self {
        self.clock = yes.then_some(SystemTime::now as fn() -> SystemTime);
        self
    }

    #[cfg(test)]
    fn clock(mut self, clock: fn() -> SystemTime) -> Self {
        self.clock = Some(clock);
        self
    }

    fn line(&mut self, text: &str) {
        let text = self.redactor.redact(text);
        // Output errors (a closed pipe) must not abort the run.
        let _ = match self.clock {
            Some(now) => writeln!(
                self.out,
                "{} {text}",
                time_of_day(self.at.unwrap_or_else(now))
            ),
            None => writeln!(self.out, "{text}"),
        };
    }

    fn step_name(&self, id: StepId) -> String {
        match self.plan.find_step(id) {
            Some((b, s)) => format!("{}: {}", b.name, s.label),
            None => format!("step {}.{}", id.bundle, id.step),
        }
    }

    fn print_plan(&mut self) {
        let g = self.glyphs;
        let plan = self.plan.clone();
        if !plan.secrets.is_empty() {
            self.line(&format!(
                "secrets needed (resolved when applying): {}",
                plan.secrets.join(", ")
            ));
        }
        if !plan.sudo_steps.is_empty() {
            // Windows has no sudo: the run needs an elevated terminal instead.
            let what = if cfg!(windows) {
                "an elevated terminal needed (checked before applying) by"
            } else {
                "sudo needed (asked for once, before applying) by"
            };
            self.line(&format!("{what}: {}", plan.sudo_steps.join("; ")));
        }
        if !plan.tty_sudo_steps.is_empty() {
            self.line(&format!(
                "sudo password asked for in the terminal while applying (Homebrew asks itself) \
                 by: {}",
                plan.tty_sudo_steps.join("; ")
            ));
        }
        // Without `-v`, only what this machine will do or cannot do: steps skipped by `when:`
        // (another OS, another profile) and actions already satisfied are left out, and so is
        // a bundle with nothing left in it. `-v` lists all of it.
        let verbose = self.verbosity >= Verbosity::Verbose;
        for b in &plan.bundles {
            if let Some(reason) = &b.skip {
                if verbose {
                    self.line(&format!("{} {} (skipped: {reason})", g.skipped, b.name));
                }
                continue;
            }
            let mut body = Vec::new();
            for s in &b.steps {
                match &s.state {
                    StepViewState::Skipped(r) => {
                        if verbose {
                            body.push(format!("  {} {} (skipped: {r})", g.skipped, s.label));
                        }
                    }
                    StepViewState::Error(e) => {
                        body.push(format!("  {} {}", g.failed, s.label));
                        body.push(format!("      error: {e}"));
                    }
                    StepViewState::Ready(actions) => {
                        let shown: Vec<_> = actions
                            .iter()
                            .filter(|a| verbose || a.status != Status::Satisfied)
                            .collect();
                        if shown.is_empty() && !verbose {
                            continue;
                        }
                        body.push(format!("  {}", s.label));
                        if actions.is_empty() {
                            body.push("    (nothing to do)".into());
                        }
                        for a in shown {
                            let (mark, word) = match a.status {
                                Status::Pending => (g.would_change, "pending"),
                                Status::Satisfied => (g.ok, "ok"),
                                Status::Unknown => (g.unknown, "unknown"),
                                Status::Conflict => (g.failed, "conflict"),
                            };
                            body.push(format!("    {mark} {} [{word}]", a.description));
                        }
                    }
                }
            }
            if body.is_empty() && !verbose {
                continue;
            }
            self.line(&format!("{} ({})", b.name, b.file.display()));
            for l in &body {
                self.line(l);
            }
        }
    }

    fn step_finished(
        &mut self,
        step: StepId,
        outcome: Outcome,
        detail: Option<&str>,
        elapsed: Duration,
    ) {
        // A step skipped because a dependency failed is part of the failure report.
        let dependency_failed = outcome == Outcome::Skipped
            && detail.is_some_and(|d| d.starts_with(dot_core::engine::DEPENDENCY_FAILED));
        let show = match outcome {
            Outcome::Failed => true,
            Outcome::Changed => self.verbosity >= Verbosity::Normal,
            Outcome::Skipped if dependency_failed => self.verbosity >= Verbosity::Normal,
            Outcome::Ok | Outcome::Skipped => self.verbosity >= Verbosity::Verbose,
        };
        let actions = std::mem::take(&mut self.actions);
        // Only this step's own output belongs in its failure report: a step that fails without
        // having started (a planning error) has none.
        let tail = (self.tail_step == Some(step)).then(|| std::mem::take(&mut self.tail));
        let dropped = self.tail_dropped;
        self.clear_tail(None);
        if !show {
            return;
        }
        let g = self.glyphs;
        let mut head = format!(
            "{} {}",
            g.outcome(outcome, self.dry_run),
            self.step_name(step)
        );
        if outcome == Outcome::Skipped {
            if let Some(d) = detail {
                head.push_str(&format!(" (skipped: {d})"));
            }
        } else if elapsed >= Duration::from_secs(1) {
            head.push_str(&format!(" ({:.1}s)", elapsed.as_secs_f64()));
        }
        self.line(&head);

        let list_actions = actions.len() > 1 || self.verbosity >= Verbosity::Verbose;
        if list_actions {
            for (o, desc, _) in &actions {
                let interesting = matches!(o, Outcome::Changed | Outcome::Failed);
                if interesting || self.verbosity >= Verbosity::Verbose {
                    self.line(&format!("    {} {desc}", g.outcome(*o, self.dry_run)));
                }
            }
        }
        if outcome == Outcome::Failed {
            if let Some(d) = detail {
                self.line(&format!("    error: {d}"));
            }
            let hint = crate::hints::hint(detail, tail.iter().flatten().map(String::as_str));
            if let Some(tail) = tail.filter(|t| !t.is_empty())
                && self.verbosity < Verbosity::Debug
            {
                let note = if dropped > 0 {
                    format!("last {} lines of output:", tail.len())
                } else {
                    "output:".to_string()
                };
                self.line(&format!("    {note}"));
                for l in &tail {
                    self.line(&format!("    | {l}"));
                }
            }
            if let Some(log) = &self.log {
                let at = log.at(step);
                self.line(&format!("    log: {at}"));
            }
            if let Some(h) = hint {
                self.line(&format!("    hint: {h}"));
            }
        }
    }

    fn summary(&mut self, s: &Summary) {
        let secs = s.elapsed.as_secs_f64();
        let text = if s.dry_run {
            format!(
                "Dry run: {} would change, {} ok, {} failed, {} skipped ({secs:.1}s)",
                s.changed, s.ok, s.failed, s.skipped
            )
        } else {
            format!(
                "Summary: {} changed, {} ok, {} failed, {} skipped ({secs:.1}s)",
                s.changed, s.ok, s.failed, s.skipped
            )
        };
        self.line(&text);
        for n in &s.notices {
            self.line(&format!("Notice: {n}"));
        }
        if !s.success()
            && let Some(log) = &self.log
        {
            let p = log.path().display().to_string();
            self.line(&format!("Full log: {p}"));
        }
    }
}

impl Renderer for PlainRenderer {
    fn handle_at(&mut self, event: &Event, at: SystemTime) {
        self.at = Some(at);
        self.handle(event);
        self.at = None;
    }

    fn handle(&mut self, event: &Event) {
        match event {
            Event::RunStarted { facts, dry_run, .. } => {
                self.dry_run = *dry_run;
                if self.verbosity >= Verbosity::Verbose {
                    let f: Vec<String> = facts
                        .pairs()
                        .into_iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    self.line(&format!("facts: {}", f.join(" ")));
                }
                if *dry_run && self.verbosity >= Verbosity::Normal {
                    self.line("Dry run: nothing will be changed.");
                }
            }
            Event::PlanReady(view) => {
                self.plan = view.clone();
                if self.show_plan {
                    self.print_plan();
                }
            }
            Event::BundleStarted { bundle, name } => {
                self.clear_tail(None);
                if self.verbosity >= Verbosity::Verbose {
                    let skip = self.plan.bundles.get(*bundle).and_then(|b| b.skip.clone());
                    match skip {
                        Some(reason) => {
                            let g = self.glyphs.skipped;
                            self.line(&format!("{g} {name} (skipped: {reason})"))
                        }
                        None => self.line(&format!("== {name}")),
                    }
                }
            }
            Event::StepStarted { step, .. } => {
                self.clear_tail(Some(*step));
                self.actions.clear();
                if self.verbosity >= Verbosity::Debug {
                    let name = self.step_name(*step);
                    self.line(&format!("> {name}"));
                }
            }
            Event::Output { step, line, .. } => {
                if self.verbosity >= Verbosity::Debug {
                    self.line(&format!("  | {line}"));
                }
                if self.tail_step != Some(*step) {
                    self.clear_tail(Some(*step));
                }
                if self.tail.len() == TAIL_LINES {
                    self.tail.pop_front();
                    self.tail_dropped += 1;
                }
                self.tail.push_back(line.clone());
            }
            Event::Progress { .. } => {}
            Event::NeedsTty { step, reason } => {
                if self.verbosity >= Verbosity::Normal {
                    match step {
                        Some(step) => {
                            let name = self.step_name(*step);
                            self.line(&format!("> {name}: {reason}"));
                        }
                        None => self.line(&format!("> {reason}")),
                    }
                }
                let _ = self.out.flush();
            }
            Event::TtyReleased { .. } => {}
            Event::ActionFinished {
                description,
                outcome,
                error,
                ..
            } => self
                .actions
                .push((*outcome, description.clone(), error.clone())),
            Event::StepFinished {
                step,
                outcome,
                detail,
                elapsed,
            } => self.step_finished(*step, *outcome, detail.as_deref(), *elapsed),
            Event::BundleFinished { .. } => {}
            Event::RunError(e) => self.line(&format!("error: {e}")),
            Event::RunFinished(s) => self.summary(s),
        }
    }

    fn finish(&mut self) {
        let _ = self.out.flush();
    }
}

/// `HH:MM:SSZ` (UTC).
fn time_of_day(t: SystemTime) -> String {
    let full = humantime::format_rfc3339_seconds(t).to_string(); // 2026-09-29T10:00:00Z
    full.get(11..).unwrap_or(&full).to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use dot_core::event::{BundleView, StepView};

    use super::*;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Buf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    const ID: StepId = StepId { bundle: 0, step: 0 };

    fn plan() -> PlanView {
        PlanView {
            bundles: vec![BundleView {
                name: "demo".into(),
                file: "/r/bundle.yml".into(),
                skip: None,
                steps: vec![StepView {
                    id: ID,
                    kind: "shell",
                    label: "shell: make".into(),
                    state: StepViewState::Ready(vec![]),
                }],
            }],
            secrets: vec![],
            sudo: vec![],
            sudo_steps: vec![],
            tty_sudo_steps: vec![],
        }
    }

    fn render(verbosity: Verbosity, outcome: Outcome, lines: usize) -> String {
        let buf = Buf::default();
        let redactor = Redactor::new();
        redactor.add("s3cret");
        let mut r = PlainRenderer::new(Box::new(buf.clone()), verbosity, redactor)
            .glyphs(Glyphs::ASCII)
            .timestamps(false)
            .log_path(Some("/logs/run.log".into()));
        r.handle(&Event::PlanReady(plan()));
        r.handle(&Event::StepStarted {
            step: ID,
            bundle: "demo".into(),
            kind: "shell",
            label: "shell: make".into(),
        });
        for i in 1..=lines {
            r.handle(&Event::Output {
                step: ID,
                stream: dot_core::Stream::Stdout,
                line: format!("line {i} s3cret"),
            });
        }
        r.handle(&Event::ActionFinished {
            step: ID,
            description: "run `make`".into(),
            outcome,
            error: None,
        });
        r.handle(&Event::StepFinished {
            step: ID,
            outcome,
            detail: (outcome == Outcome::Failed).then(|| "command exited with code 2".into()),
            elapsed: Duration::from_millis(5),
        });
        r.handle(&Event::RunFinished(Summary {
            failed: usize::from(outcome == Outcome::Failed),
            changed: usize::from(outcome == Outcome::Changed),
            ok: usize::from(outcome == Outcome::Ok),
            ..Summary::default()
        }));
        buf.text()
    }

    #[test]
    fn failure_shows_last_20_lines_and_log() {
        let text = render(Verbosity::Normal, Outcome::Failed, 30);
        assert!(text.contains("x demo: shell: make\n"), "{text}");
        assert!(
            text.contains("    error: command exited with code 2\n"),
            "{text}"
        );
        assert!(text.contains("last 20 lines of output:"), "{text}");
        assert!(!text.contains("line 10 "), "{text}");
        assert!(text.contains("| line 11 ***"), "{text}");
        assert!(text.contains("| line 30 ***"), "{text}");
        assert!(text.contains("    log: /logs/run.log"), "{text}");
        assert!(text.contains("Full log: /logs/run.log"), "{text}");
        assert!(!text.contains("s3cret"), "redacted: {text}");
    }

    #[test]
    fn notices_follow_the_summary_at_every_verbosity() {
        for v in [Verbosity::Quiet, Verbosity::Normal] {
            let buf = Buf::default();
            let mut r =
                PlainRenderer::new(Box::new(buf.clone()), v, Redactor::new()).timestamps(false);
            r.handle(&Event::RunFinished(Summary {
                changed: 1,
                notices: vec!["log out and back in: user was added to group docker".into()],
                ..Summary::default()
            }));
            assert_eq!(
                buf.text(),
                "Summary: 1 changed, 0 ok, 0 failed, 0 skipped (0.0s)\n\
                 Notice: log out and back in: user was added to group docker\n"
            );
        }
    }

    #[test]
    fn quiet_hides_changes_but_not_failures() {
        let text = render(Verbosity::Quiet, Outcome::Changed, 3);
        assert_eq!(
            text,
            "Summary: 1 changed, 0 ok, 0 failed, 0 skipped (0.0s)\n"
        );
        let text = render(Verbosity::Quiet, Outcome::Failed, 3);
        assert!(text.contains("x demo: shell: make"), "{text}");
    }

    #[test]
    fn normal_shows_changed_not_ok() {
        let text = render(Verbosity::Normal, Outcome::Changed, 3);
        assert!(text.starts_with("+ demo: shell: make\n"), "{text}");
        assert!(!text.contains("line 1"), "output hidden: {text}");
        let text = render(Verbosity::Normal, Outcome::Ok, 0);
        assert!(!text.contains("demo"), "{text}");
    }

    #[test]
    fn verbose_shows_ok_steps_and_actions() {
        let text = render(Verbosity::Verbose, Outcome::Ok, 0);
        assert!(
            text.contains("= demo: shell: make\n    = run `make`\n"),
            "{text}"
        );
    }

    #[test]
    fn lines_are_timestamped_by_default() {
        fn fixed() -> SystemTime {
            // 2026-09-29T10:11:12Z
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_676_672)
        }
        let buf = Buf::default();
        let mut r = PlainRenderer::new(Box::new(buf.clone()), Verbosity::Normal, Redactor::new())
            .glyphs(Glyphs::ASCII)
            .clock(fixed);
        r.handle(&Event::RunFinished(Summary::default()));
        assert_eq!(
            buf.text(),
            "10:11:12Z Summary: 0 changed, 0 ok, 0 failed, 0 skipped (0.0s)\n"
        );
    }

    #[test]
    fn an_event_held_back_is_stamped_with_when_it_happened() {
        fn fixed() -> SystemTime {
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_676_672)
        }
        let buf = Buf::default();
        let mut r = PlainRenderer::new(Box::new(buf.clone()), Verbosity::Normal, Redactor::new())
            .glyphs(Glyphs::ASCII)
            .clock(fixed);
        let earlier = fixed() - Duration::from_secs(3);
        r.handle_at(&Event::RunFinished(Summary::default()), earlier);
        r.handle(&Event::RunFinished(Summary::default()));
        assert_eq!(
            buf.text(),
            "10:11:09Z Summary: 0 changed, 0 ok, 0 failed, 0 skipped (0.0s)\n\
             10:11:12Z Summary: 0 changed, 0 ok, 0 failed, 0 skipped (0.0s)\n"
        );
    }

    #[test]
    fn plan_shows_unknown_actions_and_secrets() {
        let buf = Buf::default();
        let mut r = PlainRenderer::new(Box::new(buf.clone()), Verbosity::Normal, Redactor::new())
            .glyphs(Glyphs::ASCII)
            .timestamps(false)
            .show_plan(true);
        let mut view = plan();
        view.secrets = vec!["gitlab_token".into()];
        view.bundles[0].steps[0].state = StepViewState::Ready(vec![dot_core::event::ActionView {
            description: "link ~/.x -> ~/y (source does not exist yet)".into(),
            status: Status::Unknown,
        }]);
        r.handle(&Event::PlanReady(view));
        let text = buf.text();
        assert!(
            text.contains("secrets needed (resolved when applying): gitlab_token"),
            "{text}"
        );
        assert!(
            text.contains("    ? link ~/.x -> ~/y (source does not exist yet) [unknown]"),
            "{text}"
        );
    }

    #[test]
    fn plan_hides_skipped_and_satisfied_steps_unless_verbose() {
        let render = |v: Verbosity| {
            let buf = Buf::default();
            let mut r = PlainRenderer::new(Box::new(buf.clone()), v, Redactor::new())
                .glyphs(Glyphs::ASCII)
                .timestamps(false)
                .show_plan(true);
            let mut view = plan();
            view.bundles[0].steps[0].state =
                StepViewState::Skipped("when: os is linux, not windows".into());
            r.handle(&Event::PlanReady(view));
            buf.text()
        };
        let quiet = render(Verbosity::Normal);
        assert!(!quiet.contains("when: os is linux"), "{quiet}");
        let verbose = render(Verbosity::Verbose);
        assert!(
            verbose.contains("(skipped: when: os is linux, not windows)"),
            "{verbose}"
        );
    }

    #[test]
    fn a_step_failing_at_plan_time_shows_no_output_of_the_one_before() {
        let buf = Buf::default();
        let mut r = PlainRenderer::new(Box::new(buf.clone()), Verbosity::Normal, Redactor::new())
            .glyphs(Glyphs::ASCII)
            .timestamps(false);
        let mut view = plan();
        view.bundles.push(BundleView {
            name: "e1".into(),
            file: "/r/e1.yml".into(),
            skip: None,
            steps: vec![StepView {
                id: StepId { bundle: 1, step: 0 },
                kind: "shell",
                label: "shell: echo {{ nope }}".into(),
                state: StepViewState::Error("undefined variable `nope`".into()),
            }],
        });
        r.handle(&Event::PlanReady(view));
        r.handle(&Event::StepStarted {
            step: ID,
            bundle: "demo".into(),
            kind: "shell",
            label: "shell: make".into(),
        });
        r.handle(&Event::Output {
            step: ID,
            stream: dot_core::Stream::Stdout,
            line: "output of demo".into(),
        });
        r.handle(&Event::StepFinished {
            step: ID,
            outcome: Outcome::Changed,
            detail: None,
            elapsed: Duration::ZERO,
        });
        r.handle(&Event::BundleStarted {
            bundle: 1,
            name: "e1".into(),
        });
        r.handle(&Event::StepFinished {
            step: StepId { bundle: 1, step: 0 },
            outcome: Outcome::Failed,
            detail: Some("undefined variable `nope`".into()),
            elapsed: Duration::ZERO,
        });
        let text = buf.text();
        assert!(text.contains("x e1: shell: echo {{ nope }}"), "{text}");
        assert!(!text.contains("output of demo"), "{text}");
    }

    #[test]
    fn failure_points_at_the_log_line() {
        let buf = Buf::default();
        let dir = tempfile::tempdir().unwrap();
        // A real log renderer fills the locator; it runs first in the sink.
        let mut file =
            super::super::logfile::LogFileRenderer::create(dir.path(), Redactor::new()).unwrap();
        let log = file.locator();
        let mut r2 = PlainRenderer::new(Box::new(buf.clone()), Verbosity::Normal, Redactor::new())
            .glyphs(Glyphs::ASCII)
            .timestamps(false)
            .log_path(Some(log));
        for e in [
            Event::PlanReady(plan()),
            Event::StepStarted {
                step: ID,
                bundle: "demo".into(),
                kind: "shell",
                label: "shell: make".into(),
            },
            Event::StepFinished {
                step: ID,
                outcome: Outcome::Failed,
                detail: Some("boom".into()),
                elapsed: Duration::ZERO,
            },
        ] {
            file.handle(&e);
            r2.handle(&e);
        }
        let text = buf.text();
        let want = format!("    log: {}:", file.path().display());
        assert!(text.contains(&want), "{text}");
    }

    #[test]
    fn a_known_failure_gets_a_hint_under_its_block() {
        let buf = Buf::default();
        let mut r = PlainRenderer::new(Box::new(buf.clone()), Verbosity::Normal, Redactor::new())
            .glyphs(Glyphs::ASCII)
            .timestamps(false)
            .log_path(Some("/logs/run.log".into()));
        r.handle(&Event::PlanReady(plan()));
        r.handle(&Event::StepStarted {
            step: ID,
            bundle: "demo".into(),
            kind: "shell",
            label: "shell: make".into(),
        });
        r.handle(&Event::Output {
            step: ID,
            stream: dot_core::Stream::Stderr,
            line: "E: Could not get lock /var/lib/dpkg/lock-frontend".into(),
        });
        r.handle(&Event::StepFinished {
            step: ID,
            outcome: Outcome::Failed,
            detail: Some("command exited with code 100".into()),
            elapsed: Duration::ZERO,
        });
        let text = buf.text();
        assert!(
            text.ends_with(
                "    log: /logs/run.log\n    hint: another apt/dpkg process holds the package \
                 lock (on a fresh machine usually unattended-upgrades): wait for it to finish \
                 (`ps aux | grep -E 'apt|dpkg'`), then run `dot apply` again\n"
            ),
            "{text}"
        );
        // Unknown failures get none.
        assert!(!render(Verbosity::Normal, Outcome::Failed, 3).contains("hint:"));
    }

    #[test]
    fn debug_streams_output() {
        let text = render(Verbosity::Debug, Outcome::Changed, 2);
        assert!(
            text.contains("> demo: shell: make\n  | line 1 ***\n  | line 2 ***\n"),
            "{text}"
        );
    }
}
