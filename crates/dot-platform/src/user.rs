//! A user's login shell and groups (`user:`): the queries, their parsers and the commands that
//! change them, per OS. Pure, apart from [`Accounts`], which runs the queries through a
//! [`Runner`], so tests assert argv and script the answers.
//!
//! - Linux: `getent passwd` / `id -nG` / `getent group` to read; `usermod -s` (else `chsh -s`),
//!   `usermod -aG` (else `gpasswd -a`) and `groupadd` to change, as root.
//! - macOS: `dscl . -read /Users/<u> UserShell` / `id -nG` / `dscl . -read /Groups/<g>` to read;
//!   `dscl . -create /Users/<u> UserShell`, `dseditgroup -o edit -a` and `dseditgroup -o create`
//!   to change, as root.
//!
//! A login shell must be listed in `/etc/shells` (chsh refuses others, and some services lock
//! out users whose shell is not); a missing entry is appended.

use std::io;
use std::path::{Path, PathBuf};

use dot_core::facts::Os;

use crate::exec::{Cmd, Runner};

/// The file listing valid login shells.
pub const ETC_SHELLS: &str = "/etc/shells";

/// The login shell in a `getent passwd` line (its 7th field).
pub fn parse_passwd_shell(line: &str) -> Option<String> {
    let f = line.lines().next()?.split(':').nth(6)?.trim();
    (!f.is_empty()).then(|| f.to_string())
}

/// The shell in `dscl . -read /Users/<u> UserShell` output (`UserShell: /bin/zsh`).
pub fn parse_dscl_shell(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let v = l.trim().strip_prefix("UserShell:")?.trim();
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// `id -nG` output: the group names.
pub fn parse_groups(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// Whether `/etc/shells` (its text) lists `shell`.
pub fn shell_listed(etc_shells: &str, shell: &Path) -> bool {
    etc_shells
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .any(|l| Path::new(l) == shell)
}

/// `/etc/shells` with `shell` appended.
pub fn with_shell(etc_shells: &str, shell: &Path) -> String {
    let mut s = etc_shells.to_string();
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(&format!("{}\n", shell.display()));
    s
}

/// Whether a login shell is given as a path. Login shells and `/etc/shells` are Unix's, so a
/// path is a Unix path whatever the host (the functions here are pure, and tested everywhere).
fn is_unix_absolute(shell: &str) -> bool {
    shell.starts_with('/')
}

/// The path of login shell `shell`: an absolute path as is; a name (`zsh`) as `/etc/shells`
/// lists it (the first listed path with that file name that `exists`), else where `which`
/// finds it on `PATH`. `None`: not installed (yet).
pub fn resolve_shell(
    shell: &str,
    etc_shells: &str,
    exists: impl Fn(&Path) -> bool,
    which: impl Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let p = Path::new(shell);
    if is_unix_absolute(shell) {
        return exists(p).then(|| p.to_path_buf());
    }
    etc_shells
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(Path::new)
        .find(|l| l.file_name().is_some_and(|n| n == shell) && exists(l))
        .map(Path::to_path_buf)
        .or_else(|| which(shell))
}

/// Like [`resolve_shell`], for a user whose login shell is `current`: when `shell` is a name
/// and `current` is already a shell of that name listed in `/etc/shells` (and there), that is
/// the answer. Debian and Ubuntu list both `/bin/zsh` and `/usr/bin/zsh`; a user who ran `chsh
/// -s $(which zsh)` has the second, which is zsh as much as the first is (under usrmerge the
/// very same file), so nothing is changed.
pub fn resolve_shell_for(
    shell: &str,
    current: Option<&str>,
    etc_shells: &str,
    exists: impl Fn(&Path) -> bool,
    which: impl Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    if !is_unix_absolute(shell)
        && let Some(c) = current.filter(|c| is_unix_absolute(c)).map(Path::new)
        && c.file_name().is_some_and(|n| n == shell)
        && shell_listed(etc_shells, c)
        && exists(c)
    {
        return Some(c.to_path_buf());
    }
    resolve_shell(shell, etc_shells, exists, which)
}

/// The queries and changes for one user on one OS.
pub struct Accounts<'a> {
    pub os: Os,
    pub runner: &'a dyn Runner,
}

impl Accounts<'_> {
    /// `user`'s login shell. `Ok(None)`: the tool answered, but no shell is set.
    pub fn shell(&self, user: &str) -> io::Result<Option<String>> {
        match self.os {
            Os::Macos => {
                let o = self.runner.output(&Cmd::new("dscl").args([
                    ".",
                    "-read",
                    &format!("/Users/{user}"),
                    "UserShell",
                ]))?;
                if !o.success() {
                    return Err(io::Error::other(format!("no user `{user}` (dscl)")));
                }
                Ok(parse_dscl_shell(&o.stdout))
            }
            _ => {
                let o = self
                    .runner
                    .output(&Cmd::new("getent").args(["passwd", user]))?;
                if !o.success() {
                    return Err(io::Error::other(format!(
                        "no user `{user}` (getent passwd)"
                    )));
                }
                Ok(parse_passwd_shell(&o.stdout))
            }
        }
    }

    /// The groups `user` is in.
    pub fn groups(&self, user: &str) -> io::Result<Vec<String>> {
        let o = self.runner.output(&Cmd::new("id").args(["-nG", user]))?;
        if !o.success() {
            return Err(io::Error::other(format!("no user `{user}` (id -nG)")));
        }
        Ok(parse_groups(&o.stdout))
    }

    /// Whether group `group` exists.
    pub fn group_exists(&self, group: &str) -> io::Result<bool> {
        let cmd = match self.os {
            Os::Macos => Cmd::new("dscl").args([".", "-read", &format!("/Groups/{group}")]),
            _ => Cmd::new("getent").args(["group", group]),
        };
        Ok(self.runner.output(&cmd)?.success())
    }

    /// Makes `shell` `user`'s login shell (run as root).
    pub fn set_shell(&self, user: &str, shell: &Path) -> Cmd {
        let shell = shell.display().to_string();
        match self.os {
            Os::Macos => Cmd::new("dscl").args([
                ".".to_string(),
                "-create".to_string(),
                format!("/Users/{user}"),
                "UserShell".to_string(),
                shell,
            ]),
            _ if self.runner.on_path("usermod") => {
                Cmd::new("usermod").args(["-s", shell.as_str(), user])
            }
            _ => Cmd::new("chsh").args(["-s", shell.as_str(), user]),
        }
    }

    /// Adds `user` to `group` (run as root).
    pub fn add_to_group(&self, user: &str, group: &str) -> Cmd {
        match self.os {
            Os::Macos => {
                Cmd::new("dseditgroup").args(["-o", "edit", "-a", user, "-t", "user", group])
            }
            _ if self.runner.on_path("usermod") => Cmd::new("usermod").args(["-aG", group, user]),
            _ => Cmd::new("gpasswd").args(["-a", user, group]),
        }
    }

    /// Creates group `group` (run as root).
    pub fn create_group(&self, group: &str) -> Cmd {
        match self.os {
            Os::Macos => Cmd::new("dseditgroup").args(["-o", "create", group]),
            _ => Cmd::new("groupadd").arg(group),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::fake::FakeRunner;

    #[test]
    fn parsers() {
        assert_eq!(
            parse_passwd_shell("alice:x:1000:1000:Alice,,,:/home/alice:/bin/bash\n").as_deref(),
            Some("/bin/bash")
        );
        assert_eq!(parse_passwd_shell("alice:x:1000:1000::/home/alice:"), None);
        assert_eq!(
            parse_dscl_shell("UserShell: /bin/zsh\n").as_deref(),
            Some("/bin/zsh")
        );
        assert_eq!(
            parse_groups("alice adm sudo docker\n"),
            ["alice", "adm", "sudo", "docker"]
        );
        let shells = "# /etc/shells: valid login shells\n/bin/sh\n/usr/bin/zsh\n/bin/zsh";
        assert!(shell_listed(shells, Path::new("/bin/zsh")));
        assert!(!shell_listed(shells, Path::new("/opt/homebrew/bin/fish")));
        assert_eq!(
            with_shell(shells, Path::new("/opt/homebrew/bin/fish")),
            format!("{shells}\n/opt/homebrew/bin/fish\n")
        );
        assert_eq!(with_shell("", Path::new("/x")), "/x\n");
    }

    #[test]
    fn shells_resolve_through_etc_shells_then_path() {
        let shells = "/bin/sh\n/usr/bin/zsh\n/bin/zsh\n";
        let exists = |p: &Path| p == Path::new("/bin/zsh") || p == Path::new("/opt/fish");
        let which = |n: &str| (n == "fish").then(|| PathBuf::from("/opt/fish"));
        assert_eq!(
            resolve_shell("zsh", shells, exists, which),
            Some(PathBuf::from("/bin/zsh")),
            "the first listed one that exists"
        );
        assert_eq!(
            resolve_shell("fish", shells, exists, which),
            Some(PathBuf::from("/opt/fish"))
        );
        assert_eq!(resolve_shell("nu", shells, exists, which), None);
        assert_eq!(
            resolve_shell("/opt/fish", shells, exists, which),
            Some(PathBuf::from("/opt/fish"))
        );
        assert_eq!(resolve_shell("/opt/nu", shells, exists, which), None);
    }

    #[test]
    fn a_current_shell_of_the_same_name_is_kept() {
        // Debian's zsh package lists both; the user's is the second one.
        let shells = "/bin/sh\n/bin/zsh\n/usr/bin/zsh\n/bin/bash\n";
        let exists = |p: &Path| {
            ["/bin/zsh", "/usr/bin/zsh", "/bin/bash", "/opt/zsh"]
                .iter()
                .any(|x| p == Path::new(x))
        };
        let which = |_: &str| None;
        let pick = |current: Option<&str>| resolve_shell_for("zsh", current, shells, exists, which);
        assert_eq!(
            pick(Some("/usr/bin/zsh")),
            Some(PathBuf::from("/usr/bin/zsh"))
        );
        assert_eq!(pick(Some("/bin/zsh")), Some(PathBuf::from("/bin/zsh")));
        // Another shell, an unlisted zsh, or none: the first listed zsh.
        assert_eq!(pick(Some("/bin/bash")), Some(PathBuf::from("/bin/zsh")));
        assert_eq!(pick(Some("/opt/zsh")), Some(PathBuf::from("/bin/zsh")));
        assert_eq!(pick(None), Some(PathBuf::from("/bin/zsh")));
        // A path asked for is that path.
        assert_eq!(
            resolve_shell_for("/bin/zsh", Some("/usr/bin/zsh"), shells, exists, which),
            Some(PathBuf::from("/bin/zsh"))
        );
    }

    #[test]
    fn commands_per_os() {
        let r = FakeRunner::with_programs(&["usermod"]);
        let linux = Accounts {
            os: Os::Linux,
            runner: &r,
        };
        assert_eq!(
            linux.set_shell("alice", Path::new("/usr/bin/zsh")).line(),
            "usermod -s /usr/bin/zsh alice"
        );
        assert_eq!(
            linux.add_to_group("alice", "docker").line(),
            "usermod -aG docker alice"
        );
        assert_eq!(linux.create_group("docker").line(), "groupadd docker");
        let bare = FakeRunner::default();
        let linux = Accounts {
            os: Os::Linux,
            runner: &bare,
        };
        assert_eq!(
            linux.set_shell("alice", Path::new("/bin/zsh")).line(),
            "chsh -s /bin/zsh alice"
        );
        assert_eq!(
            linux.add_to_group("alice", "docker").line(),
            "gpasswd -a alice docker"
        );
        let mac = Accounts {
            os: Os::Macos,
            runner: &r,
        };
        assert_eq!(
            mac.set_shell("alice", Path::new("/opt/homebrew/bin/zsh"))
                .line(),
            "dscl . -create /Users/alice UserShell /opt/homebrew/bin/zsh"
        );
        assert_eq!(
            mac.add_to_group("alice", "admin").line(),
            "dseditgroup -o edit -a alice -t user admin"
        );
        assert_eq!(mac.create_group("dev").line(), "dseditgroup -o create dev");
    }

    #[test]
    fn queries() {
        let r = FakeRunner::with_programs(&["getent", "id", "dscl"]);
        r.on(
            "getent passwd alice",
            0,
            "alice:x:1000:1000::/home/alice:/bin/bash\n",
        );
        r.on("getent passwd bob", 2, "");
        r.on("id -nG alice", 0, "alice sudo\n");
        r.on("getent group docker", 2, "");
        r.on(
            "dscl . -read /Users/alice UserShell",
            0,
            "UserShell: /bin/zsh\n",
        );
        let linux = Accounts {
            os: Os::Linux,
            runner: &r,
        };
        assert_eq!(linux.shell("alice").unwrap().as_deref(), Some("/bin/bash"));
        assert!(linux.shell("bob").is_err());
        assert_eq!(linux.groups("alice").unwrap(), ["alice", "sudo"]);
        assert!(!linux.group_exists("docker").unwrap());
        assert!(linux.group_exists("sudo").unwrap());
        let mac = Accounts {
            os: Os::Macos,
            runner: &r,
        };
        assert_eq!(mac.shell("alice").unwrap().as_deref(), Some("/bin/zsh"));
        assert!(mac.group_exists("staff").unwrap());
        assert_eq!(r.lines().last().unwrap(), "dscl . -read /Groups/staff");
    }
}
