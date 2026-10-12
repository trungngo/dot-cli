//! `copy`: like `link`, but the destination gets a copy of the source (for files a program
//! refuses to read through a symlink, or that must survive the repository moving). A source
//! directory is copied file by file; files in the destination that the source does not have
//! are left alone, except the ones `dot` copied there on an earlier run (recorded in the state
//! store) that the source no longer has: those are pruned while they still hold what `dot`
//! wrote, and kept (and reported) once the user changed them. Satisfied when every copied
//! file's bytes (and mode, when one is given) are equal and nothing is left to prune.
//! `root: true` makes root-owned copies (see [`crate::rootfile`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use dot_core::config::FileEntry;
use dot_core::paths::{on_disk, starts_with_fold};
use dot_core::{Action, Outcome, PlanCtx, Resource, RunCtx, StateStore, Status, Undo, Written};
use dot_platform::exec::Cmd;
use dot_platform::link::LinkStrategy;

use crate::fileutil::{
    DEFAULT_FILE_MODE, Existing, atomic_write, change_note, expand_dest, file_diff, guard,
    is_dest_pattern, mode_of, mode_ok, new_file_note, unwritable, unwritable_note, with_notes,
    within, written,
};
use crate::linkstate::sha256_file;
use crate::rootfile::{self, RootExisting, RootFiles};

pub fn plan(
    entries: &[FileEntry],
    ctx: &PlanCtx<'_>,
    links: &Arc<dyn LinkStrategy>,
    root_files: &RootFiles,
) -> Result<Vec<Box<dyn Action>>> {
    let mut actions: Vec<Box<dyn Action>> = Vec::new();
    // What earlier runs copied, by resource key: a directory copy prunes what its source lost.
    let state = StateStore::at(ctx.state_dir).read();
    for e in entries {
        if e.root {
            RootFiles::check_supported(ctx.facts.os).map_err(|m| ctx.config_error(m))?;
        }
        let pattern = ctx.target(&e.dest)?;
        let pattern = if e.root {
            root_files.map(pattern)
        } else {
            pattern
        };
        let src = ctx.source(&e.src)?;
        let mode = e
            .mode
            .as_ref()
            .map(|m| ctx.mode(m))
            .transpose()?
            .map(|m| m.0);
        // A pattern (`~/.config/JetBrains/*/options/x.xml`): every place it matches now.
        let dests = if is_dest_pattern(&pattern) {
            expand_dest(&pattern)
        } else {
            vec![pattern]
        };
        for dest in dests {
            let show_dest = ctx.show(&dest);
            let recorded = match state
                .resources
                .get(&format!("file:{show_dest}"))
                .and_then(|r| r.undo.as_ref())
            {
                Some(Undo::RemoveFiles { files, .. }) => files.clone(),
                _ => Vec::new(),
            };
            let action = CopyAction {
                show_dest,
                recorded,
                show_src: ctx.show_source(&src),
                src: src.clone(),
                dest,
                mode,
                home: ctx.home.to_path_buf(),
                root: ctx.bundle.root.clone(),
                links: links.clone(),
                as_root: e.root.then(|| root_files.clone()),
            };
            action.guard()?;
            actions.push(Box::new(action));
        }
    }
    Ok(actions)
}

pub struct CopyAction {
    pub src: PathBuf,
    pub dest: PathBuf,
    /// `None`: the source file's mode.
    pub mode: Option<u32>,
    home: PathBuf,
    root: PathBuf,
    show_src: String,
    show_dest: String,
    /// Removes a link (or junction) at the destination.
    links: Arc<dyn LinkStrategy>,
    /// `root: true`: root-owned copies, written through sudo.
    as_root: Option<RootFiles>,
    /// The files an earlier run copied (the state store's undo list): the ones under a
    /// directory destination that the source no longer has are pruned.
    recorded: Vec<Written>,
}

/// One file to copy.
struct Pair {
    src: PathBuf,
    dest: PathBuf,
}

/// What differs between the source and the destination.
struct Diff {
    /// Files whose content or mode differs, with what is there now.
    changed: Vec<(Pair, Existing)>,
    total: usize,
    /// The destination itself is a symlink (replaced, never written through).
    dest_link: Option<PathBuf>,
    src_is_dir: bool,
    /// Root-owned destination files this user cannot read before sudo is primed.
    unreadable: bool,
    /// Files an earlier run copied that the source no longer has.
    stale: Vec<Stale>,
}

/// A file `dot` copied on an earlier run whose source is gone.
struct Stale {
    path: PathBuf,
    /// Changed since `dot` wrote it (or no longer a regular file): kept, and reported.
    edited: bool,
}

impl Diff {
    fn prune(&self) -> impl Iterator<Item = &Stale> {
        self.stale.iter().filter(|s| !s.edited)
    }

    fn kept(&self) -> impl Iterator<Item = &Stale> {
        self.stale.iter().filter(|s| s.edited)
    }

    /// Nothing to copy, replace or prune.
    fn in_sync(&self) -> bool {
        self.changed.is_empty() && self.dest_link.is_none() && self.stale.is_empty()
    }
}

impl CopyAction {
    fn guard(&self) -> Result<()> {
        guard(&self.dest, &self.show_dest, &self.home, &self.root)?;
        if let (Some(src), Some(dest)) = (on_disk(&self.src), on_disk(&self.dest))
            && (within(&dest, &src) || within(&src, &dest))
        {
            bail!(
                "refusing to copy {} to {}: one contains the other",
                self.show_src,
                self.show_dest
            );
        }
        Ok(())
    }

    fn source_exists(&self) -> bool {
        fs::metadata(&self.src).is_ok()
    }

    /// The files to copy: the source itself, or every file under a source directory.
    fn pairs(&self) -> io::Result<(bool, Vec<Pair>)> {
        if !fs::metadata(&self.src)?.is_dir() {
            return Ok((
                false,
                vec![Pair {
                    src: self.src.clone(),
                    dest: self.dest.clone(),
                }],
            ));
        }
        let mut out = Vec::new();
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            let mut entries: Vec<_> =
                fs::read_dir(self.src.join(&rel))?.collect::<io::Result<Vec<_>>>()?;
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let r = rel.join(entry.file_name());
                // Follows links inside the source: their content is what gets copied.
                match fs::metadata(entry.path()) {
                    Ok(m) if m.is_dir() => stack.push(r),
                    Ok(_) => out.push(Pair {
                        src: self.src.join(&r),
                        dest: self.dest.join(&r),
                    }),
                    Err(_) => {} // a dangling link: nothing to copy
                }
            }
        }
        out.sort_by(|a, b| a.src.cmp(&b.src));
        Ok((true, out))
    }

    fn want_mode(&self, src: &Path) -> Option<u32> {
        self.mode
            .or_else(|| fs::metadata(src).ok().and_then(|m| mode_of(&m)))
    }

    fn compare(&self) -> io::Result<Diff> {
        let (src_is_dir, pairs) = self.pairs()?;
        let dest_link = match Existing::peek(&self.dest)? {
            Existing::Symlink(t) => Some(t),
            _ => None,
        };
        let total = pairs.len();
        // Under a link nothing is `dot`'s: the link is replaced, not pruned through.
        let stale = if src_is_dir && dest_link.is_none() {
            self.stale(&pairs)?
        } else {
            Vec::new()
        };
        let mut changed = Vec::new();
        let mut unreadable = false;
        for pair in pairs {
            let (existing, owned) = if dest_link.is_some() {
                // Nothing under a link counts: it is replaced.
                (Existing::Missing, true)
            } else if let Some(r) = &self.as_root {
                match r.read(&pair.dest).map_err(io::Error::other)? {
                    RootExisting::Read { existing, uid } => (existing, r.owned(uid)),
                    RootExisting::Unreadable => {
                        unreadable = true;
                        (Existing::Missing, false)
                    }
                }
            } else {
                match Existing::read(&pair.dest) {
                    // A file where the source has a directory (a stale copy, pruned first).
                    Err(e) if e.kind() == io::ErrorKind::NotADirectory => (Existing::Missing, true),
                    r => (r?, true),
                }
            };
            let same = match existing.bytes() {
                Some(have) => {
                    have == fs::read(&pair.src)?.as_slice()
                        && mode_ok(existing.mode(), self.mode)
                        && owned
                }
                None => false,
            };
            if !same {
                changed.push((pair, existing));
            }
        }
        Ok(Diff {
            changed,
            total,
            dest_link,
            src_is_dir,
            unreadable,
            stale,
        })
    }

    /// The recorded copies under the destination directory that `pairs` no longer has and
    /// that are still there.
    fn stale(&self, pairs: &[Pair]) -> io::Result<Vec<Stale>> {
        let wanted: std::collections::BTreeSet<&Path> =
            pairs.iter().map(|p| p.dest.as_path()).collect();
        let mut out = Vec::new();
        for w in &self.recorded {
            if w.path == self.dest
                || !w.path.starts_with(&self.dest)
                || wanted.contains(w.path.as_path())
                // Renamed only by case on a case-insensitive filesystem: the recorded name is
                // the wanted file itself, which pruning would delete.
                || wanted.iter().any(|d| same_file_by_case(&w.path, d))
            {
                continue;
            }
            let existing = match &self.as_root {
                Some(r) => match r.read(&w.path).map_err(io::Error::other)? {
                    RootExisting::Read { existing, .. } => existing,
                    // Cannot tell whether it changed: kept.
                    RootExisting::Unreadable => Existing::Other,
                },
                None => Existing::read(&w.path)?,
            };
            let edited = match existing {
                Existing::Missing => continue,
                Existing::File { bytes, .. } => {
                    w.sha256.as_deref() != Some(crate::fileutil::sha256_hex(&bytes).as_str())
                }
                _ => true,
            };
            out.push(Stale {
                path: w.path.clone(),
                edited,
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// `path` as shown: under the shown destination.
    fn show_under(&self, path: &Path) -> String {
        match path.strip_prefix(&self.dest) {
            Ok(rel) => format!(
                "{}/{}",
                self.show_dest.trim_end_matches(['/', '\\']),
                rel.display()
            ),
            Err(_) => path.display().to_string(),
        }
    }

    /// Removes the stale copies that still hold what `dot` wrote, then the directories that
    /// held them while they are empty and the source no longer has them.
    fn prune(&self, diff: &Diff, ctx: &RunCtx) -> Result<()> {
        for s in diff.prune() {
            if let Some(r) = &self.as_root {
                r.run(
                    Cmd::new("rm")
                        .args(["-f", "--"])
                        .arg(s.path.display().to_string()),
                    ctx,
                )?;
                continue;
            }
            match fs::remove_file(&s.path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("removing {}", s.path.display()));
                }
                _ => {}
            }
            let mut dir = s.path.parent();
            while let Some(d) = dir {
                let Ok(rel) = d.strip_prefix(&self.dest) else {
                    break;
                };
                if rel.as_os_str().is_empty()
                    || self.src.join(rel).exists()
                    || fs::remove_dir(d).is_err()
                {
                    break;
                }
                dir = d.parent();
            }
        }
        for s in diff.kept() {
            ctx.notice(format!(
                "kept {}: dot copied it, the source no longer has it, but it changed since",
                self.show_under(&s.path)
            ));
        }
        Ok(())
    }

    /// The status before [`unwritable`] is considered.
    fn base_status(&self) -> Result<Status> {
        if !self.source_exists() {
            return Ok(Status::Unknown);
        }
        let diff = self
            .compare()
            .with_context(|| format!("comparing {} with {}", self.show_dest, self.show_src))?;
        let in_the_way = !diff.src_is_dir
            && diff
                .changed
                .first()
                .is_some_and(|(_, e)| matches!(e, Existing::Dir | Existing::Other));
        Ok(if diff.unreadable {
            Status::Unknown
        } else if diff.in_sync() {
            Status::Satisfied
        } else if in_the_way {
            Status::Conflict
        } else {
            Status::Pending
        })
    }

    /// A pending write this user cannot make ([`unwritable`]); root-owned copies are written
    /// through sudo instead.
    fn blocked(&self) -> Option<PathBuf> {
        if self.as_root.is_some() || !matches!(self.base_status(), Ok(Status::Pending)) {
            return None;
        }
        unwritable(&self.dest)
    }
}

/// Whether `a` and `b` differ at most by case and name the same existing file (a case-insensitive
/// filesystem: APFS, NTFS). On a case-sensitive one they are two files, or `b` is missing.
fn same_file_by_case(a: &Path, b: &Path) -> bool {
    if a == b || !(starts_with_fold(a, b, true) && starts_with_fold(b, a, true)) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        // The final path of an open handle carries the on-disk case.
        match (a.canonicalize(), b.canonicalize()) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }
}

impl Action for CopyAction {
    fn needs_sudo(&self) -> bool {
        self.as_root.as_ref().is_some_and(RootFiles::needs_sudo)
    }

    fn describe(&self) -> String {
        let base = format!("copy {} <- {}", self.show_dest, self.show_src);
        if !self.source_exists() {
            return with_notes(
                base,
                &["source does not exist yet; an earlier step may create it".into()],
            );
        }
        let diff = match self.compare() {
            Ok(d) => d,
            Err(e) => return with_notes(base, &[format!("cannot inspect: {e}")]),
        };
        let mut notes = Vec::new();
        if diff.unreadable {
            notes.push(rootfile::unreadable_note());
        }
        if let Some(t) = &diff.dest_link {
            notes.push(format!("replaces link to {}", t.display()));
        }
        if diff.src_is_dir {
            if !diff.changed.is_empty() {
                notes.push(format!(
                    "{} of {} files to copy",
                    diff.changed.len(),
                    diff.total
                ));
            }
            let (prune, kept) = (diff.prune().count(), diff.kept().count());
            if prune > 0 {
                notes.push(format!(
                    "{prune} stale {} to remove",
                    if prune == 1 { "file" } else { "files" }
                ));
            }
            if kept > 0 {
                notes.push(format!(
                    "{kept} stale {} kept: changed since copied",
                    if kept == 1 { "file" } else { "files" }
                ));
            }
        } else if let Some((pair, existing)) = diff.changed.first() {
            let new = fs::read(&pair.src).unwrap_or_default();
            match existing {
                Existing::Missing if diff.dest_link.is_none() => notes.push(new_file_note(&new)),
                Existing::File { bytes, mode } => {
                    if *bytes != new {
                        notes.push(change_note(bytes, &new));
                    }
                    if let (Some(have), Some(want)) = (mode, self.mode)
                        && *have != want
                    {
                        notes.push(format!("mode {have:04o} -> {want:04o}"));
                    }
                }
                Existing::Dir | Existing::Other => {
                    notes.push("conflict: not a regular file".into())
                }
                _ => {}
            }
        }
        if self.as_root.is_some() && (!diff.changed.is_empty() || diff.dest_link.is_some()) {
            if notes.is_empty() {
                notes.push("owner -> root".into());
            }
            notes.push(rootfile::AS_ROOT.into());
        }
        if let Some(dir) = self.blocked() {
            notes.push(unwritable_note(&dir));
        }
        with_notes(base, &notes)
    }

    fn status(&self) -> Result<Status> {
        let status = self.base_status()?;
        Ok(if status == Status::Pending && self.blocked().is_some() {
            Status::Conflict
        } else {
            status
        })
    }

    /// `file:<dest>`: every file copied, removed while it holds what `dot` copied; a single
    /// file that was there before is saved first and put back.
    fn resources(&self) -> Vec<Resource> {
        let (is_dir, pairs) = self.pairs().unwrap_or((false, Vec::new()));
        let root = self.as_root.is_some();
        // A root-owned copy may not be readable by this user: it holds its source's bytes.
        let written_pair = |p: &Pair| match sha256_file(&p.src) {
            Ok(sha) if root => Written {
                path: p.dest.clone(),
                sha256: Some(sha),
            },
            _ => written(&p.dest),
        };
        let files = if pairs.is_empty() {
            vec![written(&self.dest)]
        } else {
            pairs.iter().map(written_pair).collect()
        };
        let r = Resource::new("file", self.show_dest.clone())
            .with_undo(Undo::RemoveFiles { files, root });
        // A root-owned file is saved by `apply`, which may read it through sudo.
        vec![if is_dir || root {
            r
        } else {
            r.preserving(&self.dest)
        }]
    }

    fn diff(&self) -> Result<Option<dot_core::Diff>> {
        if !self.source_exists() {
            return Ok(None);
        }
        let diff = self.compare()?;
        if diff.unreadable {
            return Ok(Some(dot_core::Diff::Note(format!(
                "{}: {}",
                self.show_dest,
                rootfile::unreadable_note()
            ))));
        }
        let mut out = String::new();
        let mut notes = Vec::new();
        if let Some(t) = &diff.dest_link {
            notes.push(format!(
                "{} is a link to {}: replaced",
                self.show_dest,
                t.display()
            ));
        }
        for (pair, existing) in &diff.changed {
            let show = if diff.src_is_dir {
                match pair.dest.strip_prefix(&self.dest) {
                    Ok(rel) => format!(
                        "{}/{}",
                        self.show_dest.trim_end_matches(['/', '\\']),
                        rel.display()
                    ),
                    Err(_) => pair.dest.display().to_string(),
                }
            } else {
                self.show_dest.clone()
            };
            let new =
                fs::read(&pair.src).with_context(|| format!("reading {}", pair.src.display()))?;
            let label = format!("{show} (copy of {})", pair.src.display());
            match file_diff(&show, existing, &new, &label, self.mode) {
                Some(dot_core::Diff::File { unified, .. }) => out.push_str(&unified),
                Some(d) => notes.push(d.to_string().trim_end().to_string()),
                None => {}
            }
        }
        for s in &diff.stale {
            notes.push(if s.edited {
                format!(
                    "{}: no longer in the source, but changed since dot copied it: kept",
                    self.show_under(&s.path)
                )
            } else {
                format!(
                    "{}: no longer in the source: removed",
                    self.show_under(&s.path)
                )
            });
        }
        for n in notes {
            out.push_str(&n);
            out.push('\n');
        }
        Ok((!out.is_empty()).then(|| dot_core::Diff::File {
            path: self.show_dest.clone(),
            unified: out,
        }))
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        self.guard()?;
        if !self.source_exists() {
            bail!(
                "copy source not found: `{}` ({})",
                self.show_src,
                self.src.display()
            );
        }
        let diff = self.compare()?;
        if diff.unreadable {
            bail!("cannot read {}, even through sudo", self.dest.display());
        }
        if diff.in_sync() {
            return Ok(Outcome::Ok);
        }
        // Before copying: a stale file may sit where the source now has a directory.
        self.prune(&diff, ctx)?;
        if diff.changed.is_empty() && diff.dest_link.is_none() && diff.prune().next().is_none() {
            // Only edited stale files: kept, and no longer `dot`'s once recorded again.
            return Ok(Outcome::Ok);
        }
        if let Some(r) = &self.as_root {
            // `install` would write through a link: it goes first.
            r.remove_link(&self.dest, ctx)?;
        } else if diff.dest_link.is_some() && diff.src_is_dir {
            // Writing under the link would write into whatever it points at.
            self.links
                .remove(&self.dest)
                .with_context(|| format!("removing link {}", self.dest.display()))?;
        }
        match Existing::peek(&self.dest)? {
            Existing::Dir if !diff.src_is_dir => {
                bail!(
                    "{} is a directory; the source is a file",
                    self.dest.display()
                )
            }
            Existing::File { .. } if diff.src_is_dir => {
                bail!(
                    "{} is a file; the source is a directory",
                    self.dest.display()
                )
            }
            _ => {}
        }
        for (pair, existing) in &diff.changed {
            if matches!(existing, Existing::Dir | Existing::Other) {
                bail!("{} exists and is not a regular file", pair.dest.display());
            }
            let mode = self.want_mode(&pair.src).unwrap_or(DEFAULT_FILE_MODE);
            if let Some(r) = &self.as_root {
                if !diff.src_is_dir
                    && let Existing::File { bytes, mode: have } = existing
                {
                    let key = format!("file:{}", self.show_dest);
                    ctx.preserve_root(&key, &pair.dest, bytes, have.unwrap_or(DEFAULT_FILE_MODE))?;
                }
                r.remove_link(&pair.dest, ctx)?;
                r.install(&pair.src, &pair.dest, mode, ctx)?;
                continue;
            }
            let bytes =
                fs::read(&pair.src).with_context(|| format!("reading {}", pair.src.display()))?;
            atomic_write(&pair.dest, &bytes, mode)?;
        }
        Ok(Outcome::Changed)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::testutil::Fixture;

    fn mode_of(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn copies_a_file_and_converges() {
        let f = Fixture::new("steps:\n  - copy:\n      ~/.config/app/rc: files/rc\n");
        f.write_repo("files/rc", "a\nb\n");
        fs::set_permissions(f.repo().join("files/rc"), fs::Permissions::from_mode(0o755)).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(
            actions[0].describe(),
            "copy ~/.config/app/rc <- files/rc (new file, 2 lines)"
        );
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        let dest = f.home().join(".config/app/rc");
        assert!(
            !fs::symlink_metadata(&dest)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "a\nb\n");
        assert_eq!(mode_of(&dest), 0o755, "source mode kept");
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
        fs::write(&dest, "a\nc\n").unwrap();
        assert!(actions[0].describe().ends_with("(+1/-1 lines)"));
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        f.apply(&actions);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "a\nb\n");
    }

    #[test]
    fn copies_a_directory_and_leaves_extra_files() {
        let f = Fixture::new("steps:\n  - copy:\n      ~/.app: { src: app, mode: \"0600\" }\n");
        f.write_repo("app/one", "1");
        f.write_repo("app/sub/two", "2");
        let dest = f.home().join(".app");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("mine"), "keep").unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(
            actions[0].describe(),
            "copy ~/.app <- app (2 of 2 files to copy)"
        );
        f.apply(&actions);
        assert_eq!(fs::read_to_string(dest.join("sub/two")).unwrap(), "2");
        assert_eq!(mode_of(&dest.join("one")), 0o600);
        assert_eq!(fs::read_to_string(dest.join("mine")).unwrap(), "keep");
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
        fs::write(dest.join("one"), "changed").unwrap();
        assert_eq!(
            actions[0].describe(),
            "copy ~/.app <- app (1 of 2 files to copy)"
        );
    }

    /// Applies like a real run: the resources are recorded in the state store.
    fn apply_recorded(f: &Fixture, actions: &[Box<dyn Action>]) -> Vec<Outcome> {
        let store = StateStore::at(&f.context().state_dir);
        actions
            .iter()
            .map(|a| {
                let o = f.apply_one(a.as_ref()).unwrap();
                store
                    .record_applied("demo", &a.resources(), o == Outcome::Changed, &[])
                    .unwrap();
                o
            })
            .collect()
    }

    #[test]
    fn prunes_what_the_source_no_longer_has() {
        let f = Fixture::new("steps:\n  - copy:\n      ~/.app: app\n");
        f.write_repo("app/keep", "k");
        f.write_repo("app/gone", "g");
        f.write_repo("app/sub/old", "o");
        f.write_repo("app/edited", "e");
        let dest = f.home().join(".app");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("mine"), "not dot's").unwrap();
        apply_recorded(&f, &f.plan(0).unwrap());
        // The source loses three files; the user edits one of the copies.
        for rel in ["app/gone", "app/sub/old", "app/edited"] {
            fs::remove_file(f.repo().join(rel)).unwrap();
        }
        fs::remove_dir(f.repo().join("app/sub")).unwrap();
        fs::write(dest.join("edited"), "mine now").unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert_eq!(
            actions[0].describe(),
            "copy ~/.app <- app (2 stale files to remove; 1 stale file kept: changed since copied)"
        );
        let diff = actions[0].diff().unwrap().unwrap().to_string();
        assert!(
            diff.contains("~/.app/gone: no longer in the source: removed"),
            "{diff}"
        );
        assert!(
            diff.contains("~/.app/edited: no longer in the source, but changed"),
            "{diff}"
        );
        assert_eq!(apply_recorded(&f, &actions), [Outcome::Changed]);
        assert!(!dest.join("gone").exists());
        assert!(!dest.join("sub").exists(), "emptied directory removed");
        assert_eq!(fs::read_to_string(dest.join("edited")).unwrap(), "mine now");
        assert_eq!(fs::read_to_string(dest.join("mine")).unwrap(), "not dot's");
        assert_eq!(fs::read_to_string(dest.join("keep")).unwrap(), "k");
        // Converged: the kept file is no longer recorded as `dot`'s.
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
        assert_eq!(actions[0].describe(), "copy ~/.app <- app");
        let Some(Undo::RemoveFiles { files, .. }) = &actions[0].resources()[0].undo else {
            panic!("no undo");
        };
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn a_rename_by_case_only_keeps_the_file() {
        let f = Fixture::new("steps:\n  - copy:\n      ~/.app: app\n");
        f.write_repo("app/Foo.py", "x = 1\n");
        apply_recorded(&f, &f.plan(0).unwrap());
        let dest = f.home().join(".app");
        let case_insensitive = dest.join("foo.py").exists();
        fs::rename(f.repo().join("app/Foo.py"), f.repo().join("app/tmp")).unwrap();
        fs::rename(f.repo().join("app/tmp"), f.repo().join("app/foo.py")).unwrap();
        let actions = f.plan(0).unwrap();
        apply_recorded(&f, &actions);
        // Either way the module is there after the run, and the run converged.
        assert_eq!(fs::read_to_string(dest.join("foo.py")).unwrap(), "x = 1\n");
        if !case_insensitive {
            assert!(!dest.join("Foo.py").exists(), "the old name is pruned");
        }
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
        assert_eq!(actions[0].describe(), "copy ~/.app <- app");
    }

    #[test]
    fn a_stale_file_where_the_source_now_has_a_directory_is_replaced() {
        let f = Fixture::new("steps:\n  - copy:\n      ~/.app: app\n");
        f.write_repo("app/x", "file");
        apply_recorded(&f, &f.plan(0).unwrap());
        fs::remove_file(f.repo().join("app/x")).unwrap();
        f.write_repo("app/x/inner", "i");
        let actions = f.plan(0).unwrap();
        assert_eq!(apply_recorded(&f, &actions), [Outcome::Changed]);
        assert_eq!(
            fs::read_to_string(f.home().join(".app/x/inner")).unwrap(),
            "i"
        );
        assert_eq!(f.plan(0).unwrap()[0].status().unwrap(), Status::Satisfied);
    }

    #[test]
    fn only_edited_stale_files_are_reported_once_then_forgotten() {
        let f = Fixture::new("steps:\n  - copy:\n      ~/.app: app\n");
        f.write_repo("app/a", "a");
        f.write_repo("app/b", "b");
        apply_recorded(&f, &f.plan(0).unwrap());
        fs::remove_file(f.repo().join("app/b")).unwrap();
        fs::write(f.home().join(".app/b"), "edited").unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        let ctx = RunCtx::new(&f.sink(), dot_core::StepId { bundle: 0, step: 0 });
        assert_eq!(actions[0].apply(&ctx).unwrap(), Outcome::Ok);
        assert_eq!(
            ctx.notices(),
            ["kept ~/.app/b: dot copied it, the source no longer has it, but it changed since"]
        );
        StateStore::at(&f.context().state_dir)
            .record_applied("demo", &actions[0].resources(), false, &[])
            .unwrap();
        assert_eq!(
            fs::read_to_string(f.home().join(".app/b")).unwrap(),
            "edited"
        );
        assert_eq!(f.plan(0).unwrap()[0].status().unwrap(), Status::Satisfied);
    }

    #[test]
    fn replaces_a_link_instead_of_writing_through_it() {
        let f = Fixture::new("steps:\n  - copy:\n      ~/.app: app\n");
        f.write_repo("app/one", "1");
        let elsewhere = f.home().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, f.home().join(".app")).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert!(actions[0].describe().contains("replaces link to"));
        f.apply(&actions);
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
        assert_eq!(fs::read_to_string(f.home().join(".app/one")).unwrap(), "1");
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    }

    #[test]
    fn missing_source_is_unknown_and_a_type_clash_fails() {
        let f = Fixture::new("steps:\n  - copy:\n      ~/.a: nope\n      ~/.b: file\n");
        f.write_repo("file", "x");
        fs::create_dir(f.home().join(".b")).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Unknown);
        let err = f.apply_err(actions[0].as_ref());
        assert!(err.contains("copy source not found: `nope`"), "{err}");
        assert!(
            actions[1].describe().contains("conflict"),
            "{}",
            actions[1].describe()
        );
        assert_eq!(actions[1].status().unwrap(), Status::Conflict);
        let err = f.apply_err(actions[1].as_ref());
        assert!(err.contains("is a directory"), "{err}");
    }

    #[test]
    fn refuses_to_copy_into_the_repository_or_itself() {
        let f = Fixture::repo_in_home("steps:\n  - copy:\n      ~/dotfiles/bundles/demo/x: y\n");
        f.write_repo("y", "");
        let err = format!("{:#}", f.plan(0).err().unwrap());
        assert!(err.contains("inside the repository"), "{err}");
        let g = Fixture::new("steps:\n  - copy:\n      ~/d/inner: ~/d\n");
        fs::create_dir(g.home().join("d")).unwrap();
        let err = format!("{:#}", g.plan(0).err().unwrap());
        assert!(err.contains("one contains the other"), "{err}");
    }

    #[test]
    fn a_pattern_destination_is_every_place_it_matches() {
        // One file into every JetBrains product's `options`.
        let f = Fixture::new(
            "steps:\n  - copy:\n      ~/.config/JetBrains/*/options/markdown.xml: markdown.xml\n",
        );
        f.write_repo("markdown.xml", "<x/>\n");
        for product in ["IntelliJIdea2025.2", "PyCharm2025.1"] {
            fs::create_dir_all(
                f.home()
                    .join(format!(".config/JetBrains/{product}/options")),
            )
            .unwrap();
        }
        // No `options` yet: not matched (the directories above must exist).
        fs::create_dir_all(f.home().join(".config/JetBrains/GoLand2025.1")).unwrap();
        let actions = f.plan(0).unwrap();
        let d: Vec<String> = actions.iter().map(|a| a.describe()).collect();
        assert_eq!(
            d,
            [
                "copy ~/.config/JetBrains/IntelliJIdea2025.2/options/markdown.xml <- markdown.xml (new file, 1 line)",
                "copy ~/.config/JetBrains/PyCharm2025.1/options/markdown.xml <- markdown.xml (new file, 1 line)",
            ]
        );
        f.apply(&actions);
        assert_eq!(
            fs::read_to_string(
                f.home()
                    .join(".config/JetBrains/PyCharm2025.1/options/markdown.xml")
            )
            .unwrap(),
            "<x/>\n"
        );
        // Nothing matches: nothing to do.
        let f = Fixture::new("steps:\n  - copy: { '~/nope/*/x': x }\n");
        f.write_repo("x", "");
        assert!(f.plan(0).unwrap().is_empty());
    }
}
