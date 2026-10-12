//! `template`: render a file from a template in the bundle, with the same variables (and
//! `secret()`) as every other string in the config, and write it atomically.
//!
//! The plan shows a line-diff summary, never content. While planning, `secret()` renders a
//! placeholder, so a template that uses one is compared only when applied ([`Status::Unknown`]
//! until then). A file holding a secret gets mode 0600 unless the config asks for something
//! stricter; a mode readable by group or others is refused.
//!
//! `root: true` makes a root-owned file (see [`crate::rootfile`]).

use std::fs;
use std::io;
use std::path::PathBuf;

use anyhow::{Result, bail};
use dot_core::config::FileEntry;
use dot_core::{Action, Diff, Outcome, PlanCtx, Resource, RunCtx, Status, Undo};

use crate::fileutil::{
    DEFAULT_FILE_MODE, Existing, SECRET_FILE_MODE, atomic_write, change_note, expand_dest,
    file_diff, guard, is_dest_pattern, is_loose, mode_ok, new_file_note, unwritable,
    unwritable_note, with_notes, written, written_bytes,
};
use crate::rootfile::{self, RootFiles};

pub fn plan(
    entries: &[FileEntry],
    ctx: &PlanCtx<'_>,
    root_files: &RootFiles,
) -> Result<Vec<Box<dyn Action>>> {
    let mut actions: Vec<Box<dyn Action>> = Vec::new();
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
        let show_src = ctx.show_source(&src);
        let explicit = e.mode.as_ref().map(|m| ctx.mode(m)).transpose()?;
        let content = match fs::read(&src) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes).map_err(|_| {
                    ctx.config_error(format!("template source `{show_src}` is not UTF-8 text"))
                })?;
                let r = ctx.render_text(&text, &format!("template `{show_src}`"))?;
                Some(Rendered {
                    text: r.text,
                    secrets: r.secrets,
                    placeholder: r.placeholder,
                })
            }
            // An earlier step may still create it.
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(
                    ctx.config_error(format!("cannot read template source `{show_src}`: {e}"))
                );
            }
        };
        let uses_secrets = content.as_ref().is_some_and(|c| !c.secrets.is_empty());
        let mode = match explicit {
            Some(m) if uses_secrets && is_loose(m.0) => {
                return Err(ctx.config_error(format!(
                    "template `{show_src}` uses secrets, so `{}` must not be readable by group \
                     or others: use mode \"0600\" or stricter (not \"{m}\")",
                    e.dest
                )));
            }
            Some(m) => Some(m.0),
            None if uses_secrets => Some(SECRET_FILE_MODE),
            None => None,
        };
        // A pattern (`~/.config/JetBrains/*/options/x.xml`): every place it matches now.
        let dests = if is_dest_pattern(&pattern) {
            expand_dest(&pattern)
        } else {
            vec![pattern]
        };
        for dest in dests {
            let action = TemplateAction {
                show_dest: ctx.show(&dest),
                show_src: show_src.clone(),
                dest,
                content: content.clone(),
                mode,
                home: ctx.home.to_path_buf(),
                root: ctx.bundle.root.clone(),
                as_root: e.root.then(|| root_files.clone()),
            };
            guard(&action.dest, &action.show_dest, &action.home, &action.root)?;
            actions.push(Box::new(action));
        }
    }
    Ok(actions)
}

#[derive(Clone)]
struct Rendered {
    text: String,
    secrets: Vec<String>,
    /// Planning: secrets are placeholders, so the text cannot be compared or written.
    placeholder: bool,
}

pub struct TemplateAction {
    pub dest: PathBuf,
    /// `None`: the source does not exist (yet).
    content: Option<Rendered>,
    /// `None`: keep an existing file's mode, 0644 for a new one.
    pub mode: Option<u32>,
    home: PathBuf,
    root: PathBuf,
    show_dest: String,
    show_src: String,
    /// `root: true`: a root-owned file, written through sudo.
    as_root: Option<RootFiles>,
}

/// What is at the destination: the file and its owner, or `None` when it cannot be read yet
/// (root-owned and not readable before sudo is primed).
type Inspected = Option<(Existing, Option<u32>)>;

impl TemplateAction {
    fn inspect(&self) -> Result<Inspected> {
        match &self.as_root {
            None => Ok(Some((Existing::read(&self.dest)?, None))),
            Some(r) => Ok(match r.read(&self.dest)? {
                rootfile::RootExisting::Read { existing, uid } => Some((existing, uid)),
                rootfile::RootExisting::Unreadable => None,
            }),
        }
    }

    fn satisfied(&self, text: &str, existing: &Existing, uid: Option<u32>) -> bool {
        existing.bytes() == Some(text.as_bytes())
            && mode_ok(existing.mode(), self.mode)
            && self.as_root.as_ref().is_none_or(|r| r.owned(uid))
    }

    /// The status before [`unwritable`] is considered.
    fn base_status(&self) -> Result<Status> {
        let Some(content) = self.content.as_ref().filter(|c| !c.placeholder) else {
            return Ok(Status::Unknown);
        };
        let Some((existing, uid)) = self.inspect()? else {
            return Ok(Status::Unknown);
        };
        Ok(if self.satisfied(&content.text, &existing, uid) {
            Status::Satisfied
        } else if matches!(existing, Existing::Dir | Existing::Other) {
            Status::Conflict
        } else {
            Status::Pending
        })
    }

    /// A pending write this user cannot make ([`unwritable`]); root-owned files are written
    /// through sudo instead.
    fn blocked(&self) -> Option<PathBuf> {
        if self.as_root.is_some() || !matches!(self.base_status(), Ok(Status::Pending)) {
            return None;
        }
        unwritable(&self.dest)
    }
}

impl Action for TemplateAction {
    fn needs_sudo(&self) -> bool {
        self.as_root.as_ref().is_some_and(RootFiles::needs_sudo)
    }

    fn describe(&self) -> String {
        let base = format!("template {} <- {}", self.show_dest, self.show_src);
        let mut notes = Vec::new();
        let content = match &self.content {
            None => {
                notes.push("source does not exist yet; an earlier step may create it".into());
                return with_notes(base, &notes);
            }
            Some(c) if c.placeholder => {
                notes.push(format!(
                    "uses secrets {}; compared when applied",
                    c.secrets.join(", ")
                ));
                return with_notes(base, &notes);
            }
            Some(c) => c,
        };
        let new = content.text.as_bytes();
        match self.inspect() {
            Ok(None) => notes.push(rootfile::unreadable_note()),
            Ok(Some((existing, uid))) => {
                match existing {
                    Existing::Missing => notes.push(new_file_note(new)),
                    Existing::File { bytes, mode } => {
                        if bytes != new {
                            notes.push(change_note(&bytes, new));
                        }
                        if let (Some(have), Some(want)) = (mode, self.mode)
                            && have != want
                        {
                            notes.push(format!("mode {have:04o} -> {want:04o}"));
                        }
                    }
                    Existing::Symlink(t) => notes.push(format!("replaces link to {}", t.display())),
                    Existing::Dir => notes.push("conflict: a directory is in the way".into()),
                    Existing::Other => notes.push("conflict: not a regular file".into()),
                }
                if let Some(r) = &self.as_root
                    && !r.owned(uid)
                {
                    notes.push(rootfile::owner_note(uid));
                }
            }
            Err(e) => notes.push(format!("cannot inspect: {e:#}")),
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

    /// `file:<dest>`: removed while it holds what `dot` wrote; a file that was there before
    /// is saved first and put back.
    fn resources(&self) -> Vec<Resource> {
        // A root-owned file may not be readable by this user: what `dot` writes is hashed.
        let file = match &self.content {
            Some(c) if self.as_root.is_some() && !c.placeholder => {
                written_bytes(&self.dest, c.text.as_bytes())
            }
            _ => written(&self.dest),
        };
        let r = Resource::new("file", self.show_dest.clone()).with_undo(Undo::RemoveFiles {
            files: vec![file],
            root: self.as_root.is_some(),
        });
        // A root-owned file is saved by `apply`, which may read it through sudo.
        vec![if self.as_root.is_some() {
            r
        } else {
            r.preserving(&self.dest)
        }]
    }

    /// Content rendered with `secret()` is never compared or shown.
    fn diff(&self) -> Result<Option<Diff>> {
        let content = match &self.content {
            None => {
                return Ok(Some(Diff::Note(format!(
                    "source {} does not exist yet",
                    self.show_src
                ))));
            }
            Some(c) if c.placeholder || !c.secrets.is_empty() => {
                return Ok(Some(Diff::Note(format!(
                    "{}: uses secrets {}; content not shown",
                    self.show_dest,
                    c.secrets.join(", ")
                ))));
            }
            Some(c) => c,
        };
        let Some((existing, _)) = self.inspect()? else {
            return Ok(Some(Diff::Note(format!(
                "{}: {}",
                self.show_dest,
                rootfile::unreadable_note()
            ))));
        };
        Ok(file_diff(
            &self.show_dest,
            &existing,
            content.text.as_bytes(),
            &format!("{} (rendered from {})", self.show_dest, self.show_src),
            self.mode,
        ))
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        guard(&self.dest, &self.show_dest, &self.home, &self.root)?;
        let content = match &self.content {
            None => bail!("template source not found: `{}`", self.show_src),
            Some(c) if c.placeholder => bail!("secrets for `{}` were not resolved", self.show_src),
            Some(c) => c,
        };
        let Some((existing, uid)) = self.inspect()? else {
            bail!("cannot read {}, even through sudo", self.dest.display());
        };
        if self.satisfied(&content.text, &existing, uid) {
            return Ok(Outcome::Ok);
        }
        let mode = match &existing {
            Existing::Dir | Existing::Other => {
                bail!("{} exists and is not a regular file", self.dest.display())
            }
            Existing::File { mode: Some(m), .. } => self.mode.unwrap_or(*m),
            _ => self.mode.unwrap_or(DEFAULT_FILE_MODE),
        };
        match &self.as_root {
            Some(r) => {
                if let Existing::File { bytes, mode: have } = &existing {
                    let key = format!("file:{}", self.show_dest);
                    ctx.preserve_root(&key, &self.dest, bytes, have.unwrap_or(mode))?;
                }
                r.remove_link(&self.dest, ctx)?;
                r.write(&self.dest, content.text.as_bytes(), mode, ctx)?;
            }
            None => atomic_write(&self.dest, content.text.as_bytes(), mode)?,
        }
        Ok(Outcome::Changed)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::testutil::Fixture;
    use crate::testutil_sys::plan_applying;

    fn mode_of(p: &std::path::Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn renders_writes_and_converges() {
        let f = Fixture::new(
            "vars:\n  name: world\nsteps:\n  - template:\n      ~/.config/app/conf: conf.j2\n",
        );
        f.write_repo("conf.j2", "hello {{ name }}\nos={{ os }}\n");
        let mut ctx = f.context();
        ctx.vars = ctx.vars.with_bundle(&f.bundle);
        let actions = f.plan_with(0, &ctx).unwrap();
        assert_eq!(
            actions[0].describe(),
            "template ~/.config/app/conf <- conf.j2 (new file, 2 lines)"
        );
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        let dest = f.home().join(".config/app/conf");
        assert_eq!(
            fs::read_to_string(&dest).unwrap(),
            "hello world\nos=linux\n"
        );
        assert_eq!(mode_of(&dest), 0o644);
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
        assert_eq!(f.apply(&actions), [Outcome::Ok]);

        // An edit shows as a diff summary and is put back; the file's own mode is kept.
        fs::write(&dest, "hello there\nos=linux\nextra\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o640)).unwrap();
        let actions = f.plan_with(0, &ctx).unwrap();
        assert_eq!(
            actions[0].describe(),
            "template ~/.config/app/conf <- conf.j2 (+1/-2 lines)"
        );
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        assert_eq!(
            fs::read_to_string(&dest).unwrap(),
            "hello world\nos=linux\n"
        );
        assert_eq!(mode_of(&dest), 0o640);
    }

    #[test]
    fn explicit_mode_is_enforced() {
        let f = Fixture::new("steps:\n  - template:\n      ~/x: { src: x.j2, mode: \"0640\" }\n");
        f.write_repo("x.j2", "x\n");
        let dest = f.home().join("x");
        fs::write(&dest, "x\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o644)).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert!(actions[0].describe().contains("mode 0644 -> 0640"));
        f.apply(&actions);
        assert_eq!(mode_of(&dest), 0o640);
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    }

    #[test]
    fn undefined_variables_are_located_errors() {
        let f = Fixture::new("steps:\n  - template:\n      ~/x: x.j2\n");
        f.write_repo("x.j2", "line one\n{{ nope }}\n");
        let err = format!("{:#}", f.plan(0).err().unwrap());
        assert!(
            err.contains("bundle.yml:2:5: steps[0]: template `x.j2`"),
            "{err}"
        );
        assert!(err.contains("undefined variable `nope`"), "{err}");
        assert!(err.contains("template line 2"), "{err}");
    }

    #[test]
    fn missing_source_is_unknown_until_applied() {
        let f = Fixture::new("steps:\n  - template:\n      ~/x: later.j2\n");
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Unknown);
        assert!(actions[0].describe().contains("does not exist yet"));
        let err = f.apply_err(actions[0].as_ref());
        assert!(err.contains("template source not found"), "{err}");
    }

    #[test]
    fn secrets_are_compared_when_applied_and_protected() {
        const S: &str = "tok-123-secret";
        let f = Fixture::new("steps:\n  - template:\n      ~/.config/glab/env: glab.env.j2\n");
        f.write_repo(
            "glab.env.j2",
            "GITLAB_TOKEN={{ secret('gitlab') }}\nHOST=x\n",
        );
        let dest = f.home().join(".config/glab/env");
        // Planning: placeholder, unknown, nothing shown.
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Unknown);
        assert_eq!(
            actions[0].describe(),
            "template ~/.config/glab/env <- glab.env.j2 (uses secrets gitlab; compared when applied)"
        );
        // Applying: the value is written, 0600, and never appears in the description.
        let actions = plan_applying(&f, 0, &[("gitlab", S)]);
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        let d = actions[0].describe();
        assert!(!d.contains(S), "{d}");
        assert_eq!(
            d,
            "template ~/.config/glab/env <- glab.env.j2 (new file, 2 lines)"
        );
        f.apply(&actions);
        assert_eq!(
            fs::read_to_string(&dest).unwrap(),
            format!("GITLAB_TOKEN={S}\nHOST=x\n")
        );
        assert_eq!(mode_of(&dest), 0o600);
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
        // A file that became world-readable is fixed.
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        f.apply(&actions);
        assert_eq!(mode_of(&dest), 0o600);
        // A changed secret shows only as a diff count.
        let actions = plan_applying(&f, 0, &[("gitlab", "rotated")]);
        let d = actions[0].describe();
        assert!(
            d.ends_with("(+1/-1 lines)") && !d.contains(S) && !d.contains("rotated"),
            "{d}"
        );
    }

    #[test]
    fn a_loose_mode_with_secrets_is_refused() {
        let f = Fixture::new(
            "steps:\n  - template:\n      ~/x: { src: x.j2, mode: \"0644\" }\n  - template:\n      ~/y: { src: x.j2, mode: \"0400\" }\n",
        );
        f.write_repo("x.j2", "{{ secret('tok') }}\n");
        let err = format!("{:#}", f.plan(0).err().unwrap());
        assert!(err.contains("bundle.yml:2:5: steps[0]:"), "{err}");
        assert!(
            err.contains("must not be readable by group or others"),
            "{err}"
        );
        assert!(f.plan(1).is_ok());
    }

    #[test]
    fn refuses_to_write_into_the_repository() {
        let f = Fixture::new("steps:\n  - template:\n      ~/.config/nvim/init.lua: x.j2\n");
        f.write_repo("x.j2", "x");
        f.write_repo("nvim/init.lua", "tracked");
        fs::create_dir_all(f.home().join(".config")).unwrap();
        std::os::unix::fs::symlink(f.repo().join("nvim"), f.home().join(".config/nvim")).unwrap();
        let err = format!("{:#}", f.plan(0).err().unwrap());
        assert!(err.contains("inside the repository"), "{err}");
        assert_eq!(
            fs::read_to_string(f.repo().join("nvim/init.lua")).unwrap(),
            "tracked"
        );
    }

    #[test]
    fn replaces_a_link_without_touching_its_target() {
        let f = Fixture::new("steps:\n  - template:\n      ~/.rc: rc.j2\n");
        f.write_repo("rc.j2", "new");
        let other = f.home().join("other");
        fs::write(&other, "other").unwrap();
        std::os::unix::fs::symlink(&other, f.home().join(".rc")).unwrap();
        let actions = f.plan(0).unwrap();
        assert!(actions[0].describe().contains("replaces link to"));
        f.apply(&actions);
        assert_eq!(fs::read_to_string(&other).unwrap(), "other");
        assert_eq!(fs::read_to_string(f.home().join(".rc")).unwrap(), "new");
    }

    #[test]
    fn a_root_owned_destination_is_a_conflict_not_a_half_run() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root may write anywhere
        }
        let f = Fixture::new("steps:\n  - template:\n      ~/etc/sysctl.d/idea.conf: idea.j2\n");
        f.write_repo("idea.j2", "vm.max_map_count = 1\n");
        let etc = f.home().join("etc");
        fs::create_dir(&etc).unwrap();
        fs::set_permissions(&etc, fs::Permissions::from_mode(0o555)).unwrap();
        let actions = f.plan(0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Conflict);
        assert!(
            actions[0]
                .describe()
                .contains("is not writable by you; mark the entry `root: true`"),
            "{}",
            actions[0].describe()
        );
        fs::set_permissions(&etc, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
    }
}
