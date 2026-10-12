//! `package`: install packages with the machine's package managers.
//!
//! - `package: [fd, ripgrep]` installs with this machine's manager (the `pkg` fact, or the one
//!   the OS and distro family call for when it is not installed yet), each name mapped through
//!   `dot.yml`'s `packages:` (`fd: {apt: fd-find}`); a name without a mapping is used as is.
//! - `package: {brew: [...], cask: [...], apt: [...], snap: [...], ...}` names packages per
//!   manager. Only the managers that belong to this machine are used: `brew` and `cask` on macOS
//!   (`brew` also where it is the `pkg` manager), the distro's own manager on Linux, `snap` on
//!   Linux (installed with the distro's manager, names mapped the same way, when snapd is not
//!   there), `winget` and `scoop` on Windows. The others are ignored, so one step can cover
//!   several platforms.
//!
//! Each manager is one action. Its status asks the manager which packages are installed (one
//! query), so the plan names exactly the missing ones; a manager that is not installed yet
//! (Homebrew, installed by an earlier bundle) makes the status unknown. Applying refreshes the
//! manager's metadata where that is needed and installs the missing packages in one batch,
//! holding the global package-manager lock. Casks that install an app get `--adopt`, found out
//! from Homebrew's API; when that cannot be reached the cask is installed without it, with a
//! warning.
//!
//! A cask that runs a `.pkg` installer (by the same metadata) asks for the sudo password itself:
//! Homebrew discards cached sudo credentials first, so the run's up-front `sudo -v` does not
//! cover it ([`Action::asks_for_sudo`]). An interactive run installs (or upgrades) such casks on
//! the terminal, the live view paused around them; a non-interactive one only with passwordless
//! sudo, checked before anything is applied (see `dot_platform::sudo`).
//!
//! With `upgrade: true` on the step (or `dot apply --upgrade`), the status also asks the manager
//! which installed packages are outdated, and applying upgrades them ("upgrade fd 8.7.0 ->
//! 8.7.1"). Where the manager's metadata is stale (apt's lists older than a day), that is only
//! known after refreshing: the status is unknown until then, and applying refreshes first.

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use dot_core::config::{PackageItem, PackageSpec, PackageStep, PkgManager};
use dot_core::facts::{Facts, Os};
use dot_core::{
    Action, Diff, Outcome, PackageMap, PlanCtx, Resource, RunCtx, Status, Stream, Undo,
};
use dot_platform::pkg::{self, CaskInfo, InstallOpts, Outdated, PackageManager, Packages};

/// Plans a `package:` step. `upgrade_all`: `dot apply --upgrade`, as if every step said
/// `upgrade: true`.
pub fn plan(
    step: &PackageStep,
    ctx: &PlanCtx<'_>,
    packages: &Packages,
    upgrade_all: bool,
) -> Result<Vec<Box<dyn Action>>> {
    let spec = &step.spec;
    let upgrade = step.upgrade || upgrade_all;
    let native = native_manager(ctx.facts);
    let render = |name: &str| -> Result<String> {
        let n = ctx.render(name)?;
        if n.trim().is_empty() || n.chars().any(char::is_whitespace) {
            return Err(ctx.config_error(format!(
                "package: `{name}` renders to `{n}`, which is not a package name"
            )));
        }
        Ok(n)
    };
    let mut groups: Vec<(PkgManager, Vec<PackageItem>, Option<String>)> = Vec::new();
    let mut snap_only: Vec<PackageItem> = Vec::new();
    match spec {
        PackageSpec::Names(names) => {
            let Some(m) = native else {
                return Err(ctx.config_error(
                    "package: no package manager is known for this machine (the `pkg` fact is \
                     unknown); name the manager instead, e.g. `package: { apt: [...] }`",
                ));
            };
            let items = names
                .iter()
                .map(|n| Ok(PackageItem::new(mapped(ctx.packages, &render(n)?, m))))
                .collect::<Result<Vec<_>>>()?;
            groups.push((m, items, None));
        }
        PackageSpec::ByManager(entries) => {
            for (m, items) in entries {
                let items = items
                    .iter()
                    .map(|i| {
                        Ok(PackageItem {
                            name: render(&i.name)?,
                            ..i.clone()
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                match applies(*m, ctx.facts, native) {
                    Use::No => {}
                    Use::Yes => groups.push((*m, items, None)),
                    Use::Snap if packages.snapd_available() => groups.push((*m, items, None)),
                    Use::Snap => {
                        // Decided again when the step runs (it is planned again then): snapd
                        // may be installed by an earlier step of the run.
                        let (fallback, only): (Vec<_>, Vec<_>) =
                            items.into_iter().partition(|i| i.fallback);
                        snap_only.extend(only);
                        if fallback.is_empty() {
                            continue;
                        }
                        let Some(n) = native.filter(|n| *n != PkgManager::Snap) else {
                            return Err(ctx.config_error(
                                "package: snapd is not available and no other package manager \
                                 is known for this machine",
                            ));
                        };
                        let items = fallback
                            .iter()
                            .map(|i| PackageItem::new(mapped(ctx.packages, &i.name, n)))
                            .collect();
                        let note = format!("snapd is not available: installing with {n}");
                        groups.push((n, items, Some(note)));
                    }
                }
            }
        }
    }
    let action = |m: PkgManager, items: Vec<PackageItem>, note: Option<String>| PackageAction {
        manager: packages.manager(m),
        kind: m,
        items,
        note,
        upgrade,
        run_env: ctx.run_env().clone(),
        packages: packages.clone(),
        found: Mutex::new(None),
        installed_now: Mutex::new(None),
        resolved: Mutex::new(None),
    };
    let mut out: Vec<Box<dyn Action>> = groups
        .into_iter()
        .map(|(m, items, note)| Box::new(action(m, items, note)) as Box<dyn Action>)
        .collect();
    if !snap_only.is_empty() {
        out.push(Box::new(SnapOnlyAction {
            inner: action(PkgManager::Snap, snap_only, None),
        }));
    }
    Ok(out)
}

/// Snaps with `fallback: false` while snapd is not available: nothing else installs them, so
/// the step waits for snapd (unknown: an earlier step of the run may install it) and fails
/// when it is still not there once the step runs.
pub struct SnapOnlyAction {
    inner: PackageAction,
}

impl SnapOnlyAction {
    fn names(&self) -> String {
        join(&self.inner.items)
    }
}

impl Action for SnapOnlyAction {
    fn describe(&self) -> String {
        if self.inner.packages.snapd_available() {
            return self.inner.describe();
        }
        format!(
            "snap: install {} [snap only (`fallback: false`): snapd is not available (yet)]",
            self.names()
        )
    }

    fn status(&self) -> Result<Status> {
        if self.inner.packages.snapd_available() {
            return self.inner.status();
        }
        Ok(Status::Unknown)
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        if !self.inner.packages.snapd_available() {
            bail!(
                "{} can only be installed as a snap (`fallback: false`), and snapd is not \
                 available here (install and start snapd first; containers and WSL without \
                 systemd have none)",
                self.names()
            );
        }
        self.inner.apply(ctx)
    }

    fn needs_sudo(&self) -> bool {
        self.inner.needs_sudo()
    }

    fn resources(&self) -> Vec<Resource> {
        self.inner.resources()
    }

    fn diff(&self) -> Result<Option<Diff>> {
        self.inner.diff()
    }
}

/// This machine's own package manager: the `pkg` fact, else (not installed yet) the one the OS
/// and distro family call for.
pub fn native_manager(facts: &Facts) -> Option<PkgManager> {
    if let Some(m) = facts.pkg.as_deref().and_then(PkgManager::from_fact) {
        return Some(m);
    }
    match facts.os {
        Os::Macos => Some(PkgManager::Brew),
        Os::Windows => Some(PkgManager::Winget),
        Os::Linux => match facts.family.as_deref()? {
            "debian" => Some(PkgManager::Apt),
            "rhel" => Some(PkgManager::Dnf),
            "arch" => Some(PkgManager::Pacman),
            "suse" => Some(PkgManager::Zypper),
            "alpine" => Some(PkgManager::Apk),
            _ => None,
        },
    }
}

enum Use {
    Yes,
    No,
    /// snap: when snapd is there, else the native manager.
    Snap,
}

fn applies(m: PkgManager, facts: &Facts, native: Option<PkgManager>) -> Use {
    use PkgManager::*;
    let yes = |b: bool| if b { Use::Yes } else { Use::No };
    match (facts.os, m) {
        // Extensions of an editor that runs everywhere.
        (_, Vscode) => Use::Yes,
        (Os::Macos, Brew | Cask) => Use::Yes,
        (Os::Macos, _) => Use::No,
        (Os::Windows, Winget | Scoop) => Use::Yes,
        (Os::Windows, _) => Use::No,
        (Os::Linux, Snap) => Use::Snap,
        (Os::Linux, Cask) => Use::No,
        (Os::Linux, m) => yes(native == Some(m)),
    }
}

/// `name` as manager `m` calls it, by `dot.yml`'s `packages:`.
fn mapped(map: &PackageMap, name: &str, m: PkgManager) -> String {
    map.get(name)
        .and_then(|per| per.get(m.as_str()))
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

/// What the last status check found.
struct Found {
    /// The packages, with a pattern (`dotnet-sdk-*`) replaced by the newest name it matches.
    items: Vec<PackageItem>,
    /// Patterns nothing matches (yet: the package lists may be refreshed first).
    unresolved: Vec<String>,
    missing: Vec<PackageItem>,
    installed: Vec<String>,
    /// Cask metadata for each missing cask.
    casks: Vec<(String, Result<CaskInfo, String>)>,
    /// The missing or outdated casks whose installer asks for the sudo password itself.
    asking: Vec<String>,
    /// Upgrading: the installed packages that are outdated.
    outdated: Vec<Outdated>,
    /// Upgrading: the manager's metadata is stale, so what is outdated is only known after a
    /// refresh.
    stale: bool,
}

impl Found {
    fn has_work(&self) -> bool {
        !self.missing.is_empty()
            || !self.outdated.is_empty()
            || self.stale
            || !self.unresolved.is_empty()
    }
}

pub struct PackageAction {
    manager: Arc<dyn PackageManager>,
    kind: PkgManager,
    items: Vec<PackageItem>,
    /// Why another manager than the configured one is used (snap fallback).
    note: Option<String>,
    /// Also upgrade installed packages that are outdated.
    upgrade: bool,
    /// The run's environment: a Windows installer's new `PATH` entries go there.
    run_env: dot_core::runenv::RunEnvCell,
    packages: Packages,
    found: Mutex<Option<Found>>,
    /// Once applied: the packages this run installed (the others were there already).
    installed_now: Mutex<Option<Vec<String>>>,
    /// The packages as last resolved (patterns replaced by names), for [`Action::resources`].
    resolved: Mutex<Option<Vec<PackageItem>>>,
}

fn label(m: PkgManager) -> &'static str {
    match m {
        PkgManager::Cask => "brew cask",
        PkgManager::Vscode => "vscode extensions",
        other => other.as_str(),
    }
}

/// `fd 8.7.0 -> 8.7.1, jq 1.6 -> 1.7`.
fn upgrades(outdated: &[Outdated]) -> String {
    outdated
        .iter()
        .map(|o| format!("{} {} -> {}", o.name, o.current, o.latest))
        .collect::<Vec<_>>()
        .join(", ")
}

fn join(items: &[PackageItem]) -> String {
    items
        .iter()
        .map(|i| i.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

impl PackageAction {
    fn found(&self) -> std::sync::MutexGuard<'_, Option<Found>> {
        self.found.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The packages with each pattern (`dotnet-sdk-*`) replaced by the newest name the
    /// manager knows that matches it; and the patterns nothing matches.
    fn resolve(&self) -> Result<(Vec<PackageItem>, Vec<String>)> {
        let mut items = Vec::new();
        let mut unresolved = Vec::new();
        for i in &self.items {
            if !pkg::is_name_pattern(&i.name) {
                items.push(i.clone());
                continue;
            }
            let Some(names) = self.manager.names_matching(&i.name)? else {
                bail!(
                    "{}: `{}`: {} cannot look packages up by a pattern; name the package",
                    label(self.kind),
                    i.name,
                    self.kind.as_str()
                );
            };
            match pkg::newest(&i.name, &names) {
                Some(n) => items.push(PackageItem {
                    name: n.clone(),
                    ..i.clone()
                }),
                None => unresolved.push(i.name.clone()),
            }
        }
        *self.resolved.lock().unwrap_or_else(|e| e.into_inner()) = Some(items.clone());
        Ok((items, unresolved))
    }

    /// Asks the manager which packages are installed (and, for missing casks, what they
    /// install).
    fn check(&self) -> Result<Found> {
        let (items, unresolved) = self.resolve()?;
        let names: Vec<String> = items.iter().map(|i| i.name.clone()).collect();
        let installed = self.manager.installed(&names)?;
        let (have, missing): (Vec<_>, Vec<_>) =
            items.iter().zip(installed).partition(|(_, ok)| *ok);
        let missing: Vec<PackageItem> = missing.into_iter().map(|(i, _)| i.clone()).collect();
        let installed: Vec<String> = have.into_iter().map(|(i, _)| i.name.clone()).collect();
        let stale = self.upgrade && !installed.is_empty() && self.manager.stale();
        let outdated = if self.upgrade && !installed.is_empty() && !stale {
            self.manager.outdated(&installed)?
        } else {
            Vec::new()
        };
        let casks: Vec<(String, Result<CaskInfo, String>)> = if self.kind == PkgManager::Cask {
            missing
                .iter()
                .map(|i| (i.name.clone(), self.packages.cask_info(&i.name)))
                .collect()
        } else {
            Vec::new()
        };
        let asks = |name: &str| self.packages.cask_info(name).is_ok_and(|i| i.pkg);
        let asking = if self.kind == PkgManager::Cask {
            casks
                .iter()
                .map(|(n, _)| n)
                .chain(outdated.iter().map(|o| &o.name))
                .filter(|n| asks(n))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        Ok(Found {
            items,
            unresolved,
            installed,
            missing,
            casks,
            asking,
            outdated,
            stale,
        })
    }

    fn notes(&self, found: Option<&Found>) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(f) = found {
            let adopt: Vec<&str> = f
                .casks
                .iter()
                .filter(|(_, i)| i.as_ref().is_ok_and(|i| i.app))
                .map(|(n, _)| n.as_str())
                .collect();
            if !adopt.is_empty() {
                notes.push(format!("--adopt for {}", adopt.join(", ")));
            }
            for (name, info) in &f.casks {
                if let Err(e) = info {
                    notes.push(format!(
                        "cannot tell whether cask {name} installs an app ({e}): installing it \
                         without --adopt"
                    ));
                }
            }
        }
        if let Some(n) = &self.note {
            notes.push(n.clone());
        }
        if let Some(f) = found
            && !f.unresolved.is_empty()
        {
            notes.push(format!(
                "no package matches {} yet; looked up again after refreshing the package lists",
                f.unresolved.join(", ")
            ));
        }
        if let Some(f) = found
            && f.stale
        {
            notes.push(format!(
                "upgrades of {} are checked after refreshing the package metadata",
                f.installed.join(", ")
            ));
        }
        let pending = found.is_none_or(Found::has_work);
        if pending && self.sudo(found) {
            notes.push(match (self.kind, found) {
                (PkgManager::Winget, _) => "machine-wide: needs an elevated terminal".into(),
                (PkgManager::Cask, Some(f)) => format!(
                    "{}: Homebrew asks for the sudo password in the terminal",
                    f.asking.join(", ")
                ),
                _ => "sudo".into(),
            });
        }
        notes
    }

    fn sudo(&self, found: Option<&Found>) -> bool {
        if self.kind == PkgManager::Cask {
            // Casks run their installer as root through Homebrew's own sudo, which asks for the
            // password itself ([`Action::asks_for_sudo`]); Homebrew refuses to run as root.
            return found.is_some_and(|f| !f.asking.is_empty());
        }
        let root = self.packages.runner().is_root();
        if self.kind == PkgManager::Winget {
            // Machine-wide installs and upgrades need an elevated terminal (checked up front);
            // only the ones with work to do count.
            let work = |i: &PackageItem| {
                found.is_none_or(|f| {
                    f.missing.contains(i) || f.outdated.iter().any(|o| o.name == i.name)
                })
            };
            return !root && self.items.iter().any(|i| i.machine && work(i));
        }
        self.manager.needs_root() && !root
    }
}

impl PackageAction {
    /// Runs `f`, which installs or upgrades the casks `names` whose installer's sudo asks for
    /// the password on the terminal: the live view paused, and the sudo keep-alive held back,
    /// around it. Their output is still streamed (sudo opens the terminal itself), so it keeps
    /// out of `--json`'s stdout and goes through the log and its redaction.
    fn on_terminal(
        &self,
        ctx: &RunCtx,
        verb: &str,
        names: &[String],
        f: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let sudo = self.packages.sudo();
        let _turn = sudo.terminal();
        ctx.needs_tty(format!(
            "Homebrew asks for your sudo password to {verb} {} (it discards cached sudo \
             credentials)",
            names.join(", ")
        ));
        let result = f();
        ctx.tty_released();
        result
    }
}

impl Action for PackageAction {
    fn describe(&self) -> String {
        let l = label(self.kind);
        let found = self.found();
        let mut d = match found.as_ref() {
            Some(f) if f.missing.is_empty() && f.outdated.is_empty() && f.unresolved.is_empty() => {
                format!("{l}: {} installed", join(&f.items))
            }
            Some(f) if f.missing.is_empty() && f.outdated.is_empty() => {
                format!("{l}: install {}", f.unresolved.join(", "))
            }
            Some(f) => {
                let mut parts = Vec::new();
                if !f.missing.is_empty() {
                    parts.push(format!("install {}", join(&f.missing)));
                }
                if !f.outdated.is_empty() {
                    parts.push(format!("upgrade {}", upgrades(&f.outdated)));
                }
                let mut d = format!("{l}: {}", parts.join("; "));
                let current: Vec<&str> = f
                    .installed
                    .iter()
                    .filter(|n| !f.outdated.iter().any(|o| o.name == **n))
                    .map(String::as_str)
                    .collect();
                if !current.is_empty() {
                    let what = if self.upgrade && !f.stale {
                        "up to date"
                    } else {
                        "already installed"
                    };
                    d.push_str(&format!(" ({what}: {})", current.join(", ")));
                }
                d
            }
            None if !self.manager.available() => format!(
                "{l}: install {} ({} is not installed yet)",
                join(&self.items),
                self.kind.as_str().replace("cask", "brew")
            ),
            None => format!("{l}: install {}", join(&self.items)),
        };
        let notes = self.notes(found.as_ref());
        if !notes.is_empty() {
            d.push_str(&format!(" [{}]", notes.join("; ")));
        }
        d
    }

    /// Satisfied when every package is installed; unknown while the manager itself is not
    /// installed (an earlier bundle may install it).
    fn status(&self) -> Result<Status> {
        if !self.manager.available() {
            *self.found() = None;
            return Ok(Status::Unknown);
        }
        let f = self.check()?;
        let status = if !f.missing.is_empty() || !f.outdated.is_empty() {
            Status::Pending
        } else if f.stale || !f.unresolved.is_empty() {
            // Outdated or not is only known after the refresh that applying does first.
            Status::Unknown
        } else {
            Status::Satisfied
        };
        *self.found() = Some(f);
        Ok(status)
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let l = label(self.kind);
        if !self.manager.available() {
            bail!(
                "{l}: {} is not installed",
                self.kind.as_str().replace("cask", "brew")
            );
        }
        let _one_at_a_time = pkg::lock();
        // The engine checked the status right before applying; without that, check now.
        let cached = self.found().take();
        let mut f = match cached {
            Some(f) => f,
            None => self.check()?,
        };
        if !f.has_work() {
            return Ok(Outcome::Ok);
        }
        if !f.unresolved.is_empty() {
            // A repository added earlier in the run: its packages are known once the lists
            // are refreshed.
            self.manager.refresh(&mut |s, l| ctx.output(s, l))?;
            f = self.check()?;
            if !f.unresolved.is_empty() {
                bail!(
                    "{l}: no package matches {} (after refreshing the package lists)",
                    f.unresolved.join(", ")
                );
            }
            if !f.has_work() {
                return Ok(Outcome::Ok);
            }
        }
        *self.installed_now.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(f.missing.iter().map(|i| i.name.clone()).collect());
        let mut out = |s: Stream, line: &str| ctx.output(s, line);
        for (name, info) in &f.casks {
            if let Err(e) = info {
                out(
                    Stream::Stderr,
                    &format!(
                        "warning: cannot tell whether cask {name} installs an app ({e}); \
                         installing it without --adopt"
                    ),
                );
            }
        }
        self.manager.refresh(&mut out)?;
        // One batch per set of options: casks to adopt, classic snaps, everything else.
        // A cask whose installer asks for the sudo password gets the terminal (the live view
        // paused) when someone is there to type it (else sudo is passwordless: checked up
        // front).
        let interactive = self.packages.sudo().interactive();
        let asks = |name: &str| f.asking.iter().any(|n| n == name);
        let opts_of = |i: &PackageItem| InstallOpts {
            adopt: f
                .casks
                .iter()
                .any(|(n, info)| *n == i.name && info.as_ref().is_ok_and(|c| c.app)),
            classic: i.classic,
            machine: i.machine,
            sudo: asks(&i.name),
        };
        let mut batches: Vec<(InstallOpts, Vec<String>)> = Vec::new();
        for i in &f.missing {
            let o = opts_of(i);
            match batches.iter_mut().find(|(bo, _)| *bo == o) {
                Some((_, names)) => names.push(i.name.clone()),
                None => batches.push((o, vec![i.name.clone()])),
            }
        }
        for (opts, names) in &batches {
            if opts.sudo && interactive {
                self.on_terminal(ctx, "install", names, || {
                    self.manager.install(names, *opts, &mut out)
                })?;
            } else {
                self.manager.install(names, *opts, &mut out)?;
            }
        }
        // winget and scoop put their tools on the `PATH` in the registry, which this process
        // does not see: re-read it, so later steps of the run find what was just installed.
        if matches!(self.kind, PkgManager::Winget | PkgManager::Scoop) && !batches.is_empty() {
            self.run_env
                .add_system_path(dot_platform::winpath::registry_path());
        }
        let mut changed = !f.missing.is_empty();
        if self.upgrade && !f.installed.is_empty() {
            // Stale metadata was refreshed above: only now is it known what is outdated.
            let outdated = if f.stale {
                self.manager.outdated(&f.installed)?
            } else {
                f.outdated.clone()
            };
            if !outdated.is_empty() {
                out(
                    Stream::Stdout,
                    &format!("{}: upgrade {}", label(self.kind), upgrades(&outdated)),
                );
                let (tty, names): (Vec<String>, Vec<String>) = outdated
                    .iter()
                    .map(|o| o.name.clone())
                    .partition(|n| asks(n));
                if !names.is_empty() {
                    self.manager.upgrade(&names, &mut out)?;
                }
                if !tty.is_empty() && interactive {
                    self.on_terminal(ctx, "upgrade", &tty, || {
                        self.manager.upgrade_with_sudo(&tty, &mut out)
                    })?;
                } else if !tty.is_empty() {
                    self.manager.upgrade_with_sudo(&tty, &mut out)?;
                }
                changed = true;
            }
        }
        if !changed {
            return Ok(Outcome::Ok);
        }
        let names: Vec<String> = f.missing.iter().map(|i| i.name.clone()).collect();
        if names.is_empty() {
            return Ok(Outcome::Changed);
        }
        let still: Vec<String> = self
            .manager
            .installed(&names)?
            .into_iter()
            .zip(&names)
            .filter(|(ok, _)| !ok)
            .map(|(_, n)| n.clone())
            .collect();
        if !still.is_empty() {
            out(
                Stream::Stderr,
                &format!(
                    "warning: {l} finished, but still does not list {} as installed; check \
                     the package name (an alias installs under another name)",
                    still.join(", ")
                ),
            );
        }
        Ok(Outcome::Changed)
    }

    /// `pkg:<manager>:<name>` for each package; uninstalled only by `dot remove --packages`,
    /// and only the ones `dot` installed.
    fn resources(&self) -> Vec<Resource> {
        let now = self.installed_now.lock().unwrap_or_else(|e| e.into_inner());
        let resolved = self.resolved.lock().unwrap_or_else(|e| e.into_inner());
        // A pattern not resolved yet names no package.
        resolved
            .as_deref()
            .unwrap_or(&self.items)
            .iter()
            .filter(|i| !pkg::is_name_pattern(&i.name))
            .map(|i| {
                let r = Resource::new("pkg", format!("{}:{}", self.kind.as_str(), i.name))
                    .with_undo(Undo::Uninstall {
                        manager: self.kind.as_str().to_string(),
                        name: i.name.clone(),
                    });
                match now.as_ref() {
                    Some(names) => r.changed(names.contains(&i.name)),
                    None => r,
                }
            })
            .collect()
    }

    fn diff(&self) -> Result<Option<Diff>> {
        let found = self.found();
        let Some(f) = found.as_ref() else {
            return Ok(None);
        };
        if f.missing.is_empty() && f.outdated.is_empty() {
            return Ok(None);
        }
        Ok(Some(Diff::Packages {
            manager: label(self.kind).to_string(),
            missing: f.missing.iter().map(|i| i.name.clone()).collect(),
            outdated: f
                .outdated
                .iter()
                .map(|o| format!("{} {} -> {}", o.name, o.current, o.latest))
                .collect(),
        }))
    }

    /// Root is needed by the Linux managers (unless `dot` is root) and by casks that run a
    /// `.pkg` installer; an elevated terminal by machine-wide winget installs (Windows fails the
    /// run up front when it is not elevated).
    fn needs_sudo(&self) -> bool {
        self.sudo(self.found().as_ref())
    }

    /// Casks whose installer asks for the sudo password (Homebrew's own sudo).
    fn asks_for_sudo(&self) -> bool {
        self.kind == PkgManager::Cask && self.sudo(self.found().as_ref())
    }

    /// Such casks get the terminal when someone is there to type the password.
    fn needs_tty(&self) -> bool {
        self.asks_for_sudo() && self.packages.sudo().interactive()
    }
}

#[cfg(all(test, unix))]
mod tests;
