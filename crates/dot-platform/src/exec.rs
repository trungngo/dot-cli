//! Running external programs (package managers, `sudo`, `curl`, `defaults`, `systemctl`,
//! `launchctl`, `sc.exe`) through an injectable [`Runner`], so tests assert the exact argv and
//! script the answers ([`fake::FakeRunner`], [`fake::FakeExec`]) instead of running real package
//! managers, service managers or `sudo`. [`System`] adds who `dot` runs as, for commands that
//! need root.

use std::io::{self, BufRead, BufReader, Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use dot_core::Stream;
pub use dot_core::paths::{DEFAULT_PATHEXT, exe_candidates};

/// A program and its arguments, plus extra environment variables. Never goes through a shell.
/// `Debug` shows the size of [`Cmd::stdin`], never its bytes (they may be secrets).
#[derive(Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// Added to the inherited environment.
    pub env: Vec<(String, String)>,
    /// Written to stdin by [`Runner::output`]; stdin is otherwise closed.
    pub stdin: Option<Vec<u8>>,
}

impl std::fmt::Debug for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        struct Bytes(usize);
        impl std::fmt::Debug for Bytes {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "<{} bytes>", self.0)
            }
        }
        f.debug_struct("Cmd")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &self.env)
            .field("stdin", &self.stdin.as_ref().map(|b| Bytes(b.len())))
            .finish()
    }
}

impl Cmd {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            stdin: None,
        }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    pub fn envs(mut self, env: impl IntoIterator<Item = (String, String)>) -> Self {
        self.env.extend(env);
        self
    }

    /// Program and arguments.
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }

    /// The command line for messages and test assertions, arguments quoted when a shell would
    /// need them to be.
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(quote)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Same as [`Cmd::display`].
    pub fn line(&self) -> String {
        self.display()
    }

    /// This command run as root: unchanged when `is_root`, else through `sudo -n` (never
    /// prompting: the run primed sudo up front; see [`crate::sudo`]). The environment is passed
    /// through `env`, because sudo resets it.
    pub fn as_root(self, is_root: bool) -> Cmd {
        if is_root {
            return self;
        }
        let mut c = Cmd::new("sudo").arg("-n");
        if !self.env.is_empty() {
            c = c.arg("env");
            for (k, v) in &self.env {
                c = c.arg(format!("{k}={v}"));
            }
        }
        let mut c = c.arg(self.program).args(self.args);
        c.stdin = self.stdin;
        c
    }
}

fn quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// What a finished command left: its exit code (`None` when killed by a signal) and its output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// Exit 0 with this stdout.
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self::exit(0, stdout)
    }

    /// Exit `code` with this stdout (some tools print their answer and still fail).
    pub fn exit(code: i32, stdout: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// An error for a command that failed: its exit code and what it said.
    pub fn failure(&self, cmd: &Cmd) -> anyhow::Error {
        let said = [self.stderr.trim(), self.stdout.trim()]
            .into_iter()
            .find(|s| !s.is_empty())
            .map(|s| format!(": {s}"))
            .unwrap_or_default();
        match self.code {
            Some(c) => anyhow::anyhow!("`{}` exited with code {c}{said}", cmd.line()),
            None => anyhow::anyhow!("`{}` was killed{said}", cmd.line()),
        }
    }

    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// `exited with code N` / `was killed by a signal`.
    pub fn describe_exit(&self) -> String {
        match self.code {
            Some(c) => format!("exited with code {c}"),
            None => "was killed by a signal".to_string(),
        }
    }

    /// The last non-empty line of stderr (else stdout), for error messages.
    pub fn last_line(&self) -> Option<&str> {
        [&self.stderr, &self.stdout]
            .into_iter()
            .find_map(|s| s.lines().rev().map(str::trim).find(|l| !l.is_empty()))
    }
}

/// Where a line of streamed output goes.
pub type OutFn<'a> = dyn FnMut(Stream, &str) + 'a;

/// How platform code runs programs and asks about the host. The real one is [`SystemRunner`].
pub trait Runner: Send + Sync {
    /// Runs a command to completion: output captured, stdin closed (or fed [`Cmd::stdin`]).
    /// `Err` only when it could not be started (`NotFound` when the program does not exist); a
    /// non-zero exit is an `Ok` with that code.
    fn output(&self, cmd: &Cmd) -> io::Result<Output>;

    /// Runs a command whose output the user should see: stdin closed, each line passed to `out`
    /// as it arrives, and also returned.
    fn stream(&self, cmd: &Cmd, out: &mut OutFn<'_>) -> io::Result<Output>;

    /// Runs a command on the real terminal (stdin, stdout and stderr inherited), for prompts
    /// such as `sudo -v`. Returns the exit code. With [`Cmd::stdin`] set, stdin is a pipe fed
    /// those bytes and then closed instead (output still goes straight to the terminal).
    fn interactive(&self, cmd: &Cmd) -> io::Result<Option<i32>>;

    /// Whether `program` is an executable on `PATH`.
    fn on_path(&self, program: &str) -> bool;

    fn exists(&self, path: &Path) -> bool;

    /// Whether `dot` runs as root (Unix) or elevated (Windows, which has no sudo: an elevated
    /// terminal is the equivalent).
    fn is_root(&self) -> bool;
}

/// The machine `dot` runs on.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

/// How long output is still collected after a command has exited: longer means a background
/// process it started holds the pipes open, and is not waited for.
const EXIT_GRACE: Duration = Duration::from_millis(500);

impl SystemRunner {
    fn command(&self, cmd: &Cmd) -> Command {
        let program = which(&cmd.program).unwrap_or_else(|| PathBuf::from(&cmd.program));
        let mut c = Command::new(program);
        c.args(&cmd.args);
        c.envs(cmd.env.iter().map(|(k, v)| (k, v)));
        c
    }
}

impl Runner for SystemRunner {
    fn output(&self, cmd: &Cmd) -> io::Result<Output> {
        let mut c = self.command(cmd);
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
        c.stdin(if cmd.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = c.spawn()?;
        if let (Some(bytes), Some(mut stdin)) = (&cmd.stdin, child.stdin.take()) {
            // A command that exits without reading its input is not an error here.
            let _ = stdin.write_all(bytes);
        }
        let out = child.wait_with_output()?;
        Ok(Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn stream(&self, cmd: &Cmd, out: &mut OutFn<'_>) -> io::Result<Output> {
        let mut child = self
            .command(cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (tx, rx) = mpsc::channel();
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let readers = [
            spawn_reader(stdout, Stream::Stdout, tx.clone()),
            spawn_reader(stderr, Stream::Stderr, tx),
        ];
        let mut result = Output::default();
        let mut exited: Option<(Option<i32>, Instant)> = None;
        let mut detached = false;
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok((stream, line)) => {
                    out(stream, &line);
                    let buf = match stream {
                        Stream::Stdout => &mut result.stdout,
                        Stream::Stderr => &mut result.stderr,
                    };
                    buf.push_str(&line);
                    buf.push('\n');
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            match exited {
                None => {
                    if let Some(st) = child.try_wait()? {
                        exited = Some((st.code(), Instant::now()));
                    }
                }
                Some((_, at)) if at.elapsed() >= EXIT_GRACE => {
                    detached = true;
                    break;
                }
                Some(_) => {}
            }
        }
        result.code = match exited {
            Some((code, _)) => code,
            None => child.wait()?.code(),
        };
        if !detached {
            for r in readers {
                let _ = r.join();
            }
        }
        Ok(result)
    }

    fn interactive(&self, cmd: &Cmd) -> io::Result<Option<i32>> {
        let mut c = self.command(cmd);
        c.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        let Some(bytes) = &cmd.stdin else {
            return Ok(c.stdin(Stdio::inherit()).status()?.code());
        };
        let mut child = c.stdin(Stdio::piped()).spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            // A command that exits without reading its input is not an error here.
            let _ = stdin.write_all(bytes);
        }
        Ok(child.wait()?.code())
    }

    fn on_path(&self, program: &str) -> bool {
        which(program).is_some()
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_root(&self) -> bool {
        #[cfg(unix)]
        {
            // SAFETY: geteuid has no preconditions and cannot fail.
            unsafe { libc::geteuid() == 0 }
        }
        #[cfg(not(unix))]
        {
            crate::elevation::is_elevated()
        }
    }
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

/// The executable `program` resolves to on `PATH` (with `PATHEXT` on Windows, so `scoop` finds
/// `scoop.cmd`). A program given as a path is returned when it exists. On a thread planning or
/// applying a bundle, `PATH` is that bundle's run environment's ([`dot_core::runenv::current_path`]).
pub fn which(program: &str) -> Option<PathBuf> {
    // Inside a bundle, its run environment's PATH (what `path:` steps added), else this process's.
    let path = dot_core::runenv::current_path().or_else(|| std::env::var_os("PATH"));
    which_in(program, path.as_deref())
}

/// [`which`] over `path` (a `PATH` value) instead of this process's.
pub fn which_in(program: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.components().count() > 1 {
        return is_executable(p).then(|| p.to_path_buf());
    }
    let path = path?;
    let pathext = std::env::var("PATHEXT").ok();
    let names = exe_candidates(program, pathext.as_deref(), cfg!(windows));
    std::env::split_paths(&path).find_map(|dir| {
        names.iter().find_map(|name| {
            let candidate = dir.join(name);
            is_executable(&candidate).then_some(candidate)
        })
    })
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file())
}

/// Who `dot` runs as, as far as commands that change system state care, plus the [`Runner`]
/// that runs them.
#[derive(Clone)]
pub struct System {
    pub runner: Arc<dyn Runner>,
    /// Login name (`$USER`), for `loginctl` and messages.
    pub user: Option<String>,
    /// Numeric user id (Unix), for launchd's `gui/<uid>` domain.
    pub uid: Option<u32>,
    /// Running as root (Windows: elevated): privileged commands run directly instead of
    /// through `sudo`, and nothing needs asking for up front.
    pub root: bool,
}

impl std::fmt::Debug for System {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("System")
            .field("user", &self.user)
            .field("uid", &self.uid)
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Default for System {
    /// This process's user, running commands for real.
    fn default() -> Self {
        Self::host(Arc::new(SystemRunner))
    }
}

impl System {
    /// This process's user, with commands run by `runner`.
    pub fn host(runner: Arc<dyn Runner>) -> Self {
        let user = ["USER", "LOGNAME", "USERNAME"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
        #[cfg(unix)]
        // SAFETY: getuid and geteuid cannot fail and have no preconditions.
        let (uid, root) = unsafe { (Some(libc::getuid()), libc::geteuid() == 0) };
        #[cfg(not(unix))]
        let (uid, root) = (None, crate::elevation::is_elevated());
        Self {
            runner,
            user,
            uid,
            root,
        }
    }

    /// A fixed identity for tests: `user` (uid 501), not root.
    pub fn fake(runner: Arc<dyn Runner>) -> Self {
        Self {
            runner,
            user: Some("user".into()),
            uid: Some(501),
            root: false,
        }
    }

    /// `cmd` as it must run to change system state: directly as root (and on Windows, where an
    /// elevated console is the equivalent), otherwise through `sudo -n` ([`Cmd::as_root`]),
    /// which never prompts: the run asked for sudo up front because the action said
    /// [`dot_core::Action::needs_sudo`].
    pub fn privileged(&self, cmd: Cmd) -> Cmd {
        if cfg!(windows) {
            return cmd;
        }
        cmd.as_root(self.root)
    }

    /// Runs `cmd` to completion ([`Runner::output`]).
    pub fn run(&self, cmd: &Cmd) -> io::Result<Output> {
        self.runner.output(cmd)
    }
}

/// Scripted [`Runner`]s for tests: they record every command and answer from rules.
pub mod fake {
    use std::collections::BTreeSet;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use dot_core::Stream;

    use super::{Cmd, OutFn, Output, Runner};
    use std::sync::{Arc, MutexGuard};

    /// How a command was run.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mode {
        Output,
        Stream,
        Interactive,
    }

    /// One recorded command.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Call {
        pub mode: Mode,
        pub cmd: Cmd,
    }

    impl Call {
        /// `program arg arg ...`, with `env` variables as `K=V` up front.
        pub fn line(&self) -> String {
            let mut parts: Vec<String> = self
                .cmd
                .env
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            parts.extend(self.cmd.argv());
            parts.join(" ")
        }
    }

    struct Rule {
        prefix: Vec<String>,
        output: Output,
        /// `None`: for ever.
        remaining: Option<usize>,
    }

    /// Commands whose argv starts with a rule's prefix get that rule's output (the most recently
    /// added matching rule wins; a rule added with [`FakeRunner::once`] answers only once).
    /// Anything else succeeds with no output, unless its program is not on the fake `PATH`, which
    /// fails with `NotFound`.
    #[derive(Default)]
    pub struct FakeRunner {
        calls: Mutex<Vec<Call>>,
        rules: Mutex<Vec<Rule>>,
        path: Mutex<BTreeSet<String>>,
        files: Mutex<BTreeSet<PathBuf>>,
        root: AtomicBool,
    }

    impl FakeRunner {
        /// A runner with `programs` on its `PATH`.
        pub fn with_programs(programs: &[&str]) -> Self {
            let r = Self::default();
            for p in programs {
                r.add_program(p);
            }
            r
        }

        pub fn add_program(&self, p: &str) {
            self.path.lock().unwrap().insert(p.to_string());
        }

        pub fn remove_program(&self, p: &str) {
            self.path.lock().unwrap().remove(p);
        }

        pub fn add_file(&self, p: impl Into<PathBuf>) {
            self.files.lock().unwrap().insert(p.into());
        }

        pub fn set_root(&self, root: bool) {
            self.root.store(root, Ordering::SeqCst);
        }

        fn rule(&self, prefix: &str, code: i32, stdout: &str, stderr: &str, n: Option<usize>) {
            self.rules.lock().unwrap().push(Rule {
                prefix: prefix.split_whitespace().map(str::to_string).collect(),
                output: Output {
                    code: Some(code),
                    stdout: stdout.to_string(),
                    stderr: stderr.to_string(),
                },
                remaining: n,
            });
        }

        /// Commands starting with `prefix` (words split on whitespace) exit `code` printing
        /// `stdout`.
        pub fn on(&self, prefix: &str, code: i32, stdout: &str) -> &Self {
            self.rule(prefix, code, stdout, "", None);
            self
        }

        /// Like [`FakeRunner::on`], with stderr.
        pub fn on_err(&self, prefix: &str, code: i32, stdout: &str, stderr: &str) -> &Self {
            self.rule(prefix, code, stdout, stderr, None);
            self
        }

        /// Like [`FakeRunner::on_err`], for the next matching command only.
        pub fn once(&self, prefix: &str, code: i32, stdout: &str, stderr: &str) -> &Self {
            self.rule(prefix, code, stdout, stderr, Some(1));
            self
        }

        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        /// Every recorded command as [`Call::line`].
        pub fn lines(&self) -> Vec<String> {
            self.calls().iter().map(Call::line).collect()
        }

        /// The recorded commands run with `mode`.
        pub fn lines_of(&self, mode: Mode) -> Vec<String> {
            self.calls()
                .iter()
                .filter(|c| c.mode == mode)
                .map(Call::line)
                .collect()
        }

        pub fn clear_calls(&self) {
            self.calls.lock().unwrap().clear();
        }

        fn answer(&self, mode: Mode, cmd: &Cmd) -> io::Result<Output> {
            self.calls.lock().unwrap().push(Call {
                mode,
                cmd: cmd.clone(),
            });
            if !self.on_path(&cmd.program) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{}: not found", cmd.program),
                ));
            }
            let argv = cmd.argv();
            let mut rules = self.rules.lock().unwrap();
            let hit = rules.iter_mut().rev().find(|r| {
                r.remaining != Some(0)
                    && r.prefix.len() <= argv.len()
                    && r.prefix.iter().zip(&argv).all(|(a, b)| a == b)
            });
            Ok(match hit {
                Some(r) => {
                    if let Some(n) = &mut r.remaining {
                        *n -= 1;
                    }
                    r.output.clone()
                }
                None => Output {
                    code: Some(0),
                    ..Output::default()
                },
            })
        }
    }

    impl Runner for FakeRunner {
        fn output(&self, cmd: &Cmd) -> io::Result<Output> {
            self.answer(Mode::Output, cmd)
        }

        fn stream(&self, cmd: &Cmd, out: &mut OutFn<'_>) -> io::Result<Output> {
            let o = self.answer(Mode::Stream, cmd)?;
            for l in o.stdout.lines() {
                out(Stream::Stdout, l);
            }
            for l in o.stderr.lines() {
                out(Stream::Stderr, l);
            }
            Ok(o)
        }

        fn interactive(&self, cmd: &Cmd) -> io::Result<Option<i32>> {
            self.answer(Mode::Interactive, cmd).map(|o| o.code)
        }

        fn on_path(&self, program: &str) -> bool {
            self.path.lock().unwrap().contains(program)
        }

        fn exists(&self, path: &Path) -> bool {
            self.files.lock().unwrap().contains(path)
        }

        fn is_root(&self) -> bool {
            self.root.load(Ordering::SeqCst)
        }
    }

    type ExecRule = (String, Result<Output, io::ErrorKind>);

    /// A scripted [`Runner`] matched on the start of the quoted command line ([`Cmd::line`]):
    /// the most recently added matching rule wins, so a test can change the "machine" as it
    /// goes. Unmatched commands succeed with no output; every program is on its `PATH` unless a
    /// rule says it cannot be started.
    #[derive(Default)]
    pub struct FakeExec {
        rules: Mutex<Vec<ExecRule>>,
        calls: Mutex<Vec<Cmd>>,
    }

    impl FakeExec {
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        /// Commands whose line starts with `prefix` answer `out`.
        pub fn on(&self, prefix: &str, out: Output) {
            self.rules().push((prefix.to_string(), Ok(out)));
        }

        /// Commands whose line starts with `prefix` cannot be started (e.g. `NotFound`).
        pub fn fail_to_start(&self, prefix: &str, kind: io::ErrorKind) {
            self.rules().push((prefix.to_string(), Err(kind)));
        }

        /// Every command line run so far, in order.
        pub fn calls(&self) -> Vec<String> {
            self.commands().iter().map(Cmd::line).collect()
        }

        pub fn commands(&self) -> Vec<Cmd> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        pub fn clear_calls(&self) {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }

        fn rules(&self) -> MutexGuard<'_, Vec<ExecRule>> {
            self.rules.lock().unwrap_or_else(|e| e.into_inner())
        }

        fn answer(&self, line: &str) -> Option<Result<Output, io::ErrorKind>> {
            self.rules()
                .iter()
                .rev()
                .find(|(p, _)| line.starts_with(p.as_str()))
                .map(|(_, r)| r.clone())
        }

        fn run(&self, cmd: &Cmd) -> io::Result<Output> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(cmd.clone());
            let line = cmd.line();
            match self.answer(&line) {
                Some(Ok(out)) => Ok(out),
                Some(Err(kind)) => Err(io::Error::new(kind, format!("fake: {line}"))),
                None => Ok(Output::ok("")),
            }
        }
    }

    impl Runner for FakeExec {
        fn output(&self, cmd: &Cmd) -> io::Result<Output> {
            self.run(cmd)
        }

        fn stream(&self, cmd: &Cmd, out: &mut OutFn<'_>) -> io::Result<Output> {
            let o = self.run(cmd)?;
            for l in o.stdout.lines() {
                out(Stream::Stdout, l);
            }
            for l in o.stderr.lines() {
                out(Stream::Stderr, l);
            }
            Ok(o)
        }

        fn interactive(&self, cmd: &Cmd) -> io::Result<Option<i32>> {
            self.run(cmd).map(|o| o.code)
        }

        fn on_path(&self, program: &str) -> bool {
            !matches!(self.answer(program), Some(Err(io::ErrorKind::NotFound)))
        }

        fn exists(&self, _path: &Path) -> bool {
            false
        }

        fn is_root(&self) -> bool {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_shows_the_size_of_stdin_never_its_bytes() {
        let mut c = Cmd::new("ssh").arg("box");
        c.stdin = Some(b"SUPER-SECRET-VALUE".to_vec());
        let shown = format!("{c:?}");
        assert!(!shown.contains("SUPER-SECRET"), "{shown}");
        assert!(!shown.contains("83, 85"), "no byte list either: {shown}");
        assert!(shown.contains("stdin: Some(<18 bytes>)"), "{shown}");
        assert!(shown.contains("program: \"ssh\""), "{shown}");
    }

    #[test]
    fn as_root_wraps_in_sudo_and_carries_env() {
        let c = Cmd::new("apt-get")
            .args(["install", "-y", "fd-find"])
            .env("DEBIAN_FRONTEND", "noninteractive");
        assert_eq!(
            c.clone().as_root(false).argv(),
            [
                "sudo",
                "-n",
                "env",
                "DEBIAN_FRONTEND=noninteractive",
                "apt-get",
                "install",
                "-y",
                "fd-find"
            ]
        );
        assert!(c.clone().as_root(false).env.is_empty());
        assert_eq!(c.clone().as_root(true), c);
        assert_eq!(
            Cmd::new("snap").arg("install").as_root(false).argv(),
            ["sudo", "-n", "snap", "install"]
        );
    }

    #[test]
    fn windows_never_runs_an_extensionless_file_from_path() {
        // Elsewhere: the name as it is.
        assert_eq!(exe_candidates("scoop", None, false), ["scoop"]);
        // A Scoop shims directory holds `scoop` (a `#!/bin/sh` shim for Git Bash), `scoop.cmd`
        // and `scoop.ps1`: only the PATHEXT names are candidates, in PATHEXT order.
        let pathext = ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC;.CPL";
        let c = exe_candidates("scoop", Some(pathext), true);
        assert!(!c.contains(&"scoop".to_string()), "{c:?}");
        assert_eq!(
            &c[..4],
            ["scoop.COM", "scoop.EXE", "scoop.BAT", "scoop.CMD"]
        );
        let shims = ["scoop", "scoop.cmd", "scoop.ps1"];
        let found = c
            .iter()
            .find(|n| shims.iter().any(|s| s.eq_ignore_ascii_case(n)))
            .unwrap();
        assert!(found.eq_ignore_ascii_case("scoop.cmd"), "{found}");
        // A name with an extension is tried as it is first.
        assert_eq!(
            exe_candidates("sc.exe", Some(".COM;.EXE"), true),
            ["sc.exe", "sc.exe.COM", "sc.exe.EXE"]
        );
        // No (or an empty) PATHEXT: the default; junk entries are dropped.
        assert_eq!(
            exe_candidates("npm", None, true),
            ["npm.COM", "npm.EXE", "npm.BAT", "npm.CMD"]
        );
        assert_eq!(exe_candidates("npm", Some(" "), true).len(), 4);
        assert_eq!(exe_candidates("npm", Some(";.CMD;;x;."), true), ["npm.CMD"]);
    }

    #[test]
    fn display_quotes() {
        assert_eq!(
            Cmd::new("a").args(["b c", "d"]).display(),
            "a 'b c' d".to_string()
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_streams_and_captures() {
        let mut lines = Vec::new();
        let out = SystemRunner
            .stream(
                &Cmd::new("sh").args(["-c", "echo one; echo two >&2; exit 3"]),
                &mut |s, l| lines.push((s, l.to_string())),
            )
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, "one\n");
        assert_eq!(out.stderr, "two\n");
        assert!(lines.contains(&(Stream::Stdout, "one".into())));
        assert!(lines.contains(&(Stream::Stderr, "two".into())));
        let o = SystemRunner
            .output(&Cmd::new("sh").args(["-c", "echo $X"]).env("X", "y"))
            .unwrap();
        assert_eq!((o.code, o.stdout.as_str()), (Some(0), "y\n"));
        let e = SystemRunner
            .output(&Cmd::new("dot-no-such-program-xyz"))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(SystemRunner.on_path("sh"));
        assert!(!SystemRunner.on_path("dot-no-such-program-xyz"));
    }

    #[test]
    fn fake_rules() {
        let f = fake::FakeRunner::with_programs(&["brew"]);
        f.on("brew list", 1, "fd 1.0\n");
        f.once("brew list --cask", 0, "x", "");
        assert_eq!(
            f.output(&Cmd::new("brew").args(["list", "--cask"]))
                .unwrap()
                .stdout,
            "x"
        );
        assert_eq!(
            f.output(&Cmd::new("brew").args(["list", "--cask"]))
                .unwrap()
                .code,
            Some(1)
        );
        assert!(f.output(&Cmd::new("apt-get")).is_err());
        assert_eq!(f.lines().len(), 3);
    }

    #[test]
    fn lines_quote_what_needs_it() {
        let c = Cmd::new("defaults").args(["write", "com.x", "a key", "-string", "it's"]);
        assert_eq!(c.line(), "defaults write com.x 'a key' -string 'it'\\''s'");
    }

    #[test]
    fn privileged_wraps_in_sudo_unless_root() {
        let mut sys = System::fake(fake::FakeExec::new());
        let c = sys.privileged(Cmd::new("systemctl").args(["enable", "x"]));
        if cfg!(windows) {
            assert_eq!(c.line(), "systemctl enable x");
        } else {
            assert_eq!(c.line(), "sudo -n systemctl enable x");
        }
        sys.root = true;
        let c = sys.privileged(Cmd::new("systemctl").args(["enable", "x"]));
        assert_eq!(c.line(), "systemctl enable x");
    }

    #[test]
    fn fake_exec_answers_from_the_latest_matching_rule() {
        let fake = fake::FakeExec::new();
        fake.on("systemctl is-active", Output::exit(3, "inactive\n"));
        let c = Cmd::new("systemctl").args(["is-active", "x"]);
        assert_eq!(fake.output(&c).unwrap().stdout, "inactive\n");
        fake.on("systemctl is-active x", Output::ok("active\n"));
        assert!(fake.output(&c).unwrap().success());
        fake.fail_to_start("launchctl", io::ErrorKind::NotFound);
        let e = fake.output(&Cmd::new("launchctl")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(!fake.on_path("launchctl"));
        assert!(fake.output(&Cmd::new("other")).unwrap().success());
        assert_eq!(fake.calls().len(), 4);
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_feeds_stdin_and_reports_failures() {
        let out = SystemRunner
            .output(&Cmd {
                stdin: Some(b"hello".to_vec()),
                ..Cmd::new("cat")
            })
            .unwrap();
        assert_eq!(out, Output::ok("hello"));
        let out = SystemRunner
            .output(&Cmd::new("sh").args(["-c", "echo no >&2; exit 4"]))
            .unwrap();
        assert_eq!(out.code, Some(4));
        assert_eq!(
            out.failure(&Cmd::new("x")).to_string(),
            "`x` exited with code 4: no"
        );
    }
}
