//! The log file renderer: always on, full untruncated output with timestamps, redacted. One file
//! per run in `<state dir>/dot/logs/`, newest [`KEEP_LOGS`] kept.
//!
//! Logs hold full step output, which may carry a secret in a form the redactor cannot know
//! (URL-encoded, base64), so on Unix the directory is created `0700` and each file `0600`.
//!
//! The renderer counts the lines it writes and remembers where each step starts, so the console
//! renderers can point a failure at `log: <path>:<line>` ([`LogLocator`]).

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use dot_core::event::{PlanView, StepViewState};
use dot_core::{Event, Redactor, Renderer, StepId};

use super::outcome_word;
use super::plain::TAIL_LINES;

pub const KEEP_LOGS: usize = 10;

/// `$XDG_STATE_HOME/dot/logs` or `~/.local/state/dot/logs` on Unix; the local app-data dir on
/// Windows.
pub fn default_log_dir(home: &Path) -> Option<PathBuf> {
    Some(default_state_dir(home)?.join("logs"))
}

/// `dot`'s state directory as this process's environment sets it (see
/// [`dot_core::paths::state_dir`]: `$DOT_STATE_DIR`, else the platform's default under `home`).
/// Holds the run logs, apt's refresh stamp and the other state files.
pub fn default_state_dir(home: &Path) -> Option<PathBuf> {
    Some(dot_core::paths::state_dir(home, |k| std::env::var(k).ok()))
}

/// Where a step's record starts in the run log. Cheap to clone; shared with the
/// [`LogFileRenderer`] that fills it (it is added to the sink before the console renderer, so a
/// step's line is known by the time the console reports it).
#[derive(Debug, Clone)]
pub struct LogLocator {
    path: PathBuf,
    lines: Arc<Mutex<BTreeMap<StepId, usize>>>,
}

impl LogLocator {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lines: Arc::default(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `path:line` of the step's first log line, or, when it is not written yet (the log holds a
    /// later bundle's steps back under `--jobs N`), the path and the step's heading there
    /// (`name`: `bundle[n] label`, as [`LogFileRenderer`] writes it).
    pub fn at_named(&self, step: StepId, name: &str) -> String {
        match self.line_of(step) {
            Some(n) => format!("{}:{n}", self.path.display()),
            None => format!("{} (at \"step {name}: started\")", self.path.display()),
        }
    }

    /// `path:line` of the step's first log line, or just the path when it is not known.
    pub fn at(&self, step: StepId) -> String {
        match self.line_of(step) {
            Some(n) => format!("{}:{n}", self.path.display()),
            None => self.path.display().to_string(),
        }
    }

    fn line_of(&self, step: StepId) -> Option<usize> {
        self.lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&step)
            .copied()
    }

    fn record(&self, step: StepId, line: usize) {
        self.lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(step)
            .or_insert(line);
    }
}

impl From<&str> for LogLocator {
    fn from(p: &str) -> Self {
        Self::new(p)
    }
}

pub struct LogFileRenderer {
    out: BufWriter<File>,
    redactor: Redactor,
    plan: PlanView,
    dry_run: bool,
    /// Lines written so far.
    written: usize,
    locator: LogLocator,
    /// The last output lines of each running step, for the failure hint.
    tails: BTreeMap<StepId, VecDeque<String>>,
    /// When the event being handled happened, if it was held back ([`Renderer::handle_at`]).
    at: Option<SystemTime>,
}

/// `dir` and its missing parents, private to the user on Unix.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // An existing directory keeps its mode from an earlier version; tighten it.
        let mode = fs::metadata(dir)?.permissions().mode();
        if mode & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(mode & !0o077))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

impl LogFileRenderer {
    /// Creates a new log file in `dir` (created if missing) and prunes old ones.
    pub fn create(dir: &Path, redactor: Redactor) -> io::Result<Self> {
        create_private_dir(dir)?;
        let stamp = humantime::format_rfc3339_millis(SystemTime::now())
            .to_string()
            .replace(':', "-");
        let (file, path) = create_unique(dir, &stamp)?;
        prune(dir, KEEP_LOGS);
        Ok(Self {
            out: BufWriter::new(file),
            locator: LogLocator::new(path),
            redactor,
            plan: PlanView::default(),
            dry_run: false,
            written: 0,
            tails: BTreeMap::new(),
            at: None,
        })
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        self.locator.path()
    }

    /// Where each step's record starts, for the console renderers.
    pub fn locator(&self) -> LogLocator {
        self.locator.clone()
    }

    fn line(&mut self, text: &str) {
        let now = humantime::format_rfc3339_millis(self.at.unwrap_or_else(SystemTime::now));
        let text = self.redactor.redact(text);
        if writeln!(self.out, "{now} {text}").is_ok() {
            self.written += 1 + text.matches('\n').count();
        }
    }

    /// Remembers that `step`'s record starts at the next line written (its first event).
    fn mark(&self, step: StepId) {
        self.locator.record(step, self.written + 1);
    }

    fn step_name(&self, id: StepId) -> String {
        match self.plan.find_step(id) {
            Some((b, s)) => format!("{}[{}] {}", b.name, id.step, s.label),
            None => format!("step {}.{}", id.bundle, id.step),
        }
    }

    fn write_plan(&mut self) {
        let plan = self.plan.clone();
        self.line("plan:");
        if !plan.secrets.is_empty() {
            self.line(&format!("  secrets needed: {}", plan.secrets.join(", ")));
        }
        if !plan.sudo_steps.is_empty() {
            let what = if cfg!(windows) { "elevation" } else { "sudo" };
            self.line(&format!(
                "  {what} needed by: {}",
                plan.sudo_steps.join("; ")
            ));
        }
        if !plan.tty_sudo_steps.is_empty() {
            self.line(&format!(
                "  sudo asked for in the terminal by: {}",
                plan.tty_sudo_steps.join("; ")
            ));
        }
        for b in &plan.bundles {
            let skip = b
                .skip
                .as_ref()
                .map(|r| format!(" (skipped: {r})"))
                .unwrap_or_default();
            self.line(&format!("  bundle {} ({}){skip}", b.name, b.file.display()));
            for s in &b.steps {
                let head = format!("    [{}] {} {}", s.id.step, s.kind, s.label);
                match &s.state {
                    StepViewState::Skipped(r) => self.line(&format!("{head} (skipped: {r})")),
                    StepViewState::Error(e) => self.line(&format!("{head} (error: {e})")),
                    StepViewState::Ready(actions) => {
                        self.line(&head);
                        for a in actions {
                            let st = a.status.as_str();
                            self.line(&format!("      {} [{st}]", a.description));
                        }
                    }
                }
            }
        }
    }
}

fn create_unique(dir: &Path, stamp: &str) -> io::Result<(File, PathBuf)> {
    for n in 0..1000 {
        let name = if n == 0 {
            format!("{stamp}.log")
        } else {
            format!("{stamp}_{n}.log")
        };
        let path = dir.join(name);
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&path) {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free log file name",
    ))
}

/// Log files are named exactly `YYYY-MM-DDTHH-MM-SS.mmmZ.log`, or `..._<n>.log` when two runs
/// start in the same millisecond (see [`create_unique`]). Rotation deletes nothing else, since
/// `--log-dir` may point at a directory holding other files.
fn is_log_name(name: &str) -> bool {
    const STAMP: &[u8] = b"dddd-dd-ddTdd-dd-dd.dddZ";
    let Some(rest) = name.strip_suffix(".log") else {
        return false;
    };
    let bytes = rest.as_bytes();
    if bytes.len() < STAMP.len() {
        return false;
    }
    let (stamp, suffix) = bytes.split_at(STAMP.len());
    let stamp_ok = stamp.iter().zip(STAMP).all(|(b, p)| {
        if *p == b'd' {
            b.is_ascii_digit()
        } else {
            b == p
        }
    });
    let suffix_ok = match suffix {
        [] => true,
        [b'_', n @ ..] => !n.is_empty() && n.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    stamp_ok && suffix_ok
}

/// Deletes all but the newest `keep` log files (names sort chronologically).
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| is_log_name(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect();
    logs.sort();
    let excess = logs.len().saturating_sub(keep);
    for old in &logs[..excess] {
        let _ = fs::remove_file(old);
    }
}

impl Renderer for LogFileRenderer {
    fn handle_at(&mut self, event: &Event, at: SystemTime) {
        self.at = Some(at);
        self.handle(event);
        self.at = None;
    }

    fn handle(&mut self, event: &Event) {
        match event {
            Event::RunStarted {
                facts,
                dry_run,
                profile,
            } => {
                self.dry_run = *dry_run;
                let f: Vec<String> = facts
                    .pairs()
                    .into_iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                self.line(&format!(
                    "run started: dot {} dry_run={dry_run} profile={} facts: {}",
                    env!("CARGO_PKG_VERSION"),
                    profile.as_deref().unwrap_or("-"),
                    f.join(" ")
                ));
            }
            Event::PlanReady(view) => {
                self.plan = view.clone();
                self.write_plan();
            }
            Event::BundleStarted { name, .. } => self.line(&format!("bundle {name}: started")),
            Event::StepStarted { step, .. } => {
                self.mark(*step);
                let name = self.step_name(*step);
                self.line(&format!("step {name}: started"));
            }
            Event::Output { step, stream, line } => {
                self.line(&format!("  {}| {line}", stream.as_str()));
                let tail = self.tails.entry(*step).or_default();
                if tail.len() == TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line.clone());
                return; // flushed with the step
            }
            Event::Progress {
                done, total, unit, ..
            } => {
                let total = total.map(|t| t.to_string()).unwrap_or_else(|| "?".into());
                let unit = match unit {
                    dot_core::Unit::Bytes => " bytes",
                    dot_core::Unit::Items => "",
                };
                self.line(&format!("  progress {done}/{total}{unit}"));
                return;
            }
            Event::NeedsTty { step, reason } => match step {
                Some(step) => {
                    let name = self.step_name(*step);
                    self.line(&format!(
                        "step {name}: {reason}: terminal handed over, output not captured"
                    ));
                }
                None => self.line(&format!("{reason}: terminal handed over")),
            },
            Event::TtyReleased { .. } => self.line("  terminal released"),
            Event::ActionFinished {
                description,
                outcome,
                error,
                ..
            } => {
                let word = outcome_word(*outcome, self.dry_run);
                match error {
                    Some(e) => self.line(&format!("  action {word}: {description}: {e}")),
                    None => self.line(&format!("  action {word}: {description}")),
                }
            }
            Event::StepFinished {
                step,
                outcome,
                detail,
                elapsed,
            } => {
                // A step that never started (a planning error, a skip) is found by its result.
                self.mark(*step);
                let name = self.step_name(*step);
                let word = outcome_word(*outcome, self.dry_run);
                let detail = detail
                    .as_ref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default();
                self.line(&format!(
                    "step {name}: {word} in {:.3}s{detail}",
                    elapsed.as_secs_f64()
                ));
                let tail = self.tails.remove(step).unwrap_or_default();
                if *outcome == dot_core::Outcome::Failed {
                    let error = detail.strip_prefix(": ");
                    if let Some(r) = crate::hints::rule(error, tail.iter().map(String::as_str)) {
                        self.line(&format!("  hint ({}): {}", r.name, r.hint));
                    }
                }
            }
            Event::BundleFinished { name, outcome, .. } => {
                let word = outcome_word(*outcome, self.dry_run);
                self.line(&format!("bundle {name}: {word}"));
            }
            Event::RunError(e) => self.line(&format!("run stopped before applying: {e}")),
            Event::RunFinished(s) => {
                self.line(&format!(
                    "run finished: {} changed, {} ok, {} failed, {} skipped in {:.3}s",
                    s.changed,
                    s.ok,
                    s.failed,
                    s.skipped,
                    s.elapsed.as_secs_f64()
                ));
                for n in &s.notices {
                    self.line(&format!("notice: {n}"));
                }
            }
        }
        let _ = self.out.flush();
    }

    fn finish(&mut self) {
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_newest_ten_and_ignores_other_files() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..12 {
            let name = format!("2026-01-{:02}T00-00-00.000Z.log", i + 1);
            fs::write(dir.path().join(name), "").unwrap();
        }
        fs::write(dir.path().join("notes.log"), "").unwrap();
        // Sorts before every run log, so a loose matcher would delete it first.
        fs::write(dir.path().join("2024-server-migration-notes.log"), "").unwrap();
        let r = LogFileRenderer::create(dir.path(), Redactor::new()).unwrap();
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 12, "{names:?}");
        assert!(names.contains(&"notes.log".to_string()));
        assert!(
            names.contains(&"2024-server-migration-notes.log".to_string()),
            "user file with a date-like name kept"
        );
        assert!(!names.contains(&"2026-01-03T00-00-00.000Z.log".to_string()));
        assert!(names.contains(&"2026-01-12T00-00-00.000Z.log".to_string()));
        assert!(r.path().exists());
    }

    #[test]
    fn unique_names_within_the_same_millisecond() {
        let dir = tempfile::tempdir().unwrap();
        let (_, a) = create_unique(dir.path(), "2026-01-01T00-00-00.000Z").unwrap();
        let (_, b) = create_unique(dir.path(), "2026-01-01T00-00-00.000Z").unwrap();
        assert_ne!(a, b);
        assert!(is_log_name(&b.file_name().unwrap().to_string_lossy()));
        assert!(a < b, "later file sorts later");
    }

    #[test]
    fn log_name_matching_is_exact() {
        for ok in [
            "2026-09-29T10-00-00.123Z.log",
            "2026-09-29T10-00-00.123Z_1.log",
            "2026-09-29T10-00-00.123Z_42.log",
        ] {
            assert!(is_log_name(ok), "{ok}");
        }
        for not in [
            "2024-server-migration-notes.log",
            "2026-09-29T10-00-00.123Z.txt",
            "2026-09-29T10-00-00.123Z_.log",
            "2026-09-29T10-00-00.123Z_x.log",
            "2026-09-29T10-00-00.123Zextra.log",
            "2026-09-29T10:00:00.123Z.log",
            "2026-09-29.log",
            ".log",
        ] {
            assert!(!is_log_name(not), "{not}");
        }
    }

    #[test]
    fn writes_redacted_output() {
        let dir = tempfile::tempdir().unwrap();
        let redactor = Redactor::new();
        redactor.add("tok3n");
        let mut r = LogFileRenderer::create(dir.path(), redactor).unwrap();
        r.handle(&Event::Output {
            step: StepId { bundle: 0, step: 0 },
            stream: dot_core::Stream::Stderr,
            line: "auth tok3n".into(),
        });
        r.finish();
        let text = fs::read_to_string(r.path()).unwrap();
        assert!(text.contains("stderr| auth ***"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn logs_are_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state/dot/logs");
        let r = LogFileRenderer::create(&dir, Redactor::new()).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(r.path()), 0o600);
        // A directory made earlier with the default umask is tightened.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        LogFileRenderer::create(&dir, Redactor::new()).unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn locator_points_at_the_line_where_a_step_starts() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = LogFileRenderer::create(dir.path(), Redactor::new()).unwrap();
        let loc = r.locator();
        let s0 = StepId { bundle: 0, step: 0 };
        let s1 = StepId { bundle: 0, step: 1 };
        r.handle(&Event::BundleStarted {
            bundle: 0,
            name: "b".into(),
        });
        r.handle(&Event::Output {
            step: s0,
            stream: dot_core::Stream::Stdout,
            line: "not a step line".into(),
        });
        r.handle(&Event::StepStarted {
            step: s0,
            bundle: "b".into(),
            kind: "shell",
            label: "x".into(),
        });
        r.handle(&Event::RunError("two\nlines".into()));
        r.handle(&Event::StepFinished {
            step: s1,
            outcome: dot_core::Outcome::Failed,
            detail: Some("planning error".into()),
            elapsed: std::time::Duration::ZERO,
        });
        r.finish();
        let text = fs::read_to_string(r.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let at = |s: StepId| {
            let a = loc.at(s);
            let n: usize = a.rsplit(':').next().unwrap().parse().unwrap();
            assert!(a.starts_with(&r.path().display().to_string()), "{a}");
            n
        };
        assert!(
            lines[at(s0) - 1].contains("step step 0.0: started"),
            "{text}"
        );
        assert!(lines[at(s1) - 1].contains("failed"), "{text}");
        assert_eq!(
            loc.at(StepId { bundle: 9, step: 9 }),
            r.path().display().to_string()
        );
    }

    #[test]
    fn default_dir_under_home() {
        if cfg!(unix) && std::env::var_os("XDG_STATE_HOME").is_none() {
            assert_eq!(
                default_log_dir(Path::new("/h")).unwrap(),
                PathBuf::from("/h/.local/state/dot/logs")
            );
        }
    }
}
