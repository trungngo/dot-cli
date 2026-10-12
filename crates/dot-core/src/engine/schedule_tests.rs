//! `--jobs N`: bundles run at the same time as far as the schedule allows.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};

use super::*;
use crate::config::parse_bundle;
use crate::event::Recorder;
use crate::facts::{Os, Role};
use crate::paths::tests::abs;
use crate::runenv::EnvVar;

/// What the fake actions share.
#[derive(Default)]
struct Shared {
    applied: Mutex<Vec<String>>,
    /// Actions applying right now.
    active: AtomicUsize,
    /// The most that were applying at once.
    peak: AtomicUsize,
    /// `active` as each `tty:` action saw it, while it ran.
    tty_seen: Mutex<Vec<usize>>,
    /// `meet:` actions waiting for each other.
    meeting: (Mutex<usize>, Condvar),
}

/// Plans each `shell` command word as a fake action:
///
/// - `do:<n>` is applied (recorded in `applied`), `fail:<n>` fails;
/// - `after:<x>` fails unless `<x>` was applied before it;
/// - `slow:<n>` takes a while;
/// - `meet:<n>` waits (up to 5s, then fails) until two `meet:` actions are applying at once;
/// - `tty:<n>` hands the terminal over ([`Action::needs_tty`]), `sudo:<n>` needs root;
///   `asks:<n>` needs root through a program that asks for the password itself
///   ([`Action::asks_for_sudo`]);
/// - `setenv:<VAR>` records `VAR` in the run environment for this bundle while planning;
///   `sees:<VAR>` fails unless its commands would get `VAR`.
#[derive(Default)]
struct Planner {
    shared: Arc<Shared>,
}

struct Fake {
    name: String,
    shared: Arc<Shared>,
    env: RunEnvCell,
}

impl Action for Fake {
    fn describe(&self) -> String {
        self.name.clone()
    }

    fn status(&self) -> Result<Status> {
        Ok(Status::Pending)
    }

    fn needs_tty(&self) -> bool {
        self.name.starts_with("tty:")
    }

    fn needs_sudo(&self) -> bool {
        self.name.starts_with("sudo:") || self.asks_for_sudo()
    }

    fn asks_for_sudo(&self) -> bool {
        self.name.starts_with("asks:")
    }

    fn apply(&self, _ctx: &RunCtx) -> Result<Outcome> {
        let s = &self.shared;
        let now = s.active.fetch_add(1, Ordering::SeqCst) + 1;
        s.peak.fetch_max(now, Ordering::SeqCst);
        let result = self.work();
        s.active.fetch_sub(1, Ordering::SeqCst);
        result?;
        s.applied.lock().unwrap().push(self.name.clone());
        Ok(Outcome::Changed)
    }
}

impl Fake {
    fn work(&self) -> Result<()> {
        let s = &self.shared;
        let (kind, arg) = self.name.split_once(':').unwrap_or((&self.name, ""));
        match kind {
            "fail" => bail!("boom"),
            "after" if !s.applied.lock().unwrap().iter().any(|a| a == arg) => {
                bail!("{arg} has not run yet")
            }
            "slow" => std::thread::sleep(Duration::from_millis(60)),
            "tty" | "sudo" => {
                for _ in 0..3 {
                    s.tty_seen
                        .lock()
                        .unwrap()
                        .push(s.active.load(Ordering::SeqCst));
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            "meet" => {
                let (lock, cv) = &s.meeting;
                let mut n = lock.lock().unwrap();
                *n += 1;
                cv.notify_all();
                let (n, timeout) = cv
                    .wait_timeout_while(n, Duration::from_secs(5), |n| *n < 2)
                    .unwrap();
                drop(n);
                if timeout.timed_out() {
                    bail!("nobody to meet: bundles did not run at the same time");
                }
            }
            "sees" if !self.env.child_env().iter().any(|(k, _)| k == arg) => {
                bail!("{arg} is not visible")
            }
            _ => {}
        }
        Ok(())
    }
}

impl super::Planner for Planner {
    fn plan(&self, directive: &Directive, ctx: &PlanCtx<'_>) -> Result<Vec<Box<dyn Action>>> {
        let Directive::Shell(s) = directive else {
            bail!("fake planner only handles shell")
        };
        let mut out: Vec<Box<dyn Action>> = Vec::new();
        for word in s.cmd.split_whitespace() {
            if let Some(var) = word.strip_prefix("setenv:") {
                ctx.run_env().update(|e| {
                    e.set_var(EnvVar {
                        name: var.into(),
                        value: "1".into(),
                        bundle: ctx.bundle.name.clone(),
                    })
                });
            }
            out.push(Box::new(Fake {
                name: word.to_string(),
                shared: self.shared.clone(),
                env: ctx.run_env().clone(),
            }));
        }
        Ok(out)
    }
}

fn bundle(name: &str, steps: &[&str]) -> Bundle {
    let file = abs(&format!("/nonexistent-dot-test/{name}/bundle.yml"));
    let mut yaml = String::from("steps:\n");
    for s in steps {
        yaml.push_str(&format!("  - shell: {s}\n"));
    }
    parse_bundle(&yaml, &file).unwrap()
}

fn ctx() -> Context {
    Context::new(
        Facts::new(Os::Linux, "x86_64", Role::Server),
        abs("/nonexistent-home"),
    )
}

struct Ran {
    summary: Summary,
    events: Vec<Event>,
    shared: Arc<Shared>,
}

impl Ran {
    fn applied(&self) -> Vec<String> {
        self.shared.applied.lock().unwrap().clone()
    }

    fn outcome(&self, bundle: usize, step: usize) -> Outcome {
        self.events
            .iter()
            .find_map(|e| match e {
                Event::StepFinished {
                    step: id, outcome, ..
                } if *id == (StepId { bundle, step }) => Some(*outcome),
                _ => None,
            })
            .expect("step finished")
    }
}

fn run_jobs(bundles: &[Bundle], schedule: &Schedule, jobs: usize) -> Ran {
    let planner = Planner::default();
    let rec = Recorder::default();
    let summary = run_scheduled(
        bundles,
        schedule,
        &planner,
        &ctx(),
        &EventSink::new().with(Box::new(rec.clone())),
        Options {
            jobs,
            ..Options::default()
        },
        &mut NoPrepare,
    );
    Ran {
        summary,
        events: rec.events(),
        shared: planner.shared,
    }
}

fn independent(n: usize) -> Schedule {
    Schedule {
        requires: vec![Vec::new(); n],
        after: vec![Vec::new(); n],
    }
}

#[test]
fn independent_bundles_run_at_the_same_time() {
    let bundles = [bundle("a", &["meet:a"]), bundle("b", &["meet:b"])];
    let ran = run_jobs(&bundles, &independent(2), 2);
    assert!(ran.summary.success(), "{:?}", ran.events);
    assert_eq!(ran.shared.peak.load(Ordering::SeqCst), 2);
    assert_eq!(
        ran.summary.completed,
        ["a", "b"],
        "plan order, not finish order"
    );
}

#[test]
fn one_job_runs_one_bundle_at_a_time_in_order() {
    let bundles = [
        bundle("a", &["slow:a", "do:a"]),
        bundle("b", &["after:do:a"]),
    ];
    let ran = run_jobs(&bundles, &independent(2), 1);
    assert!(ran.summary.success(), "{:?}", ran.events);
    assert_eq!(ran.shared.peak.load(Ordering::SeqCst), 1);
    // The same event order as before `--jobs`: each bundle from start to finish.
    let order: Vec<String> = ran
        .events
        .iter()
        .filter_map(|e| match e {
            Event::BundleStarted { name, .. } => Some(format!("start {name}")),
            Event::BundleFinished { name, .. } => Some(format!("end {name}")),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["start a", "end a", "start b", "end b"]);
}

#[test]
fn requires_and_after_edges_are_waited_for() {
    // b requires a, c runs after a; d is free.
    let bundles = [
        bundle("a", &["slow:a", "do:a"]),
        bundle("b", &["after:do:a"]),
        bundle("c", &["after:do:a"]),
        bundle("d", &["do:d"]),
    ];
    let schedule = Schedule {
        requires: vec![vec![], vec![0], vec![], vec![]],
        after: vec![vec![], vec![], vec![0], vec![]],
    };
    let ran = run_jobs(&bundles, &schedule, 4);
    assert!(ran.summary.success(), "{:?}", ran.events);
    let applied = ran.applied();
    assert_eq!(applied.len(), 5, "{applied:?}");
    // d did not wait for the slow bundle a.
    let pos = |n: &str| applied.iter().position(|a| a == n).unwrap();
    assert!(pos("do:d") < pos("do:a"), "{applied:?}");
}

#[test]
fn failure_skips_hard_dependents_only() {
    let bundles = [
        bundle("a", &["fail:a"]),
        bundle("b", &["do:b"]),
        bundle("c", &["do:c"]),
        bundle("e", &["do:e"]),
    ];
    let schedule = Schedule {
        requires: vec![vec![], vec![0], vec![], vec![1]],
        after: vec![vec![], vec![], vec![0], vec![]],
    };
    let ran = run_jobs(&bundles, &schedule, 3);
    assert_eq!(ran.outcome(0, 0), Outcome::Failed);
    assert_eq!(ran.outcome(1, 0), Outcome::Skipped);
    assert_eq!(ran.outcome(3, 0), Outcome::Skipped, "transitively");
    assert_eq!(ran.outcome(2, 0), Outcome::Changed, "after only orders");
    let (s, applied) = (&ran.summary, ran.applied());
    assert_eq!((s.changed, s.failed, s.skipped), (1, 1, 2), "{s:?}");
    assert_eq!(applied, ["do:c"]);
    assert_eq!(s.completed, ["c"]);
    let detail = ran.events.iter().find_map(|e| match e {
        Event::StepFinished {
            step: StepId { bundle: 3, .. },
            detail,
            ..
        } => detail.clone(),
        _ => None,
    });
    assert_eq!(detail.as_deref(), Some("dependency a failed"));
}

#[test]
fn steps_that_need_the_terminal_or_root_run_alone() {
    let bundles = [
        bundle("a", &["slow:a1", "slow:a2", "slow:a3"]),
        bundle("b", &["slow:b1", "tty:b", "slow:b2"]),
        bundle("c", &["slow:c1", "sudo:c", "slow:c2"]),
        bundle("d", &["slow:d1", "slow:d2", "slow:d3"]),
    ];
    let ran = run_jobs(&bundles, &independent(4), 4);
    assert!(ran.summary.success(), "{:?}", ran.events);
    assert!(ran.shared.peak.load(Ordering::SeqCst) > 1, "others overlap");
    let seen = ran.shared.tty_seen.lock().unwrap().clone();
    assert_eq!(seen, [1; 6], "nothing else ran meanwhile");
}

#[test]
fn run_environment_entries_are_seen_along_edges_only() {
    let bundles = [
        bundle("a", &["setenv:FROM_A"]),
        bundle("b", &["sees:FROM_A"]),
        bundle("c", &["sees:FROM_A"]),
    ];
    // b runs after a; c has no edge to a, so it must not depend on timing.
    let schedule = Schedule {
        requires: vec![vec![], vec![], vec![]],
        after: vec![vec![], vec![0], vec![1]],
    };
    let ran = run_jobs(&bundles, &schedule, 3);
    assert_eq!(ran.outcome(1, 0), Outcome::Changed);
    assert_eq!(ran.outcome(2, 0), Outcome::Changed, "transitively, via b");

    let ran = run_jobs(&bundles, &independent(3), 1);
    assert_eq!(
        ran.outcome(1, 0),
        Outcome::Failed,
        "no edge, even one at a time"
    );
    let ran = run_jobs(&bundles, &Schedule::chain(3), 3);
    assert!(ran.summary.success(), "a chain sees everything before it");
}

/// Records each [`SudoCheck::before_sudo`]; fails for `sudo:no...` actions.
#[derive(Default)]
struct Check(Mutex<Vec<String>>);

impl SudoCheck for Check {
    fn before_sudo(&self, _sink: &EventSink, step: StepId, what: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .push(format!("{}.{} {what}", step.bundle, step.step));
        if what.starts_with("sudo:no") {
            bail!("sudo asks for a password again");
        }
        Ok(())
    }
}

#[test]
fn sudo_is_checked_right_before_each_action_that_relies_on_it() {
    let check = Arc::new(Check::default());
    let mut c = ctx();
    c.sudo = Some(check.clone());
    let bundles = [
        bundle("a", &["sudo:a1", "\"asks:a2 do:a3 sudo:a4\""]),
        bundle("b", &["sudo:nob", "do:b2"]),
    ];
    let planner = Planner::default();
    let rec = Recorder::default();
    let summary = run_scheduled(
        &bundles,
        &independent(2),
        &planner,
        &c,
        &EventSink::new().with(Box::new(rec.clone())),
        Options::default(),
        &mut NoPrepare,
    );
    assert_eq!(
        *check.0.lock().unwrap(),
        ["0.0 sudo:a1", "0.1 sudo:a4", "1.0 sudo:nob"],
        "not for a program that asks itself, nor for actions without root"
    );
    let applied = planner.shared.applied.lock().unwrap().clone();
    assert_eq!(applied, ["sudo:a1", "asks:a2", "do:a3", "sudo:a4"]);
    assert!(!summary.success());
    let failed = rec.events().into_iter().find_map(|e| match e {
        Event::ActionFinished {
            description,
            outcome: Outcome::Failed,
            error,
            ..
        } => Some((description, error)),
        _ => None,
    });
    assert_eq!(
        failed,
        Some((
            "sudo:nob".to_string(),
            Some("sudo asks for a password again".to_string())
        ))
    );

    // Dry runs never check.
    let check = Arc::new(Check::default());
    c.sudo = Some(check.clone());
    run_scheduled(
        &bundles,
        &independent(2),
        &Planner::default(),
        &c,
        &EventSink::new(),
        Options {
            dry_run: true,
            ..Options::default()
        },
        &mut NoPrepare,
    );
    assert!(check.0.lock().unwrap().is_empty());
}
