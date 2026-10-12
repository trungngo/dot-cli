//! `dot doctor`: whether this machine and repository have what `dot` needs, each check one line
//! (`ok`, `warn`, `FAIL`) with what to do. Changes nothing, except removing a leftover run
//! record (the lock of a run that did not finish). Unlocks no provider, prompts for nothing.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use dot_core::StateStore;
use dot_core::config::PkgManager;
use dot_core::facts::{Os, pkg_priority};
use dot_core::state::{CORRUPT_SUFFIX, LEGACY_DIR, RunLockStatus};
use dot_core::workspace::{OpenError, Workspace};
use dot_platform::exec::{Cmd, Runner, SystemRunner};
use dot_platform::pkg::Packages;
use dot_secrets::{EnvfileProvider, Expected, Registry, SecretsConfig};

use crate::DoctorArgs;
use crate::commands::{detect, inputs};
use crate::render::logfile::default_state_dir;
use crate::secrets;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

struct Report {
    lines: Vec<(Level, String, String)>,
}

impl Report {
    fn add(&mut self, level: Level, name: &str, detail: impl Into<String>) {
        self.lines.push((level, name.to_string(), detail.into()));
    }

    fn print(&self) -> ExitCode {
        for (level, name, detail) in &self.lines {
            let tag = match level {
                Level::Ok => "ok  ",
                Level::Warn => "warn",
                Level::Fail => "FAIL",
            };
            println!("{tag}  {name}: {detail}");
        }
        let fails = self.count(Level::Fail);
        let warns = self.count(Level::Warn);
        println!(
            "doctor: {fails} problem{}, {warns} warning{}",
            crate::commands::plural(fails),
            crate::commands::plural(warns)
        );
        if fails == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }

    fn count(&self, level: Level) -> usize {
        self.lines.iter().filter(|(l, ..)| *l == level).count()
    }
}

/// The oldest time a sane clock can show: before this binary's era.
const EPOCH_FLOOR: u64 = 1_735_689_600; // 2025-01-01

pub fn doctor(a: DoctorArgs) -> Result<ExitCode> {
    let inputs = inputs(&a.repo)?;
    let mut r = Report { lines: Vec::new() };
    let runner = SystemRunner;

    // Repository and profile.
    let ws = match Workspace::open(&inputs, detect) {
        Ok(ws) => {
            r.add(
                Level::Ok,
                "repo",
                format!("{} ({})", ws.located.root.display(), ws.located.how),
            );
            match &ws.profile {
                Some((name, how)) => r.add(Level::Ok, "profile", format!("{name} ({how})")),
                None => r.add(Level::Ok, "profile", "none (dot.yml defines no profiles)"),
            }
            Some(ws)
        }
        Err(OpenError::Config(errs)) => {
            let first = errs.first().map(ToString::to_string).unwrap_or_default();
            let more = match errs.len() {
                0 | 1 => String::new(),
                n => format!(" (and {} more; `dot check` lists them)", n - 1),
            };
            r.add(Level::Fail, "repo", format!("{first}{more}"));
            None
        }
        Err(OpenError::Other(e)) => {
            r.add(Level::Fail, "repo", e.to_string());
            None
        }
    };
    let facts = match &ws {
        Some(ws) => ws.facts.clone(),
        None => detect(None),
    };

    // Package manager.
    let native = pkg_priority(facts.os, facts.family.as_deref())
        .first()
        .copied()
        .map(str::to_string);
    match facts.pkg.clone().or(native) {
        Some(name) => {
            let found = PkgManager::parse(&name)
                .is_some_and(|m| Packages::system(None).manager(m).available());
            if found {
                r.add(Level::Ok, "package manager", format!("{name} found"));
            } else {
                r.add(
                    Level::Warn,
                    "package manager",
                    format!("{name} is not on PATH: package steps are unknown until a bundle installs it"),
                );
            }
        }
        None => r.add(
            Level::Warn,
            "package manager",
            "none known for this system: package steps cannot run",
        ),
    }

    // Privileges.
    if facts.os == Os::Windows {
        if runner.is_root() {
            r.add(Level::Ok, "elevation", "running elevated");
        } else {
            r.add(
                Level::Ok,
                "elevation",
                "not elevated: machine-wide steps (system services, winget --scope machine) \
                 need an elevated terminal",
            );
        }
    } else if runner.is_root() {
        r.add(Level::Ok, "sudo", "running as root");
    } else if !runner.on_path("sudo") {
        r.add(
            Level::Warn,
            "sudo",
            "not installed: steps that need root (packages, system services) will fail",
        );
    } else {
        let cached = runner
            .output(&Cmd::new("sudo").args(["-n", "true"]))
            .is_ok_and(|o| o.success());
        r.add(
            Level::Ok,
            "sudo",
            if cached {
                "available without a password right now"
            } else {
                "available; asks for the password once per run (non-interactive runs need \
                 `sudo -v` first or passwordless sudo)"
            },
        );
    }

    // Secrets.
    if let Some(ws) = &ws {
        check_secrets(&mut r, ws, &inputs);
    }

    // git.
    match runner.output(&Cmd::new("git").arg("--version")) {
        Ok(o) if o.success() => r.add(Level::Ok, "git", o.stdout.trim().to_string()),
        _ => r.add(
            Level::Warn,
            "git",
            "not found: `git` steps stay unknown and dot cannot update the repository",
        ),
    }

    // State store, run lock, clock.
    let state_dir = default_state_dir(&inputs.home)
        .unwrap_or_else(|| dot_core::paths::state_dir(&inputs.home, |_| None));
    let store = StateStore::at(&state_dir);
    check_clock(&mut r, &store);
    check_state(&mut r, &store);

    // PATH.
    check_path(&mut r, &inputs.home, &inputs.env);

    Ok(r.print())
}

fn check_secrets(r: &mut Report, ws: &Workspace, inputs: &dot_core::workspace::Inputs) {
    let path = secrets::secrets_file(&ws.located.root);
    if !path.exists() {
        r.add(Level::Ok, "secrets", "no secrets.yml: nothing to resolve");
        return;
    }
    let config = match SecretsConfig::load(&path) {
        Ok(c) => c.with_provider_override(ws.secrets_provider.as_deref()),
        Err(e) => {
            r.add(Level::Fail, "secrets", e.to_string());
            return;
        }
    };
    let penv = secrets::provider_env(inputs);
    let registry = Registry::builtin(&penv);
    let name = config.default_provider.clone();
    match registry.create(&name, &config.providers) {
        None => r.add(
            Level::Fail,
            "secrets provider",
            format!("`{name}` is not available in this build"),
        ),
        Some(Err(e)) => r.add(Level::Fail, "secrets provider", format!("{name}: {e:#}")),
        Some(Ok(p)) => match p.available() {
            Ok(true) => r.add(Level::Ok, "secrets provider", format!("{name} available")),
            Ok(false) => r.add(
                Level::Warn,
                "secrets provider",
                format!("{name} is not available yet (its CLI is installed by a bundle?)"),
            ),
            Err(e) => r.add(Level::Warn, "secrets provider", format!("{name}: {e:#}")),
        },
    }
    let expected = Expected::from_config(&config);
    if name == "envfile" || !expected.keys.is_empty() || !expected.files.is_empty() {
        let provider = EnvfileProvider::from_config(config.providers.envfile.as_ref(), &penv);
        let report = provider.check(&expected);
        let shown = dot_core::paths::display(provider.path(), &inputs.home);
        if !report.problems.is_empty() {
            r.add(Level::Fail, "secrets file", report.problems.join("; "));
        } else if !provider.path().exists() && expected.keys.is_empty() && expected.files.is_empty()
        {
            r.add(
                Level::Ok,
                "secrets file",
                format!("{shown} does not exist (only optional secrets would use it)"),
            );
        } else if !provider.path().exists() {
            r.add(
                Level::Warn,
                "secrets file",
                format!("{shown} does not exist yet"),
            );
        } else if !report.missing_keys.is_empty() || !report.missing_files.is_empty() {
            let mut missing = report.missing_keys.clone();
            missing.extend(report.missing_files.iter().cloned());
            r.add(
                Level::Warn,
                "secrets file",
                format!("{shown}: missing {}", missing.join(", ")),
            );
        } else {
            r.add(
                Level::Ok,
                "secrets file",
                format!("{shown} (private, nothing missing)"),
            );
        }
    }
}

fn check_clock(r: &mut Report, store: &StateStore) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now < EPOCH_FLOOR {
        r.add(
            Level::Fail,
            "clock",
            "the system clock is before 2025: TLS certificates and apt's refresh interval will \
             be wrong; set the time (timedatectl set-ntp true)",
        );
        return;
    }
    // Behind the last write of the state file by more than a few minutes: the clock went back.
    let last = std::fs::metadata(store.path())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    match last {
        Some(t) if t > now + Duration::from_secs(300).as_secs() => r.add(
            Level::Warn,
            "clock",
            format!(
                "the clock is {}s behind the last dot run; check the time zone and NTP",
                t - now
            ),
        ),
        _ => r.add(Level::Ok, "clock", "sane"),
    }
}

fn check_state(r: &mut Report, store: &StateStore) {
    let shown = store.path().display().to_string();
    match store.load() {
        Ok(s) => r.add(
            Level::Ok,
            "state",
            format!(
                "{shown} (schema v{}, {} resource{} recorded)",
                s.version,
                s.resources.len(),
                crate::commands::plural(s.resources.len())
            ),
        ),
        Err(e) => r.add(Level::Fail, "state", e.to_string()),
    }
    let corrupt = PathBuf::from(format!("{shown}{CORRUPT_SUFFIX}"));
    if corrupt.exists() {
        r.add(
            Level::Warn,
            "state",
            format!(
                "{} is an unreadable state file that was set aside; delete it once you no \
                 longer need it",
                corrupt.display()
            ),
        );
    }
    if store.dir().join(LEGACY_DIR).exists() {
        r.add(
            Level::Ok,
            "state",
            format!(
                "migrated from the version-0 files (kept in {})",
                store.dir().join(LEGACY_DIR).display()
            ),
        );
    }
    match store.run_lock_status() {
        RunLockStatus::Free => r.add(Level::Ok, "locks", "no dot run in progress"),
        RunLockStatus::Held(info) => r.add(
            Level::Warn,
            "locks",
            format!(
                "a dot run is in progress{}",
                info.map(|i| format!(" ({})", i.describe()))
                    .unwrap_or_default()
            ),
        ),
        RunLockStatus::Stale(info) => {
            let removed = store.clear_stale_run().is_ok();
            r.add(
                Level::Warn,
                "locks",
                format!(
                    "{} did not finish (killed or crashed); its lock record {}",
                    info.describe(),
                    if removed {
                        "was removed"
                    } else {
                        "could not be removed"
                    }
                ),
            );
        }
    }
}

/// Where `install.sh` / `install.ps1` put `dot`.
fn install_dir(home: &Path, env: &std::collections::BTreeMap<String, String>) -> PathBuf {
    if cfg!(windows) {
        env.get("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Local"))
            .join("Programs")
            .join("dot")
    } else {
        home.join(".local").join("bin")
    }
}

fn check_path(r: &mut Report, home: &Path, env: &std::collections::BTreeMap<String, String>) {
    let want = install_dir(home, env);
    let shown = dot_core::paths::display(&want, home);
    let path = env.get("PATH").map(String::as_str).unwrap_or("");
    let norm = |p: &Path| dot_core::paths::normalize(p);
    let on_path = std::env::split_paths(path).any(|d| norm(&d) == norm(&want));
    if on_path {
        r.add(Level::Ok, "PATH", format!("contains {shown}"));
    } else {
        r.add(
            Level::Warn,
            "PATH",
            format!(
                "does not contain {shown}, where install.sh puts dot and github_release steps \
                 install by default; add it in your shell's profile"
            ),
        );
    }
}
