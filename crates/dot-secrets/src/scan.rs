//! `.env.example` markers and the committed-`.env*` leak check (used by `dot secrets check` and
//! the pre-commit hook).
//!
//! A key is a secret when its line in `.env.example` carries an inline comment starting with
//! the word `secret`:
//!
//! ```text
//! GITLAB_HOST=gitlab.com
//! GITLAB_TOKEN=          # secret
//! NGROK_AUTHTOKEN=       # secret: ngrok dashboard → Your Authtoken
//! ```

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use zeroize::Zeroizing;

use crate::dotenv;
use crate::git::Git;

fn marks_secret(comment: Option<&str>) -> bool {
    comment.is_some_and(|c| {
        let c = c.trim_start();
        c.get(..6).is_some_and(|w| w.eq_ignore_ascii_case("secret"))
            && !c[6..].starts_with(|ch: char| ch.is_alphanumeric() || ch == '_')
    })
}

/// Keys marked `# secret` in `.env.example` text.
pub fn example_secret_keys(text: &str) -> BTreeSet<String> {
    dotenv::entries(text)
        .into_iter()
        .filter(|e| marks_secret(e.comment.as_deref()))
        .map(|e| e.key)
        .collect()
}

pub fn read_example_secret_keys(path: &Path) -> Result<BTreeSet<String>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(example_secret_keys(&text))
}

/// A committed `.env*` file holding a value for a secret key. Never carries the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leak {
    /// Relative to the work tree root.
    pub file: PathBuf,
    /// The same file, absolute.
    pub path: PathBuf,
    /// 1-based.
    pub line: usize,
    pub key: String,
}

impl fmt::Display for Leak {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}: `{}` is a secret but has a committed value; move it to the secrets provider and rotate it",
            self.file.display(),
            self.line,
            self.key
        )
    }
}

/// Every tracked `.env*` file (by staged content, so a pre-commit hook sees what is about to be
/// committed) that holds a non-empty value for a key in `secret_keys` or for a key marked
/// `# secret` on its own line. Outside a work tree, or without git, nothing is committed and
/// the result is empty.
pub fn scan_committed_env_files(
    git: &Git,
    repo: &Path,
    secret_keys: &BTreeSet<String>,
) -> Result<Vec<Leak>> {
    let Some((root, files)) = git.tracked_files(repo)? else {
        return Ok(Vec::new());
    };
    let mut leaks = Vec::new();
    for rel in files {
        let is_env = rel
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with(".env"));
        if !is_env {
            continue;
        }
        let bytes = Zeroizing::new(git.staged_content(&root, &rel)?);
        let text = Zeroizing::new(String::from_utf8_lossy(&bytes).into_owned());
        for e in dotenv::entries(&text) {
            if e.has_value && (secret_keys.contains(&e.key) || marks_secret(e.comment.as_deref())) {
                leaks.push(Leak {
                    path: root.join(&rel),
                    file: rel.clone(),
                    line: e.first_line + 1,
                    key: e.key,
                });
            }
        }
    }
    Ok(leaks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testing::{add, init, isolated};

    #[test]
    fn secret_markers() {
        let keys = example_secret_keys(
            "A=1\nB= # secret\nC= #Secret: token\nD= # secretary\nE= # not secret\nF=x # secret_key\nG= #secret\n",
        );
        assert_eq!(keys.into_iter().collect::<Vec<_>>(), ["B", "C", "G"]);
    }

    #[test]
    fn finds_committed_values_for_secret_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(git) = isolated(tmp.path()) else {
            eprintln!("git not installed; skipping");
            return;
        };
        let repo = tmp.path().join("repo");
        init(&git, &repo);
        std::fs::write(repo.join(".env.example"), "HOST=\nTOKEN= # secret\n").unwrap();
        std::fs::write(repo.join(".env"), "HOST=gitlab.com\nTOKEN=glpat-123\n").unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::fs::write(
            repo.join("sub/.env.work"),
            "# c\nOTHER='x' # secret\nTOKEN=\n",
        )
        .unwrap();
        std::fs::write(repo.join("notes.env"), "TOKEN=abc\n").unwrap();
        std::fs::write(repo.join(".env.local"), "TOKEN=untracked\n").unwrap();
        for f in [".env.example", ".env", "sub/.env.work", "notes.env"] {
            add(&git, &repo, f);
        }
        let keys = read_example_secret_keys(&repo.join(".env.example")).unwrap();
        let leaks = scan_committed_env_files(&git, &repo.join("sub"), &keys).unwrap();
        let found: Vec<(PathBuf, usize, String)> = leaks
            .iter()
            .map(|l| (l.file.clone(), l.line, l.key.clone()))
            .collect();
        assert_eq!(
            found,
            [
                (".env".into(), 2, "TOKEN".into()),
                ("sub/.env.work".into(), 2, "OTHER".into()),
            ]
        );
        for l in &leaks {
            assert!(l.path.is_absolute() && l.path.ends_with(&l.file), "{l:?}");
        }
        let msg = leaks[0].to_string();
        assert!(
            msg.starts_with(".env:2: `TOKEN`") && !msg.contains("glpat"),
            "{msg}"
        );

        // The staged content counts, not the working tree.
        std::fs::write(repo.join(".env"), "HOST=gitlab.com\nTOKEN=\n").unwrap();
        assert_eq!(
            scan_committed_env_files(&git, &repo, &keys).unwrap().len(),
            2
        );
        add(&git, &repo, ".env");
        assert_eq!(
            scan_committed_env_files(&git, &repo, &keys).unwrap().len(),
            1
        );
    }

    #[test]
    fn outside_a_repo_or_without_git_nothing_is_committed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "TOKEN=x # secret\n").unwrap();
        let keys = BTreeSet::from(["TOKEN".to_string()]);
        let no_git = Git::system().with_program(tmp.path().join("no-git"));
        assert!(
            scan_committed_env_files(&no_git, tmp.path(), &keys)
                .unwrap()
                .is_empty()
        );
        if let Some(git) = isolated(tmp.path()) {
            assert!(
                scan_committed_env_files(&git, tmp.path(), &keys)
                    .unwrap()
                    .is_empty()
            );
        }
    }
}
