//! What an action manages ([`Resource`]), how `dot remove` undoes it ([`Undo`]) and how the
//! machine differs from the config ([`Diff`]).
//!
//! A resource has a stable identity, `kind:id` (`link:~/.zshrc`, `pkg:apt:fd-find`,
//! `file:/etc/apt/sources.list.d/docker.sources`): the engine records it in the state store
//! ([`crate::state`]) with the bundle that manages it whenever a real run applies the action
//! (changed or already in place), so `dot status` can tell resources the config no longer has
//! (orphans) and `dot remove` can undo what `dot` did for a bundle.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One thing an action manages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// `link`, `file`, `dir`, `block`, `pkg`, `release`, `git`, `path`, `env`, `service`, ...
    pub kind: &'static str,
    /// Stable within its kind: a path as shown (`~/...`), `apt:fd-find`, ...
    pub id: String,
    /// How `dot remove` undoes it; `None`: nothing can be undone.
    pub undo: Option<Undo>,
    /// A user file this resource writes: what is there before `dot` first writes it is saved in
    /// the state directory, so `dot remove` can put it back.
    pub preserve: Option<PathBuf>,
    /// Whether applying changed this resource, when the action can tell its resources apart (a
    /// `package` step installs only the missing ones). `None`: the action's outcome says.
    pub changed: Option<bool>,
}

impl Resource {
    pub fn new(kind: &'static str, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
            undo: None,
            preserve: None,
            changed: None,
        }
    }

    pub fn with_undo(mut self, undo: Undo) -> Self {
        self.undo = Some(undo);
        self
    }

    pub fn preserving(mut self, path: impl Into<PathBuf>) -> Self {
        self.preserve = Some(path.into());
        self
    }

    pub fn changed(mut self, changed: bool) -> Self {
        self.changed = Some(changed);
        self
    }

    /// `kind:id`, the key it is recorded under.
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind, self.id)
    }
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.id)
    }
}

/// A file `dot` wrote, with the SHA-256 of what it wrote: removed only while it still holds
/// that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Written {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// How to undo a resource. Every variant is conservative: something changed since `dot` made it
/// is left alone and reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Undo {
    /// Remove the link (or junction) at `path` while it still points at `target`.
    Unlink { path: PathBuf, target: PathBuf },
    /// Remove files `dot` wrote, each while it still holds what `dot` wrote; `root`: through
    /// `sudo`.
    RemoveFiles {
        files: Vec<Written>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        root: bool,
    },
    /// Remove a directory `dot` created, when it is empty; `root`: through `sudo`.
    RemoveDir {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        root: bool,
    },
    /// Remove a directory tree `dot` unpacked (`download: { extract, replace: true }`), while it
    /// still has the `fingerprint` recorded when it was installed (its files' names, sizes,
    /// modes and times); `root`: through `sudo`.
    RemoveTree {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        root: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint: Option<String>,
    },
    /// Remove a managed block (its marker lines and everything between) from a file; `root`:
    /// the file is root-owned, read and written through `sudo`. `created_file`: `dot` created
    /// the file to hold the block, so it goes too when nothing else is left in it; then each
    /// of `created_dirs` (the directories `dot` created for it, deepest first) while empty.
    RemoveBlock {
        path: PathBuf,
        begin: String,
        end: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        root: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        created_file: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        created_dirs: Vec<PathBuf>,
    },
    /// Take `dir` out of the Windows user `Path` in the registry (`HKCU\\Environment`), where a
    /// `path:` entry with `persist: user` added it.
    RemoveUserPath { dir: PathBuf },
    /// Uninstall a package; only with `dot remove --packages`.
    Uninstall { manager: String, name: String },
    /// A `path:` / `env:` entry: dropping the bundle's entries from the record is the undo (`dot
    /// env --shell` stops exporting them).
    Forget,
    /// Remove the executables a `github_release` step installed and its install record.
    Release {
        repo: String,
        dir: PathBuf,
        files: Vec<Written>,
    },
    /// Nothing `dot` can undo by itself; what to do by hand.
    Manual { note: String },
}

impl Undo {
    /// What an earlier record of the same resource knew that this one cannot (it was made by a
    /// later run, when the file was already there): that `dot` created the file of a block, and
    /// the directories it made for it.
    pub fn carry_from(&mut self, old: &Undo) {
        if let (
            Undo::RemoveBlock {
                path,
                created_file,
                created_dirs,
                ..
            },
            Undo::RemoveBlock {
                path: old_path,
                created_file: old_file,
                created_dirs: old_dirs,
                ..
            },
        ) = (self, old)
            && path == old_path
            && !*created_file
            && created_dirs.is_empty()
        {
            *created_file = *old_file;
            created_dirs.clone_from(old_dirs);
        }
    }
}

/// How the machine differs from what an action wants, for `dot diff`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diff {
    /// A file's content, as a unified diff. An action never puts secret values here: content
    /// rendered with `secret()` is not compared until applied.
    File { path: String, unified: String },
    /// Where a link points: what is there now (`None`: nothing), and what it should be.
    Link {
        path: String,
        from: Option<String>,
        to: String,
    },
    /// Packages missing, and installed ones with a newer version (`fd 8.7.0 -> 8.7.1`).
    Packages {
        manager: String,
        missing: Vec<String>,
        outdated: Vec<String>,
    },
    /// Anything else, as a sentence.
    Note(String),
}

impl fmt::Display for Diff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Diff::File { unified, .. } => write!(f, "{unified}"),
            Diff::Link { path, from, to } => {
                let from = from.as_deref().unwrap_or("(nothing)");
                writeln!(f, "link {path}")?;
                writeln!(f, "- {from}")?;
                writeln!(f, "+ {to}")
            }
            Diff::Packages {
                manager,
                missing,
                outdated,
            } => {
                for m in missing {
                    writeln!(f, "+ {manager}: {m} (missing)")?;
                }
                for o in outdated {
                    writeln!(f, "~ {manager}: {o}")?;
                }
                Ok(())
            }
            Diff::Note(n) => writeln!(f, "{n}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_undo_serialise_stably() {
        let r = Resource::new("link", "~/.zshrc").with_undo(Undo::Unlink {
            path: "/h/.zshrc".into(),
            target: "/r/zshrc".into(),
        });
        assert_eq!(r.key(), "link:~/.zshrc");
        assert_eq!(r.to_string(), "link:~/.zshrc");
        let json = serde_json::to_string(r.undo.as_ref().unwrap()).unwrap();
        assert_eq!(
            json,
            r#"{"op":"unlink","path":"/h/.zshrc","target":"/r/zshrc"}"#
        );
        let files = Undo::RemoveFiles {
            files: vec![Written {
                path: "/x".into(),
                sha256: None,
            }],
            root: false,
        };
        let json = serde_json::to_string(&files).unwrap();
        assert_eq!(json, r#"{"op":"remove_files","files":[{"path":"/x"}]}"#);
        assert_eq!(serde_json::from_str::<Undo>(&json).unwrap(), files);
        let user_path = Undo::RemoveUserPath {
            dir: "/opt/x".into(),
        };
        let json = serde_json::to_string(&user_path).unwrap();
        assert_eq!(json, r#"{"op":"remove_user_path","dir":"/opt/x"}"#);
        assert_eq!(serde_json::from_str::<Undo>(&json).unwrap(), user_path);
    }

    #[test]
    fn diffs_render_as_text() {
        let d = Diff::Link {
            path: "~/.rc".into(),
            from: None,
            to: "rc".into(),
        };
        assert_eq!(d.to_string(), "link ~/.rc\n- (nothing)\n+ rc\n");
        let p = Diff::Packages {
            manager: "apt".into(),
            missing: vec!["fd-find".into()],
            outdated: vec!["jq 1.6 -> 1.7".into()],
        };
        assert_eq!(
            p.to_string(),
            "+ apt: fd-find (missing)\n~ apt: jq 1.6 -> 1.7\n"
        );
    }
}
