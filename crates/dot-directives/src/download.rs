//! `download`: a file from an https URL (or from a project's latest release), saved as a file
//! (`dest`), unpacked into a directory (`dest` + `extract`: go, zed, godot, a font), or a `.deb`
//! / `.rpm` installed through the package manager (`package`).
//!
//! ```yaml
//! - download:                       # Go: the version from go.dev, the tree replaced whole
//!     url: "https://go.dev/dl/{latest}.linux-amd64.tar.gz"
//!     latest: https://go.dev/VERSION?m=text
//!     dest: /usr/local/go
//!     extract: tar.gz
//!     strip: 1
//!     replace: true
//!     root: true
//!     bin: [bin/go, bin/gofmt]      # linked into ~/.local/bin
//! - download:                       # glab: a .deb from GitLab's latest release
//!     release: { source: gitlab, repo: gitlab-org/cli, asset: "glab_*_linux_amd64.deb" }
//!     package: glab
//! - download:
//!     url: https://example.com/tool_amd64.deb
//!     package: tool
//! ```
//!
//! Where from: a fixed `url`; a `url` with `{latest}` filled in from what `latest:` answers (a
//! small text, the first line or what `pattern` picks); or `release:`, an asset of a GitHub
//! repository's or GitLab project's latest release (picked by `asset:` or from the OS and CPU),
//! checked against the SHA-256 the release publishes when the step pins none. Looked up while
//! planning (once per run); offline, the step is unknown and keeps what is there.
//!
//! What was downloaded is recorded in the `downloads` section of the state store
//! ([`dot_core::state`]: URL, SHA-256, the server's `ETag` / `Last-Modified`, and an installed
//! tree's fingerprint). A step is satisfied when the record matches: with `sha256:` (or a
//! release's digest), when that digest was installed; without, when a conditional request says
//! the file has not changed since (a plan that cannot reach the server says so and keeps what is
//! there). A saved file edited since is downloaded again. A `package` is satisfied while it is
//! installed (by any means) and unchanged upstream. A URL built with `secret()` is requested as
//! rendered, but shown and recorded with `<secret:name>` in place of the value.
//!
//! Archives are unpacked into a temporary directory first (entries outside it are refused),
//! with `strip: N` leading directories dropped (each must be the archive's only top-level
//! entry). By default they are merged into `dest`; with `replace: true`, `dest` is the tree
//! itself and is swapped whole (the old one renamed aside, then removed), so files an update
//! drops do not linger; a tree `dot` did not install there is kept as `<dest>.dot-backup`, which
//! `dot remove` puts back. `bin:` links executables of the tree into `bin_dir` (default
//! `~/.local/bin`).
//!
//! Downloads are https only, redirects too (the URL is checked here, and the real
//! [`HttpClient`] refuses a redirect that leaves https). With a digest the file is checked
//! before anything is written.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, bail};
use dot_core::config::network::{LATEST_PLACEHOLDER, check_download_url};
use dot_core::config::{DownloadFrom, DownloadSpec, DownloadTo, Extract, LatestOpts};
use dot_core::template::SecretMode;
use dot_core::{
    Action, Diff, Outcome, PlanCtx, Resource, RunCtx, StateStore, Status, Undo, Unit, Written,
    sections,
};
use dot_platform::exec::{Cmd, System};
use dot_platform::http::{HttpClient, Request, UreqClient};
use dot_platform::link::LinkStrategy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fileutil::{
    DEFAULT_FILE_MODE, Existing, guard, tree_fingerprint, unwritable, unwritable_note, with_notes,
};
use crate::github_release::{
    ChecksumSource, Github, Origin, asset_format, asset_glob, pick_asset, published_sha256,
};
use crate::pkgfile::PkgFile;
use crate::rootfile::RootFiles;
use tree::{BinLink, Tree, replace_root_tree, replace_tree, stripped};

/// HTTP access for `download` steps, and what the run already asked the servers (a step is
/// planned twice).
#[derive(Clone)]
pub struct Downloads {
    pub http: Arc<dyn HttpClient>,
    /// The state directory to record downloads in; `None`: the run's.
    pub state_dir: Option<PathBuf>,
    checked: Arc<Mutex<HashMap<String, Result<Fresh, String>>>>,
    /// Answers of `latest:` URLs, by URL and pattern.
    latest: Arc<Mutex<HashMap<String, Result<String, String>>>>,
}

impl std::fmt::Debug for Downloads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Downloads")
            .field("state_dir", &self.state_dir)
            .finish_non_exhaustive()
    }
}

impl Default for Downloads {
    fn default() -> Self {
        Self::new(Arc::new(UreqClient::new()))
    }
}

impl Downloads {
    pub fn new(http: Arc<dyn HttpClient>) -> Self {
        Self {
            http,
            state_dir: None,
            checked: Arc::default(),
            latest: Arc::default(),
        }
    }

    /// The current version `latest` answers (once per run).
    fn latest_version(&self, url: &str, pattern: Option<&str>) -> Result<String, String> {
        let key = format!("{url} {pattern:?}");
        let mut cache = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
        let answer = (|| -> Result<String, String> {
            let resp = self
                .http
                .get(&Request::new(url))
                .map_err(|e| format!("{e:#}"))?;
            if !resp.is_success() {
                return Err(format!(
                    "asking {url} for the latest version: HTTP {}",
                    resp.status
                ));
            }
            let text = resp.text(64 << 10).map_err(|e| format!("{e:#}"))?;
            pick_version(&text, pattern)
                .map_err(|e| format!("asking {url} for the latest version: {e}"))
        })();
        cache.insert(key, answer.clone());
        answer
    }
}

/// The version in `text`: what `pattern` matches (its first group, else the whole match), or
/// the first non-empty line. Letters, digits and `._+-` only: it goes into a URL.
fn pick_version(text: &str, pattern: Option<&str>) -> Result<String, String> {
    let v = match pattern {
        Some(p) => {
            let re = regex::Regex::new(p).map_err(|e| format!("pattern `{p}`: {e}"))?;
            let caps = re
                .captures(text)
                .ok_or_else(|| format!("`{p}` matches nothing in the answer"))?;
            caps.get(1)
                .or_else(|| caps.get(0))
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default()
        }
        None => text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string(),
    };
    let ok = !v.is_empty()
        && v.len() <= 100
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
    if ok {
        Ok(v)
    } else {
        Err(format!(
            "the answer `{v}` is not a version (set `pattern:`)"
        ))
    }
}

/// What the server says about a file downloaded before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fresh {
    Unchanged,
    Changed,
}

/// One recorded download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// As shown: a `secret()` in it is `<secret:name>`.
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    /// An unpacked tree (`replace: true`): its fingerprint when installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
}

/// The `downloads` section of the state store: what was downloaded where, by destination
/// (`package:<name>` for a package).
#[derive(Debug, Clone)]
pub struct StateFile {
    pub store: StateStore,
}

impl StateFile {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            store: StateStore::at(dir),
        }
    }

    /// Unreadable state counts as nothing recorded: the worst outcome is a download.
    pub fn get(&self, key: &str) -> Option<Record> {
        self.store
            .section::<BTreeMap<String, Record>>(sections::DOWNLOADS)
            .remove(key)
    }

    pub fn record(&self, key: &str, r: Record) -> Result<()> {
        self.store
            .update_section(sections::DOWNLOADS, |all: &mut BTreeMap<String, Record>| {
                all.insert(key.to_string(), r);
            })
    }
}

/// What `download` needs from the rest of the run.
pub struct Env<'a> {
    pub dl: &'a Downloads,
    pub sys: &'a System,
    pub root_files: &'a RootFiles,
    pub github: &'a Github,
    pub links: &'a Arc<dyn LinkStrategy>,
}

pub fn plan(spec: &DownloadSpec, ctx: &PlanCtx<'_>, env: &Env<'_>) -> Result<Vec<Box<dyn Action>>> {
    let sha256 = spec
        .sha256
        .as_deref()
        .map(|s| ctx.render(s).map(|s| s.to_ascii_lowercase()))
        .transpose()?;
    if let Some(s) = &sha256 {
        dot_core::config::network::check_sha256(s).map_err(|e| ctx.config_error(e))?;
    }
    let state = StateFile::in_dir(env.dl.state_dir.as_deref().unwrap_or(ctx.state_dir));
    // A package's file type decides which release asset to take.
    let package = match &spec.to {
        DownloadTo::Package(name) => {
            let name = ctx.render(name)?;
            Some(
                PkgFile::for_facts(ctx.facts, &name, env.sys)
                    .map_err(|e| ctx.config_error(format!("download: {e}")))?,
            )
        }
        DownloadTo::Path(_) => None,
    };
    let mut source = plan_source(&spec.from, ctx, env, package.as_ref().map(PkgFile::ext))?;
    // `sha256_url`: the vendor's published digest, read when the file is downloaded (like a
    // release's checksum file).
    if let (Some(u), Ok(src)) = (&spec.sha256_url, source.as_mut()) {
        let rendered = ctx.render_text(u, "download: sha256_url")?;
        if !rendered.secrets.is_empty() {
            return Err(ctx.config_error("download: sha256_url: secret() cannot be used here"));
        }
        let url = match &src.latest {
            Some(v) => rendered.text.replace(LATEST_PLACEHOLDER, v),
            None => rendered.text,
        };
        check_download_url(&url).map_err(|e| ctx.config_error(e))?;
        let name = file_name(&src.shown).to_string();
        src.checksum = Some(ChecksumSource::File {
            name: url.clone(),
            url,
        });
        src.asset.get_or_insert(name);
    }
    let (to, key) = match (&spec.to, package) {
        (DownloadTo::Package(_), Some(pkg)) => {
            let key = format!("package:{}", pkg.name);
            (Target::Package(pkg), key)
        }
        (DownloadTo::Path(p), _) => {
            if spec.root {
                RootFiles::check_supported(ctx.facts.os).map_err(|m| ctx.config_error(m))?;
            }
            let dest = ctx.target(p)?;
            let dest = if spec.root {
                env.root_files.map(dest)
            } else {
                dest
            };
            guard(&dest, &ctx.show(&dest), ctx.home, &ctx.bundle.root.clone())?;
            let name = match &source {
                Ok(s) => file_name(&s.shown).to_string(),
                Err(_) => String::new(),
            };
            let extract = match spec.extract {
                None => None,
                // Unknown until the source is known; applying fails before then anyway.
                Some(Extract::Auto) if name.is_empty() => Some(Extract::Auto),
                Some(Extract::Auto) => match asset_format(&name) {
                    Some(e) if e != Extract::Raw => Some(e),
                    _ => {
                        return Err(ctx.config_error(format!(
                            "download: cannot tell how to unpack `{name}` from its name; say \
                             `extract: tar.gz`, `tar.xz` or `zip`"
                        )));
                    }
                },
                Some(e) => Some(e),
            };
            let mode = spec.mode.as_ref().map(|m| ctx.mode(m)).transpose()?;
            let tree = match extract {
                Some(_) => Some(plan_tree(spec, ctx, env)?),
                None => None,
            };
            let key = dest.display().to_string();
            (
                Target::Path {
                    show: ctx.show(&dest),
                    dest,
                    extract,
                    mode: mode.map_or(0o644, |m| m.0),
                    as_root: spec.root.then(|| env.root_files.clone()),
                    tree,
                },
                key,
            )
        }
        (DownloadTo::Package(_), None) => unreachable!("planned above"),
    };
    Ok(vec![Box::new(DownloadAction {
        source,
        sha256,
        to,
        key,
        state,
        dl: env.dl.clone(),
        github: env.github.clone(),
    })])
}

/// Where the file comes from: the URL rendered (and its shown form), `{latest}` filled in, or a
/// release's asset looked up. `Err` (not a planning error): the lookup failed (offline, rate
/// limited); the step is then unknown.
fn plan_source(
    from: &DownloadFrom,
    ctx: &PlanCtx<'_>,
    env: &Env<'_>,
    package_ext: Option<&str>,
) -> Result<std::result::Result<Source, String>> {
    match from {
        DownloadFrom::Url { url, latest } => {
            let rendered = ctx.render_text(url, "download: url")?;
            // Shown and recorded without secret values.
            let shown = if rendered.secrets.is_empty() {
                rendered.text.clone()
            } else {
                ctx.vars
                    .render_with(url, &SecretMode::Placeholder)
                    .map_err(|e| ctx.config_error(format!("download: url: {e}")))?
                    .text
            };
            check_download_url(&shown).map_err(|e| ctx.config_error(e))?;
            let mut source = Source {
                url: rendered.text,
                shown,
                placeholder: rendered.placeholder,
                digest: None,
                checksum: None,
                asset: None,
                note: None,
                latest: None,
            };
            if let Some(LatestOpts {
                url: probe,
                pattern,
            }) = latest
            {
                // Asked while planning, so it cannot hold a secret.
                let rendered = ctx.render_text(probe, "download: latest: url")?;
                if !rendered.secrets.is_empty() {
                    return Err(ctx.config_error(
                        "download: latest: secret() cannot be used in its URL: it is asked while \
                         planning, before secrets are resolved",
                    ));
                }
                let probe = rendered.text;
                check_download_url(&probe).map_err(|e| ctx.config_error(e))?;
                let pattern = pattern.as_deref().map(|p| ctx.render(p)).transpose()?;
                let version = match env.dl.latest_version(&probe, pattern.as_deref()) {
                    Ok(v) => v,
                    Err(e) => return Ok(Err(format!("latest version unknown: {e}"))),
                };
                source.url = source.url.replace(LATEST_PLACEHOLDER, &version);
                source.shown = source.shown.replace(LATEST_PLACEHOLDER, &version);
                check_download_url(&source.shown).map_err(|e| ctx.config_error(e))?;
                source.note = Some(format!("latest: {version}"));
                source.latest = Some(version);
            }
            Ok(Ok(source))
        }
        DownloadFrom::Release(r) => {
            let origin = Origin::plan(&r.repo, r.source, r.host.as_deref(), ctx)?;
            let glob = asset_glob(r.asset.as_ref(), ctx)?;
            let token = origin.token(r.token.as_ref(), ctx, env.github)?;
            let release = match origin.latest(env.github, token.as_ref()) {
                Ok(r) => r,
                Err(e) => return Ok(Err(format!("latest release unknown: {e:#}"))),
            };
            let picked = match pick_asset(
                &release,
                glob.as_ref(),
                package_ext,
                ctx.facts.os,
                ctx.facts.arch,
            ) {
                Ok(p) => p,
                Err(e) => {
                    return Err(ctx.config_error(format!(
                        "download: {} {}: {e}",
                        origin.show(),
                        release.tag_name
                    )));
                }
            };
            let url = picked.asset.browser_download_url.clone();
            check_download_url(&url).map_err(|e| {
                ctx.config_error(format!("{e} (the release's asset {})", picked.asset.name))
            })?;
            let digest = match &picked.checksum {
                Some(ChecksumSource::Digest(hex)) => Some(hex.clone()),
                _ => None,
            };
            Ok(Ok(Source {
                shown: url.clone(),
                url,
                placeholder: false,
                digest,
                note: Some(format!(
                    "release {} {} ({})",
                    origin.show(),
                    picked.tag,
                    picked.asset.name
                )),
                asset: Some(picked.asset.name.clone()),
                checksum: picked.checksum,
                latest: None,
            }))
        }
    }
}

/// The tree options of an archive target: `replace`, `strip` and the `bin` links.
fn plan_tree(spec: &DownloadSpec, ctx: &PlanCtx<'_>, env: &Env<'_>) -> Result<Tree> {
    let bin_dir = ctx.target(spec.bin_dir.as_deref().unwrap_or(DEFAULT_BIN_DIR))?;
    let mut bins = Vec::new();
    for b in &spec.bin {
        let name = ctx.render(&b.name)?;
        let (link_name, pattern) = match &b.from {
            Some(p) => (name, ctx.render(p)?),
            None => (
                Path::new(&name)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                name,
            ),
        };
        if link_name.is_empty() {
            return Err(ctx.config_error(format!("download: bin `{pattern}` names no file")));
        }
        let glob = globset::GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| ctx.config_error(format!("download: bin `{pattern}`: {e}")))?
            .compile_matcher();
        let link = bin_dir.join(&link_name);
        guard(&link, &ctx.show(&link), ctx.home, &ctx.bundle.root.clone())?;
        bins.push(BinLink {
            show: ctx.show(&link),
            link,
            pattern,
            glob,
        });
    }
    Ok(Tree {
        replace: spec.replace,
        strip: spec.strip,
        bins,
        links: env.links.clone(),
    })
}

/// Where `bin` links go by default.
pub const DEFAULT_BIN_DIR: &str = "~/.local/bin";

/// The last path segment of a URL (without query or fragment).
fn file_name(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next().unwrap_or(path)
}

/// Where the file comes from.
#[derive(Debug, Clone)]
struct Source {
    /// Requested; holds resolved secret values when applying.
    url: String,
    /// Shown, recorded and put in errors: secrets as `<secret:name>`.
    shown: String,
    /// Planning: the URL holds secret placeholders, so nothing is requested yet.
    placeholder: bool,
    /// The release's published digest for the asset.
    digest: Option<String>,
    /// Where the release publishes the asset's checksum (a checksum file is read when
    /// applying).
    checksum: Option<ChecksumSource>,
    /// The release asset's name.
    asset: Option<String>,
    /// Said in the plan: the release and asset, or the latest version.
    note: Option<String>,
    /// What `latest:` answered.
    latest: Option<String>,
}

enum Target {
    Path {
        dest: PathBuf,
        show: String,
        /// `None`: saved as a file.
        extract: Option<Extract>,
        mode: u32,
        as_root: Option<RootFiles>,
        /// An archive's tree options (with `extract`).
        tree: Option<Tree>,
    },
    Package(PkgFile),
}

pub struct DownloadAction {
    /// `Err`: where the file comes from could not be looked up.
    source: std::result::Result<Source, String>,
    sha256: Option<String>,
    to: Target,
    /// Its key in the state file.
    key: String,
    state: StateFile,
    dl: Downloads,
    github: Github,
}

/// Where the step stands, and why.
struct Assessed {
    status: Status,
    notes: Vec<String>,
}

impl Assessed {
    fn new(status: Status, note: impl Into<String>) -> Self {
        Self {
            status,
            notes: vec![note.into()],
        }
    }
}

fn sha256_of(r: &mut dyn Read) -> io::Result<String> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

impl DownloadAction {
    /// The URL as shown (secrets as placeholders), or what the step is about when it is not
    /// known.
    fn shown(&self) -> String {
        match &self.source {
            Ok(s) => s.shown.clone(),
            Err(_) => "(unknown)".into(),
        }
    }

    /// The digest the file must have: the step's `sha256:`, else the release's.
    fn pinned(&self) -> Option<&str> {
        self.sha256
            .as_deref()
            .or_else(|| self.source.as_ref().ok()?.digest.as_deref())
    }

    /// Asks the server whether the file changed since `r` was downloaded (once per run).
    fn fresh(&self, src: &Source, r: &Record) -> Result<Fresh, String> {
        if r.etag.is_none() && r.last_modified.is_none() {
            // Nothing to ask with: the server gave no validators.
            return Ok(Fresh::Unchanged);
        }
        let mut cache = self.dl.checked.lock().unwrap_or_else(|e| e.into_inner());
        let key = format!("{} {:?} {:?}", src.shown, r.etag, r.last_modified);
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
        let mut req = Request::new(src.url.clone());
        if let Some(e) = &r.etag {
            req = req.header("If-None-Match", e.clone());
        }
        if let Some(m) = &r.last_modified {
            req = req.header("If-Modified-Since", m.clone());
        }
        let answer = match self.dl.http.get(&req) {
            Err(e) => Err(hide(&format!("{e:#}"), src)),
            Ok(resp) if resp.status == 304 => Ok(Fresh::Unchanged),
            Ok(resp) if resp.is_success() => {
                let same_etag = r.etag.is_some() && resp.header("etag") == r.etag.as_deref();
                let same_date = r.etag.is_none()
                    && r.last_modified.is_some()
                    && resp.header("last-modified") == r.last_modified.as_deref();
                Ok(if same_etag || same_date {
                    Fresh::Unchanged
                } else {
                    Fresh::Changed
                })
            }
            Ok(resp) => Err(format!("HTTP {}", resp.status)),
        };
        cache.insert(key, answer.clone());
        answer
    }

    /// Whether the recorded download is still what the step asks for (URL, pinned digest,
    /// upstream unchanged); `Err` holds the note saying why not.
    fn record_current(
        &self,
        src: &Source,
        r: &Record,
        notes: &mut Vec<String>,
    ) -> Result<(), String> {
        if r.url != src.shown {
            return Err(format!("was downloaded from {}", r.url));
        }
        match self.pinned() {
            Some(want) if *want != r.sha256 => Err("the pinned sha256 changed".into()),
            Some(_) => Ok(()),
            None => match self.fresh(src, r) {
                Ok(Fresh::Unchanged) => Ok(()),
                Ok(Fresh::Changed) => Err("changed upstream".into()),
                Err(e) => {
                    notes.push(format!("not checked for a newer file: {e}"));
                    Ok(())
                }
            },
        }
    }

    fn assess(&self) -> Result<Assessed> {
        let record = self.state.get(&self.key);
        match &self.to {
            Target::Package(pkg) => {
                let installed = match pkg.installed() {
                    Ok(v) => v,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        return Ok(Assessed::new(
                            Status::Unknown,
                            "the package database cannot be queried yet",
                        ));
                    }
                    Err(e) => return Err(e).context("querying the package database"),
                };
                let Some(version) = installed else {
                    return Ok(match &self.source {
                        Err(e) => Assessed::new(Status::Unknown, format!("not installed; {e}")),
                        Ok(_) => Assessed::new(Status::Pending, "not installed"),
                    });
                };
                let mut notes = vec![format!("{} {version} installed", pkg.name)];
                let (Some(r), Ok(src)) = (record, &self.source) else {
                    if let Err(e) = &self.source {
                        notes.push(e.clone());
                    }
                    return Ok(Assessed {
                        status: Status::Satisfied,
                        notes,
                    });
                };
                if src.placeholder {
                    return Ok(Assessed::new(
                        Status::Unknown,
                        "the URL uses secrets; checked when applying",
                    ));
                }
                Ok(match self.record_current(src, &r, &mut notes) {
                    Ok(()) => Assessed {
                        status: Status::Satisfied,
                        notes,
                    },
                    Err(why) => Assessed::new(Status::Pending, format!("update: {why}")),
                })
            }
            Target::Path {
                dest,
                extract,
                as_root,
                tree,
                ..
            } => {
                let present = match extract {
                    Some(_) => dest.is_dir(),
                    None => dest.is_file(),
                };
                // Root-owned destinations are written through sudo; a directory to unpack
                // into is written in (or, replaced, next to), a file next to.
                let blocked = || match (as_root, extract, tree) {
                    (Some(_), _, _) => None,
                    (None, Some(_), Some(t)) if t.replace => unwritable(dest),
                    (None, Some(_), _) => unwritable(&dest.join("x")),
                    (None, None, _) => unwritable(dest),
                };
                let pending = |note: String| -> Assessed {
                    match blocked() {
                        Some(dir) => Assessed {
                            status: Status::Conflict,
                            notes: vec![note, unwritable_note(&dir)],
                        },
                        None => Assessed::new(Status::Pending, note),
                    }
                };
                if let Some(t) = tree
                    && let Err(e) = t.pending_links(dest)
                {
                    return Ok(Assessed::new(Status::Conflict, format!("conflict: {e}")));
                }
                let src = match &self.source {
                    Ok(s) => s,
                    Err(e) => {
                        let status = if present {
                            // Offline: what is there is kept.
                            Status::Satisfied
                        } else {
                            Status::Unknown
                        };
                        return Ok(Assessed::new(status, e.clone()));
                    }
                };
                if src.placeholder && !present {
                    return Ok(pending("not downloaded yet".into()));
                }
                let Some(r) = record.filter(|_| present) else {
                    let note = match (present, extract, tree) {
                        (true, None, _) => "replaces the file there".to_string(),
                        (true, Some(_), Some(t)) if t.replace => format!(
                            "replaces the tree there (kept as {})",
                            crate::link::BACKUP_SUFFIX
                        ),
                        (true, Some(_), _) => "unpacks over what is there".to_string(),
                        (false, _, _) => "not downloaded yet".to_string(),
                    };
                    return Ok(pending(note));
                };
                if extract.is_none()
                    && let Ok(mut f) = File::open(dest)
                    && sha256_of(&mut f)? != r.sha256
                {
                    return Ok(pending(
                        "the file was changed since it was downloaded".into(),
                    ));
                }
                if src.placeholder {
                    return Ok(Assessed::new(
                        Status::Unknown,
                        "the URL uses secrets; checked when applying",
                    ));
                }
                let mut notes = Vec::new();
                match self.record_current(src, &r, &mut notes) {
                    Ok(()) => {}
                    Err(why) => return Ok(pending(format!("update: {why}"))),
                }
                if let Some(t) = tree {
                    let links = t.pending_links(dest).unwrap_or_default();
                    if !links.is_empty() {
                        return Ok(pending(links.join(", ")));
                    }
                }
                Ok(Assessed {
                    status: Status::Satisfied,
                    notes,
                })
            }
        }
    }

    /// Downloads the file into `dir` as `name`: its path, SHA-256 and validators. The digest
    /// is checked against the step's `sha256:`, else what the release publishes.
    fn fetch(
        &self,
        src: &Source,
        dir: &Path,
        name: &str,
        ctx: &RunCtx,
    ) -> Result<(PathBuf, Record)> {
        let resp = self
            .dl
            .http
            .get(&Request::new(src.url.clone()))
            .map_err(|e| anyhow::anyhow!("{}", hide(&format!("{e:#}"), src)))?;
        if !resp.is_success() {
            bail!("downloading {}: HTTP {}", src.shown, resp.status);
        }
        let etag = resp.header("etag").map(str::to_string);
        let last_modified = resp.header("last-modified").map(str::to_string);
        let total = resp.content_length;
        let path = dir.join(name);
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
                .with_context(|| format!("downloading {}", src.shown))?;
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
        let digest: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let expected = match (&self.sha256, &src.checksum, &src.asset) {
            (Some(want), _, _) => Some((want.clone(), None)),
            (None, Some(c), Some(asset)) => {
                published_sha256(&self.github, c, asset)?.map(|(h, from)| (h, Some(from)))
            }
            _ => None,
        };
        match expected {
            Some((want, from)) if want != digest => {
                let what = src.asset.clone().unwrap_or_else(|| src.shown.clone());
                match from {
                    Some(from) => bail!(
                        "{what}: sha256 mismatch: expected {want} ({from}), downloaded {digest}; \
                         nothing installed"
                    ),
                    None => bail!(
                        "{what}: sha256 mismatch: expected {want}, downloaded {digest}; nothing \
                         installed"
                    ),
                }
            }
            Some((_, Some(from))) => ctx.output(
                dot_core::Stream::Stdout,
                format!(
                    "{}: sha256 verified ({from})",
                    src.asset.as_deref().unwrap_or("")
                ),
            ),
            _ => {}
        }
        Ok((
            path,
            Record {
                url: src.shown.clone(),
                sha256: digest,
                etag,
                last_modified,
                tree: None,
            },
        ))
    }
}

/// `text` with the requested URL replaced by its shown form (no secret values in errors).
fn hide(text: &str, src: &Source) -> String {
    if src.url == src.shown {
        text.to_string()
    } else {
        text.replace(&src.url, &src.shown)
    }
}

impl Action for DownloadAction {
    fn describe(&self) -> String {
        let url = self.shown();
        let base = match &self.to {
            Target::Package(p) => format!(
                "download {url} and install it as package {} ({})",
                p.name,
                p.manager.as_str()
            ),
            Target::Path {
                show,
                extract: Some(e),
                tree,
                ..
            } => {
                let how = if tree.as_ref().is_some_and(|t| t.replace) {
                    "as"
                } else {
                    "into"
                };
                format!("download {url} and unpack it ({}) {how} {show}", e.as_str())
            }
            Target::Path { show, .. } => format!("download {url} -> {show}"),
        };
        let mut notes = Vec::new();
        if let Ok(Source { note: Some(n), .. }) = &self.source {
            notes.push(n.clone());
        }
        match self.assess() {
            Ok(a) => notes.extend(a.notes),
            Err(e) => notes.push(format!("cannot inspect: {e:#}")),
        }
        let release_checked = self
            .source
            .as_ref()
            .is_ok_and(|s| s.checksum.is_some() || s.digest.is_some());
        if self.sha256.is_none() && !release_checked {
            notes.push("not pinned (no sha256)".into());
        }
        if matches!(
            &self.to,
            Target::Path {
                as_root: Some(_),
                ..
            }
        ) {
            notes.push(crate::rootfile::AS_ROOT.into());
        }
        with_notes(base, &notes)
    }

    fn status(&self) -> Result<Status> {
        Ok(self.assess()?.status)
    }

    /// A package: `pkg:<manager>:<name>`, uninstalled by `dot remove --packages`. A saved file:
    /// `file:<dest>`, removed while it holds what was downloaded (through sudo for `root:
    /// true`); a file that was there before is saved first and put back. A replaced tree:
    /// `tree:<dest>`, removed while it is as unpacked; an archive merged into a directory:
    /// `archive:<dest>`, which `dot remove` leaves (it was merged into what was there). Each
    /// `bin` link: `link:<path>`.
    fn resources(&self) -> Vec<Resource> {
        match &self.to {
            Target::Package(p) => vec![p.resource()],
            Target::Path {
                dest,
                show,
                extract: None,
                as_root,
                ..
            } => {
                // A root-owned file may not be readable by this user: it holds what was
                // downloaded, whose SHA-256 is recorded.
                let file = match self.state.get(&self.key) {
                    Some(r) if as_root.is_some() => Written {
                        path: dest.clone(),
                        sha256: Some(r.sha256),
                    },
                    _ => crate::fileutil::written(dest),
                };
                let r = Resource::new("file", show.clone()).with_undo(Undo::RemoveFiles {
                    files: vec![file],
                    root: as_root.is_some(),
                });
                // A root-owned file is saved by `apply`, which may read it through sudo.
                vec![if as_root.is_some() {
                    r
                } else {
                    r.preserving(dest)
                }]
            }
            Target::Path {
                dest,
                show,
                extract: Some(_),
                as_root,
                tree,
                ..
            } => {
                let mut out = Vec::new();
                match tree {
                    Some(t) if t.replace => out.push(
                        Resource::new("tree", show.clone()).with_undo(Undo::RemoveTree {
                            path: dest.clone(),
                            root: as_root.is_some(),
                            fingerprint: self.state.get(&self.key).and_then(|r| r.tree),
                        }),
                    ),
                    _ => out.push(
                        Resource::new("archive", show.clone()).with_undo(Undo::Manual {
                            note: format!(
                                "{show}: what {} unpacked there was merged with what was there; \
                             remove it by hand",
                                file_name(&self.shown())
                            ),
                        }),
                    ),
                }
                for b in tree.iter().flat_map(|t| &t.bins) {
                    let mut r = Resource::new("link", b.show.clone());
                    if let Some(target) = Tree::target(b, dest) {
                        r = r.with_undo(Undo::Unlink {
                            path: b.link.clone(),
                            target,
                        });
                    }
                    out.push(r);
                }
                out
            }
        }
    }

    /// The download is binary: what would change, as a sentence (or a missing package).
    fn diff(&self) -> Result<Option<Diff>> {
        let a = self.assess()?;
        if a.status == Status::Satisfied {
            return Ok(None);
        }
        let url = self.shown();
        Ok(Some(match &self.to {
            Target::Package(p) if a.notes.iter().any(|n| n == "not installed") => Diff::Packages {
                manager: p.manager.as_str().to_string(),
                missing: vec![p.name.clone()],
                outdated: Vec::new(),
            },
            Target::Package(p) => Diff::Note(format!(
                "package {} from {url}: {}",
                p.name,
                a.notes.join("; ")
            )),
            Target::Path { show, .. } => {
                Diff::Note(format!("{show} <- {url}: {}", a.notes.join("; ")))
            }
        }))
    }

    fn needs_sudo(&self) -> bool {
        match &self.to {
            Target::Package(p) => p.needs_sudo(),
            Target::Path { as_root, .. } => as_root.as_ref().is_some_and(RootFiles::needs_sudo),
        }
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let src = match &self.source {
            Ok(s) => s,
            Err(e) => bail!("download: {e}"),
        };
        let a = self.assess()?;
        match a.status {
            Status::Satisfied => return Ok(Outcome::Ok),
            Status::Conflict => bail!("{}", a.notes.join("; ")),
            _ => {}
        }
        match &self.to {
            Target::Package(pkg) => {
                let staging = tempfile::Builder::new()
                    .prefix("dot-download-")
                    .tempdir()
                    .context("creating a temporary directory")?;
                let (file, record) = self.fetch(
                    src,
                    staging.path(),
                    &format!("{}.{}", pkg.name, pkg.ext()),
                    ctx,
                )?;
                // The package manager may read it as another user (apt's `_apt`).
                crate::fileutil::set_mode(staging.path(), 0o755)?;
                crate::fileutil::set_mode(&file, 0o644)?;
                pkg.install(&file, ctx)?;
                self.state.record(&self.key, record)?;
            }
            Target::Path {
                dest,
                show,
                extract,
                mode,
                as_root,
                tree,
            } => {
                // An unchanged download whose `bin` links are missing: only the links.
                let record = self.state.get(&self.key);
                let current = dest.exists()
                    && record
                        .as_ref()
                        .is_some_and(|r| self.record_current(src, r, &mut Vec::new()).is_ok());
                if let (true, Some(t)) = (current, tree) {
                    t.link(dest)?;
                    return Ok(Outcome::Changed);
                }
                // Next to the destination when this user writes there (so the final renames
                // stay on one filesystem), else in the temporary directory.
                let parent = dest.parent().context("the destination has no parent")?;
                let staging = match as_root {
                    Some(_) => tempfile::Builder::new().prefix("dot-download-").tempdir(),
                    None => {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("creating {}", parent.display()))?;
                        tempfile::Builder::new()
                            .prefix(".dot-download-")
                            .tempdir_in(parent)
                    }
                }
                .context("creating a temporary directory")?;
                let (file, mut record) = self.fetch(src, staging.path(), ".dot-download", ctx)?;
                match (extract, as_root) {
                    (None, None) => {
                        crate::fileutil::set_mode(&file, *mode)?;
                        if Existing::peek(dest)? == Existing::Dir {
                            bail!("{} is a directory", dest.display());
                        }
                        fs::rename(&file, dest)
                            .with_context(|| format!("installing {}", dest.display()))?;
                    }
                    (None, Some(r)) => {
                        if let crate::rootfile::RootExisting::Read {
                            existing: Existing::File { bytes, mode: have },
                            ..
                        } = r.read(dest)?
                        {
                            let key = format!("file:{show}");
                            ctx.preserve_root(
                                &key,
                                dest,
                                &bytes,
                                have.unwrap_or(DEFAULT_FILE_MODE),
                            )?;
                        }
                        r.remove_link(dest, ctx)?;
                        r.install(&file, dest, *mode, ctx)?;
                    }
                    (Some(Extract::Auto), _) => {
                        bail!("cannot tell how to unpack {}; say `extract:`", src.shown)
                    }
                    (Some(format), root) => {
                        let out = staging.path().join("out");
                        fs::create_dir(&out)?;
                        unpack(&file, *format, &out)?;
                        let _ = fs::remove_file(&file);
                        let strip = tree.as_ref().map_or(0, |t| t.strip);
                        let unpacked = stripped(&out, strip)?;
                        let replace = tree.as_ref().is_some_and(|t| t.replace);
                        // A tree `dot` did not install is kept, not deleted.
                        let foreign =
                            replace && dest.exists() && self.state.get(&self.key).is_none();
                        match (root, replace) {
                            (None, false) => merge(&unpacked, dest)?,
                            (None, true) => replace_tree(&unpacked, dest, foreign, ctx)?,
                            (Some(r), false) => {
                                r.mkdir(dest, 0o755, ctx)?;
                                r.run(
                                    Cmd::new("cp").args([
                                        "-R".to_string(),
                                        format!("{}/.", unpacked.display()),
                                        dest.display().to_string(),
                                    ]),
                                    ctx,
                                )?;
                            }
                            (Some(r), true) => replace_root_tree(&unpacked, dest, foreign, r, ctx)?,
                        }
                        if replace {
                            record.tree = tree_fingerprint(dest).ok();
                        }
                        if let Some(t) = tree {
                            t.link(dest)?;
                        }
                    }
                }
                self.state.record(&self.key, record)?;
            }
        }
        Ok(Outcome::Changed)
    }
}

/// Unpacks `archive` into `out`. Entries that would land outside it (`..`, absolute paths,
/// through a link) are refused by the unpackers.
fn unpack(archive: &Path, format: Extract, out: &Path) -> Result<()> {
    let open = || File::open(archive).with_context(|| format!("opening {}", archive.display()));
    match format {
        Extract::TarGz => {
            let mut a = tar::Archive::new(flate2::read::GzDecoder::new(BufReader::new(open()?)));
            unpack_tar(&mut a, out)
        }
        Extract::TarXz => {
            let tar_file = out.with_extension("tar");
            {
                let mut input = BufReader::new(open()?);
                let mut output = BufWriter::new(File::create(&tar_file)?);
                lzma_rs::xz_decompress(&mut input, &mut output)
                    .map_err(|e| anyhow::anyhow!("decompressing: {e}"))?;
                output.flush()?;
            }
            let mut a = tar::Archive::new(BufReader::new(File::open(&tar_file)?));
            let r = unpack_tar(&mut a, out);
            let _ = fs::remove_file(&tar_file);
            r
        }
        Extract::Zip => {
            let mut z = zip::ZipArchive::new(open()?).context("reading the zip archive")?;
            z.extract(out).context("unpacking the zip archive")
        }
        Extract::Auto | Extract::Raw => bail!("not an archive"),
    }
}

fn unpack_tar<R: Read>(a: &mut tar::Archive<R>, out: &Path) -> Result<()> {
    a.set_preserve_permissions(true);
    for entry in a.entries().context("reading the archive")? {
        let mut entry = entry.context("reading the archive")?;
        let path = entry.path().context("reading the archive")?.into_owned();
        // `unpack_in` refuses paths outside `out`; say which.
        if !entry
            .unpack_in(out)
            .with_context(|| format!("unpacking {}", path.display()))?
        {
            bail!(
                "the archive has an entry outside its directory ({}); refusing to unpack it",
                path.display()
            );
        }
    }
    Ok(())
}

/// Moves everything under `from` into `to`, replacing files and keeping what `from` does not
/// have.
fn merge(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dest = to.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() && fs::symlink_metadata(&dest).is_ok_and(|m| m.is_dir()) {
            merge(&src, &dest)?;
            continue;
        }
        match fs::symlink_metadata(&dest) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&dest)?,
            Ok(_) => fs::remove_file(&dest)?,
            Err(_) => {}
        }
        fs::rename(&src, &dest).with_context(|| format!("installing {}", dest.display()))?;
    }
    Ok(())
}

mod tree;

#[cfg(all(test, unix))]
mod tests;
