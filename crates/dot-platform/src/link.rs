//! Creating links through a [`LinkStrategy`]: how a `link` destination is made on this machine,
//! and how the `link`, `clean` and `copy` directives remove one.
//!
//! Everywhere symbolic links can be made (Unix; Windows with Developer Mode on or the
//! `SeCreateSymbolicLinkPrivilege`, e.g. an elevated console) a link is a symlink. Otherwise the
//! fallbacks are a directory *junction* for a directory (NTFS, no privilege needed) and a *copy*
//! for a file. Which one applies is decided by [`LinkCaps::kind_for`], a pure function of the
//! capabilities, so the choice is tested on every host; [`LinkCaps::probe`] finds the
//! capabilities with a real attempt in a temporary directory, once per run.
//!
//! A fallback is not what a reader of `~` would take for a link (a copy is a plain file), so the
//! `link` directive records every fallback it makes in the state directory; see
//! `dot_directives::link`.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// How a link destination is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LinkKind {
    Symlink,
    /// A directory junction (Windows); a plain directory symlink elsewhere, where junctions
    /// only ever come up in tests of the fallback logic.
    Junction,
    /// A copy of the source file: symlinks are not available.
    Copy,
}

impl LinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Symlink => "symlink",
            LinkKind::Junction => "junction",
            LinkKind::Copy => "copy",
        }
    }

    pub fn parse(s: &str) -> Option<LinkKind> {
        [LinkKind::Symlink, LinkKind::Junction, LinkKind::Copy]
            .into_iter()
            .find(|k| k.as_str() == s)
    }

    /// Whether this is a fallback for a symlink (recorded in state, shown in plans).
    pub fn is_fallback(self) -> bool {
        self != LinkKind::Symlink
    }

    /// The plan note for a fallback.
    pub fn note(self) -> Option<&'static str> {
        match self {
            LinkKind::Symlink => None,
            LinkKind::Junction => Some("junction (symlinks unavailable)"),
            LinkKind::Copy => Some("copied (symlinks unavailable)"),
        }
    }
}

impl fmt::Display for LinkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What this machine can make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkCaps {
    /// Symbolic links (files and directories).
    pub symlinks: bool,
    /// Directory junctions (Windows, NTFS).
    pub junctions: bool,
}

impl LinkCaps {
    /// Unix: symlinks, always.
    pub const UNIX: LinkCaps = LinkCaps {
        symlinks: true,
        junctions: false,
    };

    /// How a link to a source that is (`src_is_dir`) or is not a directory is made: a symlink
    /// when possible, else a junction for a directory and a copy for a file. `Err` when neither
    /// works for a directory (Windows without Developer Mode on a filesystem without
    /// junctions); copying a whole directory would silently stop tracking the repository.
    pub fn kind_for(self, src_is_dir: bool) -> Result<LinkKind, String> {
        match (self.symlinks, src_is_dir, self.junctions) {
            (true, _, _) => Ok(LinkKind::Symlink),
            (false, true, true) => Ok(LinkKind::Junction),
            (false, true, false) => Err("cannot link a directory here: symlinks are not \
                 available (turn on Windows Developer Mode, or run from an elevated terminal) \
                 and neither are junctions"
                .to_string()),
            (false, false, _) => Ok(LinkKind::Copy),
        }
    }

    /// How a link is made from WSL onto a Windows drive to a source in the Linux filesystem
    /// (see `dot_core::folders::wsl_link_to_windows_side`): a copy for a file, since the only
    /// symlink WSL can store there is one Windows programs cannot read; for a directory, `Err`
    /// (copying a tree would silently stop tracking the repository).
    pub fn kind_for_windows_side(src_is_dir: bool) -> Result<LinkKind, String> {
        if src_is_dir {
            Err(
                "cannot link a directory from WSL onto a Windows drive: Windows programs cannot \
                 follow the link WSL would make; use `copy:` for it, or keep the source on the \
                 Windows drive"
                    .to_string(),
            )
        } else {
            Ok(LinkKind::Copy)
        }
    }

    /// The capabilities of this machine, found with a real attempt in a temporary directory (on
    /// Windows; Unix always has symlinks).
    pub fn probe() -> LinkCaps {
        #[cfg(windows)]
        {
            probe_windows()
        }
        #[cfg(not(windows))]
        {
            LinkCaps::UNIX
        }
    }
}

#[cfg(windows)]
fn probe_windows() -> LinkCaps {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "dot-link-probe-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let attempt = || -> io::Result<LinkCaps> {
        fs::create_dir_all(dir.join("target"))?;
        fs::write(dir.join("file"), b"")?;
        let symlinks =
            std::os::windows::fs::symlink_file(dir.join("file"), dir.join("link")).is_ok();
        let junctions = junction::create(dir.join("target"), dir.join("junction")).is_ok();
        Ok(LinkCaps {
            symlinks,
            junctions,
        })
    };
    let caps = attempt().unwrap_or(LinkCaps {
        symlinks: false,
        junctions: true,
    });
    let _ = fs::remove_dir_all(&dir);
    caps
}

/// How the directives make and remove links. The real one is [`SystemLinks`].
pub trait LinkStrategy: Send + Sync + fmt::Debug {
    /// What this machine can make.
    fn caps(&self) -> LinkCaps;

    /// How a link to `src` is made here ([`LinkCaps::kind_for`]).
    fn kind_for(&self, src_is_dir: bool) -> Result<LinkKind, String> {
        self.caps().kind_for(src_is_dir)
    }

    /// Makes `dest` a `kind` of `src`. `dest` must not exist (a copy replaces a file there).
    fn create(&self, kind: LinkKind, src: &Path, dest: &Path) -> io::Result<()>;

    /// Removes the symlink or junction at `link` itself, never what it points to.
    fn remove(&self, link: &Path) -> io::Result<()>;
}

/// The machine's own links: capabilities probed on first use (or fixed, for tests).
#[derive(Default)]
pub struct SystemLinks {
    caps: OnceLock<LinkCaps>,
}

impl fmt::Debug for SystemLinks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemLinks")
            .field("caps", &self.caps.get())
            .finish()
    }
}

impl SystemLinks {
    /// Capabilities probed on first use.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fixed capabilities: tests of the fallbacks on any host.
    pub fn with_caps(caps: LinkCaps) -> Self {
        let s = Self::default();
        let _ = s.caps.set(caps);
        s
    }
}

impl LinkStrategy for SystemLinks {
    fn caps(&self) -> LinkCaps {
        *self.caps.get_or_init(LinkCaps::probe)
    }

    fn create(&self, kind: LinkKind, src: &Path, dest: &Path) -> io::Result<()> {
        match kind {
            LinkKind::Symlink => symlink(src, dest),
            LinkKind::Junction => junction(src, dest),
            LinkKind::Copy => copy_file(src, dest),
        }
    }

    fn remove(&self, link: &Path) -> io::Result<()> {
        remove_link(link)
    }
}

fn symlink(src: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, dest)
    }
    #[cfg(windows)]
    {
        if src.is_dir() {
            std::os::windows::fs::symlink_dir(src, dest)
        } else {
            std::os::windows::fs::symlink_file(src, dest)
        }
    }
}

fn junction(src: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        // Junction targets must be absolute; `src` always is (resolved from the bundle).
        junction::create(src, dest)
    }
    #[cfg(not(windows))]
    {
        symlink(src, dest)
    }
}

/// Copies `src` over `dest` through a temporary file next to it, so a reader never sees half a
/// file.
fn copy_file(src: &Path, dest: &Path) -> io::Result<()> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp: PathBuf = dest.with_file_name(format!(
        ".{name}.dot-tmp-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let r = fs::copy(src, &tmp).and_then(|_| fs::rename(&tmp, dest));
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// Removes a symlink or junction itself (never what it points to). Windows removes directory
/// symlinks and junctions with `remove_dir`.
fn remove_link(link: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        if fs::remove_file(link).is_ok() {
            return Ok(());
        }
        fs::remove_dir(link)
    }
    #[cfg(not(windows))]
    {
        fs::remove_file(link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: LinkCaps = LinkCaps {
        symlinks: false,
        junctions: false,
    };
    const NO_DEV_MODE: LinkCaps = LinkCaps {
        symlinks: false,
        junctions: true,
    };

    #[test]
    fn strategy_follows_the_capabilities() {
        let dev_mode = LinkCaps {
            symlinks: true,
            junctions: true,
        };
        for caps in [LinkCaps::UNIX, dev_mode] {
            assert_eq!(caps.kind_for(true), Ok(LinkKind::Symlink));
            assert_eq!(caps.kind_for(false), Ok(LinkKind::Symlink));
        }
        assert_eq!(NO_DEV_MODE.kind_for(true), Ok(LinkKind::Junction));
        assert_eq!(NO_DEV_MODE.kind_for(false), Ok(LinkKind::Copy));
        assert_eq!(NONE.kind_for(false), Ok(LinkKind::Copy));
        let e = NONE.kind_for(true).unwrap_err();
        assert!(e.contains("Developer Mode"), "{e}");
    }

    #[test]
    fn the_windows_side_of_wsl_gets_copies() {
        assert_eq!(LinkCaps::kind_for_windows_side(false), Ok(LinkKind::Copy));
        let e = LinkCaps::kind_for_windows_side(true).unwrap_err();
        assert!(e.contains("use `copy:`"), "{e}");
    }

    #[test]
    fn kinds_round_trip_and_notes() {
        for k in [LinkKind::Symlink, LinkKind::Junction, LinkKind::Copy] {
            assert_eq!(LinkKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(LinkKind::Copy.note(), Some("copied (symlinks unavailable)"));
        assert!(!LinkKind::Symlink.is_fallback());
        assert!(LinkKind::Junction.is_fallback());
    }

    #[cfg(unix)]
    #[test]
    fn creates_and_removes_each_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let (file, dir) = (tmp.path().join("f"), tmp.path().join("d"));
        fs::write(&file, "one").unwrap();
        fs::create_dir(&dir).unwrap();
        let links = SystemLinks::with_caps(NO_DEV_MODE);
        assert_eq!(links.caps(), NO_DEV_MODE);
        let copy = tmp.path().join("copy");
        links.create(LinkKind::Copy, &file, &copy).unwrap();
        assert!(
            !fs::symlink_metadata(&copy)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&copy).unwrap(), "one");
        fs::write(&file, "two").unwrap();
        links.create(LinkKind::Copy, &file, &copy).unwrap();
        assert_eq!(fs::read_to_string(&copy).unwrap(), "two", "replaced");
        let j = tmp.path().join("j");
        links.create(LinkKind::Junction, &dir, &j).unwrap();
        assert_eq!(fs::read_link(&j).unwrap(), dir);
        links.remove(&j).unwrap();
        assert!(dir.is_dir(), "the target is kept");
        assert!(fs::symlink_metadata(&j).is_err());
        let names: Vec<_> = fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(names.len(), 3, "no temporary files left: {names:?}");
    }

    #[cfg(windows)]
    #[test]
    fn junctions_link_directories_without_privileges() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("x"), "x").unwrap();
        let j = tmp.path().join("j");
        SystemLinks::new()
            .create(LinkKind::Junction, &dir, &j)
            .unwrap();
        assert_eq!(fs::read_to_string(j.join("x")).unwrap(), "x");
        assert!(fs::symlink_metadata(&j).unwrap().file_type().is_symlink());
        SystemLinks::new().remove(&j).unwrap();
        assert!(dir.join("x").exists());
    }
}
