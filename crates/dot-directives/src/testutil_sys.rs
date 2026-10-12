//! Test helpers for the file and system directives: planning as the engine does right before
//! applying (secrets resolved), and planning with a scripted [`FakeExec`] and chosen facts.

use std::sync::Arc;

use dot_core::facts::{Facts, Os, Role};
use dot_core::{Action, BundleEnv, PlanCtx, Planner, SecretCache, SecretRefs};
use dot_platform::exec::System;
use dot_platform::exec::fake::FakeExec;

use crate::Builtins;
use crate::testutil::Fixture;

/// Plans step `step` as the engine does right before applying it, with `secrets` resolved.
pub fn plan_applying(f: &Fixture, step: usize, secrets: &[(&str, &str)]) -> Vec<Box<dyn Action>> {
    plan_applying_by(f, &Builtins::default(), step, secrets)
}

/// [`plan_applying`] with `builtins`.
pub fn plan_applying_by(
    f: &Fixture,
    builtins: &Builtins,
    step: usize,
    secrets: &[(&str, &str)],
) -> Vec<Box<dyn Action>> {
    let cache = Arc::new(SecretCache::default());
    for (k, v) in secrets {
        cache.insert(*k, *v);
    }
    let mut ctx = f.context();
    ctx.secrets = Some(cache);
    let env = BundleEnv::new(&ctx, &f.bundle);
    let refs = SecretRefs::default();
    let s = &f.bundle.steps[step];
    let pctx = PlanCtx::for_bundle(&ctx, &env, &f.bundle, s, &refs, true);
    builtins.plan(&s.directive, &pctx).unwrap()
}

/// Builtins whose system commands go to a fresh [`FakeExec`].
pub fn fake_builtins() -> (Builtins, Arc<FakeExec>) {
    let fake = FakeExec::new();
    let b = Builtins {
        system: System::fake(fake.clone()),
        ..Builtins::default()
    };
    (b, fake)
}

/// Plans step `step` with `builtins` on a host with operating system `os`.
pub fn plan_on(
    f: &Fixture,
    builtins: &Builtins,
    os: Os,
    step: usize,
) -> anyhow::Result<Vec<Box<dyn Action>>> {
    let mut ctx = f.context();
    ctx.facts = Facts::new(os, "aarch64", Role::Desktop);
    let env = BundleEnv::new(&ctx, &f.bundle);
    let refs = SecretRefs::default();
    let s = &f.bundle.steps[step];
    let pctx = PlanCtx::for_bundle(&ctx, &env, &f.bundle, s, &refs, false);
    builtins.plan(&s.directive, &pctx)
}
