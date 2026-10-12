//! The little git knowledge the guardrails need, by shelling out to `git`. A missing `git`
//! binary means nothing can be committed, so every path is treated as outside a repository.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

/// How to run `git`: the program and extra environment. Tests point `HOME` and the global
/// config at a temp dir so the user's own git configuration never leaks in.
#[derive(Debug, Clone)]
pub struct Git {
    program: OsString,
    envs: Vec<(OsString, OsString)>,
}

impl Default for Git {
    fn default() -> Self {
        Self::system()
    }
}

/// Where a path sits relative to git.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitPlacement {
    /// `git` is not installed.
    NoGit,
    /// Not inside any work tree.
    OutsideWorkTree,
    /// Inside a work tree, and ignored (so it cannot be committed by accident).
    Ignored,
    /// Inside a work tree and not ignored (or already tracked).
    NotIgnored,
}

impl Git {
    /// `git` from `PATH`.
    pub fn system() -> Self {
        Self {
            program: "git".into(),
            envs: Vec::new(),
        }
    }

    /// A specific executable (a path that does not exist behaves like git being absent).
    pub fn with_program(mut self, program: impl Into<OsString>) -> Self {
        self.program = program.into();
        self
    }

    /// An extra environment variable for every git invocation.
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    /// A `git` command running in `dir`, with the repository-selecting variables a hook might
    /// have set cleared so `dir` alone decides the repository.
    pub fn command(&self, dir: &Path) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.current_dir(dir)
            .stdin(Stdio::null())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_PREFIX");
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
        cmd
    }

    /// Runs git; `Ok(None)` when the program does not exist.
    fn run<I, S>(&self, dir: &Path, args: I) -> Result<Option<Output>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        match self.command(dir).args(args).output() {
            Ok(out) => Ok(Some(out)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("cannot run git"),
        }
    }

    /// Whether `path` (which need not exist) is inside a work tree, and if so whether git
    /// ignores it.
    pub fn placement(&self, path: &Path) -> Result<GitPlacement> {
        let (dir, rel) = existing_ancestor(path)?;
        let Some(out) = self.run(&dir, ["rev-parse", "--is-inside-work-tree"])? else {
            return Ok(GitPlacement::NoGit);
        };
        if !out.status.success() || String::from_utf8_lossy(&out.stdout).trim() != "true" {
            return Ok(GitPlacement::OutsideWorkTree);
        }
        let mut args: Vec<OsString> = vec!["check-ignore".into(), "-q".into(), "--".into()];
        args.push(rel.into_os_string());
        let Some(out) = self.run(&dir, &args)? else {
            return Ok(GitPlacement::NoGit);
        };
        match out.status.code() {
            Some(0) => Ok(GitPlacement::Ignored),
            Some(1) => Ok(GitPlacement::NotIgnored),
            _ => bail!(
                "git check-ignore failed for {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
    }

    /// Paths (relative to the work tree root) of every tracked file; `None` without git or
    /// outside a work tree. Returns the root too.
    pub(crate) fn tracked_files(&self, dir: &Path) -> Result<Option<(PathBuf, Vec<PathBuf>)>> {
        let Some(out) = self.run(dir, ["rev-parse", "--show-toplevel"])? else {
            return Ok(None);
        };
        if !out.status.success() {
            return Ok(None);
        }
        let root =
            PathBuf::from(String::from_utf8_lossy(&out.stdout).trim_end_matches(['\n', '\r']));
        let Some(out) = self.run(&root, ["ls-files", "-z"])? else {
            return Ok(None);
        };
        if !out.status.success() {
            bail!(
                "git ls-files failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let files = out
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| PathBuf::from(String::from_utf8_lossy(p).into_owned()))
            .collect();
        Ok(Some((root, files)))
    }

    /// The staged (index) content of `rel` in the work tree at `root`.
    pub(crate) fn staged_content(&self, root: &Path, rel: &Path) -> Result<Vec<u8>> {
        let spec = format!(":{}", rel.to_string_lossy().replace('\\', "/"));
        let out = self
            .run(root, ["show", spec.as_str()])?
            .context("git disappeared")?;
        if !out.status.success() {
            bail!(
                "cannot read {} from the index: {}",
                rel.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }
}

/// The nearest existing ancestor directory of `path` (canonicalized, so git sees the same
/// path it would report) and `path` relative to it.
fn existing_ancestor(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let mut parts: Vec<&OsStr> = path.file_name().into_iter().collect();
    let mut dir = path.parent();
    while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
        if d.is_dir() {
            let canonical = d
                .canonicalize()
                .with_context(|| format!("cannot resolve {}", d.display()))?;
            return Ok((canonical, parts.iter().rev().collect()));
        }
        parts.extend(d.file_name());
        dir = d.parent();
    }
    let cwd = std::env::current_dir().context("no current directory")?;
    Ok((cwd, parts.iter().rev().collect()))
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A git isolated from the user's configuration, or `None` when git is not installed.
    pub fn isolated(tmp: &Path) -> Option<Git> {
        let git = Git::system()
            .with_env("HOME", tmp)
            .with_env("XDG_CONFIG_HOME", tmp.join("xdg"))
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("GIT_CONFIG_GLOBAL", tmp.join("no-gitconfig"));
        match git.command(tmp).arg("--version").output() {
            Ok(o) if o.status.success() => Some(git),
            _ => None,
        }
    }

    pub fn init(git: &Git, dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let out = git.command(dir).args(["init", "-q", "."]).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    pub fn add(git: &Git, dir: &Path, path: &str) {
        let out = git
            .command(dir)
            .args(["add", "-f", "--", path])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn placement_in_and_out_of_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(git) = isolated(tmp.path()) else {
            eprintln!("git not installed; skipping");
            return;
        };
        let outside = tmp.path().join("plain/secrets.env");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        assert_eq!(
            git.placement(&outside).unwrap(),
            GitPlacement::OutsideWorkTree
        );

        let repo = tmp.path().join("repo");
        init(&git, &repo);
        std::fs::write(repo.join(".gitignore"), "ignored.env\nsecret-dir/\n").unwrap();
        assert_eq!(
            git.placement(&repo.join("secrets.env")).unwrap(),
            GitPlacement::NotIgnored
        );
        assert_eq!(
            git.placement(&repo.join("ignored.env")).unwrap(),
            GitPlacement::Ignored
        );
        // Neither the file nor its directory exists yet.
        assert_eq!(
            git.placement(&repo.join("secret-dir/deep/x.pem")).unwrap(),
            GitPlacement::Ignored
        );
        assert_eq!(
            git.placement(&repo.join("new-dir/x.pem")).unwrap(),
            GitPlacement::NotIgnored
        );
    }

    #[test]
    fn missing_git_means_no_git() {
        let tmp = tempfile::tempdir().unwrap();
        let git = Git::system().with_program(tmp.path().join("no-such-git"));
        assert_eq!(
            git.placement(&tmp.path().join("x")).unwrap(),
            GitPlacement::NoGit
        );
        assert!(git.tracked_files(tmp.path()).unwrap().is_none());
    }
}
