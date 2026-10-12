//! `dot env --shell zsh|bash|fish|pwsh [-o FILE]`: a file for an interactive shell to source,
//! so it sees the values `dot` works with.
//!
//! It holds every non-sensitive variable a shell step would receive from `dot.yml`, the profile,
//! the `.env` files, `local.env` and `--var` (see [`dot_core::Vars::exported`]; bundle-level
//! `vars:` stay with their bundle) whose name is an environment-style one (no lowercase letters:
//! `WORKSPACE_DIR`, not the `workspace_dir` a `dot.yml` uses in templates, which would be noise
//! in every shell), then what `env` / `path` directives recorded when they were
//! applied (the [`dot_core::runenv::EnvRecord`] a real `dot apply` saves in the state directory;
//! a recorded variable wins over a declared one). A sensitive value is never written, not even
//! inside another value.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result};
use clap::{Args, ValueEnum};
use dot_core::Redactor;
use dot_core::runenv::EnvRecord;
use dot_core::shellenv::{self, Shell};

use crate::RepoArgs;
use crate::commands::{open, state_store};

#[derive(Clone, Copy, ValueEnum)]
pub enum ShellArg {
    Zsh,
    Bash,
    Fish,
    Pwsh,
}

impl From<ShellArg> for Shell {
    fn from(s: ShellArg) -> Self {
        match s {
            ShellArg::Zsh => Shell::Zsh,
            ShellArg::Bash => Shell::Bash,
            ShellArg::Fish => Shell::Fish,
            ShellArg::Pwsh => Shell::Pwsh,
        }
    }
}

#[derive(Args)]
pub struct EnvArgs {
    /// The shell to write for.
    #[arg(long, value_enum)]
    pub shell: ShellArg,
    /// Write the file here instead of printing it.
    #[arg(short, long, value_name = "FILE", value_parser = crate::path_arg())]
    pub output: Option<PathBuf>,
    /// Keep the file in dot's cache directory (`$XDG_CACHE_HOME/dot/env.<shell>`), write it
    /// again only when something it comes from changed (the state store, `local.env`, the
    /// repository's `dot.yml` and `.env*` files, this binary), and print its path: `source
    /// "$(dot env --shell zsh --cache)"` in a shell's startup file.
    #[arg(long, conflicts_with = "output")]
    pub cache: bool,
    #[command(flatten)]
    pub repo: RepoArgs,
}

pub fn env(a: EnvArgs) -> Result<ExitCode> {
    // The fast path: nothing changed since the cached file was written.
    let cached = if a.cache {
        match cache_file(&a) {
            Some((path, inputs)) if fresh(&path, &inputs) => {
                println!("{}", path.display());
                return Ok(ExitCode::SUCCESS);
            }
            Some((path, _)) => Some(path),
            None => None,
        }
    } else {
        None
    };
    let (ws, inputs) = match open(&a.repo)? {
        Ok(x) => x,
        Err(code) => return Ok(code),
    };
    let redactor = Redactor::new();
    for (_, v) in ws.vars.iter().filter(|(_, v)| v.sensitive) {
        redactor.add(v.as_string());
    }
    let mut values: BTreeMap<String, String> = ws
        .vars
        .exported(&[])
        .map_err(anyhow::Error::msg)?
        .into_iter()
        .filter(|(n, _)| is_shell_name(n))
        .collect();
    let record = match state_store(&inputs.home) {
        Some(store) => EnvRecord::load(&store)?,
        None => EnvRecord::default(),
    };
    values.extend(
        record
            .env
            .vars
            .iter()
            .map(|v| (v.name.clone(), v.value.clone())),
    );
    // Backstop: nothing that holds a sensitive value leaves this process.
    values.retain(|_, v| redactor.redact(v) == v.as_str());
    let text = shellenv::render(a.shell.into(), &values, &record.env);
    let dirs = record.env.path.len();
    if let Some(path) = &cached {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        write_atomically(path, &text)?;
        println!("{}", path.display());
        return Ok(ExitCode::SUCCESS);
    }
    match &a.output {
        None => print!("{text}"),
        Some(path) => {
            let tmp = path.with_extension(format!("tmp{}", std::process::id()));
            std::fs::write(&tmp, &text)
                .with_context(|| format!("cannot write {}", tmp.display()))?;
            std::fs::rename(&tmp, path)
                .with_context(|| format!("cannot write {}", path.display()))?;
            eprintln!(
                "wrote {}: {} variable{}, {} PATH entr{}",
                path.display(),
                values.len(),
                if values.len() == 1 { "" } else { "s" },
                dirs,
                if dirs == 1 { "y" } else { "ies" }
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn write_atomically(path: &std::path::Path, text: &str) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
}

/// Where `--cache` keeps the file, and what it is made from (files whose change makes it
/// stale). Found without loading the repository: the fast path is a few `stat`s.
fn cache_file(a: &EnvArgs) -> Option<(PathBuf, Vec<PathBuf>)> {
    let inputs = crate::commands::inputs(&a.repo).ok()?;
    let local = inputs.local_control().ok()?;
    let repo = dot_core::repo::find_repo(
        inputs.repo.as_deref(),
        inputs.env.get("DOT_REPO").map(String::as_str),
        &inputs.cwd,
        local.get("DOT_REPO").map(String::as_str),
        &inputs.home,
    )
    .ok()?;
    let cache = match inputs
        .env
        .get("XDG_CACHE_HOME")
        .filter(|v| v.starts_with('/'))
    {
        Some(v) => PathBuf::from(v),
        None if cfg!(windows) => dirs::cache_dir()?,
        None => inputs.home.join(".cache"),
    }
    .join("dot");
    let name = match a.shell {
        ShellArg::Zsh => "env.zsh",
        ShellArg::Bash => "env.bash",
        ShellArg::Fish => "env.fish",
        ShellArg::Pwsh => "env.ps1",
    };
    let mut from = vec![inputs.local_env(), repo.root.join("dot.yml")];
    if let Some(state) = crate::render::logfile::default_state_dir(&inputs.home) {
        from.push(state.join(dot_core::state::STATE_FILE));
    }
    if let Ok(entries) = std::fs::read_dir(&repo.root) {
        from.extend(
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(".env"))
                .map(|e| e.path()),
        );
    }
    if let Ok(exe) = std::env::current_exe() {
        from.push(exe);
    }
    Some((cache.join(name), from))
}

/// Whether `path` exists and is newer than every one of `inputs` that exists.
fn fresh(path: &std::path::Path, inputs: &[PathBuf]) -> bool {
    let modified = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let Some(at) = modified(path) else {
        return false;
    };
    inputs.iter().filter_map(|p| modified(p)).all(|t| t <= at)
}

/// Whether a declared variable goes into a shell's environment: an environment-style name, with
/// no lowercase letters (`GOPATH`, `WORKSPACE_DIR`). Lower-case names (`workspace_dir`) are
/// config variables for templates; shell steps still receive them, interactive shells do not.
/// Variables set by `env:` steps are written whatever their name.
fn is_shell_name(name: &str) -> bool {
    !name.chars().any(|c| c.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_cache_is_fresh_while_nothing_it_comes_from_changed() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("local.env");
        let cache = dir.path().join("env.zsh");
        std::fs::write(&input, "A=1\n").unwrap();
        assert!(!super::fresh(&cache, std::slice::from_ref(&input)));
        std::fs::write(&cache, "x").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&cache)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(super::fresh(
            &cache,
            &[input.clone(), dir.path().join("gone")]
        ));
        std::fs::File::options()
            .write(true)
            .open(&input)
            .unwrap()
            .set_modified(later + std::time::Duration::from_secs(5))
            .unwrap();
        assert!(!super::fresh(&cache, &[input]));
    }

    #[test]
    fn only_environment_style_names_reach_the_shell() {
        assert!(super::is_shell_name("WORKSPACE_DIR"));
        assert!(super::is_shell_name("GO111MODULE"));
        assert!(!super::is_shell_name("workspace_dir"));
        assert!(!super::is_shell_name("Mixed_Case"));
    }
}
