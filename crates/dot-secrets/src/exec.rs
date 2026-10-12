//! Finding vendor CLIs (`lpass`, `ssh`, …) on a search path. The path is injectable so tests
//! put fake executables in a temp directory and never reach the real tools.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The executable `name` would run as: `name` itself when it is a path (has a separator),
/// else the first match in `search_path` (default: `$PATH`). On Windows only the `PATHEXT`
/// names are tried for a name without an extension ([`exe_candidates`]).
pub fn find_program(name: &OsStr, search_path: Option<&OsStr>) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.components().count() > 1 || p.is_absolute() {
        return is_executable(p).then(|| p.to_path_buf());
    }
    let path: OsString = match search_path {
        Some(s) => s.to_os_string(),
        None => std::env::var_os("PATH")?,
    };
    let pathext = std::env::var("PATHEXT").ok();
    let names = exe_candidates(name, pathext.as_deref(), cfg!(windows));
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for n in &names {
            let candidate = dir.join(n);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// The file names `name` may be found as in one search-path directory, in order: `name` itself
/// except on Windows, where a name without an extension is only tried with each `PATHEXT`
/// extension (an extensionless file, such as the `#!/bin/sh` shims Scoop and npm put next to
/// their `.cmd` ones, is not a Windows executable) and one with an extension is tried as it is
/// first. The twin of `dot_core::paths::exe_candidates` (this crate does not depend on `dot-core`).
pub fn exe_candidates(name: &OsStr, pathext: Option<&str>, windows: bool) -> Vec<OsString> {
    if !windows {
        return vec![name.to_os_string()];
    }
    let exts = pathext
        .filter(|p| !p.trim().is_empty())
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .map(str::trim)
        .filter(|e| e.starts_with('.') && e.len() > 1);
    let mut out = Vec::new();
    if Path::new(name).extension().is_some() {
        out.push(name.to_os_string());
    }
    for e in exts {
        let mut with = name.to_os_string();
        with.push(e);
        out.push(with);
    }
    out
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// The first line of a CLI's stderr, trimmed, for error messages. Vendor CLIs print their
/// errors there; stdout (which may hold a value) is never used.
#[cfg_attr(not(feature = "secrets-lastpass"), allow(dead_code))]
pub(crate) fn first_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut out: String = line.chars().take(300).collect();
    if out.is_empty() {
        out.push_str("(no message)");
    }
    out
}

#[cfg(test)]
mod candidate_tests {
    use super::*;

    #[test]
    fn windows_tries_only_pathext_names_for_a_bare_name() {
        let names = |n: &str, ext: Option<&str>, w: bool| -> Vec<String> {
            exe_candidates(OsStr::new(n), ext, w)
                .into_iter()
                .map(|s| s.into_string().unwrap())
                .collect()
        };
        assert_eq!(names("lpass", None, false), ["lpass"]);
        // A Scoop-style directory with `op` (a `#!/bin/sh` shim) and `op.cmd`: never the shim.
        let c = names("op", Some(".COM;.EXE;.BAT;.CMD;.PS1"), true);
        assert_eq!(c, ["op.COM", "op.EXE", "op.BAT", "op.CMD", "op.PS1"]);
        assert_eq!(
            names("ssh.exe", Some(".EXE"), true),
            ["ssh.exe", "ssh.exe.EXE"]
        );
        assert_eq!(names("ssh", None, true).len(), 4);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn finds_executables_on_the_given_path_only() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("tool"), "").unwrap(); // not executable
        std::fs::write(b.join("tool"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(b.join("tool"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([&a, &b]).unwrap();
        assert_eq!(
            find_program(OsStr::new("tool"), Some(&path)),
            Some(b.join("tool"))
        );
        assert_eq!(find_program(OsStr::new("nope"), Some(&path)), None);
        assert_eq!(find_program(OsStr::new("tool"), Some(OsStr::new(""))), None);
        let direct = b.join("tool");
        assert_eq!(
            find_program(direct.as_os_str(), Some(OsStr::new(""))),
            Some(direct)
        );
        assert_eq!(first_line(b"\n  Error: nope \nmore"), "Error: nope");
        assert_eq!(first_line(b""), "(no message)");
    }
}
