//! Unpacked trees of `download: { extract }`: stripping the archive's leading directories,
//! swapping a tree in whole (`replace: true`, as this user or through sudo), and the `bin` links
//! into it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use dot_core::RunCtx;
use dot_platform::exec::Cmd;
use dot_platform::link::{LinkKind, LinkStrategy};

use crate::rootfile::RootFiles;

/// An archive's tree: replaced whole or merged, and the executables linked from it.
pub(super) struct Tree {
    pub replace: bool,
    pub strip: u32,
    pub bins: Vec<BinLink>,
    pub links: Arc<dyn LinkStrategy>,
}

/// A link in `bin_dir` to an executable of the tree.
pub(super) struct BinLink {
    pub link: PathBuf,
    pub show: String,
    /// A glob over paths in the tree (`bin/zed`, `Godot_v*`).
    pub pattern: String,
    pub glob: globset::GlobMatcher,
}

impl Tree {
    /// The file `b` points at in the tree at `root`: the shallowest match, then the first by
    /// name.
    pub fn target(b: &BinLink, root: &Path) -> Option<PathBuf> {
        let mut best: Option<(usize, PathBuf)> = None;
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            let Ok(entries) = fs::read_dir(root.join(&rel)) else {
                continue;
            };
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let path = rel.join(e.file_name());
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                if is_dir {
                    stack.push(path);
                    continue;
                }
                let depth = path.components().count();
                let key = path.to_string_lossy().replace('\\', "/");
                if b.glob.is_match(&key)
                    && best
                        .as_ref()
                        .is_none_or(|(d, p)| depth < *d || (depth == *d && path < *p))
                {
                    best = Some((depth, path));
                }
            }
        }
        best.map(|(_, p)| root.join(p))
    }

    /// Links that are not (yet) what they should be, with why; `Err` for one that cannot be
    /// made (something else is in the way).
    pub fn pending_links(&self, root: &Path) -> std::result::Result<Vec<String>, String> {
        let mut out = Vec::new();
        for b in &self.bins {
            let want = Self::target(b, root);
            match (fs::read_link(&b.link), &want) {
                (Ok(have), Some(w)) if have == *w => {}
                (Ok(have), _) if have.starts_with(root) || !b.link.exists() => {
                    out.push(format!("link {}", b.show));
                }
                (Ok(have), _) => {
                    return Err(format!(
                        "{} links to {}, not into the tree; remove it",
                        b.show,
                        have.display()
                    ));
                }
                (Err(_), _) if fs::symlink_metadata(&b.link).is_ok() => {
                    return Err(format!(
                        "{} exists and is not a link; remove it or pick another `bin_dir`",
                        b.show
                    ));
                }
                (Err(_), _) => out.push(format!("link {}", b.show)),
            }
        }
        Ok(out)
    }

    /// Makes every link point at its file in the tree at `root`.
    pub fn link(&self, root: &Path) -> Result<()> {
        for b in &self.bins {
            let Some(target) = Self::target(b, root) else {
                bail!(
                    "bin `{}`: nothing in {} matches it",
                    b.pattern,
                    root.display()
                );
            };
            if fs::read_link(&b.link).is_ok_and(|t| t == target) {
                continue;
            }
            if fs::read_link(&b.link).is_ok() {
                self.links
                    .remove(&b.link)
                    .with_context(|| format!("removing {}", b.link.display()))?;
            } else if fs::symlink_metadata(&b.link).is_ok() {
                bail!("{} exists and is not a link", b.link.display());
            }
            if let Some(parent) = b.link.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let kind = self.links.kind_for(false).map_err(anyhow::Error::msg)?;
            if kind != LinkKind::Symlink {
                bail!(
                    "{}: this machine cannot make symlinks; put the tree's directory on PATH \
                     with a `path:` step instead of `bin:`",
                    b.show
                );
            }
            self.links
                .create(kind, &target, &b.link)
                .with_context(|| format!("linking {} -> {}", b.link.display(), target.display()))?;
        }
        Ok(())
    }
}

/// With `strip`, the directory inside `out` that is the tree: each of the `n` levels must be
/// the only entry of the one above.
pub(super) fn stripped(out: &Path, n: u32) -> Result<PathBuf> {
    let mut dir = out.to_path_buf();
    for level in 0..n {
        let entries: Vec<fs::DirEntry> = fs::read_dir(&dir)?.collect::<io::Result<_>>()?;
        match entries.as_slice() {
            [one] if one.file_type()?.is_dir() => dir = one.path(),
            _ => bail!(
                "strip: {n}: level {} of the archive has {} entries, not one directory: {}",
                level + 1,
                entries.len(),
                entries
                    .iter()
                    .take(5)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    Ok(dir)
}

/// `<dir>/.<name>.dot-<what>`: a sibling of `dest` for swapping trees.
fn sibling(dest: &Path, what: &str) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.with_file_name(format!(".{name}.dot-{what}-{}", std::process::id()))
}

/// Swaps the tree `from` in as `dest` (this user's): the old tree is renamed aside and removed
/// once the new one is in place; one `dot` did not install (`foreign`) is kept as
/// `<dest>.dot-backup`.
pub(super) fn replace_tree(from: &Path, dest: &Path, foreign: bool, ctx: &RunCtx) -> Result<()> {
    let old = sibling(dest, "old");
    let had = fs::symlink_metadata(dest).is_ok();
    if had {
        fs::rename(dest, &old).with_context(|| format!("moving {} aside", dest.display()))?;
    }
    if let Err(e) = fs::rename(from, dest) {
        if had {
            let _ = fs::rename(&old, dest);
        }
        return Err(e).with_context(|| format!("installing {}", dest.display()));
    }
    if had {
        if foreign {
            let backup = crate::link::backup_path(dest);
            fs::rename(&old, &backup)
                .with_context(|| format!("keeping the old tree as {}", backup.display()))?;
            ctx.backed_up(&backup);
            ctx.output(
                dot_core::Stream::Stdout,
                format!(
                    "{} was not installed by dot: kept as {}",
                    dest.display(),
                    backup.display()
                ),
            );
        } else {
            fs::remove_dir_all(&old)
                .with_context(|| format!("removing the old tree {}", old.display()))?;
        }
    }
    Ok(())
}

/// [`replace_tree`] for a root-owned `dest`, through sudo: the new tree is copied next to it
/// (root-owned), then swapped in.
pub(super) fn replace_root_tree(
    from: &Path,
    dest: &Path,
    foreign: bool,
    r: &RootFiles,
    ctx: &RunCtx,
) -> Result<()> {
    let run = |cmd: Cmd| r.run(cmd, ctx);
    let parent = dest.parent().context("the destination has no parent")?;
    if fs::symlink_metadata(parent).is_err() {
        r.mkdir(parent, 0o755, ctx)?;
    }
    let new = sibling(dest, "new");
    let old = sibling(dest, "old");
    let s = |p: &Path| p.display().to_string();
    run(Cmd::new("rm").args(["-rf".to_string(), "--".into(), s(&new)]))?;
    run(Cmd::new("cp").args(["-R".to_string(), s(from), s(&new)]))?;
    run(Cmd::new("chown").args(["-R".to_string(), format!("root:{}", r.group), s(&new)]))?;
    let had = fs::symlink_metadata(dest).is_ok();
    if had {
        run(Cmd::new("mv").args([s(dest), s(&old)]))?;
    }
    if let Err(e) = run(Cmd::new("mv").args([s(&new), s(dest)])) {
        if had {
            let _ = run(Cmd::new("mv").args([s(&old), s(dest)]));
        }
        return Err(e);
    }
    if had {
        if foreign {
            let backup = crate::link::backup_path(dest);
            run(Cmd::new("mv").args([s(&old), s(&backup)]))?;
            ctx.backed_up(&backup);
        } else {
            run(Cmd::new("rm").args(["-rf".to_string(), "--".into(), s(&old)]))?;
        }
    }
    Ok(())
}
