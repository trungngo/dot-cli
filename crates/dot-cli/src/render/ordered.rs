//! Plan-order output for renderers that write a linear record (plain, log file) while bundles
//! run at the same time (`--jobs N`).
//!
//! [`Ordered`] passes a bundle's events on while it is the first unfinished bundle in plan
//! order, and holds the events of the bundles after it until then: the record reads exactly as
//! if the bundles had run one after the other, whatever order they finished in. With one job
//! nothing is ever held.
//!
//! Run-level events pass straight through, and so do `NeedsTty` / `TtyReleased`: they say what
//! the terminal is doing now (a prompt is waiting), which must not wait for another bundle.
//! `RunError` and `RunFinished` flush everything still held first. A held event keeps the time
//! it happened, and is replayed with it ([`Renderer::handle_at`]), so timestamps stay true.

use std::collections::{BTreeMap, BTreeSet};
use std::time::SystemTime;

use dot_core::{Event, Renderer};

pub struct Ordered<R> {
    inner: R,
    /// The bundle whose events pass straight through.
    current: usize,
    held: BTreeMap<usize, Vec<(SystemTime, Event)>>,
    /// Bundles after `current` that have finished (their events are all held).
    finished: BTreeSet<usize>,
}

impl<R: Renderer> Ordered<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            current: 0,
            held: BTreeMap::new(),
            finished: BTreeSet::new(),
        }
    }

    /// Moves past the current bundle, replaying what the next ones held, until one that is
    /// still running.
    fn advance(&mut self) {
        loop {
            self.current += 1;
            for (at, e) in self.held.remove(&self.current).unwrap_or_default() {
                self.inner.handle_at(&e, at);
            }
            if !self.finished.remove(&self.current) {
                break;
            }
        }
    }

    fn flush_all(&mut self) {
        for (_, events) in std::mem::take(&mut self.held) {
            for (at, e) in events {
                self.inner.handle_at(&e, at);
            }
        }
        self.finished.clear();
    }
}

/// The bundle an event belongs to, when it must be kept in plan order.
fn bundle_of(event: &Event) -> Option<usize> {
    match event {
        Event::BundleStarted { bundle, .. } | Event::BundleFinished { bundle, .. } => Some(*bundle),
        Event::StepStarted { step, .. }
        | Event::Output { step, .. }
        | Event::Progress { step, .. }
        | Event::ActionFinished { step, .. }
        | Event::StepFinished { step, .. } => Some(step.bundle),
        Event::RunStarted { .. }
        | Event::PlanReady(_)
        | Event::NeedsTty { .. }
        | Event::TtyReleased { .. }
        | Event::RunError(_)
        | Event::RunFinished(_) => None,
    }
}

impl<R: Renderer> Renderer for Ordered<R> {
    fn handle(&mut self, event: &Event) {
        let done = matches!(event, Event::BundleFinished { .. });
        match bundle_of(event) {
            None => {
                if matches!(event, Event::RunError(_) | Event::RunFinished(_)) {
                    self.flush_all();
                }
                self.inner.handle(event);
            }
            Some(b) if b <= self.current => {
                self.inner.handle(event);
                if done && b == self.current {
                    self.advance();
                }
            }
            Some(b) => {
                self.held
                    .entry(b)
                    .or_default()
                    .push((SystemTime::now(), event.clone()));
                if done {
                    self.finished.insert(b);
                }
            }
        }
    }

    fn finish(&mut self) {
        self.flush_all();
        self.inner.finish();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use dot_core::{Outcome, StepId, Stream, Summary};

    use super::*;

    /// Records a short name for each event it gets.
    #[derive(Clone, Default)]
    struct Names(Arc<Mutex<Vec<String>>>);

    impl Renderer for Names {
        fn handle(&mut self, event: &Event) {
            let name = match event {
                Event::BundleStarted { bundle, .. } => format!("start {bundle}"),
                Event::BundleFinished { bundle, .. } => format!("end {bundle}"),
                Event::Output { step, line, .. } => format!("{}:{line}", step.bundle),
                Event::NeedsTty { .. } => "tty".into(),
                Event::RunFinished(_) => "summary".into(),
                _ => "other".into(),
            };
            self.0.lock().unwrap().push(name);
        }
    }

    fn start(bundle: usize) -> Event {
        Event::BundleStarted {
            bundle,
            name: format!("b{bundle}"),
        }
    }

    fn end(bundle: usize) -> Event {
        Event::BundleFinished {
            bundle,
            name: format!("b{bundle}"),
            outcome: Outcome::Changed,
        }
    }

    fn out(bundle: usize, line: &str) -> Event {
        Event::Output {
            step: StepId { bundle, step: 0 },
            stream: Stream::Stdout,
            line: line.into(),
        }
    }

    fn render(events: Vec<Event>) -> Vec<String> {
        let names = Names::default();
        let mut r = Ordered::new(names.clone());
        for e in events {
            r.handle(&e);
        }
        r.finish();
        names.0.lock().unwrap().clone()
    }

    #[test]
    fn interleaved_bundles_come_out_in_plan_order() {
        let got = render(vec![
            start(0),
            start(1),
            start(2),
            out(2, "c"),
            out(1, "b"),
            end(2),
            out(0, "a1"),
            Event::NeedsTty {
                step: None,
                reason: "sudo".into(),
            },
            out(1, "b2"),
            out(0, "a2"),
            end(0),
            out(1, "b3"),
            end(1),
            start(3),
            end(3),
            Event::RunFinished(Summary {
                elapsed: Duration::ZERO,
                ..Summary::default()
            }),
        ]);
        assert_eq!(
            got,
            [
                "start 0", "0:a1", "tty", "0:a2", "end 0", "start 1", "1:b", "1:b2", "1:b3",
                "end 1", "start 2", "2:c", "end 2", "start 3", "end 3", "summary"
            ]
        );
    }

    #[test]
    fn one_at_a_time_is_passed_straight_through() {
        let events = vec![start(0), out(0, "x"), end(0), start(1), out(1, "y"), end(1)];
        assert_eq!(
            render(events),
            ["start 0", "0:x", "end 0", "start 1", "1:y", "end 1"]
        );
    }

    /// Records when each event is said to have happened.
    #[derive(Clone, Default)]
    struct Times(Arc<Mutex<Vec<(String, SystemTime)>>>);

    impl Renderer for Times {
        fn handle(&mut self, event: &Event) {
            self.handle_at(event, SystemTime::now());
        }
        fn handle_at(&mut self, event: &Event, at: SystemTime) {
            let name = match event {
                Event::Output { line, .. } => line.clone(),
                _ => return,
            };
            self.0.lock().unwrap().push((name, at));
        }
    }

    #[test]
    fn held_events_keep_the_time_they_happened() {
        let times = Times::default();
        let mut r = Ordered::new(times.clone());
        r.handle(&start(0));
        r.handle(&start(1));
        r.handle(&out(1, "early"));
        let early = SystemTime::now();
        std::thread::sleep(Duration::from_millis(30));
        r.handle(&out(0, "late"));
        r.handle(&end(0));
        let got = times.0.lock().unwrap().clone();
        assert_eq!(got[0].0, "late");
        assert_eq!(got[1].0, "early");
        // Replayed after `late`, stamped before it.
        assert!(got[1].1 <= early, "{got:?}");
        assert!(got[1].1 < got[0].1, "{got:?}");
    }

    #[test]
    fn what_is_held_comes_out_before_the_summary() {
        let got = render(vec![
            start(1),
            out(1, "late"),
            Event::RunFinished(Summary::default()),
        ]);
        assert_eq!(got, ["start 1", "1:late", "summary"]);
    }
}
