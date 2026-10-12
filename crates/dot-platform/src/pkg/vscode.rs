//! VS Code extensions as packages (`package: { vscode: [...] }`), on every OS.

use std::sync::Arc;

use anyhow::Result;
use dot_core::config::PkgManager;

use super::{InstallOpts, Outdated, PackageManager, query};
use crate::exec::{Cmd, OutFn, Runner};

/// VS Code extensions, through the `code` command line on any OS (`code.cmd` on Windows).
/// Extensions update themselves, so nothing is ever outdated here.
pub struct Vscode(pub(super) Arc<dyn Runner>);

impl PackageManager for Vscode {
    fn kind(&self) -> PkgManager {
        PkgManager::Vscode
    }

    fn available(&self) -> bool {
        self.0.on_path("code")
    }

    /// `code --list-extensions`: one id per line; ids compare without case.
    fn installed(&self, packages: &[String]) -> Result<Vec<bool>> {
        let out = query(
            self.0.as_ref(),
            &Cmd::new("code").arg("--list-extensions"),
            &[0],
        )?;
        let found: Vec<String> = out
            .stdout
            .lines()
            .map(|l| l.trim().to_ascii_lowercase())
            .filter(|l| !l.is_empty())
            .collect();
        Ok(packages
            .iter()
            .map(|p| found.contains(&p.to_ascii_lowercase()))
            .collect())
    }

    fn outdated(&self, _packages: &[String]) -> Result<Vec<Outdated>> {
        Ok(Vec::new())
    }

    fn upgrade(&self, _packages: &[String], _out: &mut OutFn<'_>) -> Result<()> {
        Ok(())
    }

    /// `code --install-extension <id>` for each.
    fn install(&self, packages: &[String], _: InstallOpts, out: &mut OutFn<'_>) -> Result<()> {
        let mut cmd = Cmd::new("code");
        for p in packages {
            cmd = cmd.args(["--install-extension", p.as_str()]);
        }
        super::run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    /// `code --uninstall-extension <id>` for each.
    fn uninstall(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        let mut cmd = Cmd::new("code");
        for p in packages {
            cmd = cmd.args(["--uninstall-extension", p.as_str()]);
        }
        super::run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    fn needs_root(&self) -> bool {
        false
    }
}
