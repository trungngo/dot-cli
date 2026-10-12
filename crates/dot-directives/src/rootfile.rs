//! Root-owned files for `root: true` entries of `template`, `copy`, `block` and `create`
//! (`/etc/sysctl.d/...`, `/etc/ssh/sshd_config.d/...`): the content is written by this user into
//! a private temporary directory, then put in place with `install -m <mode> -o root -g <root's
//! group>` as root (through `sudo -n`, asked for once, up front: the actions say
//! [`dot_core::Action::needs_sudo`]). `install` creates the file whole, with its owner and mode,
//! so nothing half-written or wrongly owned is ever visible.
//!
//! A file this user cannot read (`/etc/sudoers.d/...`) is read with `sudo -n cat` to compare
//! it; while planning sudo is not primed yet, so such an entry is unknown until applied. A
//! file is satisfied when its content and mode are as wanted and root owns it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use dot_core::RunCtx;
use dot_core::facts::Os;
use dot_platform::exec::{Cmd, System};

use crate::fileutil::{Existing, mode_of};
use crate::syscmd;

/// How root-owned files are read and written on this machine.
#[derive(Debug, Clone)]
pub struct RootFiles {
    pub sys: System,
    /// root's group: `root` on Linux, `wheel` on macOS.
    pub group: &'static str,
    /// The owner a root-owned file has: 0 (tests use their own uid).
    pub uid: u32,
    /// `DOT_SYSROOT`: root-owned files go under this directory instead, written as this user
    /// (no sudo, owners left alone), so a sandbox can apply and converge them.
    pub sysroot: Option<PathBuf>,
}

/// What is at a root-owned destination.
#[derive(Debug)]
pub enum RootExisting {
    /// Read (directly, or with `sudo -n cat`), with its owner.
    Read {
        existing: Existing,
        uid: Option<u32>,
    },
    /// This user cannot read it and sudo is not primed yet (planning).
    Unreadable,
}

impl RootFiles {
    pub fn new(sys: System, os: Os, uid: u32) -> Self {
        Self {
            sys,
            group: if os == Os::Macos { "wheel" } else { "root" },
            uid,
            sysroot: None,
        }
    }

    /// The same, writing under `sysroot` (see [`RootFiles::sysroot`]).
    pub fn with_sysroot(mut self, sysroot: Option<PathBuf>) -> Self {
        self.sysroot = sysroot;
        self
    }

    /// Where a root-owned destination really is: under the sysroot when there is one.
    pub fn map(&self, path: PathBuf) -> PathBuf {
        match &self.sysroot {
            Some(root) => dot_core::paths::rebase(root, &path),
            None => path,
        }
    }

    /// Runs `cmd` as root: through `sudo -n` (unless `dot` is root). Under a sysroot it runs as
    /// this user, and `chown` (to root) is left out.
    pub fn run(&self, cmd: Cmd, ctx: &RunCtx) -> Result<()> {
        if self.sysroot.is_some() {
            if cmd.program == "chown" {
                return Ok(());
            }
            return syscmd::run(&self.sys, &cmd, ctx).map(|_| ());
        }
        syscmd::run(&self.sys, &self.sys.privileged(cmd), ctx).map(|_| ())
    }

    /// `install`'s owner options: root and root's group (none under a sysroot).
    fn owner_args(&self) -> Vec<String> {
        if self.sysroot.is_some() {
            return Vec::new();
        }
        vec![
            "-o".to_string(),
            "root".to_string(),
            "-g".to_string(),
            self.group.to_string(),
        ]
    }

    /// `root: true` where it cannot work: Windows has no root-owned files (an elevated terminal
    /// writes anywhere, and `dot` does not manage ACLs).
    pub fn check_supported(os: Os) -> Result<(), String> {
        if os == Os::Windows {
            return Err("`root: true` is not supported on Windows".into());
        }
        Ok(())
    }

    /// Whether writing needs sudo (not when `dot` runs as root, nor under a sysroot).
    pub fn needs_sudo(&self) -> bool {
        !self.sys.root && self.sysroot.is_none()
    }

    /// What is at `dest`, read through sudo when this user may not read it.
    pub fn read(&self, dest: &Path) -> Result<RootExisting> {
        let uid = owner(dest);
        match Existing::read(dest) {
            Ok(existing) => Ok(RootExisting::Read { existing, uid }),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                let cmd = self
                    .sys
                    .privileged(Cmd::new("cat").arg(dest.display().to_string()));
                match self.sys.run(&cmd) {
                    Ok(out) if out.success() => Ok(RootExisting::Read {
                        existing: Existing::File {
                            bytes: out.stdout.into_bytes(),
                            mode: fs::symlink_metadata(dest).ok().and_then(|m| mode_of(&m)),
                        },
                        uid,
                    }),
                    _ => Ok(RootExisting::Unreadable),
                }
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", dest.display())),
        }
    }

    /// Whether `uid` is root's (as far as this machine's files are concerned).
    pub fn owned(&self, uid: Option<u32>) -> bool {
        self.sysroot.is_some() || uid.is_none_or(|u| u == self.uid)
    }

    /// Writes `bytes` to `dest` as a root-owned file with `mode`.
    pub fn write(&self, dest: &Path, bytes: &[u8], mode: u32, ctx: &RunCtx) -> Result<()> {
        let dir = tempfile::Builder::new()
            .prefix("dot-root-")
            .tempdir()
            .context("creating a temporary directory")?;
        let tmp = dir.path().join("file");
        crate::fileutil::atomic_write(&tmp, bytes, 0o600)?;
        self.install(&tmp, dest, mode, ctx)
    }

    /// Installs the file `src` (readable by this user) as root-owned `dest` with `mode`,
    /// creating missing parent directories (root-owned, 0755) first.
    pub fn install(&self, src: &Path, dest: &Path, mode: u32, ctx: &RunCtx) -> Result<()> {
        if let Some(parent) = dest.parent()
            && fs::symlink_metadata(parent).is_err()
        {
            self.mkdir(parent, 0o755, ctx)?;
        }
        let mut args = vec!["-m".to_string(), format!("{mode:04o}")];
        args.extend(self.owner_args());
        args.extend([src.display().to_string(), dest.display().to_string()]);
        self.run(Cmd::new("install").args(args), ctx)
    }

    /// Creates the directory `dir` (and its parents), root-owned, with `mode` (also applied to an
    /// existing one).
    pub fn mkdir(&self, dir: &Path, mode: u32, ctx: &RunCtx) -> Result<()> {
        let mut args = vec!["-d".to_string(), "-m".to_string(), format!("{mode:04o}")];
        args.extend(self.owner_args());
        args.push(dir.display().to_string());
        self.run(Cmd::new("install").args(args), ctx)
    }

    /// Replaces what is at `dest` (a symlink) before a file is installed there: `install`
    /// would write through it.
    pub fn remove_link(&self, dest: &Path, ctx: &RunCtx) -> Result<()> {
        let is_link = fs::symlink_metadata(dest).is_ok_and(|m| m.file_type().is_symlink());
        if !is_link {
            return Ok(());
        }
        self.run(
            Cmd::new("rm").args(["-f".to_string(), dest.display().to_string()]),
            ctx,
        )
    }
}

/// The owner of `path` (not followed through a symlink), where there are owners.
#[cfg(unix)]
pub fn owner(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).ok().map(|m| m.uid())
}

#[cfg(not(unix))]
pub fn owner(_path: &Path) -> Option<u32> {
    None
}

/// The plan note of a root-owned destination that cannot be read before sudo is primed.
pub fn unreadable_note() -> String {
    "not readable without sudo; compared when applying".to_string()
}

/// The plan note of a file that is not owned by root.
pub fn owner_note(uid: Option<u32>) -> String {
    match uid {
        Some(u) => format!("owner uid {u} -> root"),
        None => "owner -> root".into(),
    }
}

/// The note shown for root-owned entries.
pub const AS_ROOT: &str = "as root";

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use dot_core::{Outcome, Status};
    use dot_platform::exec::Output;

    use crate::testutil::Fixture;
    use crate::testutil_sys::fake_builtins;

    use super::*;

    fn me() -> u32 {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { libc::getuid() }
    }

    /// Builtins whose root-owned files are this user's (tests cannot make root's), with system
    /// commands going to a fake.
    fn builtins() -> (
        crate::Builtins,
        std::sync::Arc<dot_platform::exec::fake::FakeExec>,
    ) {
        let (mut b, fake) = fake_builtins();
        b.root_uid = me();
        (b, fake)
    }

    fn installs(fake: &dot_platform::exec::fake::FakeExec) -> Vec<String> {
        fake.calls()
            .into_iter()
            .filter(|c| c.starts_with("sudo -n install"))
            .collect()
    }

    #[test]
    fn a_root_template_is_installed_through_sudo_and_then_satisfied() {
        let f = Fixture::new(
            "steps:\n  - template:\n      ~/etc/sysctl.d/60-dot.conf: { src: s.j2, root: true }\n",
        );
        f.write_repo("s.j2", "vm.swappiness = {{ 10 }}\n");
        let (b, fake) = builtins();
        let dest = f.home().join("etc/sysctl.d/60-dot.conf");
        let actions = f.plan_by(0, &f.context(), &b).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert!(actions[0].needs_sudo());
        assert_eq!(
            actions[0].describe(),
            "template ~/etc/sysctl.d/60-dot.conf <- s.j2 (new file, 1 line; as root)"
        );
        assert_eq!(f.apply_one(actions[0].as_ref()).unwrap(), Outcome::Changed);
        let calls = fake.calls();
        assert_eq!(
            calls[0],
            format!(
                "sudo -n install -d -m 0755 -o root -g root {}",
                dest.parent().unwrap().display()
            )
        );
        assert!(
            calls[1].starts_with("sudo -n install -m 0644 -o root -g root /")
                && calls[1].ends_with(&format!(" {}", dest.display())),
            "{calls:?}"
        );
        // Once in place (the fake installs nothing: done here), it is satisfied.
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, "vm.swappiness = 10\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
        // Owned by someone else: installed again, as root.
        let (mut other, _) = builtins();
        other.root_uid = me() + 1;
        let actions = f.plan_by(0, &f.context(), &other).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert!(
            actions[0].describe().contains("owner uid"),
            "{}",
            actions[0].describe()
        );
    }

    #[test]
    fn an_unreadable_root_file_is_read_through_sudo_or_unknown() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root reads everything
        }
        let f = Fixture::new(
            "steps:\n  - template:\n      ~/sudoers.d/dot: { src: s.j2, root: true, mode: \"0440\" }\n  - block: { file: ~/hosts, content: \"10.0.0.1 box\", root: true }\n",
        );
        f.write_repo("s.j2", "me ALL=(ALL) NOPASSWD: ALL\n");
        let dest = f.home().join("sudoers.d/dot");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, "me ALL=(ALL) NOPASSWD: ALL\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o000)).unwrap();
        let (b, fake) = builtins();
        // Planning: sudo is not primed, so `sudo -n cat` fails.
        fake.on("sudo -n cat", Output::exit(1, ""));
        let actions = f.plan_by(0, &f.context(), &b).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Unknown);
        assert!(actions[0].needs_sudo());
        assert!(actions[0].describe().contains("not readable without sudo"));
        // Applying: it is, and the file holds what it should; only the mode differs.
        fake.on("sudo -n cat", Output::ok("me ALL=(ALL) NOPASSWD: ALL\n"));
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert!(actions[0].describe().contains("mode 0000 -> 0440"));
        fake.clear_calls();
        f.apply_one(actions[0].as_ref()).unwrap();
        assert_eq!(fake.calls()[0], format!("sudo -n cat {}", dest.display()));
        assert!(installs(&fake)[0].starts_with("sudo -n install -m 0440 -o root -g root "));
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o440)).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Satisfied);

        // A root-owned block file: read through sudo, spliced, installed whole.
        let hosts = f.home().join("hosts");
        fs::write(&hosts, "127.0.0.1 localhost\n").unwrap();
        fs::set_permissions(&hosts, fs::Permissions::from_mode(0o000)).unwrap();
        fake.on("sudo -n cat", Output::ok("127.0.0.1 localhost\n"));
        let actions = f.plan_by(1, &f.context(), &b).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        let d = actions[0].describe();
        assert!(d.ends_with("(adds 3 lines; as root)"), "{d}");
        fake.clear_calls();
        f.apply_one(actions[0].as_ref()).unwrap();
        let installed = installs(&fake);
        assert_eq!(installed.len(), 1, "{installed:?}");
        // The mode is kept.
        assert!(installed[0].starts_with("sudo -n install -m 0000 -o root -g root "));
        fs::set_permissions(&hosts, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn root_copies_and_directories() {
        let f = Fixture::new(
            "steps:\n  - copy:\n      ~/opt/app/conf: { src: conf, root: true }\n  - create:\n      ~/opt/data: { root: true, mode: \"0750\" }\n",
        );
        f.write_repo("conf", "x\n");
        let (b, fake) = builtins();
        let actions = f.plan_by(0, &f.context(), &b).unwrap();
        assert!(actions[0].needs_sudo());
        assert!(
            actions[0]
                .describe()
                .ends_with("(new file, 1 line; as root)")
        );
        f.apply_one(actions[0].as_ref()).unwrap();
        let src = f.repo().join("conf");
        let dest = f.home().join("opt/app/conf");
        assert_eq!(
            installs(&fake),
            [
                format!(
                    "sudo -n install -d -m 0755 -o root -g root {}",
                    dest.parent().unwrap().display()
                ),
                format!(
                    "sudo -n install -m 0644 -o root -g root {} {}",
                    src.display(),
                    dest.display()
                )
            ]
        );
        let actions = f.plan_by(1, &f.context(), &b).unwrap();
        assert_eq!(
            actions[0].describe(),
            "create ~/opt/data (mode 0750, as root)"
        );
        assert!(actions[0].needs_sudo());
        fake.clear_calls();
        f.apply_one(actions[0].as_ref()).unwrap();
        assert_eq!(
            fake.calls(),
            [format!(
                "sudo -n install -d -m 0750 -o root -g root {}",
                f.home().join("opt/data").display()
            )]
        );
        // Running as root: no sudo.
        let (mut b, fake) = builtins();
        b.system.root = true;
        let actions = f.plan_by(1, &f.context(), &b).unwrap();
        assert!(!actions[0].needs_sudo());
        f.apply_one(actions[0].as_ref()).unwrap();
        assert!(fake.calls()[0].starts_with("install -d "));
    }

    #[test]
    fn root_is_refused_on_windows_and_group_follows_the_os() {
        let f = Fixture::new("steps:\n  - template:\n      ~/x: { src: s, root: true }\n");
        f.write_repo("s", "");
        let (b, _) = builtins();
        let mut ctx = f.context();
        ctx.facts.os = Os::Windows;
        let e = format!("{:#}", f.plan_by(0, &ctx, &b).err().unwrap());
        assert!(
            e.contains("`root: true` is not supported on Windows"),
            "{e}"
        );
        assert_eq!(b.root_files(Os::Macos).group, "wheel");
        assert_eq!(b.root_files(Os::Linux).group, "root");
    }
}
