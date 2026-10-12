//! Path helpers shared by the loader and directives. Nothing here reads `$HOME` itself: the home
//! directory is always passed in, so tests can point it at a temp dir.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

/// The environment variable that overrides [`state_dir`] on every system.
pub const STATE_DIR_ENV: &str = "DOT_STATE_DIR";

/// `dot`'s state directory (run logs, installed releases, apt's refresh stamp, the `path:` /
/// `env:` record, pending service restarts): `$DOT_STATE_DIR` when set to an absolute path,
/// else `$XDG_STATE_HOME/dot` or `~/.local/state/dot` on Unix, `%LOCALAPPDATA%\dot` or
/// `~\AppData\Local\dot` on Windows. `env` looks variables up, so callers (and tests) decide
/// which environment counts; with none set it derives from `home` alone.
pub fn state_dir(home: &Path, env: impl Fn(&str) -> Option<String>) -> PathBuf {
    let absolute = |k: &str| env(k).map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(p) = absolute(STATE_DIR_ENV) {
        return p;
    }
    if cfg!(windows) {
        absolute("LOCALAPPDATA")
            .unwrap_or_else(|| home.join("AppData").join("Local"))
            .join("dot")
    } else {
        absolute("XDG_STATE_HOME")
            .unwrap_or_else(|| home.join(".local/state"))
            .join("dot")
    }
}

/// Expands a leading `~` (alone or followed by a separator) to `home`. Other paths are returned
/// unchanged; `~user` forms are not supported and are left as-is.
pub fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// `path` (absolute) as the same path under `root`: `/etc/apt/keyrings/x.gpg` under
/// `/tmp/sbx/root` is `/tmp/sbx/root/etc/apt/keyrings/x.gpg`. For `DOT_SYSROOT`, which moves
/// every root-owned file `dot` writes into a directory (a sandbox without root).
pub fn rebase(root: &Path, path: &Path) -> PathBuf {
    let mut out = root.to_path_buf();
    for c in path.components() {
        if let Component::Normal(n) = c {
            out.push(n);
        }
    }
    out
}

/// A destination path: must be absolute after `~` expansion. Relative destinations are rejected
/// rather than guessed at, because they would depend on the working directory. Destinations
/// parse on every OS (a Linux step's `/usr/local/go` under a `when:`), so this is where a path
/// that is absolute only on another OS is refused, saying so.
pub fn expand_target(path: &str, home: &Path) -> Result<PathBuf> {
    let p = expand_tilde(path, home);
    if !p.is_absolute() {
        if let Some(why) = other_os_absolute(path, cfg!(windows)) {
            bail!(
                "`{path}` {why}; if the step is for another system, give it a \
                 `when: {{ os: ... }}`"
            );
        }
        bail!("`{path}` must be an absolute path or start with `~`");
    }
    Ok(normalize(&p))
}

/// Why `path`, not absolute on this system (Windows when `windows`), is absolute on another
/// one: a rooted path without a drive on Windows, a drive or UNC path elsewhere.
fn other_os_absolute(path: &str, windows: bool) -> Option<&'static str> {
    let b = path.as_bytes();
    if windows {
        (b.first().is_some_and(|c| matches!(c, b'/' | b'\\')))
            .then_some("has no drive letter, which an absolute path on Windows needs")
    } else {
        let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
        (drive || path.starts_with(r"\\")).then_some("is a Windows path, not absolute here")
    }
}

/// A path from the config that may be relative to `base` (the bundle's directory).
pub fn resolve_from(path: &str, base: &Path, home: &Path) -> PathBuf {
    let p = expand_tilde(path, home);
    normalize(&base.join(p))
}

/// Lexically removes `.` and resolves `..` components. Does not touch the filesystem.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(c);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// The ordinary spelling of a Windows path prefix: `\\?\C:` becomes `C:` and `\\?\UNC\srv\share`
/// becomes `\\srv\share`. `canonicalize` returns the verbatim forms while `read_link` and config
/// paths use the ordinary ones, and `Path::starts_with` treats the two as different, so every
/// path that is compared goes through this. Other prefixes (and all Unix paths) are unchanged.
pub fn plain_prefix(prefix: std::path::PrefixComponent<'_>) -> PathBuf {
    use std::path::Prefix;
    match prefix.kind() {
        Prefix::VerbatimDisk(d) => PathBuf::from(format!("{}:", char::from(d))),
        Prefix::VerbatimUNC(server, share) => {
            let mut s = std::ffi::OsString::from(r"\\");
            s.push(server);
            s.push(r"\");
            s.push(share);
            PathBuf::from(s)
        }
        _ => PathBuf::from(prefix.as_os_str()),
    }
}

/// `path` with a verbatim Windows prefix replaced by its ordinary spelling (see
/// [`plain_prefix`]). Does not touch the filesystem.
pub fn simplify(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => out.push(plain_prefix(p)),
            other => out.push(other),
        }
    }
    out
}

/// `path` in its ordinary spelling when it is a verbatim Windows path (`\\?\C:\...`, what
/// `canonicalize` and some tools hand out) that names the same file that way (see
/// [`ordinary_spelling`]); any other path exactly as given. For paths from the command line and
/// the environment, which are opened and end up in messages: users see the verbatim form only
/// when it is the only correct spelling.
pub fn plain(path: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return path;
    }
    match path.to_str().and_then(ordinary_spelling) {
        Some(s) => PathBuf::from(s),
        None => path,
    }
}

/// The ordinary spelling of a verbatim Windows path: `\\?\C:\x` is `C:\x` and
/// `\\?\UNC\srv\share\x` is `\\srv\share\x`. `None` for any other path, and when the two
/// would not name the same file: the verbatim prefix turns off the normalisation Win32 applies
/// to ordinary paths, which drops a trailing `.` or space from a name, resolves `.` and `..`,
/// treats `/` as a separator and a device name (`NUL`, `con.txt`, `COM1`) as the device, and
/// limits the length to `MAX_PATH`. Any of those in the path keeps it verbatim (the rule
/// `dunce::simplified` follows). Pure string work, so it is the same on every OS.
pub fn ordinary_spelling(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\?\")?;
    let (head, tail) = if let Some(unc) = rest.strip_prefix(r"UNC\") {
        let mut it = unc.splitn(3, '\\');
        let (server, share) = (it.next()?, it.next()?);
        if !ordinary_name(server) || !ordinary_name(share) {
            return None;
        }
        match it.next() {
            Some(tail) => (format!(r"\\{server}\{share}\"), tail),
            None => (format!(r"\\{server}\{share}"), ""),
        }
    } else {
        let b = rest.as_bytes();
        if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
            return None;
        }
        (rest[..3].to_string(), &rest[3..])
    };
    if !tail.is_empty() {
        // A single trailing separator is spelled the same both ways.
        let names = tail.strip_suffix('\\').unwrap_or(tail);
        if !names.split('\\').all(ordinary_name) {
            return None;
        }
    }
    let out = head + tail;
    (out.encode_utf16().count() < 260).then_some(out)
}

/// Whether one path component means the same with and without the verbatim prefix.
fn ordinary_name(name: &str) -> bool {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name.contains('/')
    {
        return false;
    }
    // Device names count with any extension (`nul.txt`) and trailing spaces before it.
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let stem = stem.to_ascii_uppercase();
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|n| {
            let mut c = n.chars();
            c.next()
                .is_some_and(|d| d.is_ascii_digit() || "¹²³".contains(d))
                && c.next().is_none()
        });
    !device
}

/// Like `realpath`, but works for paths that do not (fully) exist: every existing symlink along
/// the way is followed, including dangling ones, and missing components are appended as-is.
/// Returns `None` on a symlink loop or an unreadable link, so callers can treat the path as
/// unknown. `path` should be absolute. Windows prefixes come out in their ordinary spelling
/// (see [`plain_prefix`]), whichever form the input or the link targets used.
pub fn resolve_lenient(path: &Path) -> Option<PathBuf> {
    use std::collections::VecDeque;
    use std::ffi::OsString;

    enum Part {
        Root(PathBuf),
        Parent,
        Name(OsString),
    }
    fn parts(p: &Path) -> Vec<Part> {
        let mut out = Vec::new();
        for c in p.components() {
            match c {
                Component::Prefix(p) => out.push(Part::Root(plain_prefix(p))),
                Component::RootDir => out.push(Part::Root(PathBuf::from(c.as_os_str()))),
                Component::CurDir => {}
                Component::ParentDir => out.push(Part::Parent),
                Component::Normal(n) => out.push(Part::Name(n.to_os_string())),
            }
        }
        out
    }

    let mut pending: VecDeque<Part> = parts(path).into();
    let mut out = PathBuf::new();
    let mut hops = 0;
    while let Some(part) = pending.pop_front() {
        match part {
            // `PathBuf::push` of a rooted component replaces what is there (keeping a Windows
            // drive prefix for a bare `\`), which is exactly path-resolution semantics.
            Part::Root(r) => out.push(r),
            Part::Parent => {
                out.pop();
            }
            Part::Name(n) => {
                let candidate = out.join(&n);
                match std::fs::symlink_metadata(&candidate) {
                    Ok(m) if m.file_type().is_symlink() => {
                        hops += 1;
                        if hops > 40 {
                            return None;
                        }
                        let target = std::fs::read_link(&candidate).ok()?;
                        let mut new: VecDeque<Part> = parts(&target).into();
                        new.extend(pending);
                        pending = new;
                    }
                    Ok(_) => out = long_name(candidate),
                    Err(_) => out = candidate,
                }
            }
        }
    }
    Some(out)
}

/// `path` (which exists) with its last component in its long form when it is a Windows 8.3
/// short name (`RUNNER~1`, as `%TEMP%` may hold): `canonicalize` gives long names, and the two
/// spellings must compare equal. Only the last component changes; anywhere else, and when the
/// lookup fails, `path` as given.
#[cfg(windows)]
fn long_name(path: PathBuf) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetLongPathNameW;

    if !path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().contains('~'))
    {
        return path;
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut buf = vec![0u16; 260];
    let long = loop {
        // SAFETY: `wide` is NUL-terminated and `buf` holds `buf.len()` u16s, the size passed.
        let n = unsafe { GetLongPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        let n = n as usize;
        if n == 0 {
            return path;
        }
        if n < buf.len() {
            buf.truncate(n);
            break PathBuf::from(OsString::from_wide(&buf));
        }
        buf.resize(n, 0);
    };
    match long.file_name() {
        Some(name) => path.with_file_name(name),
        None => path,
    }
}

#[cfg(not(windows))]
fn long_name(path: PathBuf) -> PathBuf {
    path
}

/// `PATHEXT` when the environment has none.
pub const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// The file names a program name may be found as in one `PATH` directory, in the order they
/// are tried. Elsewhere than on Windows, just the name. On Windows, a name without an extension
/// only with each of `PATHEXT`'s (default [`DEFAULT_PATHEXT`]), in order, never bare: an
/// extensionless file on `PATH` is not something Windows runs (Scoop and npm put `#!/bin/sh`
/// shims for Git Bash next to their `.cmd` ones, and CreateProcess fails on those with "not a
/// valid Win32 application"). A name that has an extension (`sc.exe`) is tried as it is first,
/// as `cmd.exe` does.
pub fn exe_candidates(program: &str, pathext: Option<&str>, windows: bool) -> Vec<String> {
    if !windows {
        return vec![program.to_string()];
    }
    let exts: Vec<&str> = pathext
        .filter(|p| !p.trim().is_empty())
        .unwrap_or(DEFAULT_PATHEXT)
        .split(';')
        .map(str::trim)
        .filter(|e| e.starts_with('.') && e.len() > 1)
        .collect();
    let has_ext = Path::new(program).extension().is_some();
    let mut out = Vec::new();
    if has_ext {
        out.push(program.to_string());
    }
    out.extend(exts.iter().map(|e| format!("{program}{e}")));
    out
}

/// Whether this platform's filesystems are case-insensitive by default (APFS and HFS+ on macOS,
/// NTFS on Windows): `~/DOTFILES` there is `~/dotfiles`. Safety checks fold case here, erring
/// towards refusing on the rare case-sensitive volume.
pub const CASE_INSENSITIVE_FS: bool = cfg!(any(windows, target_os = "macos"));

/// Whether `path` is `base` or inside it, component by component, ignoring case when `fold`
/// (Unicode lower case; drive letters too). Pure: nothing is looked up on disk.
pub fn starts_with_fold(path: &Path, base: &Path, fold: bool) -> bool {
    if !fold {
        return path.starts_with(base);
    }
    let key = |c: Component<'_>| -> std::ffi::OsString {
        let s = c.as_os_str();
        match s.to_str() {
            Some(t) => t.to_lowercase().into(),
            None => s.to_os_string(),
        }
    };
    let mut p = path.components();
    for b in base.components() {
        match p.next() {
            Some(c) if key(c) == key(b) => {}
            _ => return false,
        }
    }
    true
}

/// `path` as it is on disk: every symlink followed ([`resolve_lenient`]), and the part that
/// exists spelled the way the filesystem stores it (`canonicalize`: `realpath` on macOS and
/// `GetFinalPathNameByHandle` on Windows give the on-disk case), the rest appended as given.
/// `None` on a symlink loop.
pub fn on_disk(path: &Path) -> Option<PathBuf> {
    let resolved = resolve_lenient(path)?;
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    let mut existing: &Path = &resolved;
    loop {
        if let Ok(c) = std::fs::canonicalize(existing) {
            let mut out = simplify(&c);
            out.extend(missing.iter().rev());
            return Some(out);
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                existing = parent;
            }
            _ => return Some(resolved),
        }
    }
}

/// Where `dest` itself is, for deciding whether it may be written or removed: its parent
/// [`on_disk`], and its own name as the directory stores it (a symlink at `dest` is not
/// followed: it is what would be replaced). On a case-insensitive filesystem `~/DOTFILES` comes
/// out as `~/dotfiles`, so comparing it with the repository or the home directory is not fooled
/// by how it was typed. `None` on a symlink loop.
pub fn real_location(dest: &Path) -> Option<PathBuf> {
    let (Some(parent), Some(name)) = (dest.parent(), dest.file_name()) else {
        return Some(simplify(&normalize(dest)));
    };
    let parent = on_disk(parent)?;
    let at = parent.join(name);
    let Ok(meta) = std::fs::symlink_metadata(&at) else {
        return Some(at);
    };
    if !meta.file_type().is_symlink()
        && let Ok(c) = std::fs::canonicalize(&at)
    {
        return Some(simplify(&c));
    }
    // A symlink: the entry's own spelling, found among its directory's entries.
    let wanted = name.to_str().map(str::to_lowercase);
    let spelled = std::fs::read_dir(&parent).ok().and_then(|entries| {
        let names: Vec<std::ffi::OsString> = entries.flatten().map(|e| e.file_name()).collect();
        if names.iter().any(|n| n == name) {
            return None;
        }
        names
            .into_iter()
            .find(|n| wanted.is_some() && n.to_str().map(str::to_lowercase) == wanted)
    });
    Some(match spelled {
        Some(n) => parent.join(n),
        None => at,
    })
}

/// Shortens a path for display by replacing the home prefix with `~`.
pub fn display(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An absolute path on every platform: `/h` on Unix, `C:\h` on Windows.
    pub(crate) fn abs(p: &str) -> PathBuf {
        assert!(p.starts_with('/'));
        if cfg!(windows) {
            PathBuf::from(format!("C:{}", p.replace('/', "\\")))
        } else {
            PathBuf::from(p)
        }
    }

    /// Whether the temp directory's filesystem ignores case (APFS and NTFS by default).
    pub(crate) fn case_insensitive_tmp() -> bool {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("Probe")).unwrap();
        t.path().join("PROBE").exists()
    }

    #[test]
    fn prefixes_compare_with_or_without_case() {
        let (a, b) = (abs("/Users/Alice/.dot/x"), abs("/users/alice"));
        assert!(!starts_with_fold(&a, &b, false));
        assert!(starts_with_fold(&a, &b, true));
        assert!(starts_with_fold(&b, &b, true));
        assert!(
            !starts_with_fold(&b, &a, true),
            "a parent is not inside its child"
        );
        assert!(
            !starts_with_fold(&abs("/Users/Alicex"), &b, true),
            "whole components"
        );
        // Unicode names fold too.
        assert!(starts_with_fold(
            &abs("/HOME/ÉLISE/a"),
            &abs("/home/élise"),
            true
        ));
        if cfg!(windows) {
            assert!(starts_with_fold(
                Path::new(r"c:\users\alice\x"),
                Path::new(r"C:\Users\Alice"),
                true
            ));
        }
    }

    #[test]
    fn locations_are_spelled_as_on_disk() {
        let t = tempfile::tempdir().unwrap();
        let home = on_disk(t.path()).unwrap();
        std::fs::create_dir_all(home.join("dotfiles/sub")).unwrap();
        // Missing parts are kept as written, below the part that exists.
        assert_eq!(
            real_location(&home.join("dotfiles/new/file")).unwrap(),
            home.join("dotfiles/new/file")
        );
        if !case_insensitive_tmp() {
            return;
        }
        assert_eq!(
            real_location(&home.join("DOTFILES")).unwrap(),
            home.join("dotfiles")
        );
        assert_eq!(
            real_location(&home.join("DotFiles/SUB/x")).unwrap(),
            home.join("dotfiles/sub/x")
        );
        assert_eq!(
            on_disk(&home.join("DOTFILES/Sub")).unwrap(),
            home.join("dotfiles/sub")
        );
        #[cfg(unix)]
        {
            // A symlink is not followed, but its name is spelled as the directory has it.
            std::os::unix::fs::symlink(home.join("dotfiles"), home.join("Link")).unwrap();
            assert_eq!(
                real_location(&home.join("LINK")).unwrap(),
                home.join("Link")
            );
            assert_eq!(
                real_location(&home.join("Link")).unwrap(),
                home.join("Link")
            );
        }
    }

    #[test]
    fn tilde_expansion() {
        let home = abs("/h");
        assert_eq!(expand_tilde("~", &home), home);
        assert_eq!(expand_tilde("~/a/b", &home), home.join("a/b"));
        assert_eq!(expand_tilde("~x/a", &home), PathBuf::from("~x/a"));
        assert_eq!(expand_tilde("/abs", &home), PathBuf::from("/abs"));
    }

    #[test]
    fn target_must_be_absolute() {
        let home = abs("/h");
        assert!(expand_target("rel/path", &home).is_err());
        assert_eq!(expand_target("~/.x/../y", &home).unwrap(), abs("/h/y"));
        // A path that is absolute only on another OS says so.
        let other = if cfg!(windows) {
            "/usr/local/go"
        } else {
            r"C:\Go"
        };
        let err = expand_target(other, &home).unwrap_err().to_string();
        let why = if cfg!(windows) {
            "has no drive letter"
        } else {
            "is a Windows path"
        };
        assert!(
            err.contains(why) && err.contains("when: { os: ... }"),
            "{err}"
        );
        let err = expand_target("rel/path", &home).unwrap_err().to_string();
        assert!(err.contains("must be an absolute path"), "{err}");
    }

    #[test]
    fn other_os_absolute_paths_are_recognised() {
        assert!(other_os_absolute("/usr/local/go", true).is_some());
        assert!(other_os_absolute(r"\Windows", true).is_some());
        assert!(other_os_absolute("rel", true).is_none());
        assert!(other_os_absolute(r"C:\Go", false).is_some());
        assert!(other_os_absolute(r"\\srv\share\x", false).is_some());
        assert!(other_os_absolute("rel", false).is_none());
        assert!(other_os_absolute("C:rel", false).is_none());
    }

    #[test]
    fn normalize_lexically() {
        assert_eq!(normalize(&abs("/a/./b/../c")), abs("/a/c"));
    }

    #[test]
    fn simplify_leaves_ordinary_paths_alone() {
        assert_eq!(simplify(&abs("/a/b")), abs("/a/b"));
    }

    #[test]
    fn plain_keeps_ordinary_paths_as_given() {
        for p in ["rel/./x/", "/a//b/", "~/x"] {
            assert_eq!(plain(PathBuf::from(p)).as_os_str(), p);
        }
        // Elsewhere than on Windows, `\\?\` means nothing: a file name like any other.
        let verbatim = r"\\?\C:\repo\x";
        let want = if cfg!(windows) {
            r"C:\repo\x"
        } else {
            verbatim
        };
        assert_eq!(plain(PathBuf::from(verbatim)).as_os_str(), want);
        if cfg!(windows) {
            assert_eq!(plain(PathBuf::from(r"C:\a/b")).as_os_str(), r"C:\a/b");
            // Kept verbatim where the ordinary spelling would name something else.
            for p in [r"\\?\C:\x\foo.", r"\\?\C:\d\nul", r"\\?\C:\a\..\b"] {
                assert_eq!(plain(PathBuf::from(p)).as_os_str(), p);
            }
        }
    }

    #[test]
    fn ordinary_spelling_only_when_it_names_the_same_file() {
        let o = ordinary_spelling;
        assert_eq!(o(r"\\?\C:\repo\x").as_deref(), Some(r"C:\repo\x"));
        assert_eq!(o(r"\\?\C:\").as_deref(), Some(r"C:\"));
        assert_eq!(o(r"\\?\c:\x\").as_deref(), Some(r"c:\x\"));
        assert_eq!(o(r"\\?\C:\a.b\.git").as_deref(), Some(r"C:\a.b\.git"));
        assert_eq!(
            o(r"\\?\C:\console\com10\lpt").as_deref(),
            Some(r"C:\console\com10\lpt")
        );
        assert_eq!(o(r"\\?\UNC\srv\share\x").as_deref(), Some(r"\\srv\share\x"));
        assert_eq!(o(r"\\?\UNC\srv\share").as_deref(), Some(r"\\srv\share"));
        // Not verbatim, or not a disk or UNC path.
        for p in [
            r"C:\x",
            "/x",
            r"\\?\Volume{1234}\x",
            r"\\?\C:",
            r"\\?\UNC\srv",
        ] {
            assert_eq!(o(p), None, "{p}");
        }
        // Win32 normalisation would change what these name: trailing dots and spaces, `.` and
        // `..`, empty names, `/`, device names with any extension, and MAX_PATH.
        for p in [
            r"\\?\C:\x\foo.",
            r"\\?\C:\x\foo ",
            r"\\?\C:\x\.\y",
            r"\\?\C:\x\..\y",
            r"\\?\C:\x\\y",
            r"\\?\C:\x/y",
            r"\\?\C:\d\nul",
            r"\\?\C:\d\NUL.txt",
            r"\\?\C:\d\con .log",
            r"\\?\C:\d\Com1",
            r"\\?\C:\d\lpt9.x",
            r"\\?\C:\d\COM¹",
            r"\\?\C:\d\conin$",
            r"\\?\UNC\srv.\share\x",
        ] {
            assert_eq!(o(p), None, "{p}");
        }
        let long = format!(r"\\?\C:\{}", "a".repeat(257));
        assert_eq!(o(&long), None);
        let fits = format!(r"\\?\C:\{}", "a".repeat(256));
        assert_eq!(o(&fits).map(|s| s.len()), Some(259));
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_prefixes_are_simplified() {
        assert_eq!(
            simplify(Path::new(r"\\?\C:\repo\x")),
            PathBuf::from(r"C:\repo\x")
        );
        assert_eq!(
            simplify(Path::new(r"\\?\UNC\srv\share\x")),
            PathBuf::from(r"\\srv\share\x")
        );
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let resolved = resolve_lenient(&canonical).unwrap();
        assert!(
            !resolved.to_string_lossy().starts_with(r"\\?\"),
            "{resolved:?}"
        );
        assert_eq!(resolve_lenient(&simplify(&canonical)).unwrap(), resolved);
    }

    #[test]
    fn lenient_handles_missing_tail() {
        // From the temp directory as the system names it: on macOS under `/var`, a link to
        // `/private/var`, which is followed; on Windows `%TEMP%` may hold an 8.3 short name
        // (`C:\Users\RUNNER~1\...`), which comes out long, as `canonicalize` gives it.
        let dir = tempfile::tempdir().unwrap();
        let real = simplify(&dir.path().canonicalize().unwrap());
        let p = dir.path().join("missing/../also-missing/x");
        assert_eq!(resolve_lenient(&p).unwrap(), real.join("also-missing/x"));
    }

    #[cfg(windows)]
    #[test]
    fn short_names_resolve_to_long_ones() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

        let dir = tempfile::tempdir().unwrap();
        let real = simplify(&dir.path().canonicalize().unwrap());
        let long = real.join("a long directory name");
        std::fs::create_dir(&long).unwrap();
        let wide: Vec<u16> = long.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut buf = vec![0u16; 1024];
        // SAFETY: `wide` is NUL-terminated and `buf` holds the 1024 u16s passed.
        let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), 1024) } as usize;
        assert!(n > 0 && n < buf.len());
        let short = PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]));
        if short.file_name() == long.file_name() {
            eprintln!("8.3 names are off on this volume: nothing to resolve");
            return;
        }
        assert_eq!(
            resolve_lenient(&short.join("missing")).unwrap(),
            long.join("missing")
        );
        assert_eq!(on_disk(&short).unwrap(), long);
    }

    #[cfg(unix)]
    #[test]
    fn lenient_follows_dangling_intermediate_links() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        symlink(real.join("elsewhere/x"), real.join("hop")).unwrap();
        symlink("hop/y", real.join("link")).unwrap();
        assert_eq!(
            resolve_lenient(&real.join("link")).unwrap(),
            real.join("elsewhere/x/y")
        );
        symlink(real.join("loop2"), real.join("loop1")).unwrap();
        symlink(real.join("loop1"), real.join("loop2")).unwrap();
        assert_eq!(resolve_lenient(&real.join("loop1")), None);
    }

    #[test]
    fn display_uses_tilde() {
        let home = abs("/h");
        assert_eq!(display(&abs("/h/.zshrc"), &home), "~/.zshrc");
        assert_eq!(
            display(&abs("/etc/x"), &home),
            abs("/etc/x").display().to_string()
        );
    }

    #[test]
    fn state_dir_honours_the_override_then_the_platform_default() {
        let home = abs("/h");
        let over = abs("/elsewhere/state");
        let env = |pairs: Vec<(&'static str, String)>| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
        };
        let with_override = env(vec![
            (STATE_DIR_ENV, over.display().to_string()),
            ("XDG_STATE_HOME", abs("/x").display().to_string()),
            ("LOCALAPPDATA", abs("/x").display().to_string()),
        ]);
        assert_eq!(state_dir(&home, with_override), over);
        // Relative values are ignored.
        let relative = env(vec![
            (STATE_DIR_ENV, "rel".into()),
            ("XDG_STATE_HOME", "rel".into()),
            ("LOCALAPPDATA", "rel".into()),
        ]);
        let from_home = if cfg!(windows) {
            home.join("AppData").join("Local").join("dot")
        } else {
            home.join(".local/state/dot")
        };
        assert_eq!(state_dir(&home, relative), from_home);
        assert_eq!(state_dir(&home, |_| None), from_home);
        let platform = env(vec![
            ("XDG_STATE_HOME", abs("/xdg").display().to_string()),
            ("LOCALAPPDATA", abs("/lad").display().to_string()),
        ]);
        let want = if cfg!(windows) {
            abs("/lad")
        } else {
            abs("/xdg")
        };
        assert_eq!(state_dir(&home, platform), want.join("dot"));
    }
}
