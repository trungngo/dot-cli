//! `github_release: { repo, package: <name> }` (GitHub or GitLab): the latest release's `.deb`
//! (apt) or `.rpm`
//! (dnf, zypper) for this architecture, installed through the package manager (see
//! [`crate::pkgfile`]). Satisfied while the package is installed at the latest release: the
//! version recorded when `dot` installed it, or an installed version that is the release's tag.
//! The asset is checked against the release's published SHA-256, like executables are.

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use dot_core::config::GithubReleaseSpec;
use dot_core::{Action, Diff, Outcome, PlanCtx, Resource, RunCtx, Status};
use dot_platform::exec::System;

use super::api::Github;
use super::state::{Installed, StateFile};
use super::{Origin, Pin, Resolved, asset, asset_glob, download_asset, verify_asset};
use crate::pkgfile::{PkgFile, version_is};

pub(super) fn plan(
    spec: &GithubReleaseSpec,
    origin: &Origin,
    name: &str,
    ctx: &PlanCtx<'_>,
    gh: &Github,
    sys: &System,
) -> Result<Vec<Box<dyn Action>>> {
    let name = ctx.render(name)?;
    let pkg = PkgFile::for_facts(ctx.facts, &name, sys)
        .map_err(|e| ctx.config_error(format!("github_release: {e}")))?;
    let repo = &origin.show();
    let glob = asset_glob(spec.asset.as_ref(), ctx)?;
    let token = origin.token(spec.token.as_ref(), ctx, gh)?;
    let pin = Pin::plan(spec, ctx)?;
    let release = match origin.release(gh, pin.tag.as_deref(), token.as_ref()) {
        Err(e) => Err(format!("{e:#}")),
        Ok(release) => {
            let chosen = asset::select_package(
                &release.assets,
                glob.as_ref().map(|(p, m)| (p.as_str(), m)),
                pkg.ext(),
                ctx.facts.arch,
            )
            .map_err(|e| anyhow::anyhow!("{repo} {}: {e}", release.tag_name))?;
            Ok(Resolved {
                tag: release.tag_name.clone(),
                checksum: asset::checksum_source(&release.assets, chosen),
                asset: chosen.clone(),
                format: dot_core::config::Extract::Raw,
                sha256: pin.sha256.clone(),
            })
        }
    };
    let state_dir = gh
        .state_dir
        .clone()
        .unwrap_or_else(|| ctx.state_dir.to_path_buf());
    Ok(vec![Box::new(PackageReleaseAction {
        repo: repo.to_string(),
        pkg,
        release,
        state: StateFile::in_dir(&state_dir),
        gh: gh.clone(),
    })])
}

pub struct PackageReleaseAction {
    repo: String,
    pkg: PkgFile,
    /// `Err`: the latest release could not be looked up.
    release: std::result::Result<Resolved, String>,
    state: StateFile,
    gh: Github,
}

impl PackageReleaseAction {
    /// Where the install is recorded in the state file (which is keyed by directory).
    fn key(&self) -> PathBuf {
        PathBuf::from(format!("package:{}", self.pkg.name))
    }

    /// The installed version (`None`: not installed; `Err`: the database cannot be asked yet).
    fn installed(&self) -> Result<Option<Option<String>>> {
        match self.pkg.installed() {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("querying the package database"),
        }
    }

    /// Whether `version` (installed) is the release `r`.
    fn current(&self, r: &Resolved, version: &str) -> bool {
        let recorded = self.state.get(&self.repo, &self.key());
        recorded.is_some_and(|i| i.tag == r.tag && i.asset == r.asset.name)
            || version_is(version, &r.tag)
    }
}

impl Action for PackageReleaseAction {
    fn describe(&self) -> String {
        let base = format!(
            "package {} from the latest release of {} ({})",
            self.pkg.name,
            self.repo,
            self.pkg.manager.as_str()
        );
        let installed = self.installed().ok().flatten().flatten();
        let note = match (&self.release, &installed) {
            (Err(e), Some(v)) => format!("{v} installed; latest release unknown ({e}): keeping it"),
            (Err(e), None) => format!("latest release unknown: {e}"),
            (Ok(r), None) => format!("install {} ({})", r.tag, r.asset.name),
            (Ok(r), Some(v)) if self.current(r, v) => format!("{v} installed"),
            (Ok(r), Some(v)) => format!("update {v} -> {} ({})", r.tag, r.asset.name),
        };
        format!("{base} ({note})")
    }

    fn status(&self) -> Result<Status> {
        let Ok(r) = &self.release else {
            // Offline or rate limited: an installed package is kept (as `download` keeps what
            // is there), instead of failing the bundle.
            return Ok(match self.installed() {
                Ok(Some(Some(_))) => Status::Satisfied,
                _ => Status::Unknown,
            });
        };
        Ok(match self.installed()? {
            None => Status::Unknown,
            Some(None) => Status::Pending,
            Some(Some(v)) if self.current(r, &v) => Status::Satisfied,
            Some(Some(_)) => Status::Pending,
        })
    }

    fn needs_sudo(&self) -> bool {
        self.pkg.needs_sudo()
    }

    /// `pkg:<manager>:<name>`, like any package (`dot remove --packages`).
    fn resources(&self) -> Vec<Resource> {
        vec![self.pkg.resource()]
    }

    fn diff(&self) -> Result<Option<Diff>> {
        let (Ok(r), Some(installed)) = (&self.release, self.installed()?) else {
            return Ok(None);
        };
        let name = self.pkg.name.clone();
        let manager = self.pkg.manager.as_str().to_string();
        Ok(match installed {
            None => Some(Diff::Packages {
                manager,
                missing: vec![name],
                outdated: Vec::new(),
            }),
            Some(v) if !self.current(r, &v) => Some(Diff::Packages {
                manager,
                missing: Vec::new(),
                outdated: vec![format!("{name} {v} -> {}", r.tag)],
            }),
            Some(_) => None,
        })
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let r = match &self.release {
            Ok(r) => r,
            Err(e) => bail!("{}: cannot look up the latest release: {e}", self.repo),
        };
        let staging = tempfile::Builder::new()
            .prefix("dot-release-")
            .tempdir()
            .context("creating a temporary directory")?;
        let file = staging
            .path()
            .join(format!("{}.{}", self.pkg.name, self.pkg.ext()));
        let (file, digest) = download_asset(&self.gh, r, &file, ctx)?;
        verify_asset(&self.gh, r, &digest, ctx)?;
        // The package manager may read it as another user (apt's `_apt`).
        crate::fileutil::set_mode(staging.path(), 0o755)?;
        crate::fileutil::set_mode(&file, 0o644)?;
        self.pkg.install(&file, ctx)?;
        self.state.record(Installed {
            repo: self.repo.clone(),
            dir: self.key(),
            tag: r.tag.clone(),
            asset: r.asset.name.clone(),
            bins: Vec::new(),
        })?;
        Ok(Outcome::Changed)
    }
}
