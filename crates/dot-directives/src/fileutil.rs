//! Shared by the directives that write files (`template`, `copy`, `secret_file`, `block`,
//! `service` units): what is at a destination now, the guard that keeps writes out of the
//! repository and off the home directory, atomic writes whose mode is set before any content
//! lands, and line-diff summaries for plans.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result, bail};
use dot_core::paths::{
    CASE_INSENSITIVE_FS, normalize, on_disk, real_location, simplify, starts_with_fold,
};
use dot_core::{Diff, Written};

/// Mode for a new file when the config gives none.
pub const DEFAULT_FILE_MODE: u32 = 0o644;
/// Mode for a new file holding a secret when the config gives none.
pub const SECRET_FILE_MODE: u32 = 0o600;

/// What is at a destination.
#[derive(Debug, PartialEq, Eq)]
pub enum Existing {
    Missing,
    /// A regular file, with its mode (Unix).
    File {
        bytes: Vec<u8>,
        mode: Option<u32>,
    },
    /// A symlink (not followed), with its target.
    Symlink(PathBuf),
    Dir,
    /// A socket, fifo, device, ...
    Other,
}

impl Existing {
    pub fn read(path: &Path) -> io::Result<Existing> {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Existing::Missing),
            Err(e) => return Err(e),
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            return Ok(Existing::Symlink(fs::read_link(path)?));
        }
        if ft.is_dir() {
            return Ok(Existing::Dir);
        }
        if !ft.is_file() {
            return Ok(Existing::Other);
        }
        Ok(Existing::File {
            bytes: fs::read(path)?,
            mode: mode_of(&meta),
        })
    }

    /// What kind of thing is at `path`, without reading a file's content (its `bytes` are
    /// empty): works on files this user may not read.
    pub fn peek(path: &Path) -> io::Result<Existing> {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Existing::Missing),
            Err(e) => return Err(e),
        };
        let ft = meta.file_type();
        Ok(if ft.is_symlink() {
            Existing::Symlink(fs::read_link(path)?)
        } else if ft.is_dir() {
            Existing::Dir
        } else if ft.is_file() {
            Existing::File {
                bytes: Vec::new(),
                mode: mode_of(&meta),
            }
        } else {
            Existing::Other
        })
    }

    /// The file's bytes when it is a regular file.
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Existing::File { bytes, .. } => Some(bytes),
            _ => None,
        }
    }

    pub fn mode(&self) -> Option<u32> {
        match self {
            Existing::File { mode, .. } => *mode,
            _ => None,
        }
    }

    /// Overwrites the bytes read (for files that may hold a secret).
    pub fn wipe(&mut self) {
        if let Existing::File { bytes, .. } = self {
            bytes.iter_mut().for_each(|b| *b = 0);
            std::hint::black_box(bytes);
        }
    }
}

#[cfg(unix)]
pub fn mode_of(meta: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
pub fn mode_of(_meta: &fs::Metadata) -> Option<u32> {
    None
}

/// Readable or writable by group or others.
pub fn is_loose(mode: u32) -> bool {
    mode & 0o077 != 0
}

/// Whether a file with mode `have` satisfies `want`. Modes are not managed where the platform
/// has none (`have` is `None`).
pub fn mode_ok(have: Option<u32>, want: Option<u32>) -> bool {
    match (have, want) {
        (Some(h), Some(w)) => h == w,
        _ => true,
    }
}

/// Refuses a destination that would put the home directory or the repository at risk, judged on
/// where it really is (its parent directories' symlinks followed): the home directory or a
/// directory above it, the repository root or a directory above it, or anything inside the
/// repository (e.g. `~/.config/nvim/init.lua` once `~/.config/nvim` links into the repo:
/// writing there would overwrite the tracked source).
pub fn guard(dest: &Path, show: &str, home: &Path, root: &Path) -> Result<()> {
    guard_as("write", dest, show, home, root)
}

/// [`guard`], saying what is refused (`write`, `remove`).
///
/// Judged on where the path is on disk ([`real_location`]): parents' symlinks followed and names
/// spelled as the filesystem stores them, and compared case-insensitively where filesystems are
/// ([`CASE_INSENSITIVE_FS`]), so `~/DOTFILES` on macOS or Windows is the repository at
/// `~/dotfiles`, not another path.
pub fn guard_as(verb: &str, dest: &Path, show: &str, home: &Path, root: &Path) -> Result<()> {
    let Some(at) = located(dest) else {
        bail!("refusing to {verb} {show}: its parent directory is a symlink loop");
    };
    if at.parent().is_none() {
        bail!("refusing to {verb} {show}: it is a filesystem root");
    }
    let via = via_note(dest, &at);
    let (home, root) = (on_disk_or_lexical(home), on_disk_or_lexical(root));
    if within(&home, &at) {
        bail!("refusing to {verb} {show}: it is the home directory or one of its parents{via}");
    }
    if within(&root, &at) {
        bail!("refusing to {verb} {show}: it contains the repository{via}");
    }
    if within(&at, &root) {
        bail!("refusing to {verb} {show}: it is inside the repository{via}");
    }
    Ok(())
}

/// `dest`'s [`real_location`]; `None` on a symlink loop. A filesystem root is itself.
pub fn located(dest: &Path) -> Option<PathBuf> {
    if dest.parent().is_none() || dest.file_name().is_none() {
        return Some(simplify(&normalize(dest)));
    }
    real_location(dest)
}

/// A directory the guards compare against (home, the repository), as it is on disk; as written
/// when that cannot be told.
pub fn on_disk_or_lexical(p: &Path) -> PathBuf {
    on_disk(p).unwrap_or_else(|| simplify(&normalize(p)))
}

/// Whether `path` is `base` or inside it, as this platform's filesystems compare names.
pub fn within(path: &Path, base: &Path) -> bool {
    starts_with_fold(path, base, CASE_INSENSITIVE_FS)
}

/// ` (it resolves to ...)` when where `dest` really is differs from how it was written by more
/// than case, ` (it is ... on disk)` when only by case, else nothing.
pub fn via_note(dest: &Path, at: &Path) -> String {
    let written = simplify(&normalize(dest));
    if written == at {
        String::new()
    } else if starts_with_fold(&written, at, true) && starts_with_fold(at, &written, true) {
        format!(" (it is {} on disk)", at.display())
    } else {
        format!(
            " (it resolves to {} through a symlinked parent)",
            at.display()
        )
    }
}

/// The directory that keeps this user from writing `dest`: the nearest existing ancestor of its
/// parent (where the file, or the directories missing above it, would be created), when this
/// process may not write there. `None` when it may, or when that cannot be told (not Unix).
/// `dot` writes root-owned files only for entries marked `root: true` (see
/// [`crate::rootfile`]), so planning reports the others as conflicts instead of
/// failing half-way through a run.
pub fn unwritable(dest: &Path) -> Option<PathBuf> {
    let mut dir = dest.parent()?;
    loop {
        match fs::symlink_metadata(dir) {
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => dir = dir.parent()?,
            Err(_) => return None,
        }
    }
    (!may_write(dir)).then(|| dir.to_path_buf())
}

#[cfg(unix)]
fn may_write(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: a valid NUL-terminated path; `access` only reads it.
    unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

#[cfg(not(unix))]
fn may_write(_dir: &Path) -> bool {
    true
}

/// The plan note for [`unwritable`].
pub fn unwritable_note(dir: &Path) -> String {
    format!(
        "conflict: {} is not writable by you; mark the entry `root: true` to write it as root \
         (template, copy, block, create, download), or use a `shell` step with `sudo: true`",
        dir.display()
    )
}

/// Writes `bytes` to `dest` atomically: into a temporary file next to it, created readable by
/// the owner only (so a secret is never exposed, even briefly), then given `mode`, flushed, and
/// renamed over `dest`. A symlink at `dest` is replaced, not followed. Missing parent
/// directories are created.
///
/// Windows has no modes: a `mode` that gives group and others nothing (`0600`) makes the file
/// private by its own protected ACL ([`dot_secrets::private_file`]: the user, `SYSTEM`,
/// `Administrators`), whatever its directory would pass down; any other mode leaves it the ACL
/// its directory gives it.
pub fn atomic_write(dest: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let parent = dest
        .parent()
        .with_context(|| format!("{} has no parent directory", dest.display()))?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "creating {}{}",
            parent.display(),
            root_hint(&parent.join("x"))
        )
    })?;
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(
        ".{name}.dot-tmp-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let result = (|| -> io::Result<()> {
        let mut f = if cfg!(windows) && is_loose(mode) {
            create_inheriting(&tmp)?
        } else {
            dot_secrets::private_file::create(&tmp)?
        };
        f.write_all(bytes)?;
        set_mode(&tmp, mode)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, dest)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        let hint = if e.kind() == io::ErrorKind::PermissionDenied {
            root_hint(dest)
        } else {
            String::new()
        };
        return Err(e).with_context(|| format!("writing {}{hint}", dest.display()));
    }
    Ok(())
}

/// ` (...)` explaining how to write a root-owned file, when `path` cannot be written
/// ([`unwritable`]).
fn root_hint(path: &Path) -> String {
    match unwritable(path) {
        Some(dir) => format!(
            " ({} is not writable by you; mark the entry `root: true` to write it as root)",
            dir.display()
        ),
        None => String::new(),
    }
}

/// A new file with the access its directory passes down (Windows, for a mode that is not
/// private).
fn create_inheriting(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// How a file's `mode` is shown in a plan: the mode where there are modes; on Windows, whether
/// the file is private ([`atomic_write`]) or has its directory's access.
pub fn mode_label(mode: u32, windows: bool) -> String {
    match (windows, is_loose(mode)) {
        (false, _) => format!("mode {mode:04o}"),
        (true, false) => "private: you, SYSTEM and Administrators only".to_string(),
        (true, true) => "access as its directory gives".to_string(),
    }
}

#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// How many lines a change adds and removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineDiff {
    pub added: usize,
    pub removed: usize,
}

impl LineDiff {
    /// A minimal line diff (Myers), counts only.
    pub fn between(old: &str, new: &str) -> LineDiff {
        let a: Vec<&str> = old.lines().collect();
        let b: Vec<&str> = new.lines().collect();
        // Common prefix and suffix cost nothing; only the middle is searched.
        let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
        let (a, b) = (&a[pre..], &b[pre..]);
        let suf = a
            .iter()
            .rev()
            .zip(b.iter().rev())
            .take_while(|(x, y)| x == y)
            .count();
        let (a, b) = (&a[..a.len() - suf], &b[..b.len() - suf]);
        let (n, m) = (a.len(), b.len());
        let d = if n + m > 40_000 {
            // Too big to search cheaply: report it as replaced.
            n + m
        } else {
            edit_distance(a, b)
        };
        LineDiff {
            added: (d + m - n) / 2,
            removed: (d + n - m) / 2,
        }
    }
}

impl std::fmt::Display for LineDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "+{}/-{} lines", self.added, self.removed)
    }
}

/// Myers' O((N+M)D) shortest edit script length (insertions plus deletions).
fn edit_distance(a: &[&str], b: &[&str]) -> usize {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    if max == 0 {
        return 0;
    }
    let off = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    for d in 0..=max as isize {
        let mut k = -d;
        while k <= d {
            let i = (k + off) as usize;
            let mut x = if k == -d || (k != d && v[i - 1] < v[i + 1]) {
                v[i + 1]
            } else {
                v[i - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[i] = x;
            if x >= n && y >= m {
                return d as usize;
            }
            k += 2;
        }
    }
    max
}

/// A plan note for replacing `old` with `new`: a line diff summary for text, or a size for
/// binary content. Content itself is never shown.
pub fn change_note(old: &[u8], new: &[u8]) -> String {
    match (std::str::from_utf8(old), std::str::from_utf8(new)) {
        (Ok(o), Ok(n)) => LineDiff::between(o, n).to_string(),
        _ => format!("binary content differs, {} bytes", new.len()),
    }
}

/// A plan note for a new file.
pub fn new_file_note(new: &[u8]) -> String {
    match std::str::from_utf8(new) {
        Ok(t) => format!("new file, {}", count(t.lines().count(), "line")),
        Err(_) => format!("new file, {}", count(new.len(), "byte")),
    }
}

/// `n` and `noun`, plural unless `n` is 1: `1 line`, `2 lines`.
pub fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// What `dot` wrote at `path`, for [`dot_core::Undo::RemoveFiles`]: with the SHA-256 of what is
/// there now (right after writing).
pub fn written(path: &Path) -> Written {
    Written {
        path: path.to_path_buf(),
        sha256: crate::linkstate::sha256_file(path).ok(),
    }
}

/// What `dot` wrote at `path` when it knows the bytes it wrote (`bytes`): with their SHA-256.
/// For root-owned files, which this user may not be able to read back.
pub fn written_bytes(path: &Path, bytes: &[u8]) -> Written {
    Written {
        path: path.to_path_buf(),
        sha256: Some(sha256_hex(bytes)),
    }
}

/// The SHA-256 of `bytes`, as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether a destination is a pattern (`*`, `?` in a component) that [`expand_dest`] expands.
pub fn is_dest_pattern(dest: &Path) -> bool {
    dest.to_string_lossy().contains(['*', '?'])
}

/// The existing paths `pattern` matches, component by component (`*` any run of characters
/// but a separator, `?` one): `~/.config/JetBrains/*/options/markdown.xml` is that file in
/// every product's directory that exists. The last component may name a file that does not
/// exist yet (it is written); the directories above must exist. Sorted.
pub fn expand_dest(pattern: &Path) -> Vec<PathBuf> {
    let parts: Vec<std::path::Component<'_>> = pattern.components().collect();
    let mut found: Vec<PathBuf> = vec![PathBuf::new()];
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let text = part.as_os_str().to_string_lossy();
        let mut next = Vec::new();
        for base in &found {
            if !text.contains(['*', '?']) {
                let p = base.join(part.as_os_str());
                if last || p.is_dir() {
                    next.push(p);
                }
                continue;
            }
            let Ok(entries) = fs::read_dir(base) else {
                continue;
            };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') && !text.starts_with('.') {
                    continue;
                }
                let p = base.join(&name);
                if dot_platform::pkg::name_matches(&text, &name) && (last || p.is_dir()) {
                    next.push(p);
                }
            }
        }
        found = next;
    }
    found.sort();
    found
}

/// A fingerprint of the tree at `root`: every entry's path, type, size, mode and modification
/// time (a link's target), hashed. Changes when anything in it is added, removed or edited;
/// reads no file contents, so a large tree (a toolchain) is cheap.
pub fn tree_fingerprint(root: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let mut entries: Vec<fs::DirEntry> =
            fs::read_dir(root.join(&rel))?.collect::<io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let path = rel.join(e.file_name());
            let meta = fs::symlink_metadata(e.path())?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            let ft = meta.file_type();
            let kind = if ft.is_symlink() {
                format!("l {}", fs::read_link(e.path())?.display())
            } else if ft.is_dir() {
                stack.push(path.clone());
                "d".to_string()
            } else {
                format!("f {}", meta.len())
            };
            let mode = mode_of(&meta).unwrap_or(0);
            h.update(format!("{}\0{kind}\0{mode:o}\0{mtime}\n", path.display()).as_bytes());
        }
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// `dot diff` for a file: what is at the destination (`existing`, shown as `show`) against
/// `new` (labelled `new_label`), as a unified diff of text; `want_mode`: the mode it should
/// have. `None` when nothing differs. The caller makes sure neither side is secret.
pub fn file_diff(
    show: &str,
    existing: &Existing,
    new: &[u8],
    new_label: &str,
    want_mode: Option<u32>,
) -> Option<Diff> {
    let (old, old_label): (&[u8], &str) = match existing {
        Existing::Missing => (b"", "/dev/null"),
        Existing::File { bytes, .. } => (bytes, show),
        Existing::Symlink(t) => {
            return Some(Diff::Note(format!(
                "{show} is a link to {}: replaced by a file",
                t.display()
            )));
        }
        Existing::Dir => return Some(Diff::Note(format!("{show} is a directory"))),
        Existing::Other => return Some(Diff::Note(format!("{show} is not a regular file"))),
    };
    let mode = match (existing.mode(), want_mode) {
        (Some(have), Some(want)) if have != want => Some(format!("mode {have:04o} -> {want:04o}")),
        _ => None,
    };
    if old == new {
        return mode.map(|m| Diff::Note(format!("{show}: {m}")));
    }
    match (std::str::from_utf8(old), std::str::from_utf8(new)) {
        (Ok(o), Ok(n)) => {
            let mut unified = dot_core::textdiff::unified(o, n, old_label, new_label, 3);
            if let Some(m) = mode {
                unified.push_str(&format!("({m})\n"));
            }
            Some(Diff::File {
                path: show.to_string(),
                unified,
            })
        }
        _ => Some(Diff::Note(format!(
            "{show}: binary content differs ({} bytes -> {} bytes)",
            old.len(),
            new.len()
        ))),
    }
}

/// `show` with `notes` in parentheses, if any.
pub fn with_notes(base: String, notes: &[String]) -> String {
    if notes.is_empty() {
        base
    } else {
        format!("{base} ({})", notes.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_read_as_access_on_windows() {
        assert_eq!(mode_label(0o600, false), "mode 0600");
        assert_eq!(mode_label(0o640, false), "mode 0640");
        assert_eq!(
            mode_label(0o600, true),
            "private: you, SYSTEM and Administrators only"
        );
        assert_eq!(mode_label(0o400, true), mode_label(0o600, true));
        assert_eq!(mode_label(0o644, true), "access as its directory gives");
    }

    #[test]
    fn line_diff_counts() {
        let d = |a: &str, b: &str| {
            let d = LineDiff::between(a, b);
            (d.added, d.removed)
        };
        assert_eq!(d("a\nb\nc\n", "a\nb\nc\n"), (0, 0));
        assert_eq!(d("a\nb\nc\n", "a\nx\nc\n"), (1, 1));
        assert_eq!(d("", "a\nb\n"), (2, 0));
        assert_eq!(d("a\nb\n", ""), (0, 2));
        assert_eq!(d("a\nb\nc\nd\n", "b\nc\nd\ne\n"), (1, 1));
        assert_eq!(d("x\na\ny\nb\n", "a\nb\nz\n"), (1, 2));
        assert_eq!(LineDiff::between("a", "b").to_string(), "+1/-1 lines");
        assert_eq!(
            change_note(b"\xff", b"\xfe\x00"),
            "binary content differs, 2 bytes"
        );
        assert_eq!(new_file_note(b"a\nb"), "new file, 2 lines");
        assert_eq!(new_file_note(b"a\n"), "new file, 1 line");
        assert_eq!(new_file_note(&[0xff]), "new file, 1 byte");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_sets_mode_and_replaces_links() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, "keep").unwrap();
        let dest = dir.path().join("sub/dest");
        atomic_write(&dest, b"one", 0o640).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"one");
        assert_eq!(Existing::read(&dest).unwrap().mode(), Some(0o640));
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            Existing::read(&link).unwrap(),
            Existing::Symlink(target.clone())
        );
        atomic_write(&link, b"two", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        assert_eq!(
            Existing::read(&link).unwrap(),
            Existing::File {
                bytes: b"two".to_vec(),
                mode: Some(0o600)
            }
        );
        // No temporary files left behind.
        let names: Vec<_> = fs::read_dir(dir.path().join("sub"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["dest"]);
    }

    #[test]
    fn guard_refuses_home_and_repo() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("dotfiles");
        fs::create_dir_all(root.join("x")).unwrap();
        let g = |p: &Path| guard(p, "dest", home.path(), &root).map_err(|e| e.to_string());
        assert!(g(&home.path().join(".rc")).is_ok());
        assert!(g(home.path()).unwrap_err().contains("home directory"));
        assert!(g(&root).unwrap_err().contains("contains the repository"));
        assert!(
            g(&root.join("x/y"))
                .unwrap_err()
                .contains("inside the repository")
        );
        let probe = home.path().join("Probe");
        fs::create_dir(&probe).unwrap();
        let case_insensitive = home.path().join("PROBE").exists();
        fs::remove_dir(&probe).unwrap();
        if case_insensitive {
            let upper = |p: &Path| {
                p.parent()
                    .unwrap()
                    .join(p.file_name().unwrap().to_string_lossy().to_uppercase())
            };
            assert!(
                g(&upper(&root))
                    .unwrap_err()
                    .contains("contains the repository")
            );
            assert!(
                g(&upper(home.path()))
                    .unwrap_err()
                    .contains("home directory")
            );
            assert!(
                g(&upper(&root).join("X/y"))
                    .unwrap_err()
                    .contains("inside the repository")
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("x"), home.path().join("cfg")).unwrap();
            let e = g(&home.path().join("cfg/file")).unwrap_err();
            assert!(e.contains("inside the repository"), "{e}");
            assert!(e.contains("symlinked parent"), "{e}");
        }
    }
}
