use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};

use super::*;
use crate::config::parse_bundle;
use crate::event::Recorder;
use crate::facts::{Os, Role};
use crate::paths::tests::abs;

/// Plans each `shell` command word as one fake action: `ok:<n>` is satisfied, `do:<n>` is
/// pending, `fail:<n>` fails when applied, `planerr` fails planning, `needs:<x>` is unknown
/// until an action named `<x>` has been applied (and fails if applied before that), and
/// `secret:<name>` requires that secret and is unknown.
#[derive(Default)]
struct FakePlanner {
    applied: Arc<Mutex<Vec<String>>>,
    plans: AtomicUsize,
}

struct FakeAction {
    name: String,
    applied: Arc<Mutex<Vec<String>>>,
}

impl FakeAction {
    fn done(&self, name: &str) -> bool {
        self.applied.lock().unwrap().iter().any(|a| a == name)
    }
}

impl Action for FakeAction {
    fn describe(&self) -> String {
        self.name.clone()
    }
    fn status(&self) -> Result<Status> {
        if let Some(dep) = self.name.strip_prefix("needs:")
            && !self.done(dep)
        {
            return Ok(Status::Unknown);
        }
        if self.name.starts_with("secret:") {
            return Ok(Status::Unknown);
        }
        if self.name.starts_with("conflict:") {
            return Ok(Status::Conflict);
        }
        Ok(if self.name.starts_with("ok:") || self.done(&self.name) {
            Status::Satisfied
        } else {
            Status::Pending
        })
    }
    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        ctx.output(crate::Stream::Stdout, format!("applying {}", self.name));
        if self.name.starts_with("fail:") {
            bail!("boom");
        }
        if self.name.starts_with("panic:") {
            panic!("a directive panicked");
        }
        if self.name.starts_with("conflict:") {
            bail!("in the way");
        }
        if let Some(dep) = self.name.strip_prefix("needs:")
            && !self.done(dep)
        {
            bail!("{dep} is missing");
        }
        self.applied.lock().unwrap().push(self.name.clone());
        Ok(Outcome::Changed)
    }
}

impl Planner for FakePlanner {
    fn plan(&self, directive: &Directive, ctx: &PlanCtx<'_>) -> Result<Vec<Box<dyn Action>>> {
        self.plans.fetch_add(1, Ordering::SeqCst);
        let Directive::Shell(s) = directive else {
            bail!("fake planner only handles shell")
        };
        let cmd = ctx.render(&s.cmd)?;
        let mut out: Vec<Box<dyn Action>> = Vec::new();
        for word in cmd.split_whitespace() {
            if word == "planerr" {
                bail!("cannot plan");
            }
            if let Some(name) = word.strip_prefix("secret:") {
                ctx.require_secret(name, crate::secret::SecretUse::TEXT);
            }
            if let Some(name) = word.strip_prefix("secretfile:") {
                ctx.require_secret(name, crate::secret::SecretUse::FILE);
            }
            out.push(Box::new(FakeAction {
                name: word.to_string(),
                applied: self.applied.clone(),
            }));
        }
        Ok(out)
    }

    /// `ok` is satisfied, `done:<x>` once `<x>` was applied, `boom` cannot run, anything else
    /// is not satisfied.
    fn check(&self, command: &str, _bundle: &Bundle, env: &[(String, String)]) -> Result<bool> {
        if command == "boom" {
            bail!("cannot run");
        }
        if let Some(x) = command.strip_prefix("done:") {
            return Ok(self.applied.lock().unwrap().iter().any(|a| a == x));
        }
        if let Some(var) = command.strip_prefix("env:") {
            return Ok(env.iter().any(|(k, _)| k == var));
        }
        Ok(command == "ok")
    }
}

fn ctx() -> Context {
    let mut facts = Facts::new(Os::Linux, "x86_64", Role::Server);
    facts.distro = Some("ubuntu".into());
    Context::new(facts, abs("/nonexistent-home"))
}

fn bundle(name: &str, yaml: &str) -> Bundle {
    let file = abs(&format!("/nonexistent-dot-test/{name}/bundle.yml"));
    parse_bundle(yaml, &file).unwrap()
}

fn run_fake(bundles: &[Bundle], dry_run: bool) -> (Summary, Vec<String>, Vec<Event>) {
    let planner = FakePlanner::default();
    let rec = Recorder::default();
    let sink = EventSink::new().with(Box::new(rec.clone()));
    let summary = run(
        bundles,
        &planner,
        &ctx(),
        &sink,
        Options {
            dry_run,
            ..Default::default()
        },
    );
    let applied = planner.applied.lock().unwrap().clone();
    (summary, applied, rec.events())
}

fn step_outcomes(events: &[Event]) -> Vec<(StepId, Outcome)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::StepFinished { step, outcome, .. } => Some((*step, *outcome)),
            _ => None,
        })
        .collect()
}

#[test]
fn applies_pending_and_skips_satisfied() {
    let b = bundle(
        "a",
        "steps:\n  - shell: ok:1 do:2\n  - shell: ok:3\n  - shell: do:4\n",
    );
    let (summary, applied, events) = run_fake(&[b], false);
    assert_eq!(applied, ["do:2", "do:4"]);
    assert_eq!((summary.changed, summary.ok, summary.failed), (2, 1, 0));
    assert!(summary.success());
    assert!(matches!(events.first(), Some(Event::RunStarted { .. })));
    assert!(matches!(events[1], Event::PlanReady(_)));
    assert!(matches!(events.last(), Some(Event::RunFinished(_))));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Output { line, .. } if line == "applying do:2"))
    );
}

#[test]
fn dry_run_changes_nothing() {
    let b = bundle("a", "steps:\n  - shell: do:1 ok:2\n  - shell: fail:3\n");
    let (summary, applied, events) = run_fake(&[b], true);
    assert!(applied.is_empty());
    assert!(summary.dry_run);
    assert_eq!((summary.changed, summary.failed), (2, 0));
    assert!(!events.iter().any(|e| matches!(e, Event::Output { .. })));
}

#[test]
fn failure_skips_rest_of_bundle_but_not_other_bundles() {
    let a = bundle(
        "a",
        "steps:\n  - shell: do:1\n  - shell: fail:2 do:never\n  - shell: do:3\n",
    );
    let b = bundle("b", "steps:\n  - shell: do:4\n");
    let (summary, applied, events) = run_fake(&[a, b], false);
    assert_eq!(applied, ["do:1", "do:4"]);
    assert_eq!(
        (summary.changed, summary.failed, summary.skipped),
        (2, 1, 1)
    );
    assert!(!summary.success());
    let outcomes = step_outcomes(&events);
    assert_eq!(outcomes[1].1, Outcome::Failed);
    assert_eq!(outcomes[2].1, Outcome::Skipped);
    let failed_detail = events.iter().find_map(|e| match e {
        Event::StepFinished {
            outcome: Outcome::Failed,
            detail,
            ..
        } => detail.clone(),
        _ => None,
    });
    assert_eq!(failed_detail.as_deref(), Some("boom"));
    let bundle_outcomes: Vec<Outcome> = events
        .iter()
        .filter_map(|e| match e {
            Event::BundleFinished { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .collect();
    assert_eq!(bundle_outcomes, [Outcome::Failed, Outcome::Changed]);
}

#[test]
fn when_skips_steps_and_bundles() {
    let a = bundle(
        "a",
        "steps:\n  - shell: do:1\n    when: { os: macos }\n  - shell: do:2\n    when: { os: linux, role: server }\n",
    );
    let b = bundle("b", "when: { role: desktop }\nsteps:\n  - shell: do:3\n");
    let (summary, applied, events) = run_fake(&[a, b], false);
    assert_eq!(applied, ["do:2"]);
    assert_eq!((summary.changed, summary.skipped), (1, 2));
    let Some(Event::PlanReady(view)) = events.get(1) else {
        panic!()
    };
    assert_eq!(
        view.bundles[1].skip.as_deref(),
        Some("when: role is server, not desktop")
    );
    let detail = events.iter().find_map(|e| match e {
        Event::StepFinished {
            outcome: Outcome::Skipped,
            detail,
            ..
        } => detail.clone(),
        _ => None,
    });
    assert_eq!(detail.as_deref(), Some("when: os is linux, not macos"));
}

#[test]
fn planning_error_blocks_the_whole_bundle() {
    let a = bundle("a", "steps:\n  - shell: do:1\n  - shell: planerr\n");
    let b = bundle("b", "steps:\n  - shell: do:2\n");
    let (summary, applied, events) = run_fake(&[a, b], false);
    assert_eq!(applied, ["do:2"]);
    assert_eq!(
        (summary.changed, summary.failed, summary.skipped),
        (1, 1, 1)
    );
    let Some(Event::PlanReady(view)) = events.get(1) else {
        panic!()
    };
    assert!(view.has_errors());
}

#[test]
fn plan_is_pure() {
    let a = bundle("a", "steps:\n  - shell: do:1 fail:2\n");
    let planner = FakePlanner::default();
    let plan = plan(&[a], &planner, &ctx());
    assert!(planner.applied.lock().unwrap().is_empty());
    let view = plan.view();
    let crate::event::StepViewState::Ready(actions) = &view.bundles[0].steps[0].state else {
        panic!()
    };
    assert_eq!(actions.len(), 2);
    assert!(actions.iter().all(|a| a.status == Status::Pending));
}

#[test]
fn steps_are_replanned_after_earlier_steps_run() {
    // Step 2 depends on step 1's effect: unknown in the preview, fine when it actually runs.
    let a = bundle("a", "steps:\n  - shell: do:1\n  - shell: needs:do:1\n");
    let planner = FakePlanner::default();
    let rec = Recorder::default();
    let sink = EventSink::new().with(Box::new(rec.clone()));
    let summary = run(
        std::slice::from_ref(&a),
        &planner,
        &ctx(),
        &sink,
        Options::default(),
    );
    assert!(summary.success(), "{summary:?}");
    assert_eq!(summary.changed, 2);
    assert_eq!(*planner.applied.lock().unwrap(), ["do:1", "needs:do:1"]);
    // Two steps planned for the preview, then each again right before it ran.
    assert_eq!(planner.plans.load(Ordering::SeqCst), 4);
    let Some(Event::PlanReady(view)) = rec.events().get(1).cloned() else {
        panic!()
    };
    let StepViewState::Ready(actions) = &view.bundles[0].steps[1].state else {
        panic!()
    };
    assert_eq!(actions[0].status, Status::Unknown);
    assert!(!view.has_errors());
}

#[test]
fn unknown_that_stays_unknown_fails_when_applied() {
    let a = bundle("a", "steps:\n  - shell: needs:never\n  - shell: do:2\n");
    let (summary, applied, events) = run_fake(&[a], false);
    assert!(applied.is_empty());
    assert_eq!((summary.failed, summary.skipped), (1, 1));
    let detail = events.iter().find_map(|e| match e {
        Event::StepFinished {
            outcome: Outcome::Failed,
            detail,
            ..
        } => detail.clone(),
        _ => None,
    });
    assert_eq!(detail.as_deref(), Some("never is missing"));
}

#[test]
fn conflicts_are_would_fails_in_dry_runs_and_fail_when_applied() {
    let a = bundle(
        "a",
        "steps:\n  - shell: do:1 conflict:2 do:3\n  - shell: do:4\n",
    );
    let b = bundle("b", "steps:\n  - shell: do:5\n");
    let (summary, applied, events) = run_fake(&[a.clone(), b.clone()], true);
    assert!(applied.is_empty());
    // As in a real run: the rest of `a` is skipped, `b` goes on.
    assert_eq!(
        (summary.changed, summary.failed, summary.skipped),
        (1, 1, 1)
    );
    assert!(events.iter().any(|e| matches!(
        e,
        Event::StepFinished { outcome: Outcome::Failed, detail: Some(d), .. }
            if d == "would fail: conflict:2"
    )));
    assert!(summary.completed.is_empty(), "a dry run completes nothing");

    let plan = super::plan(std::slice::from_ref(&a), &FakePlanner::default(), &ctx());
    let PlannedState::Ready(actions) = &plan.bundles[0].steps[0].state else {
        panic!("planned")
    };
    let statuses: Vec<Status> = actions.iter().map(|a| a.status).collect();
    assert_eq!(
        statuses,
        [Status::Pending, Status::Conflict, Status::Pending]
    );
    assert!(!Status::Conflict.has_work());

    let (summary, applied, _) = run_fake(&[a, b], false);
    assert_eq!(applied, ["do:1", "do:5"]);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.completed, ["b"], "only bundles that ran to the end");
}

#[test]
fn dry_run_reports_unknown_as_would_change() {
    let a = bundle("a", "steps:\n  - shell: do:1\n  - shell: needs:do:1\n");
    let (summary, applied, _) = run_fake(&[a], true);
    assert!(applied.is_empty());
    assert_eq!((summary.changed, summary.failed), (2, 0));
}

struct RecordingPrepare {
    seen: Vec<String>,
    fail: bool,
}

impl Prepare for RecordingPrepare {
    fn prepare(&mut self, plan: &Plan, _sink: &EventSink) -> Result<()> {
        self.seen = plan.secret_names();
        if self.fail {
            bail!("vault is locked");
        }
        Ok(())
    }
}

#[test]
fn planning_collects_secrets_without_resolving_them() {
    let a = bundle(
        "a",
        "steps:\n  - shell: secret:gitlab_token do:1\n  - shell: secret:vpn secret:gitlab_token\n",
    );
    let planner = FakePlanner::default();
    let plan = plan(std::slice::from_ref(&a), &planner, &ctx());
    assert_eq!(plan.secret_names(), ["gitlab_token", "vpn"]);
    assert_eq!(plan.view().secrets, ["gitlab_token", "vpn"]);
    assert!(!plan.has_errors());

    // A real run hands them to the prepare stage before any step runs...
    let mut prep = RecordingPrepare {
        seen: vec![],
        fail: false,
    };
    let summary = run_with(
        std::slice::from_ref(&a),
        &planner,
        &ctx(),
        &EventSink::new(),
        Options::default(),
        &mut prep,
    );
    assert_eq!(prep.seen, ["gitlab_token", "vpn"]);
    assert!(summary.success(), "{summary:?}");

    // ...a dry run never prepares (no unlock prompts)...
    let mut prep = RecordingPrepare {
        seen: vec![],
        fail: true,
    };
    let summary = run_with(
        std::slice::from_ref(&a),
        &planner,
        &ctx(),
        &EventSink::new(),
        Options {
            dry_run: true,
            ..Default::default()
        },
        &mut prep,
    );
    assert!(prep.seen.is_empty() && summary.success());

    // ...and a failed prepare applies nothing.
    let planner = FakePlanner::default();
    let rec = Recorder::default();
    let summary = run_with(
        std::slice::from_ref(&a),
        &planner,
        &ctx(),
        &EventSink::new().with(Box::new(rec.clone())),
        Options::default(),
        &mut prep,
    );
    assert!(!summary.success());
    assert_eq!(summary.run_error.as_deref(), Some("vault is locked"));
    assert_eq!(summary.skipped, 2);
    assert!(planner.applied.lock().unwrap().is_empty());
    assert!(
        rec.events()
            .iter()
            .any(|e| matches!(e, Event::RunError(m) if m == "vault is locked"))
    );
}

#[test]
fn templates_render_at_plan_time_with_located_errors() {
    let a = bundle(
        "a",
        "steps:\n  - shell: do:{{ os }}\n    when: { role: \"{{ role }}\" }\n  - shell: do:{{ nope }}\n",
    );
    let planner = FakePlanner::default();
    let view = plan(std::slice::from_ref(&a), &planner, &ctx()).view();
    let StepViewState::Ready(actions) = &view.bundles[0].steps[0].state else {
        panic!("{:?}", view.bundles[0].steps[0].state)
    };
    assert_eq!(actions[0].description, "do:linux");
    let StepViewState::Error(e) = &view.bundles[0].steps[1].state else {
        panic!()
    };
    assert!(
        e.contains("bundle.yml:4:5: steps[1]: undefined variable `nope`"),
        "{e}"
    );

    let b = bundle(
        "b",
        "when: { os: \"{{ nope }}\" }\nsteps:\n  - shell: do:1\n",
    );
    let view = plan(std::slice::from_ref(&b), &planner, &ctx()).view();
    let StepViewState::Error(e) = &view.bundles[0].steps[0].state else {
        panic!()
    };
    assert!(e.contains("when: undefined variable `nope`"), "{e}");
}

#[test]
fn plan_and_sink_can_be_shared_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Plan>();
    assert_send_sync::<EventSink>();
    assert_send_sync::<Box<dyn Action>>();
}

fn run_deps(
    bundles: &[Bundle],
    deps: &[Vec<usize>],
    opts: Options,
) -> (Summary, Vec<String>, Vec<Event>) {
    let planner = FakePlanner::default();
    let rec = Recorder::default();
    let sink = EventSink::new().with(Box::new(rec.clone()));
    let summary = run_graph(bundles, deps, &planner, &ctx(), &sink, opts, &mut NoPrepare);
    let applied = planner.applied.lock().unwrap().clone();
    (summary, applied, rec.events())
}

fn bundle_outcomes(events: &[Event]) -> Vec<(String, Outcome)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::BundleFinished { name, outcome, .. } => Some((name.clone(), *outcome)),
            _ => None,
        })
        .collect()
}

#[test]
fn dependents_of_a_failed_bundle_are_skipped_others_continue() {
    // base fails; mid needs base; top needs mid; other is unrelated.
    let bundles = [
        bundle("base", "steps:\n  - shell: fail:1\n"),
        bundle("other", "steps:\n  - shell: do:2\n"),
        bundle("mid", "steps:\n  - shell: do:3\n  - shell: do:4\n"),
        bundle("top", "steps:\n  - shell: do:5\n"),
    ];
    let deps = [vec![], vec![], vec![0], vec![2]];
    let (summary, applied, events) = run_deps(&bundles, &deps, Options::default());
    assert_eq!(applied, ["do:2"]);
    assert_eq!(
        (summary.failed, summary.changed, summary.skipped),
        (1, 1, 3)
    );
    assert_eq!(
        bundle_outcomes(&events),
        [
            ("base".to_string(), Outcome::Failed),
            ("other".to_string(), Outcome::Changed),
            ("mid".to_string(), Outcome::Skipped),
            ("top".to_string(), Outcome::Skipped),
        ]
    );
    let details: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::StepFinished {
                outcome: Outcome::Skipped,
                detail,
                ..
            } => detail.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(
        details,
        [
            "dependency base failed",
            "dependency base failed",
            "dependency base failed"
        ]
    );
}

#[test]
fn planning_errors_also_block_dependents() {
    let bundles = [
        bundle("a", "steps:\n  - shell: planerr\n"),
        bundle("b", "steps:\n  - shell: do:1\n"),
    ];
    let (summary, applied, _) = run_deps(&bundles, &[vec![], vec![0]], Options::default());
    assert!(applied.is_empty());
    assert_eq!((summary.failed, summary.skipped), (1, 1));
}

#[test]
fn check_satisfied_bundles_are_skipped_unless_forced() {
    let bundles = [
        bundle("a", "check: ok\nsteps:\n  - shell: do:1\n"),
        bundle("b", "check: \"no\"\nsteps:\n  - shell: do:2\n"),
    ];
    let (summary, applied, events) = run_deps(&bundles, &[], Options::default());
    assert_eq!(applied, ["do:2"]);
    assert_eq!((summary.changed, summary.skipped), (1, 1));
    let Some(Event::PlanReady(view)) = events.get(1) else {
        panic!()
    };
    assert!(
        view.bundles[0]
            .skip
            .as_deref()
            .unwrap()
            .contains("already satisfied: check `ok` succeeded"),
        "{:?}",
        view.bundles[0].skip
    );
    let (_, applied, _) = run_deps(
        &bundles,
        &[],
        Options {
            force: true,
            ..Default::default()
        },
    );
    assert_eq!(applied, ["do:1", "do:2"]);
}

#[test]
fn checks_are_rerun_before_each_bundle() {
    // b's check only passes once a has run.
    let bundles = [
        bundle("a", "steps:\n  - shell: do:x\n"),
        bundle("b", "check: done:do:x\nsteps:\n  - shell: do:never\n"),
    ];
    let (summary, applied, events) = run_deps(&bundles, &[vec![], vec![0]], Options::default());
    assert_eq!(applied, ["do:x"]);
    assert_eq!(bundle_outcomes(&events)[1].1, Outcome::Ok);
    assert_eq!(summary.skipped, 1);
}

#[test]
fn a_check_that_cannot_run_is_a_bundle_error() {
    let b = bundle("a", "check: boom\nsteps:\n  - shell: do:1\n");
    let planner = FakePlanner::default();
    let view = plan(std::slice::from_ref(&b), &planner, &ctx()).view();
    let StepViewState::Error(e) = &view.bundles[0].steps[0].state else {
        panic!()
    };
    assert!(e.contains("check: cannot run"), "{e}");
}

#[test]
fn bundle_vars_and_defaults_apply_to_their_own_steps() {
    let a = bundle(
        "a",
        "vars: { word: alpha }\ncheck: env:word\nsteps:\n  - shell: do:{{ word }}\n",
    );
    let b = bundle("b", "steps:\n  - shell: do:{{ word }}\n");
    let planner = FakePlanner::default();
    let mut c = ctx();
    c.vars.set("word", "fromcli");
    let view = plan_with(
        &[a.clone(), b.clone()],
        &planner,
        &c,
        Options {
            force: true,
            ..Default::default()
        },
    )
    .view();
    let desc = |i: usize| match &view.bundles[i].steps[0].state {
        StepViewState::Ready(a) => a[0].description.clone(),
        other => panic!("{other:?}"),
    };
    // The command line beats the bundle layer; without it, the bundle's own value is used and
    // only that bundle sees it.
    assert_eq!(desc(0), "do:fromcli");
    let view = plan(&[a, b], &planner, &ctx()).view();
    assert!(
        view.bundles[0].skip.is_some(),
        "bundle vars reach the check's env"
    );
    let StepViewState::Error(e) = &view.bundles[1].steps[0].state else {
        panic!()
    };
    assert!(e.contains("undefined variable `word`"), "{e}");
}

#[test]
fn secrets_render_as_placeholders_when_planning_and_values_when_applying() {
    let a = bundle("a", "steps:\n  - shell: do:{{ secret('tok') }}\n");
    let planner = FakePlanner::default();
    let p = plan(std::slice::from_ref(&a), &planner, &ctx());
    assert_eq!(p.secret_names(), ["tok"]);
    let StepViewState::Ready(actions) = &p.view().bundles[0].steps[0].state else {
        panic!()
    };
    assert_eq!(actions[0].description, "do:<secret:tok>");

    // No provider: applying fails with a clear error.
    let (summary, applied, events) = run_fake(std::slice::from_ref(&a), false);
    assert!(applied.is_empty());
    assert_eq!(summary.failed, 1);
    assert!(events.iter().any(|e| matches!(e,
        Event::StepFinished { detail: Some(d), .. } if d.contains("secret `tok` was not resolved"))));

    // Resolved by the prepare stage: applied with the value.
    let cache = Arc::new(crate::secret::SecretCache::default());
    cache.insert("tok", "v1");
    let mut c = ctx();
    c.secrets = Some(cache);
    c.profile = Some("mac".into());
    let rec = Recorder::default();
    let summary = run(
        std::slice::from_ref(&a),
        &planner,
        &c,
        &EventSink::new().with(Box::new(rec.clone())),
        Options::default(),
    );
    assert!(summary.success(), "{summary:?}");
    assert_eq!(*planner.applied.lock().unwrap(), ["do:v1"]);
    assert!(matches!(&rec.events()[0], Event::RunStarted { profile: Some(p), .. } if p == "mac"));
}

#[test]
fn secrets_are_collected_per_bundle_with_their_use() {
    use crate::secret::SecretUse;
    let a = bundle("a", "steps:\n  - shell: secret:tok\n");
    let b = bundle("b", "steps:\n  - shell: secretfile:vpn secret:tok\n");
    let p = plan(&[a, b], &FakePlanner::default(), &ctx());
    assert_eq!(p.bundles[0].secrets.keys().collect::<Vec<_>>(), ["tok"]);
    assert_eq!(p.bundles[1].secrets["vpn"], SecretUse::FILE);
    assert_eq!(p.secrets["tok"], SecretUse::TEXT);
    assert_eq!(p.secrets["vpn"], SecretUse::FILE);
}

/// Records each `before_bundle` call; fails for the bundles named in `fail`.
#[derive(Default)]
struct PerBundle {
    calls: Vec<(String, Vec<String>)>,
    fail: Vec<String>,
}

impl Prepare for PerBundle {
    fn prepare(&mut self, _plan: &Plan, _sink: &EventSink) -> Result<()> {
        Ok(())
    }

    fn before_bundle(
        &mut self,
        name: &str,
        secrets: &std::collections::BTreeMap<String, crate::secret::SecretUse>,
        _sink: &EventSink,
    ) -> Result<()> {
        self.calls
            .push((name.to_string(), secrets.keys().cloned().collect()));
        if self.fail.iter().any(|f| f == name) {
            bail!("provider `lastpass` is not available on this machine");
        }
        Ok(())
    }
}

#[test]
fn secrets_can_be_resolved_right_before_the_bundle_that_needs_them() {
    let install = bundle("install", "steps:\n  - shell: do:cli\n");
    let user = bundle(
        "user",
        "steps:\n  - shell: secret:tok do:user\n  - shell: do:more\n",
    );
    let dependent = bundle("dependent", "steps:\n  - shell: do:dep\n");
    let other = bundle("other", "steps:\n  - shell: do:other\n");
    let bundles = [install, user, dependent, other];
    let deps = vec![vec![], vec![0], vec![1], vec![]];
    let planner = FakePlanner::default();
    let mut prep = PerBundle::default();
    let summary = run_graph(
        &bundles,
        &deps,
        &planner,
        &ctx(),
        &EventSink::new(),
        Options::default(),
        &mut prep,
    );
    assert!(summary.success(), "{summary:?}");
    assert_eq!(
        prep.calls,
        [
            ("install".to_string(), vec![]),
            ("user".to_string(), vec!["tok".to_string()]),
            ("dependent".to_string(), vec![]),
            ("other".to_string(), vec![]),
        ]
    );

    // Still unavailable then: that bundle fails, its dependents are skipped, others run.
    let planner = FakePlanner::default();
    let rec = Recorder::default();
    let mut prep = PerBundle {
        fail: vec!["user".into()],
        ..PerBundle::default()
    };
    let summary = run_graph(
        &bundles,
        &deps,
        &planner,
        &ctx(),
        &EventSink::new().with(Box::new(rec.clone())),
        Options::default(),
        &mut prep,
    );
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert_eq!(*planner.applied.lock().unwrap(), ["do:cli", "do:other"]);
    assert!(rec.events().iter().any(|e| matches!(e,
        Event::StepFinished { outcome: Outcome::Failed, detail: Some(d), .. }
            if d.contains("lastpass` is not available"))));
    assert!(rec.events().iter().any(|e| matches!(e,
        Event::StepFinished { outcome: Outcome::Skipped, detail: Some(d), .. }
            if d.starts_with(DEPENDENCY_FAILED))));

    // Dry runs never prepare.
    let mut prep = PerBundle::default();
    run_graph(
        &bundles,
        &deps,
        &FakePlanner::default(),
        &ctx(),
        &EventSink::new(),
        Options {
            dry_run: true,
            ..Options::default()
        },
        &mut prep,
    );
    assert!(prep.calls.is_empty());
}

fn run_in(
    bundles: &[Bundle],
    planner: &FakePlanner,
    ctx: &Context,
    dry_run: bool,
) -> (Summary, Vec<Event>) {
    let rec = Recorder::default();
    let summary = run(
        bundles,
        planner,
        ctx,
        &EventSink::new().with(Box::new(rec.clone())),
        Options {
            dry_run,
            ..Default::default()
        },
    );
    (summary, rec.events())
}

fn step_details(events: &[Event]) -> Vec<(usize, Outcome, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::StepFinished {
                step,
                outcome,
                detail,
                ..
            } => Some((step.step, *outcome, detail.clone())),
            _ => None,
        })
        .collect()
}

const HANDLERS: &str = r#"
steps:
  - shell: do:conf
    notify: restart
  - shell: ok:other
    notify: [reload]
  - shell: do:third
handlers:
  restart:
    shell: do:restart
  reload:
    shell: do:reload
"#;

#[test]
fn handlers_run_once_at_the_end_only_when_notified() {
    let b = bundle("ssh", HANDLERS);
    assert_eq!(b.handlers.len(), 2);
    let tmp = tempfile::tempdir().unwrap();
    let mut c = ctx();
    c.state_dir = tmp.path().to_path_buf();
    // The plan: `restart` may be notified, `reload` cannot be (its step is satisfied).
    let plan = plan(std::slice::from_ref(&b), &FakePlanner::default(), &c);
    let steps = &plan.bundles[0].steps;
    assert_eq!(steps.len(), 5);
    assert_eq!(steps[3].label, "handler restart: shell: do:restart");
    assert!(matches!(steps[3].state, PlannedState::Ready(_)));
    assert!(matches!(&steps[4].state, PlannedState::Skipped(r) if r == NOT_NOTIFIED));

    // A dry run previews the notified handler.
    let planner = FakePlanner::default();
    let (summary, events) = run_in(std::slice::from_ref(&b), &planner, &c, true);
    assert_eq!((summary.changed, summary.ok, summary.skipped), (3, 1, 1));
    assert!(planner.applied.lock().unwrap().is_empty());
    assert_eq!(step_details(&events)[3].1, Outcome::Changed);

    // A real run: the handler after every step.
    let planner = FakePlanner::default();
    let (summary, events) = run_in(std::slice::from_ref(&b), &planner, &c, false);
    assert!(summary.success(), "{summary:?}");
    assert_eq!(
        *planner.applied.lock().unwrap(),
        ["do:conf", "do:third", "do:restart"]
    );
    let d = step_details(&events);
    assert_eq!(
        d[4],
        (4, Outcome::Skipped, Some("handler: not notified".into()))
    );
    assert!(PendingHandlers::in_dir(tmp.path()).get("ssh").is_empty());
}

#[test]
fn a_notification_survives_a_failed_bundle() {
    let b = bundle(
        "ssh",
        "steps:\n  - shell: do:conf\n    notify: restart\n  - shell: fail:later\nhandlers:\n  restart: { shell: do:restart }\n",
    );
    let tmp = tempfile::tempdir().unwrap();
    let mut c = ctx();
    c.state_dir = tmp.path().to_path_buf();
    let planner = FakePlanner::default();
    let (summary, events) = run_in(std::slice::from_ref(&b), &planner, &c, false);
    assert!(!summary.success());
    assert_eq!(*planner.applied.lock().unwrap(), ["do:conf"]);
    let d = step_details(&events);
    assert_eq!(d[2].1, Outcome::Skipped);
    assert!(d[2].2.as_deref().unwrap().contains("still due"), "{d:?}");
    let pending = PendingHandlers::in_dir(tmp.path());
    assert_eq!(pending.get("ssh"), ["restart".to_string()].into());

    // The next run: `conf` is satisfied now, but the handler is still due. Planned as such.
    let fixed = bundle(
        "ssh",
        "steps:\n  - shell: ok:conf\n    notify: restart\n  - shell: ok:later\nhandlers:\n  restart: { shell: do:restart }\n",
    );
    let p = plan(std::slice::from_ref(&fixed), &FakePlanner::default(), &c);
    assert!(matches!(
        p.bundles[0].steps[2].state,
        PlannedState::Ready(_)
    ));
    let planner = FakePlanner::default();
    let (summary, _) = run_in(std::slice::from_ref(&fixed), &planner, &c, false);
    assert!(summary.success(), "{summary:?}");
    assert_eq!(*planner.applied.lock().unwrap(), ["do:restart"]);
    assert!(pending.get("ssh").is_empty());

    // A failing handler fails the bundle and stays due.
    let failing = bundle(
        "ssh",
        "steps:\n  - shell: do:conf\n    notify: restart\nhandlers:\n  restart: { shell: fail:restart }\n",
    );
    let (summary, _) = run_in(
        std::slice::from_ref(&failing),
        &FakePlanner::default(),
        &c,
        false,
    );
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert_eq!(pending.get("ssh"), ["restart".to_string()].into());
}

#[test]
fn a_notification_survives_a_panic_later_in_the_bundle() {
    let b = bundle(
        "ssh",
        "steps:\n  - shell: do:conf\n    notify: restart\n  - shell: panic:later\nhandlers:\n  restart: { shell: do:restart }\n",
    );
    let tmp = tempfile::tempdir().unwrap();
    let mut c = ctx();
    c.state_dir = tmp.path().to_path_buf();
    let planner = FakePlanner::default();
    let (summary, _) = run_in(std::slice::from_ref(&b), &planner, &c, false);
    assert!(!summary.success());
    assert_eq!(*planner.applied.lock().unwrap(), ["do:conf"]);
    // The handlers' turn never came, but the notification was kept as the step finished.
    let pending = PendingHandlers::in_dir(tmp.path());
    assert_eq!(pending.get("ssh"), ["restart".to_string()].into());
    // A dry run records nothing.
    let other = tempfile::tempdir().unwrap();
    c.state_dir = other.path().to_path_buf();
    run_in(std::slice::from_ref(&b), &FakePlanner::default(), &c, true);
    assert!(PendingHandlers::in_dir(other.path()).get("ssh").is_empty());
}

#[test]
fn notify_must_name_a_handler_of_the_bundle() {
    let file = abs("/nonexistent-dot-test/x/bundle.yml");
    let err = |yaml: &str| parse_bundle(yaml, &file).unwrap_err().to_string();
    let e = err("steps:\n  - shell: a\n    notify: nope\n");
    assert!(
        e.contains("bundle.yml:2:5: steps[0]: notify: no handler `nope`"),
        "{e}"
    );
    assert!(e.contains("it has no `handlers:`"), "{e}");
    let e = err("steps:\n  - shell: a\n    notify: [h, nope]\nhandlers:\n  h: { shell: b }\n");
    assert!(e.contains("its handlers: h"), "{e}");
    let e = err("steps: []\nhandlers:\n  h: { shell: b, notify: x }\n");
    assert!(e.contains("cannot `notify`"), "{e}");
    let e = err("steps: []\nhandlers:\n  'a b': { shell: b }\n");
    assert!(e.contains("may only use letters"), "{e}");
    let e = err("steps: []\nhandlers:\n  h: { label: x }\n");
    assert!(e.contains("exactly one directive"), "{e}");
    // A handler's errors name it.
    let b = bundle("x", "steps: []\nhandlers:\n  h: { shell: b }\n");
    let e = ConfigError::at_step(&b, &b.handlers[0].step, "boom").to_string();
    assert!(e.ends_with("handlers.h: boom"), "{e}");
}

#[test]
fn a_handler_only_unknown_steps_notify_is_unknown_too() {
    let b = bundle(
        "ssh",
        concat!(
            "steps:\n",
            "  - shell: secret:tok\n",
            "    notify: restart\n",
            "handlers:\n",
            "  restart: { shell: do:restart }\n",
        ),
    );
    let tmp = tempfile::tempdir().unwrap();
    let mut c = ctx();
    c.state_dir = tmp.path().to_path_buf();
    let plan = plan(std::slice::from_ref(&b), &FakePlanner::default(), &c);
    let PlannedState::Ready(actions) = &plan.bundles[0].steps[1].state else {
        panic!("the handler is planned");
    };
    assert_eq!(actions[0].status, Status::Unknown);
    assert!(
        actions[0]
            .description
            .ends_with("unknown yet, changes something)"),
        "{}",
        actions[0].description
    );
}

#[test]
fn only_steps_with_work_need_their_secrets() {
    let b = bundle(
        "glab",
        concat!(
            "steps:\n",
            "  - shell: \"ok:{{ secret('satisfied') }}\"\n",
            "  - shell: \"do:{{ secret('pending') }}\"\n",
        ),
    );
    let plan = plan(std::slice::from_ref(&b), &FakePlanner::default(), &ctx());
    assert_eq!(plan.secret_names(), ["pending", "satisfied"]);
    assert_eq!(plan.needed_names(), ["pending"]);
    assert_eq!(plan.bundles[0].steps[0].secrets, ["satisfied"]);
    assert!(!plan.bundles[0].steps[0].has_work());
}

#[test]
fn if_only_if_and_unless_guard_steps() {
    let mut c = ctx();
    c.vars.set_fact("mono", serde_json::Value::Bool(false));
    c.vars.set_fact("client", "");
    let b = bundle(
        "g",
        concat!(
            "steps:\n",
            "  - shell: do:mono\n",
            "    if: mono\n",
            "  - shell: do:client\n",
            "    if: 'client != \"\"'\n",
            "  - shell: do:std\n",
            "    if: '{{ not mono }}'\n",
            "  - shell: do:closed\n",
            "    only_if: ide-running\n",
            "  - shell: do:open\n",
            "    only_if: ok\n",
            "  - shell: ok:already\n",
            "    only_if: boom\n",
        ),
    );
    let planner = FakePlanner::default();
    let plan = plan_with(std::slice::from_ref(&b), &planner, &c, Options::default());
    let states: Vec<String> = plan.bundles[0]
        .steps
        .iter()
        .map(|s| match &s.state {
            PlannedState::Skipped(r) => format!("skipped: {r}"),
            PlannedState::Error(e) => format!("error: {e}"),
            PlannedState::Ready(a) => format!("ready: {}", a[0].status.as_str()),
        })
        .collect();
    assert_eq!(
        states,
        [
            "skipped: if: `mono` is false",
            "skipped: if: `client != \"\"` is false",
            "ready: pending",
            "skipped: only_if: `ide-running` failed",
            "ready: pending",
            // Nothing to do: the guard is not asked (it would fail to run).
            "ready: satisfied",
        ]
    );
    let (summary, _) = run_in(std::slice::from_ref(&b), &planner, &c, false);
    assert!(summary.success(), "{summary:?}");
    assert_eq!(*planner.applied.lock().unwrap(), ["do:std", "do:open"]);

    // An undefined variable in `if:` is an error at the step.
    let b = bundle("g", "steps:\n  - shell: do:x\n    if: nope\n");
    let p2 = super::plan(std::slice::from_ref(&b), &FakePlanner::default(), &c);
    assert!(
        matches!(&p2.bundles[0].steps[0].state, PlannedState::Error(e)
            if e.contains("steps[0]: if:") && e.contains("undefined variable `nope`")),
        "{:?}",
        p2.bundles[0].steps[0].label
    );
}
