//! Private-file helpers: owner-only writes and the "readable by others" guardrail.
//!
//! On Windows there is no mode to check: the guardrail reads the file's access control list
//! instead and refuses a file that anyone but the current user, `SYSTEM` and `Administrators` can
//! read or change ([`crate::acl`]). Private files are created with exactly that ACL of their own
//! ([`crate::private_file`]), not the one their directory would pass down.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// `Err(message)` when `path` can be read or written by group or other. The message names the
/// fix. Never reads the file's content.
#[cfg(unix)]
pub(crate) fn check_private(path: &Path, what: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::metadata(path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "{what} {} is accessible by group/other (mode {mode:04o}); run `chmod 600 {}`",
            path.display(),
            path.display()
        ));
    }
    Ok(())
}

/// `Err(message)` when anyone but the current user, `SYSTEM` and `Administrators` can read or
/// change `path`, by its ACL. The message names them and the fix.
#[cfg(windows)]
pub(crate) fn check_private(path: &Path, what: &str) -> Result<(), String> {
    let acl = crate::acl::read(path)?;
    let who = crate::acl::exposed_to(&acl);
    if who.is_empty() {
        Ok(())
    } else {
        Err(crate::acl::exposed_message(
            what,
            &path.display().to_string(),
            &acl.me,
            &who,
        ))
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn check_private(_path: &Path, _what: &str) -> Result<(), String> {
    Ok(())
}

/// Whether `path` exists and is private ([`check_private`]).
pub(crate) fn is_private(path: &Path) -> bool {
    path.exists() && check_private(path, "").is_ok()
}

/// Creates `dir` and its missing parents, owner-only (`0700`) on unix.
pub(crate) fn create_private_dir(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))
}

/// Replaces `path` with `bytes` atomically (temp file + rename), owner-only (`0600`) on unix.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    write_with_mode(path, bytes, 0o600)
}

/// Replaces `path` with `bytes` atomically (temp file + rename), with `mode` on unix. Missing
/// parent directories are created owner-only.
pub(crate) fn write_with_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    #[cfg(not(unix))]
    let _ = mode;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    create_private_dir(dir)?;
    let name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        // A stale temp file (from a crash) could have any access rights: never reused.
        match fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot remove {}", tmp.display())),
        }
        // Created owner-only (Windows: a private ACL of its own, which a file outside the
        // profile would not inherit); opened up to `mode` only once complete, and on Windows
        // only when `mode` gives group or others access.
        let mut f = if cfg!(windows) && mode & 0o077 != 0 {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
        } else {
            crate::private_file::create(&tmp)
        }
        .with_context(|| format!("cannot create {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        #[cfg(unix)]
        if mode != 0o600 {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        drop(f);
        fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn writes_are_private_and_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a/b/secrets.env");
        write_private(&p, b"one").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"one");
        assert_eq!(mode(&p), 0o600);
        assert_eq!(mode(&tmp.path().join("a/b")), 0o700);
        write_private(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(fs::read_dir(tmp.path().join("a/b")).unwrap().count(), 1);
    }

    #[test]
    fn group_or_other_access_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("s.env");
        fs::write(&p, "").unwrap();
        for (m, ok) in [
            (0o600, true),
            (0o400, true),
            (0o640, false),
            (0o604, false),
            (0o620, false),
        ] {
            fs::set_permissions(&p, fs::Permissions::from_mode(m)).unwrap();
            let r = check_private(&p, "secrets file");
            assert_eq!(r.is_ok(), ok, "{m:o}");
            if let Err(msg) = r {
                assert!(
                    msg.contains("chmod 600") && msg.contains(&format!("{m:04o}")),
                    "{msg}"
                );
            }
        }
    }
}
