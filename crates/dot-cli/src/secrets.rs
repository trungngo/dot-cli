//! The CLI's side of `dot-secrets`: the repository's `secrets.yml`, the prepare stage that
//! resolves the secret names a plan collected, and the console seam providers prompt through.
//!
//! Planning never touches a provider: `secret("x")` renders a placeholder and records `x`
//! (`Plan::secrets`). Only a real `apply` reaches [`SecretsPrepare`], which unlocks each provider
//! once, resolves every name in one pass, registers each value with the [`Redactor`] and hands
//! the values to steps through a [`SecretCache`]. Anything it cannot resolve fails the run
//! before a single step is applied, naming every missing secret, with one exception: a secret
//! whose provider is not available yet (its CLI is installed by a bundle of this run, such as
//! `lastpass-cli`) is resolved right before the first bundle that uses it, and only that bundle
//! (and its dependents) fail if it still cannot be.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use dot_core::workspace::{Inputs, Workspace};
use dot_core::{
    ConfigError, Event, EventSink, Plan, Prepare, Redactor, SecretCache, SecretLookup, SecretUse,
};
use dot_secrets::{
    Interactive, Partial, ProviderEnv, Registry, Resolver, SecretValue, SecretsConfig,
    SecretsConfigError,
};

/// The mapping from logical secret names to provider refs, at the repository root.
pub const SECRETS_FILE: &str = "secrets.yml";

pub fn secrets_file(root: &Path) -> PathBuf {
    root.join(SECRETS_FILE)
}

/// Where providers find the user's files: the given home and `dot`'s config directory
/// (`$XDG_CONFIG_HOME/dot`, else `~/.config/dot`), as the rest of the CLI sees them.
pub fn provider_env(inputs: &Inputs) -> ProviderEnv {
    let mut env = ProviderEnv::with_home(inputs.home.clone());
    env.config_dir = inputs.config_dir();
    env
}

/// The workspace's `secrets.yml`, with the profile's (else `dot.yml`'s) `secrets_provider` as
/// the provider for bare refs. `None` when the repository has no `secrets.yml`.
pub fn load_config(ws: &Workspace) -> Result<Option<SecretsConfig>, SecretsConfigError> {
    let path = secrets_file(&ws.located.root);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(
        SecretsConfig::load(&path)?.with_provider_override(ws.secrets_provider.as_deref()),
    ))
}

/// A `secrets.yml` problem as a config error (same shape as every other `dot check` error).
pub fn config_error(e: SecretsConfigError) -> ConfigError {
    ConfigError {
        file: e.file,
        line: e.line,
        column: e.column,
        message: e.message,
    }
}

/// `dot check` for `secrets.yml`: parse errors, and refs that do not work under the default
/// provider or any profile's `secrets_provider`. Never contacts a provider.
pub fn check_config(
    root: &Path,
    overrides: &[Option<String>],
    registry: &Registry,
) -> Vec<ConfigError> {
    let path = secrets_file(root);
    if !path.exists() {
        return Vec::new();
    }
    let config = match SecretsConfig::load(&path) {
        Ok(c) => c,
        Err(e) => return vec![config_error(e)],
    };
    let mut problems = BTreeSet::new();
    let mut providers: Vec<Option<String>> = vec![None];
    providers.extend(overrides.iter().cloned());
    providers.dedup();
    for p in providers {
        let c = config.clone().with_provider_override(p.as_deref());
        for problem in c.validate(registry) {
            let under = match &p {
                Some(p) if *p != config.default_provider => format!(" (with provider `{p}`)"),
                _ => String::new(),
            };
            problems.insert(format!("{problem}{under}"));
        }
    }
    problems
        .into_iter()
        .map(|m| ConfigError::new(&path, m))
        .collect()
}

/// The guardrail against committed secrets: every tracked `.env*` file holding a value for a key
/// marked `# secret` (in `.env.example`, or on its own line). Keys, files and lines only, never
/// values. `dot check` and `dot apply` fail on these; `dot secrets check` reports them too.
pub fn committed_secrets(root: &Path, env: &ProviderEnv) -> Vec<ConfigError> {
    let example = root.join(".env.example");
    let keys = if example.exists() {
        match dot_secrets::read_example_secret_keys(&example) {
            Ok(k) => k,
            Err(e) => return vec![ConfigError::new(&example, format!("{e:#}"))],
        }
    } else {
        BTreeSet::new()
    };
    match dot_secrets::scan_committed_env_files(&env.git, root, &keys) {
        Ok(leaks) => leaks
            .into_iter()
            .map(|l| ConfigError {
                file: l.path,
                line: Some(l.line),
                column: None,
                message: format!(
                    "`{}` is a secret but has a committed value; move it to the secrets provider \
                     and rotate it",
                    l.key
                ),
            })
            .collect(),
        Err(e) => vec![ConfigError::new(
            root,
            format!("cannot scan committed .env files: {e:#}"),
        )],
    }
}

/// A resolver over the built-in providers for `config`.
pub fn resolver(config: SecretsConfig, env: &ProviderEnv) -> Resolver {
    Resolver::new(Registry::builtin(env), config)
}

/// The prepare stage of a real run: resolves `Plan::needed` (the secrets of the steps that have
/// work) into the cache that `Context::secrets` hands to templates. Dry runs and `dot plan`
/// never get here.
///
/// A secret that cannot be resolved fails only the bundles that need it (and their
/// dependents), when they are about to start; an `optional: true` one with no value makes the
/// steps that use it skip. A step that had nothing to do when the run was planned but has work
/// by the time it runs gets its secret then, without a prompt ([`SecretCache::set_late`]).
pub struct SecretsPrepare {
    /// `None`: the repository has no `secrets.yml` (or the run is `-c FILE`).
    resolver: Option<Arc<Mutex<Resolver>>>,
    cache: Arc<SecretCache>,
    interactive: bool,
    /// Names the up-front pass could not resolve yet (their provider was not available), and
    /// how they are used; resolved by [`Prepare::before_bundle`].
    deferred: BTreeMap<String, SecretUse>,
    /// Why each deferred name could not be resolved yet.
    deferred_why: BTreeMap<String, String>,
    /// Names that could not be resolved, and why: a bundle that needs one fails.
    failed: BTreeMap<String, String>,
    /// Where the run's bundles name each secret (`bundle.yml:3`), for errors about
    /// undeclared names.
    locations: BTreeMap<String, String>,
}

impl SecretsPrepare {
    /// Every value `resolver` hands out is registered with `redactor` first. `interactive`:
    /// whether a person can answer prompts (false under `--non-interactive` or without a TTY).
    pub fn new(mut resolver: Option<Resolver>, redactor: &Redactor, interactive: bool) -> Self {
        if let Some(r) = resolver.as_mut() {
            let redactor = redactor.clone();
            r.on_resolved(move |v: &SecretValue| {
                if let Some(s) = v.expose_str() {
                    redactor.add(s);
                } else {
                    redactor.add(String::from_utf8_lossy(v.expose()).into_owned());
                }
            });
        }
        let resolver = resolver.map(|r| Arc::new(Mutex::new(r)));
        let cache: Arc<SecretCache> = Arc::default();
        if let Some(r) = &resolver {
            let r = r.clone();
            cache.set_late(Box::new(move |name| {
                let mut r = r.lock().unwrap_or_else(|e| e.into_inner());
                let got = r.resolve_partial([name], &mut dot_secrets::NonInteractive);
                got.resolved.get(name).map(|v| v.expose().to_vec())
            }));
        }
        Self {
            resolver,
            cache,
            interactive,
            deferred: BTreeMap::new(),
            deferred_why: BTreeMap::new(),
            failed: BTreeMap::new(),
            locations: BTreeMap::new(),
        }
    }

    /// Values handed over by the machine that started this run (`dot apply --host`), already
    /// resolved there: kept in memory for this run, registered with `redactor`, and used before
    /// any provider is asked (a provider is never asked for these names at all).
    pub fn preload(&self, received: &[(String, SecretValue)], redactor: &Redactor) {
        for (name, v) in received {
            match v.expose_str() {
                Some(s) => redactor.add(s),
                None => redactor.add(String::from_utf8_lossy(v.expose()).into_owned()),
            }
            self.cache.insert(name.clone(), v.expose().to_vec());
        }
    }

    /// Where each secret is used (name -> `file:line, ...`), named when it is not declared.
    pub fn with_locations(mut self, locations: BTreeMap<String, String>) -> Self {
        self.locations = locations;
        self
    }

    /// Whether `name` is declared `optional: true`.
    fn is_optional(&self, name: &str) -> bool {
        self.resolver.as_ref().is_some_and(|r| {
            r.lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_optional(name)
        })
    }

    /// Puts what `partial` found in the cache and the failure lists. Only a secret used through
    /// `secret()` must be UTF-8; one used as a file may be any bytes.
    fn take(&mut self, partial: Partial, uses: &BTreeMap<String, SecretUse>) {
        for (name, value) in partial.resolved.iter() {
            if uses.get(name).is_some_and(|u| u.text) && value.expose_str().is_none() {
                self.failed.insert(
                    name.to_string(),
                    format!("not UTF-8 text, so `secret(\"{name}\")` cannot use it"),
                );
                continue;
            }
            self.cache.insert(name, value.expose().to_vec());
        }
        for f in partial.deferred {
            let u = uses.get(&f.name).copied().unwrap_or_default();
            self.deferred_why.insert(f.name.clone(), f.reason());
            self.deferred.insert(f.name, u);
        }
        for f in partial.failed {
            if self.is_optional(&f.name) && f.is_missing() {
                self.cache.mark_absent(&f.name);
            } else {
                let mut reason = f.reason();
                if matches!(f.problem, dot_secrets::Problem::Undeclared)
                    && let Some(at) = self.locations.get(&f.name)
                {
                    reason.push_str(&format!("\n  `{}` is used at {at}", f.name));
                }
                self.cache.mark_missing(&f.name);
                self.failed.insert(f.name, reason);
            }
        }
    }

    /// `e`, plus where each undeclared secret among `names` is used.
    fn locate(&self, e: anyhow::Error, names: &[String]) -> anyhow::Error {
        let Some(resolver) = self.resolver.as_ref() else {
            return e;
        };
        let resolver = resolver.lock().unwrap_or_else(|e| e.into_inner());
        let mut msg = format!("{e:#}");
        for n in names {
            if resolver.config().entry(n).is_none()
                && let Some(at) = self.locations.get(n)
            {
                msg.push_str(&format!("\n`{n}` is used at {at}"));
            }
        }
        anyhow!(msg)
    }

    fn explain(&self, e: impl std::fmt::Display) -> anyhow::Error {
        if self.interactive {
            anyhow!("{e}")
        } else {
            anyhow!("{e}\nnothing was prompted for (running non-interactively)")
        }
    }

    /// What templates read while applying; filled by [`Prepare::prepare`].
    pub fn lookup(&self) -> Arc<dyn SecretLookup> {
        self.cache.clone()
    }

    /// The error for a bundle that needs the secrets `failed` (name, why).
    fn failure(&self, failed: &[(String, String)]) -> anyhow::Error {
        let mut msg = format!(
            "{} secret{} could not be resolved:",
            failed.len(),
            plural(failed.len())
        );
        for (n, why) in failed {
            msg.push_str(&format!("\n  {n}: {why}"));
        }
        self.explain(msg)
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

impl Prepare for SecretsPrepare {
    fn prepare(&mut self, plan: &Plan, sink: &EventSink) -> Result<()> {
        // Handed over already (`preload`): nothing to resolve for those.
        let names: Vec<String> = plan
            .needed_names()
            .into_iter()
            .filter(|n| !self.cache.contains(n))
            .collect();
        if names.is_empty() {
            return Ok(());
        }
        let Some(resolver) = self.resolver.clone() else {
            bail!(
                "this run needs secret{} {}, but there is no {SECRETS_FILE} to say where they live",
                plural(names.len()),
                names.join(", ")
            );
        };
        let mut tty = ConsoleTty {
            sink,
            interactive: self.interactive,
            suspended: false,
        };
        let mut partial = resolver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resolve_partial(&names, &mut tty);
        drop(tty);
        // A misconfiguration every secret may share (an undeclared name, a bad ref, a
        // provider that fails, such as a secrets.env others can read) stops the run: it is
        // fixed rather than worked around. A value that is simply not there, or one secret's
        // own file that others can read, fails only the bundles that need it.
        let (hard, missing): (Vec<_>, Vec<_>) = std::mem::take(&mut partial.failed)
            .into_iter()
            .partition(|f| !f.is_scoped());
        if !hard.is_empty() {
            let names: Vec<String> = hard.iter().map(|f| f.name.clone()).collect();
            let e = dot_secrets::ResolveError { failures: hard };
            return Err(self.locate(self.explain(e), &names));
        }
        partial.failed = missing;
        self.take(partial, &plan.needed);
        Ok(())
    }

    fn before_bundle(
        &mut self,
        _name: &str,
        secrets: &BTreeMap<String, SecretUse>,
        sink: &EventSink,
    ) -> Result<()> {
        let wanted: Vec<String> = secrets
            .keys()
            .filter(|n| self.deferred.contains_key(*n) && !self.cache.contains(n))
            .cloned()
            .collect();
        if !wanted.is_empty()
            && let Some(resolver) = self.resolver.clone()
        {
            let mut tty = ConsoleTty {
                sink,
                interactive: self.interactive,
                suspended: false,
            };
            let partial = resolver
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .resolve_partial(&wanted, &mut tty);
            drop(tty);
            for n in &wanted {
                self.deferred.remove(n);
            }
            self.take(partial, secrets);
        }
        let mut failed: Vec<(String, String)> = Vec::new();
        for n in secrets.keys() {
            if let Some(why) = self.failed.get(n) {
                failed.push((n.clone(), why.clone()));
            } else if self.deferred.contains_key(n) && !self.cache.contains(n) {
                // Its provider is still not there: this bundle fails (a later one may find it
                // installed), unless the secret is optional.
                if self.is_optional(n) {
                    self.cache.mark_absent(n);
                } else {
                    let why = self.deferred_why.get(n).cloned().unwrap_or_default();
                    failed.push((n.clone(), why));
                }
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(self.failure(&failed))
        }
    }
}

/// The console as providers see it: `suspend` / `resume` pause the live view with
/// `NeedsTty` / `TtyReleased` (run-level, `step: None`) around an unlock prompt.
struct ConsoleTty<'a> {
    sink: &'a EventSink,
    interactive: bool,
    suspended: bool,
}

impl Interactive for ConsoleTty<'_> {
    fn is_interactive(&self) -> bool {
        self.interactive
    }

    fn suspend(&mut self, reason: &str) {
        if !self.suspended {
            self.suspended = true;
            self.sink.emit(Event::NeedsTty {
                step: None,
                reason: reason.to_string(),
            });
        }
    }

    fn resume(&mut self) {
        if self.suspended {
            self.suspended = false;
            self.sink.emit(Event::TtyReleased { step: None });
        }
    }

    fn read_secret(&mut self, prompt: &str) -> Result<SecretValue> {
        if !self.interactive {
            bail!("cannot ask for {prompt}: running non-interactively");
        }
        let term = console::Term::stderr();
        term.write_str(prompt)?;
        let line = term.read_secure_line()?;
        Ok(SecretValue::from(line))
    }
}

impl Drop for ConsoleTty<'_> {
    fn drop(&mut self) {
        self.resume();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use dot_core::event::Recorder;
    use dot_secrets::mock::MemoryProvider;
    use dot_secrets::{Caps, ProvidersConfig, SecretProvider};

    use super::*;

    fn config(yaml: &str) -> SecretsConfig {
        SecretsConfig::from_yaml(yaml, Path::new("secrets.yml")).unwrap()
    }

    fn resolver_with(p: &MemoryProvider, yaml: &str) -> Resolver {
        let mut reg = Registry::new();
        let p = p.clone();
        reg.register("mem", move |_: &ProvidersConfig| {
            Ok(p.boxed() as Box<dyn SecretProvider>)
        });
        Resolver::new(reg, config(yaml))
    }

    fn plan(names: &[&str]) -> Plan {
        let uses: BTreeMap<String, SecretUse> = names
            .iter()
            .map(|s| (s.to_string(), SecretUse::TEXT))
            .collect();
        Plan {
            bundles: vec![],
            secrets: uses.clone(),
            needed: uses,
        }
    }

    fn uses(names: &[&str]) -> BTreeMap<String, SecretUse> {
        plan(names).needed
    }

    fn text(lookup: &Arc<dyn SecretLookup>, name: &str) -> Option<String> {
        lookup
            .lookup(name)
            .and_then(|v| v.as_str().map(str::to_string))
    }

    const YAML: &str = "default_provider: mem\nsecrets:\n  tok: { ref: TOKEN }\n  other: { ref: OTHER }\n  gone: { ref: GONE }\n";

    #[test]
    fn resolves_into_the_cache_and_the_redactor() {
        let p = MemoryProvider::new("mem")
            .with_value("TOKEN", "s3cr3t-value")
            .with_value("OTHER", "another-one");
        let redactor = Redactor::new();
        let mut prep = SecretsPrepare::new(Some(resolver_with(&p, YAML)), &redactor, false);
        let lookup = prep.lookup();
        prep.prepare(&plan(&["tok", "other"]), &EventSink::new())
            .unwrap();
        assert_eq!(text(&lookup, "tok").as_deref(), Some("s3cr3t-value"));
        assert_eq!(redactor.redact("x s3cr3t-value another-one"), "x *** ***");
        assert_eq!(p.unlocks(), 1);
        assert_eq!(p.resolve_calls(), 1, "one batch");
        // Not needed up front: looked up when a step asks, without a prompt.
        assert!(lookup.lookup("gone").is_none());
        assert_eq!(p.resolve_calls(), 2);
    }

    #[test]
    fn handed_over_values_come_first_and_are_redacted() {
        let p = MemoryProvider::new("mem").with_value("TOKEN", "from-the-provider");
        let redactor = Redactor::new();
        let mut prep = SecretsPrepare::new(Some(resolver_with(&p, YAML)), &redactor, false);
        prep.preload(
            &[(
                "tok".to_string(),
                SecretValue::from("handed-over".to_string()),
            )],
            &redactor,
        );
        let lookup = prep.lookup();
        prep.prepare(&plan(&["tok"]), &EventSink::new()).unwrap();
        assert_eq!(text(&lookup, "tok").as_deref(), Some("handed-over"));
        assert_eq!(
            p.unlocks(),
            0,
            "no provider is asked for a handed-over name"
        );
        assert_eq!(redactor.redact("x handed-over"), "x ***");
        // Without a secrets.yml too: nothing left to resolve.
        let mut none = SecretsPrepare::new(None, &redactor, false);
        none.preload(
            &[("tok".to_string(), SecretValue::from("v".to_string()))],
            &redactor,
        );
        none.prepare(&plan(&["tok"]), &EventSink::new()).unwrap();
    }

    #[test]
    fn nothing_needed_touches_nothing() {
        let p = MemoryProvider::new("mem");
        let mut prep = SecretsPrepare::new(Some(resolver_with(&p, YAML)), &Redactor::new(), false);
        prep.prepare(&plan(&[]), &EventSink::new()).unwrap();
        assert_eq!(p.unlocks(), 0);
        let mut none = SecretsPrepare::new(None, &Redactor::new(), false);
        none.prepare(&plan(&[]), &EventSink::new()).unwrap();
    }

    #[test]
    fn missing_secrets_fail_only_the_bundles_that_need_them() {
        let p = MemoryProvider::new("mem").with_value("TOKEN", "v");
        let mut prep = SecretsPrepare::new(Some(resolver_with(&p, YAML)), &Redactor::new(), false)
            .with_locations([("nope".to_string(), "b/bundle.yml:7".to_string())].into());
        let sink = EventSink::new();
        // The run goes on: what could be resolved is there.
        prep.prepare(&plan(&["tok", "gone"]), &sink).unwrap();
        assert_eq!(text(&prep.lookup(), "tok").as_deref(), Some("v"));
        // A bundle that needs only `tok` starts; one that needs the others fails, naming them.
        prep.before_bundle("a", &uses(&["tok"]), &sink).unwrap();
        let e = prep
            .before_bundle("b", &uses(&["tok", "gone"]), &sink)
            .unwrap_err()
            .to_string();
        assert!(e.contains("1 secret could not be resolved"), "{e}");
        assert!(e.contains("gone: `mem:GONE` not found"), "{e}");
        assert!(e.contains("non-interactively"), "{e}");
        // An undeclared name is a configuration error: the run stops before anything runs.
        let e = prep
            .prepare(&plan(&["tok", "nope"]), &sink)
            .unwrap_err()
            .to_string();
        assert!(e.contains("nope: not declared in secrets.yml"), "{e}");
        assert!(e.contains("`nope` is used at b/bundle.yml:7"), "{e}");

        let mut none = SecretsPrepare::new(None, &Redactor::new(), false);
        let e = none
            .prepare(&plan(&["tok"]), &EventSink::new())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("needs secret tok") && e.contains("no secrets.yml"),
            "{e}"
        );
    }

    #[test]
    fn an_optional_secret_without_a_value_is_absent_not_a_failure() {
        let p = MemoryProvider::new("mem");
        let yaml = "default_provider: mem\nsecrets:\n  db: { ref: DB, optional: true }\n";
        let mut prep = SecretsPrepare::new(Some(resolver_with(&p, yaml)), &Redactor::new(), false);
        let sink = EventSink::new();
        prep.prepare(&plan(&["db"]), &sink).unwrap();
        prep.before_bundle("scripts", &uses(&["db"]), &sink)
            .unwrap();
        let lookup = prep.lookup();
        assert!(lookup.lookup("db").is_none());
        assert!(lookup.absent("db"));
    }

    #[test]
    fn a_secret_not_needed_up_front_is_resolved_when_a_step_asks() {
        let p = MemoryProvider::new("mem").with_value("TOKEN", "late-value");
        let redactor = Redactor::new();
        let prep = SecretsPrepare::new(Some(resolver_with(&p, YAML)), &redactor, false);
        let lookup = prep.lookup();
        assert_eq!(text(&lookup, "tok").as_deref(), Some("late-value"));
        assert_eq!(redactor.redact("late-value"), "***");
        assert!(lookup.lookup("gone").is_none());
    }

    #[test]
    fn unlock_pauses_the_console() {
        let p = MemoryProvider::new("mem")
            .with_value("TOKEN", "v")
            .with_caps(Caps {
                read: true,
                batch: true,
                needs_unlock: true,
                ..Caps::default()
            });
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = EventSink::new().with(Box::new(Recorder(events.clone())));
        let mut prep = SecretsPrepare::new(Some(resolver_with(&p, YAML)), &Redactor::new(), true);
        prep.prepare(&plan(&["tok"]), &sink).unwrap();
        let events = events.lock().unwrap();
        assert!(
            matches!(events.first(), Some(Event::NeedsTty { step: None, .. })),
            "{events:?}"
        );
        assert!(
            matches!(events.last(), Some(Event::TtyReleased { step: None })),
            "{events:?}"
        );
    }

    #[test]
    fn check_config_reports_bad_refs_per_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::builtin(&ProviderEnv::with_home(tmp.path()));
        assert!(
            check_config(tmp.path(), &[], &reg).is_empty(),
            "no file, no problems"
        );
        std::fs::write(
            tmp.path().join(SECRETS_FILE),
            "secrets:\n  tok: { ref: TOKEN }\n  x: { ref: \"nosuch:X\" }\n",
        )
        .unwrap();
        let errs: Vec<String> = check_config(tmp.path(), &[Some("lastpass".into())], &reg)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(errs.iter().any(|e| e.contains("nosuch")), "{errs:?}");
        assert!(
            errs.iter()
                .any(|e| e.contains("tok") && e.contains("with provider `lastpass`")),
            "{errs:?}"
        );
        std::fs::write(
            tmp.path().join(SECRETS_FILE),
            "secrets:\n  tok: { reff: X }\n",
        )
        .unwrap();
        let errs = check_config(tmp.path(), &[], &reg);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].line.is_some(), "{:?}", errs[0]);
    }
}

#[cfg(test)]
mod incremental {
    use std::sync::Arc;

    use dot_secrets::mock::MemoryProvider;
    use dot_secrets::{ProvidersConfig, SecretProvider};

    use super::*;

    fn resolver_with(ps: &[&MemoryProvider], yaml: &str) -> Resolver {
        let mut reg = Registry::new();
        for p in ps {
            let p = (*p).clone();
            reg.register(p.scheme(), move |_: &ProvidersConfig| {
                Ok(p.boxed() as Box<dyn SecretProvider>)
            });
        }
        Resolver::new(
            reg,
            SecretsConfig::from_yaml(yaml, Path::new("secrets.yml")).unwrap(),
        )
    }

    fn uses(pairs: &[(&str, SecretUse)]) -> BTreeMap<String, SecretUse> {
        pairs.iter().map(|(n, u)| (n.to_string(), *u)).collect()
    }

    #[test]
    fn a_provider_installed_during_the_run_is_used_by_later_bundles() {
        let lp = MemoryProvider::new("lp")
            .with_value("GL", "glpat-late")
            .unavailable();
        let ef = MemoryProvider::new("ef").with_value("A", "early-a");
        let yaml = "secrets:\n  glab: { ref: \"lp:GL\" }\n  a: { ref: \"ef:A\" }\n";
        let redactor = Redactor::new();
        let mut prep =
            SecretsPrepare::new(Some(resolver_with(&[&lp, &ef], yaml)), &redactor, false);
        let lookup = prep.lookup();
        let plan = Plan {
            bundles: vec![],
            secrets: uses(&[("glab", SecretUse::TEXT), ("a", SecretUse::TEXT)]),
            needed: uses(&[("glab", SecretUse::TEXT), ("a", SecretUse::TEXT)]),
        };
        let sink = EventSink::new();
        prep.prepare(&plan, &sink).unwrap();
        assert!(lookup.lookup("a").is_some());
        assert!(lookup.lookup("glab").is_none(), "deferred");
        // Not installed yet when the bundle runs: that bundle fails.
        let e = prep
            .before_bundle("git", &uses(&[("glab", SecretUse::TEXT)]), &sink)
            .unwrap_err()
            .to_string();
        assert!(e.contains("provider `lp` is not available"), "{e}");
        // The lastpass-cli bundle ran: the next bundle gets it, and it is redacted.
        lp.set_available(true);
        prep.before_bundle("git", &uses(&[("glab", SecretUse::TEXT)]), &sink)
            .unwrap();
        assert!(lookup.lookup("glab").is_some());
        assert_eq!(redactor.redact("glpat-late"), "***");
        let _ = Arc::clone(&lookup);
    }

    #[test]
    fn only_text_secrets_must_be_utf8() {
        let p = MemoryProvider::new("mem")
            .with_file("vpn.p12", &[0xff, 0x00, 0x01])
            .with_file("t.bin", &[0xff]);
        let yaml = "secrets:\n  vpn: { ref: \"mem-file:vpn.p12\", kind: file }\n  t: { ref: \"mem-file:t.bin\", kind: file }\n";
        let mut prep =
            SecretsPrepare::new(Some(resolver_with(&[&p], yaml)), &Redactor::new(), false);
        let lookup = prep.lookup();
        let plan = Plan {
            bundles: vec![],
            secrets: uses(&[("vpn", SecretUse::FILE)]),
            needed: uses(&[("vpn", SecretUse::FILE)]),
        };
        prep.prepare(&plan, &EventSink::new()).unwrap();
        assert_eq!(lookup.lookup("vpn").unwrap().as_bytes(), [0xff, 0x00, 0x01]);

        let mut prep =
            SecretsPrepare::new(Some(resolver_with(&[&p], yaml)), &Redactor::new(), false);
        let plan = Plan {
            bundles: vec![],
            secrets: uses(&[("t", SecretUse::TEXT)]),
            needed: uses(&[("t", SecretUse::TEXT)]),
        };
        prep.prepare(&plan, &EventSink::new()).unwrap();
        let e = prep
            .before_bundle("b", &uses(&[("t", SecretUse::TEXT)]), &EventSink::new())
            .unwrap_err()
            .to_string();
        assert!(e.contains("not UTF-8 text"), "{e}");
    }
}
