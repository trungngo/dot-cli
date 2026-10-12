//! `link`: symlink a destination (usually under `~`) to a source in the bundle.
//!
//! Replacing whatever is at the destination (backup, overwrite, relink) is only ever done to a
//! path that is safely *outside* both the repository and the chain of directories above the home
//! directory: see [`LinkAction::guard`], checked when planning and again right before applying.
//!
//! Links are made through the run's [`LinkStrategy`]: a symlink wherever possible, else (Windows
//! without Developer Mode) a junction for a directory and a copy for a file. A fallback is
//! recorded in the state directory ([`crate::linkstate`]): a recorded copy that still holds what
//! `dot` wrote is `dot`'s own, satisfied while its bytes equal the source's and refreshed without
//! a backup when the source changes, and replaced by a real link once symlinks are available.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use dot_core::config::{LinkEntry, LinkOptions, OnConflict};
use dot_core::paths::{on_disk, resolve_lenient, simplify};
use dot_core::{Action, Diff, Outcome, PlanCtx, Resource, RunCtx, Status, Undo};
use dot_platform::link::{LinkCaps, LinkKind, LinkStrategy};

use crate::fileutil::{located, on_disk_or_lexical, via_note, within};
use crate::linkstate::{Fallback, LinkState, sha256_file};

/// Suffix for backups of files a link replaces; numbered (`.dot-backup.1`, ...) when taken.
pub const BACKUP_SUFFIX: &str = ".dot-backup";

pub fn plan(
    entries: &[LinkEntry],
    ctx: &PlanCtx<'_>,
    links: &Arc<dyn LinkStrategy>,
) -> Result<Vec<Box<dyn Action>>> {
    let mut actions: Vec<Box<dyn Action>> = Vec::new();
    for e in entries {
        let dest = ctx.target(&e.dest)?;
        let src = ctx.source(&e.src)?;
        let windows_side = dot_core::folders::wsl_link_to_windows_side(
            ctx.facts.env.iter().any(|e| e == "wsl"),
            &dest.to_string_lossy(),
            &src.to_string_lossy(),
        );
        let action = LinkAction {
            windows_side,
            show_dest: ctx.show(&dest),
            show_src: ctx.show_source(&src),
            src,
            dest,
            opts: e.resolve(ctx.defaults),
            home: ctx.home.to_path_buf(),
            root: ctx.bundle.root.clone(),
            links: links.clone(),
            state: LinkState::in_dir(ctx.state_dir),
        };
        action.guard()?;
        actions.push(Box::new(action));
    }
    Ok(actions)
}

pub struct LinkAction {
    pub src: PathBuf,
    pub dest: PathBuf,
    pub opts: LinkOptions,
    /// The home directory and repository root, which a destination must never be or contain.
    home: PathBuf,
    root: PathBuf,
    show_src: String,
    show_dest: String,
    links: Arc<dyn LinkStrategy>,
    state: LinkState,
    /// WSL, onto a Windows drive from the Linux filesystem: copied, never symlinked (see
    /// [`dot_core::folders::wsl_link_to_windows_side`]).
    windows_side: bool,
}

/// What is at the destination now.
#[derive(Debug, PartialEq)]
enum Dest {
    Missing,
    /// A symlink (or junction) that already resolves to the source.
    Correct,
    /// A symlink pointing somewhere else (possibly dangling, or a loop).
    OtherLink(PathBuf),
    /// A copy `dot` made of this source (recorded, and not edited since); `current`: its bytes
    /// equal the source's.
    Copied {
        current: bool,
    },
    /// A real file or directory.
    Occupied {
        is_dir: bool,
    },
}

impl LinkAction {
    fn inspect(&self) -> io::Result<Dest> {
        let meta = match fs::symlink_metadata(&self.dest) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Dest::Missing),
            Err(e) => return Err(e),
        };
        if !meta.file_type().is_symlink() {
            if meta.is_file()
                && let Some(copy) = self.own_copy()
            {
                let current = sha256_file(&self.src).ok() == copy.sha256;
                return Ok(Dest::Copied { current });
            }
            return Ok(Dest::Occupied {
                is_dir: meta.is_dir(),
            });
        }
        let target = fs::read_link(&self.dest)?;
        // A link that loops (e.g. onto itself) is never correct, whatever it names.
        let loops = resolve_lenient(&self.dest).is_none();
        let same_target = simplify(&target) == simplify(&self.src);
        if !loops && (same_target || same_file(&self.dest, &self.src)) {
            Ok(Dest::Correct)
        } else {
            Ok(Dest::OtherLink(target))
        }
    }

    /// The recorded copy of this source at the destination, when it still holds what `dot`
    /// wrote there.
    fn own_copy(&self) -> Option<Fallback> {
        self.state
            .get(&self.dest)
            .filter(|f| f.kind() == Some(LinkKind::Copy) && f.src == self.src && f.untouched())
    }

    fn source_exists(&self) -> bool {
        fs::symlink_metadata(&self.src).is_ok()
    }

    /// How the link is made here: see [`LinkStrategy::kind_for`].
    fn kind(&self) -> Result<LinkKind, String> {
        let is_dir = fs::metadata(&self.src).is_ok_and(|m| m.is_dir());
        if self.windows_side {
            return LinkCaps::kind_for_windows_side(is_dir);
        }
        self.links.kind_for(is_dir)
    }

    /// The plan note for how the link is made, when it is not a symlink.
    fn kind_note(&self, kind: LinkKind) -> Option<String> {
        if self.windows_side && kind == LinkKind::Copy {
            return Some(
                "copied (Windows side of WSL: Windows programs cannot follow a WSL link)".into(),
            );
        }
        kind.note().map(str::to_string)
    }

    /// Refuses destinations that would put the home directory, the repository or the source
    /// itself at risk, judged on where the destination really is (its parent directories'
    /// symlinks followed):
    ///
    /// - the home directory, a filesystem root, or any directory above home;
    /// - the repository root or any directory above it;
    /// - anything inside the repository, e.g. `~/.config/nvim/init.lua` once `~/.config/nvim` is
    ///   itself a link into the repo: backing that "file" up would move the tracked source, and
    ///   the new link would point at itself;
    /// - the source, or a directory containing it.
    ///
    /// "Really is" means on disk, names spelled and compared as the filesystem does (see
    /// [`crate::fileutil::guard_as`]): `~/DOTFILES` is the repository at `~/dotfiles` on macOS
    /// and Windows.
    pub fn guard(&self) -> Result<()> {
        let shown = &self.show_dest;
        let Some(at) = located(&self.dest) else {
            bail!("refusing to link {shown}: its parent directory is a symlink loop");
        };
        if at.parent().is_none() {
            bail!("refusing to link {shown}: it is a filesystem root");
        }
        let via = via_note(&self.dest, &at);
        let home = on_disk_or_lexical(&self.home);
        let root = on_disk_or_lexical(&self.root);
        if within(&home, &at) {
            bail!("refusing to link {shown}: it is the home directory or one of its parents{via}");
        }
        if within(&root, &at) {
            bail!("refusing to link {shown}: it contains the repository{via}");
        }
        if within(&at, &root) {
            bail!("refusing to link {shown}: the destination is inside the repository{via}");
        }
        if let Some(src) = on_disk(&self.src)
            && within(&src, &at)
        {
            bail!("refusing to link {shown}: it is or contains the link source{via}");
        }
        Ok(())
    }

    fn ensure_parent(&self) -> Result<()> {
        let Some(parent) = self.dest.parent() else {
            return Ok(());
        };
        if parent.is_dir() {
            return Ok(());
        }
        if !self.opts.create {
            bail!(
                "parent directory {} does not exist (create: false)",
                parent.display()
            );
        }
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))
    }

    /// Makes the link as `kind` and records (or forgets) the fallback.
    fn make_link(&self, kind: LinkKind) -> Result<()> {
        self.links
            .create(kind, &self.src, &self.dest)
            .with_context(|| {
                format!(
                    "creating {} {} -> {}",
                    if kind == LinkKind::Symlink {
                        "link".to_string()
                    } else {
                        kind.to_string()
                    },
                    self.dest.display(),
                    self.src.display()
                )
            })?;
        if kind.is_fallback() {
            let sha256 = match kind {
                LinkKind::Copy => Some(
                    sha256_file(&self.dest)
                        .with_context(|| format!("reading {}", self.dest.display()))?,
                ),
                _ => None,
            };
            self.state.record(Fallback {
                dest: self.dest.clone(),
                src: self.src.clone(),
                kind: kind.to_string(),
                sha256,
            })
        } else {
            self.state.forget(&self.dest)
        }
    }
}

/// Whether both paths resolve to the same existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `<dest>.dot-backup`, or the first free `<dest>.dot-backup.<n>`. Built on the raw OS string,
/// so a destination whose path is not valid UTF-8 keeps its real parent directory.
pub fn backup_path(dest: &Path) -> PathBuf {
    let mut base = dest.as_os_str().to_os_string();
    base.push(BACKUP_SUFFIX);
    let mut candidate = PathBuf::from(&base);
    let mut n = 1;
    while fs::symlink_metadata(&candidate).is_ok() {
        let mut numbered: OsString = base.clone();
        numbered.push(format!(".{n}"));
        candidate = PathBuf::from(numbered);
        n += 1;
    }
    candidate
}

impl Action for LinkAction {
    fn describe(&self) -> String {
        let base = format!("link {} -> {}", self.show_dest, self.show_src);
        let mut notes: Vec<String> = Vec::new();
        let kind = self.kind();
        if self.source_exists() {
            match &kind {
                Ok(k) => notes.extend(self.kind_note(*k)),
                Err(e) => notes.push(format!("conflict: {e}")),
            }
        }
        let note = match self.inspect() {
            _ if !self.source_exists() => {
                Some("source does not exist yet; an earlier step may create it".to_string())
            }
            Ok(Dest::Missing | Dest::Correct) => None,
            Ok(Dest::Copied { .. }) if kind != Ok(LinkKind::Copy) => {
                Some("replaces the copy made while symlinks were unavailable".to_string())
            }
            Ok(Dest::Copied { current: true }) => None,
            Ok(Dest::Copied { current: false }) => {
                Some("the source changed: copied again".to_string())
            }
            Ok(Dest::OtherLink(t)) if self.opts.relink => {
                Some(format!("replaces link to {}", t.display()))
            }
            Ok(Dest::OtherLink(t)) => Some(format!(
                "conflict: links to {} and relink is false",
                t.display()
            )),
            Ok(Dest::Occupied { is_dir }) => {
                let what = if is_dir { "directory" } else { "file" };
                Some(match self.opts.on_conflict {
                    OnConflict::Backup => format!("existing {what} moved to {BACKUP_SUFFIX}"),
                    OnConflict::Overwrite => format!("existing {what} deleted"),
                    OnConflict::Fail => format!("conflict: existing {what}, on_conflict is fail"),
                })
            }
            Err(e) => Some(format!("cannot inspect: {e}")),
        };
        notes.extend(note);
        crate::fileutil::with_notes(base, &notes)
    }

    fn status(&self) -> Result<Status> {
        let d = self
            .inspect()
            .with_context(|| format!("inspecting {}", self.dest.display()))?;
        let kind = self.kind();
        Ok(match d {
            Dest::Correct => Status::Satisfied,
            _ if !self.source_exists() => Status::Unknown,
            _ if kind.is_err() => Status::Conflict,
            Dest::Copied { current: true } if kind == Ok(LinkKind::Copy) => Status::Satisfied,
            Dest::OtherLink(_) if !self.opts.relink => Status::Conflict,
            Dest::Occupied { .. } if self.opts.on_conflict == OnConflict::Fail => Status::Conflict,
            _ => Status::Pending,
        })
    }

    /// `link:<dest>`: removed while it still points at the source (a copy: while it still
    /// holds what `dot` copied).
    fn resources(&self) -> Vec<Resource> {
        let undo = if self.kind() == Ok(LinkKind::Copy) {
            Undo::RemoveFiles {
                files: vec![crate::fileutil::written(&self.dest)],
                root: false,
            }
        } else {
            Undo::Unlink {
                path: self.dest.clone(),
                target: self.src.clone(),
            }
        };
        vec![Resource::new("link", self.show_dest.clone()).with_undo(undo)]
    }

    fn diff(&self) -> Result<Option<Diff>> {
        let copy = self.kind() == Ok(LinkKind::Copy);
        let from = match self.inspect()? {
            Dest::Correct => return Ok(None),
            Dest::Copied { current: true } if copy => return Ok(None),
            Dest::Missing => None,
            Dest::OtherLink(t) => Some(format!("link to {}", t.display())),
            Dest::Copied { current: true } => {
                Some("a copy made while symlinks were unavailable".to_string())
            }
            Dest::Copied { current: false } => {
                Some("a copy of an older version of the source".to_string())
            }
            Dest::Occupied { is_dir: true } => Some("a directory".to_string()),
            Dest::Occupied { is_dir: false } => Some("a file".to_string()),
        };
        let what = if copy { "copy of" } else { "link to" };
        Ok(Some(Diff::Link {
            path: self.show_dest.clone(),
            from,
            to: format!("{what} {}", self.src.display()),
        }))
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let dest = &self.dest;
        // Re-checked here: earlier steps (or earlier entries of this step) may have turned a
        // parent of the destination into a link since planning.
        self.guard()?;
        if !self.source_exists() {
            bail!(
                "link source not found: `{}` ({})",
                self.show_src,
                self.src.display()
            );
        }
        let kind = self.kind().map_err(anyhow::Error::msg)?;
        match self.inspect()? {
            Dest::Correct => return Ok(Outcome::Ok),
            Dest::Copied { current: true } if kind == LinkKind::Copy => return Ok(Outcome::Ok),
            // `dot`'s own copy: replaced as is, no backup.
            Dest::Copied { .. } if kind == LinkKind::Copy => {}
            Dest::Copied { .. } => fs::remove_file(dest)
                .with_context(|| format!("removing the old copy {}", dest.display()))?,
            Dest::Missing => self.ensure_parent()?,
            Dest::OtherLink(target) => {
                if !self.opts.relink {
                    bail!(
                        "{} is a link to {}, not {}, and relink is false",
                        dest.display(),
                        target.display(),
                        self.src.display()
                    );
                }
                self.links
                    .remove(dest)
                    .with_context(|| format!("removing old link {}", dest.display()))?;
            }
            Dest::Occupied { is_dir } => match self.opts.on_conflict {
                OnConflict::Fail => bail!(
                    "{} already exists and is not a link (on_conflict: fail)",
                    dest.display()
                ),
                OnConflict::Backup => {
                    let backup = backup_path(dest);
                    fs::rename(dest, &backup).with_context(|| {
                        format!("backing up {} to {}", dest.display(), backup.display())
                    })?;
                    ctx.backed_up(&backup);
                }
                OnConflict::Overwrite => {
                    let r = if is_dir {
                        fs::remove_dir_all(dest)
                    } else {
                        fs::remove_file(dest)
                    };
                    r.with_context(|| format!("removing {}", dest.display()))?;
                }
            },
        }
        self.make_link(kind)?;
        Ok(Outcome::Changed)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::testutil::Fixture;

    fn fixture(link_yaml: &str) -> Fixture {
        let f = Fixture::new(&format!("steps:\n  - link:\n{link_yaml}"));
        f.write_repo("files/rc", "rc contents");
        f.write_repo("files/dir/inner", "x");
        f
    }

    #[test]
    fn another_spelling_of_the_repo_is_refused_where_case_does_not_matter() {
        let f = Fixture::repo_in_home(
            "steps:\n  - link:\n      ~/DOTFILES: { src: files/rc, on_conflict: backup }\n",
        );
        f.write_repo("files/rc", "rc");
        if !crate::testutil::case_insensitive_fs(f.home()) {
            return;
        }
        let e = format!("{:#}", f.plan(0).err().expect("refused"));
        assert!(e.contains("contains the repository"), "{e}");
        // Nothing was moved aside.
        assert!(f.repo().join("files/rc").exists());
        let g = f.with_bundle("steps:\n  - link:\n      ~/DotFiles/bundles/x: files/rc\n");
        let e = format!("{:#}", g.plan(0).err().expect("refused"));
        assert!(e.contains("inside the repository"), "{e}");
    }

    #[test]
    fn from_wsl_onto_a_windows_drive_a_file_is_copied_and_a_directory_refused() {
        let f = fixture(
            "      /mnt/c/Users/me/AppData/Roaming/Code/User/settings.json: files/rc\n      /mnt/c/Users/me/.config/dir: files/dir\n      ~/.rc: files/rc\n",
        );
        let mut ctx = f.context();
        ctx.facts.env = vec!["wsl".into()];
        let actions = f.plan_with(0, &ctx).unwrap();
        assert_eq!(
            actions[0].describe(),
            "link /mnt/c/Users/me/AppData/Roaming/Code/User/settings.json -> files/rc (copied \
             (Windows side of WSL: Windows programs cannot follow a WSL link))"
        );
        assert_eq!(actions[1].status().unwrap(), Status::Conflict);
        assert!(
            actions[1].describe().contains("use `copy:`"),
            "{}",
            actions[1].describe()
        );
        assert_eq!(
            actions[2].describe(),
            "link ~/.rc -> files/rc",
            "Linux side: a symlink"
        );
        // Not WSL: ordinary links.
        let actions = f.plan(0).unwrap();
        assert!(!actions[0].describe().contains("copied"));
    }

    #[test]
    fn fresh_link_creates_parents() {
        let f = fixture("      ~/.config/app/rc: files/rc\n");
        let actions = f.plan(0).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert_eq!(actions[0].describe(), "link ~/.config/app/rc -> files/rc");
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        let dest = f.home().join(".config/app/rc");
        assert_eq!(fs::read_link(&dest).unwrap(), f.repo().join("files/rc"));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "rc contents");
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    }

    #[test]
    fn already_correct_is_satisfied() {
        let f = fixture("      ~/.rc: files/rc\n");
        symlink(f.repo().join("files/rc"), f.home().join(".rc")).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
        // Also satisfied via a different spelling of the same file.
        fs::remove_file(f.home().join(".rc")).unwrap();
        symlink(f.repo().join("files/../files/rc"), f.home().join(".rc")).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    }

    #[test]
    fn no_parent_with_create_false_fails() {
        let f = fixture("      ~/missing/rc: { src: files/rc, create: false }\n");
        let actions = f.plan(0).unwrap();
        let err = f.apply_err(actions[0].as_ref());
        assert!(err.contains("create: false"), "{err}");
        assert!(!f.home().join("missing").exists());
    }

    #[test]
    fn relinks_a_link_pointing_elsewhere() {
        let f = fixture("      ~/.rc: files/rc\n");
        let other = f.home().join("other");
        fs::write(&other, "other").unwrap();
        symlink(&other, f.home().join(".rc")).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert!(actions[0].describe().contains("replaces link"));
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        assert_eq!(
            fs::read_link(f.home().join(".rc")).unwrap(),
            f.repo().join("files/rc")
        );
        assert_eq!(
            fs::read_to_string(&other).unwrap(),
            "other",
            "target untouched"
        );
    }

    #[test]
    fn relinks_a_dangling_link() {
        let f = fixture("      ~/.rc: files/rc\n");
        symlink(f.home().join("gone"), f.home().join(".rc")).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        assert_eq!(
            fs::read_link(f.home().join(".rc")).unwrap(),
            f.repo().join("files/rc")
        );
    }

    #[test]
    fn relink_false_refuses() {
        let f = fixture("      ~/.rc: { src: files/rc, relink: false }\n");
        let other = f.home().join("other");
        fs::write(&other, "").unwrap();
        symlink(&other, f.home().join(".rc")).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Conflict);
        let err = f.apply_err(actions[0].as_ref());
        assert!(err.contains("relink is false"), "{err}");
        assert_eq!(fs::read_link(f.home().join(".rc")).unwrap(), other);
    }

    #[test]
    fn backup_moves_existing_file_with_numbering() {
        let f = fixture("      ~/.rc: files/rc\n");
        let dest = f.home().join(".rc");
        fs::write(&dest, "mine").unwrap();
        fs::write(f.home().join(".rc.dot-backup"), "older backup").unwrap();
        let actions = f.plan(0).unwrap();
        assert!(actions[0].describe().contains("moved to .dot-backup"));
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        assert_eq!(fs::read_link(&dest).unwrap(), f.repo().join("files/rc"));
        assert_eq!(
            fs::read_to_string(f.home().join(".rc.dot-backup")).unwrap(),
            "older backup"
        );
        assert_eq!(
            fs::read_to_string(f.home().join(".rc.dot-backup.1")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn backup_moves_existing_directory() {
        let f = fixture("      ~/.dir: files/dir\n");
        let dest = f.home().join(".dir");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("keep"), "k").unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        assert!(
            fs::symlink_metadata(&dest)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(f.home().join(".dir.dot-backup/keep")).unwrap(),
            "k"
        );
    }

    #[test]
    fn overwrite_deletes_existing() {
        let f = fixture(
            "      ~/.rc: { src: files/rc, on_conflict: overwrite }\n      ~/.dir: { src: files/dir, on_conflict: overwrite }\n",
        );
        fs::write(f.home().join(".rc"), "mine").unwrap();
        fs::create_dir_all(f.home().join(".dir/sub")).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(f.apply(&actions), [Outcome::Changed, Outcome::Changed]);
        assert_eq!(
            fs::read_to_string(f.home().join(".rc")).unwrap(),
            "rc contents"
        );
        assert!(f.home().join(".dir/inner").exists());
        assert!(!f.home().join(".rc.dot-backup").exists());
    }

    #[test]
    fn fail_leaves_existing_alone() {
        let f = fixture("      ~/.rc: { src: files/rc, on_conflict: fail }\n");
        fs::write(f.home().join(".rc"), "mine").unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Conflict);
        let err = f.apply_err(actions[0].as_ref());
        assert!(err.contains("on_conflict: fail"), "{err}");
        assert_eq!(fs::read_to_string(f.home().join(".rc")).unwrap(), "mine");
    }

    #[test]
    fn missing_source_is_unknown_until_applied() {
        // An earlier step may still create it, so planning does not fail...
        let f = fixture("      ~/.a: files/rc\n      ~/.b: nope\n");
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[1].status().unwrap(), Status::Unknown);
        assert!(actions[1].describe().contains("source does not exist yet"));
        // ...but applying with the source still missing does, and changes nothing.
        let err = f.apply_err(actions[1].as_ref());
        assert!(err.contains("link source not found: `nope`"), "{err}");
        assert!(fs::symlink_metadata(f.home().join(".b")).is_err());
        // Once it exists, the same action works.
        f.write_repo("nope", "late");
        assert_eq!(actions[1].status().unwrap(), Status::Pending);
        assert_eq!(f.apply(&actions[1..]), [Outcome::Changed]);
    }

    #[test]
    fn refuses_self_link_through_a_symlinked_parent() {
        // ~/.config/nvim -> <repo>/nvim, then ~/.config/nvim/init.lua -> nvim/init.lua would
        // back up (or delete) the tracked file and leave it a link to itself.
        let f = Fixture::new(
            "steps:\n  - link: { ~/.config/nvim: nvim }\n  - link: { ~/.config/nvim/init.lua: nvim/init.lua }\n  - link:\n      ~/.config/nvim/init.lua: { src: nvim/init.lua, on_conflict: overwrite }\n",
        );
        f.write_repo("nvim/init.lua", "real content");
        assert_eq!(f.apply(&f.plan(0).unwrap()), [Outcome::Changed]);
        for step in [1, 2] {
            let err = format!("{:#}", f.plan(step).err().unwrap());
            assert!(err.contains("inside the repository"), "{err}");
            assert!(err.contains("through a symlinked parent"), "{err}");
        }
        // The apply-time re-check catches it even for an action planned before the parent
        // became a link.
        let g = Fixture::new("steps:\n  - link: { ~/.config/nvim/init.lua: nvim/init.lua }\n");
        g.write_repo("nvim/init.lua", "real content");
        let late = g.plan(0).unwrap();
        fs::create_dir_all(g.home().join(".config")).unwrap();
        symlink(g.repo().join("nvim"), g.home().join(".config/nvim")).unwrap();
        let err = g.apply_err(late[0].as_ref());
        assert!(err.contains("inside the repository"), "{err}");
        for fx in [&f, &g] {
            let src = fx.repo().join("nvim/init.lua");
            assert_eq!(fs::read_to_string(&src).unwrap(), "real content");
            assert!(!fs::symlink_metadata(&src).unwrap().file_type().is_symlink());
            assert!(!fx.repo().join("nvim/init.lua.dot-backup").exists());
        }
    }

    #[test]
    fn a_self_loop_is_never_satisfied() {
        let f = fixture("      ~/.rc: files/rc\n");
        let dest = f.home().join(".rc");
        symlink(&dest, &dest).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "rc contents");
    }

    #[test]
    fn refuses_home_roots_and_repo_ancestors() {
        let f = Fixture::new("steps: []\n");
        fs::write(f.home().join("keep"), "precious").unwrap();
        f.write_repo("dir/x", "");
        let repo_parent = f.root().parent().unwrap().display().to_string();
        let cases = [
            ("~", "home directory"),
            ("~/", "home directory"),
            ("~/..", "home directory"),
            ("/", "filesystem root"),
            (
                repo_parent.as_str(),
                "home directory or one of its parents|contains the repository",
            ),
            (&f.root().display().to_string(), "contains the repository"),
            (
                &f.repo().join("dir/x").display().to_string(),
                "inside the repository",
            ),
        ];
        for (dest, why) in cases {
            let yaml = format!(
                "steps:\n  - link:\n      '{dest}': {{ src: dir, on_conflict: overwrite }}\n"
            );
            let g = f.with_bundle(&yaml);
            // Planning only: these must never get as far as applying.
            let err = format!("{:#}", g.plan(0).err().unwrap_or_else(|| panic!("{dest}")));
            assert!(why.split('|').any(|w| err.contains(w)), "{dest}: {err}");
        }
        assert_eq!(
            fs::read_to_string(f.home().join("keep")).unwrap(),
            "precious"
        );
    }

    #[test]
    fn refuses_a_destination_containing_an_outside_source() {
        let f = Fixture::new("steps:\n  - link: { ~/.oh: ~/.oh/theme }\n");
        fs::create_dir(f.home().join(".oh")).unwrap();
        fs::write(f.home().join(".oh/theme"), "t").unwrap();
        let err = format!("{:#}", f.plan(0).err().unwrap());
        assert!(err.contains("contains the link source"), "{err}");
    }

    #[test]
    fn backup_path_keeps_non_utf8_parents() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let dest = Path::new(OsStr::from_bytes(b"/nonexistent-dot-test/caf\xe9/.rc"));
        let backup = backup_path(dest);
        assert_eq!(
            backup.as_os_str().as_bytes(),
            b"/nonexistent-dot-test/caf\xe9/.rc.dot-backup"
        );
        assert_eq!(backup.parent(), dest.parent());
    }

    #[test]
    fn inherited_defaults_apply_to_unset_options() {
        let f = fixture("      ~/.rc: files/rc\n      ~/.rc2: { src: files/rc, relink: true }\n");
        let other = f.home().join("other");
        fs::write(&other, "").unwrap();
        symlink(&other, f.home().join(".rc")).unwrap();
        symlink(&other, f.home().join(".rc2")).unwrap();
        let mut ctx = f.context();
        ctx.defaults.link.relink = Some(false);
        let actions = f.plan_with(0, &ctx).unwrap();
        let err = f.apply_err(actions[0].as_ref());
        assert!(err.contains("relink is false"), "{err}");
        assert_eq!(
            f.apply(&actions[1..]),
            [Outcome::Changed],
            "explicit value wins"
        );
    }

    use std::sync::Arc;

    use dot_platform::link::{LinkCaps, SystemLinks};

    /// Builtins on a machine without symlinks (Windows without Developer Mode): files are
    /// copied, directories get junctions (plain symlinks off Windows).
    fn no_symlinks() -> crate::Builtins {
        crate::Builtins {
            links: Arc::new(SystemLinks::with_caps(LinkCaps {
                symlinks: false,
                junctions: true,
            })),
            ..crate::Builtins::default()
        }
    }

    fn state(f: &Fixture) -> LinkState {
        LinkState::in_dir(&f.context().state_dir)
    }

    #[test]
    fn without_symlinks_files_are_copied_and_directories_junctioned_and_recorded() {
        let f = fixture("      ~/.rc: files/rc\n      ~/.dir: files/dir\n");
        let b = no_symlinks();
        let ctx = f.context();
        let actions = f.plan_by(0, &ctx, &b).unwrap();
        let described: Vec<String> = actions.iter().map(|a| a.describe()).collect();
        assert_eq!(
            described,
            [
                "link ~/.rc -> files/rc (copied (symlinks unavailable))",
                "link ~/.dir -> files/dir (junction (symlinks unavailable))",
            ]
        );
        assert_eq!(f.apply(&actions), [Outcome::Changed, Outcome::Changed]);
        let rc = f.home().join(".rc");
        assert!(fs::symlink_metadata(&rc).unwrap().file_type().is_file());
        assert_eq!(fs::read_to_string(&rc).unwrap(), "rc contents");
        assert_eq!(
            fs::read_to_string(f.home().join(".dir/inner")).unwrap(),
            "x"
        );
        let recorded: Vec<(String, String)> = state(&f)
            .all()
            .into_iter()
            .map(|r| {
                (
                    r.dest.file_name().unwrap().to_string_lossy().into_owned(),
                    r.kind,
                )
            })
            .collect();
        assert_eq!(
            recorded,
            [
                (".dir".into(), "junction".into()),
                (".rc".into(), "copy".into())
            ]
        );

        // Converged: the copy's bytes equal the source's.
        let actions = f.plan_by(0, &ctx, &b).unwrap();
        let statuses: Vec<Status> = actions.iter().map(|a| a.status().unwrap()).collect();
        assert_eq!(statuses, [Status::Satisfied, Status::Satisfied]);

        // The source changed: copied again, no backup (the copy is dot's own).
        f.write_repo("files/rc", "new contents");
        let actions = f.plan_by(0, &ctx, &b).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert!(
            actions[0]
                .describe()
                .contains("the source changed: copied again"),
            "{}",
            actions[0].describe()
        );
        f.apply(&actions[..1]);
        assert_eq!(fs::read_to_string(&rc).unwrap(), "new contents");
        assert!(!f.home().join(".rc.dot-backup").exists());
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);

        // Edited by the user since: theirs now, backed up like any other file.
        fs::write(&rc, "my edit").unwrap();
        let actions = f.plan_by(0, &ctx, &b).unwrap();
        assert!(
            actions[0]
                .describe()
                .contains("existing file moved to .dot-backup")
        );
        f.apply(&actions[..1]);
        assert_eq!(
            fs::read_to_string(f.home().join(".rc.dot-backup")).unwrap(),
            "my edit"
        );
        assert_eq!(fs::read_to_string(&rc).unwrap(), "new contents");

        // Symlinks available now (Developer Mode turned on): the copy becomes a link and its
        // record goes.
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert!(actions[0].describe().contains("replaces the copy"));
        assert_eq!(actions[1].status().unwrap(), Status::Satisfied);
        f.apply(&actions);
        assert!(fs::symlink_metadata(&rc).unwrap().file_type().is_symlink());
        assert!(state(&f).get(&rc).is_none());
        assert!(state(&f).get(&f.home().join(".dir")).is_some());
    }

    #[test]
    fn a_directory_without_symlinks_or_junctions_is_a_conflict() {
        let f = fixture("      ~/.dir: files/dir\n");
        let b = crate::Builtins {
            links: Arc::new(SystemLinks::with_caps(LinkCaps {
                symlinks: false,
                junctions: false,
            })),
            ..crate::Builtins::default()
        };
        let actions = f.plan_by(0, &f.context(), &b).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Conflict);
        assert!(actions[0].describe().contains("Developer Mode"));
        let err = f.apply_err(actions[0].as_ref());
        assert!(err.contains("cannot link a directory"), "{err}");
    }

    #[test]
    fn clean_removes_stale_copies_it_made_and_nothing_else() {
        let f = Fixture::new(
            "steps:\n  - link:\n      ~/.a: a\n      ~/.b: b\n      ~/sub/.c: c\n  - clean: [~/]\n",
        );
        for n in ["a", "b", "c"] {
            f.write_repo(n, n);
        }
        let b = no_symlinks();
        let ctx = f.context();
        f.apply(&f.plan_by(0, &ctx, &b).unwrap());
        assert!(
            f.plan_by(1, &ctx, &b).unwrap().is_empty(),
            "sources all there"
        );
        for n in ["a", "b", "c"] {
            fs::remove_file(f.repo().join(n)).unwrap();
        }
        fs::write(f.home().join(".b"), "edited: the user's now").unwrap();
        let actions = f.plan_by(1, &ctx, &b).unwrap();
        let described: Vec<String> = actions.iter().map(|a| a.describe()).collect();
        assert_eq!(described.len(), 1, "{described:?}");
        assert!(
            described[0].starts_with("remove stale copy ~/.a (a copy of "),
            "{described:?}"
        );
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        assert!(!f.home().join(".a").exists());
        assert!(state(&f).get(&f.home().join(".a")).is_none());
        assert_eq!(
            fs::read_to_string(f.home().join(".b")).unwrap(),
            "edited: the user's now"
        );
        assert!(f.home().join("sub/.c").exists(), "not recursive");
    }

    #[test]
    fn dangling_source_link_counts_as_existing() {
        let f = fixture("      ~/.a: files/link\n");
        symlink("nowhere", f.repo().join("files/link")).unwrap();
        assert_eq!(f.plan(0).unwrap().len(), 1);
    }
}
