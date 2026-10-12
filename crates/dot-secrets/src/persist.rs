//! Writing answers into a machine-local dotenv file such as `~/.config/dot/local.env`, where a
//! prompted variable declared `persist: local` is kept so it is asked only once.
//!
//! Each key is replaced in place (or appended), comments and other entries are kept, and values
//! are quoted so `dotenvy` reads them back verbatim. The file is written atomically, `0644` for
//! plain values and `0600` once it holds a sensitive one; a file that is already owner-only is
//! never opened up again.

use std::path::Path;

use anyhow::{Context, Result};
use zeroize::Zeroizing;

use crate::dotenv;
use crate::fsutil::{is_private, write_with_mode};

/// Upserts `entries` (`KEY`, value) into the dotenv file at `path`, creating it (and its
/// directory) when missing. `sensitive`: at least one value is sensitive, so the file must be
/// owner-only.
pub fn persist_env(path: &Path, entries: &[(&str, &str)], sensitive: bool) -> Result<()> {
    let current = if path.exists() {
        Zeroizing::new(
            std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?,
        )
    } else {
        Zeroizing::new(String::new())
    };
    let mut text = Zeroizing::new(current.to_string());
    for (key, value) in entries {
        text = Zeroizing::new(dotenv::upsert(&text, key, value));
    }
    let mode = if sensitive || is_private(path) {
        0o600
    } else {
        0o644
    };
    write_with_mode(path, text.as_bytes(), mode)
}

/// [`persist_env`] for values coming from another machine (`dot apply --host
/// --persist-secrets`): a key that already holds a different value is replaced only with
/// `force`. Returns what happened to each key, in order; never a value. `dry`: only works out
/// what would happen.
pub fn merge_env(
    path: &Path,
    entries: &[(&str, &str)],
    sensitive: bool,
    force: bool,
    dry: bool,
) -> Result<Vec<(String, crate::transfer::Merge)>> {
    use crate::transfer::Merge;
    let mut have: std::collections::BTreeMap<String, Zeroizing<String>> = Default::default();
    if path.exists() {
        let text = Zeroizing::new(
            std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?,
        );
        for item in dotenvy::from_read_iter(text.as_bytes()) {
            let (k, v) =
                item.map_err(|_| anyhow::anyhow!("{} is not valid dotenv", path.display()))?;
            have.insert(k, Zeroizing::new(v));
        }
    }
    let mut out = Vec::new();
    let mut write: Vec<(&str, &str)> = Vec::new();
    for (key, value) in entries {
        let result = match have.get(*key) {
            Some(cur) if cur.as_str() == *value => Merge::Unchanged,
            Some(cur) if !cur.is_empty() && !force => Merge::Kept,
            Some(cur) if !cur.is_empty() => Merge::Updated,
            _ => Merge::Added,
        };
        if matches!(result, Merge::Added | Merge::Updated) {
            write.push((key, value));
        }
        out.push((key.to_string(), result));
    }
    if !write.is_empty() && !dry {
        persist_env(path, &write, sensitive)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn read(p: &Path) -> Vec<(String, String)> {
        dotenvy::from_path_iter(p)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn answers_are_upserted_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("cfg/dot/local.env");
        persist_env(&p, &[("EMAIL", "me@example.com")], false).unwrap();
        #[cfg(unix)]
        assert_eq!(mode(&p), 0o644);
        std::fs::write(&p, "# mine\nDOT_PROFILE=mac\nEMAIL=old\n").unwrap();
        persist_env(&p, &[("EMAIL", "new $x \"q\""), ("NAME", "Alice")], false).unwrap();
        assert_eq!(
            read(&p),
            [
                ("DOT_PROFILE".into(), "mac".into()),
                ("EMAIL".into(), "new $x \"q\"".into()),
                ("NAME".into(), "Alice".into())
            ]
        );
        assert!(std::fs::read_to_string(&p).unwrap().starts_with("# mine\n"));
    }

    #[test]
    fn merging_keeps_different_values_unless_forced() {
        use crate::transfer::Merge;
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("local.env");
        std::fs::write(&p, "# mine\nEMAIL=old@example.com\nNAME=Alice\n").unwrap();
        let r = merge_env(
            &p,
            &[
                ("EMAIL", "new@example.com"),
                ("NAME", "Alice"),
                ("CITY", "Oslo"),
            ],
            false,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            r,
            [
                ("EMAIL".to_string(), Merge::Kept),
                ("NAME".to_string(), Merge::Unchanged),
                ("CITY".to_string(), Merge::Added)
            ]
        );
        assert_eq!(read(&p)[0], ("EMAIL".into(), "old@example.com".into()));
        let before = std::fs::read(&p).unwrap();
        let r = merge_env(&p, &[("EMAIL", "new@example.com")], false, true, true).unwrap();
        assert_eq!(r, [("EMAIL".to_string(), Merge::Updated)]);
        assert_eq!(
            std::fs::read(&p).unwrap(),
            before,
            "a dry merge writes nothing"
        );
        let r = merge_env(&p, &[("EMAIL", "new@example.com")], false, true, false).unwrap();
        assert_eq!(r, [("EMAIL".to_string(), Merge::Updated)]);
        assert_eq!(read(&p)[0], ("EMAIL".into(), "new@example.com".into()));
        assert!(std::fs::read_to_string(&p).unwrap().starts_with("# mine\n"));
    }

    #[cfg(unix)]
    #[test]
    fn a_sensitive_answer_makes_the_file_private_for_good() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("local.env");
        persist_env(&p, &[("A", "1")], false).unwrap();
        assert_eq!(mode(&p), 0o644);
        persist_env(&p, &[("PIN", "1234")], true).unwrap();
        assert_eq!(mode(&p), 0o600);
        persist_env(&p, &[("B", "2")], false).unwrap();
        assert_eq!(mode(&p), 0o600, "never loosened");
        assert_eq!(read(&p).len(), 3);
    }
}
