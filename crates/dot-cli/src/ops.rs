//! `dot status` and `dot diff`: the selected bundles planned again (nothing changed, nothing
//! unlocked), and what the state store recorded about what earlier runs applied.
//!
//! - drift: every action that is not satisfied (pending, unknown, conflicting) or could not be
//!   planned. Actions that manage nothing (a `shell` step without `unless`) run on every apply
//!   and are listed apart, not as drift.
//! - orphans: resources recorded in the state store that the configuration no longer produces:
//!   their bundle was deleted or deselected, or the step changed. `dot remove <bundle>` undoes
//!   them.

use std::collections::BTreeSet;
use std::process::ExitCode;

use anyhow::Result;
use dot_core::engine::{Plan, PlannedState};
use dot_core::graph::Resolution;
use dot_core::workspace::Workspace;
use dot_core::{Diff, Options, Redactor, StateStore, Status, engine};

use crate::commands::{
    builtins, open, plural, resolution_report, resolve, secrets_config, use_state,
};
use crate::{DiffArgs, RepoArgs, StatusArgs};

/// The selected bundles, planned against this machine.
pub(crate) struct Planned {
    pub ws: Workspace,
    pub res: Resolution,
    pub plan: Plan,
    pub redactor: Redactor,
    pub store: StateStore,
    /// Secrets the plan uses that could not be resolved without asking anyone: what their
    /// steps would do is not known, which is not drift.
    pub unverified: BTreeSet<String>,
}

/// Opens the repository and plans `bundles` (default: the profile's selection) as `dot plan`
/// does. `Err` carries the exit code (problems already printed).
pub(crate) fn planned(
    repo: &RepoArgs,
    bundles: &[String],
    no_deps: bool,
) -> Result<std::result::Result<Planned, ExitCode>> {
    let (mut ws, inputs) = match open(repo)? {
        Ok(x) => x,
        Err(code) => return Ok(Err(code)),
    };
    let res = match resolve(&ws, bundles, no_deps) {
        Ok(r) => r,
        Err(code) => return Ok(Err(code)),
    };
    // The whole profile: bundles applied on purpose (`dot apply docker`, an opt-in bundle the
    // profile does not select) are part of this machine's configuration while they exist, so
    // they are checked too instead of their records reading as orphans.
    let res = if bundles.is_empty() {
        with_applied_bundles(&ws, res, &inputs.home)
    } else {
        res
    };
    // Never asked here: what uses them is unknown.
    ws.defer_prompts(&res);
    let redactor = Redactor::new();
    for v in ws.sensitive_values(&res) {
        redactor.add(v);
    }
    let config = match secrets_config(&ws) {
        Ok(c) => c,
        Err(code) => return Ok(Err(code)),
    };
    let mut ctx = ws.context(&inputs.home);
    use_state(&mut ctx);
    let b = builtins(&ctx.home, config.as_ref(), false);
    let opts = Options {
        dry_run: true,
        ..Default::default()
    };
    // Scheduled as `dot apply` runs it: `path:` / `env:` entries are seen along edges only.
    let bundles = ws.bundles(&res);
    let schedule = res.schedule();
    let mut plan = engine::plan_scheduled(&bundles, &schedule, &b, &ctx, opts);
    // Steps that use secrets are compared with the values that can be had without asking
    // (an envfile, an unlocked password manager), so a converged machine is in sync.
    let mut unverified = BTreeSet::new();
    if !plan.secrets.is_empty() {
        let (lookup, missing) = resolve_quietly(config, &inputs, &plan, &redactor);
        unverified = missing;
        if lookup.is_some() {
            ctx.plan_secrets = lookup;
            plan = engine::plan_scheduled(&bundles, &schedule, &b, &ctx, opts);
        }
    }
    let store = StateStore::at(&ctx.state_dir);
    Ok(Ok(Planned {
        ws,
        res,
        plan,
        redactor,
        store,
        unverified,
    }))
}

/// The plan's secrets resolved without a prompt, each value registered with `redactor`, and
/// the names that could not be. `None`: nothing resolved.
fn resolve_quietly(
    config: Option<dot_secrets::SecretsConfig>,
    inputs: &dot_core::workspace::Inputs,
    plan: &Plan,
    redactor: &Redactor,
) -> (
    Option<std::sync::Arc<dyn dot_core::SecretLookup>>,
    BTreeSet<String>,
) {
    let names = plan.secret_names();
    let Some(config) = config else {
        return (None, names.into_iter().collect());
    };
    let mut resolver = crate::secrets::resolver(config, &crate::secrets::provider_env(inputs));
    let r = redactor.clone();
    resolver.on_resolved(move |v| {
        r.add(String::from_utf8_lossy(v.expose()).into_owned());
    });
    let partial = resolver.resolve_partial(&names, &mut dot_secrets::NonInteractive);
    let cache = dot_core::SecretCache::default();
    let mut missing: BTreeSet<String> = names.iter().cloned().collect();
    for (name, value) in partial.resolved.iter() {
        if plan.secrets.get(name).is_some_and(|u| u.text) && value.expose_str().is_none() {
            continue;
        }
        cache.insert(name, value.expose().to_vec());
        missing.remove(name);
    }
    let lookup = (!cache.is_empty())
        .then(|| std::sync::Arc::new(cache) as std::sync::Arc<dyn dot_core::SecretLookup>);
    (lookup, missing)
}

/// Why [`with_applied_bundles`] adds a bundle.
pub(crate) const APPLIED: &str = "`dot apply <bundle>` (opted in on this machine)";

/// `res` (the profile's selection) plus every bundle that was applied by name although the
/// profile does not select it (`dot apply docker`, an opt-in bundle; see
/// [`StateStore::opted_in`]) and still exists, with its dependencies. Such a bundle is part of
/// this machine's configuration until `dot remove`, so it is checked instead of its records
/// reading as orphans. Unchanged when there are none, or when adding them does not resolve.
fn with_applied_bundles(ws: &Workspace, res: Resolution, home: &std::path::Path) -> Resolution {
    let Some(store) = crate::commands::state_store(home) else {
        return res;
    };
    let extra: Vec<String> = store
        .opted_in()
        .into_iter()
        .filter(|b| ws.repo.bundles.contains_key(b) && res.get(b).is_none())
        .collect();
    if extra.is_empty() {
        return res;
    }
    let mut sel = ws.selection(&[], false);
    sel.roots.extend(
        extra
            .into_iter()
            .map(|b| (b, dot_core::graph::Reason::Selected(APPLIED.into()))),
    );
    ws.graph().resolve(&sel).unwrap_or(res)
}

/// One action (or step) that is not as the config wants.
pub(crate) struct Drift {
    pub bundle: String,
    pub label: String,
    pub description: String,
    /// `None`: the step could not be planned (`description` is the error).
    pub status: Option<Status>,
    pub diff: Option<Diff>,
}

/// What [`find_drift`] found.
pub(crate) struct Found {
    pub drift: Vec<Drift>,
    /// `bundle: label (secrets)` of steps whose status is unknown only because their secrets
    /// cannot be had without a prompt.
    pub unverified: Vec<String>,
    /// `bundle: label` of steps that run on every apply.
    pub always: Vec<String>,
    /// Every resource key the plan produces.
    pub keys: BTreeSet<String>,
}

pub(crate) fn find_drift(p: &Planned, with_diffs: bool) -> Found {
    let mut found = Found {
        drift: Vec::new(),
        unverified: Vec::new(),
        always: Vec::new(),
        keys: BTreeSet::new(),
    };
    for b in &p.plan.bundles {
        // Handlers come after the bundle's steps. One planned to run is due (a step notifying
        // it has work, or an earlier run left it pending): drift, even when it manages nothing.
        let first_handler =
            p.ws.repo
                .bundles
                .get(&b.name)
                .map_or(usize::MAX, |x| x.steps.len());
        for s in &b.steps {
            let handler = s.id.step >= first_handler;
            match &s.state {
                PlannedState::Skipped(_) => {}
                PlannedState::Error(e) => found.drift.push(Drift {
                    bundle: b.name.clone(),
                    label: s.label.clone(),
                    description: e.clone(),
                    status: None,
                    diff: None,
                }),
                PlannedState::Ready(actions) => {
                    for a in actions {
                        let resources = a.action.resources();
                        found.keys.extend(resources.iter().map(|r| r.key()));
                        if a.status == Status::Satisfied {
                            continue;
                        }
                        let missing: Vec<&str> = s
                            .secrets
                            .iter()
                            .filter(|n| p.unverified.contains(*n))
                            .map(String::as_str)
                            .collect();
                        if a.status == Status::Unknown && !missing.is_empty() {
                            let entry =
                                format!("{}: {} (needs {})", b.name, s.label, missing.join(", "));
                            if found.unverified.last() != Some(&entry) {
                                found.unverified.push(entry);
                            }
                            continue;
                        }
                        if resources.is_empty() && a.status == Status::Pending && !handler {
                            let entry = format!("{}: {}", b.name, s.label);
                            if found.always.last() != Some(&entry) {
                                found.always.push(entry);
                            }
                            continue;
                        }
                        let diff = if with_diffs {
                            match a.action.diff() {
                                Ok(d) => d,
                                Err(e) => Some(Diff::Note(format!("cannot compare: {e:#}"))),
                            }
                        } else {
                            None
                        };
                        // The step label of a handler is `handler <name>: ...`.
                        let description = match s.label.strip_prefix("handler ") {
                            Some(rest) if handler => format!(
                                "handler {}: {}",
                                rest.split(": ").next().unwrap_or(rest),
                                a.description
                            ),
                            _ => a.description.clone(),
                        };
                        found.drift.push(Drift {
                            bundle: b.name.clone(),
                            label: s.label.clone(),
                            description,
                            status: Some(a.status),
                            diff,
                        });
                    }
                }
            }
        }
    }
    found
}

/// A recorded resource the configuration no longer produces, and why.
pub(crate) struct Orphan {
    pub key: String,
    pub why: String,
}

pub(crate) fn find_orphans(p: &Planned, keys: &BTreeSet<String>) -> Vec<Orphan> {
    let state = p.store.read();
    let selected: BTreeSet<&str> = p.plan.bundles.iter().map(|b| b.name.as_str()).collect();
    let profile =
        p.ws.profile_name()
            .map(|n| format!(" by profile {n}"))
            .unwrap_or_default();
    let mut out = Vec::new();
    for (key, record) in state.resources {
        if keys.contains(&key) {
            continue;
        }
        let b = &record.bundle;
        let why = if !p.ws.repo.bundles.contains_key(b) {
            format!("bundle {b} no longer exists")
        } else if let Some(e) = p.res.excluded.iter().find(|e| &e.name == b) {
            format!("bundle {b} does not apply here ({})", e.detail)
        } else if !selected.contains(b.as_str()) {
            format!("bundle {b} is not selected{profile}")
        } else {
            let planned = p.plan.bundles.iter().find(|x| &x.name == b);
            match planned {
                Some(pb)
                    if pb
                        .steps
                        .iter()
                        .any(|s| matches!(s.state, PlannedState::Error(_))) =>
                {
                    // Cannot tell: the bundle did not plan.
                    continue;
                }
                Some(pb) => match &pb.skip {
                    Some(reason) if reason.starts_with("when:") => {
                        format!("bundle {b} does not apply here ({reason})")
                    }
                    // `check:` says the bundle is satisfied: its steps were not planned.
                    Some(_) => continue,
                    None => format!("no longer in bundle {b}"),
                },
                None => continue,
            }
        };
        out.push(Orphan { key, why });
    }
    out
}

fn status_word(s: Option<Status>) -> &'static str {
    s.map_or("error", Status::as_str)
}

pub fn status(a: StatusArgs) -> Result<ExitCode> {
    let p = match planned(&a.repo, &a.bundles, a.no_deps)? {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let found = find_drift(&p, false);
    // What earlier runs recorded cannot be read: orphans are unknown, not none.
    let unreadable = p.store.load().err();
    let orphans = if a.bundles.is_empty() && unreadable.is_none() {
        find_orphans(&p, &found.keys)
    } else {
        // Only some bundles were planned: nothing can be said about the others' records.
        Vec::new()
    };
    let r = &p.redactor;
    let mut out = String::new();
    if a.verbose {
        out.push_str(&resolution_report(&p.ws, &p.res));
    }
    if !found.drift.is_empty() {
        out.push_str("drift:\n");
        for d in &found.drift {
            let what = match d.status {
                None => format!("{}: {}", d.label, d.description),
                Some(_) => d.description.clone(),
            };
            out.push_str(&format!(
                "  {}: {what} [{}]\n",
                d.bundle,
                status_word(d.status)
            ));
        }
    }
    if !orphans.is_empty() {
        out.push_str(
            "orphans (recorded, no longer in the configuration; `dot remove <bundle>`):\n",
        );
        for o in &orphans {
            out.push_str(&format!("  {} ({})\n", o.key, o.why));
        }
    }
    if !found.always.is_empty() {
        out.push_str(&format!(
            "run on every apply: {}\n",
            found.always.join(", ")
        ));
    }
    if !found.unverified.is_empty() {
        out.push_str(&format!(
            "not checked (secrets not available without a prompt): {}\n",
            found.unverified.join(", ")
        ));
    }
    let (n, m) = (found.drift.len(), orphans.len());
    if let Some(e) = &unreadable {
        out.push_str(&format!(
            "status: {n} drifted, orphans unknown (the state store cannot be read: {e}; see `dot \
             doctor`)\n"
        ));
    } else if n == 0 && m == 0 && found.unverified.is_empty() {
        out.push_str("status: in sync\n");
    } else if n == 0 && m == 0 {
        let u = found.unverified.len();
        out.push_str(&format!(
            "status: in sync ({u} step{} not checked)\n",
            plural(u)
        ));
    } else {
        out.push_str(&format!("status: {n} drifted, {m} orphan{}\n", plural(m)));
    }
    print!("{}", r.redact(&out));
    Ok(if n == 0 && m == 0 && unreadable.is_none() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub fn diff(a: DiffArgs) -> Result<ExitCode> {
    let p = match planned(&a.repo, &a.bundles, a.no_deps)? {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let found = find_drift(&p, true);
    let mut out = String::new();
    for d in &found.drift {
        let what = match d.status {
            None => format!("{}: {}", d.label, d.description),
            Some(_) => d.description.clone(),
        };
        out.push_str(&format!(
            "# {}: {what} [{}]\n",
            d.bundle,
            status_word(d.status)
        ));
        if let Some(diff) = &d.diff {
            out.push_str(&diff.to_string());
        }
        out.push('\n');
    }
    if !found.always.is_empty() {
        out.push_str(&format!(
            "# run on every apply: {}\n",
            found.always.join(", ")
        ));
    }
    if found.drift.is_empty() {
        out.push_str("no differences\n");
    }
    print!("{}", p.redactor.redact(&out));
    Ok(if found.drift.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
