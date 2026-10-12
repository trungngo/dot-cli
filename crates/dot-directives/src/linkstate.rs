//! Which link destinations `dot` made as a fallback rather than a symlink (a junction or a
//! copy, where symlinks are not available: see [`dot_platform::link`]): the `links` section of
//! the state store ([`dot_core::state`]).
//!
//! A copy is an ordinary file, indistinguishable on disk from one the user made, so without this
//! record `link` would back it up and copy again on every run, and `clean` could never tell it
//! from the user's own files. With it:
//!
//! - `link` treats a recorded copy that still holds what `dot` wrote as its own: satisfied while
//!   it equals the source, refreshed (no backup) when the source changed; a recorded copy the
//!   user has since edited is treated like any other file in the way (`on_conflict`);
//! - `clean` removes a recorded copy whose repository source is gone, if it still holds what
//!   `dot` wrote;
//! - `dot remove` drops the record of a link it removes.
//!
//! Symlinks are never recorded here: they say where they point themselves.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::Result;
use dot_core::StateStore;
use dot_core::sections::LINKS;
use dot_platform::link::LinkKind;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One fallback link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fallback {
    pub dest: PathBuf,
    pub src: PathBuf,
    /// `junction` or `copy`.
    pub kind: String,
    /// Copies: the SHA-256 of what was written, to tell `dot`'s copy from a later edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl Fallback {
    pub fn kind(&self) -> Option<LinkKind> {
        LinkKind::parse(&self.kind)
    }

    /// Whether the file at `dest` still holds exactly what `dot` copied there.
    pub fn untouched(&self) -> bool {
        match (&self.sha256, sha256_file(&self.dest)) {
            (Some(want), Ok(have)) => *want == have,
            _ => false,
        }
    }
}

type Links = BTreeMap<String, Fallback>;

/// The fallback records. Every change goes through the store's lock, so steps running in
/// parallel (`--jobs`) or another `dot` process never lose each other's records.
#[derive(Debug, Clone)]
pub struct LinkState {
    pub store: StateStore,
}

fn key(dest: &Path) -> String {
    dest.display().to_string()
}

impl LinkState {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            store: StateStore::at(dir),
        }
    }

    /// Unreadable or malformed state counts as "nothing recorded": the worst outcome is that a
    /// copy is backed up once and made again.
    fn read(&self) -> Links {
        self.store.section(LINKS)
    }

    pub fn get(&self, dest: &Path) -> Option<Fallback> {
        self.read().remove(&key(dest))
    }

    /// Every recorded fallback.
    pub fn all(&self) -> Vec<Fallback> {
        self.read().into_values().collect()
    }

    /// Records a fallback, replacing any record for the same destination.
    pub fn record(&self, f: Fallback) -> Result<()> {
        self.store.update_section(LINKS, |links: &mut Links| {
            links.insert(key(&f.dest), f);
        })
    }

    /// Drops the record for `dest`, if any (the file is not touched).
    pub fn forget(&self, dest: &Path) -> Result<()> {
        if self.get(dest).is_none() {
            return Ok(());
        }
        self.store.update_section(LINKS, |links: &mut Links| {
            links.remove(&key(dest));
        })
    }
}

/// Lower-case hex SHA-256 of a file's content.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_forgets_and_tells_edits_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let s = LinkState::in_dir(&tmp.path().join("state/dot"));
        let dest = tmp.path().join("rc");
        std::fs::write(&dest, "one").unwrap();
        assert_eq!(s.get(&dest), None);
        let f = Fallback {
            dest: dest.clone(),
            src: tmp.path().join("repo/rc"),
            kind: LinkKind::Copy.as_str().into(),
            sha256: Some(sha256_file(&dest).unwrap()),
        };
        s.record(f.clone()).unwrap();
        assert_eq!(s.get(&dest), Some(f.clone()));
        assert_eq!(s.all(), std::slice::from_ref(&f));
        assert_eq!(f.kind(), Some(LinkKind::Copy));
        assert!(f.untouched());
        std::fs::write(&dest, "edited").unwrap();
        assert!(!f.untouched());
        s.forget(&dest).unwrap();
        assert_eq!(s.get(&dest), None);
        s.forget(&dest).unwrap();
        std::fs::write(s.store.path(), "not json").unwrap();
        assert!(s.all().is_empty());
        std::fs::write(&dest, "abc").unwrap();
        assert_eq!(
            sha256_file(&dest).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
