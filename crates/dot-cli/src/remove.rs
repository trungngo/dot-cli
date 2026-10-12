//! `dot remove <bundle>`: undo what the state store recorded for a bundle (see
//! [`dot_directives::undo`]). Refused while other selected bundles require it, unless
//! `--cascade` removes them too (dependents first).

use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use dot_core::graph::Resolution;
use dot_core::{EventSink, StateStore, Stream};
use dot_directives::undo::{Removal, Remover, Verdict};
use dot_platform::exec::{Runner, System, SystemRunner};
use dot_platform::link::SystemLinks;
use dot_platform::pkg::Packages;

use crate::RemoveArgs;
use crate::commands::{config_exit, open, plural, resolve};
use crate::render::logfile::default_state_dir;

/// The selected bundles that require `name`, directly or through others, in the order to
/// remove them (dependents first).
fn dependents(res: &Resolution, name: &str) -> Vec<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut todo = vec![name.to_string()];
    while let Some(n) = todo.pop() {
        for e in res.edges.iter().filter(|e| e.to == n && e.kind.is_hard()) {
            if found.insert(e.from.clone()) {
                todo.push(e.from.clone());
            }
        }
    }
    res.order
        .iter()
        .rev()
        .filter(|r| found.contains(&r.name))
        .map(|r| r.name.clone())
        .collect()
}

fn line(v: &Verdict) -> String {
    match v {
        Verdict::Undo(what) => what.clone(),
        Verdict::Gone(what) => format!("nothing to do: {what}"),
        Verdict::Keep { why, .. } => format!("keep: {why}"),
        Verdict::Manual(note) => format!("by hand: {note}"),
        Verdict::Conflict(why) => format!("conflict: {why}"),
    }
}

pub fn remove(a: RemoveArgs) -> Result<ExitCode> {
    let (ws, inputs) = match open(&a.repo)? {
        Ok(x) => x,
        Err(code) => return Ok(code),
    };
    let res = match resolve(&ws, &[], false) {
        Ok(r) => r,
        Err(code) => return Ok(code),
    };
    let Some(state_dir) = default_state_dir(&inputs.home) else {
        eprintln!("error: cannot tell where dot's state directory is");
        return Ok(config_exit());
    };
    let store = StateStore::at(&state_dir);

    let deps = dependents(&res, &a.bundle);
    if !deps.is_empty() && !a.cascade {
        let who = ws
            .profile_name()
            .map(|p| format!(" (selected by profile {p})"))
            .unwrap_or_default();
        eprintln!(
            "error: {} {} {}{who}: remove {} too with --cascade, or deselect {}",
            deps.join(", "),
            if deps.len() == 1 {
                "requires"
            } else {
                "require"
            },
            a.bundle,
            if deps.len() == 1 { "it" } else { "them" },
            if deps.len() == 1 { "it" } else { "them" },
        );
        return Ok(ExitCode::FAILURE);
    }
    let mut order = deps;
    order.push(a.bundle.clone());

    let remover = Remover {
        links: Arc::new(SystemLinks::new()),
        packages: Packages::system(Some(state_dir.clone())),
        system: System::default(),
        store: store.clone(),
        home: inputs.home.clone(),
        uninstall: a.packages,
    };
    let mut planned: Vec<(String, Vec<(Removal, Verdict)>)> = Vec::new();
    let mut problems = 0usize;
    for b in &order {
        let mut items = Vec::new();
        for removal in remover.plan(b) {
            match removal.check() {
                Ok(v) => items.push((removal, v)),
                Err(e) => {
                    eprintln!("error: {b}: {}: {e:#}", removal.key);
                    problems += 1;
                }
            }
        }
        planned.push((b.clone(), items));
    }
    if planned.iter().all(|(_, items)| items.is_empty()) && problems == 0 {
        if !ws.repo.bundles.contains_key(&a.bundle) {
            eprintln!(
                "error: unknown bundle `{}`, and nothing is recorded for it",
                a.bundle
            );
            return Ok(config_exit());
        }
        println!(
            "nothing recorded for {}: dot has not applied it on this machine (or it was removed)",
            order.join(", ")
        );
        return Ok(ExitCode::SUCCESS);
    }

    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    // The list is shown up front for a dry run and for the question; otherwise each line is
    // printed once, as it is done.
    let confirm = interactive && !a.yes && !a.dry_run;
    let preview = a.dry_run || confirm;
    let verb = if a.dry_run { "would remove" } else { "remove" };
    for (b, items) in planned.iter().filter(|_| preview) {
        if items.is_empty() {
            continue;
        }
        println!("{verb} bundle {b}:");
        for (_, v) in items {
            println!("  {}", line(v));
        }
    }
    let conflicts = planned
        .iter()
        .flat_map(|(_, i)| i)
        .filter(|(_, v)| matches!(v, Verdict::Conflict(_)))
        .count();
    let still_selected: Vec<&String> = order.iter().filter(|b| res.get(b).is_some()).collect();
    let selected_note = if still_selected.is_empty() {
        None
    } else {
        Some(format!(
            "note: {} still selected{}: the next `dot apply` puts {} back",
            still_selected
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            ws.profile_name()
                .map(|p| format!(" by profile {p}"))
                .unwrap_or_default(),
            if still_selected.len() == 1 {
                "it"
            } else {
                "them"
            }
        ))
    };
    if a.dry_run {
        if let Some(n) = &selected_note {
            println!("{n}");
        }
        return Ok(if conflicts + problems == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }

    if confirm {
        print!("Remove? [y/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer).ok();
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("nothing removed");
            return Ok(ExitCode::FAILURE);
        }
    }
    let _lock = match store.run_lock(&format!("remove {}", a.bundle)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: {e:#}");
            return Ok(ExitCode::FAILURE);
        }
    };
    let root_needed: Vec<String> = planned
        .iter()
        .flat_map(|(_, i)| i)
        .filter(|(r, v)| matches!(v, Verdict::Undo(_)) && r.needs_sudo())
        .map(|(r, _)| r.key.clone())
        .collect();
    let runner = SystemRunner;
    if !root_needed.is_empty() && !runner.is_root() && !cfg!(windows) {
        let who = root_needed.join(", ");
        if let Err(e) =
            dot_platform::sudo::ensure_sudo(&runner, interactive, &who, &EventSink::new())
        {
            eprintln!("error: {e:#}");
            return Ok(ExitCode::FAILURE);
        }
    }

    let mut failed = 0usize;
    for (b, items) in &planned {
        let mut forget_env = false;
        for (removal, verdict) in items {
            let ok = match verdict {
                Verdict::Undo(what) => {
                    let mut out = |s: Stream, l: &str| match s {
                        Stream::Stdout => println!("    {l}"),
                        Stream::Stderr => eprintln!("    {l}"),
                    };
                    match removal.apply(&mut out) {
                        Ok(()) => {
                            println!("{b}: {what}");
                            true
                        }
                        Err(e) => {
                            eprintln!("error: {b}: {what}: {e:#}");
                            failed += 1;
                            false
                        }
                    }
                }
                other => {
                    if !preview {
                        println!("{b}: {}", line(other));
                    }
                    true
                }
            };
            if ok && verdict.forgets() {
                forget_env |= matches!(removal.record.undo, Some(dot_core::Undo::Forget));
                let key = removal.key.clone();
                if let Err(e) = store.update(|s| s.resources.remove(&key)) {
                    eprintln!("warning: cannot update the state store: {e:#}");
                }
            }
        }
        if forget_env {
            let name = b.clone();
            let dropped = store.update(|s| {
                s.env.path.retain(|e| e.bundle != name);
                s.env.vars.retain(|v| v.bundle != name);
            });
            if let Err(e) = dropped {
                eprintln!("warning: cannot update the state store: {e:#}");
            }
        }
        // No longer opted in on this machine (see `StateStore::opted_in`).
        if let Err(e) = store.set_opted_in(std::slice::from_ref(b), false) {
            eprintln!("warning: cannot update the state store: {e:#}");
        }
        // Handlers left due by an earlier run have nothing left to act on.
        let pending = dot_core::handlers::PendingHandlers {
            store: store.clone(),
        };
        if let Err(e) = pending.set(b, &Default::default()) {
            eprintln!("warning: cannot update the state store: {e:#}");
        }
    }
    let left = conflicts + failed + problems;
    if let Some(n) = &selected_note {
        println!("{n}");
    }
    if left == 0 {
        println!("removed {}", order.join(", "));
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "{left} resource{} left in place (see above); their records are kept",
            plural(left)
        );
        Ok(ExitCode::FAILURE)
    }
}
