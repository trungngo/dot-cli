//! `block`: keep a block of lines in a file `dot` does not own (`~/.bashrc`, `~/.ssh/config`),
//! between marker lines:
//!
//! ```text
//! # >>> dot:<bundle>:<marker> >>>
//! ...content...
//! # <<< dot:<bundle>:<marker> <<<
//! ```
//!
//! (`dot:<bundle>` alone without a `marker`). The block is replaced in place when it changed,
//! appended when it is missing, and the file is created when absent; extra copies of the same
//! block are removed. Everything outside the block is left as it is. A file that is a symlink is
//! written through (its target is the real file), unless the target is inside the repository.
//!
//! `root: true` edits a root-owned file (`/etc/hosts`): read through sudo when this user may
//! not read it, written back whole through `sudo install` (see [`crate::rootfile`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use dot_core::config::BlockSpec;
use dot_core::paths::resolve_lenient;
use dot_core::{Action, Diff, Outcome, PlanCtx, Resource, RunCtx, Status, Undo};

use crate::fileutil::{
    DEFAULT_FILE_MODE, Existing, LineDiff, SECRET_FILE_MODE, atomic_write, guard, is_loose,
    unwritable, unwritable_note, with_notes,
};
use crate::rootfile::{self, RootExisting, RootFiles};

pub fn plan(
    spec: &BlockSpec,
    ctx: &PlanCtx<'_>,
    root_files: &RootFiles,
) -> Result<Vec<Box<dyn Action>>> {
    if spec.root {
        RootFiles::check_supported(ctx.facts.os).map_err(|m| ctx.config_error(m))?;
    }
    let file = ctx.target(&spec.file)?;
    let file = if spec.root {
        root_files.map(file)
    } else {
        file
    };
    let r = ctx.render_text(&spec.content, "block content")?;
    let id = match &spec.marker {
        Some(m) => format!("dot:{}:{m}", ctx.bundle.name),
        None => format!("dot:{}", ctx.bundle.name),
    };
    let action = BlockAction {
        show: ctx.show(&file),
        id: id.clone(),
        begin: format!("{} >>> {id} >>>", spec.comment),
        end: format!("{} <<< {id} <<<", spec.comment),
        body: (!r.placeholder).then_some(r.text),
        secrets: r.secrets,
        file,
        home: ctx.home.to_path_buf(),
        root: ctx.bundle.root.clone(),
        as_root: spec.root.then(|| root_files.clone()),
        created: std::sync::Mutex::default(),
    };
    action.target()?;
    Ok(vec![Box::new(action)])
}

pub struct BlockAction {
    pub file: PathBuf,
    /// `dot:<bundle>[:<marker>]`.
    id: String,
    pub begin: String,
    pub end: String,
    /// The rendered content; `None` while planning a block that uses secrets.
    body: Option<String>,
    /// Secrets the content uses: the file must then be readable by its owner only.
    secrets: Vec<String>,
    home: PathBuf,
    root: PathBuf,
    show: String,
    /// `root: true`: a root-owned file, read and written through sudo.
    as_root: Option<RootFiles>,
    /// Set by `apply` when it created the file: the missing directories it made for it,
    /// deepest first (see [`Undo::RemoveBlock`]).
    created: std::sync::Mutex<Option<Vec<PathBuf>>>,
}

/// The directories missing above `file`, deepest first: what creating it makes.
fn missing_dirs(file: &Path) -> Vec<PathBuf> {
    file.ancestors()
        .skip(1)
        .take_while(|d| !d.as_os_str().is_empty() && fs::symlink_metadata(d).is_err())
        .map(Path::to_path_buf)
        .collect()
}

/// The block put into the file as it is now.
struct Computed {
    new: Spliced,
    /// `None`: the file does not exist.
    old: Option<String>,
    mode: Option<u32>,
    /// Owned as it should be (by root, for `root: true`).
    owned: bool,
}

/// The result of putting the block into a file's text.
#[derive(Debug, PartialEq, Eq)]
pub struct Spliced {
    pub text: String,
    /// How many copies of the block the old text had.
    pub found: usize,
}

/// Puts the block (`begin`, the lines of `body`, `end`) into `text`: in place of the first copy
/// of it, removing any others, or appended. Marker lines match ignoring surrounding whitespace.
/// Line endings follow the file (`\r\n` when it uses them).
pub fn splice(text: &str, begin: &str, end: &str, body: &str) -> Result<Spliced, String> {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let is = |line: &str, marker: &str| line.trim() == marker;
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut open: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        match open {
            None if is(line, begin) => open = Some(i),
            None if is(line, end) => {
                return Err(format!(
                    "line {}: `{end}` without a matching `{begin}` before it",
                    i + 1
                ));
            }
            Some(_) if is(line, begin) => {
                return Err(format!("line {}: `{begin}` inside the block", i + 1));
            }
            Some(start) if is(line, end) => {
                blocks.push((start, i));
                open = None;
            }
            _ => {}
        }
    }
    if let Some(start) = open {
        return Err(format!(
            "line {}: `{begin}` is never closed by `{end}`; fix the file by hand",
            start + 1
        ));
    }
    let mut block = String::new();
    for line in std::iter::once(begin)
        .chain(
            body.strip_suffix('\n')
                .unwrap_or(body)
                .split('\n')
                .filter(|_| !body.is_empty()),
        )
        .chain(std::iter::once(end))
    {
        block.push_str(line.strip_suffix('\r').unwrap_or(line));
        block.push_str(eol);
    }
    let mut out = String::with_capacity(text.len() + block.len());
    match blocks.first() {
        None => {
            out.push_str(text);
            if !text.is_empty() && !text.ends_with('\n') {
                out.push_str(eol);
            }
            out.push_str(&block);
        }
        Some(_) => {
            let mut i = 0;
            for (n, (start, stop)) in blocks.iter().enumerate() {
                out.extend(lines[i..*start].iter().copied());
                if n == 0 {
                    out.push_str(&block);
                }
                i = stop + 1;
            }
            out.extend(lines[i..].iter().copied());
        }
    }
    Ok(Spliced {
        text: out,
        found: blocks.len(),
    })
}

/// The lines between the first `begin` / `end` marker pair of `text` (each with its line
/// ending), if there is one.
pub fn block_body(text: &str, begin: &str, end: &str) -> Option<String> {
    let mut body: Option<String> = None;
    for line in text.split_inclusive('\n') {
        match &mut body {
            None if line.trim() == begin => body = Some(String::new()),
            None => {}
            Some(b) if line.trim() == end => return Some(std::mem::take(b)),
            Some(b) => b.push_str(line),
        }
    }
    None
}

/// `text` without any copy of the block (marker lines included); `None` when it has none.
pub fn unsplice(text: &str, begin: &str, end: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    let mut found = false;
    for line in text.split_inclusive('\n') {
        if !inside && line.trim() == begin {
            inside = true;
            found = true;
        } else if inside && line.trim() == end {
            inside = false;
        } else if !inside {
            out.push_str(line);
        }
    }
    (found && !inside).then_some(out)
}

impl BlockAction {
    /// The real file: a symlink is followed, but never into the repository (a linked
    /// `~/.bashrc` is a tracked file, not a foreign one).
    fn target(&self) -> Result<PathBuf> {
        let real = match fs::symlink_metadata(&self.file) {
            Ok(m) if m.file_type().is_symlink() => resolve_lenient(&self.file)
                .with_context(|| format!("{} is a symlink loop", self.file.display()))?,
            _ => self.file.clone(),
        };
        guard(&real, &self.show, &self.home, &self.root).map_err(|e| {
            if real != self.file {
                anyhow::anyhow!("{e:#} (it is a symlink to {})", real.display())
            } else {
                e
            }
        })?;
        Ok(real)
    }

    /// The file's text and mode (`None`: it does not exist) and whether it is owned as it
    /// should be; `Ok(None)` when it is root-owned and cannot be read before sudo is primed.
    #[allow(clippy::type_complexity)]
    fn read(&self, target: &Path) -> Result<Option<(Option<(String, Option<u32>)>, bool)>> {
        let (existing, owned) = match &self.as_root {
            None => (Existing::read(target)?, true),
            Some(r) => match r.read(target)? {
                RootExisting::Read { existing, uid } => (existing, r.owned(uid)),
                RootExisting::Unreadable => return Ok(None),
            },
        };
        match existing {
            Existing::Missing => Ok(Some((None, true))),
            Existing::File { bytes, mode } => {
                let text = String::from_utf8(bytes)
                    .map_err(|_| io::Error::other("not UTF-8 text"))
                    .with_context(|| format!("reading {}", target.display()))?;
                Ok(Some((Some((text, mode)), owned)))
            }
            _ => bail!("{} is not a regular file", target.display()),
        }
    }

    /// The block put into the file as it is now; `None` while it cannot be read (root-owned).
    fn compute(&self, body: &str) -> Result<Option<Computed>> {
        let target = self.target()?;
        let Some((read, owned)) = self.read(&target)? else {
            return Ok(None);
        };
        let (old, mode) = match read {
            Some((t, m)) => (Some(t), m),
            None => (None, None),
        };
        let new = splice(old.as_deref().unwrap_or(""), &self.begin, &self.end, body)
            .map_err(|e| anyhow::anyhow!("{}: {e}", target.display()))?;
        Ok(Some(Computed {
            new,
            old,
            mode,
            owned,
        }))
    }

    /// A file holding secrets that others can read.
    fn exposed(&self, mode: Option<u32>) -> bool {
        !self.secrets.is_empty() && mode.is_some_and(is_loose)
    }

    /// The status before [`unwritable`] is considered.
    fn base_status(&self) -> Result<Status> {
        let Some(body) = &self.body else {
            return Ok(Status::Unknown);
        };
        let Some(c) = self.compute(body)? else {
            return Ok(Status::Unknown);
        };
        Ok(if self.exposed(c.mode) {
            Status::Conflict
        } else if c.old.as_deref() == Some(c.new.text.as_str()) && c.owned {
            Status::Satisfied
        } else {
            Status::Pending
        })
    }

    /// A pending write this user cannot make ([`unwritable`]); a root-owned file is written
    /// through sudo instead.
    fn blocked(&self) -> Option<PathBuf> {
        if self.as_root.is_some() || !matches!(self.base_status(), Ok(Status::Pending)) {
            return None;
        }
        unwritable(&self.target().ok()?)
    }
}

impl Action for BlockAction {
    fn needs_sudo(&self) -> bool {
        self.as_root.as_ref().is_some_and(RootFiles::needs_sudo)
    }

    fn describe(&self) -> String {
        let base = format!("block {} [{}]", self.show, self.begin);
        let Some(body) = &self.body else {
            return with_notes(
                base,
                &[format!(
                    "uses secrets {}; compared when applied",
                    self.secrets.join(", ")
                )],
            );
        };
        let mut notes = Vec::new();
        match self.compute(body) {
            Ok(None) => notes.push(rootfile::unreadable_note()),
            Ok(Some(Computed { new, old: None, .. })) => notes.push(format!(
                "new file, {}",
                crate::fileutil::count(new.text.lines().count(), "line")
            )),
            Ok(Some(Computed {
                new,
                old: Some(old),
                mode,
                owned,
            })) => {
                match new.found {
                    0 => notes.push(format!(
                        "adds {}",
                        crate::fileutil::count(
                            new.text.lines().count() - old.lines().count(),
                            "line"
                        )
                    )),
                    _ if new.text != old => notes.push(format!(
                        "updates block, {}",
                        LineDiff::between(&old, &new.text)
                    )),
                    _ => {}
                }
                if new.found > 1 {
                    notes.push(format!("removes {} duplicate blocks", new.found - 1));
                }
                if !owned {
                    notes.push("owner -> root".into());
                }
                if self.exposed(mode) {
                    notes.push("conflict: the file is readable by group or others".into());
                }
            }
            Err(e) => notes.push(format!("cannot update: {e:#}")),
        }
        if self.as_root.is_some() && !notes.is_empty() {
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

    /// `block:<file> [dot:<bundle>:<marker>]`: the block is taken out again, the rest of the
    /// file left as it is.
    fn resources(&self) -> Vec<Resource> {
        let path = self.target().unwrap_or_else(|_| self.file.clone());
        let created = self
            .created
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        vec![
            Resource::new("block", format!("{} [{}]", self.show, self.id)).with_undo(
                Undo::RemoveBlock {
                    path,
                    begin: self.begin.clone(),
                    end: self.end.clone(),
                    root: self.as_root.is_some(),
                    created_file: created.is_some(),
                    created_dirs: created.unwrap_or_default(),
                },
            ),
        ]
    }

    /// Only the block itself is compared and shown, never the rest of the (foreign) file.
    fn diff(&self) -> Result<Option<Diff>> {
        let Some(body) = &self.body else {
            return Ok(Some(Diff::Note(format!(
                "block {} [{}]: uses secrets {}; content not shown",
                self.show,
                self.id,
                self.secrets.join(", ")
            ))));
        };
        if !self.secrets.is_empty() {
            return Ok(Some(Diff::Note(format!(
                "block {} [{}]: uses secrets; content not shown",
                self.show, self.id
            ))));
        }
        let target = self.target()?;
        let Some((file, _)) = self.read(&target)? else {
            return Ok(Some(Diff::Note(format!(
                "block {} [{}]: {}",
                self.show,
                self.id,
                rootfile::unreadable_note()
            ))));
        };
        let old = file.and_then(|(text, _)| block_body(&text, &self.begin, &self.end));
        let mut new = body.replace("\r\n", "\n");
        if !new.is_empty() && !new.ends_with('\n') {
            new.push('\n');
        }
        let old_text = old.as_deref().unwrap_or("").replace("\r\n", "\n");
        if old.is_some() && old_text == new {
            return Ok(None);
        }
        let label = format!("{} [{}]", self.show, self.id);
        let old_label = if old.is_some() {
            label.clone()
        } else {
            "/dev/null".to_string()
        };
        let mut unified = dot_core::textdiff::unified(&old_text, &new, &old_label, &label, 3);
        if unified.is_empty() {
            // An empty block that is not there yet: only its markers are added.
            unified = format!("{label}: adds the (empty) block\n");
        }
        Ok(Some(Diff::File {
            path: self.show.clone(),
            unified,
        }))
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let Some(body) = &self.body else {
            bail!("secrets for the block were not resolved");
        };
        let Some(Computed {
            new,
            old,
            mode,
            owned,
        }) = self.compute(body)?
        else {
            bail!("cannot read {}, even through sudo", self.show);
        };
        if self.exposed(mode) {
            bail!(
                "the block uses secrets ({}) but {} is readable by group or others (mode \
                 {:04o}); make it private (chmod 600) or keep secrets out of the block",
                self.secrets.join(", "),
                self.show,
                mode.unwrap_or_default()
            );
        }
        if old.as_deref() == Some(new.text.as_str()) && owned {
            return Ok(Outcome::Ok);
        }
        let mode = mode.unwrap_or(if self.secrets.is_empty() {
            DEFAULT_FILE_MODE
        } else {
            SECRET_FILE_MODE
        });
        let target = self.target()?;
        let dirs = old.is_none().then(|| missing_dirs(&target));
        match &self.as_root {
            // The real file (a symlink is followed): installed over it, whole.
            Some(r) => r.write(&target, new.text.as_bytes(), mode, ctx)?,
            None => atomic_write(&target, new.text.as_bytes(), mode)?,
        }
        if dirs.is_some() {
            *self.created.lock().unwrap_or_else(|e| e.into_inner()) = dirs;
        }
        Ok(Outcome::Changed)
    }
}

#[cfg(test)]
mod splice_tests {
    use super::*;

    const B: &str = "# >>> dot:x >>>";
    const E: &str = "# <<< dot:x <<<";

    fn sp(text: &str, body: &str) -> String {
        splice(text, B, E, body).unwrap().text
    }

    #[test]
    fn a_block_is_found_and_taken_out_again() {
        let text = format!("top\n{B}\na\nb\n{E}\nbottom\n");
        assert_eq!(block_body(&text, B, E).as_deref(), Some("a\nb\n"));
        assert_eq!(unsplice(&text, B, E).as_deref(), Some("top\nbottom\n"));
        assert_eq!(block_body("top\n", B, E), None);
        assert_eq!(unsplice("top\n", B, E), None);
        // Every copy goes; an unclosed block is left for the user.
        let two = format!("{B}\nx\n{E}\nmid\n  {B}\ny\n{E}\n");
        assert_eq!(unsplice(&two, B, E).as_deref(), Some("mid\n"));
        assert_eq!(unsplice(&format!("{B}\nx\n"), B, E), None);
        // Round trip: splice then unsplice gives the original back.
        let orig = "keep\n";
        let spliced = sp(orig, "z\n");
        assert_eq!(unsplice(&spliced, B, E).as_deref(), Some(orig));
    }

    #[test]
    fn appends_replaces_and_dedupes() {
        let block = format!("{B}\na\nb\n{E}\n");
        assert_eq!(sp("", "a\nb\n"), block);
        assert_eq!(sp("top", "a\nb"), format!("top\n{block}"));
        // Idempotent.
        let once = sp("top\n", "a\nb\n");
        assert_eq!(sp(&once, "a\nb\n"), once);
        // Replaced in place, surroundings kept.
        let text = format!("1\n{B}\nold\n{E}\n2\n{B}\ndup\n{E}\n3\n");
        let s = splice(&text, B, E, "new\n").unwrap();
        assert_eq!(s.found, 2);
        assert_eq!(s.text, format!("1\n{B}\nnew\n{E}\n2\n3\n"));
        // Empty content: just the markers.
        assert_eq!(sp("", ""), format!("{B}\n{E}\n"));
        // Indented markers still match.
        assert_eq!(
            sp(&format!("  {B}\nold\n  {E}\n"), "n"),
            format!("{B}\nn\n{E}\n")
        );
    }

    #[test]
    fn keeps_crlf_files_crlf() {
        let text = format!("a\r\n{B}\r\nold\r\n{E}\r\nz\r\n");
        assert_eq!(
            sp(&text, "n1\nn2\n"),
            format!("a\r\n{B}\r\nn1\r\nn2\r\n{E}\r\nz\r\n")
        );
    }

    #[test]
    fn broken_markers_are_errors() {
        let e = splice(&format!("a\n{B}\nx\n"), B, E, "").unwrap_err();
        assert!(e.contains("line 2") && e.contains("never closed"), "{e}");
        let e = splice(&format!("{E}\n"), B, E, "").unwrap_err();
        assert!(e.contains("without a matching"), "{e}");
        let e = splice(&format!("{B}\n{B}\n{E}\n"), B, E, "").unwrap_err();
        assert!(e.contains("inside the block"), "{e}");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::testutil::Fixture;
    use crate::testutil_sys::plan_applying;

    #[test]
    fn creates_updates_and_converges() {
        let f = Fixture::new(
            "vars:\n  dir: /opt/fnm\nsteps:\n  - block:\n      file: ~/.bashrc\n      marker: fnm\n      content: |\n        export FNM_DIR={{ dir }}\n        eval \"$(fnm env)\"\n",
        );
        let mut ctx = f.context();
        ctx.vars = ctx.vars.with_bundle(&f.bundle);
        let rc = f.home().join(".bashrc");
        let actions = f.plan_with(0, &ctx).unwrap();
        assert_eq!(
            actions[0].describe(),
            "block ~/.bashrc [# >>> dot:demo:fnm >>>] (new file, 4 lines)"
        );
        f.apply(&actions);
        let expected = "# >>> dot:demo:fnm >>>\nexport FNM_DIR=/opt/fnm\neval \"$(fnm env)\"\n# <<< dot:demo:fnm <<<\n";
        assert_eq!(fs::read_to_string(&rc).unwrap(), expected);
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);

        // Someone edits the file around and inside the block.
        fs::write(
            &rc,
            format!(
                "alias ll='ls -l'\n{}\nmine\n",
                expected.replace("/opt/fnm", "/old")
            ),
        )
        .unwrap();
        fs::set_permissions(&rc, fs::Permissions::from_mode(0o640)).unwrap();
        let actions = f.plan_with(0, &ctx).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert!(
            actions[0]
                .describe()
                .ends_with("(updates block, +1/-1 lines)"),
            "{}",
            actions[0].describe()
        );
        f.apply(&actions);
        assert_eq!(
            fs::read_to_string(&rc).unwrap(),
            format!("alias ll='ls -l'\n{expected}\nmine\n")
        );
        assert_eq!(
            fs::metadata(&rc).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(f.apply(&actions), [Outcome::Ok]);
    }

    #[test]
    fn custom_comment_and_appending() {
        let f = Fixture::new(
            "steps:\n  - block: { file: ~/.vimrc, content: set number, comment: '\"' }\n",
        );
        fs::write(f.home().join(".vimrc"), "syntax on").unwrap();
        let actions = f.plan(0).unwrap();
        assert!(
            actions[0].describe().ends_with("(adds 3 lines)"),
            "{}",
            actions[0].describe()
        );
        f.apply(&actions);
        assert_eq!(
            fs::read_to_string(f.home().join(".vimrc")).unwrap(),
            "syntax on\n\" >>> dot:demo >>>\nset number\n\" <<< dot:demo <<<\n"
        );
    }

    #[test]
    fn writes_through_a_link_but_not_into_the_repository() {
        let f = Fixture::new("steps:\n  - block: { file: ~/.profile, content: x }\n");
        let real = f.home().join("real-profile");
        fs::write(&real, "keep\n").unwrap();
        std::os::unix::fs::symlink(&real, f.home().join(".profile")).unwrap();
        f.apply(&f.plan(0).unwrap());
        assert!(
            fs::symlink_metadata(f.home().join(".profile"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&real).unwrap(),
            "keep\n# >>> dot:demo >>>\nx\n# <<< dot:demo <<<\n"
        );

        let g = Fixture::new("steps:\n  - block: { file: ~/.zshrc, content: x }\n");
        g.write_repo("zshrc", "tracked\n");
        std::os::unix::fs::symlink(g.repo().join("zshrc"), g.home().join(".zshrc")).unwrap();
        let err = format!("{:#}", g.plan(0).err().unwrap());
        assert!(
            err.contains("inside the repository") && err.contains("symlink to"),
            "{err}"
        );
        assert_eq!(
            fs::read_to_string(g.repo().join("zshrc")).unwrap(),
            "tracked\n"
        );
    }

    #[test]
    fn an_unterminated_block_is_reported_not_guessed_at() {
        let f = Fixture::new("steps:\n  - block: { file: ~/.rc, content: x }\n");
        fs::write(f.home().join(".rc"), "# >>> dot:demo >>>\nhalf\n").unwrap();
        let actions = f.plan(0).unwrap();
        let err = format!("{:#}", actions[0].status().unwrap_err());
        assert!(err.contains("never closed"), "{err}");
        assert!(f.apply_one(actions[0].as_ref()).is_err());
        assert_eq!(
            fs::read_to_string(f.home().join(".rc")).unwrap(),
            "# >>> dot:demo >>>\nhalf\n"
        );
    }

    #[test]
    fn secrets_need_a_private_file() {
        let f = Fixture::new(
            "steps:\n  - block: { file: ~/.netrc, content: \"password {{ secret('pw') }}\" }\n",
        );
        let planned = f.plan(0).unwrap();
        assert_eq!(planned[0].status().unwrap(), Status::Unknown);
        assert!(planned[0].describe().contains("uses secrets pw"));
        // New file: created 0600.
        let actions = plan_applying(&f, 0, &[("pw", "hunter2")]);
        assert!(!actions[0].describe().contains("hunter2"));
        f.apply(&actions);
        let netrc = f.home().join(".netrc");
        assert_eq!(
            fs::metadata(&netrc).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Readable by others: refused.
        fs::set_permissions(&netrc, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Conflict);
        let err = f.apply_err(actions[0].as_ref());
        assert!(err.contains("readable by group or others"), "{err}");
        assert!(!err.contains("hunter2"));
    }
}
