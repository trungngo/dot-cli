//! The `--json` renderer: one JSON object per event, one per line.
//!
//! Every object has an `"event"` tag in snake_case (`"step_started"`, `"output"`, …) and, unless
//! turned off, a `"ts"` RFC 3339 timestamp with milliseconds. Steps are `{"bundle": n, "step":
//! m}`; durations are `elapsed_ms`. Every string value is redacted before it is written.
//!
//! `dot_core::Event` is not `Serialize` (and must not depend on serde's shape), so this module
//! mirrors it in [`JsonEvent`].

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::time::{Duration, SystemTime};

use dot_core::event::{PlanView, StepViewState};
use dot_core::{Event, Outcome, Redactor, Renderer, StepId, Summary, Unit};
use serde::Serialize;
use serde_json::Value;

use super::plain::TAIL_LINES;

#[derive(Serialize)]
struct JsonStep {
    bundle: usize,
    step: usize,
}

impl From<StepId> for JsonStep {
    fn from(s: StepId) -> Self {
        Self {
            bundle: s.bundle,
            step: s.step,
        }
    }
}

#[derive(Serialize)]
struct JsonBundle<'a> {
    name: &'a str,
    file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    skip: Option<&'a str>,
    steps: Vec<JsonPlanStep<'a>>,
}

#[derive(Serialize)]
struct JsonPlanStep<'a> {
    step: JsonStep,
    kind: &'a str,
    label: &'a str,
    #[serde(flatten)]
    state: JsonStepState<'a>,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum JsonStepState<'a> {
    Skipped { reason: &'a str },
    Error { error: &'a str },
    Ready { actions: Vec<JsonAction<'a>> },
}

#[derive(Serialize)]
struct JsonAction<'a> {
    description: &'a str,
    status: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum JsonEvent<'a> {
    RunStarted {
        facts: BTreeMap<&'static str, String>,
        dry_run: bool,
        profile: Option<&'a str>,
    },
    PlanReady {
        bundles: Vec<JsonBundle<'a>>,
        secrets: &'a [String],
        /// Bundles that need sudo; left out when none do.
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        sudo: &'a [String],
        /// The steps that need it, as `bundle: label`; left out when none do.
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        sudo_steps: &'a [String],
        /// The steps whose program asks for the sudo password itself, on the terminal, while
        /// applying; left out when none do.
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        tty_sudo_steps: &'a [String],
    },
    BundleStarted {
        bundle: usize,
        name: &'a str,
    },
    StepStarted {
        step: JsonStep,
        bundle: &'a str,
        kind: &'a str,
        label: &'a str,
    },
    Output {
        step: JsonStep,
        stream: &'static str,
        line: &'a str,
    },
    Progress {
        step: JsonStep,
        done: u64,
        total: Option<u64>,
        unit: &'static str,
    },
    NeedsTty {
        step: Option<JsonStep>,
        reason: &'a str,
    },
    TtyReleased {
        step: Option<JsonStep>,
    },
    ActionFinished {
        step: JsonStep,
        description: &'a str,
        outcome: &'static str,
        error: Option<&'a str>,
    },
    StepFinished {
        step: JsonStep,
        outcome: &'static str,
        detail: Option<&'a str>,
        elapsed_ms: u64,
        /// What to do about a failure, from the known-failure table (see `crate::hints`);
        /// left out when none applies.
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<&'static str>,
    },
    BundleFinished {
        bundle: usize,
        name: &'a str,
        outcome: &'static str,
    },
    RunError {
        message: &'a str,
    },
    RunFinished {
        changed: usize,
        ok: usize,
        failed: usize,
        skipped: usize,
        elapsed_ms: u64,
        dry_run: bool,
        success: bool,
        run_error: Option<&'a str>,
        /// What the user has to do after the run (log out and back in, ...).
        notices: &'a [String],
    },
}

#[derive(Serialize)]
struct Line<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    ts: Option<String>,
    #[serde(flatten)]
    event: JsonEvent<'a>,
}

fn outcome_str(o: Outcome) -> &'static str {
    match o {
        Outcome::Changed => "changed",
        Outcome::Ok => "ok",
        Outcome::Skipped => "skipped",
        Outcome::Failed => "failed",
    }
}

fn unit_str(u: Unit) -> &'static str {
    match u {
        Unit::Bytes => "bytes",
        Unit::Items => "items",
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn plan_event(p: &PlanView) -> JsonEvent<'_> {
    let bundles = p
        .bundles
        .iter()
        .map(|b| JsonBundle {
            name: &b.name,
            file: b.file.display().to_string(),
            skip: b.skip.as_deref(),
            steps: b
                .steps
                .iter()
                .map(|s| JsonPlanStep {
                    step: s.id.into(),
                    kind: s.kind,
                    label: &s.label,
                    state: match &s.state {
                        StepViewState::Skipped(r) => JsonStepState::Skipped { reason: r },
                        StepViewState::Error(e) => JsonStepState::Error { error: e },
                        StepViewState::Ready(actions) => JsonStepState::Ready {
                            actions: actions
                                .iter()
                                .map(|a| JsonAction {
                                    description: &a.description,
                                    status: a.status.as_str(),
                                })
                                .collect(),
                        },
                    },
                })
                .collect(),
        })
        .collect();
    JsonEvent::PlanReady {
        bundles,
        secrets: &p.secrets,
        sudo: &p.sudo,
        sudo_steps: &p.sudo_steps,
        tty_sudo_steps: &p.tty_sudo_steps,
    }
}

fn summary_event(s: &Summary) -> JsonEvent<'_> {
    JsonEvent::RunFinished {
        changed: s.changed,
        ok: s.ok,
        failed: s.failed,
        skipped: s.skipped,
        elapsed_ms: millis(s.elapsed),
        dry_run: s.dry_run,
        success: s.success(),
        run_error: s.run_error.as_deref(),
        notices: &s.notices,
    }
}

fn mirror(event: &Event) -> JsonEvent<'_> {
    match event {
        Event::RunStarted {
            facts,
            dry_run,
            profile,
        } => JsonEvent::RunStarted {
            facts: facts.pairs().into_iter().collect(),
            dry_run: *dry_run,
            profile: profile.as_deref(),
        },
        Event::PlanReady(p) => plan_event(p),
        Event::BundleStarted { bundle, name } => JsonEvent::BundleStarted {
            bundle: *bundle,
            name,
        },
        Event::StepStarted {
            step,
            bundle,
            kind,
            label,
        } => JsonEvent::StepStarted {
            step: (*step).into(),
            bundle,
            kind,
            label,
        },
        Event::Output { step, stream, line } => JsonEvent::Output {
            step: (*step).into(),
            stream: stream.as_str(),
            line,
        },
        Event::Progress {
            step,
            done,
            total,
            unit,
        } => JsonEvent::Progress {
            step: (*step).into(),
            done: *done,
            total: *total,
            unit: unit_str(*unit),
        },
        Event::NeedsTty { step, reason } => JsonEvent::NeedsTty {
            step: step.map(Into::into),
            reason,
        },
        Event::TtyReleased { step } => JsonEvent::TtyReleased {
            step: step.map(Into::into),
        },
        Event::ActionFinished {
            step,
            description,
            outcome,
            error,
        } => JsonEvent::ActionFinished {
            step: (*step).into(),
            description,
            outcome: outcome_str(*outcome),
            error: error.as_deref(),
        },
        Event::StepFinished {
            step,
            outcome,
            detail,
            elapsed,
        } => JsonEvent::StepFinished {
            step: (*step).into(),
            outcome: outcome_str(*outcome),
            detail: detail.as_deref(),
            elapsed_ms: millis(*elapsed),
            hint: None,
        },
        Event::BundleFinished {
            bundle,
            name,
            outcome,
        } => JsonEvent::BundleFinished {
            bundle: *bundle,
            name,
            outcome: outcome_str(*outcome),
        },
        Event::RunError(message) => JsonEvent::RunError { message },
        Event::RunFinished(s) => summary_event(s),
    }
}

/// Replaces every string in `v` (not keys) with its redacted form.
fn redact_value(v: &mut Value, redactor: &Redactor) {
    match v {
        Value::String(s) => {
            if let std::borrow::Cow::Owned(r) = redactor.redact(s) {
                *s = r;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| redact_value(v, redactor)),
        Value::Object(o) => o.values_mut().for_each(|v| redact_value(v, redactor)),
        _ => {}
    }
}

pub struct JsonRenderer {
    out: Box<dyn Write + Send>,
    redactor: Redactor,
    clock: Option<fn() -> SystemTime>,
    /// The last output lines of each running step, for the failure hint.
    tails: BTreeMap<StepId, VecDeque<String>>,
}

impl JsonRenderer {
    pub fn new(out: Box<dyn Write + Send>, redactor: Redactor) -> Self {
        Self {
            out,
            redactor,
            clock: Some(SystemTime::now),
            tails: BTreeMap::new(),
        }
    }

    /// Add a `"ts"` field to every line (on by default).
    // Used by tests; a builder option the CLI does not need yet.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn timestamps(mut self, yes: bool) -> Self {
        self.clock = yes.then_some(SystemTime::now as fn() -> SystemTime);
        self
    }

    /// The JSON line for `event`, without the newline; a failed step's gets `hint`.
    fn encode(&self, event: &Event, hint: Option<&'static str>) -> String {
        let mut event = mirror(event);
        if let JsonEvent::StepFinished { hint: h, .. } = &mut event {
            *h = hint;
        }
        let line = Line {
            ts: self
                .clock
                .map(|now| humantime::format_rfc3339_millis(now()).to_string()),
            event,
        };
        let mut v = serde_json::to_value(&line).unwrap_or(Value::Null);
        redact_value(&mut v, &self.redactor);
        v.to_string()
    }
}

impl Renderer for JsonRenderer {
    fn handle(&mut self, event: &Event) {
        let mut hint = None;
        match event {
            Event::StepStarted { step, .. } => {
                self.tails.remove(step);
            }
            Event::Output { step, line, .. } => {
                let tail = self.tails.entry(*step).or_default();
                if tail.len() == TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line.clone());
            }
            Event::StepFinished {
                step,
                outcome,
                detail,
                ..
            } => {
                let tail = self.tails.remove(step).unwrap_or_default();
                if *outcome == Outcome::Failed {
                    hint = crate::hints::hint(detail.as_deref(), tail.iter().map(String::as_str));
                }
            }
            _ => {}
        }
        let line = self.encode(event, hint);
        // Output errors (a closed pipe) must not abort the run.
        let _ = writeln!(self.out, "{line}");
        if matches!(event, Event::NeedsTty { .. } | Event::RunFinished(_)) {
            let _ = self.out.flush();
        }
    }

    fn finish(&mut self) {
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use dot_core::event::{ActionView, BundleView, StepView};
    use dot_core::{Facts, Status, Stream};
    use serde_json::json;

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

    const ID: StepId = StepId { bundle: 0, step: 1 };

    fn render(events: &[Event]) -> Vec<Value> {
        let buf = Buf::default();
        let redactor = Redactor::new();
        redactor.add("s3cret");
        let mut r = JsonRenderer::new(Box::new(buf.clone()), redactor).timestamps(false);
        for e in events {
            r.handle(e);
        }
        r.finish();
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(text.ends_with('\n'));
        text.lines()
            .map(|l| serde_json::from_str(l).expect("one JSON object per line"))
            .collect()
    }

    #[test]
    fn one_object_per_event() {
        let events = [
            Event::BundleStarted {
                bundle: 0,
                name: "nodejs".into(),
            },
            Event::StepStarted {
                step: ID,
                bundle: "nodejs".into(),
                kind: "shell",
                label: "Install fnm".into(),
            },
            Event::Output {
                step: ID,
                stream: Stream::Stderr,
                line: "token=s3cret\n\"quoted\"".into(),
            },
            Event::Progress {
                step: ID,
                done: 10,
                total: None,
                unit: Unit::Bytes,
            },
            Event::NeedsTty {
                step: None,
                reason: "sudo".into(),
            },
            Event::TtyReleased { step: Some(ID) },
            Event::ActionFinished {
                step: ID,
                description: "run `install.sh`".into(),
                outcome: Outcome::Failed,
                error: Some("exit 2".into()),
            },
            Event::StepFinished {
                step: ID,
                outcome: Outcome::Failed,
                detail: Some("exit 2".into()),
                elapsed: Duration::from_millis(1500),
            },
            Event::BundleFinished {
                bundle: 0,
                name: "nodejs".into(),
                outcome: Outcome::Failed,
            },
            Event::RunError("no".into()),
            Event::RunFinished(Summary {
                failed: 1,
                elapsed: Duration::from_secs(2),
                notices: vec!["log out and back in".into()],
                ..Summary::default()
            }),
        ];
        let got = render(&events);
        let step = json!({"bundle": 0, "step": 1});
        assert_eq!(
            got,
            vec![
                json!({"event": "bundle_started", "bundle": 0, "name": "nodejs"}),
                json!({"event": "step_started", "step": step, "bundle": "nodejs",
                       "kind": "shell", "label": "Install fnm"}),
                json!({"event": "output", "step": step, "stream": "stderr",
                       "line": "token=***\n\"quoted\""}),
                json!({"event": "progress", "step": step, "done": 10, "total": null,
                       "unit": "bytes"}),
                json!({"event": "needs_tty", "step": null, "reason": "sudo"}),
                json!({"event": "tty_released", "step": step}),
                json!({"event": "action_finished", "step": step,
                       "description": "run `install.sh`", "outcome": "failed",
                       "error": "exit 2"}),
                json!({"event": "step_finished", "step": step, "outcome": "failed",
                       "detail": "exit 2", "elapsed_ms": 1500}),
                json!({"event": "bundle_finished", "bundle": 0, "name": "nodejs",
                       "outcome": "failed"}),
                json!({"event": "run_error", "message": "no"}),
                json!({"event": "run_finished", "changed": 0, "ok": 0, "failed": 1,
                       "skipped": 0, "elapsed_ms": 2000, "dry_run": false,
                       "success": false, "run_error": null,
                       "notices": ["log out and back in"]}),
            ]
        );
    }

    #[test]
    fn run_started_and_plan() {
        let plan = PlanView {
            bundles: vec![BundleView {
                name: "demo".into(),
                file: "/r/bundle.yml".into(),
                skip: None,
                steps: vec![
                    StepView {
                        id: StepId { bundle: 0, step: 0 },
                        kind: "link",
                        label: "link".into(),
                        state: StepViewState::Ready(vec![ActionView {
                            description: "link ~/.x -> x".into(),
                            status: Status::Unknown,
                        }]),
                    },
                    StepView {
                        id: ID,
                        kind: "shell",
                        label: "make".into(),
                        state: StepViewState::Skipped("when: os=windows".into()),
                    },
                ],
            }],
            secrets: vec!["gitlab_token".into()],
            sudo: vec![],
            sudo_steps: vec![],
            tty_sudo_steps: vec![],
        };
        let got = render(&[
            Event::RunStarted {
                facts: Facts::detect(),
                dry_run: true,
                profile: None,
            },
            Event::PlanReady(plan),
        ]);
        assert_eq!(got[0]["event"], "run_started");
        assert_eq!(got[0]["dry_run"], true);
        assert!(got[0]["facts"]["os"].is_string(), "{}", got[0]);
        assert_eq!(
            got[1],
            json!({"event": "plan_ready", "secrets": ["gitlab_token"], "bundles": [{
            "name": "demo", "file": "/r/bundle.yml", "steps": [
                {"step": {"bundle": 0, "step": 0}, "kind": "link", "label": "link",
                 "state": "ready",
                 "actions": [{"description": "link ~/.x -> x", "status": "unknown"}]},
                {"step": step_json(ID), "kind": "shell", "label": "make",
                 "state": "skipped", "reason": "when: os=windows"},
            ]}]})
        );
    }

    fn step_json(s: StepId) -> Value {
        json!({"bundle": s.bundle, "step": s.step})
    }

    #[test]
    fn a_known_failure_has_a_hint() {
        let got = render(&[
            Event::StepStarted {
                step: ID,
                bundle: "tools".into(),
                kind: "package",
                label: "package".into(),
            },
            Event::Output {
                step: ID,
                stream: Stream::Stderr,
                line: "E: Unable to locate package fd-find".into(),
            },
            Event::StepFinished {
                step: ID,
                outcome: Outcome::Failed,
                detail: Some("apt-get install failed".into()),
                elapsed: Duration::ZERO,
            },
        ]);
        let hint = got[2]["hint"].as_str().expect("hint");
        assert!(hint.starts_with("apt does not know this package"), "{hint}");
    }

    #[test]
    fn timestamps_by_default() {
        let buf = Buf::default();
        let mut r = JsonRenderer::new(Box::new(buf.clone()), Redactor::new());
        r.handle(&Event::RunError("x".into()));
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let v: Value = serde_json::from_str(text.trim_end()).unwrap();
        let ts = v["ts"].as_str().unwrap();
        assert!(humantime::parse_rfc3339(ts).is_ok(), "{ts}");
    }
}
