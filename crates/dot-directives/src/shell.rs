//! `shell`: run a command through an interpreter: the step's `interpreter:`, else the inherited
//! `defaults: { shell: { interpreter } }`, else the platform's (`sh`; on Windows `pwsh` when
//! installed, else `powershell`). See [`dot_platform::shell`] for how each one is run
//! (PowerShell scripts, multi-line ones included, go through `-EncodedCommand`, and their exit
//! codes are propagated). `cmd` and `powershell` exist only on Windows: naming one for another
//! OS is a plan-time error.
//!
//! Non-interactive steps inherit stdin (so `sudo` can prompt on the terminal) and have stdout and
//! stderr piped and streamed line by line as `Output` events. Interactive steps get the terminal
//! outright, bracketed by `NeedsTty` / `TtyReleased`.
//!
//! A `secret()` in `cmd` or `unless` never appears in the command line (argv is readable by
//! every local user through `ps`): it renders as a reference to an environment variable holding
//! the value (`${DOT_SECRET_<name>}` for `sh`, `${env:DOT_SECRET_<name>}` for PowerShell,
//! `%DOT_SECRET_<name>%` for `cmd`), which is set on the child only. Use it unquoted or in double
//! quotes, not in single quotes.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use dot_core::config::{ShellInterpreter, ShellSpec};
use dot_core::facts::Os;
use dot_core::{Action, Outcome, PlanCtx, RenderedCommand, RunCtx, SecretBytes, Status, Stream};
use dot_platform::shell::Interpreter;

/// The comment line between a step's `prelude` and its command (both run as one script; plans
/// show what follows it).
const PRELUDE_END: &str = "# -- dot: end of prelude --";

/// The platform interpreter a config name stands for.
pub fn interpreter_of(i: ShellInterpreter) -> Interpreter {
    Interpreter::parse(i.as_str()).expect("config and platform name the same interpreters")
}

/// `default`: the platform's interpreter, for steps whose config names none. `upgrading`:
/// `dot apply --upgrade`, under which a step's `upgrade:` command runs when `unless` succeeds.
pub fn plan(
    spec: &ShellSpec,
    ctx: &PlanCtx<'_>,
    default: Interpreter,
    upgrading: bool,
) -> Result<Vec<Box<dyn Action>>> {
    if spec.upgrade.is_some() && spec.unless.is_none() {
        return Err(ctx.config_error(
            "shell: `upgrade` needs `unless` (without it the command runs every time anyway)",
        ));
    }
    let chosen = spec.interpreter(ctx.defaults);
    let interpreter = chosen.map_or(default, interpreter_of);
    if interpreter.windows_only() && ctx.facts.os != Os::Windows {
        return Err(ctx.config_error(format!(
            "interpreter `{interpreter}` only exists on Windows (this is {}); use `pwsh`, or add \
             `when: {{ os: windows }}` to the step",
            ctx.facts.os.as_str()
        )));
    }
    let cwd = match &spec.cwd {
        Some(c) => ctx.source(c)?,
        None => ctx.bundle.dir.clone(),
    };
    let prelude = spec.prelude(ctx.defaults);
    let script = |s: &str| -> Result<RenderedCommand> {
        // The prelude runs first, in the same script; plans show the command alone.
        let full = match prelude {
            Some(p) => format!("{}\n{PRELUDE_END}\n{s}", p.trim_end()),
            None => s.to_string(),
        };
        let mut rendered = ctx.render_command(&full, move |var| interpreter.env_ref(var))?;
        interpreter
            .validate(&rendered.text)
            .map_err(|e| ctx.config_error(e))?;
        if let Some((_, cmd)) = rendered.display.split_once(&format!("{PRELUDE_END}\n")) {
            rendered.display = cmd.to_string();
        }
        Ok(rendered)
    };
    // Declared variables first (sensitive ones only when listed), then what earlier `path:` /
    // `env:` steps set, then the step's own `env:`.
    let mut env = ctx.exported_env(&spec.export_sensitive)?;
    for (k, v) in ctx.run_env().child_env() {
        env.retain(|(n, _)| *n != k);
        env.push((k, v));
    }
    for (k, v) in &spec.env {
        env.retain(|(n, _)| n != k);
        env.push((k.clone(), ctx.render(v)?));
    }
    let cmd = script(&spec.cmd)?;
    let unless = spec.unless.as_deref().map(script).transpose()?;
    // Only rendered when it may run.
    let upgrade = match &spec.upgrade {
        Some(u) if upgrading => Some(script(u)?),
        _ => None,
    };
    let mut secret_env = cmd.secret_env.clone();
    for extra in [&unless, &upgrade].into_iter().flatten() {
        for (var, value) in &extra.secret_env {
            if !secret_env.iter().any(|(v, _)| v == var) {
                secret_env.push((var.clone(), value.clone()));
            }
        }
    }
    Ok(vec![Box::new(ShellAction {
        unless_needs_secret: unless.as_ref().is_some_and(|u| u.placeholder),
        cmd_needs_secret: cmd.placeholder,
        upgrade_needs_secret: upgrade.as_ref().is_some_and(|u| u.placeholder),
        unless_display: unless.as_ref().map(|u| u.display.clone()),
        unless: unless.map(|u| u.text),
        upgrade_display: upgrade.as_ref().map(|u| u.display.clone()),
        upgrade: upgrade.map(|u| u.text),
        cmd_display: cmd.display,
        cmd: cmd.text,
        cwd,
        env,
        secret_env,
        interactive: spec.interactive(ctx.defaults),
        sudo: spec.sudo,
        interpreter,
        named: chosen.is_some(),
    })])
}

pub struct ShellAction {
    /// The script as run: secrets in it are references to `secret_env` variables.
    pub cmd: String,
    /// The script as shown, secrets as `<secret:name>`.
    pub cmd_display: String,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// The variables `cmd` and `unless` refer to for their secrets, with the values; set on the
    /// child's environment only.
    pub secret_env: Vec<(String, SecretBytes)>,
    pub interactive: bool,
    pub unless: Option<String>,
    pub unless_display: Option<String>,
    /// The `unless` check uses a secret not resolved yet (planning): its answer is unknown.
    pub unless_needs_secret: bool,
    /// `cmd` / `upgrade` use a secret that is not resolved: they must not run.
    pub cmd_needs_secret: bool,
    pub upgrade_needs_secret: bool,
    /// `dot apply --upgrade` only: run instead of skipping when `unless` succeeds.
    pub upgrade: Option<String>,
    pub upgrade_display: Option<String>,
    /// The command uses `sudo` (`sudo: true`): see [`Action::needs_sudo`].
    pub sudo: bool,
    pub interpreter: Interpreter,
    /// The config chose the interpreter (shown in the plan).
    named: bool,
}

impl ShellAction {
    fn command(&self, script: &str) -> Command {
        self.command_with(script, false)
    }

    fn command_with(&self, script: &str, interactive: bool) -> Command {
        let mut c = self.interpreter.command_with(script, interactive);
        c.current_dir(&self.cwd);
        c.envs(self.env.iter().map(|(k, v)| (k, v)));
        for (var, value) in &self.secret_env {
            // Checked UTF-8 when rendered (`secret()` needs text).
            if let Some(v) = value.as_str() {
                c.env(var, v);
            }
        }
        c
    }

    fn check_cwd(&self) -> Result<()> {
        if !self.cwd.is_dir() {
            bail!("working directory {} does not exist", self.cwd.display());
        }
        Ok(())
    }

    fn cwd_missing(&self) -> bool {
        !self.cwd.is_dir()
    }

    /// The interpreter is not installed (yet): an earlier step or bundle may install it
    /// (`pwsh`), and a plan for another OS (`DOT_FACTS=os=windows`) may not have it at all.
    fn interpreter_missing(&self) -> bool {
        let path = self
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| std::ffi::OsString::from(v))
            .or_else(|| std::env::var_os("PATH"));
        dot_platform::exec::which_in(self.interpreter.program(), path.as_deref()).is_none()
    }

    /// Whether the `unless` check (there must be one) exits 0.
    fn unless_passes(&self) -> Result<bool> {
        let unless = self.unless.as_deref().unwrap_or_default();
        let st = self
            .command(unless)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| {
                let shown = self.unless_display.as_deref().unwrap_or(unless);
                format!("running unless check `{shown}`")
            })?;
        Ok(st.success())
    }
}

fn first_line(s: &str) -> String {
    let s = s.trim();
    let first = s.lines().next().unwrap_or("");
    if first.len() < s.len() {
        format!("{first} ...")
    } else {
        first.to_string()
    }
}

impl Action for ShellAction {
    fn needs_sudo(&self) -> bool {
        self.sudo
    }

    fn needs_tty(&self) -> bool {
        self.interactive
    }

    fn describe(&self) -> String {
        let mut d = format!("run `{}`", first_line(&self.cmd_display));
        if self.named {
            d.push_str(&format!(" with {}", self.interpreter));
        }
        if let Some(u) = &self.unless_display {
            d.push_str(&format!(" unless `{}` succeeds", first_line(u)));
        }
        if let Some(u) = &self.upgrade_display {
            d.push_str(&format!(", else (upgrading) `{}`", first_line(u)));
        }
        if self.interactive {
            d.push_str(" (interactive)");
        }
        if self.cwd_missing() {
            d.push_str(&format!(
                " (working directory {} does not exist yet)",
                self.cwd.display()
            ));
        }
        if self.unless.is_some() && self.interpreter_missing() {
            d.push_str(&format!(
                " ({} is not installed yet: checked when applied)",
                self.interpreter.program()
            ));
        }
        d
    }

    /// Runs the `unless` check, if any; that command must be read-only. With the working
    /// directory not there yet (an earlier step may create it) the answer is unknown. When it
    /// succeeds and the run upgrades, a step with `upgrade:` still has work.
    fn status(&self) -> Result<Status> {
        if self.unless.is_none() {
            return Ok(Status::Pending);
        }
        if self.cwd_missing() || self.unless_needs_secret || self.interpreter_missing() {
            return Ok(Status::Unknown);
        }
        Ok(match (self.unless_passes()?, &self.upgrade) {
            (true, None) => Status::Satisfied,
            (true, Some(_)) | (false, _) => Status::Pending,
        })
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        self.check_cwd()?;
        if self.unless_needs_secret {
            bail!("the `unless` check uses a secret that was not resolved");
        }
        let program = self.interpreter.program();
        if dot_platform::exec::which(program).is_none() {
            bail!("the interpreter `{program}` is not installed (or not on PATH)");
        }
        // Installed already (the run upgrades, or the step would not be applied): upgrade.
        let (script, shown, placeholder) = match (&self.upgrade, &self.upgrade_display) {
            (Some(u), Some(d)) if self.unless.is_some() && self.unless_passes()? => {
                (u, d, self.upgrade_needs_secret)
            }
            _ => (&self.cmd, &self.cmd_display, self.cmd_needs_secret),
        };
        if placeholder {
            bail!(
                "`{}` uses a secret that was not resolved",
                first_line(shown)
            );
        }
        let mut cmd = self.command_with(script, self.interactive);
        let status = if self.interactive {
            ctx.needs_tty(format!("interactive command `{}`", first_line(shown)));
            let st = cmd
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status();
            ctx.tty_released();
            st.context("starting command")?
        } else {
            run_captured(cmd, ctx)?.status
        };
        if !status.success() {
            bail!("command {}", describe_exit(status));
        }
        Ok(Outcome::Changed)
    }
}

/// The result of [`run_captured`]: exit status and every line, in arrival order.
pub struct Captured {
    pub status: ExitStatus,
    pub lines: Vec<(Stream, String)>,
}

/// How long output is still collected after the command itself has exited. Output written later
/// can only come from background processes it started, which may run for ever.
pub const EXIT_GRACE: Duration = Duration::from_millis(500);

/// Runs `cmd` with stdin inherited and stdout/stderr piped, forwarding each line to `ctx` as it
/// arrives and keeping the full output.
///
/// Returns once the command has exited and its pipes are drained, or [`EXIT_GRACE`] after it
/// exited if a background process it started (`server &`) still holds the pipes open. Readers
/// still blocked then are detached and a note is added to the output.
pub fn run_captured(mut cmd: Command, ctx: &RunCtx) -> Result<Captured> {
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("starting command")?;
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let readers = [
        spawn_reader(stdout, Stream::Stdout, tx.clone()),
        spawn_reader(stderr, Stream::Stderr, tx),
    ];
    let mut lines = Vec::new();
    let mut exited: Option<(ExitStatus, Instant)> = None;
    let mut detached = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok((stream, line)) => {
                ctx.output(stream, line.clone());
                lines.push((stream, line));
            }
            // Both pipes closed: everything has been read.
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        match exited {
            None => {
                if let Some(st) = child.try_wait().context("waiting for command")? {
                    exited = Some((st, Instant::now()));
                }
            }
            Some((_, at)) if at.elapsed() >= EXIT_GRACE => {
                detached = true;
                break;
            }
            Some(_) => {}
        }
    }
    let status = match exited {
        Some((st, _)) => st,
        None => child.wait().context("waiting for command")?,
    };
    if detached {
        let note = "[dot] the command exited but background processes it started still hold \
                    its output open; not waiting for them, and their later output is not \
                    captured";
        ctx.output(Stream::Stderr, note);
        lines.push((Stream::Stderr, note.to_string()));
        // The readers end on their own when the background processes close the pipes.
        drop(readers);
    } else {
        for r in readers {
            let _ = r.join();
        }
    }
    Ok(Captured { status, lines })
}

fn spawn_reader<R: Read + Send + 'static>(
    r: R,
    stream: Stream,
    tx: mpsc::Sender<(Stream, String)>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(r);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    while matches!(buf.last(), Some(b'\n' | b'\r')) {
                        buf.pop();
                    }
                    let line = String::from_utf8_lossy(&buf).into_owned();
                    if tx.send((stream, line)).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

fn describe_exit(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exited with code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("was killed by signal {sig}");
        }
    }
    format!("failed ({status})")
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;

    use dot_core::Event;

    use super::*;
    use crate::testutil::Fixture;

    fn output(f: &Fixture) -> Vec<(Stream, String)> {
        f.recorder
            .events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Output { stream, line, .. } => Some((stream, line)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn success_streams_output() {
        let f = Fixture::new(
            "steps:\n  - shell: |\n      echo one\n      echo two >&2\n      printf 'no newline'\n",
        );
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].describe(), "run `echo one ...`");
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        let out = output(&f);
        assert!(out.contains(&(Stream::Stdout, "one".into())), "{out:?}");
        assert!(out.contains(&(Stream::Stderr, "two".into())), "{out:?}");
        assert!(
            out.contains(&(Stream::Stdout, "no newline".into())),
            "{out:?}"
        );
    }

    #[test]
    fn failure_reports_exit_code_and_keeps_output() {
        let f = Fixture::new("steps:\n  - shell: echo about to fail; exit 3\n");
        let actions = f.plan(0).unwrap();
        let err = f.apply_err(actions[0].as_ref());
        assert_eq!(err, "command exited with code 3");
        assert_eq!(output(&f), [(Stream::Stdout, "about to fail".into())]);
    }

    #[test]
    fn run_captured_retains_all_lines() {
        let f = Fixture::new("steps: []\n");
        let ctx = RunCtx::new(&f.sink(), dot_core::StepId { bundle: 0, step: 0 });
        let cmd = Interpreter::Sh.command("for i in $(seq 1 500); do echo line$i; done; exit 1");
        let captured = run_captured(cmd, &ctx).unwrap();
        assert!(!captured.status.success());
        assert_eq!(captured.lines.len(), 500);
        assert_eq!(captured.lines[499].1, "line500");
        drop(f);
    }

    #[test]
    fn unless_success_means_satisfied() {
        let f = Fixture::new("steps:\n  - shell: { cmd: touch ran, unless: test -e marker }\n");
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert!(actions[0].describe().contains("unless `test -e marker`"));
        fs::write(f.repo().join("marker"), "").unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
        assert!(!f.repo().join("ran").exists());
    }

    #[test]
    fn upgrade_runs_only_when_upgrading_and_installed() {
        let f = Fixture::new(
            "steps:\n  - shell: { cmd: echo x >> installed, unless: test -e installed, upgrade: echo u >> upgraded }\n",
        );
        let builtins = |upgrade: bool| crate::Builtins {
            upgrade,
            ..crate::Builtins::default()
        };
        // Not installed: the install command, upgrading or not.
        let actions = f.plan_by(0, &f.context(), &builtins(true)).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert_eq!(
            actions[0].describe(),
            "run `echo x >> installed` unless `test -e installed` succeeds, else (upgrading) \
             `echo u >> upgraded`"
        );
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        assert!(f.repo().join("installed").exists());
        assert!(!f.repo().join("upgraded").exists());
        // Installed: satisfied normally...
        let actions = f.plan_by(0, &f.context(), &builtins(false)).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
        assert!(!actions[0].describe().contains("upgrading"));
        // ...and the upgrade command under --upgrade.
        let actions = f.plan_by(0, &f.context(), &builtins(true)).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        assert_eq!(
            fs::read_to_string(f.repo().join("upgraded")).unwrap(),
            "u\n"
        );
        assert_eq!(
            fs::read_to_string(f.repo().join("installed")).unwrap(),
            "x\n"
        );

        // `upgrade` without `unless` would run every time anyway: an error.
        let g = f.with_bundle("steps:\n  - shell: { cmd: 'true', upgrade: 'true' }\n");
        let e = format!("{:#}", g.plan(0).err().unwrap());
        assert!(e.contains("`upgrade` needs `unless`"), "{e}");
    }

    #[test]
    fn sudo_steps_ask_for_sudo_up_front_unless_satisfied() {
        let f = Fixture::new(concat!(
            "steps:\n",
            "  - shell: { cmd: 'true', sudo: true }\n",
            "  - shell: 'true'\n",
        ));
        let sudo: Vec<bool> = (0..2).map(|i| f.plan(i).unwrap()[0].needs_sudo()).collect();
        assert_eq!(sudo, [true, false]);
        let plan = dot_core::engine::plan(
            std::slice::from_ref(&f.bundle),
            &crate::Builtins::default(),
            &f.context(),
        );
        assert_eq!(plan.sudo_bundles(), ["demo"]);

        // Already satisfied (`unless` succeeds): nothing to ask for.
        let f = Fixture::new("steps:\n  - shell: { cmd: 'true', sudo: true, unless: 'true' }\n");
        let plan = dot_core::engine::plan(
            std::slice::from_ref(&f.bundle),
            &crate::Builtins::default(),
            &f.context(),
        );
        assert!(plan.sudo_bundles().is_empty());
    }

    #[test]
    fn cwd_and_env() {
        let f = Fixture::new(
            "steps:\n  - shell:\n      cmd: echo \"$GREETING\" > out.txt; pwd >> out.txt\n      cwd: ~/work\n      env: { GREETING: hello there }\n",
        );
        let work = f.home().join("work");
        fs::create_dir(&work).unwrap();
        let actions = f.plan(0).unwrap();
        f.apply(&actions);
        let text = fs::read_to_string(work.join("out.txt")).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("hello there"));
        assert_eq!(
            PathBuf::from(lines.next().unwrap()).canonicalize().unwrap(),
            work.canonicalize().unwrap()
        );
    }

    #[test]
    fn default_cwd_is_bundle_dir() {
        let f = Fixture::new("steps:\n  - shell: touch here\n");
        f.apply(&f.plan(0).unwrap());
        assert!(f.repo().join("here").exists());
    }

    #[test]
    fn missing_cwd_fails_before_running() {
        let f = Fixture::new("steps:\n  - shell: { cmd: 'true', cwd: ~/nope }\n");
        let err = f.apply_err(f.plan(0).unwrap()[0].as_ref());
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn missing_cwd_makes_unless_unknown_not_an_error() {
        // An earlier `create` step may make the directory.
        let f = Fixture::new(
            "steps:\n  - shell: { cmd: touch made, cwd: ~/src, unless: test -e made }\n",
        );
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Unknown);
        assert!(actions[0].describe().contains("does not exist yet"));
        fs::create_dir(f.home().join("src")).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        f.apply(&actions);
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    }

    #[test]
    fn background_children_do_not_block_the_step() {
        let f = Fixture::new("steps:\n  - shell: '(sleep 5 &) ; echo started'\n");
        let started = std::time::Instant::now();
        assert_eq!(f.apply(&f.plan(0).unwrap()), [Outcome::Changed]);
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(3), "took {took:?}");
        let out = output(&f);
        assert!(out.contains(&(Stream::Stdout, "started".into())), "{out:?}");
        assert!(
            out.iter().any(|(_, l)| l.contains("background processes")),
            "{out:?}"
        );
    }

    #[test]
    fn quick_commands_are_not_cut_short() {
        // Output that arrives after exit but before the pipes close is kept, and no note.
        let f = Fixture::new("steps:\n  - shell: 'for i in 1 2 3; do echo $i; done'\n");
        f.apply(&f.plan(0).unwrap());
        let out = output(&f);
        assert_eq!(out.len(), 3, "{out:?}");
    }

    #[test]
    fn templates_render_into_command_env_and_cwd() {
        let f = Fixture::new(
            "steps:\n  - shell:\n      cmd: echo \"{{ os }} $X\" > out\n      cwd: \"{{ home }}\"\n      env: { X: \"{{ arch }}\" }\n",
        );
        f.apply(&f.plan(0).unwrap());
        assert_eq!(
            fs::read_to_string(f.home().join("out")).unwrap().trim(),
            "linux x86_64"
        );
        let f = Fixture::new("steps:\n  - shell: echo {{ nope }}\n");
        let err = format!("{:#}", f.plan(0).err().unwrap());
        assert!(
            err.contains("bundle.yml:2:5: steps[0]: undefined variable `nope`"),
            "{err}"
        );
    }

    /// Plans step `step` of `f` on a machine with `os`, the platform default being `default`.
    fn plan_on(
        f: &Fixture,
        step: usize,
        os: dot_core::facts::Os,
        default: Interpreter,
    ) -> Result<Vec<Box<dyn Action>>> {
        let spec = match &f.bundle.steps[step].directive {
            dot_core::Directive::Shell(s) => s.clone(),
            _ => unreachable!(),
        };
        let mut ctx = f.context();
        ctx.facts.os = os;
        let env = dot_core::BundleEnv::new(&ctx, &f.bundle);
        let secrets = dot_core::SecretRefs::default();
        let pctx = PlanCtx::for_bundle(
            &ctx,
            &env,
            &f.bundle,
            &f.bundle.steps[step],
            &secrets,
            false,
        );
        plan(&spec, &pctx, default, false)
    }

    #[test]
    fn a_prelude_runs_before_cmd_unless_and_upgrade() {
        let f = Fixture::new(concat!(
            "steps:\n",
            "  - shell:\n",
            "      prelude: 'greet() { echo \"hi $1\" >> out; }'\n",
            "      cmd: greet cmd\n",
            "      unless: 'greet unless; false'\n",
        ));
        let a = f.plan(0).unwrap();
        assert_eq!(
            a[0].describe(),
            "run `greet cmd` unless `greet unless; false` succeeds"
        );
        assert_eq!(a[0].status().unwrap(), Status::Pending);
        f.apply(&a);
        assert_eq!(
            fs::read_to_string(f.repo().join("out")).unwrap(),
            "hi unless\nhi cmd\n"
        );
    }

    #[test]
    fn an_unless_whose_interpreter_is_missing_is_unknown_not_an_error() {
        // A plan for Windows from a Unix host, or `pwsh` installed by an earlier step: the
        // check cannot run yet.
        let f = Fixture::new(concat!(
            "steps:\n",
            "  - shell: { cmd: 'echo hi', unless: 'exit 0', interpreter: bash,\n",
            "             env: { PATH: /nonexistent-dot-test } }\n",
        ));
        let a = f.plan(0).unwrap();
        assert_eq!(a[0].status().unwrap(), Status::Unknown);
        assert!(
            a[0].describe()
                .ends_with("(bash is not installed yet: checked when applied)"),
            "{}",
            a[0].describe()
        );
    }

    #[test]
    fn cmd_interpreter_rejects_multi_line_scripts_at_plan_time() {
        use dot_core::facts::Os;
        let f = Fixture::new("steps:\n  - shell: |\n      mkdir foo\n      copy a b\n");
        let err = format!(
            "{:#}",
            plan_on(&f, 0, Os::Windows, Interpreter::Cmd).err().unwrap()
        );
        assert!(err.contains("first line"), "{err}");
        assert!(plan_on(&f, 0, Os::Linux, Interpreter::Sh).is_ok());
        // PowerShell, the Windows default, takes multi-line scripts.
        assert!(plan_on(&f, 0, Os::Windows, Interpreter::Pwsh).is_ok());
        assert!(plan_on(&f, 0, Os::Windows, Interpreter::PowerShell).is_ok());
    }

    #[test]
    fn the_step_then_the_defaults_then_the_platform_pick_the_interpreter() {
        use dot_core::facts::Os;
        let f = Fixture::new(concat!(
            "defaults: { shell: { interpreter: bash } }\n",
            "steps:\n",
            "  - shell: echo \"$0\" > which-default\n",
            "  - shell: { cmd: 'echo \"$0\" > which-step', interpreter: sh }\n",
            "  - shell: { cmd: dir, interpreter: cmd }\n",
            "  - shell: { cmd: Get-ChildItem, interpreter: pwsh }\n",
        ));
        let bundle_default = plan_on(&f, 0, Os::Linux, Interpreter::Sh).unwrap();
        assert_eq!(
            bundle_default[0].describe(),
            "run `echo \"$0\" > which-default` with bash"
        );
        f.apply(&bundle_default);
        assert_eq!(
            fs::read_to_string(f.repo().join("which-default"))
                .unwrap()
                .trim(),
            "bash"
        );
        let step = plan_on(&f, 1, Os::Linux, Interpreter::Sh).unwrap();
        f.apply(&step);
        assert_eq!(
            fs::read_to_string(f.repo().join("which-step"))
                .unwrap()
                .trim(),
            "sh"
        );
        // Windows-only interpreters are refused elsewhere, at plan time.
        let err = format!(
            "{:#}",
            plan_on(&f, 2, Os::Linux, Interpreter::Sh).err().unwrap()
        );
        assert!(
            err.contains("`cmd` only exists on Windows (this is linux)"),
            "{err}"
        );
        assert!(err.contains("bundle.yml:5:5"), "{err}");
        assert!(plan_on(&f, 2, Os::Windows, Interpreter::Pwsh).is_ok());
        // pwsh exists on macOS and Linux too.
        let a = plan_on(&f, 3, Os::Macos, Interpreter::Sh).unwrap();
        assert_eq!(a[0].describe(), "run `Get-ChildItem` with pwsh");
        // Without any choice: the platform's, not named in the plan.
        let g = Fixture::new("steps:\n  - shell: echo hi\n");
        let a = plan_on(&g, 0, Os::Windows, Interpreter::Pwsh).unwrap();
        assert_eq!(a[0].describe(), "run `echo hi`");
    }

    #[test]
    fn declared_vars_are_exported_but_sensitive_ones_only_on_request() {
        let f = Fixture::new(
            "vars:\n  greeting: hello\n  api_token: s3cret\nsteps:\n  - shell: echo \"$greeting/$api_token/$os\" > out\n  - shell:\n      cmd: echo \"$api_token $greeting\" > out2\n      export_sensitive: [api_token]\n      env: { greeting: override }\n  - shell: { cmd: 'true', export_sensitive: [nope] }\n",
        );
        let mut ctx = f.context();
        ctx.vars = ctx.vars.with_bundle(&f.bundle);
        f.apply(&f.plan_with(0, &ctx).unwrap());
        assert_eq!(
            fs::read_to_string(f.repo().join("out")).unwrap().trim(),
            "hello//"
        );
        f.apply(&f.plan_with(1, &ctx).unwrap());
        assert_eq!(
            fs::read_to_string(f.repo().join("out2")).unwrap().trim(),
            "s3cret override"
        );
        let err = format!("{:#}", f.plan_with(2, &ctx).err().unwrap());
        assert!(
            err.contains("export_sensitive: `nope` is not a variable"),
            "{err}"
        );
    }

    #[test]
    fn unless_using_a_secret_is_unknown_until_resolved() {
        let f = Fixture::new(
            "steps:\n  - shell: { cmd: 'true', unless: \"test {{ secret('tok') }} = x\" }\n",
        );
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Unknown);
        assert!(actions[0].describe().contains("<secret:tok>"));
    }

    /// Plans step `step` as the engine does right before applying it, with `secrets` resolved.
    fn plan_applying(f: &Fixture, step: usize, secrets: &[(&str, &str)]) -> Vec<Box<dyn Action>> {
        let cache = std::sync::Arc::new(dot_core::SecretCache::default());
        for (k, v) in secrets {
            cache.insert(*k, *v);
        }
        let mut ctx = f.context();
        ctx.secrets = Some(cache);
        let env = dot_core::BundleEnv::new(&ctx, &f.bundle);
        let refs = dot_core::SecretRefs::default();
        let step_ref = &f.bundle.steps[step];
        let pctx = PlanCtx::for_bundle(&ctx, &env, &f.bundle, step_ref, &refs, true);
        use dot_core::Planner;
        crate::Builtins::default()
            .plan(&step_ref.directive, &pctx)
            .unwrap()
    }

    #[test]
    fn secrets_reach_the_command_through_its_environment_not_its_argv() {
        const S: &str = "supersecretvalue123";
        let f = Fixture::new(concat!(
            "steps:\n",
            "  - shell:\n",
            "      cmd: |\n",
            "        ps -o args= -p $$ > argv\n",
            "        cat /proc/$$/cmdline >> argv 2>/dev/null || true\n",
            "        echo \"{{ secret('tok') }}\" > value\n",
            "        echo {{ secret('tok') }}-{{ secret('other') }} >> value\n",
            "      unless: test \"{{ secret('other') }}\" = never\n",
        ));
        let actions = plan_applying(&f, 0, &[("tok", S), ("other", "o-2")]);
        let d = actions[0].describe();
        assert!(
            d.contains("unless `test \"<secret:other>\" = never`"),
            "{d}"
        );
        assert!(!d.contains(S), "{d}");
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        f.apply(&actions);
        let argv = fs::read_to_string(f.repo().join("argv")).unwrap();
        assert!(argv.contains("DOT_SECRET_tok"), "{argv}");
        assert!(!argv.contains(S), "secret in argv: {argv}");
        assert_eq!(
            fs::read_to_string(f.repo().join("value")).unwrap(),
            format!("{S}\n{S}-o-2\n")
        );
    }

    #[test]
    fn a_filtered_secret_in_a_command_is_an_error() {
        let f = Fixture::new("steps:\n  - shell: echo {{ secret('tok') | upper }}\n");
        let cache = std::sync::Arc::new(dot_core::SecretCache::default());
        cache.insert("tok", "abc");
        let mut ctx = f.context();
        ctx.secrets = Some(cache);
        let env = dot_core::BundleEnv::new(&ctx, &f.bundle);
        let refs = dot_core::SecretRefs::default();
        let pctx = PlanCtx::for_bundle(&ctx, &env, &f.bundle, &f.bundle.steps[0], &refs, true);
        use dot_core::Planner;
        let err = format!(
            "{:#}",
            crate::Builtins::default()
                .plan(&f.bundle.steps[0].directive, &pctx)
                .err()
                .unwrap()
        );
        assert!(err.contains("must be used as is"), "{err}");
    }

    #[test]
    fn interactive_brackets_with_tty_events() {
        let f = Fixture::new("steps:\n  - shell: { cmd: 'true', interactive: true }\n");
        f.apply(&f.plan(0).unwrap());
        let kinds: Vec<&str> = f
            .recorder
            .events()
            .iter()
            .map(|e| match e {
                Event::NeedsTty { .. } => "needs",
                Event::TtyReleased { .. } => "released",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["needs", "released"]);
    }
}
