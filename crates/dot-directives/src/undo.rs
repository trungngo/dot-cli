//! `dot remove`: undoing what the state store recorded for a bundle ([`dot_core::state`]).
//!
//! Each record becomes a [`Removal`]: [`Removal::check`] says what would happen without
//! changing anything (for `--dry-run` and the confirmation), [`Removal::apply`] does it. Every
//! undo is conservative: a link that points elsewhere now, a file edited since `dot` wrote it, a
//! directory with files in it, are left alone and reported. What `dot` did not make (it was
//! already in place when `dot` first met it) is left; packages are uninstalled only when asked
//! (`--packages`), and only the ones `dot` installed.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use dot_core::config::PkgManager;
use dot_core::paths::{display, simplify};
use dot_core::state::{Record, Restore};
use dot_core::{StateStore, Stream, Undo, Written};
use dot_platform::exec::{Cmd, OutFn, System};
use dot_platform::link::LinkStrategy;
use dot_platform::pkg::Packages;

use crate::block::unsplice;
use crate::fileutil::{Existing, atomic_write, sha256_hex, tree_fingerprint, unwritable};
use crate::github_release::StateFile;
use crate::linkstate::{LinkState, sha256_file};

/// What removes things: the run's link strategy, package managers and system commands.
#[derive(Clone)]
pub struct Remover {
    pub links: Arc<dyn LinkStrategy>,
    pub packages: Packages,
    pub system: System,
    pub store: StateStore,
    pub home: PathBuf,
    /// `--packages`: uninstall the packages `dot` installed.
    pub uninstall: bool,
}

/// One recorded resource to undo.
pub struct Removal {
    pub key: String,
    pub record: Record,
    remover: Remover,
}

/// What removing a record would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Undo it (what, as a sentence).
    Undo(String),
    /// Nothing left to undo (already gone); the record is dropped.
    Gone(String),
    /// Left in place on purpose (not made by `dot`, a package without `--packages`, a
    /// directory with files in it). `forget`: the record is dropped too.
    Keep { why: String, forget: bool },
    /// Something `dot` cannot undo: what to do by hand. The record is dropped.
    Manual(String),
    /// Changed since `dot` made it: left alone, the record kept, the removal fails.
    Conflict(String),
}

impl Verdict {
    /// Whether the record goes once this is done.
    pub fn forgets(&self) -> bool {
        match self {
            Verdict::Undo(_) | Verdict::Gone(_) | Verdict::Manual(_) => true,
            Verdict::Keep { forget, .. } => *forget,
            Verdict::Conflict(_) => false,
        }
    }
}

impl Remover {
    /// `bundle`'s records as removals, newest first (what was made last is undone first: the
    /// files in a directory before the directory).
    pub fn plan(&self, bundle: &str) -> Vec<Removal> {
        self.store
            .read()
            .records_of(bundle)
            .into_iter()
            .map(|(key, record)| Removal {
                key,
                record,
                remover: self.clone(),
            })
            .collect()
    }

    fn show(&self, p: &Path) -> String {
        display(p, &self.home)
    }
}

/// Whether `link` is a link (or junction) to `target`.
fn links_to(link: &Path, target: &Path) -> bool {
    match fs::read_link(link) {
        Ok(t) => {
            simplify(&t) == simplify(target)
                || matches!((link.canonicalize(), target.canonicalize()), (Ok(a), Ok(b)) if a == b)
        }
        Err(_) => false,
    }
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

impl Removal {
    fn show(&self, p: &Path) -> String {
        self.remover.show(p)
    }

    /// The path the resource occupies, where a restore puts things back.
    fn target(&self) -> Option<PathBuf> {
        match self.record.undo.as_ref()? {
            Undo::Unlink { path, .. }
            | Undo::RemoveDir { path, .. }
            | Undo::RemoveTree { path, .. } => Some(path.clone()),
            Undo::RemoveFiles { files, .. } if files.len() == 1 => Some(files[0].path.clone()),
            _ => None,
        }
    }

    /// Needs root to apply: a root-owned file or directory this user cannot remove, a block in
    /// a root-owned file, a root-owned file to put back, or a package whose manager installs
    /// as root.
    pub fn needs_sudo(&self) -> bool {
        if self.remover.system.root || cfg!(windows) {
            return false;
        }
        if matches!(&self.record.restore, Some(Restore::RootBackup { path, .. }) if exists(path)) {
            return true;
        }
        match &self.record.undo {
            Some(Undo::RemoveFiles { files, root: true }) => files
                .iter()
                .any(|f| exists(&f.path) && unwritable(&f.path).is_some()),
            Some(Undo::RemoveDir { path, root: true })
            | Some(Undo::RemoveTree {
                path, root: true, ..
            }) => exists(path) && unwritable(path).is_some(),
            Some(Undo::RemoveBlock {
                path, root: true, ..
            }) => exists(path),
            Some(Undo::Uninstall { manager, .. }) if self.remover.uninstall => {
                PkgManager::parse(manager)
                    .is_some_and(|m| self.remover.packages.manager(m).needs_root())
            }
            _ => false,
        }
    }

    /// What removing would do; changes nothing.
    pub fn check(&self) -> Result<Verdict> {
        let Some(undo) = &self.record.undo else {
            return Ok(Verdict::Keep {
                why: "dot cannot undo it".into(),
                forget: true,
            });
        };
        if !self.record.owned {
            return Ok(Verdict::Keep {
                why: "it was already in place before dot".into(),
                forget: true,
            });
        }
        let restore = self.restore_note();
        let v = match undo {
            Undo::Unlink { path, target } => {
                let show = self.show(path);
                if !exists(path) {
                    Verdict::Gone(format!("{show} is already gone"))
                } else if links_to(path, target) {
                    Verdict::Undo(format!(
                        "remove link {show} -> {}{restore}",
                        self.show(target)
                    ))
                } else {
                    Verdict::Conflict(format!(
                        "{show} is no longer the link dot made to {}; left alone",
                        self.show(target)
                    ))
                }
            }
            Undo::RemoveFiles { files, .. } | Undo::Release { files, .. } => {
                let root = matches!(undo, Undo::RemoveFiles { root: true, .. });
                let present: Vec<&Written> = files.iter().filter(|f| exists(&f.path)).collect();
                let mut changed = Vec::new();
                let mut unchecked = Vec::new();
                let mut unknown = Vec::new();
                for f in &present {
                    match self.untouched(f, root) {
                        Held::Same => {}
                        Held::Changed => changed.push(self.show(&f.path)),
                        Held::NoChecksum => unchecked.push(self.show(&f.path)),
                        Held::Unreadable => unknown.push(self.show(&f.path)),
                    }
                }
                let shown: Vec<String> = present.iter().map(|f| self.show(&f.path)).collect();
                if !changed.is_empty() {
                    Verdict::Conflict(format!(
                        "{} changed since dot wrote {}; left alone",
                        changed.join(", "),
                        if changed.len() == 1 { "it" } else { "them" }
                    ))
                } else if !unchecked.is_empty() {
                    Verdict::Conflict(format!(
                        "{}: dot recorded no checksum of what it wrote, so it cannot tell \
                         whether {} changed since; left alone",
                        unchecked.join(", "),
                        if unchecked.len() == 1 { "it" } else { "they" }
                    ))
                } else if present.is_empty() && restore.is_empty() {
                    let all: Vec<String> = files.iter().map(|f| self.show(&f.path)).collect();
                    Verdict::Gone(format!("{} already gone", all.join(", ")))
                } else if present.is_empty() {
                    Verdict::Undo(restore.trim_start_matches("; ").to_string())
                } else if !unknown.is_empty() {
                    // Root-owned and unreadable until sudo is primed (`--dry-run`).
                    Verdict::Undo(format!(
                        "remove {}{restore} (compared as root when removing)",
                        shown.join(", ")
                    ))
                } else {
                    Verdict::Undo(format!("remove {}{restore}", shown.join(", ")))
                }
            }
            Undo::RemoveDir { path, .. } => {
                let show = self.show(path);
                match fs::read_dir(path) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        Verdict::Gone(format!("{show} is already gone"))
                    }
                    Err(e) => bail!("reading {show}: {e}"),
                    Ok(mut entries) => {
                        if entries.next().is_some() {
                            Verdict::Keep {
                                why: format!("{show} is not empty"),
                                forget: true,
                            }
                        } else {
                            Verdict::Undo(format!("remove directory {show}"))
                        }
                    }
                }
            }
            Undo::RemoveTree {
                path, fingerprint, ..
            } => {
                let show = self.show(path);
                if !exists(path) {
                    Verdict::Gone(format!("{show} is already gone"))
                } else {
                    match (fingerprint, tree_fingerprint(path)) {
                        (Some(want), Ok(have)) if have == *want => {
                            Verdict::Undo(format!("remove tree {show}{restore}"))
                        }
                        (Some(_), Ok(_)) => Verdict::Conflict(format!(
                            "{show} changed since dot unpacked it; left alone"
                        )),
                        (Some(_), Err(e)) => Verdict::Conflict(format!(
                            "{show}: cannot read all of it ({e}) to tell whether it changed; \
                             left alone"
                        )),
                        (None, _) => Verdict::Conflict(format!(
                            "{show}: dot recorded no fingerprint of the tree it unpacked, so it \
                             cannot tell whether it changed since; left alone"
                        )),
                    }
                }
            }
            Undo::RemoveBlock {
                path,
                begin,
                end,
                root,
                created_file,
                ..
            } => {
                let show = self.show(path);
                let Some(existing) = self.read(path, *root)? else {
                    // Root-owned and unreadable until sudo is primed (`--dry-run`).
                    return Ok(Verdict::Undo(format!(
                        "remove block [{}] from {show} (read as root when removing)",
                        marker(begin)
                    )));
                };
                match existing {
                    Existing::Missing => Verdict::Gone(format!("{show} is gone")),
                    Existing::File { bytes, .. } => {
                        let text = String::from_utf8_lossy(&bytes);
                        match unsplice(&text, begin, end) {
                            Some(rest) if *created_file && rest.trim().is_empty() => {
                                Verdict::Undo(format!(
                                    "remove {show} (dot created it for block [{}], and nothing \
                                     else is in it)",
                                    marker(begin)
                                ))
                            }
                            Some(_) => Verdict::Undo(format!(
                                "remove block [{}] from {show}",
                                marker(begin)
                            )),
                            None => Verdict::Gone(format!("{show} no longer has the block")),
                        }
                    }
                    _ => Verdict::Conflict(format!("{show} is not a regular file; left alone")),
                }
            }
            Undo::Uninstall { manager, name } => {
                if !self.remover.uninstall {
                    return Ok(Verdict::Keep {
                        why: format!("package {manager}:{name} (uninstalled only with --packages)"),
                        forget: false,
                    });
                }
                let Some(m) = PkgManager::parse(manager) else {
                    bail!("unknown package manager `{manager}` in the state store");
                };
                let pm = self.remover.packages.manager(m);
                if !pm.available() {
                    Verdict::Gone(format!("{manager} is not installed"))
                } else if pm.installed(std::slice::from_ref(name))?[0] {
                    Verdict::Undo(format!("uninstall {manager}:{name}"))
                } else {
                    Verdict::Gone(format!("{manager}:{name} is not installed"))
                }
            }
            Undo::RemoveUserPath { dir } => {
                if cfg!(windows) && !dot_platform::winpath::user_path_has(dir) {
                    Verdict::Gone(format!("{} is no longer in the user Path", dir.display()))
                } else if cfg!(windows) {
                    Verdict::Undo(format!(
                        "take {} out of the user Path (HKCU\\Environment)",
                        dir.display()
                    ))
                } else {
                    Verdict::Gone("the user Path in the registry exists only on Windows".into())
                }
            }
            Undo::Forget => Verdict::Undo(format!("drop {} from dot env", self.key)),
            Undo::Manual { note } => Verdict::Manual(note.clone()),
        };
        Ok(v)
    }

    fn restore_note(&self) -> String {
        match (&self.record.restore, self.target()) {
            (Some(Restore::Backup { path }), Some(t)) if exists(path) => {
                format!("; put back {} from {}", self.show(&t), self.show(path))
            }
            (Some(Restore::RootBackup { path, .. }), Some(t)) if exists(path) => {
                format!(
                    "; put back {} (as root) from {}",
                    self.show(&t),
                    self.show(path)
                )
            }
            (Some(Restore::Copy { src }), Some(t)) if exists(src) => {
                format!(
                    "; put a copy of {} back at {}",
                    self.show(src),
                    self.show(&t)
                )
            }
            _ => String::new(),
        }
    }

    /// Undoes the resource ([`Removal::check`] said [`Verdict::Undo`]). Removing a root-owned
    /// file runs `sudo -n` (the caller asked for sudo up front: [`Removal::needs_sudo`]).
    pub fn apply(&self, out: &mut OutFn<'_>) -> Result<()> {
        let Some(undo) = &self.record.undo else {
            return Ok(());
        };
        let r = &self.remover;
        match undo {
            Undo::Unlink { path, .. } => {
                r.links
                    .remove(path)
                    .with_context(|| format!("removing link {}", path.display()))?;
                LinkState {
                    store: r.store.clone(),
                }
                .forget(path)?;
            }
            Undo::RemoveFiles { files, root } => {
                self.remove_files(files, *root)?;
                let links = LinkState {
                    store: r.store.clone(),
                };
                for f in files {
                    links.forget(&f.path)?;
                }
            }
            Undo::Release { repo, dir, files } => {
                self.remove_files(files, false)?;
                StateFile {
                    store: r.store.clone(),
                }
                .forget(repo, dir)?;
            }
            Undo::RemoveDir { path, root } => {
                if *root && self.as_root(path) {
                    self.run_as_root(Cmd::new("rmdir").arg(path.display().to_string()))?;
                } else {
                    match fs::remove_dir(path) {
                        Err(e) if e.kind() != io::ErrorKind::NotFound => {
                            return Err(e).with_context(|| format!("removing {}", path.display()));
                        }
                        _ => {}
                    }
                }
            }
            Undo::RemoveTree {
                path,
                root,
                fingerprint,
            } => {
                if exists(path) {
                    if fingerprint.is_none() || tree_fingerprint(path).ok() != *fingerprint {
                        bail!("{} changed since dot unpacked it", path.display());
                    }
                    if *root && self.as_root(path) {
                        self.run_as_root(
                            Cmd::new("rm")
                                .args(["-rf", "--"])
                                .arg(path.display().to_string()),
                        )?;
                    } else {
                        fs::remove_dir_all(path)
                            .with_context(|| format!("removing {}", path.display()))?;
                    }
                }
            }
            Undo::RemoveBlock {
                path,
                begin,
                end,
                root,
                created_file,
                created_dirs,
            } => {
                let Some(existing) = self.read(path, *root)? else {
                    bail!("cannot read {}, even through sudo", path.display());
                };
                let as_root = *root && !self.remover.system.root;
                if let Existing::File { bytes, mode } = existing {
                    let text = String::from_utf8(bytes)
                        .with_context(|| format!("{} is not UTF-8 text", path.display()))?;
                    match unsplice(&text, begin, end) {
                        // `dot` made the file for the block, and nothing else is in it.
                        Some(rest) if *created_file && rest.trim().is_empty() => {
                            if as_root {
                                self.run_as_root(
                                    Cmd::new("rm")
                                        .args(["-f", "--"])
                                        .arg(path.display().to_string()),
                                )?;
                            } else {
                                fs::remove_file(path)
                                    .with_context(|| format!("removing {}", path.display()))?;
                            }
                            self.remove_empty_dirs(created_dirs, as_root)?;
                        }
                        Some(new) => {
                            let mode = mode.unwrap_or(0o644);
                            if as_root {
                                self.install_as_root(new.as_bytes(), path, mode)?;
                            } else {
                                atomic_write(path, new.as_bytes(), mode)?;
                            }
                        }
                        None => {}
                    }
                }
            }
            Undo::Uninstall { manager, name } => {
                let Some(m) = PkgManager::parse(manager) else {
                    bail!("unknown package manager `{manager}`");
                };
                let _one_at_a_time = dot_platform::pkg::lock();
                r.packages
                    .manager(m)
                    .uninstall(std::slice::from_ref(name), out)?;
            }
            Undo::RemoveUserPath { dir } => {
                dot_platform::winpath::remove_user_path(dir)
                    .with_context(|| format!("taking {} out of the user Path", dir.display()))?;
            }
            // The CLI drops the bundle's entries from the environment record at once.
            Undo::Forget | Undo::Manual { .. } => {}
        }
        self.restore(out)
    }

    /// Removes each of `dirs` (deepest first) that is empty; stops at the first that is not.
    fn remove_empty_dirs(&self, dirs: &[PathBuf], as_root: bool) -> Result<()> {
        for d in dirs {
            let empty = fs::read_dir(d).is_ok_and(|mut it| it.next().is_none());
            if !empty {
                break;
            }
            if as_root {
                self.run_as_root(Cmd::new("rmdir").arg(d.display().to_string()))?;
            } else {
                fs::remove_dir(d).with_context(|| format!("removing {}", d.display()))?;
            }
        }
        Ok(())
    }

    fn remove_files(&self, files: &[Written], root: bool) -> Result<()> {
        for f in files {
            if !exists(&f.path) {
                continue;
            }
            match self.untouched(f, root) {
                Held::Same => {}
                Held::Changed => bail!("{} changed since dot wrote it", f.path.display()),
                Held::NoChecksum => bail!(
                    "{}: no checksum recorded, so it cannot be told unchanged",
                    f.path.display()
                ),
                Held::Unreadable => {
                    bail!("cannot read {}, even through sudo", f.path.display())
                }
            }
            if root && self.as_root(&f.path) {
                self.run_as_root(
                    Cmd::new("rm")
                        .args(["-f", "--"])
                        .arg(f.path.display().to_string()),
                )?;
            } else {
                fs::remove_file(&f.path)
                    .with_context(|| format!("removing {}", f.path.display()))?;
            }
        }
        Ok(())
    }

    /// Puts back what was there before `dot`, when the place is free.
    fn restore(&self, out: &mut OutFn<'_>) -> Result<()> {
        let (Some(restore), Some(target)) = (&self.record.restore, self.target()) else {
            return Ok(());
        };
        if exists(&target) {
            out(
                Stream::Stderr,
                &format!(
                    "warning: {} is in the way; not putting back what was there before dot",
                    self.show(&target)
                ),
            );
            return Ok(());
        }
        match restore {
            Restore::Backup { path }
                if exists(path) && self.root_undo() && self.as_root(&target) =>
            {
                // A root-owned tree or file kept aside: moved back as root.
                self.run_as_root(Cmd::new("mv").args([
                    "--".to_string(),
                    path.display().to_string(),
                    target.display().to_string(),
                ]))?;
            }
            Restore::Backup { path } if exists(path) => {
                if fs::rename(path, &target).is_err() {
                    // Another filesystem (the state directory's backups).
                    copy_tree(path, &target)?;
                    remove_tree(path)?;
                }
            }
            Restore::Copy { src } if exists(src) => copy_tree(src, &target)?,
            Restore::RootBackup { path, mode } if exists(path) => {
                let bytes =
                    fs::read(path).with_context(|| format!("reading {}", path.display()))?;
                if self.remover.system.root {
                    atomic_write(&target, &bytes, *mode)?;
                } else {
                    self.install_as_root(&bytes, &target, *mode)?;
                }
                remove_tree(path)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Whether the record is of something root-owned (`root: true`).
    fn root_undo(&self) -> bool {
        matches!(
            self.record.undo,
            Some(
                Undo::RemoveFiles { root: true, .. }
                    | Undo::RemoveDir { root: true, .. }
                    | Undo::RemoveTree { root: true, .. }
            )
        )
    }

    /// Whether removing `path` takes root: this user may not write where it is.
    fn as_root(&self, path: &Path) -> bool {
        !self.remover.system.root && unwritable(path).is_some()
    }

    /// Runs `cmd` as root (`sudo -n`, primed up front), failing with its output.
    fn run_as_root(&self, cmd: Cmd) -> Result<()> {
        let cmd = self.remover.system.privileged(cmd);
        let o = self
            .remover
            .system
            .run(&cmd)
            .with_context(|| format!("running {}", cmd.display()))?;
        if !o.success() {
            return Err(o.failure(&cmd));
        }
        Ok(())
    }

    /// Puts `bytes` at `dest` as a root-owned file with `mode` (see [`crate::rootfile`]).
    fn install_as_root(&self, bytes: &[u8], dest: &Path, mode: u32) -> Result<()> {
        let dir = tempfile::Builder::new()
            .prefix("dot-root-")
            .tempdir()
            .context("creating a temporary directory")?;
        let tmp = dir.path().join("file");
        atomic_write(&tmp, bytes, 0o600)?;
        let group = if cfg!(target_os = "macos") {
            "wheel"
        } else {
            "root"
        };
        self.run_as_root(Cmd::new("install").args([
            "-m".to_string(),
            format!("{mode:04o}"),
            "-o".to_string(),
            "root".to_string(),
            "-g".to_string(),
            group.to_string(),
            tmp.display().to_string(),
            dest.display().to_string(),
        ]))
    }

    /// What is at `path`; `root`: a file this user cannot read is read with `sudo -n cat`
    /// (`None` when that fails too: sudo is not primed, as in `--dry-run`).
    fn read(&self, path: &Path, root: bool) -> Result<Option<Existing>> {
        match Existing::read(path) {
            Ok(e) => Ok(Some(e)),
            Err(e) if root && e.kind() == io::ErrorKind::PermissionDenied => {
                let sys = &self.remover.system;
                let cmd = sys.privileged(Cmd::new("cat").arg(path.display().to_string()));
                Ok(match sys.run(&cmd) {
                    Ok(o) if o.success() => Some(Existing::File {
                        bytes: o.stdout.into_bytes(),
                        mode: fs::symlink_metadata(path)
                            .ok()
                            .and_then(|m| crate::fileutil::mode_of(&m)),
                    }),
                    _ => None,
                })
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

/// Whether a file still holds what `dot` wrote.
enum Held {
    Same,
    Changed,
    /// No SHA-256 was recorded for a root-owned file: whether it changed cannot be told.
    NoChecksum,
    /// Root-owned, and not readable even through `sudo -n cat` (sudo is not primed yet).
    Unreadable,
}

impl Removal {
    /// Whether `f` still holds what `dot` wrote. A file this user cannot read is read with `sudo
    /// -n cat` when it is root-owned (`root`). Without a recorded hash (secrets keep none): a
    /// user file counts as unchanged while it is a regular file; a root-owned one cannot be
    /// told.
    fn untouched(&self, f: &Written, root: bool) -> Held {
        let Some(want) = &f.sha256 else {
            return if root {
                Held::NoChecksum
            } else if fs::symlink_metadata(&f.path).is_ok_and(|m| m.is_file()) {
                Held::Same
            } else {
                Held::Changed
            };
        };
        let have = match sha256_file(&f.path) {
            Ok(h) => h,
            Err(e) if root && e.kind() == io::ErrorKind::PermissionDenied => {
                match self.read(&f.path, true) {
                    Ok(Some(Existing::File { bytes, .. })) => sha256_hex(&bytes),
                    Ok(Some(_)) => return Held::Changed,
                    Ok(None) | Err(_) => return Held::Unreadable,
                }
            }
            Err(_) => return Held::Changed,
        };
        if have == *want {
            Held::Same
        } else {
            Held::Changed
        }
    }
}

/// `dot:<bundle>:<marker>` from a begin marker line.
fn marker(begin: &str) -> &str {
    begin
        .split_whitespace()
        .find(|w| w.starts_with("dot:"))
        .unwrap_or(begin)
}

/// Copies a file or directory tree (links copied as links on Unix).
pub fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from).with_context(|| format!("reading {}", from.display()))?;
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    #[cfg(unix)]
    if meta.file_type().is_symlink() {
        let t = fs::read_link(from)?;
        std::os::unix::fs::symlink(&t, to)
            .with_context(|| format!("creating link {}", to.display()))?;
        return Ok(());
    }
    if meta.is_dir() {
        fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
        for e in fs::read_dir(from)? {
            let e = e?;
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
        fs::set_permissions(to, meta.permissions()).ok();
        return Ok(());
    }
    fs::copy(from, to)
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    Ok(())
}

fn remove_tree(p: &Path) -> Result<()> {
    let r = if fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) {
        fs::remove_dir_all(p)
    } else {
        fs::remove_file(p)
    };
    r.with_context(|| format!("removing {}", p.display()))
}

#[cfg(all(test, unix))]
mod tests;
