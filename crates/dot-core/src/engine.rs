//! Plan/apply orchestration. Directive implementations live in `dot-directives` and reach the
//! engine through a [`Planner`], so this crate never depends on them.
//!
//! A run has three stages:
//!
//! 1. **Plan** every step up front, without changing anything. This is a preview for `dot plan`,
//!    `--dry-run` and the `PlanReady` event, and the place where config errors (a bad path after
//!    rendering, an undefined variable) are found. Facts only known at run time (a source an
//!    earlier step downloads) are [`Status::Unknown`] here, not errors.
//! 2. **Prepare**: unlock secret providers and resolve the secret names planning collected
//!    ([`Prepare`]). Skipped by dry runs, so they never prompt. Secrets whose provider is not
//!    available yet (its CLI is installed by an earlier bundle of the same run) are resolved
//!    right before the first bundle that needs them ([`Prepare::before_bundle`]).
//! 3. **Execute**: each step is planned *again* right before it runs, after the earlier steps have
//!    changed the machine, and each action's status is checked right before it is applied.
//!    Bundles run in the order given (the graph's topological order); a bundle whose dependency
//!    failed is skipped, the others continue. A bundle whose `check:` command succeeds is
//!    already satisfied and skipped, unless [`Options::force`].
//!
//! With [`Options::jobs`] above 1, bundles whose [`Schedule`] predecessors (`requires` and
//! `after`) have finished run at the same time, on worker threads, up to that many at once;
//! the steps of one bundle still run in order. Starting a bundle (its `check:`, the deferred
//! secrets of [`Prepare::before_bundle`]) and finishing it happen on the calling thread, which
//! is the only one that emits `BundleStarted` / `BundleFinished`. A step that hands the terminal
//! over or needs root ([`Action::needs_tty`], [`Action::needs_sudo`]) runs alone: it waits for
//! the running steps to finish and holds the others back until it is done; so does resolving a
//! bundle's deferred secrets (a provider may unlock on the terminal). Package managers are
//! serialised by their own lock (`dot_platform::pkg::lock`), whatever the schedule.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::time::Instant;

use anyhow::Result;

use crate::action::{Action, Notices, Outcome, RunCtx, Status, SudoCheck};
use crate::config::{Bundle, ConfigError, Defaults, Directive, Guard, Mode, ModeSpec, Step, When};
use crate::event::{
    ActionView, BundleView, Event, EventSink, PlanView, StepId, StepView, StepViewState, Summary,
};
use crate::facts::Facts;
use crate::handlers::{NOT_NOTIFIED, PendingHandlers};
use crate::paths;
use crate::runenv::RunEnvCell;
use crate::secret::{SecretBytes, SecretLookup, SecretUse};
use crate::state::{Restore, StateStore};
use crate::template::{Indirect, SecretMode, Vars, secret_placeholder};

/// Run-wide inputs to planning. `home` is passed in (never read from the environment here) so
/// tests can point it at a temp dir.
#[derive(Clone)]
pub struct Context {
    pub facts: Facts,
    pub home: PathBuf,
    /// Option defaults inherited by every bundle (`dot.yml`'s under the profile's). Each
    /// bundle's own `defaults:` go on top. Empty means the built-in values.
    pub defaults: Defaults,
    /// Variables for `{{ ... }}` templates (every layer but the bundle's own, which is added
    /// per bundle).
    pub vars: Vars,
    /// The selected profile, reported in `RunStarted`.
    pub profile: Option<String>,
    /// Secret values for templates while applying, filled by the prepare stage. `None`: no
    /// provider, so a step using `secret()` fails when applied (planning never needs one).
    pub secrets: Option<Arc<dyn SecretLookup>>,
    /// Planning with these values (`dot status`: what could be resolved without asking):
    /// `secret()` renders a value the lookup has, and a placeholder for any other, so actions
    /// can compare what they would write. `None`: placeholders only (`dot plan`, `apply`).
    pub plan_secrets: Option<Arc<dyn SecretLookup>>,
    /// `dot.yml`'s package name map (name -> manager -> name), for `package: [names]`.
    pub packages: PackageMap,
    /// The environment `dot` started with, and what the `path:` / `env:` steps of the last real
    /// run added to it (see [`crate::runenv`]).
    pub run_env: RunEnvCell,
    /// `dot`'s state directory ([`paths::state_dir`]); derived from `home` unless set.
    pub state_dir: PathBuf,
    /// Checks `sudo -n` again right before each action that needs it (real runs). `None`:
    /// not checked (the action's own `sudo -n` fails clearly when it does not work).
    pub sudo: Option<Arc<dyn SudoCheck>>,
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("facts", &self.facts)
            .field("home", &self.home)
            .field("defaults", &self.defaults)
            .field("vars", &self.vars)
            .field("profile", &self.profile)
            .field("secrets", &self.secrets.as_ref().map(|_| "..."))
            .field("plan_secrets", &self.plan_secrets.as_ref().map(|_| "..."))
            .field("packages", &self.packages)
            .field("run_env", &self.run_env)
            .field("state_dir", &self.state_dir)
            .field("sudo", &self.sudo.as_ref().map(|_| "..."))
            .finish()
    }
}

impl Context {
    /// A context with the fact variables (facts and `home`) and no inherited defaults.
    pub fn new(facts: Facts, home: PathBuf) -> Self {
        let vars = Vars::from_facts(&facts, &home);
        Self {
            facts,
            state_dir: paths::state_dir(&home, |_| None),
            home,
            defaults: Defaults::default(),
            vars,
            profile: None,
            secrets: None,
            plan_secrets: None,
            packages: PackageMap::new(),
            run_env: RunEnvCell::default(),
            sudo: None,
        }
    }
}

/// Package name -> manager (`apt`, `brew`, ...) -> the name that manager uses.
pub type PackageMap = BTreeMap<String, BTreeMap<String, String>>;

/// What one bundle's steps see: the run's variables plus the bundle's `vars:`, and the run's
/// defaults under the bundle's `defaults:`.
#[derive(Debug, Clone)]
pub struct BundleEnv {
    pub vars: Vars,
    pub defaults: Defaults,
}

impl BundleEnv {
    pub fn new(ctx: &Context, bundle: &Bundle) -> Self {
        Self {
            vars: ctx.vars.with_bundle(bundle),
            defaults: bundle.manifest.defaults.or(&ctx.defaults),
        }
    }
}

/// Logical secret names that planning found the run will need, and how each is used. Filled
/// through [`PlanCtx::render`] (`secret()` in a template) and [`PlanCtx::require_secret`];
/// resolved in the prepare stage, never while planning.
#[derive(Debug, Default)]
pub struct SecretRefs(Mutex<BTreeMap<String, SecretUse>>);

impl SecretRefs {
    /// A name used through `secret()` in a template.
    pub fn add(&self, name: &str) {
        self.add_use(name, SecretUse::TEXT);
    }

    pub fn add_use(&self, name: &str, how: SecretUse) {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let entry = m.entry(name.to_string()).or_default();
        *entry = entry.merge(how);
    }

    pub fn names(&self) -> Vec<String> {
        self.uses().into_keys().collect()
    }

    pub fn uses(&self) -> BTreeMap<String, SecretUse> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// A command line rendered by [`PlanCtx::render_command`].
#[derive(Debug, Clone)]
pub struct RenderedCommand {
    /// What the interpreter runs. While applying, every `secret()` in it is a reference to an
    /// environment variable, never the value.
    pub text: String,
    /// For display: each secret shown as `<secret:name>`.
    pub display: String,
    /// The environment variables the text refers to, with the secret values; set them on the
    /// child only.
    pub secret_env: Vec<(String, SecretBytes)>,
    /// The text holds a secret placeholder (planning, or a secret that was not resolved), so it
    /// must not run.
    pub placeholder: bool,
}

/// What a planner sees for one step.
pub struct PlanCtx<'a> {
    pub facts: &'a Facts,
    pub home: &'a Path,
    /// `dot`'s state directory ([`Context::state_dir`]).
    pub state_dir: &'a Path,
    pub bundle: &'a Bundle,
    pub step: &'a Step,
    /// Inherited option defaults; entries resolve their unset options against these.
    pub defaults: &'a Defaults,
    pub vars: &'a Vars,
    /// `dot.yml`'s package name map.
    pub packages: &'a PackageMap,
    secrets: &'a SecretRefs,
    mode: SecretMode,
    run_env: RunEnvCell,
    /// Deferred prompt variables (see [`Vars::defer_prompts`]) the step's texts used.
    asked: Mutex<BTreeSet<String>>,
}

/// A whole text rendered by [`PlanCtx::render_text`].
#[derive(Debug, Clone)]
pub struct RenderedText {
    pub text: String,
    /// The secret names the text used. Non-empty means the result holds secret values (or,
    /// while planning, placeholders for them).
    pub secrets: Vec<String>,
    /// Planning: the text holds secret placeholders, so it cannot be compared or written yet.
    pub placeholder: bool,
}

impl<'a> PlanCtx<'a> {
    /// A planning context using the run's variables and defaults as they are (without the
    /// bundle's own layer); `secret()` renders placeholders. The engine uses
    /// [`PlanCtx::for_bundle`].
    pub fn new(
        ctx: &'a Context,
        bundle: &'a Bundle,
        step: &'a Step,
        secrets: &'a SecretRefs,
    ) -> Self {
        Self {
            facts: &ctx.facts,
            home: &ctx.home,
            state_dir: &ctx.state_dir,
            bundle,
            step,
            defaults: &ctx.defaults,
            vars: &ctx.vars,
            packages: &ctx.packages,
            secrets,
            mode: SecretMode::Placeholder,
            run_env: ctx.run_env.clone(),
            asked: Mutex::default(),
        }
    }

    /// A planning context for a step of `bundle`. `applying`: the step is about to run, so
    /// `secret()` returns resolved values ([`Context::secrets`]) instead of placeholders.
    pub fn for_bundle(
        ctx: &'a Context,
        env: &'a BundleEnv,
        bundle: &'a Bundle,
        step: &'a Step,
        secrets: &'a SecretRefs,
        applying: bool,
    ) -> Self {
        let mode = if applying {
            match &ctx.secrets {
                Some(s) => SecretMode::Resolve(s.clone()),
                None => SecretMode::Resolve(Arc::new(crate::secret::SecretCache::default())),
            }
        } else {
            match &ctx.plan_secrets {
                Some(s) => SecretMode::Available(s.clone()),
                None => SecretMode::Placeholder,
            }
        };
        Self {
            facts: &ctx.facts,
            home: &ctx.home,
            state_dir: &ctx.state_dir,
            bundle,
            step,
            defaults: &env.defaults,
            vars: &env.vars,
            packages: &ctx.packages,
            secrets,
            mode,
            run_env: ctx.run_env.clone(),
            asked: Mutex::default(),
        }
    }

    /// The same context recording `path:` / `env:` entries into `run_env` (one per engine
    /// pass) instead of [`Context::run_env`].
    pub fn with_run_env(mut self, run_env: RunEnvCell) -> Self {
        self.run_env = run_env;
        self
    }

    /// The run environment: what earlier `path:` / `env:` steps added, over the environment
    /// `dot` started with. Commands a step runs should get [`RunEnvCell::child_env`].
    pub fn run_env(&self) -> &RunEnvCell {
        &self.run_env
    }

    /// The deferred prompt variables (see [`Vars::defer_prompts`]) this step's texts used so
    /// far. Non-empty: what the step would do is unknown until they are answered.
    pub fn asked(&self) -> Vec<String> {
        self.asked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    fn note_asked(&self, text: &str) {
        let names = self.vars.asked_in(text);
        if !names.is_empty() {
            self.asked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(names);
        }
    }

    /// A config error located at this step (file, line and column when known).
    pub fn config_error(&self, message: impl AsRef<str>) -> anyhow::Error {
        ConfigError::at_step(self.bundle, self.step, message).into()
    }

    /// Renders a config string. An undefined variable is a config error at this step. Secret
    /// names used through `secret()` are recorded for the prepare stage.
    pub fn render(&self, text: &str) -> Result<String> {
        self.render_tracked(text).map(|(s, _)| s)
    }

    /// [`PlanCtx::render`], also saying whether the result holds a secret placeholder (so an
    /// action must not run it or compare with it until secrets are resolved).
    pub fn render_tracked(&self, text: &str) -> Result<(String, bool)> {
        self.note_asked(text);
        let r = self
            .vars
            .render_with(text, &self.mode)
            .map_err(|e| self.config_error(format!("{e} (in `{}`)", first_line(text))))?;
        for name in &r.secrets {
            self.secrets.add(name);
        }
        let placeholder = self.holds_placeholders(&r.secrets);
        Ok((r.text, placeholder))
    }

    /// Whether a text that used `secrets` was rendered with a placeholder for any of them.
    fn holds_placeholders(&self, secrets: &[String]) -> bool {
        match &self.mode {
            SecretMode::Placeholder => !secrets.is_empty(),
            SecretMode::Available(lookup) => secrets.iter().any(|n| lookup.lookup(n).is_none()),
            _ => false,
        }
    }

    /// Renders a whole text (a `template:` source, a `block:` body), reporting which secrets it
    /// used so the caller can protect the result. Errors name `origin` (e.g. the source file).
    pub fn render_text(&self, text: &str, origin: &str) -> Result<RenderedText> {
        self.note_asked(text);
        let r = self
            .vars
            .render_with(text, &self.mode)
            .map_err(|e| self.config_error(format!("{origin}: {e}")))?;
        for name in &r.secrets {
            self.secrets.add(name);
        }
        let placeholder = self.holds_placeholders(&r.secrets);
        Ok(RenderedText {
            text: r.text,
            secrets: r.secrets,
            placeholder,
        })
    }

    /// Renders a command line for an interpreter. While applying, each `secret(name)` renders as
    /// `reference(var)` (e.g. `${DOT_SECRET_name}` for `sh`, `%DOT_SECRET_name%` for `cmd`) and
    /// the value comes back in [`RenderedCommand::secret_env`] for the child's environment: a
    /// process's argv is readable by every local user (`ps`, `/proc/<pid>/cmdline`), its
    /// environment only by its owner. A secret whose reference does not reach the output
    /// unchanged (transformed by a filter) is an error rather than a silently wrong command.
    pub fn render_command(
        &self,
        text: &str,
        reference: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Result<RenderedCommand> {
        let lookup = match &self.mode {
            // A command whose secrets are all there is rendered with their values in the
            // child's environment. One that uses a secret nobody resolved (it was not needed
            // when the run was planned: a `shell` whose `unless` succeeded) holds placeholders:
            // the action may still find it has nothing to do, and must not run it otherwise.
            // Planning with what is available (`dot status`) does the same, so an `unless`
            // check can run.
            SecretMode::Resolve(lookup) | SecretMode::Available(lookup) => {
                self.note_asked(text);
                let r = self
                    .vars
                    .render_with(text, &SecretMode::Placeholder)
                    .map_err(|e| self.config_error(format!("{e} (in `{}`)", first_line(text))))?;
                for name in &r.secrets {
                    self.secrets.add(name);
                }
                if r.secrets.is_empty() || r.secrets.iter().any(|n| lookup.lookup(n).is_none()) {
                    return Ok(RenderedCommand {
                        display: r.text.clone(),
                        placeholder: !r.secrets.is_empty(),
                        text: r.text,
                        secret_env: Vec::new(),
                    });
                }
                lookup
            }
            _ => {
                let (text, placeholder) = self.render_tracked(text)?;
                return Ok(RenderedCommand {
                    display: text.clone(),
                    text,
                    secret_env: Vec::new(),
                    placeholder,
                });
            }
        };
        self.note_asked(text);
        let ind = Arc::new(Indirect::new(lookup.clone(), reference));
        let r = self
            .vars
            .render_with(text, &SecretMode::Indirect(ind.clone()))
            .map_err(|e| self.config_error(format!("{e} (in `{}`)", first_line(text))))?;
        for name in &r.secrets {
            self.secrets.add(name);
        }
        let mut display = r.text.clone();
        let mut secret_env = Vec::new();
        for (name, var) in ind.bound() {
            let reference = ind.reference(&var);
            if !r.text.contains(&reference) {
                return Err(self.config_error(format!(
                    "secret(\"{name}\") in a command must be used as is: its value is passed \
                     to the command in ${var}, so a filter or expression cannot change it (in \
                     `{}`)",
                    first_line(text)
                )));
            }
            display = display.replace(&reference, &secret_placeholder(&name));
            let value = ind
                .lookup()
                .lookup(&name)
                .ok_or_else(|| self.config_error(format!("secret `{name}` was not resolved")))?;
            secret_env.push((var, value));
        }
        Ok(RenderedCommand {
            text: r.text,
            display,
            secret_env,
            placeholder: false,
        })
    }

    /// A secret's bytes for a directive that uses it as a file (`secret_file:`). Records the
    /// use; `Ok(None)` while planning (nothing is resolved then).
    pub fn secret_file(&self, name: &str) -> Result<Option<SecretBytes>> {
        self.require_secret(name, SecretUse::FILE);
        match &self.mode {
            SecretMode::Resolve(lookup) => lookup
                .lookup(name)
                .map(Some)
                .ok_or_else(|| self.config_error(format!("secret `{name}` was not resolved"))),
            SecretMode::Available(lookup) => Ok(lookup.lookup(name)),
            _ => Ok(None),
        }
    }

    /// The variables a shell step receives as environment (see [`Vars::exported`]).
    pub fn exported_env(&self, allow_sensitive: &[String]) -> Result<Vec<(String, String)>> {
        self.vars
            .exported(allow_sensitive)
            .map_err(|e| self.config_error(e))
    }

    /// A destination path: rendered, `~` expanded, must be absolute.
    pub fn target(&self, path: &str) -> Result<PathBuf> {
        let rendered = self.render(path)?;
        paths::expand_target(&rendered, self.home).map_err(|e| self.config_error(format!("{e:#}")))
    }

    /// A source-side path: rendered, `~` expanded, relative paths resolved from the bundle
    /// directory.
    pub fn source(&self, path: &str) -> Result<PathBuf> {
        let rendered = self.render(path)?;
        Ok(paths::resolve_from(&rendered, &self.bundle.dir, self.home))
    }

    /// A file mode, rendered and validated if it is a template.
    pub fn mode(&self, mode: &ModeSpec) -> Result<Mode> {
        match mode {
            ModeSpec::Literal(m) => Ok(*m),
            ModeSpec::Template(t) => {
                let rendered = self.render(t)?;
                Mode::parse(&rendered).map_err(|e| self.config_error(e))
            }
        }
    }

    /// Records that the run needs this secret, and how. Planning never resolves secrets: an
    /// action whose status depends on one reports [`Status::Unknown`] until the prepare stage
    /// has run.
    pub fn require_secret(&self, name: &str, how: SecretUse) {
        self.secrets.add_use(name, how);
    }

    /// Shortens a path for display: `~/...` under home.
    pub fn show(&self, path: &Path) -> String {
        paths::display(path, self.home)
    }

    /// Shortens a source-side path for display: relative when inside the bundle directory.
    pub fn show_source(&self, path: &Path) -> String {
        match path.strip_prefix(&self.bundle.dir) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
            _ => self.show(path),
        }
    }
}

fn first_line(s: &str) -> String {
    let t = s.trim_end();
    match t.split_once('\n') {
        Some((first, _)) => format!("{first} ..."),
        None => t.to_string(),
    }
}

/// Turns a directive into actions. Must be pure: it may inspect the machine but never change it.
/// `Sync` so bundles can be planned from several threads (`--jobs N`).
pub trait Planner: Sync {
    fn plan(&self, directive: &Directive, ctx: &PlanCtx<'_>) -> Result<Vec<Box<dyn Action>>>;

    /// Runs a bundle's `check:` command (already rendered) in the bundle directory with `env`
    /// added; `true` when it exits 0. Must be read-only.
    fn check(&self, command: &str, bundle: &Bundle, env: &[(String, String)]) -> Result<bool> {
        let _ = (command, bundle, env);
        anyhow::bail!("this planner cannot run bundle checks")
    }
}

/// The up-front plan: a preview of the run. `Sync`, so worker threads can share it.
pub struct Plan {
    pub bundles: Vec<PlannedBundle>,
    /// Logical secret names the plan's steps use, and how.
    pub secrets: BTreeMap<String, SecretUse>,
    /// The part of [`Plan::secrets`] used by steps that have work (not satisfied): what the
    /// prepare stage resolves. A step already satisfied (a `shell` whose `unless` succeeds, a
    /// template already in place) does not need its secrets.
    pub needed: BTreeMap<String, SecretUse>,
}

impl Plan {
    /// The secret names the plan uses, sorted.
    pub fn secret_names(&self) -> Vec<String> {
        self.secrets.keys().cloned().collect()
    }

    /// The secret names of [`Plan::needed`], sorted.
    pub fn needed_names(&self) -> Vec<String> {
        self.needed.keys().cloned().collect()
    }
}

pub struct PlannedBundle {
    pub name: String,
    pub file: PathBuf,
    pub skip: Option<String>,
    pub steps: Vec<PlannedStep>,
    /// The secrets this bundle's steps use.
    pub secrets: BTreeMap<String, SecretUse>,
    /// Those used by its steps that have work (see [`Plan::needed`]).
    pub needed: BTreeMap<String, SecretUse>,
}

pub struct PlannedStep {
    pub id: StepId,
    pub kind: &'static str,
    pub label: String,
    pub state: PlannedState,
    /// The secret names the step uses.
    pub secrets: Vec<String>,
}

impl PlannedStep {
    /// Whether applying the step may change something or fail: an action that is not
    /// satisfied.
    pub fn has_work(&self) -> bool {
        matches!(&self.state, PlannedState::Ready(actions)
            if actions.iter().any(|a| a.status != Status::Satisfied))
    }
}

pub enum PlannedState {
    Skipped(String),
    Error(String),
    Ready(Vec<PlannedAction>),
}

pub struct PlannedAction {
    pub action: Box<dyn Action>,
    pub description: String,
    /// Status at plan time.
    pub status: Status,
}

impl PlannedBundle {
    fn has_errors(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s.state, PlannedState::Error(_)))
    }
}

impl Plan {
    pub fn has_errors(&self) -> bool {
        self.bundles.iter().any(PlannedBundle::has_errors)
    }

    /// Whether a bundle that will be applied has an action that would fail
    /// ([`Status::Conflict`]).
    pub fn has_conflicts(&self) -> bool {
        self.bundles
            .iter()
            .filter(|b| b.skip.is_none())
            .flat_map(|b| &b.steps)
            .any(|s| match &s.state {
                PlannedState::Ready(actions) => {
                    actions.iter().any(|a| a.status == Status::Conflict)
                }
                _ => false,
            })
    }

    /// The bundles that will be applied and have an action that is not satisfied and needs
    /// root ([`Action::needs_sudo`]), in plan order: the run asks for sudo once, up front, when
    /// this is not empty.
    pub fn sudo_bundles(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (b, _) in self.sudo_step_list() {
            if out.last() != Some(&b.name) {
                out.push(b.name.clone());
            }
        }
        out
    }

    /// The steps behind [`Plan::sudo_bundles`], as `bundle: step label`, in plan order: what
    /// sudo is asked for.
    pub fn sudo_steps(&self) -> Vec<String> {
        self.sudo_step_list()
            .into_iter()
            .map(|(b, s)| format!("{}: {}", b.name, s.label))
            .collect()
    }

    /// Like [`Plan::sudo_steps`], for the steps with an action that asks for the sudo password
    /// by itself ([`Action::asks_for_sudo`]): they ask on the terminal while applying.
    pub fn tty_sudo_steps(&self) -> Vec<String> {
        self.steps_with(|a| a.asks_for_sudo())
            .into_iter()
            .map(|(b, s)| format!("{}: {}", b.name, s.label))
            .collect()
    }

    /// The steps that need `dot`'s own sudo (asked for once, up front).
    fn sudo_step_list(&self) -> Vec<(&PlannedBundle, &PlannedStep)> {
        self.steps_with(|a| a.needs_sudo() && !a.asks_for_sudo())
    }

    /// The steps of the bundles that will be applied with an action that has work and
    /// matches `pick`.
    fn steps_with(
        &self,
        pick: impl Fn(&dyn Action) -> bool,
    ) -> Vec<(&PlannedBundle, &PlannedStep)> {
        self.bundles
            .iter()
            .filter(|b| b.skip.is_none() && !b.has_errors())
            .flat_map(|b| b.steps.iter().map(move |s| (b, s)))
            .filter(|(_, s)| match &s.state {
                PlannedState::Ready(actions) => actions
                    .iter()
                    .any(|a| a.status.has_work() && pick(a.action.as_ref())),
                _ => false,
            })
            .collect()
    }

    pub fn view(&self) -> PlanView {
        PlanView {
            secrets: self.secret_names(),
            sudo: self.sudo_bundles(),
            sudo_steps: self.sudo_steps(),
            tty_sudo_steps: self.tty_sudo_steps(),
            bundles: self
                .bundles
                .iter()
                .map(|b| BundleView {
                    name: b.name.clone(),
                    file: b.file.clone(),
                    skip: b.skip.clone(),
                    steps: b
                        .steps
                        .iter()
                        .map(|s| StepView {
                            id: s.id,
                            kind: s.kind,
                            label: s.label.clone(),
                            state: match &s.state {
                                PlannedState::Skipped(r) => StepViewState::Skipped(r.clone()),
                                PlannedState::Error(e) => StepViewState::Error(e.clone()),
                                PlannedState::Ready(actions) => StepViewState::Ready(
                                    actions
                                        .iter()
                                        .map(|a| ActionView {
                                            description: a.description.clone(),
                                            status: a.status,
                                        })
                                        .collect(),
                                ),
                            },
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// Renders and evaluates a `when:`. The outer `Err` is a config error (undefined variable, or a
/// rendered value that is not a known value of its fact); the inner one is why it did not match.
fn eval_when(
    when: &When,
    ctx: &Context,
    env: &BundleEnv,
    bundle: &Bundle,
    step: Option<&Step>,
) -> Result<Result<(), String>, String> {
    let rendered = when.render(&env.vars).map_err(|e| {
        let msg = format!("when: {e}");
        match step {
            Some(s) => ConfigError::at_step(bundle, s, msg).to_string(),
            None => ConfigError::new(&bundle.file, msg).to_string(),
        }
    })?;
    Ok(rendered.eval(&ctx.facts))
}

/// Plans every step of every bundle. Never changes the machine; problems become
/// [`PlannedState::Error`] entries rather than aborting, so all of them can be reported at once.
pub fn plan(bundles: &[Bundle], planner: &dyn Planner, ctx: &Context) -> Plan {
    plan_with(bundles, planner, ctx, Options::default())
}

/// [`plan`], honouring [`Options::force`] (bundle `check:` commands are not run).
pub fn plan_with(bundles: &[Bundle], planner: &dyn Planner, ctx: &Context, opts: Options) -> Plan {
    plan_scheduled(bundles, &Schedule::chain(bundles.len()), planner, ctx, opts)
}

/// [`plan_with`] for bundles run by `schedule`: each bundle's steps see the `path:` / `env:`
/// entries of the bundles it requires or runs after (see [`RunEnvCell::visible_to`]).
pub fn plan_scheduled(
    bundles: &[Bundle],
    schedule: &Schedule,
    planner: &dyn Planner,
    ctx: &Context,
    opts: Options,
) -> Plan {
    let mut all: BTreeMap<String, SecretUse> = BTreeMap::new();
    let mut all_needed: BTreeMap<String, SecretUse> = BTreeMap::new();
    let merge = |into: &mut BTreeMap<String, SecretUse>, from: &BTreeMap<String, SecretUse>| {
        for (n, u) in from {
            let e = into.entry(n.clone()).or_default();
            *e = e.merge(*u);
        }
    };
    let mut out = Vec::new();
    let fresh = ctx.run_env.fresh();
    for (bi, bundle) in bundles.iter().enumerate() {
        let run_env = schedule.run_env(bundles, bi, &fresh);
        let _entered = run_env.enter();
        let mut secrets: BTreeMap<String, SecretUse> = BTreeMap::new();
        let mut needed: BTreeMap<String, SecretUse> = BTreeMap::new();
        let env = BundleEnv::new(ctx, bundle);
        let (mut skip, mut bundle_error) = match bundle.manifest.when.as_ref() {
            None => (None, None),
            Some(w) => match eval_when(w, ctx, &env, bundle, None) {
                Ok(Ok(())) => (None, None),
                Ok(Err(reason)) => (Some(format!("when: {reason}")), None),
                Err(e) => (None, Some(e)),
            },
        };
        if skip.is_none() && bundle_error.is_none() && !opts.force {
            match run_check(bundle, &env, planner) {
                Ok(Some(reason)) => skip = Some(reason),
                Ok(None) => {}
                Err(e) => bundle_error = Some(e),
            }
        }
        let pending = PendingHandlers::in_dir(&ctx.state_dir).get(&bundle.name);
        let mut steps: Vec<PlannedStep> = Vec::new();
        for (si, step) in bundle.all_steps().enumerate() {
            let handler = bundle.handler_name(step);
            let refs = SecretRefs::default();
            let notified = handler.map(|h| {
                if pending.contains(h) {
                    Notified::Yes
                } else {
                    may_notify(&bundle.steps, &steps, h)
                }
            });
            let state = if skip.is_some() {
                PlannedState::Skipped("bundle skipped".into())
            } else if let Some(e) = &bundle_error {
                PlannedState::Error(e.clone())
            } else if notified == Some(Notified::No) {
                PlannedState::Skipped(NOT_NOTIFIED.into())
            } else {
                let state = preview_step(bundle, &env, step, planner, ctx, &refs, &run_env);
                match notified {
                    // Runs only if a notifying step, whose outcome is not known yet, changes
                    // something: not known either.
                    Some(Notified::Maybe) => maybe_notified(state),
                    _ => state,
                }
            };
            let uses = refs.uses();
            merge(&mut secrets, &uses);
            let planned = PlannedStep {
                id: StepId {
                    bundle: bi,
                    step: si,
                },
                kind: step.directive.kind(),
                label: bundle.step_label(step),
                state,
                secrets: uses.keys().cloned().collect(),
            };
            if planned.has_work() {
                merge(&mut needed, &uses);
            }
            steps.push(planned);
        }
        merge(&mut all, &secrets);
        merge(&mut all_needed, &needed);
        out.push(PlannedBundle {
            name: bundle.name.clone(),
            file: bundle.file.clone(),
            skip,
            steps,
            secrets,
            needed,
        });
    }
    Plan {
        bundles: out,
        secrets: all,
        needed: all_needed,
    }
}

/// Whether a handler is planned to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Notified {
    /// A notifying step has work to do (or an earlier run left the handler due).
    Yes,
    /// Only notifying steps whose status is unknown (secrets, an input made earlier in the
    /// run): it runs if one of them changes something.
    Maybe,
    No,
}

/// Whether a step that notifies handler `name` may change something, going by its plan.
fn may_notify(configs: &[Step], planned: &[PlannedStep], name: &str) -> Notified {
    let mut out = Notified::No;
    for (c, p) in configs.iter().zip(planned) {
        if !c.notify.iter().any(|n| n == name) {
            continue;
        }
        if let PlannedState::Ready(actions) = &p.state {
            if actions.iter().any(|a| a.status == Status::Pending) {
                return Notified::Yes;
            }
            if actions.iter().any(|a| a.status == Status::Unknown) {
                out = Notified::Maybe;
            }
        }
    }
    out
}

/// A handler's plan when only unknown steps may notify it: what it would do is pending on
/// them, so its pending actions are unknown.
fn maybe_notified(state: PlannedState) -> PlannedState {
    match state {
        PlannedState::Ready(actions) => PlannedState::Ready(
            actions
                .into_iter()
                .map(|mut a| {
                    if a.status == Status::Pending {
                        a.status = Status::Unknown;
                        a.description
                            .push_str(" (if a notifying step, unknown yet, changes something)");
                    }
                    a
                })
                .collect(),
        ),
        other => other,
    }
}

/// Runs the bundle's `check:` command, if any. `Ok(Some(reason))`: satisfied, skip the bundle.
fn run_check(
    bundle: &Bundle,
    env: &BundleEnv,
    planner: &dyn Planner,
) -> Result<Option<String>, String> {
    let Some(cmd) = &bundle.manifest.check else {
        return Ok(None);
    };
    let located = |msg: String| ConfigError::new(&bundle.file, format!("check: {msg}")).to_string();
    let rendered = env.vars.render(cmd).map_err(located)?;
    let exported = env.vars.exported(&[]).map_err(located)?;
    match planner.check(&rendered, bundle, &exported) {
        Ok(true) => Ok(Some(format!(
            "already satisfied: check `{}` succeeded (--force to apply anyway)",
            first_line(&rendered)
        ))),
        Ok(false) => Ok(None),
        Err(e) => Err(located(format!("{e:#}"))),
    }
}

enum Prepared {
    Skip(String),
    /// The actions, and the deferred prompt variables (see [`Vars::defer_prompts`]) they use.
    Actions(Vec<Box<dyn Action>>, Vec<String>),
}

/// Evaluates the step's `when:` and asks the planner for its actions.
#[allow(clippy::too_many_arguments)]
fn prepare_step(
    bundle: &Bundle,
    env: &BundleEnv,
    step: &Step,
    planner: &dyn Planner,
    ctx: &Context,
    secrets: &SecretRefs,
    applying: bool,
    run_env: &RunEnvCell,
) -> Result<Prepared, String> {
    if let Some(w) = &step.when
        && let Err(reason) = eval_when(w, ctx, env, bundle, Some(step))?
    {
        return Ok(Prepared::Skip(format!("when: {reason}")));
    }
    if let Some(expr) = &step.condition {
        let holds = env.vars.eval_bool(expr).map_err(|e| {
            ConfigError::at_step(bundle, step, format!("if: {e} (in `{expr}`)")).to_string()
        })?;
        if !holds {
            return Ok(Prepared::Skip(format!("if: `{}` is false", expr.trim())));
        }
    }
    let pctx = PlanCtx::for_bundle(ctx, env, bundle, step, secrets, applying)
        .with_run_env(run_env.clone());
    let actions = planner
        .plan(&step.directive, &pctx)
        .map_err(|e| format!("{e:#}"))?;
    Ok(Prepared::Actions(actions, pctx.asked()))
}

/// Asks a step's `only_if` / `unless` (see [`Guard`]): `Ok(Some(reason))` when the step is
/// to be skipped. Run like a bundle `check:`, with the variables and what earlier `path:` /
/// `env:` steps set.
fn guard_skips(
    bundle: &Bundle,
    env: &BundleEnv,
    step: &Step,
    planner: &dyn Planner,
    run_env: &RunEnvCell,
) -> Result<Option<String>, String> {
    let Some(guard) = &step.guard else {
        return Ok(None);
    };
    let (key, cmd) = match guard {
        Guard::OnlyIf(c) => ("only_if", c),
        Guard::Unless(c) => ("unless", c),
    };
    let located = |msg: String| ConfigError::at_step(bundle, step, format!("{key}: {msg}"));
    let rendered = env.vars.render(cmd).map_err(|e| located(e).to_string())?;
    let mut vars = env.vars.exported(&[]).map_err(|e| located(e).to_string())?;
    for (k, v) in run_env.child_env() {
        vars.retain(|(n, _)| *n != k);
        vars.push((k, v));
    }
    let ok = planner
        .check(&rendered, bundle, &vars)
        .map_err(|e| located(format!("{e:#}")).to_string())?;
    let shown = first_line(&rendered);
    Ok(match (guard, ok) {
        (Guard::OnlyIf(_), false) => Some(format!("only_if: `{shown}` failed")),
        (Guard::Unless(_), true) => Some(format!("unless: `{shown}` succeeded")),
        _ => None,
    })
}

fn preview_step(
    bundle: &Bundle,
    env: &BundleEnv,
    step: &Step,
    planner: &dyn Planner,
    ctx: &Context,
    secrets: &SecretRefs,
    run_env: &RunEnvCell,
) -> PlannedState {
    let state = preview_actions(bundle, env, step, planner, ctx, secrets, run_env);
    // The guard is asked only when the step has something to do (or cannot be planned).
    let asks = match &state {
        PlannedState::Error(_) => true,
        PlannedState::Ready(actions) => actions.iter().any(|a| a.status != Status::Satisfied),
        PlannedState::Skipped(_) => false,
    };
    if !asks {
        return state;
    }
    match guard_skips(bundle, env, step, planner, run_env) {
        Ok(Some(reason)) => PlannedState::Skipped(reason),
        Ok(None) => state,
        Err(e) => PlannedState::Error(e),
    }
}

fn preview_actions(
    bundle: &Bundle,
    env: &BundleEnv,
    step: &Step,
    planner: &dyn Planner,
    ctx: &Context,
    secrets: &SecretRefs,
    run_env: &RunEnvCell,
) -> PlannedState {
    let (actions, asked) =
        match prepare_step(bundle, env, step, planner, ctx, secrets, false, run_env) {
            Err(e) => return PlannedState::Error(e),
            Ok(Prepared::Skip(reason)) => return PlannedState::Skipped(reason),
            Ok(Prepared::Actions(a, asked)) => (a, asked),
        };
    let mut planned = Vec::with_capacity(actions.len());
    for action in actions {
        let (status, description) = if asked.is_empty() {
            match action.status() {
                Ok(s) => (s, action.describe()),
                Err(e) => return PlannedState::Error(format!("{}: {e:#}", action.describe())),
            }
        } else {
            // Planned with placeholders for the answers: what it would do is not known yet.
            (
                Status::Unknown,
                format!(
                    "{} (uses {}, asked when applying)",
                    action.describe(),
                    asked.join(", ")
                ),
            )
        };
        planned.push(PlannedAction {
            description,
            action,
            status,
        });
    }
    PlannedState::Ready(planned)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Evaluate and report, change nothing.
    pub dry_run: bool,
    /// Apply bundles even when their `check:` command says they are already satisfied.
    pub force: bool,
    /// How many bundles may run at the same time (`--jobs N`); 0 and 1 mean one at a time,
    /// on the calling thread.
    pub jobs: usize,
}

/// The order bundles may run in. For each bundle (by plan position): the earlier bundles it
/// hard-requires (it is skipped when one of them fails) and those it only runs after (it waits
/// for them without depending on their success). Both kinds decide which `path:` / `env:`
/// entries its commands see. An edge to the bundle itself or a later position is ignored, so
/// a schedule can never deadlock.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schedule {
    pub requires: Vec<Vec<usize>>,
    pub after: Vec<Vec<usize>>,
}

impl Schedule {
    /// Each bundle after the one before it: the order given, one at a time whatever
    /// [`Options::jobs`] says (`dot apply -c a.yml b.yml`).
    pub fn chain(n: usize) -> Self {
        Self {
            requires: vec![Vec::new(); n],
            after: (0..n)
                .map(|i| i.checked_sub(1).into_iter().collect())
                .collect(),
        }
    }

    /// [`Schedule::chain`] with hard dependencies `deps` (as [`run_graph`] takes them).
    pub fn sequential(deps: &[Vec<usize>], n: usize) -> Self {
        let mut s = Self::chain(n);
        for (r, d) in s.requires.iter_mut().zip(deps) {
            r.clone_from(d);
        }
        s
    }

    /// The bundles `i` hard-requires.
    pub fn requires_of(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        self.requires
            .get(i)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |&d| d < i)
    }

    /// The bundles that must have finished before `i` starts.
    pub fn predecessors(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        let after = self.after.get(i).into_iter().flatten().copied();
        self.requires_of(i).chain(after.filter(move |&d| d < i))
    }

    /// `i` and every bundle it requires or runs after, transitively.
    pub fn ancestors(&self, i: usize) -> BTreeSet<usize> {
        let mut seen = BTreeSet::from([i]);
        let mut todo = vec![i];
        while let Some(b) = todo.pop() {
            for p in self.predecessors(b) {
                if seen.insert(p) {
                    todo.push(p);
                }
            }
        }
        seen
    }

    /// `run_env` as bundle `i` sees it: the entries of its [`Schedule::ancestors`].
    fn run_env(&self, bundles: &[Bundle], i: usize, run_env: &RunEnvCell) -> RunEnvCell {
        let names = self
            .ancestors(i)
            .into_iter()
            .filter_map(|b| bundles.get(b).map(|b| b.name.clone()))
            .collect();
        run_env.visible_to(names)
    }
}

/// The stage between planning and applying: unlock secret providers (the console is paused with
/// `NeedsTty { step: None, .. }` around any prompt) and resolve [`Plan::secrets`]. Not run for dry
/// runs.
pub trait Prepare {
    /// Before any step is applied. An error stops the run.
    fn prepare(&mut self, plan: &Plan, sink: &EventSink) -> Result<()>;

    /// Right before bundle `name` is applied, with the secrets its steps use: resolve those the
    /// up-front pass had to defer (their provider only becomes available once an earlier bundle
    /// has installed it). An error fails this bundle (and skips its dependents); the rest of the
    /// run continues.
    fn before_bundle(
        &mut self,
        name: &str,
        secrets: &BTreeMap<String, SecretUse>,
        sink: &EventSink,
    ) -> Result<()> {
        let _ = (name, secrets, sink);
        Ok(())
    }
}

/// Several prepare stages, run in order; the first error stops the run.
pub struct PrepareChain<'a>(pub Vec<&'a mut dyn Prepare>);

impl Prepare for PrepareChain<'_> {
    fn prepare(&mut self, plan: &Plan, sink: &EventSink) -> Result<()> {
        for p in &mut self.0 {
            p.prepare(plan, sink)?;
        }
        Ok(())
    }

    fn before_bundle(
        &mut self,
        name: &str,
        secrets: &BTreeMap<String, SecretUse>,
        sink: &EventSink,
    ) -> Result<()> {
        for p in &mut self.0 {
            p.before_bundle(name, secrets, sink)?;
        }
        Ok(())
    }
}

/// Nothing to prepare: no secrets to resolve and no sudo to prime ([`run`] and tests).
pub struct NoPrepare;

impl Prepare for NoPrepare {
    fn prepare(&mut self, _plan: &Plan, _sink: &EventSink) -> Result<()> {
        Ok(())
    }
}

/// Plans and applies `bundles`, emitting the full event stream from `RunStarted` to
/// `RunFinished`.
pub fn run(
    bundles: &[Bundle],
    planner: &dyn Planner,
    ctx: &Context,
    sink: &EventSink,
    opts: Options,
) -> Summary {
    run_with(bundles, planner, ctx, sink, opts, &mut NoPrepare)
}

/// [`run`], with a prepare stage between planning and applying.
pub fn run_with(
    bundles: &[Bundle],
    planner: &dyn Planner,
    ctx: &Context,
    sink: &EventSink,
    opts: Options,
    prepare: &mut dyn Prepare,
) -> Summary {
    run_graph(bundles, &[], planner, ctx, sink, opts, prepare)
}

/// [`run_with`] for bundles in dependency order: `deps[i]` lists the positions (all before `i`)
/// of the bundles bundle `i` hard-depends on. A bundle whose dependency failed (or was skipped
/// because of a failure) is skipped with "dependency X failed"; the others continue. An empty
/// `deps` means no dependencies.
pub fn run_graph(
    bundles: &[Bundle],
    deps: &[Vec<usize>],
    planner: &dyn Planner,
    ctx: &Context,
    sink: &EventSink,
    opts: Options,
    prepare: &mut dyn Prepare,
) -> Summary {
    let schedule = Schedule::sequential(deps, bundles.len());
    run_scheduled(bundles, &schedule, planner, ctx, sink, opts, prepare)
}

/// [`run_with`] for bundles in dependency order, run as `schedule` allows: with
/// [`Options::jobs`] above 1, bundles whose predecessors have finished run at the same time.
pub fn run_scheduled(
    bundles: &[Bundle],
    schedule: &Schedule,
    planner: &dyn Planner,
    ctx: &Context,
    sink: &EventSink,
    opts: Options,
    prepare: &mut dyn Prepare,
) -> Summary {
    let start = Instant::now();
    sink.emit(Event::RunStarted {
        facts: ctx.facts.clone(),
        dry_run: opts.dry_run,
        profile: ctx.profile.clone(),
    });
    let plan = plan_scheduled(bundles, schedule, planner, ctx, opts);
    sink.emit(Event::PlanReady(plan.view()));
    let prepared = if opts.dry_run {
        Ok(())
    } else {
        prepare.prepare(&plan, sink)
    };
    let mut summary = match prepared {
        Ok(()) => execute_prepared(&plan, bundles, schedule, planner, ctx, sink, opts, prepare),
        Err(e) => {
            let msg = format!("{e:#}");
            sink.emit(Event::RunError(msg.clone()));
            Summary {
                skipped: plan.bundles.iter().map(|b| b.steps.len()).sum(),
                run_error: Some(msg),
                ..Summary::default()
            }
        }
    };
    summary.elapsed = start.elapsed();
    sink.emit(Event::RunFinished(summary.clone()));
    sink.finish();
    summary
}

/// Applies a plan (or, with `dry_run`, reports what would change). `bundles`, `planner` and `ctx`
/// must be the ones the plan was made from: in a real run each step is planned again right before
/// it runs. A failed step fails its bundle and skips the bundle's remaining steps; other bundles
/// continue. A bundle with planning errors is not applied at all.
pub fn execute(
    plan: &Plan,
    bundles: &[Bundle],
    planner: &dyn Planner,
    ctx: &Context,
    sink: &EventSink,
    opts: Options,
) -> Summary {
    execute_graph(plan, bundles, &[], planner, ctx, sink, opts)
}

/// Why a step of a bundle skipped for a failed dependency is not run. Renderers show these at
/// default verbosity.
pub const DEPENDENCY_FAILED: &str = "dependency";

/// [`execute`] with dependencies (see [`run_graph`]).
pub fn execute_graph(
    plan: &Plan,
    bundles: &[Bundle],
    deps: &[Vec<usize>],
    planner: &dyn Planner,
    ctx: &Context,
    sink: &EventSink,
    opts: Options,
) -> Summary {
    execute_prepared(
        plan,
        bundles,
        &Schedule::sequential(deps, bundles.len()),
        planner,
        ctx,
        sink,
        opts,
        &mut NoPrepare,
    )
}

/// [`execute_graph`] as `schedule` allows, calling [`Prepare::before_bundle`] before each
/// bundle is applied.
#[allow(clippy::too_many_arguments)]
fn execute_prepared(
    plan: &Plan,
    bundles: &[Bundle],
    schedule: &Schedule,
    planner: &dyn Planner,
    ctx: &Context,
    sink: &EventSink,
    opts: Options,
    prepare: &mut dyn Prepare,
) -> Summary {
    // A real run records what its `path:` / `env:` steps add afresh.
    if !opts.dry_run {
        ctx.run_env.clear();
    }
    let notices = Notices::default();
    let terminal = RwLock::new(());
    let n = plan.bundles.len().min(bundles.len());
    let mut sched = Scheduler {
        plan,
        bundles,
        schedule,
        planner,
        ctx,
        sink,
        opts,
        notices: &notices,
        terminal: &terminal,
        summary: Summary {
            dry_run: opts.dry_run,
            ..Summary::default()
        },
        state: vec![BundleState::Waiting; n],
        blocked: vec![None; n],
        completed: vec![false; n],
    };
    let jobs = opts.jobs.max(1);
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel::<(usize, Outcome, Summary)>();
        let mut running = 0;
        loop {
            while running < jobs {
                let Some(bi) = sched.next_ready() else {
                    break;
                };
                let Some(run) = sched.start(bi, prepare) else {
                    continue;
                };
                if jobs == 1 {
                    let (outcome, counts) = run.execute_caught();
                    sched.finish(bi, outcome, &counts);
                } else {
                    running += 1;
                    let tx = tx.clone();
                    scope.spawn(move || {
                        let (outcome, counts) = run.execute_caught();
                        let _ = tx.send((bi, outcome, counts));
                    });
                }
            }
            if running == 0 {
                break;
            }
            // Every worker sends exactly once (panics are caught), and `tx` is still held
            // here, so this only returns a result.
            let Ok((bi, outcome, counts)) = rx.recv() else {
                break;
            };
            running -= 1;
            sched.finish(bi, outcome, &counts);
        }
    });
    let mut summary = sched.summary;
    summary.completed = plan
        .bundles
        .iter()
        .zip(&sched.completed)
        .filter(|(_, done)| **done)
        .map(|(b, _)| b.name.clone())
        .collect();
    summary.notices = notices.list();
    summary
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BundleState {
    Waiting,
    Running,
    Done,
}

/// The calling thread's side of [`execute_prepared`]: which bundles may start, starting them
/// and recording how they finished.
struct Scheduler<'a> {
    plan: &'a Plan,
    bundles: &'a [Bundle],
    schedule: &'a Schedule,
    planner: &'a dyn Planner,
    ctx: &'a Context,
    sink: &'a EventSink,
    opts: Options,
    notices: &'a Notices,
    terminal: &'a RwLock<()>,
    summary: Summary,
    state: Vec<BundleState>,
    /// For each bundle: the failed bundle that stops its dependents, if any.
    blocked: Vec<Option<String>>,
    /// The bundles that ran to the end in a real run (see [`Summary::completed`]).
    completed: Vec<bool>,
}

impl<'a> Scheduler<'a> {
    /// The first waiting bundle (in plan order) whose predecessors have all finished.
    fn next_ready(&self) -> Option<usize> {
        (0..self.state.len()).find(|&i| {
            self.state[i] == BundleState::Waiting
                && self
                    .schedule
                    .predecessors(i)
                    .all(|p| self.state.get(p).is_none_or(|s| *s == BundleState::Done))
        })
    }

    /// Starts bundle `bi`: reports it started, then either settles it here (a failed
    /// dependency, a `check:` that is satisfied, a `when:` that does not match, secrets that
    /// cannot be resolved) or returns the run that applies its steps.
    fn start(&mut self, bi: usize, prepare: &mut dyn Prepare) -> Option<BundleRun<'a>> {
        self.state[bi] = BundleState::Running;
        let planned = &self.plan.bundles[bi];
        let bundle = &self.bundles[bi];
        self.sink.emit(Event::BundleStarted {
            bundle: bi,
            name: planned.name.clone(),
        });
        let failed_dep = self
            .schedule
            .requires_of(bi)
            .find_map(|d| self.blocked.get(d).cloned().flatten());
        let env = BundleEnv::new(self.ctx, bundle);
        let opts = self.opts;
        let satisfied =
            if planned.skip.is_some() || failed_dep.is_some() || opts.dry_run || opts.force {
                None
            } else {
                // Earlier bundles may have satisfied it since planning.
                run_check(bundle, &env, self.planner).ok().flatten()
            };
        let mut counts = Summary::default();
        let outcome = if let Some(dep) = &failed_dep {
            let detail = format!("{DEPENDENCY_FAILED} {dep} failed");
            skip_steps(planned, &detail, self.sink, &mut counts);
            Outcome::Skipped
        } else if let Some(reason) = satisfied {
            skip_steps(planned, &reason, self.sink, &mut counts);
            Outcome::Ok
        } else if planned.skip.is_some() {
            for _ in &planned.steps {
                counts.record(Outcome::Skipped);
            }
            Outcome::Skipped
        } else if let Err(e) = if opts.dry_run {
            Ok(())
        } else {
            // Resolving deferred secrets may unlock a provider on the terminal: like a step
            // that needs it, that happens with no step of another bundle running.
            let _alone = (!planned.needed.is_empty())
                .then(|| self.terminal.write().unwrap_or_else(|e| e.into_inner()));
            prepare.before_bundle(&planned.name, &planned.needed, self.sink)
        } {
            fail_steps(planned, &format!("{e:#}"), self.sink, &mut counts);
            Outcome::Failed
        } else {
            return Some(BundleRun {
                planned,
                bundle,
                run_env: self.schedule.run_env(self.bundles, bi, &self.ctx.run_env),
                env,
                planner: self.planner,
                ctx: self.ctx,
                sink: self.sink,
                opts,
                notices: self.notices,
                terminal: self.terminal,
            });
        };
        // Dependents of a bundle skipped for a failed dependency are skipped for that one.
        self.blocked[bi] = failed_dep;
        self.settle(bi, outcome, &counts, false);
        None
    }

    /// Records how a bundle returned by [`Scheduler::start`] finished.
    fn finish(&mut self, bi: usize, outcome: Outcome, counts: &Summary) {
        self.settle(bi, outcome, counts, true);
    }

    /// `ran`: the bundle's steps were applied (not settled by [`Scheduler::start`]).
    fn settle(&mut self, bi: usize, outcome: Outcome, counts: &Summary, ran: bool) {
        let name = self.plan.bundles[bi].name.clone();
        if outcome == Outcome::Failed && self.blocked[bi].is_none() {
            self.blocked[bi] = Some(name.clone());
        }
        if ran && !self.opts.dry_run && outcome != Outcome::Failed {
            self.completed[bi] = true;
        }
        let s = &mut self.summary;
        s.changed += counts.changed;
        s.ok += counts.ok;
        s.failed += counts.failed;
        s.skipped += counts.skipped;
        self.state[bi] = BundleState::Done;
        self.sink.emit(Event::BundleFinished {
            bundle: bi,
            name,
            outcome,
        });
    }
}

/// Reports every step of a bundle that is not run as skipped, with `detail`.
fn skip_steps(planned: &PlannedBundle, detail: &str, sink: &EventSink, summary: &mut Summary) {
    for step in &planned.steps {
        summary.record(Outcome::Skipped);
        sink.emit(Event::StepFinished {
            step: step.id,
            outcome: Outcome::Skipped,
            detail: Some(detail.to_string()),
            elapsed: std::time::Duration::ZERO,
        });
    }
}

/// Reports a bundle that could not start: its first step failed with `error`, the rest not run.
fn fail_steps(planned: &PlannedBundle, error: &str, sink: &EventSink, summary: &mut Summary) {
    for (i, step) in planned.steps.iter().enumerate() {
        let (outcome, detail) = if i == 0 {
            (Outcome::Failed, error.to_string())
        } else {
            (
                Outcome::Skipped,
                "not applied: an earlier step failed".to_string(),
            )
        };
        summary.record(outcome);
        sink.emit(Event::StepFinished {
            step: step.id,
            outcome,
            detail: Some(detail),
            elapsed: std::time::Duration::ZERO,
        });
    }
}

struct BundleRun<'a> {
    planned: &'a PlannedBundle,
    bundle: &'a Bundle,
    env: BundleEnv,
    /// The run environment as this bundle sees it (see [`Schedule::run_env`]).
    run_env: RunEnvCell,
    planner: &'a dyn Planner,
    ctx: &'a Context,
    sink: &'a EventSink,
    opts: Options,
    notices: &'a Notices,
    /// Held shared by every step that runs, and exclusively by one that needs the terminal
    /// or root (see [`step_runs_alone`]).
    terminal: &'a RwLock<()>,
}

impl BundleRun<'_> {
    /// An optional secret the step uses that has no value (see [`SecretLookup::absent`]): the
    /// step is skipped rather than failed.
    fn absent_secret(&self, step: &PlannedStep) -> Option<String> {
        let lookup = self.ctx.secrets.as_ref()?;
        step.secrets.iter().find(|n| lookup.absent(n)).cloned()
    }

    /// [`BundleRun::execute`], with its own counts; a panic in a directive fails the bundle
    /// instead of the run.
    fn execute_caught(&self) -> (Outcome, Summary) {
        let mut counts = Summary::default();
        match catch_unwind(AssertUnwindSafe(|| self.execute(&mut counts))) {
            Ok(outcome) => (outcome, counts),
            Err(_) => {
                counts.record(Outcome::Failed);
                (Outcome::Failed, counts)
            }
        }
    }

    fn execute(&self, summary: &mut Summary) -> Outcome {
        // Programs the bundle's directives run are looked up on its run environment's PATH.
        let _entered = self.run_env.enter();
        let plan_errors = self.planned.has_errors();
        let mut failed = plan_errors;
        let mut changed = false;
        // Handlers due: left over from an earlier run (real runs), then those of the steps that
        // changed something.
        let pending = PendingHandlers::in_dir(&self.ctx.state_dir);
        let mut notified = if self.opts.dry_run || self.bundle.handlers.is_empty() {
            BTreeSet::new()
        } else {
            pending.get(&self.bundle.name)
        };
        for (step, config) in self.planned.steps.iter().zip(&self.bundle.steps) {
            let started = Instant::now();
            let (outcome, detail) = match &step.state {
                PlannedState::Error(e) => (Outcome::Failed, Some(e.clone())),
                _ if plan_errors => (
                    Outcome::Skipped,
                    Some("not applied: the bundle has planning errors".into()),
                ),
                _ if failed => (
                    Outcome::Skipped,
                    Some("not applied: an earlier step failed".into()),
                ),
                PlannedState::Skipped(reason) => (Outcome::Skipped, Some(reason.clone())),
                PlannedState::Ready(_)
                    if !self.opts.dry_run && self.absent_secret(step).is_some() =>
                {
                    (
                        Outcome::Skipped,
                        self.absent_secret(step).map(|n| {
                            format!(
                                "optional secret `{n}` has no value (secrets.yml `optional: true`)"
                            )
                        }),
                    )
                }
                PlannedState::Ready(actions) => {
                    self.sink.emit(Event::StepStarted {
                        step: step.id,
                        bundle: self.planned.name.clone(),
                        kind: step.kind,
                        label: step.label.clone(),
                    });
                    if self.opts.dry_run {
                        self.preview(step.id, actions)
                    } else {
                        self.apply(step.id, config, actions, true)
                    }
                }
            };
            match outcome {
                Outcome::Failed => failed = true,
                Outcome::Changed => {
                    changed = true;
                    notified.extend(config.notify.iter().cloned());
                    // Recorded at once: a later step may take the run down before the
                    // handlers' turn (a panic, Ctrl-C), and the next run finds this step
                    // satisfied and would not notify again.
                    if !self.opts.dry_run
                        && !config.notify.is_empty()
                        && let Err(e) = pending.add(&self.bundle.name, &config.notify)
                    {
                        self.sink.emit(Event::Output {
                            step: step.id,
                            stream: crate::event::Stream::Stderr,
                            line: format!("warning: cannot record pending handlers: {e:#}"),
                        });
                    }
                }
                _ => {}
            }
            summary.record(outcome);
            self.sink.emit(Event::StepFinished {
                step: step.id,
                outcome,
                detail,
                elapsed: started.elapsed(),
            });
        }
        if !self.bundle.handlers.is_empty() {
            let (handlers_failed, handlers_changed) =
                self.run_handlers(failed, &notified, &pending, summary);
            failed |= handlers_failed;
            changed |= handlers_changed;
        }
        if failed {
            Outcome::Failed
        } else if changed {
            Outcome::Changed
        } else {
            Outcome::Ok
        }
    }

    /// Runs the notified handlers, once each, in the order the bundle lists them; the others
    /// are skipped. After a failure (`failed`) none run: they stay pending for the next run,
    /// as does a handler that fails. Returns (a handler failed, a handler changed something).
    fn run_handlers(
        &self,
        failed: bool,
        notified: &BTreeSet<String>,
        pending: &PendingHandlers,
        summary: &mut Summary,
    ) -> (bool, bool) {
        let n = self.bundle.steps.len();
        let mut due = BTreeSet::new();
        let (mut any_failed, mut any_changed) = (false, false);
        for (step, handler) in self.planned.steps.iter().skip(n).zip(&self.bundle.handlers) {
            let started = Instant::now();
            let is_notified = notified.contains(&handler.name);
            let (outcome, detail) = match &step.state {
                PlannedState::Error(e) => (Outcome::Failed, Some(e.clone())),
                _ if !is_notified => (Outcome::Skipped, Some("handler: not notified".into())),
                _ if failed => {
                    due.insert(handler.name.clone());
                    (
                        Outcome::Skipped,
                        Some(
                            "not applied: an earlier step failed (the handler is still due, and \
                             runs at the end of the next run)"
                                .into(),
                        ),
                    )
                }
                state => {
                    self.sink.emit(Event::StepStarted {
                        step: step.id,
                        bundle: self.planned.name.clone(),
                        kind: step.kind,
                        label: step.label.clone(),
                    });
                    let r = match (self.opts.dry_run, state) {
                        (true, PlannedState::Ready(actions)) => self.preview(step.id, actions),
                        (true, _) => (Outcome::Skipped, Some(NOT_NOTIFIED.into())),
                        (false, PlannedState::Ready(actions)) => {
                            self.apply(step.id, &handler.step, actions, false)
                        }
                        // Not expected to be notified when planned: nothing known up front.
                        (false, _) => self.apply(step.id, &handler.step, &[], false),
                    };
                    if r.0 == Outcome::Failed {
                        due.insert(handler.name.clone());
                    }
                    r
                }
            };
            match outcome {
                Outcome::Failed => any_failed = true,
                Outcome::Changed => any_changed = true,
                _ => {}
            }
            summary.record(outcome);
            self.sink.emit(Event::StepFinished {
                step: step.id,
                outcome,
                detail,
                elapsed: started.elapsed(),
            });
        }
        if !self.opts.dry_run
            && let Err(e) = pending.set(&self.bundle.name, &due)
        {
            self.sink.emit(Event::Output {
                step: StepId {
                    bundle: self.planned.steps.first().map_or(0, |s| s.id.bundle),
                    step: n,
                },
                stream: crate::event::Stream::Stderr,
                line: format!("warning: cannot record pending handlers: {e:#}"),
            });
        }
        (any_failed, any_changed)
    }

    /// Dry run: report the plan-time statuses.
    /// A conflict is a would-fail: the step fails, as applying it would.
    fn preview(&self, id: StepId, actions: &[PlannedAction]) -> (Outcome, Option<String>) {
        let mut changed = false;
        for pa in actions {
            let (outcome, error) = match pa.status {
                Status::Satisfied => (Outcome::Ok, None),
                Status::Pending | Status::Unknown => (Outcome::Changed, None),
                Status::Conflict => (
                    Outcome::Failed,
                    Some(format!("would fail: {}", pa.description)),
                ),
            };
            changed |= outcome == Outcome::Changed;
            self.sink.emit(Event::ActionFinished {
                step: id,
                description: pa.description.clone(),
                outcome,
                error: error.clone(),
            });
            if outcome == Outcome::Failed {
                return (Outcome::Failed, error);
            }
        }
        (step_outcome(changed), None)
    }

    /// Real run: plan the step again against the machine as the earlier steps left it, then
    /// check and apply each action in turn. `record`: record what it manages in the state store
    /// (not for handlers: they run on a change, and what they do is an event, not something
    /// the configuration keeps in place, so `dot status` would find it orphaned between
    /// notifications).
    fn apply(
        &self,
        id: StepId,
        step: &Step,
        planned: &[PlannedAction],
        record: bool,
    ) -> (Outcome, Option<String>) {
        let scratch = SecretRefs::default();
        let guard = || guard_skips(self.bundle, &self.env, step, self.planner, &self.run_env);
        let actions = match prepare_step(
            self.bundle,
            &self.env,
            step,
            self.planner,
            self.ctx,
            &scratch,
            true,
            &self.run_env,
        ) {
            Err(e) => {
                return match guard() {
                    Ok(Some(reason)) => (Outcome::Skipped, Some(reason)),
                    _ => (Outcome::Failed, Some(e)),
                };
            }
            Ok(Prepared::Skip(reason)) => return (Outcome::Skipped, Some(reason)),
            Ok(Prepared::Actions(a, _)) => a,
        };
        // Asked once, before the first action with something to do.
        let mut guarded = step.guard.is_none();
        // The single-terminal rule: one step at a time may prompt or hand the terminal over.
        let _turn = if step_runs_alone(planned, &actions) {
            Turn::Alone(self.terminal.write().unwrap_or_else(|e| e.into_inner()))
        } else {
            Turn::Shared(self.terminal.read().unwrap_or_else(|e| e.into_inner()))
        };
        let store = StateStore::at(&self.ctx.state_dir);
        let mut run_ctx = RunCtx::new(self.sink, id).with_notices(self.notices.clone());
        if record {
            run_ctx = run_ctx.with_state(store.clone());
        }
        let warn = |what: &str, e: anyhow::Error| {
            run_ctx.output(
                crate::event::Stream::Stderr,
                format!("warning: cannot {what} in {}: {e:#}", store.dir().display()),
            );
        };
        let mut changed = false;
        for action in &actions {
            let status = action.status();
            // Described after `status`, which may have found out more (which packages are
            // missing).
            let description = action.describe();
            let mut restores: Vec<(String, Restore)> = Vec::new();
            if !guarded && !matches!(status, Ok(Status::Satisfied)) {
                guarded = true;
                match guard() {
                    Ok(Some(reason)) => return (Outcome::Skipped, Some(reason)),
                    Ok(None) => {}
                    Err(e) => return (Outcome::Failed, Some(e)),
                }
            }
            let result = match status {
                Ok(Status::Satisfied) => Ok(Outcome::Ok),
                // Unknown at this point means the action itself must find out (and fail with a
                // clear error if its input is still missing). A conflicting action fails in
                // `apply` with its own, precise reason.
                Ok(s @ (Status::Pending | Status::Unknown | Status::Conflict)) => {
                    // What `dot` is about to write over for the first time is saved, so
                    // `dot remove` can put it back.
                    if record {
                        match store.preserve(&action.resources()) {
                            Ok(r) => restores = r,
                            Err(e) => warn("save what is replaced", e),
                        }
                    }
                    // Something since the prepare stage (any `brew` command) may have
                    // discarded the sudo timestamp the action's `sudo -n` relies on.
                    let sudo_ok = match &self.ctx.sudo {
                        Some(check)
                            if s != Status::Conflict
                                && action.needs_sudo()
                                && !action.asks_for_sudo() =>
                        {
                            check.before_sudo(self.sink, id, &description)
                        }
                        _ => Ok(()),
                    };
                    sudo_ok.and_then(|()| action.apply(&run_ctx))
                }
                Err(e) => Err(e),
            };
            let (outcome, error) = match result {
                Ok(o) => (o, None),
                Err(e) => (Outcome::Failed, Some(format!("{e:#}"))),
            };
            let backups = run_ctx.take_backups();
            if record && matches!(outcome, Outcome::Changed | Outcome::Ok) {
                let resources = action.resources();
                if let Some(first) = resources.first() {
                    restores.extend(backups.into_iter().map(|r| (first.key(), r)));
                }
                if let Err(e) = store.record_applied(
                    &self.planned.name,
                    &resources,
                    outcome == Outcome::Changed,
                    &restores,
                ) {
                    warn("record what was applied", e);
                }
            }
            self.sink.emit(Event::ActionFinished {
                step: id,
                description,
                outcome,
                error: error.clone(),
            });
            match outcome {
                Outcome::Failed => return (Outcome::Failed, error),
                Outcome::Changed => changed = true,
                _ => {}
            }
        }
        (step_outcome(changed), None)
    }
}

/// A held turn on the terminal lock, given back when the step is over.
enum Turn<'a> {
    Alone(#[allow(dead_code)] std::sync::RwLockWriteGuard<'a, ()>),
    Shared(#[allow(dead_code)] std::sync::RwLockReadGuard<'a, ()>),
}

/// Whether a step must run with no other step running: an action of it hands the terminal
/// over, or needs root (sudo's prompt, or its timestamp being refreshed, must not race another
/// step's output). Judged from the plan (statuses known) and from the actions as planned again
/// right before applying.
fn step_runs_alone(planned: &[PlannedAction], actions: &[Box<dyn Action>]) -> bool {
    planned
        .iter()
        .any(|a| a.action.needs_tty() || (a.status.has_work() && a.action.needs_sudo()))
        || actions.iter().any(|a| a.needs_tty() || a.needs_sudo())
}

fn step_outcome(changed: bool) -> Outcome {
    if changed {
        Outcome::Changed
    } else {
        Outcome::Ok
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod schedule_tests;
