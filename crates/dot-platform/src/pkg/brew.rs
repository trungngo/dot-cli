//! Homebrew: formulae and casks.
//!
//! Every `brew` command but a few read-only ones discards the sudo timestamp first (Homebrew
//! 4.3.11 and later), so a cask that runs a `.pkg` installer asks for the sudo password itself,
//! on the terminal, whatever `dot` asked for up front (see [`crate::sudo`]). Hence:
//!
//! - commands that never need root (the queries, everything about formulae) set
//!   `HOMEBREW_NO_SUDO=1`, which keeps Homebrew 7.0.7 and later from discarding the timestamp
//!   (older ones ignore it);
//! - a cask whose metadata says it runs sudo ([`InstallOpts::sudo`]) is installed or upgraded
//!   with its sudo free to ask for the password: on the controlling terminal, whatever its stdio,
//!   so its output is still streamed and the caller pauses the live view around it (or made sure
//!   sudo is passwordless);
//! - any other cask command gets `SUDO_ASKPASS=/usr/bin/false`, so a `sudo` it runs after all
//!   fails at once ("a password is required") instead of waiting, unseen, for a password nobody
//!   can type; and, unless sudo is passwordless (where sudo never asks, so nothing changes),
//!   `HOMEBREW_NO_SUDO=1` too, which keeps Homebrew 7.0.7 and later from discarding the
//!   timestamp `dot`'s own root steps rely on.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Result, bail};
use dot_core::config::PkgManager;

use super::{
    InstallOpts, Outdated, PackageManager, first_words, membership, pick_outdated, query, run,
};
use crate::exec::{Cmd, OutFn, Runner};

/// An askpass program that answers nothing: a `sudo -A` that would ask fails at once.
const NO_ASKPASS: &str = "/usr/bin/false";

/// `brew` for formulae (`cask: false`) or casks.
pub struct Brew {
    runner: Arc<dyn Runner>,
    cask: bool,
    /// Whether sudo needs no password at all here (checked once, when a cask first needs it).
    passwordless: OnceLock<bool>,
}

impl Brew {
    pub fn formula(runner: Arc<dyn Runner>) -> Self {
        Self {
            runner,
            cask: false,
            passwordless: OnceLock::new(),
        }
    }

    pub fn cask(runner: Arc<dyn Runner>) -> Self {
        Self {
            runner,
            cask: true,
            passwordless: OnceLock::new(),
        }
    }

    /// `sudo -n -k true`: passwordless sudo, whatever the cached timestamp (which `brew`
    /// discards anyway).
    fn passwordless(&self) -> bool {
        *self.passwordless.get_or_init(|| {
            self.runner.on_path("sudo")
                && self
                    .runner
                    .output(&Cmd::new("sudo").args(["-n", "-k", "true"]))
                    .is_ok_and(|o| o.success())
        })
    }

    fn side(&self) -> &'static str {
        if self.cask { "--cask" } else { "--formula" }
    }

    /// `brew <args>` for a command that never needs root: Homebrew leaves the sudo timestamp
    /// alone.
    fn no_sudo(&self, args: &[&str]) -> Cmd {
        Cmd::new("brew")
            .args(args.iter().copied())
            .env("HOMEBREW_NO_SUDO", "1")
    }

    /// `brew install|upgrade ... --formula|--cask`, set up to never wait for input unseen:
    /// formulae never need root; a cask that may run `sudo` gets an askpass that answers
    /// nothing (and, unless sudo is passwordless, `HOMEBREW_NO_SUDO`, so the timestamp
    /// survives), unless its sudo may ask (`sudo`).
    fn changing(&self, verb: &str, sudo: bool) -> Cmd {
        let cmd = Cmd::new("brew")
            .args([verb, self.side()])
            .env("NONINTERACTIVE", "1")
            .env("HOMEBREW_NO_ENV_HINTS", "1");
        match (self.cask, sudo) {
            (false, _) => cmd.env("HOMEBREW_NO_SUDO", "1"),
            (true, false) if self.passwordless() => cmd.env("SUDO_ASKPASS", NO_ASKPASS),
            (true, false) => cmd
                .env("SUDO_ASKPASS", NO_ASKPASS)
                .env("HOMEBREW_NO_SUDO", "1"),
            (true, true) => cmd,
        }
    }

    /// Runs `cmd` (`brew <verb> ... <packages>`, from [`Brew::changing`] with `sudo`),
    /// streamed to `out`; a cask that failed because it ran sudo, which `dot` did not expect,
    /// says so.
    fn run_changing(
        &self,
        cmd: &Cmd,
        (verb, sudo): (&str, bool),
        packages: &[String],
        out: &mut OutFn<'_>,
    ) -> Result<()> {
        if self.runner.is_root() {
            bail!("Homebrew does not run as root: run dot as your own user");
        }
        let mut asked = false;
        let mut watch = |s: dot_core::Stream, l: &str| {
            if self.cask && !sudo && sudo_refused(l) {
                asked = true;
            }
            out(s, l);
        };
        match run(self.runner.as_ref(), cmd, &mut watch) {
            Ok(_) => Ok(()),
            Err(e) if asked => Err(e.context(format!(
                "Homebrew ran sudo for {}, although the cask's metadata does not say it needs \
                 root, so dot gave it no terminal to ask for the password on: run `brew {verb} \
                 --cask {}` yourself, then run dot again, or allow passwordless sudo",
                packages.join(", "),
                packages.join(" ")
            ))),
            Err(e) => Err(e),
        }
    }
}

/// A line of `brew` output saying a `sudo` could not ask for its password (the askpass that
/// answers nothing, or `HOMEBREW_NO_SUDO`).
fn sudo_refused(line: &str) -> bool {
    line.contains("a password is required")
        || line.contains("no password was provided")
        || line.contains("sudo is disabled by HOMEBREW_NO_SUDO")
        || line.contains("requires sudo, but HOMEBREW_NO_SUDO is set")
}

impl PackageManager for Brew {
    fn kind(&self) -> PkgManager {
        if self.cask {
            PkgManager::Cask
        } else {
            PkgManager::Brew
        }
    }

    fn available(&self) -> bool {
        self.runner.on_path("brew")
    }

    /// `brew list --formula|--cask --versions <names>`: one line (`name version...`) per
    /// installed one; exits 1 when any is not installed.
    ///
    /// The lines carry the canonical name, not the one asked for: `python3` (an alias) lists as
    /// `python@3.14`, a renamed formula under its new name. So exit 0 means every name is
    /// installed, and a name whose line cannot be told apart is asked about on its own, where
    /// exit 0 answers it.
    fn installed(&self, packages: &[String]) -> Result<Vec<bool>> {
        let list = |names: &[String]| {
            let cmd = self
                .no_sudo(&["list", self.side(), "--versions"])
                .args(names.iter().cloned());
            query(self.runner.as_ref(), &cmd, &[0, 1])
        };
        let out = list(packages)?;
        if out.success() {
            return Ok(vec![true; packages.len()]);
        }
        let mut found = membership(packages, &first_words(&out.stdout));
        if packages.len() > 1 {
            for (p, f) in packages.iter().zip(found.iter_mut()) {
                if !*f {
                    *f = list(std::slice::from_ref(p))?.success();
                }
            }
        }
        Ok(found)
    }

    /// `brew install --formula|--cask [--adopt] <names>`. Homebrew's own auto-update is the
    /// refresh. `NONINTERACTIVE` keeps it from ever waiting for input; only the sudo of a cask
    /// whose installer needs root may ask for the password, on the terminal
    /// ([`InstallOpts::sudo`]).
    fn install(&self, packages: &[String], opts: InstallOpts, out: &mut OutFn<'_>) -> Result<()> {
        let sudo = self.cask && opts.sudo;
        let mut cmd = self.changing("install", sudo);
        if self.cask && opts.adopt {
            cmd = cmd.arg("--adopt");
        }
        self.run_changing(
            &cmd.args(packages.iter().cloned()),
            ("install", sudo),
            packages,
            out,
        )
    }

    /// `brew outdated --formula|--cask --json=v2 <names>` (exits 1 when some are outdated),
    /// without Homebrew's auto-update: planning changes nothing. Pinned formulae are left out
    /// (`brew upgrade` leaves them alone).
    fn outdated(&self, packages: &[String]) -> Result<Vec<Outdated>> {
        let cmd = self
            .no_sudo(&["outdated", self.side(), "--json=v2"])
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .args(packages.iter().cloned());
        let out = query(self.runner.as_ref(), &cmd, &[0, 1])?;
        Ok(pick_outdated(packages, parse_outdated_json(&out.stdout)?))
    }

    /// `brew upgrade --formula|--cask <names>`.
    fn upgrade(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        let cmd = self
            .changing("upgrade", false)
            .args(packages.iter().cloned());
        self.run_changing(&cmd, ("upgrade", false), packages, out)
    }

    /// `brew upgrade --cask <names>`, their installer's sudo free to ask for the password.
    fn upgrade_with_sudo(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        let cmd = self
            .changing("upgrade", self.cask)
            .args(packages.iter().cloned());
        self.run_changing(&cmd, ("upgrade", self.cask), packages, out)
    }

    /// `brew uninstall --formula|--cask <names>`. A cask's uninstaller may ask for the sudo
    /// password (`dot remove` has no live view: the prompt shows).
    fn uninstall(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        if self.runner.is_root() {
            bail!("Homebrew does not run as root: run dot as your own user");
        }
        let mut cmd = Cmd::new("brew")
            .args(["uninstall", self.side()])
            .env("NONINTERACTIVE", "1");
        if !self.cask {
            cmd = cmd.env("HOMEBREW_NO_SUDO", "1");
        }
        let cmd = cmd.args(packages.iter().cloned());
        run(self.runner.as_ref(), &cmd, out).map(|_| ())
    }

    fn needs_root(&self) -> bool {
        false
    }
}

/// `brew outdated --json=v2`: `(name, installed, current)` of each formula and cask that is not
/// pinned. A cask's `installed_versions` is a string or a list.
pub fn parse_outdated_json(json: &str) -> Result<Vec<(String, String, String)>> {
    let v: serde_json::Value = serde_json::from_str(json.trim())
        .map_err(|e| anyhow::anyhow!("cannot read `brew outdated` output: {e}"))?;
    let mut out = Vec::new();
    for side in ["formulae", "casks"] {
        for item in v.get(side).and_then(|a| a.as_array()).into_iter().flatten() {
            if item.get("pinned").and_then(|p| p.as_bool()) == Some(true) {
                continue;
            }
            let Some(name) = item.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            let installed = match item.get("installed_versions") {
                Some(serde_json::Value::Array(a)) => a
                    .iter()
                    .filter_map(|x| x.as_str())
                    .next_back()
                    .unwrap_or("")
                    .to_string(),
                Some(serde_json::Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            let current = item
                .get("current_version")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            out.push((name.to_string(), installed, current));
        }
    }
    Ok(out)
}

/// What a cask installs, from Homebrew's API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaskInfo {
    /// It installs an `.app`: safe to `--adopt` a copy already in `/Applications`.
    pub app: bool,
    /// It runs a `.pkg` installer (or an installer script or install steps with `sudo: true`, or
    /// installs a keyboard layout), whose sudo asks for the password: Homebrew discards cached
    /// sudo credentials first.
    pub pkg: bool,
}

/// Reads `https://formulae.brew.sh/api/cask/<token>.json`. Injectable, so tests never use the
/// network.
pub trait CaskFetcher: Send + Sync {
    fn fetch(&self, token: &str) -> Result<String>;
}

/// The API over `curl` (on every Mac); `dot` itself has no HTTP client for this.
pub struct CurlFetcher {
    runner: Arc<dyn Runner>,
}

impl CurlFetcher {
    pub fn new(runner: Arc<dyn Runner>) -> Self {
        Self { runner }
    }
}

pub const CASK_API: &str = "https://formulae.brew.sh/api/cask";

impl CaskFetcher for CurlFetcher {
    fn fetch(&self, token: &str) -> Result<String> {
        let cmd = Cmd::new("curl").args([
            "-fsSL".to_string(),
            "--max-time".to_string(),
            "15".to_string(),
            format!("{CASK_API}/{token}.json"),
        ]);
        Ok(query(self.runner.as_ref(), &cmd, &[0])?.stdout)
    }
}

/// Reads a cask's `artifacts` from its API JSON.
pub fn parse_cask_info(json: &str) -> Result<CaskInfo> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    let Some(artifacts) = v.get("artifacts").and_then(|a| a.as_array()) else {
        bail!("no `artifacts` in the cask's metadata");
    };
    let entries = |key: &str| -> Vec<&serde_json::Value> {
        artifacts
            .iter()
            .filter_map(|a| a.as_object().and_then(|o| o.get(key)))
            .collect()
    };
    let has = |key: &str| !entries(key).is_empty();
    // Homebrew's `requires_sudo?`: always for `pkg` and `keyboard_layout`; for an `installer`
    // script or install-time steps only when they say `sudo: true` (a manual installer, or a
    // script run as the user, does not).
    let sudo_in = |key: &str| entries(key).into_iter().any(says_sudo);
    Ok(CaskInfo {
        app: has("app"),
        pkg: has("pkg")
            || has("keyboard_layout")
            || sudo_in("installer")
            || sudo_in("preflight_steps")
            || sudo_in("postflight_steps"),
    })
}

/// Whether an artifact's arguments ask for sudo anywhere: a `"sudo": true`, or a step that
/// always runs as root (`delete_keychain_certificate`).
fn says_sudo(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Array(a) => a.iter().any(says_sudo),
        serde_json::Value::Object(o) => {
            o.get("sudo") == Some(&serde_json::Value::Bool(true))
                || o.get("type").and_then(|t| t.as_str()) == Some("delete_keychain_certificate")
                || o.values().any(says_sudo)
        }
        _ => false,
    }
}

/// Per-run cache of [`CaskInfo`] lookups (failures included, so an offline run asks once).
pub(super) struct CaskCache {
    fetcher: Arc<dyn CaskFetcher>,
    known: Mutex<HashMap<String, Result<CaskInfo, String>>>,
}

impl CaskCache {
    pub(super) fn new(fetcher: Arc<dyn CaskFetcher>) -> Self {
        Self {
            fetcher,
            known: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn get(&self, token: &str) -> Result<CaskInfo, String> {
        if let Some(v) = self.lock().get(token) {
            return v.clone();
        }
        let v = if token.contains('/') {
            Err("casks from taps are not in Homebrew's API".to_string())
        } else if !token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._+-".contains(c))
        {
            Err(format!("`{token}` is not a cask token"))
        } else {
            self.fetcher
                .fetch(token)
                .and_then(|json| parse_cask_info(&json))
                .map_err(|e| format!("{e:#}"))
        };
        self.lock().insert(token.to_string(), v.clone());
        v
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Result<CaskInfo, String>>> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }
}
