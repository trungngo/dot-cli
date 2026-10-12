//! From logical names to values, following the lifecycle in docs/design.md ("Secrets"):
//!
//! 1. every name is looked up in `secrets.yml` (undeclared names are failures, not panics);
//! 2. the primary refs are grouped by provider; each provider is created, checked for
//!    availability and unlocked once per [`Resolver`] (so once per run), then asked for its whole
//!    group in one `resolve` call (one call per ref when it lacks [`Caps::batch`]);
//! 3. names whose primary ref is missing (no value, provider unavailable or not compiled in, or
//!    no file support) retry with their `fallback`, grouped and batched the same way;
//! 4. names still missing whose value is simply absent from a working provider (not undeclared,
//!    not a failing provider, not a file) are asked for when a person is at the terminal
//!    (hidden input, console suspended); the answer is stored in the first provider where the
//!    value was looked for and not found, if that provider can write ([`Caps::write`]), else
//!    used for this run only;
//! 5. if anything is still missing the run fails with one error listing every such name.
//!
//! A provider that *fails* (unlock refused, guardrail tripped, CLI error) is not treated as
//! missing: its secrets fail without falling back, so a misconfiguration is fixed rather than
//! papered over. A problem with one ref only (a file secret others can read) fails that ref
//! alone ([`Problem::RefFailed`]); the rest of its batch resolves. One exception: a provider that needs unlocking while nobody can answer a prompt
//! (`--non-interactive`, no terminal) is [`Problem::Locked`], which does fall back, so a server
//! run can use its `envfile` fallback while `lpass` is installed but logged out.
//!
//! A provider that is not available is asked again on the next `resolve`: a bundle earlier in
//! the same run may have installed its CLI. [`Resolver::resolve_available`] is the up-front pass
//! that resolves what it can now and defers the names whose providers are not there yet.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::config::SecretsConfig;
use crate::registry::{Registry, missing_provider_hint};
use crate::{Caps, Interactive, SecretProvider, SecretRef, SecretValue};

type Hook = Box<dyn Fn(&SecretValue) + Send + Sync>;

pub struct Resolver {
    registry: Registry,
    config: SecretsConfig,
    /// Providers created (and unlocked) so far, or why one could not be.
    providers: BTreeMap<String, Result<Box<dyn SecretProvider>, Problem>>,
    on_resolved: Option<Hook>,
}

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver")
            .field("registry", &self.registry)
            .field("default_provider", &self.config.default_provider)
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Why one ref produced no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The name is not in `secrets.yml`.
    Undeclared,
    /// The spec's ref cannot be used under the effective provider (e.g. a bare name while the
    /// profile's provider is `lastpass`).
    BadRef(String),
    UnknownProvider {
        provider: String,
    },
    Unavailable {
        provider: String,
    },
    /// Needs unlocking, and nobody can answer the prompt (non-interactive run).
    Locked {
        provider: String,
        message: String,
    },
    NoFileSupport {
        reference: String,
        provider: String,
    },
    NotFound {
        reference: String,
        location: Option<String>,
    },
    /// The provider itself failed; never falls back.
    ProviderFailed {
        provider: String,
        message: String,
    },
    /// This one ref could not be read (a file secret others can read); never falls back, and
    /// fails only what uses it ([`SecretProvider::resolve_each`]).
    RefFailed {
        reference: String,
        message: String,
    },
}

impl Problem {
    fn falls_back(&self) -> bool {
        matches!(
            self,
            Self::UnknownProvider { .. }
                | Self::Unavailable { .. }
                | Self::Locked { .. }
                | Self::NoFileSupport { .. }
                | Self::NotFound { .. }
        )
    }

    /// The provider may become available later in the run (installed by a bundle).
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undeclared => f.write_str("not declared in secrets.yml"),
            Self::BadRef(msg) => f.write_str(msg),
            Self::UnknownProvider { provider } => {
                write!(
                    f,
                    "provider `{provider}` {}",
                    missing_provider_hint(provider)
                )
            }
            Self::Unavailable { provider } => {
                write!(f, "provider `{provider}` is not available on this machine")
            }
            Self::Locked { provider, message } => write!(
                f,
                "provider `{provider}` is locked and cannot be unlocked without a terminal: \
                 {message}"
            ),
            Self::NoFileSupport {
                reference,
                provider,
            } => write!(
                f,
                "`{reference}`: provider `{provider}` does not support file secrets"
            ),
            Self::NotFound {
                reference,
                location: Some(loc),
            } => write!(f, "`{reference}` not found in {loc}"),
            Self::NotFound {
                reference,
                location: None,
            } => write!(f, "`{reference}` not found"),
            Self::ProviderFailed { provider, message } => {
                write!(f, "provider `{provider}` failed: {message}")
            }
            Self::RefFailed { reference, message } => write!(f, "`{reference}`: {message}"),
        }
    }
}

/// A secret that could not be resolved: its primary problem and, when a fallback was tried,
/// the fallback's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub name: String,
    pub problem: Problem,
    pub fallback: Option<Problem>,
}

impl Failure {
    /// Only unavailable providers stood in the way (primary and fallback alike): resolving
    /// again later in the run may work.
    pub fn is_deferrable(&self) -> bool {
        self.problem.is_unavailable() && self.fallback.as_ref().is_none_or(Problem::is_unavailable)
    }
}

/// Every secret a run needs that could not be resolved. Names and refs only, never values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ResolveError {
    pub failures: Vec<Failure>,
}

impl ResolveError {
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.failures.iter().map(|f| f.name.as_str())
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.failures.len();
        write!(
            f,
            "{n} secret{} could not be resolved:",
            if n == 1 { "" } else { "s" }
        )?;
        for fail in &self.failures {
            write!(f, "\n  {}: {}", fail.name, fail.problem)?;
            if let Some(fb) = &fail.fallback {
                write!(f, "; fallback: {fb}")?;
            }
        }
        Ok(())
    }
}

/// What [`Resolver::resolve_partial`] found.
#[derive(Debug, Default)]
pub struct Partial {
    pub resolved: Resolved,
    /// Only unavailable providers stood in the way: try again later in the run.
    pub deferred: Vec<Failure>,
    pub failed: Vec<Failure>,
}

impl Failure {
    /// The failure as a one-line reason (names and refs only).
    pub fn reason(&self) -> String {
        match &self.fallback {
            Some(fb) => format!("{}; fallback: {fb}", self.problem),
            None => self.problem.to_string(),
        }
    }

    /// Whether the value is simply not there (no value, provider unavailable or locked): an
    /// optional secret then counts as absent. A failing provider or a bad ref does not.
    pub fn is_missing(&self) -> bool {
        self.problem.falls_back() && self.fallback.as_ref().is_none_or(Problem::falls_back)
    }

    /// Whether the failure concerns this secret alone (its value is missing, or its own ref
    /// could not be read), so a run fails only what uses it. `false` for a misconfiguration
    /// every secret of the run may share: an undeclared name, a bad ref, a failing provider.
    pub fn is_scoped(&self) -> bool {
        let scoped = |p: &Problem| p.falls_back() || matches!(p, Problem::RefFailed { .. });
        scoped(&self.problem) && self.fallback.as_ref().is_none_or(scoped)
    }
}

/// Resolved values by logical name. `Debug` shows names only.
#[derive(Default)]
pub struct Resolved {
    values: BTreeMap<String, SecretValue>,
    /// Names whose value was typed in, and the ref it was stored under (if any).
    prompted: BTreeMap<String, Option<String>>,
}

impl fmt::Debug for Resolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.values.iter()).finish()
    }
}

impl Resolved {
    pub fn get(&self, name: &str) -> Option<&SecretValue> {
        self.values.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &SecretValue)> {
        self.values.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Adds the values of `other` (a later resolve in the same run).
    pub fn extend(&mut self, other: Resolved) {
        self.values.extend(other.values);
        self.prompted.extend(other.prompted);
    }

    /// Names that were asked for, in order.
    pub fn prompted(&self) -> impl Iterator<Item = &str> {
        self.prompted.keys().map(String::as_str)
    }

    /// Answers that were stored: `(name, ref)`.
    pub fn stored(&self) -> impl Iterator<Item = (&str, &str)> {
        self.prompted
            .iter()
            .filter_map(|(n, r)| r.as_deref().map(|r| (n.as_str(), r)))
    }
}

impl Resolver {
    pub fn new(registry: Registry, config: SecretsConfig) -> Self {
        Self {
            registry,
            config,
            providers: BTreeMap::new(),
            on_resolved: None,
        }
    }

    /// Called once for every value handed out, before `resolve` returns: the CLI registers
    /// each with the log redactor here.
    pub fn on_resolved(
        &mut self,
        hook: impl Fn(&SecretValue) + Send + Sync + 'static,
    ) -> &mut Self {
        self.on_resolved = Some(Box::new(hook));
        self
    }

    pub fn config(&self) -> &SecretsConfig {
        &self.config
    }

    /// Resolves `names` (duplicates are fine). All or nothing: on error no value is returned
    /// and the error lists every name that failed.
    pub fn resolve<I, S>(
        &mut self,
        names: I,
        tty: &mut dyn Interactive,
    ) -> Result<Resolved, ResolveError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let (values, failures, prompted) = self.resolve_inner(names, tty);
        if !failures.is_empty() {
            return Err(ResolveError { failures });
        }
        Ok(self.hand_out(values, prompted))
    }

    /// The up-front pass of a run: resolves every name it can now. Names that failed only
    /// because their providers are not available (yet) come back in the second list, to be
    /// resolved again when a step needs them; any other failure is an error listing every such
    /// name (and nothing is returned).
    pub fn resolve_available<I, S>(
        &mut self,
        names: I,
        tty: &mut dyn Interactive,
    ) -> Result<(Resolved, Vec<String>), ResolveError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let (values, failures, prompted) = self.resolve_inner(names, tty);
        let (deferred, hard): (Vec<Failure>, Vec<Failure>) =
            failures.into_iter().partition(Failure::is_deferrable);
        if !hard.is_empty() {
            return Err(ResolveError { failures: hard });
        }
        Ok((
            self.hand_out(values, prompted),
            deferred.into_iter().map(|f| f.name).collect(),
        ))
    }

    /// Resolves every name it can, never failing as a whole: the values, the names whose
    /// providers are not available (yet), and every other failure. For a run that fails only
    /// the bundles that need what is missing.
    pub fn resolve_partial<I, S>(&mut self, names: I, tty: &mut dyn Interactive) -> Partial
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let (values, failures, prompted) = self.resolve_inner(names, tty);
        let (deferred, failed): (Vec<Failure>, Vec<Failure>) =
            failures.into_iter().partition(Failure::is_deferrable);
        Partial {
            resolved: self.hand_out(values, prompted),
            deferred,
            failed,
        }
    }

    /// Whether `name` is declared `optional: true`.
    pub fn is_optional(&self, name: &str) -> bool {
        self.config.secrets.get(name).is_some_and(|s| s.optional)
    }

    fn hand_out(
        &self,
        values: BTreeMap<String, SecretValue>,
        prompted: BTreeMap<String, Option<String>>,
    ) -> Resolved {
        if let Some(hook) = &self.on_resolved {
            values.values().for_each(hook);
        }
        Resolved { values, prompted }
    }

    #[allow(clippy::type_complexity)]
    fn resolve_inner<I, S>(
        &mut self,
        names: I,
        tty: &mut dyn Interactive,
    ) -> (
        BTreeMap<String, SecretValue>,
        Vec<Failure>,
        BTreeMap<String, Option<String>>,
    )
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let names: BTreeSet<String> = names.into_iter().map(|n| n.as_ref().to_string()).collect();
        let mut failures: BTreeMap<String, Failure> = BTreeMap::new();
        let mut fallbacks = BTreeMap::new();
        let mut primary = Vec::new();
        let mut entries = BTreeMap::new();
        for name in names {
            match self.config.entry(&name) {
                None => fail(&mut failures, name, Problem::Undeclared, None),
                Some(Err(e)) => fail(&mut failures, name, Problem::BadRef(e.to_string()), None),
                Some(Ok(entry)) => {
                    if let Some(fb) = entry.fallback.clone() {
                        fallbacks.insert(name.clone(), fb);
                    }
                    primary.push((name.clone(), entry.reference.clone()));
                    entries.insert(name, entry);
                }
            }
        }

        let mut values = BTreeMap::new();
        let mut retry = Vec::new();
        let mut first_problems = BTreeMap::new();
        for (name, result) in self.fetch(primary, tty) {
            match result {
                Ok(v) => {
                    values.insert(name, v);
                }
                Err(p) => match fallbacks.remove(&name) {
                    Some(fb) if p.falls_back() => {
                        first_problems.insert(name.clone(), p);
                        retry.push((name, fb));
                    }
                    _ => fail(&mut failures, name, p, None),
                },
            }
        }
        for (name, result) in self.fetch(retry, tty) {
            let first = first_problems
                .remove(&name)
                .expect("retried names have a problem");
            match result {
                Ok(v) => {
                    values.insert(name, v);
                }
                Err(p) => fail(&mut failures, name, first, Some(p)),
            }
        }

        let mut prompted = BTreeMap::new();
        if tty.is_interactive() {
            let askable: Vec<(String, SecretRef)> = failures
                .values()
                .filter_map(|f| {
                    let entry = entries.get(&f.name)?;
                    let target = ask_target(f, entry)?;
                    Some((f.name.clone(), target.clone()))
                })
                .collect();
            for (name, target) in askable {
                match self.ask(&name, &target, tty) {
                    Ok(Some((value, stored))) => {
                        failures.remove(&name);
                        values.insert(name.clone(), value);
                        prompted.insert(name, stored);
                    }
                    Ok(None) => {}
                    Err(problem) => {
                        if let Some(f) = failures.get_mut(&name) {
                            f.problem = problem;
                            f.fallback = None;
                        }
                    }
                }
            }
        }

        (values, failures.into_values().collect(), prompted)
    }

    /// Asks for `name` on the terminal and stores the answer under `target` when its provider
    /// can write. `Ok(None)`: no answer (empty, or the terminal could not be read).
    #[allow(clippy::type_complexity)]
    fn ask(
        &mut self,
        name: &str,
        target: &SecretRef,
        tty: &mut dyn Interactive,
    ) -> Result<Option<(SecretValue, Option<String>)>, Problem> {
        let writer = match self.providers.get(target.provider()) {
            Some(Ok(p)) if p.capabilities().write => Some(p.as_ref()),
            _ => None,
        };
        let saved = match writer {
            Some(p) => format!(
                "it will be saved to {}",
                p.location().unwrap_or_else(|| format!("`{target}`"))
            ),
            None => "it is used for this run only".to_string(),
        };
        tty.suspend(&format!("asking for secret `{name}`"));
        let answer = tty.read_secret(&format!(
            "Secret `{name}` ({target}) is not set; {saved}. Value (hidden): "
        ));
        tty.resume();
        let value = match answer {
            Ok(v) if !v.is_empty() => v,
            _ => return Ok(None),
        };
        let Some(p) = writer else {
            return Ok(Some((value, None)));
        };
        match p.store(target, &value) {
            Ok(()) => Ok(Some((value, Some(target.to_string())))),
            Err(e) => Err(Problem::ProviderFailed {
                provider: target.provider().to_string(),
                message: format!("storing the value typed for `{name}` failed: {e:#}"),
            }),
        }
    }

    /// Resolves `(name, ref)` pairs, grouped by provider.
    fn fetch(
        &mut self,
        wanted: Vec<(String, SecretRef)>,
        tty: &mut dyn Interactive,
    ) -> Vec<(String, Result<SecretValue, Problem>)> {
        let mut by_provider: BTreeMap<String, Vec<(String, SecretRef)>> = BTreeMap::new();
        for (name, r) in wanted {
            by_provider
                .entry(r.provider().to_string())
                .or_default()
                .push((name, r));
        }
        let mut out = Vec::new();
        for (provider, group) in by_provider {
            let p = match self.ready(&provider, tty) {
                Ok(p) => p,
                Err(problem) => {
                    out.extend(group.into_iter().map(|(n, _)| (n, Err(problem.clone()))));
                    continue;
                }
            };
            let caps = p.capabilities();
            let (group, unsupported): (Vec<_>, Vec<_>) = group
                .into_iter()
                .partition(|(_, r)| caps.files || !r.is_file());
            out.extend(unsupported.into_iter().map(|(n, r)| {
                let problem = Problem::NoFileSupport {
                    reference: r.to_string(),
                    provider: provider.clone(),
                };
                (n, Err(problem))
            }));
            if group.is_empty() {
                continue;
            }
            let unique: Vec<SecretRef> = group
                .iter()
                .map(|(_, r)| r.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            match resolve_group(p, caps, &unique) {
                Ok(found) => {
                    let location = p.location();
                    for (name, r) in group {
                        let result = match found.get(&r) {
                            Some(Ok(v)) => Ok(v.clone()),
                            Some(Err(message)) => Err(Problem::RefFailed {
                                reference: r.to_string(),
                                message: message.clone(),
                            }),
                            None => Err(Problem::NotFound {
                                reference: r.to_string(),
                                location: location.clone(),
                            }),
                        };
                        out.push((name, result));
                    }
                }
                Err(message) => {
                    let problem = Problem::ProviderFailed {
                        provider: provider.clone(),
                        message,
                    };
                    out.extend(group.into_iter().map(|(n, _)| (n, Err(problem.clone()))));
                }
            }
        }
        out
    }

    /// The provider named `name`, created, checked and unlocked on first use. An unavailable
    /// one is checked again each time (a bundle may have installed it since).
    fn ready(
        &mut self,
        name: &str,
        tty: &mut dyn Interactive,
    ) -> Result<&dyn SecretProvider, Problem> {
        let retry = match self.providers.get(name) {
            None => true,
            Some(Err(p)) => p.is_unavailable(),
            Some(Ok(_)) => false,
        };
        if retry {
            let state = self.open(name, tty);
            self.providers.insert(name.to_string(), state);
        }
        match &self.providers[name] {
            Ok(p) => Ok(p.as_ref()),
            Err(problem) => Err(problem.clone()),
        }
    }

    fn open(
        &self,
        name: &str,
        tty: &mut dyn Interactive,
    ) -> Result<Box<dyn SecretProvider>, Problem> {
        let failed = |e: anyhow::Error| Problem::ProviderFailed {
            provider: name.to_string(),
            message: format!("{e:#}"),
        };
        let mut p = match self.registry.create(name, &self.config.providers) {
            None => {
                return Err(Problem::UnknownProvider {
                    provider: name.to_string(),
                });
            }
            Some(created) => created.map_err(failed)?,
        };
        if !p.available().map_err(failed)? {
            return Err(Problem::Unavailable {
                provider: name.to_string(),
            });
        }
        let pause = p.capabilities().needs_unlock;
        if pause {
            tty.suspend(&format!("unlocking secrets provider `{name}`"));
        }
        let interactive = tty.is_interactive();
        let unlocked = p.unlock(tty);
        if pause {
            tty.resume();
        }
        match unlocked {
            Ok(()) => Ok(p),
            Err(e) if pause && !interactive => Err(Problem::Locked {
                provider: name.to_string(),
                message: format!("{e:#}"),
            }),
            Err(e) => Err(failed(e)),
        }
    }
}

/// The ref a missing secret's answer belongs to: the first of its refs whose provider works but
/// has no value. `None` when the secret should not be asked for: a file secret, a failing
/// provider, or no provider that could be asked (unavailable, locked, not compiled in).
fn ask_target<'a>(f: &Failure, entry: &'a crate::config::SecretEntry) -> Option<&'a SecretRef> {
    if entry.kind == crate::config::SecretKind::File || entry.optional || !f.problem.falls_back() {
        return None;
    }
    let target = if matches!(f.problem, Problem::NotFound { .. }) {
        &entry.reference
    } else {
        match (&f.fallback, &entry.fallback) {
            (Some(Problem::NotFound { .. }), Some(fb)) => fb,
            _ => return None,
        }
    };
    (!target.is_file()).then_some(target)
}

fn fail(
    failures: &mut BTreeMap<String, Failure>,
    name: String,
    problem: Problem,
    fallback: Option<Problem>,
) {
    failures.insert(
        name.clone(),
        Failure {
            name,
            problem,
            fallback,
        },
    );
}

/// One batched call (or one call per ref without [`Caps::batch`]): each ref's value, or why that
/// ref alone failed (a ref with no value is left out). `Err` carries the provider's message
/// when it failed as a whole.
fn resolve_group(
    p: &dyn SecretProvider,
    caps: Caps,
    refs: &[SecretRef],
) -> Result<BTreeMap<SecretRef, Result<SecretValue, String>>, String> {
    let values = if caps.batch {
        p.resolve_each(refs).map_err(|e| format!("{e:#}"))?
    } else {
        let mut all = Vec::with_capacity(refs.len());
        for r in refs {
            all.extend(
                p.resolve_each(std::slice::from_ref(r))
                    .map_err(|e| format!("{e:#}"))?,
            );
        }
        all
    };
    if values.len() != refs.len() {
        return Err(format!(
            "returned {} values for {} refs",
            values.len(),
            refs.len()
        ));
    }
    Ok(refs
        .iter()
        .cloned()
        .zip(values)
        .filter_map(|(r, v)| v.transpose().map(|v| (r, v)))
        .collect())
}
