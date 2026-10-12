//! Asking for sudo once per run, up front, and keeping it working.
//!
//! Actions that install as root say so ([`dot_core::Action::needs_sudo`]); before anything is
//! applied, [`SudoPrepare`] checks the plan. When some bundle needs root and `dot` is not root:
//!
//! - interactive: unless sudo already has a valid timestamp (`sudo -n true`), the live view is
//!   paused (run-level `NeedsTty`), `sudo -v` asks for the password on the terminal, and the view
//!   resumes (`TtyReleased`);
//! - non-interactive (`--yes`, or no terminal): `sudo -n true` must succeed (passwordless sudo,
//!   or a timestamp from an earlier `sudo -v`), else the run fails before anything is applied,
//!   naming the steps that need sudo.
//!
//! Then a background thread runs `sudo -n -v` every minute until the run ends, so a long run
//! never hits an expired timestamp. `dot` runs its own root commands through `sudo -n`
//! ([`crate::exec::Cmd::as_root`]: apt, dnf, snap, root files, services...), so they never
//! prompt in the middle of the live view.
//!
//! Homebrew does not reuse that timestamp: every `brew` command (but a few read-only ones)
//! discards it first (`sudo --reset-timestamp`, since Homebrew 4.3.11), and a cask's `.pkg`
//! installer then runs `sudo` without `-n`, which asks for the password on the terminal (it
//! finds the controlling terminal whatever its stdin is). So:
//!
//! - an action whose program asks for the password itself ([`dot_core::Action::asks_for_sudo`])
//!   is not covered by the up-front `sudo -v` (which would not help). An interactive run hands
//!   it the terminal (`NeedsTty` for its step, the live view paused; its output is still
//!   streamed, as sudo opens the controlling terminal itself); a non-interactive run allows it
//!   only with
//!   passwordless sudo, `sudo -n -k true` (which ignores, without discarding, any cached
//!   timestamp: what that program will meet), and fails up front otherwise, naming the steps.
//! - `sudo -n` is checked again right before each action that needs it
//!   ([`dot_core::SudoCheck`], [`Sudo::before_sudo`]): when something discarded the timestamp
//!   (an earlier `brew`) or it expired, an interactive run asks for the password again (the view paused), and a
//!   non-interactive one fails that action with a clear message instead of a bare `sudo -n`
//!   error.
//! - while a program has the terminal for a password prompt ([`Sudo::terminal`]), the keep-alive
//!   skips its refresh: a `sudo -n -v` would wait for the prompting sudo's lock on the
//!   timestamp. Dropping the keep-alive never waits long for a refresh still running.
//!
//! Windows has no sudo, and a process cannot elevate itself: when some unsatisfied action needs
//! administrator rights (a system service, a machine-wide winget install) and the terminal is
//! not elevated, the run fails here, before anything is applied, listing those steps and asking
//! for an elevated terminal ([`crate::elevation`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use dot_core::{Event, EventSink, Plan, Prepare, StepId, SudoCheck};

use crate::exec::{Cmd, Runner, SystemRunner};

/// How often the sudo timestamp is refreshed while the run goes on.
pub const KEEPALIVE_EVERY: Duration = Duration::from_secs(60);

/// How long stopping the keep-alive waits for a refresh that is still running.
const KEEPALIVE_STOP_WAIT: Duration = Duration::from_secs(2);

/// How this platform gets the privileges an action needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    /// `sudo`, asked for once up front (Unix).
    Sudo,
    /// An elevated terminal, checked up front (Windows).
    Elevation,
}

impl Privilege {
    /// This platform's.
    pub fn host() -> Self {
        if cfg!(windows) {
            Privilege::Elevation
        } else {
            Privilege::Sudo
        }
    }
}

/// A run's sudo, shared by the prepare stage, the keep-alive, the engine's check before each
/// action that needs root and the package managers: whether a person is there to type a
/// password, whether `dot` asked for sudo up front, and the terminal's turn for a program that
/// asks for the password itself.
pub struct Sudo {
    runner: Arc<dyn Runner>,
    interactive: bool,
    /// The prepare stage made sure `sudo -n` works (so it is checked again before each action
    /// that relies on it).
    primed: AtomicBool,
    /// Held while a program may be asking for the sudo password on the terminal.
    terminal: Mutex<()>,
}

impl std::fmt::Debug for Sudo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sudo")
            .field("interactive", &self.interactive)
            .field("primed", &self.primed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl Sudo {
    /// `interactive`: a terminal is there to ask for the password on.
    pub fn new(runner: Arc<dyn Runner>, interactive: bool) -> Arc<Self> {
        Arc::new(Self {
            runner,
            interactive,
            primed: AtomicBool::new(false),
            terminal: Mutex::new(()),
        })
    }

    /// The real machine, nobody to ask (`dot plan`, `dot status`).
    pub fn system() -> Arc<Self> {
        Self::new(Arc::new(SystemRunner), false)
    }

    /// Whether a person can answer a password prompt on the terminal.
    pub fn interactive(&self) -> bool {
        self.interactive
    }

    /// Whether `dot` runs as root (or elevated): nothing asks for a password then.
    pub fn is_root(&self) -> bool {
        self.runner.is_root()
    }

    /// The terminal's turn for a program that may ask for the sudo password on it: hold it
    /// while it runs, so the keep-alive's `sudo -n -v` does not queue behind the prompt.
    pub fn terminal(&self) -> MutexGuard<'_, ()> {
        self.terminal.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether sudo needs no password at all here (passwordless sudo): `sudo -n -k true`
    /// ignores any cached timestamp, without discarding it, which is what a program that
    /// discards the timestamp (Homebrew) meets.
    pub fn passwordless(&self) -> bool {
        self.runner.on_path("sudo")
            && self
                .runner
                .output(&Cmd::new("sudo").args(["-n", "-k", "true"]))
                .is_ok_and(|o| o.success())
    }

    /// One keep-alive refresh: `sudo -n -v`, unless a program has the terminal for a prompt.
    fn refresh(&self) {
        let Ok(_turn) = self.terminal.try_lock() else {
            return;
        };
        // Best effort: a failure shows up when the next action that needs root checks again.
        let _ = self.runner.output(&Cmd::new("sudo").args(["-n", "-v"]));
    }
}

impl SudoCheck for Sudo {
    /// Checks `sudo -n true` again when the prepare stage asked for sudo; asks for the
    /// password again (interactive runs) when something discarded the timestamp since.
    fn before_sudo(&self, sink: &EventSink, step: StepId, what: &str) -> Result<()> {
        if !self.primed.load(Ordering::SeqCst) || self.runner.is_root() {
            return Ok(());
        }
        if sudo_works(self.runner.as_ref()) {
            return Ok(());
        }
        if !self.interactive {
            bail!(
                "sudo asks for a password again (something discarded or expired its timestamp \
                 since the run started; on macOS, a `brew` command does that), and dot is \
                 running non-interactively: run it interactively, or allow passwordless sudo"
            );
        }
        let _turn = self.terminal();
        sink.emit(Event::NeedsTty {
            step: Some(step),
            reason: format!(
                "sudo password, again (the sudo timestamp was discarded or expired since the \
                 run started; needed by {what})"
            ),
        });
        let result = self.runner.interactive(&Cmd::new("sudo").arg("-v"));
        sink.emit(Event::TtyReleased { step: Some(step) });
        match result {
            Ok(Some(0)) => Ok(()),
            Ok(_) => bail!("sudo -v failed; sudo is needed by {what}"),
            Err(e) => bail!("cannot run sudo -v: {e}"),
        }
    }
}

pub struct SudoPrepare {
    sudo: Arc<Sudo>,
    interval: Duration,
    keepalive: Option<KeepAlive>,
    privilege: Privilege,
}

impl SudoPrepare {
    /// `interactive`: a terminal is there to ask for the password on.
    pub fn new(runner: Arc<dyn Runner>, interactive: bool) -> Self {
        Self::with_sudo(Sudo::new(runner, interactive))
    }

    /// For the run's shared [`Sudo`] (also handed to the engine and the package managers).
    pub fn with_sudo(sudo: Arc<Sudo>) -> Self {
        Self {
            sudo,
            interval: KEEPALIVE_EVERY,
            keepalive: None,
            privilege: Privilege::host(),
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// For tests of either platform's flow on any host.
    pub fn with_privilege(mut self, privilege: Privilege) -> Self {
        self.privilege = privilege;
        self
    }

    /// The run's sudo.
    pub fn sudo(&self) -> Arc<Sudo> {
        self.sudo.clone()
    }

    /// Whether the keep-alive thread runs.
    pub fn keeping_alive(&self) -> bool {
        self.keepalive.is_some()
    }

    /// Stops the keep-alive (also done on drop).
    pub fn stop(&mut self) {
        self.keepalive = None;
    }
}

impl Prepare for SudoPrepare {
    fn prepare(&mut self, plan: &Plan, sink: &EventSink) -> Result<()> {
        let steps = plan.sudo_steps();
        let asking = plan.tty_sudo_steps();
        let runner = self.sudo.runner.as_ref();
        if (steps.is_empty() && asking.is_empty()) || runner.is_root() {
            return Ok(());
        }
        if self.privilege == Privilege::Elevation {
            let mut all = steps;
            all.extend(asking);
            bail!(crate::elevation::not_elevated_error(&all));
        }
        if !asking.is_empty() && !self.sudo.interactive {
            let who = asking.join("; ");
            if !runner.on_path("sudo") {
                bail!(
                    "root is needed by {who}, but sudo is not installed: install sudo, or \
                     allow passwordless sudo"
                );
            }
            if !self.sudo.passwordless() {
                bail!(
                    "{who}: Homebrew asks for the sudo password itself (it discards cached \
                     sudo credentials, so an earlier `sudo -v` does not help), but dot is \
                     running non-interactively: run it interactively (it asks on the \
                     terminal), or allow passwordless sudo"
                );
            }
        }
        if steps.is_empty() {
            return Ok(());
        }
        let who = steps.join("; ");
        ensure_sudo(runner, self.sudo.interactive, &who, sink)?;
        self.sudo.primed.store(true, Ordering::SeqCst);
        self.keepalive = Some(KeepAlive::start(self.sudo.clone(), self.interval));
        Ok(())
    }
}

/// Whether `sudo -n true` succeeds (a valid timestamp, or passwordless sudo).
fn sudo_works(runner: &dyn Runner) -> bool {
    runner
        .output(&Cmd::new("sudo").args(["-n", "true"]))
        .is_ok_and(|o| o.success())
}

/// Makes sure `sudo -n` works for the rest of the run, asking for the password once (on the
/// terminal, the console paused around it) when it does not and the run is interactive. `who`
/// says what needs root, for the messages. Not for root, nor Windows (no sudo there).
pub fn ensure_sudo(
    runner: &dyn Runner,
    interactive: bool,
    who: &str,
    sink: &EventSink,
) -> Result<()> {
    if !runner.on_path("sudo") {
        bail!(
            "root is needed by {who}, but sudo is not installed: run dot as root, or install \
             sudo"
        );
    }
    if sudo_works(runner) {
        return Ok(());
    }
    if !interactive {
        bail!(
            "sudo is needed by {who}; it asks for a password, but dot is running \
             non-interactively: run it interactively, run `sudo -v` first, or allow \
             passwordless sudo"
        );
    }
    sink.emit(Event::NeedsTty {
        step: None,
        reason: format!("sudo password (needed by {who})"),
    });
    let result = runner.interactive(&Cmd::new("sudo").arg("-v"));
    sink.emit(Event::TtyReleased { step: None });
    match result {
        Ok(Some(0)) => Ok(()),
        Ok(_) => bail!("sudo -v failed; sudo is needed by {who}"),
        Err(e) => bail!("cannot run sudo -v: {e}"),
    }
}

/// Runs `sudo -n -v` every `interval` on a background thread until dropped, skipping a refresh
/// while a program has the terminal for a password prompt ([`Sudo::terminal`]).
pub struct KeepAlive {
    stop: Option<mpsc::Sender<()>>,
    done: Option<mpsc::Receiver<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl KeepAlive {
    pub fn start(sudo: Arc<Sudo>, interval: Duration) -> Self {
        let (tx, rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let thread = thread::spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(interval) {
                sudo.refresh();
            }
            let _ = done_tx.send(());
        });
        Self {
            stop: Some(tx),
            done: Some(done_rx),
            thread: Some(thread),
        }
    }
}

impl Drop for KeepAlive {
    /// Stops the thread. A refresh still running (a `sudo -n -v` that hangs) is waited for only
    /// briefly; the thread is then left to end on its own.
    fn drop(&mut self) {
        drop(self.stop.take());
        let ended = self
            .done
            .take()
            .is_some_and(|d| d.recv_timeout(KEEPALIVE_STOP_WAIT).is_ok());
        if let Some(t) = self.thread.take()
            && ended
        {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use dot_core::engine::{PlannedAction, PlannedBundle, PlannedState, PlannedStep};
    use dot_core::event::Recorder;
    use dot_core::{Action, Outcome, RunCtx, Status, StepId, SudoCheck};

    use super::*;
    use crate::exec::Runner;
    use crate::exec::fake::{FakeRunner, Mode};

    struct Root(bool);
    impl Action for Root {
        fn describe(&self) -> String {
            "x".into()
        }
        fn status(&self) -> Result<Status> {
            Ok(Status::Pending)
        }
        fn apply(&self, _: &RunCtx) -> Result<Outcome> {
            Ok(Outcome::Changed)
        }
        fn needs_sudo(&self) -> bool {
            self.0
        }
    }

    /// Needs root, through a program that asks for the password itself (a `.pkg` cask).
    struct Asks;
    impl Action for Asks {
        fn describe(&self) -> String {
            "x".into()
        }
        fn status(&self) -> Result<Status> {
            Ok(Status::Pending)
        }
        fn apply(&self, _: &RunCtx) -> Result<Outcome> {
            Ok(Outcome::Changed)
        }
        fn needs_sudo(&self) -> bool {
            true
        }
        fn asks_for_sudo(&self) -> bool {
            true
        }
    }

    /// Bundle `casks` with an [`Asks`] action, after the bundles of `bundles`.
    fn plan_asking(bundles: &[(&str, bool, Status)]) -> Plan {
        let mut p = plan(bundles);
        let mut b = plan(&[("casks", true, Status::Pending)]).bundles.remove(0);
        b.steps[0].id.bundle = p.bundles.len();
        if let PlannedState::Ready(actions) = &mut b.steps[0].state {
            actions[0].action = Box::new(Asks);
        }
        p.bundles.push(b);
        p
    }

    fn plan(bundles: &[(&str, bool, Status)]) -> Plan {
        Plan {
            bundles: bundles
                .iter()
                .enumerate()
                .map(|(i, (name, sudo, status))| PlannedBundle {
                    name: name.to_string(),
                    file: "/b".into(),
                    skip: None,
                    steps: vec![PlannedStep {
                        id: StepId { bundle: i, step: 0 },
                        kind: "package",
                        label: "x".into(),
                        state: PlannedState::Ready(vec![PlannedAction {
                            action: Box::new(Root(*sudo)),
                            description: "x".into(),
                            status: *status,
                        }]),
                        secrets: Vec::new(),
                    }],
                    secrets: Default::default(),
                    needed: Default::default(),
                })
                .collect(),
            secrets: Default::default(),
            needed: Default::default(),
        }
    }

    fn runner() -> Arc<FakeRunner> {
        let r = Arc::new(FakeRunner::with_programs(&["sudo"]));
        r.on("sudo -n true", 1, "");
        r
    }

    fn prepare(r: &Arc<FakeRunner>, interactive: bool, p: &Plan) -> (Result<()>, Vec<Event>) {
        let (res, events, _) = prepare_sudo(r, interactive, p);
        (res, events)
    }

    /// [`prepare`], with the run's [`Sudo`].
    fn prepare_sudo(
        r: &Arc<FakeRunner>,
        interactive: bool,
        p: &Plan,
    ) -> (Result<()>, Vec<Event>, Arc<Sudo>) {
        let rec = Recorder::default();
        let sink = EventSink::new().with(Box::new(rec.clone()));
        let mut s = SudoPrepare::new(r.clone(), interactive)
            .with_privilege(Privilege::Sudo)
            .with_interval(Duration::from_secs(3600));
        let res = s.prepare(p, &sink);
        let expected = res.is_ok() && !p.sudo_bundles().is_empty() && !r.is_root();
        assert_eq!(s.keeping_alive(), expected);
        (res, rec.events(), s.sudo())
    }

    #[test]
    fn a_program_asking_for_the_password_itself_is_not_asked_for_up_front() {
        let r = runner();
        let p = plan_asking(&[]);
        assert!(p.sudo_steps().is_empty());
        assert_eq!(p.tty_sudo_steps(), ["casks: x"]);
        assert_eq!(p.view().tty_sudo_steps, ["casks: x"]);

        // Interactive: it asks on the terminal when it runs; nothing to do now.
        let (res, events) = prepare(&r, true, &p);
        res.unwrap();
        assert!(events.is_empty());
        assert!(r.calls().is_empty(), "{:?}", r.lines());

        // With other steps needing root: those are asked for, once, as before.
        let p = plan_asking(&[("apt-tools", true, Status::Pending)]);
        assert_eq!(p.sudo_steps(), ["apt-tools: x"]);
        let (res, events) = prepare(&r, true, &p);
        res.unwrap();
        assert_eq!(r.lines(), ["sudo -n true", "sudo -v"]);
        match &events[..] {
            [Event::NeedsTty { reason, .. }, Event::TtyReleased { .. }] => {
                assert!(!reason.contains("casks"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_program_asking_for_the_password_itself_needs_passwordless_sudo_unattended() {
        let r = runner();
        // A valid timestamp does not count: that program discards it.
        r.on("sudo -n true", 0, "");
        r.on("sudo -n -k true", 1, "");
        let (res, events) = prepare(&r, false, &plan_asking(&[]));
        let e = res.unwrap_err().to_string();
        assert!(
            e.contains("casks: x: Homebrew asks for the sudo password itself"),
            "{e}"
        );
        assert!(e.contains("non-interactively"), "{e}");
        assert!(e.contains("allow passwordless sudo"), "{e}");
        assert!(events.is_empty());
        assert_eq!(r.lines(), ["sudo -n -k true"], "never prompts");

        r.on("sudo -n -k true", 0, "");
        prepare(&r, false, &plan_asking(&[])).0.unwrap();

        let r = Arc::new(FakeRunner::default());
        let e = prepare(&r, false, &plan_asking(&[])).0.unwrap_err();
        assert!(e.to_string().contains("sudo is not installed"), "{e}");
    }

    #[test]
    fn sudo_is_checked_again_before_each_action_that_needs_it() {
        let r = runner();
        let (res, _, sudo) = prepare_sudo(&r, true, &plan(&[("a", true, Status::Pending)]));
        res.unwrap();
        r.clear_calls();
        let rec = Recorder::default();
        let sink = EventSink::new().with(Box::new(rec.clone()));
        let step = StepId { bundle: 0, step: 0 };

        // Still valid: nothing asked.
        r.on("sudo -n true", 0, "");
        sudo.before_sudo(&sink, step, "write /etc/x").unwrap();
        assert_eq!(r.lines(), ["sudo -n true"]);
        assert!(rec.events().is_empty());

        // Discarded since (a `brew` ran): asked again, the step's view paused around it.
        r.clear_calls();
        r.on("sudo -n true", 1, "");
        sudo.before_sudo(&sink, step, "write /etc/x").unwrap();
        assert_eq!(r.lines(), ["sudo -n true", "sudo -v"]);
        assert_eq!(r.lines_of(Mode::Interactive), ["sudo -v"]);
        match &rec.events()[..] {
            [
                Event::NeedsTty {
                    step: Some(s),
                    reason,
                },
                Event::TtyReleased { step: Some(t) },
            ] => {
                assert_eq!((*s, *t), (step, step));
                assert!(reason.contains("again"), "{reason}");
                assert!(reason.contains("write /etc/x"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        r.on("sudo -v", 1, "");
        let e = sudo.before_sudo(&sink, step, "w").unwrap_err();
        assert!(e.to_string().contains("sudo -v failed"), "{e}");

        // Non-interactive: a clear failure, never a prompt.
        let r = runner();
        r.on("sudo -n true", 0, "");
        let (res, _, sudo) = prepare_sudo(&r, false, &plan(&[("a", true, Status::Pending)]));
        res.unwrap();
        r.on("sudo -n true", 1, "");
        r.clear_calls();
        let e = sudo.before_sudo(&sink, step, "w").unwrap_err().to_string();
        assert!(e.contains("discarded or expired its timestamp"), "{e}");
        assert!(e.contains("non-interactively"), "{e}");
        assert!(r.lines_of(Mode::Interactive).is_empty());

        // Not asked for up front (nothing needed it, or root): not checked either.
        let r = runner();
        let sudo = Sudo::new(r.clone(), true);
        sudo.before_sudo(&sink, step, "w").unwrap();
        assert!(r.calls().is_empty());
    }

    #[test]
    fn keepalive_skips_while_a_program_has_the_terminal() {
        let r = runner();
        let sudo = Sudo::new(r.clone(), true);
        let count = || r.lines().iter().filter(|l| *l == "sudo -n -v").count();
        let turn = sudo.terminal();
        let k = KeepAlive::start(sudo.clone(), Duration::from_millis(5));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(count(), 0, "no refresh while the terminal is handed over");
        drop(turn);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while count() == 0 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(count() > 0, "refreshes again afterwards");
        // A refresh never runs while the terminal is held, even one already due.
        let _turn = sudo.terminal();
        let held = count();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(count(), held);
        drop(k);
    }

    #[test]
    fn stopping_the_keepalive_never_waits_long_for_a_stuck_refresh() {
        struct Stuck(Arc<FakeRunner>, std::sync::Mutex<bool>);
        impl Runner for Stuck {
            fn output(&self, cmd: &Cmd) -> std::io::Result<crate::exec::Output> {
                *self.1.lock().unwrap() = true;
                thread::sleep(Duration::from_secs(30));
                self.0.output(cmd)
            }
            fn stream(
                &self,
                cmd: &Cmd,
                out: &mut crate::exec::OutFn<'_>,
            ) -> std::io::Result<crate::exec::Output> {
                self.0.stream(cmd, out)
            }
            fn interactive(&self, cmd: &Cmd) -> std::io::Result<Option<i32>> {
                self.0.interactive(cmd)
            }
            fn on_path(&self, p: &str) -> bool {
                self.0.on_path(p)
            }
            fn exists(&self, p: &std::path::Path) -> bool {
                self.0.exists(p)
            }
            fn is_root(&self) -> bool {
                false
            }
        }
        let stuck = Arc::new(Stuck(runner(), std::sync::Mutex::new(false)));
        let k = KeepAlive::start(Sudo::new(stuck.clone(), false), Duration::from_millis(1));
        while !*stuck.1.lock().unwrap() {
            thread::sleep(Duration::from_millis(1));
        }
        let t = std::time::Instant::now();
        drop(k);
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    }

    #[test]
    fn nothing_needs_sudo_nothing_runs() {
        let r = runner();
        let (res, events) = prepare(
            &r,
            true,
            &plan(&[
                ("a", false, Status::Pending),
                ("b", true, Status::Satisfied),
            ]),
        );
        res.unwrap();
        assert!(events.is_empty());
        assert!(r.calls().is_empty());
    }

    #[test]
    fn interactive_asks_once_with_the_view_paused() {
        let r = runner();
        let p = plan(&[
            ("apt-tools", true, Status::Pending),
            ("b", false, Status::Pending),
        ]);
        let (res, events) = prepare(&r, true, &p);
        res.unwrap();
        assert_eq!(
            r.lines(),
            ["sudo -n true", "sudo -v"],
            "checks, then asks on the terminal"
        );
        assert_eq!(r.lines_of(Mode::Interactive), ["sudo -v"]);
        match &events[..] {
            [
                Event::NeedsTty { step: None, reason },
                Event::TtyReleased { step: None },
            ] => assert!(reason.contains("apt-tools"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn skipped_bundles_need_no_sudo() {
        let mut p = plan(&[("a", true, Status::Pending), ("b", true, Status::Pending)]);
        p.bundles[0].skip = Some("when: os=macos".into());
        assert_eq!(p.sudo_bundles(), ["b"]);
        assert_eq!(p.view().sudo, ["b"]);
        assert_eq!(p.view().sudo_steps, ["b: x"]);
    }

    #[test]
    fn a_valid_timestamp_needs_no_prompt() {
        let r = runner();
        r.on("sudo -n true", 0, "");
        let (res, events) = prepare(&r, true, &plan(&[("a", true, Status::Unknown)]));
        res.unwrap();
        assert!(events.is_empty());
        assert_eq!(r.lines(), ["sudo -n true"]);
    }

    #[test]
    fn non_interactive_fails_up_front_naming_the_bundles() {
        let r = runner();
        let p = plan(&[
            ("apt-tools", true, Status::Pending),
            ("fonts", false, Status::Pending),
            ("snaps", true, Status::Unknown),
        ]);
        let (res, events) = prepare(&r, false, &p);
        let e = res.unwrap_err().to_string();
        assert!(
            e.contains("sudo is needed by apt-tools: x; snaps: x; it asks for a password"),
            "{e}"
        );
        assert!(e.contains("non-interactively"), "{e}");
        assert!(events.is_empty());
        assert_eq!(r.lines(), ["sudo -n true"], "never prompts");

        // Passwordless sudo is fine.
        r.on("sudo -n true", 0, "");
        prepare(&r, false, &p).0.unwrap();
    }

    #[test]
    fn a_failed_password_fails_the_run() {
        let r = runner();
        r.on("sudo -v", 1, "");
        let (res, events) = prepare(&r, true, &plan(&[("a", true, Status::Pending)]));
        assert!(res.unwrap_err().to_string().contains("sudo -v failed"));
        assert_eq!(events.len(), 2, "the view resumes either way");
    }

    #[test]
    fn root_and_missing_sudo() {
        let r = runner();
        r.set_root(true);
        prepare(&r, false, &plan(&[("a", true, Status::Pending)]))
            .0
            .unwrap();
        assert!(r.calls().is_empty());
        let r = Arc::new(FakeRunner::default());
        let rec = Recorder::default();
        let sink = EventSink::new().with(Box::new(rec));
        let e = SudoPrepare::new(r, true)
            .with_privilege(Privilege::Sudo)
            .prepare(&plan(&[("a", true, Status::Pending)]), &sink)
            .unwrap_err();
        assert!(e.to_string().contains("sudo is not installed"), "{e}");
    }

    #[test]
    fn windows_fails_up_front_unless_elevated() {
        let r = runner();
        let p = plan(&[
            ("services", true, Status::Pending),
            ("fonts", false, Status::Pending),
            ("apps", true, Status::Unknown),
            ("done", true, Status::Satisfied),
        ]);
        for interactive in [true, false] {
            let rec = Recorder::default();
            let sink = EventSink::new().with(Box::new(rec.clone()));
            let mut s =
                SudoPrepare::new(r.clone(), interactive).with_privilege(Privilege::Elevation);
            let e = s.prepare(&p, &sink).unwrap_err().to_string();
            assert!(
                e.contains("needed by services: x; apps: x; run dot from an elevated terminal"),
                "{e}"
            );
            assert!(!s.keeping_alive());
            assert!(rec.events().is_empty(), "never prompts");
        }
        assert!(r.calls().is_empty(), "no sudo on Windows");
        // Elevated: nothing to do.
        r.set_root(true);
        let mut s = SudoPrepare::new(r.clone(), false).with_privilege(Privilege::Elevation);
        s.prepare(&p, &EventSink::new()).unwrap();
        assert!(r.calls().is_empty());
        // Nothing needs it: nothing to do either.
        r.set_root(false);
        s.prepare(&plan(&[("a", false, Status::Pending)]), &EventSink::new())
            .unwrap();
    }

    #[test]
    fn keepalive_refreshes_until_dropped() {
        let r = runner();
        r.on("sudo -n true", 0, "");
        let mut s = SudoPrepare::new(r.clone(), false)
            .with_privilege(Privilege::Sudo)
            .with_interval(Duration::from_millis(10));
        s.prepare(&plan(&[("a", true, Status::Pending)]), &EventSink::new())
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let count = || r.lines().iter().filter(|l| *l == "sudo -n -v").count();
        while count() < 2 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(count() >= 2);
        s.stop();
        let after = count();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(count(), after, "stopped with the run");
    }
}
