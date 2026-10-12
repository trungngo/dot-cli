//! `github_release`: install executables from a GitHub repository's latest release (or, with
//! `source: gitlab`, a GitLab project's: `repo` is then the project's path and `host` the GitLab
//! instance, gitlab.com by default).
//!
//! Planning asks the API for the latest release (cached for the run, so the second planning pass
//! right before applying costs nothing), picks the asset for this OS and architecture (see
//! [`asset`]) and compares the release with the version recorded in the state file: the step is
//! satisfied when that version, from that asset, is installed and every executable is still
//! there. `tag:` pins a release instead of following the latest, and `sha256:` pins the asset's
//! digest (checked instead of the release's own). When the API cannot be reached the status is
//! unknown, not an error, so `dot plan` works offline; applying then fails with the reason.
//!
//! An executable already in the install directory that the state file does not record as
//! installed by `dot` is a conflict: the step fails rather than replace a program the user put
//! there. With `replace: true` it is moved aside to `<name>.dot-backup` first.
//!
//! Applying downloads the asset into a temporary directory next to the install directory
//! (reporting byte progress), checks its SHA-256 against GitHub's published digest or a
//! checksum file from the release when there is one, takes the executables out of it, and
//! renames each into place with mode 0755. `bin:` may map the installed name to a glob over the
//! archive's file names (`godot: 'Godot_v*_linux.x86_64'`). A token (a `token:` secret, else
//! `GITHUB_TOKEN` / `GH_TOKEN`, else the `github_token` secret when `secrets.yml` declares it;
//! for GitLab `GITLAB_TOKEN`) is only sent to the API, never to download hosts, and never logged.

mod api;
mod archive;
mod asset;
mod package;
mod state;

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use dot_core::config::network::{
    GITLAB_HOST, check_bin_name, check_github_repo, check_gitlab_project, check_host_name,
    check_release_tag, check_sha256,
};
use dot_core::config::{AssetSpec, Extract, GithubReleaseSpec, ReleaseSource};
use dot_core::facts::Os;
use dot_core::{Action, Outcome, PlanCtx, Resource, RunCtx, SecretUse, Status, Stream, Undo, Unit};
use sha2::{Digest, Sha256};

pub use api::{
    API_ENV, Asset, GITHUB_API, GITLAB_API_ENV, GITLAB_TOKEN_ENV, Github, Release, TOKEN_ENV,
    TOKEN_SECRET,
};
pub use state::{Installed, StateFile};

pub use archive::{Want, extract as extract_archive};
pub(crate) use asset::ChecksumSource;

use crate::fileutil::{unwritable, unwritable_note};
use crate::link;

/// The default install directory.
pub const DEFAULT_TO: &str = "~/.local/bin";

/// How a file of this name is unpacked (`Raw`: it is not an archive); `None`: a format `dot`
/// cannot unpack.
pub fn asset_format(name: &str) -> Option<Extract> {
    asset::format_of(name)
}

/// Where a release is published: a GitHub repository, or a GitLab project on a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Github(String),
    Gitlab { host: String, project: String },
}

impl Origin {
    /// From a step's `repo`, `source` and `host` (rendered and checked).
    pub fn plan(
        repo: &str,
        source: ReleaseSource,
        host: Option<&str>,
        ctx: &PlanCtx<'_>,
    ) -> Result<Origin> {
        let repo = ctx.render(repo)?;
        match source {
            ReleaseSource::Github => {
                check_github_repo(&repo).map_err(|e| ctx.config_error(e))?;
                Ok(Origin::Github(repo))
            }
            ReleaseSource::Gitlab => {
                check_gitlab_project(&repo).map_err(|e| ctx.config_error(e))?;
                let host = match host {
                    Some(h) => ctx.render(h)?,
                    None => GITLAB_HOST.to_string(),
                };
                check_host_name(&host).map_err(|e| ctx.config_error(e))?;
                Ok(Origin::Gitlab {
                    host,
                    project: repo,
                })
            }
        }
    }

    /// How it is shown and recorded: `owner/name` (GitHub), `gitlab.com/group/name`.
    pub fn show(&self) -> String {
        match self {
            Origin::Github(r) => r.clone(),
            Origin::Gitlab { host, project } => format!("{host}/{project}"),
        }
    }

    /// The project's own name (the last path segment): the default executable name.
    pub fn name(&self) -> &str {
        let path = match self {
            Origin::Github(r) => r,
            Origin::Gitlab { project, .. } => project,
        };
        path.rsplit('/').next().unwrap_or(path)
    }

    /// The latest release (cached for the run).
    pub fn latest(
        &self,
        gh: &Github,
        token: Option<&dot_core::SecretBytes>,
    ) -> Result<Arc<Release>> {
        self.release(gh, None, token)
    }

    /// The release tagged `tag`, or the latest one (cached for the run).
    pub fn release(
        &self,
        gh: &Github,
        tag: Option<&str>,
        token: Option<&dot_core::SecretBytes>,
    ) -> Result<Arc<Release>> {
        match self {
            Origin::Github(r) => gh.release(r, tag, token),
            Origin::Gitlab { host, project } => gh.release_gitlab(host, project, tag, token),
        }
    }

    /// The token for API requests: the step's own token secret (`token_secret`), once resolved
    /// (never while planning), else the environment's (`GITHUB_TOKEN` / `GH_TOKEN`, or
    /// `GITLAB_TOKEN`), else, for GitHub, the repository-wide `github_token` secret.
    pub fn token(
        &self,
        token_secret: Option<&String>,
        ctx: &PlanCtx<'_>,
        gh: &Github,
    ) -> Result<Option<dot_core::SecretBytes>> {
        let (env, fallback) = match self {
            Origin::Github(_) => (&gh.env_token, gh.token_secret.as_ref()),
            Origin::Gitlab { .. } => (&gh.gitlab_token, None),
        };
        let name = token_secret.or(if env.is_none() { fallback } else { None });
        let secret = match name {
            Some(name) => {
                ctx.require_secret(name, SecretUse::TEXT);
                ctx.secret_file(name)?
            }
            None => None,
        };
        Ok(secret.or_else(|| env.clone()))
    }
}

pub fn plan(
    spec: &GithubReleaseSpec,
    ctx: &PlanCtx<'_>,
    gh: &Github,
    sys: &dot_platform::exec::System,
) -> Result<Vec<Box<dyn Action>>> {
    let origin = Origin::plan(&spec.repo, spec.source, spec.host.as_deref(), ctx)?;
    if let Some(name) = &spec.package {
        return package::plan(spec, &origin, name, ctx, gh, sys);
    }
    let repo = origin.show();
    let to = ctx.target(spec.to.as_deref().unwrap_or(DEFAULT_TO))?;
    let os = ctx.facts.os;
    let arch = ctx.facts.arch;
    let entries: Vec<(String, Option<globset::GlobMatcher>)> = if spec.bin.is_empty() {
        vec![(origin.name().to_string(), None)]
    } else {
        spec.bin
            .iter()
            .map(|b| {
                let name = ctx.render(&b.name)?;
                check_bin_name(&name).map_err(|e| ctx.config_error(e))?;
                let glob = b
                    .from
                    .as_deref()
                    .map(|g| {
                        let g = ctx.render(g)?;
                        globset::Glob::new(&g)
                            .map(|g| g.compile_matcher())
                            .map_err(|e| {
                                ctx.config_error(format!("github_release: bin `{name}`: {e}"))
                            })
                    })
                    .transpose()?;
                Ok((name, glob))
            })
            .collect::<Result<_>>()?
    };
    let bins = entries
        .into_iter()
        .map(|(n, glob)| {
            let installed = if os == Os::Windows && !n.to_ascii_lowercase().ends_with(".exe") {
                format!("{n}.exe")
            } else {
                n.clone()
            };
            let mut accepted = vec![n.clone()];
            if installed != n {
                accepted.push(installed.clone());
            }
            Want {
                installed,
                names: accepted,
                glob,
            }
        })
        .collect::<Vec<_>>();
    let glob = asset_glob(spec.asset.as_ref(), ctx)?;
    let token = origin.token(spec.token.as_ref(), ctx, gh)?;
    let pin = Pin::plan(spec, ctx)?;
    let release = match origin.release(gh, pin.tag.as_deref(), token.as_ref()) {
        Err(e) => Err(format!("{e:#}")),
        Ok(release) => {
            let chosen = asset::select(
                &release.assets,
                glob.as_ref().map(|(p, m)| (p.as_str(), m)),
                os,
                arch,
                bins.len() == 1,
            )
            .map_err(|e| anyhow::anyhow!("{repo} {}: {e}", release.tag_name))?;
            let format = match spec.extract {
                Extract::Auto => asset::format_of(&chosen.name).ok_or_else(|| {
                    anyhow::anyhow!(
                        "{repo}: cannot unpack {} (supported: .tar.gz, .tar.xz, .zip, or a plain \
                         executable)",
                        chosen.name
                    )
                })?,
                other => other,
            };
            if format == Extract::Raw && bins.len() > 1 {
                bail!(
                    "{repo}: {} is a single executable, but `bin` names {}",
                    chosen.name,
                    bins.len()
                );
            }
            Ok(Resolved {
                tag: release.tag_name.clone(),
                checksum: asset::checksum_source(&release.assets, chosen),
                asset: chosen.clone(),
                format,
                sha256: pin.sha256.clone(),
            })
        }
    };
    let state_dir = gh
        .state_dir
        .clone()
        .unwrap_or_else(|| ctx.state_dir.to_path_buf());
    Ok(vec![Box::new(ReleaseAction {
        which: pin.which(),
        show_to: ctx.show(&to),
        repo,
        to,
        bins,
        release,
        state: StateFile::in_dir(&state_dir),
        gh: gh.clone(),
        replace: spec.replace,
    })])
}

/// What `download: { release }` takes from a release: an asset, and where its checksum is.
#[derive(Debug, Clone)]
pub(crate) struct PickedAsset {
    pub tag: String,
    pub asset: Asset,
    pub checksum: Option<ChecksumSource>,
}

/// The asset of `release` for this machine: a `.deb` / `.rpm` (`package_ext`), or any file
/// (an archive or a single file).
pub(crate) fn pick_asset(
    release: &Release,
    glob: Option<&(String, globset::GlobMatcher)>,
    package_ext: Option<&str>,
    os: Os,
    arch: &str,
) -> std::result::Result<PickedAsset, String> {
    let glob = glob.map(|(p, m)| (p.as_str(), m));
    let chosen = match package_ext {
        Some(ext) => asset::select_package(&release.assets, glob, ext, arch)?,
        None => asset::select(&release.assets, glob, os, arch, true)?,
    };
    Ok(PickedAsset {
        tag: release.tag_name.clone(),
        checksum: asset::checksum_source(&release.assets, chosen),
        asset: chosen.clone(),
    })
}

/// The SHA-256 a release publishes for `asset_name` (fetching its checksum file if need be),
/// with where it came from; `None`: the release lists none for it.
pub(crate) fn published_sha256(
    gh: &Github,
    checksum: &ChecksumSource,
    asset_name: &str,
) -> Result<Option<(String, String)>> {
    Ok(match checksum {
        ChecksumSource::Digest(hex) => Some((hex.clone(), "the release's asset digest".into())),
        ChecksumSource::File { name, url } => {
            let text = gh
                .download_text(url)
                .with_context(|| format!("fetching checksums from {name}"))?;
            asset::find_checksum(&text, asset_name).map(|h| (h, name.clone()))
        }
    })
}

/// The step's `asset:` glob for this platform, if any.
pub(crate) fn asset_glob(
    asset: Option<&AssetSpec>,
    ctx: &PlanCtx<'_>,
) -> Result<Option<(String, globset::GlobMatcher)>> {
    let Some(a) = asset else {
        return Ok(None);
    };
    let pattern = a
        .for_platform(ctx.facts.os.as_str(), ctx.facts.arch)
        .map_err(|e| ctx.config_error(format!("github_release: {e}")))?;
    let pattern = ctx.render(pattern)?;
    let m = globset::Glob::new(&pattern)
        .map_err(|e| ctx.config_error(format!("github_release: asset `{pattern}`: {e}")))?
        .compile_matcher();
    Ok(Some((pattern, m)))
}

/// A step's `tag:` and `sha256:`, rendered and checked.
pub(crate) struct Pin {
    pub tag: Option<String>,
    pub sha256: Option<String>,
}

impl Pin {
    pub fn plan(spec: &GithubReleaseSpec, ctx: &PlanCtx<'_>) -> Result<Pin> {
        let tag = spec
            .tag
            .as_deref()
            .map(|t| {
                let t = ctx.render(t)?;
                check_release_tag(&t).map_err(|e| ctx.config_error(e))?;
                Ok::<_, anyhow::Error>(t)
            })
            .transpose()?;
        let sha256 = spec
            .sha256
            .as_deref()
            .map(|s| {
                let s = ctx.render(s)?.to_ascii_lowercase();
                check_sha256(&s).map_err(|e| {
                    ctx.config_error(e.replacen("download: ", "github_release: ", 1))
                })?;
                Ok::<_, anyhow::Error>(s)
            })
            .transpose()?;
        Ok(Pin { tag, sha256 })
    }

    /// For messages: `latest release`, `release v1.2.3`.
    pub fn which(&self) -> String {
        match &self.tag {
            Some(t) => format!("release {t}"),
            None => "latest release".into(),
        }
    }
}

/// The release, and what to take from it.
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    tag: String,
    asset: Asset,
    format: Extract,
    checksum: Option<ChecksumSource>,
    /// The step's own `sha256:` for the asset, checked instead of the release's.
    sha256: Option<String>,
}

pub struct ReleaseAction {
    /// `latest release`, or `release <tag>` for a pinned one.
    which: String,
    repo: String,
    to: PathBuf,
    show_to: String,
    bins: Vec<Want>,
    /// `Err`: the latest release could not be looked up (offline, rate limited, ...).
    release: std::result::Result<Resolved, String>,
    state: StateFile,
    gh: Github,
    /// Replace (after backing up) executables `dot` did not install.
    replace: bool,
}

impl ReleaseAction {
    fn names(&self) -> String {
        self.bins
            .iter()
            .map(|w| w.names[0].as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn all_present(&self) -> bool {
        self.bins
            .iter()
            .all(|w| self.to.join(&w.installed).is_file())
    }

    fn installed(&self) -> Option<Installed> {
        self.state.get(&self.repo, &self.to)
    }

    /// Files in the install directory named like one of ours that `dot` did not install
    /// there for this repository (per the state file).
    fn foreign(&self, installed: Option<&Installed>) -> Vec<String> {
        self.bins
            .iter()
            .map(|w| w.installed.clone())
            .filter(|n| !installed.is_some_and(|i| i.bins.contains(n)))
            .filter(|n| std::fs::symlink_metadata(self.to.join(n)).is_ok())
            .collect()
    }

    /// The directory that keeps this user from installing into `to` ([`unwritable`]).
    fn blocked(&self) -> Option<PathBuf> {
        unwritable(&self.to.join(&self.bins.first()?.installed))
    }

    fn foreign_note(&self, foreign: &[String]) -> String {
        let paths: Vec<String> = foreign
            .iter()
            .map(|n| format!("{}/{n}", self.show_to.trim_end_matches(['/', '\\'])))
            .collect();
        if self.replace {
            format!(
                "replaces {}, not installed by dot (kept as {})",
                paths.join(", "),
                link::BACKUP_SUFFIX
            )
        } else {
            format!(
                "conflict: {} {} not installed by dot; set `replace: true` to replace (and back \
                 up) {}",
                paths.join(", "),
                if paths.len() == 1 {
                    "exists and was"
                } else {
                    "exist and were"
                },
                if paths.len() == 1 { "it" } else { "them" }
            )
        }
    }

    /// Downloads the asset into `dir`, reporting progress, and returns its path and SHA-256.
    fn download(&self, r: &Resolved, dir: &Path, ctx: &RunCtx) -> Result<(PathBuf, String)> {
        download_asset(&self.gh, r, &dir.join(".dot-asset"), ctx)
    }

    /// Checks `actual` against the published SHA-256, if the release has one.
    fn verify(&self, r: &Resolved, actual: &str, ctx: &RunCtx) -> Result<()> {
        verify_asset(&self.gh, r, actual, ctx)
    }
}

/// Downloads the asset to `path`, reporting progress, and returns its path and SHA-256.
fn download_asset(
    gh: &Github,
    r: &Resolved,
    path: &Path,
    ctx: &RunCtx,
) -> Result<(PathBuf, String)> {
    let resp = gh.download(&r.asset.browser_download_url)?;
    // The API's size is the file's; a Content-Length may describe an encoded transfer.
    let expected = (r.asset.size > 0).then_some(r.asset.size);
    let total = expected.or(resp.content_length);
    let path = path.to_path_buf();
    let mut out = BufWriter::new(
        File::create(&path).with_context(|| format!("creating {}", path.display()))?,
    );
    let mut body = resp.body;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 << 10];
    let (mut done, mut reported) = (0u64, 0u64);
    ctx.progress(0, total, Unit::Bytes);
    loop {
        let n = body
            .read(&mut buf)
            .with_context(|| format!("downloading {}", r.asset.name))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])
            .with_context(|| format!("writing {}", path.display()))?;
        done += n as u64;
        if done - reported >= 256 << 10 {
            ctx.progress(done, total, Unit::Bytes);
            reported = done;
        }
    }
    out.flush()?;
    ctx.progress(done, total, Unit::Bytes);
    if let Some(t) = expected
        && t != done
    {
        bail!("downloading {}: got {done} of {t} bytes", r.asset.name);
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((path, digest))
}

/// Checks `actual` against the step's `sha256:`, else the published SHA-256 of the asset, if
/// the release has one.
fn verify_asset(gh: &Github, r: &Resolved, actual: &str, ctx: &RunCtx) -> Result<()> {
    if let Some(pinned) = &r.sha256 {
        if pinned != actual {
            bail!(
                "{}: sha256 mismatch: the file downloaded does not match its sha256 (expected \
                 {pinned}, downloaded {actual}); nothing installed",
                r.asset.name
            );
        }
        ctx.output(
            Stream::Stdout,
            format!("{}: sha256 verified (the step's sha256)", r.asset.name),
        );
        return Ok(());
    }
    let (expected, from) = match &r.checksum {
        None => {
            ctx.output(
                Stream::Stdout,
                format!(
                    "{}: the release publishes no SHA-256 for it; not verified",
                    r.asset.name
                ),
            );
            return Ok(());
        }
        Some(ChecksumSource::Digest(hex)) => (Some(hex.clone()), "GitHub's asset digest"),
        Some(ChecksumSource::File { name, url }) => {
            let text = gh
                .download_text(url)
                .with_context(|| format!("fetching checksums from {name}"))?;
            (asset::find_checksum(&text, &r.asset.name), name.as_str())
        }
    };
    match expected {
        Some(e) if e == actual => {
            ctx.output(
                Stream::Stdout,
                format!("{}: sha256 verified ({from})", r.asset.name),
            );
            Ok(())
        }
        Some(e) => bail!(
            "{}: sha256 mismatch: expected {e} ({from}), downloaded {actual}; nothing \
             installed",
            r.asset.name
        ),
        None => {
            ctx.output(
                Stream::Stdout,
                format!("{}: not listed in {from}; not verified", r.asset.name),
            );
            Ok(())
        }
    }
}

impl Action for ReleaseAction {
    fn describe(&self) -> String {
        let names = self.names();
        let installed = self.installed();
        let r = match &self.release {
            Err(e) => {
                return match &installed {
                    Some(i) => format!(
                        "{names} {} installed from {} in {}; {} unknown ({e}): keeping it",
                        i.tag, self.repo, self.show_to, self.which
                    ),
                    None => format!(
                        "install {names} from {} -> {} ({} unknown: {e})",
                        self.repo, self.show_to, self.which
                    ),
                };
            }
            Ok(r) => r,
        };
        let base = match &installed {
            Some(i) if i.tag != r.tag => format!(
                "update {names} {} -> {} from {} in {}",
                i.tag, r.tag, self.repo, self.show_to
            ),
            Some(i) if i.asset != r.asset.name => format!(
                "switch {names} {} from {} in {} to another asset: {} -> {}",
                r.tag, self.repo, self.show_to, i.asset, r.asset.name
            ),
            Some(_) if !self.all_present() => format!(
                "install {names} {} from {} -> {} (missing from {})",
                r.tag, self.repo, self.show_to, self.show_to
            ),
            _ => format!(
                "install {names} {} from {} -> {} ({})",
                r.tag, self.repo, self.show_to, r.asset.name
            ),
        };
        let mut notes = Vec::new();
        let foreign = self.foreign(installed.as_ref());
        if !foreign.is_empty() {
            notes.push(self.foreign_note(&foreign));
        }
        if let Some(dir) = self.blocked() {
            notes.push(unwritable_note(&dir));
        }
        if notes.is_empty() {
            base
        } else {
            format!("{base} ({})", notes.join("; "))
        }
    }

    fn status(&self) -> Result<Status> {
        let Ok(r) = &self.release else {
            // Offline or rate limited: what `dot` installed is kept, as `download` keeps its
            // file, instead of failing the bundle.
            let kept = self.installed().is_some() && self.all_present();
            return Ok(if kept {
                Status::Satisfied
            } else {
                Status::Unknown
            });
        };
        let installed = self.installed();
        if !self.replace && !self.foreign(installed.as_ref()).is_empty() {
            return Ok(Status::Conflict);
        }
        Ok(match installed {
            Some(i) if i.tag == r.tag && i.asset == r.asset.name && self.all_present() => {
                Status::Satisfied
            }
            _ if self.blocked().is_some() => Status::Conflict,
            _ => Status::Pending,
        })
    }

    /// `release:<repo> in <dir>`: the installed executables are removed (while they hold what
    /// `dot` installed) and the install record dropped.
    fn resources(&self) -> Vec<Resource> {
        let files = self
            .bins
            .iter()
            .map(|w| crate::fileutil::written(&self.to.join(&w.installed)))
            .collect();
        vec![
            Resource::new("release", format!("{} in {}", self.repo, self.show_to)).with_undo(
                Undo::Release {
                    repo: self.repo.clone(),
                    dir: self.to.clone(),
                    files,
                },
            ),
        ]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let r = match &self.release {
            Ok(r) => r,
            Err(e) => bail!("{}: cannot look up the {}: {e}", self.repo, self.which),
        };
        let foreign = self.foreign(self.installed().as_ref());
        if !foreign.is_empty() && !self.replace {
            bail!("{}", self.foreign_note(&foreign));
        }
        std::fs::create_dir_all(&self.to)
            .with_context(|| format!("creating {}", self.to.display()))?;
        // Next to the destination, so the final renames stay on one filesystem.
        let staging = tempfile::Builder::new()
            .prefix(".dot-download-")
            .tempdir_in(&self.to)
            .with_context(|| format!("creating a temporary directory in {}", self.to.display()))?;
        let (downloaded, digest) = self.download(r, staging.path(), ctx)?;
        self.verify(r, &digest, ctx)?;
        let out = staging.path().join("out");
        std::fs::create_dir(&out)?;
        let extracted = archive::extract(&downloaded, &r.asset.name, r.format, &self.bins, &out)?;
        for (want, path) in self.bins.iter().zip(&extracted) {
            set_executable(path)?;
            let dest = self.to.join(&want.installed);
            if foreign.contains(&want.installed) {
                let backup = link::backup_path(&dest);
                std::fs::rename(&dest, &backup).with_context(|| {
                    format!("backing up {} to {}", dest.display(), backup.display())
                })?;
                ctx.output(
                    Stream::Stdout,
                    format!(
                        "{} was not installed by dot: kept as {}",
                        dest.display(),
                        backup.display()
                    ),
                );
            }
            std::fs::rename(path, &dest)
                .with_context(|| format!("installing {}", dest.display()))?;
        }
        self.state.record(Installed {
            repo: self.repo.clone(),
            dir: self.to.clone(),
            tag: r.tag.clone(),
            asset: r.asset.name.clone(),
            bins: self.bins.iter().map(|w| w.installed.clone()).collect(),
        })?;
        Ok(Outcome::Changed)
    }
}

#[cfg(unix)]
fn set_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("setting mode 0755 on {}", p.display()))
}

#[cfg(not(unix))]
fn set_executable(_p: &Path) -> Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;
