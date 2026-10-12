//! Renderers: turn engine events into output. Every renderer passes the text it writes through
//! the shared [`dot_core::Redactor`].

pub mod json;
pub mod logfile;
pub mod ordered;
pub mod plain;
pub mod tty;

use std::io::IsTerminal as _;

use dot_core::{Outcome, Redactor, Renderer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verbosity {
    /// `-q`: failures and the summary.
    Quiet,
    /// Steps that changed or failed, and the summary.
    Normal,
    /// `-v`: every step, including satisfied and skipped ones.
    Verbose,
    /// `-vv`: also every line of command output, live.
    Debug,
}

impl Verbosity {
    pub fn from_flags(verbose: u8, quiet: bool) -> Self {
        match (quiet, verbose) {
            (true, _) => Verbosity::Quiet,
            (false, 0) => Verbosity::Normal,
            (false, 1) => Verbosity::Verbose,
            (false, _) => Verbosity::Debug,
        }
    }
}

/// Status marks, with an ASCII set for terminals whose locale is not UTF-8.
#[derive(Debug, Clone, Copy)]
pub struct Glyphs {
    pub changed: &'static str,
    pub would_change: &'static str,
    pub ok: &'static str,
    pub skipped: &'static str,
    pub failed: &'static str,
    /// Plan-time status not yet known (see `Status::Unknown`).
    pub unknown: &'static str,
}

impl Glyphs {
    pub const UNICODE: Glyphs = Glyphs {
        changed: "✓",
        would_change: "~",
        ok: "·",
        skipped: "-",
        failed: "✗",
        unknown: "?",
    };
    pub const ASCII: Glyphs = Glyphs {
        changed: "+",
        would_change: "~",
        ok: "=",
        skipped: "-",
        failed: "x",
        unknown: "?",
    };

    /// Unicode when the locale says UTF-8, else ASCII.
    pub fn detect() -> Glyphs {
        if locale_is_utf8(|k| std::env::var(k).ok()) {
            Glyphs::UNICODE
        } else {
            Glyphs::ASCII
        }
    }

    pub fn outcome(&self, outcome: Outcome, dry_run: bool) -> &'static str {
        match outcome {
            Outcome::Changed if dry_run => self.would_change,
            Outcome::Changed => self.changed,
            Outcome::Ok => self.ok,
            Outcome::Skipped => self.skipped,
            Outcome::Failed => self.failed,
        }
    }
}

/// POSIX rule: the first set of `LC_ALL`, `LC_CTYPE`, `LANG` decides. Windows Terminal is
/// UTF-8 regardless.
pub fn locale_is_utf8(get: impl Fn(&str) -> Option<String>) -> bool {
    if cfg!(windows) && get("WT_SESSION").is_some() {
        return true;
    }
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|k| get(k).filter(|v| !v.is_empty()))
        .map(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("utf-8") || v.contains("utf8")
        })
        .unwrap_or(false)
}

pub fn outcome_word(outcome: Outcome, dry_run: bool) -> &'static str {
    match outcome {
        Outcome::Changed if dry_run => "would change",
        Outcome::Changed => "changed",
        Outcome::Ok => "ok",
        Outcome::Skipped => "skipped",
        Outcome::Failed => "failed",
    }
}

/// Which console renderer a run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Tty,
    Plain,
    Json,
}

/// `--color`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

/// `--json` wins, then `--plain`; otherwise the live view when stdout is a terminal that can
/// move the cursor (`TERM` is not `dumb`).
pub fn select_mode(json: bool, plain: bool, stdout_is_tty: bool, term: Option<&str>) -> Mode {
    if json {
        Mode::Json
    } else if plain || !stdout_is_tty || term == Some("dumb") {
        Mode::Plain
    } else {
        Mode::Tty
    }
}

/// [`select_mode`] for this process's stdout and environment.
pub fn detect_mode(json: bool, plain: bool) -> Mode {
    let term = std::env::var("TERM").ok();
    select_mode(
        json,
        plain,
        std::io::stdout().is_terminal(),
        term.as_deref(),
    )
}

/// `--color always|never` decide; `auto` means colour unless `NO_COLOR` is set (to anything
/// non-empty), stdout is not a terminal, or the terminal cannot show colour (`TERM=dumb`, a
/// Windows console without VT mode).
pub fn use_color(
    choice: ColorChoice,
    no_color: Option<&str>,
    stdout_is_tty: bool,
    term_supports_color: bool,
) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            no_color.is_none_or(str::is_empty) && stdout_is_tty && term_supports_color
        }
    }
}

/// [`use_color`] for this process's stdout and environment.
pub fn detect_color(choice: ColorChoice) -> bool {
    let no_color = std::env::var("NO_COLOR").ok();
    // `console` checks `TERM`/`CLICOLOR` and turns on VT processing on Windows.
    let supported = console::Term::stdout().features().colors_supported();
    use_color(
        choice,
        no_color.as_deref(),
        std::io::stdout().is_terminal(),
        supported,
    )
}

/// The console renderer for `mode`, writing to stdout, and (for the live view) the handle that
/// lets the plan preview and the confirmation prompt take the terminal over.
pub fn console_renderer(
    mode: Mode,
    verbosity: Verbosity,
    color: ColorChoice,
    redactor: Redactor,
    log_path: Option<logfile::LogLocator>,
) -> (Box<dyn Renderer>, Option<tty::TtyConsole>) {
    match mode {
        Mode::Json => (
            Box::new(json::JsonRenderer::new(
                Box::new(std::io::stdout()),
                redactor,
            )),
            None,
        ),
        // In plan order, whatever order `--jobs` finishes bundles in.
        Mode::Plain => (
            Box::new(ordered::Ordered::new(
                plain::PlainRenderer::new(Box::new(std::io::stdout()), verbosity, redactor)
                    .log_path(log_path),
            )),
            None,
        ),
        Mode::Tty => {
            let r = tty::TtyRenderer::stdout(verbosity, detect_color(color), redactor)
                .log_path(log_path);
            let console = r.console();
            (Box::new(r), Some(console))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_selection() {
        let term = Some("xterm-256color");
        assert_eq!(select_mode(false, false, true, term), Mode::Tty);
        assert_eq!(select_mode(false, false, true, None), Mode::Tty);
        assert_eq!(select_mode(false, false, false, term), Mode::Plain);
        assert_eq!(select_mode(false, false, true, Some("dumb")), Mode::Plain);
        assert_eq!(select_mode(false, true, true, term), Mode::Plain);
        assert_eq!(select_mode(true, false, true, term), Mode::Json);
        assert_eq!(select_mode(true, true, false, Some("dumb")), Mode::Json);
    }

    #[test]
    fn color_choice() {
        use ColorChoice::*;
        assert!(use_color(Auto, None, true, true));
        assert!(use_color(Auto, Some(""), true, true));
        assert!(!use_color(Auto, Some("1"), true, true));
        assert!(!use_color(Auto, None, false, true));
        assert!(!use_color(Auto, None, true, false));
        assert!(use_color(Always, Some("1"), false, false));
        assert!(!use_color(Never, None, true, true));
    }

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn utf8_detection() {
        assert!(locale_is_utf8(env(&[("LANG", "en_US.UTF-8")])));
        assert!(locale_is_utf8(env(&[("LC_ALL", "C.utf8"), ("LANG", "C")])));
        assert!(!locale_is_utf8(env(&[
            ("LC_ALL", "C"),
            ("LANG", "en_US.UTF-8")
        ])));
        assert!(!locale_is_utf8(env(&[("LC_ALL", ""), ("LANG", "POSIX")])));
        assert!(!locale_is_utf8(env(&[])));
    }

    #[test]
    fn verbosity_flags() {
        assert_eq!(Verbosity::from_flags(0, true), Verbosity::Quiet);
        assert_eq!(Verbosity::from_flags(0, false), Verbosity::Normal);
        assert_eq!(Verbosity::from_flags(1, false), Verbosity::Verbose);
        assert_eq!(Verbosity::from_flags(3, false), Verbosity::Debug);
    }
}
