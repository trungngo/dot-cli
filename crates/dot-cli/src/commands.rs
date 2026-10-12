//! The subcommands. Repo mode goes through [`dot_core::workspace::Workspace`]; `-c FILE` keeps
//! applying bundle files directly, outside any repository.

use std::collections::BTreeSet;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, Result};
use dot_core::config::{self, ConfigError};
use dot_core::engine::Schedule;
use dot_core::facts::{Fact, HostProbe};
use dot_core::graph::{Reason, Resolution};
use dot_core::runenv::EnvRecord;
use dot_core::vars::{EnvFile, Rank, load_env_files, parse_assignment};
use dot_core::workspace::{Inputs, OpenError, Workspace};
use dot_core::{
    Bundle, Context, Event, EventSink, Facts, Options, Plan, PrepareChain, Redactor, Renderer,
    StateStore, engine,
};
use dot_directives::Builtins;
use dot_directives::github_release::{Github, TOKEN_SECRET};
use dot_platform::{Packages, Sudo, SudoPrepare, SystemRunner};
use dot_secrets::{
    EnvfileProvider, Expected, Registry, Resolver, SecretsConfig, read_example_secret_keys,
    scan_committed_env_files,
};

use crate::prompt::{ConfirmPrepare, LinePrompter, Prompter, StdinLines};
use crate::render::json::JsonRenderer;
use crate::render::logfile::{LogFileRenderer, default_log_dir, default_state_dir};
use crate::render::plain::PlainRenderer;
use crate::render::tty::TtyPrompter;
use crate::render::{Mode, Verbosity};
use crate::secrets::{self, SecretsPrepare};
use crate::{
    ApplyArgs, CheckArgs, EXIT_CONFIG, GraphArgs, PlanArgs, RepoArgs, SecretsCheckArgs, VarsArgs,
    WhyArgs,
};

pub(crate) fn config_exit() -> ExitCode {
    ExitCode::from(EXIT_CONFIG)
}

/// The command-line inputs plus the process's home, cwd and environment.
pub fn inputs(r: &RepoArgs) -> Result<Inputs> {
    let vars = r
        .vars
        .iter()
        .map(|s| parse_assignment(s).map_err(anyhow::Error::msg))
        .collect::<Result<Vec<_>>>()
        .context("--var")?;
    Ok(Inputs {
        home: dirs::home_dir().context("cannot determine the home directory")?,
        cwd: dot_core::paths::plain(
            std::env::current_dir().context("cannot determine the current directory")?,
        ),
        env: std::env::vars().collect(),
        repo: r.repo.clone(),
        profile: r.profile.clone(),
        vars,
        env_files: r.env_files.clone(),
    })
}

/// The environment variable that overrides detected facts (see [`Facts::apply_overrides`]).
pub const FACTS_ENV: &str = "DOT_FACTS";

pub(crate) fn detect(role: Option<dot_core::facts::Role>) -> Facts {
    let mut facts = Facts::detect_with(&HostProbe, role);
    if let Ok(o) = std::env::var(FACTS_ENV)
        && let Err(e) = facts.apply_overrides(&o)
    {
        eprintln!("warning: {FACTS_ENV}: {e}; using the detected facts");
        facts = Facts::detect_with(&HostProbe, role);
    }
    facts
}

/// Opens the repository, printing every problem. `Err` carries the exit code.
pub(crate) fn open(r: &RepoArgs) -> Result<std::result::Result<(Workspace, Inputs), ExitCode>> {
    let inputs = inputs(r)?;
    match Workspace::open(&inputs, detect) {
        Ok(ws) => Ok(Ok((ws, inputs))),
        Err(OpenError::Config(errs)) => {
            for e in errs {
                eprintln!("error: {e}");
            }
            Ok(Err(config_exit()))
        }
        Err(OpenError::Other(e)) => {
            eprintln!("error: {e}");
            Ok(Err(config_exit()))
        }
    }
}

/// Resolves the bundle graph, printing every error.
pub(crate) fn resolve(
    ws: &Workspace,
    bundles: &[String],
    no_deps: bool,
) -> std::result::Result<Resolution, ExitCode> {
    ws.resolve(bundles, no_deps).map_err(|errs| {
        for e in errs {
            eprintln!("error: {e}");
        }
        config_exit()
    })
}

/// The workspace's `secrets.yml`, printing a parse error. `Err` carries the exit code.
pub(crate) fn secrets_config(
    ws: &Workspace,
) -> std::result::Result<Option<SecretsConfig>, ExitCode> {
    secrets::load_config(ws).map_err(|e| {
        eprintln!("error: {e}");
        config_exit()
    })
}

/// Secret names a plan uses that `secrets.yml` cannot serve: undeclared, or with a ref that does
/// not work under the effective provider. Checked without contacting any provider. Each names
/// where `bundles` use the secret.
fn plan_secret_problems(
    config: Option<&SecretsConfig>,
    names: &[String],
    bundles: &[Bundle],
) -> Vec<String> {
    if names.is_empty() {
        return Vec::new();
    }
    let Some(config) = config else {
        return vec![format!(
            "the plan needs secret{} {}, but there is no {}",
            plural(names.len()),
            names.join(", "),
            secrets::SECRETS_FILE
        )];
    };
    names
        .iter()
        .filter_map(|n| match config.entry(n) {
            None => Some(format!(
                "secret `{n}` is not declared in {}{}",
                secrets::SECRETS_FILE,
                secret_locations(bundles, n)
            )),
            Some(Err(e)) => Some(format!("secret `{n}`: {e}")),
            Some(Ok(_)) => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// apply / plan

pub fn apply(mut a: ApplyArgs) -> Result<ExitCode> {
    if let Some(host) = &a.remote.host {
        return crate::remote::apply(&a, host);
    }
    // Started by another machine (`dot apply --host`): what it resolved there, for this run.
    let mut received = match a.secrets_from.take() {
        Some(source) => {
            let repo = a.repo.clone();
            crate::handoff::receive_for_run(&source, &mut a.repo.vars, a.dry_run, || inputs(&repo))?
        }
        None => crate::handoff::Received::default(),
    };
    if !a.config.is_empty() {
        let Some(bundles) = load_files(&a.config) else {
            return Ok(config_exit());
        };
        let inputs = inputs(&a.repo)?;
        let ctx = match file_context(&inputs) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {e}");
                return Ok(config_exit());
            }
        };
        // Files are applied in the order given, one after the other.
        let schedule = Schedule::chain(bundles.len());
        return run(
            &a,
            &bundles,
            &schedule,
            ctx,
            Redactor::new(),
            None,
            None,
            &mut received,
        );
    }
    let (mut ws, inputs) = match open(&a.repo)? {
        Ok(x) => x,
        Err(code) => return Ok(code),
    };
    let res = match resolve(&ws, &a.bundles, a.no_deps) {
        Ok(r) => r,
        Err(code) => return Ok(code),
    };
    // Variables with a `prompt:` and no value: asked now, before anything runs; a run that
    // cannot ask fails here, listing them with every other missing variable.
    let unasked = match ask_target(&a) {
        _ if ws.var_prompts(&res).is_empty() => Vec::new(),
        Some(to) => {
            let term = match to {
                PromptTo::Stdout => console::Term::stdout(),
                PromptTo::Stderr => console::Term::stderr(),
            };
            let mut asker = crate::prompt::TermAsker(term);
            if let Err(e) =
                crate::prompt::ask_vars(&mut ws, &res, &mut asker, &inputs.local_env(), a.dry_run)
            {
                eprintln!("error: {e:#}");
                return Ok(config_exit());
            }
            Vec::new()
        }
        None => ws.var_prompts(&res).into_iter().map(|p| p.name).collect(),
    };
    if let Err(e) = ws.check_vars_including(&res, &unasked) {
        for line in e.lines() {
            eprintln!("error: {line}");
        }
        if !unasked.is_empty() {
            eprintln!(
                "error: there is no terminal to ask for {} here: give them with --var NAME=value, \
                 or run `dot apply` from a terminal",
                unasked.join(", ")
            );
        }
        return Ok(config_exit());
    }
    let leaks = secrets::committed_secrets(&ws.located.root, &secrets::provider_env(&inputs));
    if !leaks.is_empty() {
        for e in &leaks {
            eprintln!("error: {e}");
        }
        return Ok(config_exit());
    }
    let redactor = Redactor::new();
    for v in ws.sensitive_values(&res) {
        redactor.add(v);
    }
    let config = match secrets_config(&ws) {
        Ok(c) => c,
        Err(code) => return Ok(code),
    };
    let resolver = config.map(|c| secrets::resolver(c, &secrets::provider_env(&inputs)));
    let bundles = ws.bundles(&res);
    let schedule = res.schedule();
    let ctx = ws.context(&inputs.home);
    let header = resolution_report(&ws, &res);
    // Bundles named here that the profile does not select are opted in on this machine.
    let opted: Vec<String> = match ws.resolve(&[], false) {
        Ok(profile) => a
            .bundles
            .iter()
            .filter(|b| profile.get(b).is_none())
            .cloned()
            .collect(),
        Err(_) => Vec::new(),
    };
    let home = inputs.home.clone();
    let code = run(
        &a,
        &bundles,
        &schedule,
        ctx,
        redactor,
        Some(header),
        resolver,
        &mut received,
    );
    // Once something of theirs was applied (a cancelled run opts nothing in).
    if !a.dry_run
        && !opted.is_empty()
        && let Some(store) = state_store(&home)
    {
        let state = store.read();
        let applied: Vec<String> = opted
            .into_iter()
            .filter(|b| state.resources.values().any(|r| &r.bundle == b))
            .collect();
        if let Err(e) = store.set_opted_in(&applied, true) {
            eprintln!("warning: cannot record the opted-in bundles: {e:#}");
        }
    }
    code
}

/// The directives, set up for this machine and repository: `dot`'s state directory (apt's
/// refresh stamp, the installed release versions) and, when `secrets.yml` declares it, the
/// GitHub token secret for `github_release` steps without `token:`.
pub(crate) fn builtins(home: &Path, secrets: Option<&SecretsConfig>, upgrade: bool) -> Builtins {
    let state = default_state_dir(home);
    let mut github = Github::from_env();
    if state.is_some() {
        github.state_dir.clone_from(&state);
    }
    if secrets.is_some_and(|c| c.secrets.contains_key(TOKEN_SECRET)) {
        github.token_secret = Some(TOKEN_SECRET.to_string());
    }
    let mut b = Builtins {
        packages: Packages::system(state),
        github,
        upgrade,
        ..Builtins::default()
    };
    if let Some(root) = sysroot() {
        b.root_uid = b.system.uid.unwrap_or(0);
        b.etc_shells = dot_core::paths::rebase(&root, &b.etc_shells);
        b.sysroot = Some(root);
    }
    b
}

/// The environment variable that moves root-owned files into a directory (see
/// [`Builtins::sysroot`]), for sandboxes without root.
pub const SYSROOT_ENV: &str = "DOT_SYSROOT";

/// [`SYSROOT_ENV`], when set to an absolute path.
fn sysroot() -> Option<PathBuf> {
    let v = std::env::var_os(SYSROOT_ENV).filter(|v| !v.is_empty())?;
    let p = PathBuf::from(v);
    if p.is_absolute() {
        Some(p)
    } else {
        eprintln!("warning: {SYSROOT_ENV} must be an absolute path; ignored");
        None
    }
}

/// Points the context at `dot`'s state directory and what earlier runs recorded there for
/// `path:` / `env:` steps (their statuses are judged against that record).
pub(crate) fn use_state(ctx: &mut Context) {
    if let Some(state) = default_state_dir(&ctx.home) {
        ctx.state_dir = state;
    }
    let Some(store) = state_store(&ctx.home) else {
        return;
    };
    match EnvRecord::load(&store) {
        Ok(record) => ctx.run_env = ctx.run_env.clone().with_recorded(record.env),
        Err(e) => eprintln!("warning: {e:#}; path and env steps count as changes"),
    }
}

/// Applies `bundles`. `resolver`: the repository's secrets (`None` without a `secrets.yml`);
/// `received`: what another machine handed this run (`dot apply --host`): its secrets are used
/// first, and what it says to keep is written once the run is confirmed.
#[allow(clippy::too_many_arguments)]
fn run(
    a: &ApplyArgs,
    bundles: &[Bundle],
    schedule: &Schedule,
    mut ctx: Context,
    redactor: Redactor,
    header: Option<String>,
    resolver: Option<Resolver>,
    received: &mut crate::handoff::Received,
) -> Result<ExitCode> {
    use_state(&mut ctx);
    // One real run at a time: two would interleave their changes to the machine and the
    // state store. Dry runs change nothing and never wait.
    let _run_lock = if a.dry_run {
        None
    } else {
        match StateStore::at(&ctx.state_dir).run_lock("apply") {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("error: {e:#}");
                return Ok(ExitCode::FAILURE);
            }
        }
    };
    let sink = EventSink::new();
    let log_dir = a.log_dir.clone().or_else(|| default_log_dir(&ctx.home));
    let mut log_path = None;
    if let Some(dir) = log_dir {
        match LogFileRenderer::create(&dir, redactor.clone()) {
            Ok(r) => {
                // Added before the console renderer, so a step's log line is known by the
                // time the console reports it.
                log_path = Some(r.locator());
                // In plan order, whatever order `--jobs` finishes bundles in.
                sink.add(Box::new(crate::render::ordered::Ordered::new(r)));
            }
            Err(e) => eprintln!(
                "warning: cannot create a log file in {}: {e}",
                dir.display()
            ),
        }
    }
    let verbosity = Verbosity::from_flags(a.verbose, a.quiet);
    let mode = crate::render::detect_mode(a.json, a.plain);
    let (console, tty) =
        crate::render::console_renderer(mode, verbosity, a.color, redactor.clone(), log_path);
    sink.add(console);
    let opts = Options {
        dry_run: a.dry_run,
        force: a.force,
        jobs: usize::from(a.jobs),
    };
    // `--yes` / `--non-interactive`, or no terminal to ask on: nothing is ever prompted for.
    let target = prompt_target(
        mode,
        a.yes,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        std::io::stderr().is_terminal(),
    );
    let interactive = target.is_some();
    // The run's sudo: asked for once up front (when something needs it), checked again before
    // each action that relies on it, and the terminal's turn for a cask's installer that asks
    // for the password itself.
    let run_sudo = Sudo::new(std::sync::Arc::new(SystemRunner), interactive);
    let mut builtins = builtins(
        &ctx.home,
        resolver.as_ref().map(Resolver::config),
        a.upgrade,
    );
    builtins.packages = builtins.packages.clone().with_sudo(run_sudo.clone());
    if let Some(token) = builtins.github.env_token.as_ref().and_then(|t| t.as_str()) {
        redactor.add(token);
    }
    let mut secrets =
        SecretsPrepare::new(resolver, &redactor, interactive).with_locations(secret_uses(bundles));
    secrets.preload(&received.secrets, &redactor);
    for v in &received.sensitive {
        redactor.add(v.as_str());
    }
    ctx.secrets = Some(secrets.lookup());
    // Asks for sudo once, up front, when a planned action needs root; kept alive for the run.
    if !a.dry_run {
        ctx.sudo = Some(run_sudo.clone());
    }
    let mut sudo = SudoPrepare::with_sudo(run_sudo);
    let summary = if let Some(target) = target.filter(|_| !a.dry_run) {
        let out = move || -> Box<dyn Write + Send> {
            match target {
                PromptTo::Stdout => Box::new(std::io::stdout()),
                PromptTo::Stderr => Box::new(std::io::stderr()),
            }
        };
        let preview_tty = tty.clone();
        let verbosity = Verbosity::from_flags(a.verbose, a.quiet);
        let mut preview = |plan: &Plan| {
            let show = || print_plan(out(), header.as_deref(), plan, &redactor, verbosity);
            match &preview_tty {
                Some(c) => c.suspend(show),
                None => show(),
            }
        };
        // Never a `StdinLock` held for the run: the secrets stage reads the terminal too.
        let mut prompter: Box<dyn Prompter> = match tty {
            Some(c) => Box::new(TtyPrompter::new(c, LinePrompter::new(StdinLines, out()))),
            None => Box::new(LinePrompter::new(StdinLines, out())),
        };
        let mut chain = PrepareChain(vec![&mut sudo, &mut secrets, &mut *received]);
        let mut prepare = ConfirmPrepare {
            prompter: prompter.as_mut(),
            preview: &mut preview,
            inner: &mut chain,
        };
        engine::run_scheduled(
            bundles,
            schedule,
            &builtins,
            &ctx,
            &sink,
            opts,
            &mut prepare,
        )
    } else {
        let mut chain = PrepareChain(vec![&mut sudo, &mut secrets, &mut *received]);
        engine::run_scheduled(bundles, schedule, &builtins, &ctx, &sink, opts, &mut chain)
    };
    // The run is over: stop refreshing the sudo timestamp.
    sudo.stop();
    received.print_report();
    if !a.dry_run {
        record_run_env(&ctx, &summary.completed);
    }
    Ok(if summary.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `dot`'s state store (where, among other things, a real run records what its `path:` /
/// `env:` steps set, for `dot env --shell`).
pub(crate) fn state_store(home: &Path) -> Option<StateStore> {
    Some(StateStore::at(default_state_dir(home)?))
}

/// Where `apply`'s questions go, if anywhere (see [`prompt_target`]).
fn ask_target(a: &ApplyArgs) -> Option<PromptTo> {
    prompt_target(
        crate::render::detect_mode(a.json, a.plain),
        a.yes,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        std::io::stderr().is_terminal(),
    )
}

/// Saves what the run's `path:` / `env:` steps set, for `dot env --shell`: in the state
/// directory next to the logs, replacing what the `completed` bundles recorded before. A bundle
/// that did not run to the end (the run was cancelled or stopped before applying, the bundle or
/// a dependency failed) keeps its old record.
fn record_run_env(ctx: &Context, completed: &[String]) {
    if completed.is_empty() {
        return;
    }
    let store = StateStore::at(&ctx.state_dir);
    if let Err(e) = EnvRecord::record_run(&store, completed, &ctx.run_env.snapshot()) {
        eprintln!("warning: cannot record the run's PATH and environment: {e:#}");
    }
}

/// Where the plan preview and the "Apply?" question go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptTo {
    Stdout,
    Stderr,
}

/// Whether to ask before applying, and where. Never with `--yes` or when stdin is not a
/// terminal (nobody could answer). The question goes where the console output is drawn when
/// that is a terminal; when stdout is redirected (`dot apply > out.log`, `| tee`) or carries
/// `--json` events, it goes to stderr, if that is a terminal. With neither a terminal, the
/// question could not be seen, so the run is non-interactive.
fn prompt_target(
    mode: Mode,
    yes: bool,
    stdin_tty: bool,
    stdout_tty: bool,
    stderr_tty: bool,
) -> Option<PromptTo> {
    if yes || !stdin_tty {
        None
    } else if mode != Mode::Json && stdout_tty {
        Some(PromptTo::Stdout)
    } else if stderr_tty {
        Some(PromptTo::Stderr)
    } else {
        None
    }
}

/// The `dot plan` report of one plan: an optional header, then every step.
fn print_plan(
    mut out: Box<dyn Write + Send>,
    header: Option<&str>,
    plan: &Plan,
    redactor: &Redactor,
    verbosity: Verbosity,
) {
    if let Some(h) = header {
        let _ = write!(out, "{}", redactor.redact(h));
        let _ = out.flush();
    }
    let mut r = PlainRenderer::new(out, verbosity, redactor.clone())
        .timestamps(false)
        .show_plan(true);
    r.handle(&Event::PlanReady(plan.view()));
    r.finish();
}

pub fn plan(mut a: PlanArgs) -> Result<ExitCode> {
    if let Some(host) = &a.remote.host {
        return crate::remote::plan(&a, host);
    }
    // Started by another machine (`dot plan --host`): its variables (planning needs no secret).
    if let Some(source) = a.secrets_from.take() {
        let repo = a.repo.clone();
        drop(crate::handoff::receive_for_run(
            &source,
            &mut a.repo.vars,
            true,
            || inputs(&repo),
        )?);
    }
    let opts = Options {
        dry_run: true,
        force: a.force,
        ..Options::default()
    };
    let mut var_errors = false;
    let (bundles, schedule, mut ctx, header, redactor, secrets_config) = if !a.config.is_empty() {
        let Some(bundles) = load_files(&a.config) else {
            return Ok(config_exit());
        };
        let inputs = inputs(&a.repo)?;
        let schedule = Schedule::chain(bundles.len());
        match file_context(&inputs) {
            Ok(c) => (bundles, schedule, c, None, Redactor::new(), None),
            Err(e) => {
                eprintln!("error: {e}");
                return Ok(config_exit());
            }
        }
    } else {
        let (mut ws, inputs) = match open(&a.repo)? {
            Ok(x) => x,
            Err(code) => return Ok(code),
        };
        let res = match resolve(&ws, &a.bundles, a.no_deps) {
            Ok(r) => r,
            Err(code) => return Ok(code),
        };
        // Planned before they are asked: what uses them is unknown, not an error.
        ws.defer_prompts(&res);
        let redactor = Redactor::new();
        for v in ws.sensitive_values(&res) {
            redactor.add(v);
        }
        let config = match secrets_config(&ws) {
            Ok(c) => c,
            Err(code) => return Ok(code),
        };
        let mut header = resolution_report(&ws, &res);
        let prompts = ws.var_prompts(&res);
        if !prompts.is_empty() {
            let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
            header.push_str(&format!(
                "asked for by `dot apply`: {}\n\n",
                names.join(", ")
            ));
        }
        // Reported with the plan rather than instead of it: the rest is still worth seeing.
        if let Err(e) = ws.check_vars(&res) {
            var_errors = true;
            for line in e.lines() {
                if a.json {
                    // stdout is the JSON object; the reason goes to stderr.
                    eprintln!("error: {line}");
                } else {
                    header.push_str(&format!("error: {line}\n"));
                }
            }
            header.push('\n');
        }
        (
            ws.bundles(&res),
            res.schedule(),
            ws.context(&inputs.home),
            Some(header),
            redactor,
            config,
        )
    };
    use_state(&mut ctx);
    // Planning never unlocks or resolves: `secret()` renders a placeholder and records the name.
    let plan = engine::plan_scheduled(
        &bundles,
        &schedule,
        &builtins(&ctx.home, secrets_config.as_ref(), a.upgrade),
        &ctx,
        opts,
    );
    if a.json {
        let mut r = JsonRenderer::new(Box::new(std::io::stdout()), redactor.clone());
        r.handle(&Event::PlanReady(plan.view()));
        r.finish();
    } else {
        print_plan(
            Box::new(std::io::stdout()),
            header.as_deref(),
            &plan,
            &redactor,
            Verbosity::from_flags(a.verbose, false),
        );
    }
    let secret_problems =
        plan_secret_problems(secrets_config.as_ref(), &plan.secret_names(), &bundles);
    for p in &secret_problems {
        eprintln!("error: {p}");
    }
    Ok(
        if plan.has_errors() || plan.has_conflicts() || var_errors || !secret_problems.is_empty() {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        },
    )
}

/// Repository, profile, resolved order with reasons, and excluded bundles.
pub(crate) fn resolution_report(ws: &Workspace, res: &Resolution) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "repo: {} ({})\n",
        ws.located.root.display(),
        ws.located.how
    ));
    match &ws.profile {
        Some((name, how)) => out.push_str(&format!("profile: {name} ({how})\n")),
        None => out.push_str("profile: none (dot.yml defines no profiles)\n"),
    }
    let width = res.order.iter().map(|r| r.name.len()).max().unwrap_or(0);
    out.push_str("bundles, in order:\n");
    if res.order.is_empty() {
        out.push_str("  (none)\n");
    }
    for (i, r) in res.order.iter().enumerate() {
        let reasons: Vec<String> = r.reasons.iter().map(ToString::to_string).collect();
        out.push_str(&format!(
            "  {:>2}. {:<width$}  {}\n",
            i + 1,
            r.name,
            reasons.join("; ")
        ));
    }
    if !res.excluded.is_empty() {
        out.push_str("not included:\n");
        for e in &res.excluded {
            out.push_str(&format!("  - {} ({}): {}\n", e.name, e.reason, e.detail));
        }
    }
    out.push('\n');
    out
}

// ---------------------------------------------------------------------------------------------
// file mode (-c)

/// Loads every file, printing every error. `None` if any failed.
fn load_files(files: &[PathBuf]) -> Option<Vec<Bundle>> {
    let mut bundles = Vec::new();
    let mut ok = true;
    for f in files {
        match config::load_bundle(f) {
            Ok(b) => bundles.push(b),
            Err(e) => {
                eprintln!("error: {e}");
                ok = false;
            }
        }
    }
    ok.then_some(bundles)
}

/// The context for bundle files applied without a repository: facts, the environment,
/// `--env-file` and `--var`.
fn file_context(inputs: &Inputs) -> std::result::Result<Context, String> {
    let local = inputs.local_control().map_err(|e| e.to_string())?;
    let facts = detect(inputs.role_override(&local)?);
    let mut ctx = Context::new(facts.clone(), inputs.home.clone());
    let mut vars = inputs.base_vars(&facts);
    let files: Vec<EnvFile> = inputs
        .env_files
        .iter()
        .map(|f| EnvFile {
            path: inputs.cwd.join(f),
            rank: Rank::Cli,
            label: format!("--env-file {}", f.display()),
            required: true,
        })
        .collect();
    for layer in load_env_files(&files, &[]).map_err(|e| e.to_string())? {
        vars.push_layer(layer);
    }
    ctx.vars = vars;
    Ok(ctx)
}

// ---------------------------------------------------------------------------------------------
// check

pub fn check(a: CheckArgs) -> Result<ExitCode> {
    if !a.paths.is_empty() {
        return Ok(check_files(&a.paths));
    }
    let inputs = inputs(&a.repo)?;
    let local = inputs.local_control().map_err(anyhow::Error::msg)?;
    let located = match dot_core::repo::find_repo(
        inputs.repo.as_deref(),
        inputs.env.get("DOT_REPO").map(String::as_str),
        &inputs.cwd,
        local.get("DOT_REPO").map(String::as_str),
        &inputs.home,
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(config_exit());
        }
    };
    let (mut errors, n, providers) = match dot_core::repo::Repo::load(&located.root) {
        Ok(repo) => {
            let providers: Vec<Option<String>> = std::iter::once(&repo.config.secrets_provider)
                .chain(repo.config.profiles.iter().map(|p| &p.secrets_provider))
                .filter(|p| p.is_some())
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let mut errors = repo.check();
            errors.extend(undeclared_secrets(&located.root, repo.bundles.values()));
            let facts = detect(inputs.role_override(&local).ok().flatten());
            errors.extend(dot_core::workspace::check_env_files(
                &located.root,
                &repo,
                &facts,
                &inputs.home,
            ));
            (errors, repo.bundles.len(), providers)
        }
        Err(errs) => {
            let n = dot_core::repo::load_repo_config(&located.root)
                .ok()
                .and_then(|c| config::find_bundle_files(&c.bundles_dir).ok())
                .map(|f| f.len())
                .unwrap_or(0);
            (errs, n, Vec::new())
        }
    };
    let penv = secrets::provider_env(&inputs);
    let registry = Registry::builtin(&penv);
    errors.extend(secrets::check_config(&located.root, &providers, &registry));
    errors.extend(secrets::committed_secrets(&located.root, &penv));
    let files = if secrets::secrets_file(&located.root).exists() {
        "dot.yml, secrets.yml"
    } else {
        "dot.yml"
    };
    Ok(report_check(
        &errors,
        &format!("{files} and {n} bundle{}", plural(n)),
    ))
}

pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Every place a bundle names a secret that `secrets.yml` does not declare (or names one at all
/// when there is no `secrets.yml`), located at the use. An unparsable `secrets.yml` is reported
/// by [`secrets::check_config`] instead.
fn undeclared_secrets<'a>(
    root: &Path,
    bundles: impl IntoIterator<Item = &'a Bundle>,
) -> Vec<ConfigError> {
    let path = secrets::secrets_file(root);
    let config = if path.exists() {
        match SecretsConfig::load(&path) {
            Ok(c) => Some(c),
            Err(_) => return Vec::new(),
        }
    } else {
        None
    };
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for b in bundles {
        for site in config::secret_uses(b) {
            if config
                .as_ref()
                .is_some_and(|c| c.entry(&site.name).is_some())
            {
                continue;
            }
            let key = (site.at.file.clone(), site.at.line, site.name.clone());
            if !seen.insert(key) {
                continue;
            }
            out.push(ConfigError {
                message: format!(
                    "{}: secret `{}` is not declared in {}",
                    site.at.message,
                    site.name,
                    secrets::SECRETS_FILE
                ),
                ..site.at
            });
        }
    }
    out
}

/// Where the bundles of a run name each secret: name -> `file:line, file:line`.
fn secret_uses(bundles: &[Bundle]) -> std::collections::BTreeMap<String, String> {
    let mut places: std::collections::BTreeMap<String, BTreeSet<String>> = Default::default();
    for site in bundles.iter().flat_map(config::secret_uses) {
        let mut at = site.at.file.display().to_string();
        if let Some(l) = site.at.line {
            at.push_str(&format!(":{l}"));
        }
        places.entry(site.name).or_default().insert(at);
    }
    places
        .into_iter()
        .map(|(n, p)| (n, p.into_iter().collect::<Vec<_>>().join(", ")))
        .collect()
}

/// Where the bundles of a run name `secret`, for an error about it: ` (used at a:1, b:3)`.
fn secret_locations(bundles: &[Bundle], secret: &str) -> String {
    match secret_uses(bundles).remove(secret) {
        Some(at) => format!(" (used at {at})"),
        None => String::new(),
    }
}

fn report_check(errors: &[ConfigError], what: &str) -> ExitCode {
    for e in errors {
        eprintln!("error: {e}");
    }
    if errors.is_empty() {
        println!("checked {what}: ok");
        ExitCode::SUCCESS
    } else {
        println!(
            "checked {what}: {} error{}",
            errors.len(),
            plural(errors.len())
        );
        ExitCode::FAILURE
    }
}

fn check_files(paths: &[PathBuf]) -> ExitCode {
    let mut files = Vec::new();
    let mut errors: Vec<ConfigError> = Vec::new();
    for p in paths {
        if p.is_dir() {
            match config::find_bundle_files(p) {
                Ok(found) if found.is_empty() => {
                    errors.push(ConfigError::new(p, "no bundle files found"))
                }
                Ok(found) => files.extend(found),
                Err(e) => errors.push(ConfigError::new(p, e.to_string())),
            }
        } else {
            files.push(p.clone());
        }
    }
    for f in &files {
        match config::load_bundle_all_errors(f) {
            Ok(b) => errors.extend(config::lint(&b)),
            Err(e) => errors.extend(e),
        }
    }
    let n = files.len();
    report_check(&errors, &format!("{n} bundle file{}", plural(n)))
}

// ---------------------------------------------------------------------------------------------
// graph / why / vars

pub fn graph(a: GraphArgs) -> Result<ExitCode> {
    let (ws, _) = match open(&a.repo)? {
        Ok(x) => x,
        Err(code) => return Ok(code),
    };
    let res = match resolve(&ws, &a.bundles, false) {
        Ok(r) => r,
        Err(code) => return Ok(code),
    };
    print!(
        "{}",
        if a.dot {
            graphviz(&res)
        } else {
            graph_text(&res)
        }
    );
    Ok(ExitCode::SUCCESS)
}

fn graph_text(res: &Resolution) -> String {
    let mut out = String::new();
    for r in &res.order {
        out.push_str(&r.name);
        out.push('\n');
        for e in res.edges.iter().filter(|e| e.from == r.name) {
            out.push_str(&format!("  {} {}\n", e.kind.label(), e.to));
        }
    }
    out
}

fn graphviz(res: &Resolution) -> String {
    let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let mut out = String::from("digraph dot {\n  rankdir=LR;\n");
    for r in &res.order {
        let implicit = r.reasons.contains(&Reason::Implicit);
        let selected = r
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::Requested | Reason::Selected(_)));
        let style = match (selected, implicit) {
            (true, _) => " [style=bold]",
            (_, true) => " [style=dashed]",
            _ => "",
        };
        out.push_str(&format!("  {}{style};\n", q(&r.name)));
    }
    for e in &res.edges {
        let attrs = match &e.kind {
            dot_core::graph::EdgeKind::Requires => String::new(),
            dot_core::graph::EdgeKind::RequiresIf => " [label=\"if\"]".into(),
            dot_core::graph::EdgeKind::Capability(c) => {
                format!(" [label={}]", q(&format!("cap:{c}")))
            }
            dot_core::graph::EdgeKind::After => " [style=dotted, label=\"after\"]".into(),
        };
        out.push_str(&format!("  {} -> {}{attrs};\n", q(&e.from), q(&e.to)));
    }
    out.push_str("}\n");
    out
}

pub fn why(a: WhyArgs) -> Result<ExitCode> {
    let (ws, _) = match open(&a.repo)? {
        Ok(x) => x,
        Err(code) => return Ok(code),
    };
    if !ws.repo.bundles.contains_key(&a.bundle) {
        eprintln!("error: unknown bundle `{}`", a.bundle);
        return Ok(config_exit());
    }
    let res = match resolve(&ws, &[], false) {
        Ok(r) => r,
        Err(code) => return Ok(code),
    };
    print!("{}", why_text(&ws, &res, &a.bundle));
    Ok(ExitCode::SUCCESS)
}

fn why_text(ws: &Workspace, res: &Resolution, name: &str) -> String {
    let profile = ws
        .profile_name()
        .map(|p| format!("profile {p}"))
        .unwrap_or_else(|| "this repository".into());
    if res.get(name).is_some() {
        let mut out = format!("{name} is included ({profile}):\n");
        why_tree(res, name, 1, &mut BTreeSet::new(), &mut out);
        return out;
    }
    if let Some(e) = res.excluded.iter().find(|e| e.name == name) {
        return format!(
            "{name} is not included ({profile}): it is {}, but its {}\n",
            e.reason, e.detail
        );
    }
    format!("{name} is not included ({profile}): nothing selects or requires it\n")
}

fn why_tree(
    res: &Resolution,
    name: &str,
    depth: usize,
    seen: &mut BTreeSet<String>,
    out: &mut String,
) {
    let Some(r) = res.get(name) else { return };
    if !seen.insert(name.to_string()) {
        return;
    }
    for reason in &r.reasons {
        out.push_str(&format!("{}- {reason}\n", "  ".repeat(depth)));
        if let Some(parent) = reason.parent() {
            why_tree(res, parent, depth + 1, seen, out);
        }
    }
    seen.remove(name);
}

pub fn vars(a: VarsArgs) -> Result<ExitCode> {
    let (ws, _) = match open(&a.repo)? {
        Ok(x) => x,
        Err(code) => return Ok(code),
    };
    let vars = match &a.bundle {
        None => ws.vars.clone(),
        Some(b) => match ws.repo.bundles.get(b) {
            Some(bundle) => ws.vars.with_bundle(bundle),
            None => {
                eprintln!("error: unknown bundle `{b}`");
                return Ok(config_exit());
            }
        },
    };
    // Backstop: whatever slipped through the `sensitive` flags, a sensitive value is never
    // printed, not even inside another value.
    let redactor = Redactor::new();
    for (_, v) in vars.iter().filter(|(_, v)| v.sensitive) {
        redactor.add(v.as_string());
    }
    let rows: Vec<(String, String, String)> = vars
        .iter()
        .map(|(name, v)| {
            let value = if v.sensitive {
                dot_core::redact::MASK.to_string()
            } else if v.value.is_null() {
                "(unknown)".to_string()
            } else {
                match &v.value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                }
            };
            (
                name.clone(),
                redactor.redact(&value).into_owned(),
                v.source.to_string(),
            )
        })
        .collect();
    let w_name = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let w_value = rows
        .iter()
        .map(|r| r.1.chars().count().min(60))
        .max()
        .unwrap_or(0);
    for (name, value, source) in rows {
        println!("{name:<w_name$}  {value:<w_value$}  ({source})");
    }
    let missing = vars.missing_required();
    if !missing.is_empty() {
        println!("missing required: {}", missing.join(", "));
    }
    for p in vars.problems() {
        println!("problem: {}", redactor.redact(p));
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------------------------
// secrets check

/// `dot secrets check`: what the selected profile's bundles need against what `secrets.yml`
/// declares, the envfile's missing and unused keys and its permissions, and committed values for
/// secret keys. Names and paths only; no provider is unlocked and no value is printed.
pub fn secrets_check(a: SecretsCheckArgs) -> Result<ExitCode> {
    let (ws, inputs) = match open(&a.repo)? {
        Ok(x) => x,
        Err(code) => return Ok(code),
    };
    let root = &ws.located.root;
    let path = secrets::secrets_file(root);
    let config = match secrets_config(&ws) {
        Ok(Some(c)) => c,
        Err(code) => return Ok(code),
        Ok(None) => {
            eprintln!("error: {} does not exist", path.display());
            return Ok(config_exit());
        }
    };
    let penv = secrets::provider_env(&inputs);
    let home = &inputs.home;
    let shown = |p: &std::path::Path| dot_core::paths::display(p, home);
    let mut problems = 0usize;
    let report = |label: &str, items: &[String]| {
        if !items.is_empty() {
            println!("{label}: {}", items.join(", "));
        }
    };

    println!(
        "secrets: {} (bare refs: {})",
        shown(&path),
        config.default_provider
    );
    for p in config.validate(&Registry::builtin(&penv)) {
        println!("problem: {p}");
        problems += 1;
    }

    // What this profile's bundles ask for, found by planning (which never resolves a secret).
    let declared: BTreeSet<String> = config.secrets.keys().cloned().collect();
    match ws.resolve(&[], false) {
        Ok(res) => {
            let opts = Options {
                dry_run: true,
                ..Default::default()
            };
            let plan = engine::plan_with(
                &ws.bundles(&res),
                &builtins(home, Some(&config), false),
                &ws.context(home),
                opts,
            );
            let used: BTreeSet<String> = plan.secrets.keys().cloned().collect();
            let undeclared: Vec<String> = used.difference(&declared).cloned().collect();
            let unused: Vec<String> = declared.difference(&used).cloned().collect();
            problems += undeclared.len();
            report("used but not declared", &undeclared);
            let who = ws
                .profile_name()
                .map(|p| format!("profile {p}"))
                .unwrap_or_else(|| "the selected bundles".into());
            report(&format!("declared but not used by {who}"), &unused);
        }
        Err(errs) => {
            for e in errs {
                println!("problem: {e}");
                problems += 1;
            }
        }
    }

    // `.env.example` keys marked `# secret`.
    let example = root.join(".env.example");
    let example_keys = if example.exists() {
        match read_example_secret_keys(&example) {
            Ok(k) => k,
            Err(e) => {
                println!("problem: {e:#}");
                problems += 1;
                BTreeSet::new()
            }
        }
    } else {
        BTreeSet::new()
    };

    // A `.env.example` key that `secrets.yml` maps only to optional secrets stays optional.
    let from_config = Expected::from_config(&config);
    let example_required: BTreeSet<String> = example_keys
        .iter()
        .filter(|k| !from_config.optional_keys.contains(*k))
        .cloned()
        .collect();
    let expected = from_config.merge(Expected {
        keys: example_required,
        ..Expected::default()
    });
    let uses_envfile = config.default_provider == "envfile"
        || !expected.keys.is_empty()
        || !expected.files.is_empty();
    if uses_envfile {
        let provider = EnvfileProvider::from_config(config.providers.envfile.as_ref(), &penv);
        let r = provider.check(&expected);
        let exists = if provider.path().exists() {
            ""
        } else {
            " (does not exist)"
        };
        println!("envfile: {}{exists}", shown(provider.path()));
        problems += r.missing_keys.len() + r.missing_files.len() + r.problems.len();
        report("missing keys", &r.missing_keys);
        report("missing files", &r.missing_files);
        report(
            "not set (optional: the steps using them are skipped)",
            &r.missing_optional,
        );
        report("unused keys", &r.unused_keys);
        for p in &r.problems {
            println!("problem: {p}");
        }
    }

    match scan_committed_env_files(&penv.git, root, &example_keys) {
        Ok(leaks) => {
            for l in &leaks {
                println!("committed secret: {l}");
            }
            problems += leaks.len();
        }
        Err(e) => {
            println!("problem: cannot scan committed .env files: {e:#}");
            problems += 1;
        }
    }

    if problems == 0 {
        println!("secrets check: ok");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("secrets check: {problems} problem{}", plural(problems));
        Ok(ExitCode::FAILURE)
    }
}

// ---------------------------------------------------------------------------------------------
// schema / facts

pub fn schema(which: crate::SchemaKind) -> Result<ExitCode> {
    let schema = match which {
        crate::SchemaKind::Bundle => config::json_schema(),
        crate::SchemaKind::Repo => dot_core::repo::json_schema(),
        crate::SchemaKind::Secrets => dot_secrets::json_schema(),
    };
    println!("{}", serde_json::to_string_pretty(&schema)?);
    Ok(ExitCode::SUCCESS)
}

pub fn facts() -> Result<ExitCode> {
    let inputs = inputs(&RepoArgs::default())?;
    let local = inputs.local_control().map_err(anyhow::Error::msg)?;
    let facts = detect(inputs.role_override(&local).map_err(anyhow::Error::msg)?);
    for f in Fact::ALL {
        let v = facts.values(f);
        let shown = if v.is_empty() {
            "-".to_string()
        } else {
            v.join(", ")
        };
        println!("{f}: {shown}");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_question_goes_where_someone_can_see_it() {
        use PromptTo::*;
        // A terminal everywhere: with the output.
        assert_eq!(
            prompt_target(Mode::Tty, false, true, true, true),
            Some(Stdout)
        );
        // `dot apply > out.log`: stdout redirected, stdin still a terminal.
        assert_eq!(
            prompt_target(Mode::Plain, false, true, false, true),
            Some(Stderr)
        );
        // `--json` on a terminal: stdout is for events.
        assert_eq!(
            prompt_target(Mode::Json, false, true, true, true),
            Some(Stderr)
        );
        // Nowhere to show the question: never wait for an answer.
        assert_eq!(prompt_target(Mode::Plain, false, true, false, false), None);
        assert_eq!(prompt_target(Mode::Json, false, true, true, false), None);
        // `--yes`, or nobody at stdin.
        assert_eq!(prompt_target(Mode::Tty, true, true, true, true), None);
        assert_eq!(prompt_target(Mode::Tty, false, false, true, true), None);
    }
}
