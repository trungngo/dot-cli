//! `secret_file`: write a secret (any bytes, e.g. a VPN profile or a key) to a file, mode 0600
//! unless the config says otherwise. The content is never shown, not even as a diff: the plan
//! says only whether the file is missing, differs or has the wrong mode. While planning nothing
//! is resolved, so the status is [`Status::Unknown`] until the step is applied.
//!
//! On Windows, which has no modes, a private mode (the default) means a private ACL: the file is
//! written with its own protected ACL for the user, `SYSTEM` and `Administrators`
//! ([`crate::fileutil::atomic_write`]), and one that others can read (it inherited `C:\tools`'s
//! `Users: read`, say) is rewritten.

use std::path::PathBuf;

use anyhow::{Result, bail};
use dot_core::config::SecretFileEntry;
use dot_core::{Action, Outcome, PlanCtx, Resource, RunCtx, SecretBytes, Status, Undo, Written};

use crate::fileutil::{
    Existing, SECRET_FILE_MODE, atomic_write, guard, is_loose, mode_label, unwritable,
    unwritable_note, with_notes,
};

pub fn plan(entries: &[SecretFileEntry], ctx: &PlanCtx<'_>) -> Result<Vec<Box<dyn Action>>> {
    let mut actions: Vec<Box<dyn Action>> = Vec::new();
    for e in entries {
        let dest = ctx.target(&e.dest)?;
        let name = ctx.render(&e.secret)?;
        let mode = match e.mode.as_ref().map(|m| ctx.mode(m)).transpose()? {
            Some(m) if is_loose(m.0) && !e.allow_loose_mode => {
                return Err(ctx.config_error(format!(
                    "secret_file `{}`: mode \"{m}\" lets group or others read secret `{name}`; \
                     use \"0600\" or stricter, or set `allow_loose_mode: true`",
                    e.dest
                )));
            }
            Some(m) => m.0,
            None => SECRET_FILE_MODE,
        };
        let value = ctx.secret_file(&name)?;
        let action = SecretFileAction {
            show: ctx.show(&dest),
            dest,
            name,
            mode,
            value,
            home: ctx.home.to_path_buf(),
            root: ctx.bundle.root.clone(),
        };
        guard(&action.dest, &action.show, &action.home, &action.root)?;
        actions.push(Box::new(action));
    }
    Ok(actions)
}

pub struct SecretFileAction {
    pub dest: PathBuf,
    /// The logical secret name.
    pub name: String,
    pub mode: u32,
    /// `None` while planning.
    value: Option<SecretBytes>,
    home: PathBuf,
    root: PathBuf,
    show: String,
}

/// How the file compares with the secret. Only facts, never content.
#[derive(Debug, PartialEq, Eq)]
enum State {
    Missing,
    Same,
    ContentDiffers,
    ModeDiffers(u32),
    /// Windows: the ACL lets these read or change it, and the mode says private.
    Exposed(Vec<String>),
    Link,
    NotAFile,
}

impl SecretFileAction {
    fn state(&self, value: &SecretBytes) -> std::io::Result<State> {
        let mut existing = Existing::read(&self.dest)?;
        let state = match &existing {
            Existing::Missing => State::Missing,
            Existing::Symlink(_) => State::Link,
            Existing::Dir | Existing::Other => State::NotAFile,
            Existing::File { bytes, mode } => {
                if bytes.as_slice() != value.as_bytes() {
                    State::ContentDiffers
                } else {
                    match mode {
                        Some(m) if *m != self.mode => State::ModeDiffers(*m),
                        _ => match self.exposed() {
                            Some(who) => State::Exposed(who),
                            None => State::Same,
                        },
                    }
                }
            }
        };
        // What was read may itself be (an older copy of) the secret.
        existing.wipe();
        Ok(state)
    }

    /// Windows, for a private mode: who else the file's ACL lets in, when anyone (an ACL that
    /// cannot be read counts as exposing it). `None` elsewhere.
    fn exposed(&self) -> Option<Vec<String>> {
        #[cfg(windows)]
        if !is_loose(self.mode) {
            return match dot_secrets::private_file::exposed(&self.dest) {
                Ok(who) if who.is_empty() => None,
                Ok(who) => Some(who),
                Err(e) => Some(vec![e]),
            };
        }
        None
    }

    /// The status before [`unwritable`] is considered.
    fn base_status(&self) -> Result<Status> {
        let Some(value) = &self.value else {
            return Ok(Status::Unknown);
        };
        Ok(match self.state(value)? {
            State::Same => Status::Satisfied,
            State::NotAFile => Status::Conflict,
            _ => Status::Pending,
        })
    }

    /// A pending write this user cannot make ([`unwritable`]).
    fn blocked(&self) -> Option<PathBuf> {
        if !matches!(self.base_status(), Ok(Status::Pending)) {
            return None;
        }
        unwritable(&self.dest)
    }
}

impl Action for SecretFileAction {
    fn describe(&self) -> String {
        let base = format!(
            "secret_file {} <- secret `{}` ({})",
            self.show,
            self.name,
            mode_label(self.mode, cfg!(windows))
        );
        let Some(value) = &self.value else {
            return with_notes(base, &["resolved when applied".into()]);
        };
        let note = match self.state(value) {
            Ok(State::Missing) => Some("new file".to_string()),
            Ok(State::Same) => None,
            Ok(State::ContentDiffers) => Some("content differs".into()),
            Ok(State::ModeDiffers(m)) => Some(format!("mode {m:04o} -> {:04o}", self.mode)),
            Ok(State::Exposed(who)) => Some(format!(
                "{} can read or change it: rewritten private",
                who.join(", ")
            )),
            Ok(State::Link) => Some("replaces a link".into()),
            Ok(State::NotAFile) => Some("conflict: not a regular file".into()),
            Err(e) => Some(format!("cannot inspect: {e}")),
        };
        let mut notes: Vec<String> = note.into_iter().collect();
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

    /// `file:<dest>`. No content hash is kept (not even of a secret) and nothing is saved
    /// before it is written: `dot remove` removes the file while it is a regular file.
    fn resources(&self) -> Vec<Resource> {
        vec![
            Resource::new("file", self.show.clone()).with_undo(Undo::RemoveFiles {
                files: vec![Written {
                    path: self.dest.clone(),
                    sha256: None,
                }],
                root: false,
            }),
        ]
    }

    fn apply(&self, _ctx: &RunCtx) -> Result<Outcome> {
        guard(&self.dest, &self.show, &self.home, &self.root)?;
        let Some(value) = &self.value else {
            bail!("secret `{}` was not resolved", self.name);
        };
        match self.state(value)? {
            State::Same => return Ok(Outcome::Ok),
            State::NotAFile => bail!("{} exists and is not a regular file", self.dest.display()),
            _ => {}
        }
        atomic_write(&self.dest, value.as_bytes(), self.mode)?;
        Ok(Outcome::Changed)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use dot_core::SecretUse;

    use super::*;
    use crate::testutil::Fixture;
    use crate::testutil_sys::plan_applying;

    fn mode_of(p: &std::path::Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn plans_without_resolving_then_writes_0600() {
        const S: &str = "client\nremote vpn.example 1194\n<key>secret-bits</key>\n";
        let f = Fixture::new("steps:\n  - secret_file:\n      ~/.vpn/work.ovpn: vpn_profile\n");
        let ctx = f.context();
        let refs = dot_core::SecretRefs::default();
        let pctx = PlanCtx::new(&ctx, &f.bundle, &f.bundle.steps[0], &refs);
        let dot_core::Directive::SecretFile(entries) = &f.bundle.steps[0].directive else {
            panic!()
        };
        let actions = plan(entries, &pctx).unwrap();
        assert_eq!(refs.uses()["vpn_profile"], SecretUse::FILE);
        assert_eq!(actions[0].status().unwrap(), Status::Unknown);
        assert_eq!(
            actions[0].describe(),
            "secret_file ~/.vpn/work.ovpn <- secret `vpn_profile` (mode 0600) (resolved when applied)"
        );

        let actions = plan_applying(&f, 0, &[("vpn_profile", S)]);
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert!(actions[0].describe().ends_with("(new file)"));
        f.apply(&actions);
        let dest = f.home().join(".vpn/work.ovpn");
        assert_eq!(fs::read_to_string(&dest).unwrap(), S);
        assert_eq!(mode_of(&dest), 0o600);
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);

        fs::write(&dest, "old").unwrap();
        let d = actions[0].describe();
        assert!(
            d.ends_with("(content differs)") && !d.contains("secret-bits"),
            "{d}"
        );
        f.apply(&actions);
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(actions[0].describe().ends_with("(mode 0644 -> 0600)"));
        f.apply(&actions);
        assert_eq!(mode_of(&dest), 0o600);
    }

    #[test]
    fn loose_modes_need_explicit_consent() {
        let f = Fixture::new(concat!(
            "steps:\n",
            "  - secret_file:\n      ~/a: { secret: s, mode: \"0644\" }\n",
            "  - secret_file:\n      ~/b: { secret: s, mode: \"0640\", allow_loose_mode: true }\n",
            "  - secret_file:\n      ~/c: { secret: s, mode: \"0400\" }\n",
        ));
        let err = format!("{:#}", f.plan(0).err().unwrap());
        assert!(err.contains("bundle.yml:2:5: steps[0]:"), "{err}");
        assert!(
            err.contains("lets group or others read secret `s`"),
            "{err}"
        );
        let actions = plan_applying(&f, 1, &[("s", "v")]);
        f.apply(&actions);
        assert_eq!(mode_of(&f.home().join("b")), 0o640);
        let actions = plan_applying(&f, 2, &[("s", "v")]);
        f.apply(&actions);
        assert_eq!(mode_of(&f.home().join("c")), 0o400);
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    }

    #[test]
    fn binary_secrets_are_written_as_is() {
        let f = Fixture::new("steps:\n  - secret_file:\n      ~/key.bin: key\n");
        let cache = std::sync::Arc::new(dot_core::SecretCache::default());
        cache.insert("key", vec![0u8, 0xff, 0x10]);
        let mut ctx = f.context();
        ctx.secrets = Some(cache);
        let env = dot_core::BundleEnv::new(&ctx, &f.bundle);
        let refs = dot_core::SecretRefs::default();
        let s = &f.bundle.steps[0];
        let pctx = PlanCtx::for_bundle(&ctx, &env, &f.bundle, s, &refs, true);
        let dot_core::Directive::SecretFile(entries) = &s.directive else {
            panic!()
        };
        let actions = plan(entries, &pctx).unwrap();
        f.apply(&actions);
        assert_eq!(
            fs::read(f.home().join("key.bin")).unwrap(),
            [0u8, 0xff, 0x10]
        );
    }

    #[test]
    fn unresolved_at_apply_time_fails_clearly() {
        let f = Fixture::new("steps:\n  - secret_file:\n      ~/x: missing\n");
        let actions = f.plan(0).unwrap();
        let err = f.apply_err(actions[0].as_ref());
        assert!(err.contains("secret `missing` was not resolved"), "{err}");
    }
}
