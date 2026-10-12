//! `clean`: remove dangling symlinks that point into the repository.
//!
//! Besides `absent`, this is the only directive that deletes on its own, so it is deliberately narrow. A
//! path is removed only if, at the moment of removal, all of these hold:
//!
//! - it is a symlink (never a file or directory);
//! - its target does not exist (`NotFound`; loops and permission errors leave it alone);
//! - its target, resolved like `realpath`, lies inside the repository root.
//!
//! Directories inside the repository root are never scanned, and recursion never enters
//! symlinked directories (nor junctions, which count as symlinks).
//!
//! Where symlinks are not available, `link` makes copies instead and records them
//! ([`crate::linkstate`]). The copy counterpart of a dangling link is a recorded copy whose
//! source, inside the repository, no longer exists: it is removed too, but only while it still
//! holds exactly what `dot` copied there (an edited copy is the user's file now), and its record
//! is dropped.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use dot_core::config::CleanEntry;
use dot_core::paths::resolve_lenient;
use dot_core::{Action, Outcome, PlanCtx, RunCtx, Status};
use dot_platform::link::{LinkKind, LinkStrategy};

use crate::linkstate::{Fallback, LinkState};

pub fn plan(
    entries: &[CleanEntry],
    ctx: &PlanCtx<'_>,
    links: &Arc<dyn LinkStrategy>,
) -> Result<Vec<Box<dyn Action>>> {
    // Resolved leniently rather than with `canonicalize`: on Windows that returns a verbatim
    // `\\?\C:\...` path, which never compares equal to the ordinary `C:\...` link targets.
    let root = std::path::absolute(&ctx.bundle.root)
        .ok()
        .and_then(|r| resolve_lenient(&r))
        .with_context(|| format!("resolving repository root {}", ctx.bundle.root.display()))?;
    let state = LinkState::in_dir(ctx.state_dir);
    let copies: Vec<Fallback> = state
        .all()
        .into_iter()
        .filter(|f| f.kind() == Some(LinkKind::Copy))
        .collect();
    let mut actions: Vec<Box<dyn Action>> = Vec::new();
    for e in entries {
        let dir = ctx.target(&e.path)?;
        for link in scan(&dir, e.recursive, &root)? {
            let target = fs::read_link(&link).unwrap_or_default();
            actions.push(Box::new(CleanAction {
                show: format!("{} -> {}", ctx.show(&link), ctx.show(&target)),
                link,
                root: root.clone(),
                links: links.clone(),
            }));
        }
        for copy in &copies {
            let under = if e.recursive {
                copy.dest.starts_with(&dir) && copy.dest != dir
            } else {
                copy.dest.parent() == Some(dir.as_path())
            };
            if under && !inside_root(&copy.dest, &root) && stale_copy(copy, &root) {
                actions.push(Box::new(CleanCopyAction {
                    show: format!(
                        "{} (a copy of {})",
                        ctx.show(&copy.dest),
                        ctx.show(&copy.src)
                    ),
                    copy: copy.clone(),
                    root: root.clone(),
                    state: state.clone(),
                }));
            }
        }
    }
    Ok(actions)
}

/// The removal rule for recorded copies: the source is gone, was inside the repository, and the
/// copy is a regular file that still holds what `dot` wrote.
pub fn stale_copy(copy: &Fallback, root: &Path) -> bool {
    let gone = matches!(
        fs::symlink_metadata(&copy.src),
        Err(e) if e.kind() == io::ErrorKind::NotFound
    );
    let in_repo = resolve_lenient(&copy.src).is_some_and(|p| p.starts_with(root));
    let file = fs::symlink_metadata(&copy.dest).is_ok_and(|m| m.file_type().is_file());
    gone && in_repo && file && copy.untouched()
}

/// Dangling in-repo links under `dir`, sorted. A missing `dir` has nothing to clean.
fn scan(dir: &Path, recursive: bool, root: &Path) -> Result<Vec<PathBuf>> {
    match fs::metadata(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        Ok(m) if !m.is_dir() => bail!("{} is not a directory", dir.display()),
        Ok(_) => {}
    }
    if inside_root(dir, root) {
        bail!(
            "refusing to clean {}: it is inside the repository",
            dir.display()
        );
    }
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut top = true;
    while let Some(d) = stack.pop() {
        let entries = match fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) if top => {
                return Err(e).with_context(|| format!("reading {}", d.display()));
            }
            // Unreadable subdirectories are skipped: clean only ever removes less.
            Err(_) => continue,
        };
        top = false;
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_symlink() {
                if dangling_in_root(&path, root) {
                    found.push(path);
                }
            } else if ft.is_dir() && recursive && !inside_root(&path, root) {
                stack.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

fn inside_root(path: &Path, root: &Path) -> bool {
    resolve_lenient(path).is_none_or(|p| p.starts_with(root))
}

/// The removal rule. `root` must be resolved with [`resolve_lenient`], so that it and the link
/// targets use the same path spelling.
pub fn dangling_in_root(link: &Path, root: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(link) else {
        return false;
    };
    if !meta.file_type().is_symlink() {
        return false;
    }
    match fs::metadata(link) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        _ => return false, // target exists, or can't tell: keep it
    }
    let Ok(target) = fs::read_link(link) else {
        return false;
    };
    let Some(parent) = link.parent() else {
        return false;
    };
    let abs = if target.is_absolute() {
        target
    } else {
        parent.join(target)
    };
    match resolve_lenient(&abs) {
        Some(resolved) => resolved.starts_with(root),
        None => false,
    }
}

pub struct CleanAction {
    pub link: PathBuf,
    root: PathBuf,
    show: String,
    links: Arc<dyn LinkStrategy>,
}

impl Action for CleanAction {
    fn describe(&self) -> String {
        format!("remove dangling link {}", self.show)
    }

    fn status(&self) -> Result<Status> {
        Ok(if dangling_in_root(&self.link, &self.root) {
            Status::Pending
        } else {
            Status::Satisfied
        })
    }

    fn apply(&self, _ctx: &RunCtx) -> Result<Outcome> {
        // Re-check immediately before deleting.
        if !dangling_in_root(&self.link, &self.root) {
            return Ok(Outcome::Ok);
        }
        self.links
            .remove(&self.link)
            .with_context(|| format!("removing {}", self.link.display()))?;
        Ok(Outcome::Changed)
    }
}

/// Removes a copy `link` made (symlinks unavailable) whose source is gone: see [`stale_copy`].
pub struct CleanCopyAction {
    copy: Fallback,
    root: PathBuf,
    show: String,
    state: LinkState,
}

impl Action for CleanCopyAction {
    fn describe(&self) -> String {
        format!("remove stale copy {}", self.show)
    }

    fn status(&self) -> Result<Status> {
        Ok(if stale_copy(&self.copy, &self.root) {
            Status::Pending
        } else {
            Status::Satisfied
        })
    }

    fn apply(&self, _ctx: &RunCtx) -> Result<Outcome> {
        // Re-check immediately before deleting.
        if !stale_copy(&self.copy, &self.root) {
            return Ok(Outcome::Ok);
        }
        fs::remove_file(&self.copy.dest)
            .with_context(|| format!("removing {}", self.copy.dest.display()))?;
        self.state.forget(&self.copy.dest)?;
        Ok(Outcome::Changed)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::testutil::Fixture;

    fn exists_as_link(p: &Path) -> bool {
        fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
    }

    /// home/
    ///   in_repo_dangling   -> <repo>/bundles/demo/gone         (removed)
    ///   in_repo_relative   -> ../<...>/repo/gone (relative)     (removed)
    ///   in_repo_valid      -> <repo>/bundles/demo/present       (kept: not dangling)
    ///   out_dangling       -> <home>/nowhere                    (kept: outside repo)
    ///   out_valid          -> <home>/real.txt                   (kept)
    ///   real.txt, realdir/                                      (kept: not links)
    ///   sub/in_repo_dangling_deep -> <repo>/gone2               (removed only if recursive)
    ///   linkdir -> <outside>/  containing a dangling in-repo link (never entered)
    fn setup(recursive: bool) -> (Fixture, tempfile::TempDir) {
        let yaml = if recursive {
            "steps:\n  - clean:\n      ~/: { recursive: true }\n"
        } else {
            "steps:\n  - clean: [~/]\n"
        };
        let f = Fixture::new(yaml);
        let h = f.home().to_path_buf();
        f.write_repo("present", "");
        symlink(f.repo().join("gone"), h.join("in_repo_dangling")).unwrap();
        let rel = pathdiff(&f.repo().join("gone-rel"), &h);
        symlink(rel, h.join("in_repo_relative")).unwrap();
        symlink(f.repo().join("present"), h.join("in_repo_valid")).unwrap();
        symlink(h.join("nowhere"), h.join("out_dangling")).unwrap();
        fs::write(h.join("real.txt"), "keep").unwrap();
        fs::create_dir(h.join("realdir")).unwrap();
        symlink(h.join("real.txt"), h.join("out_valid")).unwrap();
        fs::create_dir(h.join("sub")).unwrap();
        symlink(f.root().join("gone2"), h.join("sub/in_repo_dangling_deep")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(f.root().join("gone3"), outside.path().join("behind_link")).unwrap();
        symlink(outside.path(), h.join("linkdir")).unwrap();
        (f, outside)
    }

    /// `target` expressed relative to `from` (both absolute, sharing the temp root).
    fn pathdiff(target: &Path, from: &Path) -> PathBuf {
        let t: Vec<_> = target.components().collect();
        let f: Vec<_> = from.components().collect();
        let common = t.iter().zip(&f).take_while(|(a, b)| a == b).count();
        let mut out = PathBuf::new();
        for _ in common..f.len() {
            out.push("..");
        }
        for c in &t[common..] {
            out.push(c);
        }
        out
    }

    fn run(f: &Fixture) -> Vec<String> {
        let actions = f.plan(0).unwrap();
        let described = actions.iter().map(|a| a.describe()).collect();
        assert!(
            actions
                .iter()
                .all(|a| a.status().unwrap() == Status::Pending)
        );
        assert!(f.apply(&actions).iter().all(|o| *o == Outcome::Changed));
        described
    }

    fn assert_kept(f: &Fixture, outside: &Path) {
        let h = f.home();
        for keep in ["in_repo_valid", "out_dangling", "out_valid", "linkdir"] {
            assert!(exists_as_link(&h.join(keep)), "{keep} should be kept");
        }
        assert_eq!(fs::read_to_string(h.join("real.txt")).unwrap(), "keep");
        assert!(h.join("realdir").is_dir());
        assert!(exists_as_link(&outside.join("behind_link")));
    }

    #[test]
    fn removes_only_dangling_in_repo_links() {
        let (f, outside) = setup(false);
        let described = run(&f);
        assert_eq!(described.len(), 2, "{described:?}");
        assert!(described[0].starts_with("remove dangling link ~/in_repo_dangling -> "));
        let h = f.home();
        assert!(!exists_as_link(&h.join("in_repo_dangling")));
        assert!(!exists_as_link(&h.join("in_repo_relative")));
        assert!(
            exists_as_link(&h.join("sub/in_repo_dangling_deep")),
            "not recursive"
        );
        assert_kept(&f, outside.path());
        // Nothing left to do.
        assert!(f.plan(0).unwrap().is_empty());
    }

    #[test]
    fn recursive_descends_real_dirs_only() {
        let (f, outside) = setup(true);
        let described = run(&f);
        assert_eq!(described.len(), 3, "{described:?}");
        assert!(!exists_as_link(&f.home().join("sub/in_repo_dangling_deep")));
        assert_kept(&f, outside.path());
    }

    #[test]
    fn link_that_became_valid_is_kept() {
        let (f, _outside) = setup(false);
        let actions = f.plan(0).unwrap();
        // The target appears between plan and apply.
        fs::write(f.repo().join("gone"), "").unwrap();
        fs::write(f.repo().join("gone-rel"), "").unwrap();
        assert!(
            actions
                .iter()
                .all(|a| a.status().unwrap() == Status::Satisfied)
        );
        assert_eq!(f.apply(&actions), [Outcome::Ok, Outcome::Ok]);
        assert!(exists_as_link(&f.home().join("in_repo_dangling")));
    }

    #[test]
    fn link_replaced_by_a_file_is_kept() {
        let (f, _outside) = setup(false);
        let actions = f.plan(0).unwrap();
        let p = f.home().join("in_repo_dangling");
        fs::remove_file(&p).unwrap();
        fs::write(&p, "now a file").unwrap();
        f.apply(&actions);
        assert_eq!(fs::read_to_string(&p).unwrap(), "now a file");
    }

    #[test]
    fn dangling_through_a_link_that_leaves_the_repo_is_kept() {
        // <repo>/hop -> <home>/elsewhere (dangling); ~/x -> <repo>/hop/file. Lexically inside
        // the repo, but really points outside it.
        let f = Fixture::new("steps:\n  - clean: [~/]\n");
        symlink(f.home().join("elsewhere"), f.root().join("hop")).unwrap();
        symlink(f.root().join("hop/file"), f.home().join("x")).unwrap();
        assert!(f.plan(0).unwrap().is_empty());
    }

    #[test]
    fn root_and_targets_compare_in_the_same_spelling() {
        let (f, _outside) = setup(false);
        let root = resolve_lenient(f.root()).unwrap();
        assert!(dangling_in_root(&f.home().join("in_repo_dangling"), &root));
        assert!(inside_root(&f.repo(), &root));
        // A non-canonical spelling of the root still matches (the fixture's temp dirs sit
        // behind a symlink on macOS, like `\\?\` prefixes differ on Windows).
        let messy = f.root().join("bundles/../bundles/..");
        let root = resolve_lenient(&messy).unwrap();
        assert!(dangling_in_root(&f.home().join("in_repo_dangling"), &root));
    }

    #[test]
    fn missing_directory_is_nothing_to_do() {
        let f = Fixture::new("steps:\n  - clean: [~/does/not/exist]\n");
        assert!(f.plan(0).unwrap().is_empty());
    }

    #[test]
    fn refuses_to_clean_inside_the_repo() {
        let f = Fixture::new("steps:\n  - clean: [~/r]\n");
        symlink(f.root(), f.home().join("r")).unwrap();
        symlink(f.root().join("gone"), f.root().join("committed_dangling")).unwrap();
        let err = format!("{:#}", f.plan(0).err().unwrap());
        assert!(err.contains("inside the repository"), "{err}");
        assert!(exists_as_link(&f.root().join("committed_dangling")));
    }

    #[test]
    fn recursive_scan_skips_repo_inside_home() {
        // ~/dotfiles is the repo; a dangling in-repo link committed inside it must survive a
        // recursive clean of ~ while one in ~ itself is removed.
        let f = Fixture::repo_in_home("steps:\n  - clean:\n      ~/: { recursive: true }\n");
        let committed = f.root().join("bundles/committed_dangling");
        symlink(f.root().join("gone"), &committed).unwrap();
        symlink(f.root().join("gone"), f.home().join("stale")).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions.len(), 1);
        f.apply(&actions);
        assert!(!exists_as_link(&f.home().join("stale")));
        assert!(exists_as_link(&committed));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// `canonicalize` gives `\\?\C:\...`; ordinary paths are `C:\...`. Both spellings of the root
    /// must see the same paths as inside it.
    #[test]
    fn verbatim_and_plain_roots_agree() {
        // `%TEMP%` may hold an 8.3 short name (`C:\Users\RUNNER~1\...`) where `canonicalize`
        // gives the long one: resolution spells both the long way, so they agree too.
        let dir = tempfile::tempdir().unwrap();
        let verbatim = dir.path().canonicalize().unwrap();
        assert!(verbatim.to_string_lossy().starts_with(r"\\?\"));
        let root = resolve_lenient(&verbatim).unwrap();
        assert_eq!(root, resolve_lenient(dir.path()).unwrap());
        assert!(inside_root(&dir.path().join("sub"), &root));
        assert!(inside_root(&verbatim.join("sub"), &root));
        assert!(!inside_root(&std::env::temp_dir().join("elsewhere"), &root));
        // A dangling link target in plain spelling, compared against the resolved root.
        let target = dir.path().join("gone");
        assert!(resolve_lenient(&target).unwrap().starts_with(&root));
    }
}
