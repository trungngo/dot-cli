//! `create`: make directories, optionally with a mode (Unix only). `root: true` makes a
//! root-owned directory (`install -d -o root` through sudo; see [`crate::rootfile`]).
//!
//! `file: true` makes an empty file instead, only when nothing is there: a file the user owns
//! the content of (`~/.local.zshrc`, sourced by a managed rc file). What is in it is never read
//! or touched; only its mode (and, with `root: true`, its owner) is kept as asked. `dot remove`
//! removes it while it is still empty.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result, bail};
use dot_core::config::{CreateEntry, Mode};
use dot_core::{Action, Outcome, PlanCtx, Resource, RunCtx, Status, Undo};

use dot_platform::exec::Cmd;

use crate::fileutil::{unwritable, unwritable_note};
use crate::rootfile::{self, RootFiles};

pub fn plan(
    entries: &[CreateEntry],
    ctx: &PlanCtx<'_>,
    root_files: &RootFiles,
) -> Result<Vec<Box<dyn Action>>> {
    entries
        .iter()
        .map(|e| {
            if e.root {
                RootFiles::check_supported(ctx.facts.os).map_err(|m| ctx.config_error(m))?;
            }
            let path = ctx.target(&e.path)?;
            let path = if e.root { root_files.map(path) } else { path };
            let mode = e.mode.as_ref().map(|m| ctx.mode(m)).transpose()?;
            if e.file {
                crate::fileutil::guard(&path, &ctx.show(&path), ctx.home, &ctx.bundle.root)?;
                return Ok(Box::new(CreateFileAction {
                    show: ctx.show(&path),
                    path,
                    mode,
                    as_root: e.root.then(|| root_files.clone()),
                    existed: AtomicBool::new(false),
                }) as Box<dyn Action>);
            }
            Ok(Box::new(CreateAction {
                show: ctx.show(&path),
                path,
                mode,
                as_root: e.root.then(|| root_files.clone()),
                existed: AtomicBool::new(false),
            }) as Box<dyn Action>)
        })
        .collect()
}

pub struct CreateAction {
    pub path: PathBuf,
    pub mode: Option<Mode>,
    show: String,
    /// `root: true`: a root-owned directory, made through sudo.
    as_root: Option<RootFiles>,
    /// Applying found the directory already there (it only fixed the mode): not `dot`'s to
    /// remove.
    existed: AtomicBool,
}

impl CreateAction {
    /// Why the directory cannot be created: something else is there, or the directory it goes
    /// in is not writable by this user ([`unwritable`]; not a problem for a root-owned one).
    fn in_the_way(&self) -> Option<String> {
        match fs::symlink_metadata(&self.path) {
            Ok(m) if !fs::metadata(&self.path).is_ok_and(|t| t.is_dir()) => {
                let what = if m.file_type().is_symlink() {
                    "a link"
                } else {
                    "a file"
                };
                Some(format!("{what} is in the way"))
            }
            Ok(_) => None,
            Err(_) if self.as_root.is_some() => None,
            Err(_) => unwritable(&self.path).map(|dir| {
                unwritable_note(&dir)
                    .trim_start_matches("conflict: ")
                    .to_string()
            }),
        }
    }

    /// `(exists as a directory, mode matches)`
    fn inspect(&self) -> io::Result<(bool, bool)> {
        let meta = match fs::metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((false, false)),
            Err(e) => return Err(e),
        };
        if !meta.is_dir() {
            return Ok((false, false));
        }
        let owned = match &self.as_root {
            Some(r) => r.owned(rootfile::owner(&self.path)),
            None => true,
        };
        Ok((true, mode_matches(&meta, self.mode) && owned))
    }
}

#[cfg(unix)]
fn mode_matches(meta: &fs::Metadata, mode: Option<Mode>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    mode.is_none_or(|m| meta.permissions().mode() & 0o7777 == m.0)
}

#[cfg(not(unix))]
fn mode_matches(_meta: &fs::Metadata, _mode: Option<Mode>) -> bool {
    true
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: Mode) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode.0))
}

#[cfg(not(unix))]
fn set_mode(_path: &std::path::Path, _mode: Mode) -> io::Result<()> {
    Ok(())
}

impl Action for CreateAction {
    fn needs_sudo(&self) -> bool {
        self.as_root.as_ref().is_some_and(RootFiles::needs_sudo)
    }

    fn describe(&self) -> String {
        let base = match (self.mode, &self.as_root) {
            (Some(m), Some(_)) => format!("create {} (mode {m}, as root)", self.show),
            (None, Some(_)) => format!("create {} (as root)", self.show),
            (Some(m), None) if cfg!(unix) => format!("create {} (mode {m})", self.show),
            (Some(m), None) => format!("create {} (mode {m} ignored on this platform)", self.show),
            (None, None) => format!("create {}", self.show),
        };
        match self.in_the_way() {
            Some(why) => format!("{base} (conflict: {why})"),
            None => base,
        }
    }

    fn status(&self) -> Result<Status> {
        let (is_dir, mode_ok) = self
            .inspect()
            .with_context(|| format!("inspecting {}", self.path.display()))?;
        Ok(if is_dir && mode_ok {
            Status::Satisfied
        } else if !is_dir && self.in_the_way().is_some() {
            Status::Conflict
        } else {
            Status::Pending
        })
    }

    /// `dir:<path>`: removed when empty (through sudo for a root-owned one). A directory that
    /// was already there (applying only set its mode) is not `dot`'s: never removed.
    fn resources(&self) -> Vec<Resource> {
        let r = Resource::new("dir", self.show.clone()).with_undo(Undo::RemoveDir {
            path: self.path.clone(),
            root: self.as_root.is_some(),
        });
        vec![if self.existed.load(Ordering::SeqCst) {
            r.changed(false)
        } else {
            r
        }]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let (is_dir, mode_ok) = self.inspect()?;
        if is_dir && mode_ok {
            return Ok(Outcome::Ok);
        }
        self.existed.store(is_dir, Ordering::SeqCst);
        if let Some(r) = &self.as_root {
            if !is_dir && fs::symlink_metadata(&self.path).is_ok() {
                bail!("{} exists and is not a directory", self.path.display());
            }
            let mode = self.mode.map_or(0o755, |m| m.0);
            r.mkdir(&self.path, mode, ctx)?;
            return Ok(Outcome::Changed);
        }
        if !is_dir {
            if fs::symlink_metadata(&self.path).is_ok() {
                bail!("{} exists and is not a directory", self.path.display());
            }
            fs::create_dir_all(&self.path)
                .with_context(|| format!("creating {}", self.path.display()))?;
        }
        if let Some(mode) = self.mode {
            set_mode(&self.path, mode)
                .with_context(|| format!("setting mode {mode} on {}", self.path.display()))?;
        }
        Ok(Outcome::Changed)
    }
}

/// `create: { path: { file: true } }`: an empty file, made when missing.
pub struct CreateFileAction {
    pub path: PathBuf,
    pub mode: Option<Mode>,
    show: String,
    /// `root: true`: root-owned, made through sudo.
    as_root: Option<RootFiles>,
    /// Applying found the file already there: not `dot`'s to remove.
    existed: AtomicBool,
}

/// Where a file to create stands.
enum FileState {
    Missing,
    /// A regular file (or a link to one), with its mode and owner.
    File {
        mode: Option<u32>,
        uid: Option<u32>,
        link: bool,
    },
    /// A directory, a dangling link, or something else.
    InTheWay(&'static str),
}

impl CreateFileAction {
    fn inspect(&self) -> io::Result<FileState> {
        let meta = match fs::symlink_metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(FileState::Missing),
            Err(e) => return Err(e),
        };
        let link = meta.file_type().is_symlink();
        // A link to a file counts: the file is there (never written through).
        let target = if link {
            match fs::metadata(&self.path) {
                Ok(t) => t,
                Err(_) => return Ok(FileState::InTheWay("a dangling link")),
            }
        } else {
            meta
        };
        Ok(if target.is_file() {
            FileState::File {
                mode: crate::fileutil::mode_of(&target),
                uid: rootfile::owner(&self.path),
                link,
            }
        } else if target.is_dir() {
            FileState::InTheWay("a directory")
        } else {
            FileState::InTheWay("not a regular file")
        })
    }

    /// What applying would change on an existing file: its mode, its owner (root-owned).
    fn fixes(&self, mode: Option<u32>, uid: Option<u32>, link: bool) -> Vec<String> {
        if link {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let (Some(have), Some(want)) = (mode, self.mode)
            && have != want.0
        {
            out.push(format!("mode {have:04o} -> {want}"));
        }
        if let Some(r) = &self.as_root
            && !r.owned(uid)
        {
            out.push(rootfile::owner_note(uid));
        }
        out
    }

    fn blocked(&self) -> Option<PathBuf> {
        match self.as_root {
            Some(_) => None,
            None => unwritable(&self.path),
        }
    }
}

impl Action for CreateFileAction {
    fn needs_sudo(&self) -> bool {
        self.as_root.as_ref().is_some_and(RootFiles::needs_sudo)
    }

    fn describe(&self) -> String {
        let mut opts = Vec::new();
        if let Some(m) = self.mode {
            opts.push(format!("mode {m}"));
        }
        if self.as_root.is_some() {
            opts.push("as root".to_string());
        }
        let base = crate::fileutil::with_notes(format!("create file {}", self.show), &opts);
        let note = match self.inspect() {
            Ok(FileState::Missing) => match self.blocked() {
                Some(dir) => Some(unwritable_note(&dir)),
                None => Some("empty".to_string()),
            },
            Ok(FileState::File { mode, uid, link }) => {
                let fixes = self.fixes(mode, uid, link);
                (!fixes.is_empty()).then(|| fixes.join("; "))
            }
            Ok(FileState::InTheWay(what)) => Some(format!("conflict: {what} is in the way")),
            Err(e) => Some(format!("cannot inspect: {e}")),
        };
        match note {
            Some(n) => format!("{base} ({n})"),
            None => base,
        }
    }

    fn status(&self) -> Result<Status> {
        let state = self
            .inspect()
            .with_context(|| format!("inspecting {}", self.path.display()))?;
        Ok(match state {
            FileState::Missing if self.blocked().is_some() => Status::Conflict,
            FileState::Missing => Status::Pending,
            FileState::File { mode, uid, link } if self.fixes(mode, uid, link).is_empty() => {
                Status::Satisfied
            }
            FileState::File { .. } => Status::Pending,
            FileState::InTheWay(_) => Status::Conflict,
        })
    }

    /// `file:<path>`: removed while it is still empty (through sudo for a root-owned one). A
    /// file that was already there is not `dot`'s.
    fn resources(&self) -> Vec<Resource> {
        let r = Resource::new("file", self.show.clone()).with_undo(Undo::RemoveFiles {
            files: vec![crate::fileutil::written_bytes(&self.path, b"")],
            root: self.as_root.is_some(),
        });
        vec![if self.existed.load(Ordering::SeqCst) {
            r.changed(false)
        } else {
            r
        }]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let state = self.inspect()?;
        let mode = self
            .mode
            .map_or(crate::fileutil::DEFAULT_FILE_MODE, |m| m.0);
        match state {
            FileState::InTheWay(what) => bail!("{}: {what} is in the way", self.path.display()),
            FileState::File {
                mode: have,
                uid,
                link,
            } => {
                if self.fixes(have, uid, link).is_empty() {
                    return Ok(Outcome::Ok);
                }
                self.existed.store(true, Ordering::SeqCst);
                let path = self.path.display().to_string();
                match &self.as_root {
                    Some(r) => {
                        for cmd in [
                            Cmd::new("chown").args([format!("root:{}", r.group), path.clone()]),
                            Cmd::new("chmod").args([format!("{mode:04o}"), path.clone()]),
                        ] {
                            r.run(cmd, ctx)?;
                        }
                    }
                    None => crate::fileutil::set_mode(&self.path, mode)
                        .with_context(|| format!("setting mode {mode:04o} on {path}"))?,
                }
            }
            FileState::Missing => match &self.as_root {
                Some(r) => r.write(&self.path, b"", mode, ctx)?,
                None => {
                    if let Some(parent) = self.path.parent() {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("creating {}", parent.display()))?;
                    }
                    // Never over something that appeared since.
                    fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&self.path)
                        .with_context(|| format!("creating {}", self.path.display()))?;
                    crate::fileutil::set_mode(&self.path, mode)?;
                }
            },
        }
        Ok(Outcome::Changed)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::testutil::Fixture;

    fn mode_of(p: &std::path::Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn creates_nested_directories() {
        let f = Fixture::new("steps:\n  - create: [~/a/b/c, ~/d]\n");
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].describe(), "create ~/a/b/c");
        assert!(
            actions
                .iter()
                .all(|a| a.status().unwrap() == Status::Pending)
        );
        assert_eq!(f.apply(&actions), [Outcome::Changed, Outcome::Changed]);
        assert!(f.home().join("a/b/c").is_dir() && f.home().join("d").is_dir());
        assert!(
            actions
                .iter()
                .all(|a| a.status().unwrap() == Status::Satisfied)
        );
        assert_eq!(f.apply(&actions), [Outcome::Ok, Outcome::Ok]);
    }

    #[test]
    fn applies_mode() {
        let f = Fixture::new("steps:\n  - create:\n      ~/.ssh: { mode: \"0700\" }\n");
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].describe(), "create ~/.ssh (mode 0700)");
        f.apply(&actions);
        assert_eq!(mode_of(&f.home().join(".ssh")), 0o700);
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    }

    #[test]
    fn fixes_mode_of_existing_directory() {
        let f = Fixture::new("steps:\n  - create:\n      ~/.ssh: { mode: \"0700\" }\n");
        let dir = f.home().join(".ssh");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        assert_eq!(mode_of(&dir), 0o700);
    }

    #[test]
    fn templated_path_and_mode_are_rendered_at_plan_time() {
        let f =
            Fixture::new("steps:\n  - create:\n      \"{{ home }}/t\": { mode: \"{{ m }}\" }\n");
        let mut ctx = f.context();
        let err = format!("{:#}", f.plan_with(0, &ctx).err().unwrap());
        assert!(err.contains("undefined variable `m`"), "{err}");
        assert!(err.contains("bundle.yml:2:5: steps[0]:"), "located: {err}");
        ctx.vars.set("m", "0999");
        let err = format!("{:#}", f.plan_with(0, &ctx).err().unwrap());
        assert!(err.contains("invalid file mode `0999`"), "{err}");
        ctx.vars.set("m", "0750");
        let actions = f.plan_with(0, &ctx).unwrap();
        f.apply(&actions);
        assert_eq!(mode_of(&f.home().join("t")), 0o750);
    }

    #[test]
    fn file_in_the_way_fails() {
        let f = Fixture::new("steps:\n  - create: [~/x]\n");
        fs::write(f.home().join("x"), "file").unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Conflict);
        assert_eq!(
            actions[0].describe(),
            "create ~/x (conflict: a file is in the way)"
        );
        let err = f.apply_err(actions[0].as_ref());
        assert!(err.contains("not a directory"), "{err}");
        assert_eq!(fs::read_to_string(f.home().join("x")).unwrap(), "file");
    }

    #[test]
    fn a_directory_this_user_cannot_write_is_a_conflict() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root may write anywhere
        }
        let f = Fixture::new("steps:\n  - create: [~/locked/sub/dir]\n");
        let locked = f.home().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Conflict);
        let d = actions[0].describe();
        assert!(
            d.contains(&format!("{} is not writable by you", locked.display())),
            "{d}"
        );
        assert!(d.contains("`sudo: true`"), "{d}");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
    }

    #[test]
    fn an_empty_file_is_made_only_when_missing_and_never_touched() {
        let f = Fixture::new(
            "steps:\n  - create:\n      ~/.local.zshrc: { file: true }\n      ~/.local.vimrc: { file: true, mode: \"0600\" }\n",
        );
        let rc = f.home().join(".local.vimrc");
        fs::write(&rc, "set nu\n").unwrap();
        fs::set_permissions(&rc, fs::Permissions::from_mode(0o644)).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].describe(), "create file ~/.local.zshrc (empty)");
        assert_eq!(
            actions[1].describe(),
            "create file ~/.local.vimrc (mode 0600) (mode 0644 -> 0600)"
        );
        assert_eq!(f.apply(&actions), [Outcome::Changed, Outcome::Changed]);
        assert_eq!(fs::read(f.home().join(".local.zshrc")).unwrap(), b"");
        // What was in it is kept; only the mode is fixed. Not `dot`'s to remove.
        assert_eq!(fs::read_to_string(&rc).unwrap(), "set nu\n");
        assert_eq!(mode_of(&rc), 0o600);
        assert_eq!(actions[1].resources()[0].changed, Some(false));
        assert_eq!(actions[0].resources()[0].changed, None);
        // Once there, any content is fine.
        fs::write(f.home().join(".local.zshrc"), "alias x=y\n").unwrap();
        let actions = f.plan(0).unwrap();
        assert!(
            actions
                .iter()
                .all(|a| a.status().unwrap() == Status::Satisfied)
        );
        // A directory in the way is a conflict.
        let f = Fixture::new("steps:\n  - create: { ~/x: { file: true } }\n");
        fs::create_dir(f.home().join("x")).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Conflict);
        assert!(actions[0].describe().contains("a directory is in the way"));
    }
}
