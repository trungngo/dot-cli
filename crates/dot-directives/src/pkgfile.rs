//! Installing a package from a file: a `.deb` through apt, a `.rpm` through dnf or zypper (for
//! `download: { package }` and `github_release: { package }`). The package manager resolves its
//! dependencies, and knows the package afterwards (`dpkg-query` / `rpm -q`), so it is
//! upgraded and removed like any other.

use std::io;
use std::path::Path;

use anyhow::Result;
use dot_core::config::PkgManager;
use dot_core::{Facts, Resource, RunCtx, Undo};
use dot_platform::exec::{Cmd, System};

use crate::package::native_manager;
use crate::syscmd;

/// A package installed from a file on this machine.
#[derive(Debug, Clone)]
pub struct PkgFile {
    pub manager: PkgManager,
    /// The package's name, as the package database knows it.
    pub name: String,
    pub sys: System,
}

impl PkgFile {
    /// For this machine's package manager; `Err` says why there is none that installs files.
    pub fn for_facts(facts: &Facts, name: &str, sys: &System) -> Result<PkgFile, String> {
        match native_manager(facts) {
            Some(m @ (PkgManager::Apt | PkgManager::Dnf | PkgManager::Zypper)) => Ok(PkgFile {
                manager: m,
                name: name.to_string(),
                sys: sys.clone(),
            }),
            other => Err(format!(
                "`package` installs a .deb with apt or a .rpm with dnf or zypper; this machine's \
                 package manager is {}: add a `when:` to the step",
                other.map_or("unknown", PkgManager::as_str)
            )),
        }
    }

    /// The file extension the manager installs: `deb` or `rpm`.
    pub fn ext(&self) -> &'static str {
        match self.manager {
            PkgManager::Apt => "deb",
            _ => "rpm",
        }
    }

    /// The installed version, `None` when the package is not installed. `Err(NotFound)` when
    /// the package database tool is missing.
    pub fn installed(&self) -> io::Result<Option<String>> {
        let cmd = match self.manager {
            PkgManager::Apt => {
                Cmd::new("dpkg-query").args(["-W", "-f=${Status}\t${Version}", self.name.as_str()])
            }
            _ => Cmd::new("rpm").args(["-q", "--qf", "%{VERSION}-%{RELEASE}", &self.name]),
        };
        let out = self.sys.run(&cmd)?;
        if !out.success() {
            return Ok(None);
        }
        Ok(match self.manager {
            PkgManager::Apt => {
                let line = out.stdout.trim();
                let (status, version) = line.split_once('\t').unwrap_or((line, ""));
                // `deinstall ok config-files`: removed, its configuration kept.
                status
                    .ends_with(" installed")
                    .then(|| version.trim().to_string())
            }
            _ => Some(out.stdout.trim().to_string()),
        })
    }

    /// `pkg:<manager>:<name>`, as the `package` directive records it: `dot remove --packages`
    /// uninstalls it (when `dot` installed it).
    pub fn resource(&self) -> Resource {
        Resource::new("pkg", format!("{}:{}", self.manager.as_str(), self.name)).with_undo(
            Undo::Uninstall {
                manager: self.manager.as_str().to_string(),
                name: self.name.clone(),
            },
        )
    }

    /// Whether installing needs sudo.
    pub fn needs_sudo(&self) -> bool {
        !self.sys.root
    }

    /// The command that installs `file` (an absolute path) as root, non-interactively.
    pub fn install_cmd(&self, file: &Path) -> Cmd {
        let f = file.display().to_string();
        let cmd = match self.manager {
            // A path (it has a `/`) is a local package to apt.
            PkgManager::Apt => Cmd::new("apt-get")
                .args(["install", "-y", &f])
                .env("DEBIAN_FRONTEND", "noninteractive"),
            PkgManager::Dnf => Cmd::new("dnf").args(["install", "-y", &f]),
            _ => Cmd::new("zypper").args([
                "--non-interactive",
                "install",
                "--allow-unsigned-rpm",
                &f,
            ]),
        };
        self.sys.privileged(cmd)
    }

    /// Installs `file`, holding the package-manager lock.
    pub fn install(&self, file: &Path, ctx: &RunCtx) -> Result<()> {
        let _lock = dot_platform::pkg::lock();
        syscmd::run(&self.sys, &self.install_cmd(file), ctx).map(|_| ())
    }
}

/// Whether an installed package `version` is release `tag` (`v1.2.3` and `1.2.3-1` are).
/// Separators count as one (`-`, `~`, `+`, `_`, `.`): a Debian revision is spelt differently in
/// the tag, the file name and dpkg's answer (`1.3.1-0-ppa2`, `1.3.1-0.ppa2`, `1.3.1-0~ppa2`).
pub fn version_is(version: &str, tag: &str) -> bool {
    let unify = |s: &str| s.replace(['-', '+', '~', '_'], ".");
    let tag = unify(tag.trim_start_matches(['v', 'V']));
    let version = unify(version.split_once(':').map_or(version, |(_, v)| v));
    !tag.is_empty()
        && version
            .strip_prefix(&tag)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_match_tags() {
        assert!(version_is("1.2.3", "v1.2.3"));
        assert!(version_is("1.2.3-1", "1.2.3"));
        assert!(version_is("1:1.2.3-0ubuntu1", "v1.2.3"));
        assert!(!version_is("1.2.30", "1.2.3"));
        assert!(!version_is("1.2.3", ""));
        // Ghostty's PPA-style revision, spelt three ways.
        assert!(version_is("1.3.1-0~ppa2", "1.3.1-0-ppa2"));
        assert!(version_is("1.3.1-0.ppa2", "v1.3.1-0-ppa2"));
        assert!(!version_is("1.3.1-0~ppa1", "1.3.1-0-ppa2"));
    }
}
