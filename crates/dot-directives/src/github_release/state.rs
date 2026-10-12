//! Which release of which repository is installed where: the `github_releases` section of the
//! state store ([`dot_core::state`]). Lets planning say "satisfied" (or "update v1 -> v2")
//! without running anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use dot_core::StateStore;
use dot_core::sections::GITHUB_RELEASES;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub repo: String,
    pub dir: PathBuf,
    pub tag: String,
    pub asset: String,
    /// File names written into `dir`.
    pub bins: Vec<String>,
}

type Installs = BTreeMap<String, Installed>;

/// The install records.
#[derive(Debug, Clone)]
pub struct StateFile {
    pub store: StateStore,
}

fn key(repo: &str, dir: &Path) -> String {
    format!("{repo} {}", dir.display())
}

impl StateFile {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            store: StateStore::at(dir),
        }
    }

    /// Unreadable or malformed state counts as "nothing recorded": the worst outcome is a
    /// reinstall.
    pub fn get(&self, repo: &str, dir: &Path) -> Option<Installed> {
        self.store
            .section::<Installs>(GITHUB_RELEASES)
            .remove(&key(repo, dir))
    }

    /// Records an install.
    pub fn record(&self, installed: Installed) -> Result<()> {
        self.store
            .update_section(GITHUB_RELEASES, |m: &mut Installs| {
                m.insert(key(&installed.repo, &installed.dir), installed);
            })
    }

    /// Drops the record of `repo` in `dir` (`dot remove`).
    pub fn forget(&self, repo: &str, dir: &Path) -> Result<()> {
        self.store
            .update_section(GITHUB_RELEASES, |m: &mut Installs| {
                m.remove(&key(repo, dir));
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_per_repo_and_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let s = StateFile::in_dir(&tmp.path().join("state/dot"));
        let dir = tmp.path().join("bin");
        assert_eq!(s.get("a/b", &dir), None);
        let one = Installed {
            repo: "a/b".into(),
            dir: dir.clone(),
            tag: "v1".into(),
            asset: "b.tar.gz".into(),
            bins: vec!["b".into()],
        };
        s.record(one.clone()).unwrap();
        assert_eq!(s.get("a/b", &dir), Some(one.clone()));
        assert_eq!(s.get("a/b", &tmp.path().join("elsewhere")), None);
        let two = Installed {
            tag: "v2".into(),
            ..one
        };
        s.record(two.clone()).unwrap();
        assert_eq!(s.get("a/b", &dir), Some(two.clone()));
        s.forget("a/b", &dir).unwrap();
        assert_eq!(s.get("a/b", &dir), None);
        s.record(two.clone()).unwrap();
        std::fs::write(s.store.path(), "not json").unwrap();
        assert_eq!(s.get("a/b", &dir), None);
    }
}
