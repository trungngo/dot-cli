//! Windows package managers: winget and scoop. Windows has no sudo: scoop installs per user;
//! winget packages installed `--scope machine` need an elevated terminal, which the run checks
//! up front (the `package` action says it needs privileges); any other winget package whose
//! installer turns out to need one fails with a message saying so.

use std::sync::Arc;

use anyhow::{Result, bail};
use dot_core::config::PkgManager;

use super::{InstallOpts, Outdated, PackageManager, membership, pick_outdated, query, table_rows};
use crate::exec::{Cmd, OutFn, Runner};

pub struct Winget(pub(super) Arc<dyn Runner>);

const AGREE: [&str; 2] = ["--accept-source-agreements", "--disable-interactivity"];

impl PackageManager for Winget {
    fn kind(&self) -> PkgManager {
        PkgManager::Winget
    }

    fn available(&self) -> bool {
        self.0.on_path("winget")
    }

    /// `winget list --exact --id <id>` per package: winget has no multi-package query with a
    /// stable output (its tables truncate ids to the console width). Found: exits 0 and lists
    /// the id.
    fn installed(&self, packages: &[String]) -> Result<Vec<bool>> {
        packages
            .iter()
            .map(|id| {
                let cmd = Cmd::new("winget")
                    .args(["list", "--exact", "--id", id.as_str()])
                    .args(AGREE);
                let out = self
                    .0
                    .output(&cmd)
                    .map_err(|e| anyhow::anyhow!("cannot run `{}`: {e}", cmd.display()))?;
                Ok(out.success()
                    && out
                        .stdout
                        .to_ascii_lowercase()
                        .contains(&id.to_ascii_lowercase()))
            })
            .collect()
    }

    /// `winget install --exact --id <id> --silent` per package, agreements accepted;
    /// `--scope machine` for machine-wide installs.
    fn install(&self, packages: &[String], opts: InstallOpts, out: &mut OutFn<'_>) -> Result<()> {
        for id in packages {
            let mut cmd =
                Cmd::new("winget").args(["install", "--exact", "--id", id.as_str(), "--silent"]);
            if opts.machine {
                cmd = cmd.args(["--scope", "machine"]);
            }
            let cmd = cmd.arg("--accept-package-agreements").args(AGREE);
            let o = self
                .0
                .stream(&cmd, out)
                .map_err(|e| anyhow::anyhow!("cannot run `{}`: {e}", cmd.display()))?;
            if !o.success() {
                bail!(
                    "`{}` {}{}; if its installer needs administrator rights, run dot from an \
                     elevated (Administrator) terminal",
                    cmd.display(),
                    o.describe_exit(),
                    o.last_line().map(|l| format!(": {l}")).unwrap_or_default()
                );
            }
        }
        Ok(())
    }

    /// `winget list --exact --id <id> --upgrade-available` per package: a table row with the id,
    /// its version and the available one when there is an upgrade.
    fn outdated(&self, packages: &[String]) -> Result<Vec<Outdated>> {
        let mut out = Vec::new();
        for id in packages {
            let cmd = Cmd::new("winget")
                .args([
                    "list",
                    "--exact",
                    "--id",
                    id.as_str(),
                    "--upgrade-available",
                ])
                .args(AGREE);
            let o = self
                .0
                .output(&cmd)
                .map_err(|e| anyhow::anyhow!("cannot run `{}`: {e}", cmd.display()))?;
            if !o.success() {
                // winget exits non-zero when nothing matches: no upgrade.
                continue;
            }
            if let Some((cur, new)) = parse_winget_upgrade_row(&o.stdout, id) {
                out.push(Outdated::new(id.clone(), cur, new));
            }
        }
        Ok(out)
    }

    /// `winget upgrade --exact --id <id> --silent` per package.
    fn upgrade(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        for id in packages {
            let cmd = Cmd::new("winget")
                .args(["upgrade", "--exact", "--id", id.as_str(), "--silent"])
                .arg("--accept-package-agreements")
                .args(AGREE);
            super::run(self.0.as_ref(), &cmd, out)?;
        }
        Ok(())
    }

    /// `winget uninstall --exact --id <id> --silent` per package.
    fn uninstall(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        for id in packages {
            let cmd = Cmd::new("winget")
                .args(["uninstall", "--exact", "--id", id.as_str(), "--silent"])
                .args(AGREE);
            super::run(self.0.as_ref(), &cmd, out)?;
        }
        Ok(())
    }

    fn needs_root(&self) -> bool {
        false
    }
}

/// The installed and available versions in the row of `id` of a `winget list
/// --upgrade-available` table (`Name  Id  Version  Available  Source`; names may have spaces).
pub fn parse_winget_upgrade_row(text: &str, id: &str) -> Option<(String, String)> {
    table_rows(text).into_iter().find_map(|row| {
        let i = row.iter().position(|w| w.eq_ignore_ascii_case(id))?;
        let (cur, new) = (row.get(i + 1)?, row.get(i + 2)?);
        (cur != new).then(|| (cur.to_string(), new.to_string()))
    })
}

pub struct Scoop(pub(super) Arc<dyn Runner>);

impl PackageManager for Scoop {
    fn kind(&self) -> PkgManager {
        PkgManager::Scoop
    }

    fn available(&self) -> bool {
        self.0.on_path("scoop")
    }

    /// `scoop list`: a table of every installed app, under a line of dashes.
    fn installed(&self, packages: &[String]) -> Result<Vec<bool>> {
        let out = query(self.0.as_ref(), &Cmd::new("scoop").arg("list"), &[0])?;
        let found: Vec<String> = table_rows(&out.stdout)
            .into_iter()
            .map(|r| r[0].to_string())
            .collect();
        Ok(membership(packages, &found))
    }

    /// `scoop status`: a table (`Name  Installed Version  Latest Version  ...`) of the apps
    /// with an update.
    fn outdated(&self, packages: &[String]) -> Result<Vec<Outdated>> {
        let out = query(self.0.as_ref(), &Cmd::new("scoop").arg("status"), &[0])?;
        let found = table_rows(&out.stdout)
            .into_iter()
            .filter(|r| r.len() >= 3 && r[1] != r[2])
            .map(|r| (r[0].to_string(), r[1].to_string(), r[2].to_string()))
            .collect();
        Ok(pick_outdated(packages, found))
    }

    /// `scoop update <names>`.
    fn upgrade(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        let cmd = Cmd::new("scoop")
            .arg("update")
            .args(packages.iter().cloned());
        super::run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    /// `scoop install <names>` (per user, no elevation).
    fn install(&self, packages: &[String], _: InstallOpts, out: &mut OutFn<'_>) -> Result<()> {
        let cmd = Cmd::new("scoop")
            .arg("install")
            .args(packages.iter().cloned());
        super::run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    /// `scoop uninstall <names>`.
    fn uninstall(&self, packages: &[String], out: &mut OutFn<'_>) -> Result<()> {
        let cmd = Cmd::new("scoop")
            .arg("uninstall")
            .args(packages.iter().cloned());
        super::run(self.0.as_ref(), &cmd, out).map(|_| ())
    }

    fn needs_root(&self) -> bool {
        false
    }
}
