//! `git`: a checkout of a repository, cloned over https and kept up to date.
//!
//! - Missing (or an empty directory): cloned.
//! - A clean checkout of the same remote: fast-forwarded to the remote branch (the configured
//!   `branch:`, else the checkout's current branch). Planning compares the local `HEAD` with
//!   the remote tip from `git ls-remote`, so an up-to-date checkout is satisfied without
//!   fetching; with the remote unreachable the status is unknown.
//! - Local changes, local commits that diverge, a different remote or branch, or a directory
//!   that is not a checkout: a conflict ([`Status::Conflict`]), never touched (the step fails
//!   when applied, like a `link` conflict). A checkout already at the remote tip is satisfied even if
//!   dirty, since there is nothing to do.
//!
//! Shells out to `git`, with terminal prompts disabled (`GIT_TERMINAL_PROMPT=0`): credentials
//! for a private repository must come from a credential helper.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;

use anyhow::{Context as _, Result, bail};
use dot_core::config::GitSpec;
use dot_core::config::network::check_git_url;
use dot_core::{Action, Outcome, PlanCtx, Resource, RunCtx, Status, Undo};

use crate::shell::run_captured;

/// How `git` is run.
#[derive(Debug, Clone)]
pub struct Git {
    pub program: PathBuf,
    /// Added to every invocation's environment (tests use it to isolate git's config).
    pub env: Vec<(OsString, OsString)>,
}

impl Default for Git {
    fn default() -> Self {
        Self {
            program: PathBuf::from("git"),
            env: Vec::new(),
        }
    }
}

impl Git {
    fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.env("GIT_TERMINAL_PROMPT", "0");
        c.envs(self.env.iter().map(|(k, v)| (k, v)));
        c
    }

    /// Runs a read-only query; `Ok(None)` when git exits non-zero.
    fn query(&self, dir: Option<&Path>, args: &[&str]) -> io::Result<Option<String>> {
        let mut c = self.command();
        if let Some(d) = dir {
            c.arg("-C").arg(d);
        }
        let out: Output = c.args(args).stdin(Stdio::null()).output()?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
    }
}

pub fn plan(spec: &GitSpec, ctx: &PlanCtx<'_>, git: &Git) -> Result<Vec<Box<dyn Action>>> {
    // `repo` and `branch` go on git's command line (readable by every local user) and into
    // `.git/config`: no secrets there.
    let text = |what: &str, s: &str| -> Result<String> {
        let r = ctx.render_text(s, &format!("git {what}"))?;
        if !r.secrets.is_empty() {
            return Err(ctx.config_error(format!(
                "git {what}: secret() cannot be used here: it would be on git's command line \
                 and saved in .git/config; let a git credential helper supply credentials"
            )));
        }
        Ok(r.text)
    };
    let url = text("repo", &spec.repo)?;
    check_git_url(&url).map_err(|e| ctx.config_error(e))?;
    let dest = ctx.target(&spec.dest)?;
    let branch = spec
        .branch
        .as_deref()
        .map(|b| text("branch", b))
        .transpose()?;
    if let Some(b) = &branch
        && (b.trim().is_empty() || b.starts_with('-'))
    {
        return Err(ctx.config_error(format!("git: `{b}` is not a branch name")));
    }
    Ok(vec![Box::new(GitAction {
        show: ctx.show(&dest),
        url,
        dest,
        branch,
        update: spec.update,
        depth: spec.depth,
        git: git.clone(),
        tips: Mutex::default(),
    })])
}

pub struct GitAction {
    url: String,
    dest: PathBuf,
    show: String,
    branch: Option<String>,
    update: bool,
    /// `git clone --depth`.
    depth: Option<u32>,
    git: Git,
    /// Remote tips by branch, looked up once (`git ls-remote` is a network round trip).
    tips: Mutex<HashMap<String, std::result::Result<String, String>>>,
}

/// What is at the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Local {
    /// Nothing, or an empty directory: clone.
    Missing,
    /// Something that must not be touched, and why.
    Conflict(String),
    /// A checkout of the right remote, on `branch`, at `head`.
    Checkout {
        branch: String,
        head: Option<String>,
        dirty: bool,
    },
    /// `git` is not installed yet.
    NoGit,
}

/// What the step would do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    Clone,
    UpToDate(&'static str),
    Update { from: Option<String>, to: String },
    Conflict(String),
    Unknown(String),
}

/// A URL compared loosely: no trailing `/` or `.git`.
fn same_remote(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> &str {
        let s = s.trim().trim_end_matches('/');
        s.strip_suffix(".git").unwrap_or(s).trim_end_matches('/')
    }
    norm(a) == norm(b)
}

fn os_args(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

impl GitAction {
    fn inspect(&self) -> io::Result<Local> {
        let meta = match std::fs::symlink_metadata(&self.dest) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Local::Missing),
            Err(e) => return Err(e),
        };
        if !self.dest.is_dir() {
            let what = if meta.file_type().is_symlink() {
                "a symlink that is not a directory"
            } else {
                "a file"
            };
            return Ok(Local::Conflict(format!("{} is {what}", self.show)));
        }
        if std::fs::read_dir(&self.dest)?.next().is_none() {
            return Ok(Local::Missing);
        }
        if !self.dest.join(".git").exists() {
            return Ok(Local::Conflict(format!(
                "{} exists and is not a git checkout",
                self.show
            )));
        }
        let q = |args: &[&str]| self.git.query(Some(&self.dest), args);
        let remote = match q(&["config", "--get", "remote.origin.url"]) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Local::NoGit),
            r => r?,
        };
        match remote {
            None => {
                return Ok(Local::Conflict(format!(
                    "{} is a checkout with no `origin` remote",
                    self.show
                )));
            }
            Some(r) if !same_remote(&r, &self.url) => {
                return Ok(Local::Conflict(format!(
                    "{} is a checkout of {r}, not {}",
                    self.show, self.url
                )));
            }
            Some(_) => {}
        }
        let current = q(&["symbolic-ref", "-q", "--short", "HEAD"])?;
        let branch = match (&self.branch, current) {
            (Some(want), Some(cur)) if *want != cur => {
                return Ok(Local::Conflict(format!(
                    "{} is on branch {cur}, not {want}",
                    self.show
                )));
            }
            (_, Some(cur)) => cur,
            (_, None) => {
                return Ok(Local::Conflict(format!(
                    "{} has a detached HEAD",
                    self.show
                )));
            }
        };
        let head = q(&["rev-parse", "-q", "--verify", "HEAD"])?;
        let dirty =
            q(&["status", "--porcelain", "--untracked-files=no"])?.is_none_or(|s| !s.is_empty());
        Ok(Local::Checkout {
            branch,
            head,
            dirty,
        })
    }

    /// The remote tip of `branch`, looked up once.
    fn tip(&self, branch: &str) -> std::result::Result<String, String> {
        let mut tips = self.tips.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = tips.get(branch) {
            return t.clone();
        }
        let r = self.ls_remote(branch);
        tips.insert(branch.to_string(), r.clone());
        r
    }

    fn ls_remote(&self, branch: &str) -> std::result::Result<String, String> {
        let refname = format!("refs/heads/{branch}");
        let out = self
            .git
            .command()
            .args(["ls-remote", "--exit-code", "--", &self.url, &refname])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("running git: {e}"))?;
        if out.status.code() == Some(2) {
            return Err(format!("branch {branch} does not exist on {}", self.url));
        }
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            return Err(format!("cannot reach {}: {}", self.url, err.trim()));
        }
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .map(str::to_string)
            .ok_or_else(|| format!("unexpected reply from {}", self.url))
    }

    /// Whether commit `tip` is already contained in the local `HEAD` (local commits ahead).
    fn contains(&self, tip: &str) -> bool {
        let ok = |args: &[&str]| {
            self.git
                .command()
                .arg("-C")
                .arg(&self.dest)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        ok(&["cat-file", "-e", &format!("{tip}^{{commit}}")])
            && ok(&["merge-base", "--is-ancestor", tip, "HEAD"])
    }

    fn plan(&self) -> io::Result<Plan> {
        Ok(match self.inspect()? {
            Local::Missing => Plan::Clone,
            Local::NoGit => Plan::Unknown(
                "git is not installed yet; an earlier step can install it (`package: [git]`)"
                    .into(),
            ),
            Local::Conflict(why) => Plan::Conflict(why),
            Local::Checkout { .. } if !self.update => Plan::UpToDate("update: false"),
            Local::Checkout {
                branch,
                head,
                dirty,
            } => match self.tip(&branch) {
                Err(e) => Plan::Unknown(e),
                Ok(tip) if head.as_deref() == Some(tip.as_str()) => Plan::UpToDate("up to date"),
                Ok(tip) if head.is_some() && self.contains(&tip) => {
                    Plan::UpToDate("ahead of the remote")
                }
                Ok(_) if dirty => Plan::Conflict(format!(
                    "{} has local changes; commit or stash them",
                    self.show
                )),
                Ok(tip) => Plan::Update {
                    from: head,
                    to: tip,
                },
            },
        })
    }

    fn run(&self, ctx: &RunCtx, args: &[OsString], what: &str) -> Result<()> {
        let mut c = self.git.command();
        c.args(args);
        let captured = run_captured(c, ctx).with_context(|| {
            format!("running {} (is git installed?)", self.git.program.display())
        })?;
        if !captured.status.success() {
            let last = captured
                .lines
                .iter()
                .rev()
                .map(|(_, l)| l.trim())
                .find(|l| !l.is_empty())
                .unwrap_or("");
            bail!("{what} failed: {last}");
        }
        Ok(())
    }
}

impl Action for GitAction {
    fn describe(&self) -> String {
        let branch = self
            .branch
            .as_deref()
            .map(|b| format!(" (branch {b})"))
            .unwrap_or_default();
        let base = format!("update {} from {}{branch}", self.show, self.url);
        match self.plan() {
            Ok(Plan::Clone) => format!("clone {} -> {}{branch}", self.url, self.show),
            Ok(Plan::UpToDate(why)) => format!("{base} ({why})"),
            Ok(Plan::Update { from, to }) => format!(
                "{base} ({} -> {})",
                from.as_deref().map(short).unwrap_or("empty"),
                short(&to)
            ),
            Ok(Plan::Conflict(why)) => format!("{base} (conflict: {why}; left untouched)"),
            Ok(Plan::Unknown(why)) => format!("{base} ({why})"),
            Err(e) => format!("{base} (cannot inspect: {e})"),
        }
    }

    fn status(&self) -> Result<Status> {
        let plan = self
            .plan()
            .with_context(|| format!("inspecting {}", self.dest.display()))?;
        Ok(match plan {
            Plan::UpToDate(_) => Status::Satisfied,
            Plan::Unknown(_) => Status::Unknown,
            Plan::Clone | Plan::Update { .. } => Status::Pending,
            Plan::Conflict(_) => Status::Conflict,
        })
    }

    /// `git:<dest>`. A checkout may hold work of its own: `dot remove` never deletes it.
    fn resources(&self) -> Vec<Resource> {
        vec![
            Resource::new("git", self.show.clone()).with_undo(Undo::Manual {
                note: format!(
                    "dot does not delete checkouts: remove {} by hand if you no longer want it",
                    self.show
                ),
            }),
        ]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let plan = self
            .plan()
            .with_context(|| format!("inspecting {}", self.dest.display()))?;
        match plan {
            Plan::UpToDate(_) => Ok(Outcome::Ok),
            Plan::Conflict(why) => bail!("{why}; left untouched"),
            Plan::Unknown(why) => bail!("{why}"),
            Plan::Clone => {
                if let Some(parent) = self.dest.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                let mut args = os_args(&["clone"]);
                if let Some(b) = &self.branch {
                    args.extend(os_args(&["--branch", b]));
                }
                if let Some(d) = self.depth {
                    args.extend(os_args(&["--depth", &d.to_string()]));
                }
                args.extend(os_args(&["--", &self.url]));
                args.push(self.dest.clone().into_os_string());
                self.run(ctx, &args, "git clone")?;
                Ok(Outcome::Changed)
            }
            Plan::Update { .. } => {
                let Local::Checkout { branch, .. } = self.inspect()? else {
                    bail!("{} changed while it was being updated", self.show);
                };
                let in_dest = |rest: &[&str]| {
                    let mut a = vec![OsString::from("-C"), self.dest.clone().into_os_string()];
                    a.extend(os_args(rest));
                    a
                };
                let refspec = format!("refs/heads/{branch}");
                self.run(ctx, &in_dest(&["fetch", "origin", &refspec]), "git fetch")?;
                self.run(
                    ctx,
                    &in_dest(&["merge", "--ff-only", "FETCH_HEAD"]),
                    "fast-forward (local commits diverge from the remote?)",
                )?;
                Ok(Outcome::Changed)
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
