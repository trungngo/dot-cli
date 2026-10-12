//! The release API calls `github_release` (and `download: { release }`) make: the latest
//! release of a GitHub repository or a GitLab project, and downloads of its assets. Everything
//! goes through an injected [`HttpClient`].

use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use dot_core::SecretBytes;
use dot_platform::http::{HttpClient, Request, Response, UreqClient};
use serde::Deserialize;

/// The public API.
pub const GITHUB_API: &str = "https://api.github.com";

/// Environment variables a token is taken from when a step names no `token:` secret.
pub const TOKEN_ENV: [&str; 2] = ["GITHUB_TOKEN", "GH_TOKEN"];

/// The secret a step without `token:` uses when the environment has no token and `secrets.yml`
/// declares it (see [`Github::token_secret`]).
pub const TOKEN_SECRET: &str = "github_token";

/// Overrides [`GITHUB_API`] (a GitHub Enterprise server's `https://<host>/api/v3`, a mirror, or a
/// local fixture in tests).
pub const API_ENV: &str = "DOT_GITHUB_API";

/// Overrides the GitLab API base, `https://<host>/api/v4` (a local fixture in tests).
pub const GITLAB_API_ENV: &str = "DOT_GITLAB_API";

/// The environment variable a GitLab token is taken from when a step names no `token:` secret.
pub const GITLAB_TOKEN_ENV: &str = "GITLAB_TOKEN";

/// GitLab's answer for a release: its asset links.
#[derive(Debug, Deserialize)]
struct GitlabRelease {
    tag_name: String,
    #[serde(default)]
    assets: GitlabAssets,
}

#[derive(Debug, Default, Deserialize)]
struct GitlabAssets {
    #[serde(default)]
    links: Vec<GitlabLink>,
}

#[derive(Debug, Deserialize)]
struct GitlabLink {
    name: String,
    url: String,
    #[serde(default)]
    direct_asset_url: Option<String>,
}

impl From<GitlabRelease> for Release {
    fn from(r: GitlabRelease) -> Release {
        Release {
            tag_name: r.tag_name,
            assets: r
                .assets
                .links
                .into_iter()
                .map(|l| Asset {
                    browser_download_url: l.direct_asset_url.unwrap_or(l.url),
                    name: l.name,
                    size: 0,
                    digest: None,
                    id: None,
                })
                .collect(),
        }
    }
}

/// `group/sub/name` as one path segment of a GitLab API URL.
fn gitlab_id(project: &str) -> String {
    project.replace('/', "%2F")
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
    /// `sha256:<hex>`, published by GitHub for assets uploaded since mid-2025.
    #[serde(default)]
    pub digest: Option<String>,
    /// GitHub's asset id: [`Github::download_asset`] fetches it through the API, which is how
    /// a private repository's assets are downloaded (`browser_download_url` needs a browser
    /// session there).
    #[serde(default)]
    pub id: Option<u64>,
}

/// Release access for a run (GitHub, and GitLab): the HTTP client, the API bases, tokens from the
/// environment, where installed versions are recorded, and the latest releases already looked
/// up (a step is planned twice, and several steps may use one repository; only successes are
/// kept).
#[derive(Clone)]
pub struct Github {
    pub http: Arc<dyn HttpClient>,
    /// API base URL, without a trailing slash.
    pub api: String,
    /// From [`TOKEN_ENV`]; a step's `token:` secret takes precedence.
    pub env_token: Option<SecretBytes>,
    /// The secret steps without `token:` use when there is no [`Github::env_token`]:
    /// [`TOKEN_SECRET`] when `secrets.yml` declares it. Resolved with the run's other secrets,
    /// so it is only used when applying.
    pub token_secret: Option<String>,
    /// Directory for the installed-versions file; `None`: `~/.local/state/dot`.
    pub state_dir: Option<PathBuf>,
    /// [`GITLAB_API_ENV`]: the GitLab API base for every host (tests); `None`:
    /// `https://<host>/api/v4`.
    pub gitlab_api: Option<String>,
    /// From [`GITLAB_TOKEN_ENV`]; a step's `token:` secret takes precedence.
    pub gitlab_token: Option<SecretBytes>,
    cache: Arc<Mutex<HashMap<String, Arc<Release>>>>,
}

impl fmt::Debug for Github {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Github")
            .field("api", &self.api)
            .field("env_token", &self.env_token.as_ref().map(|_| "***"))
            .field("token_secret", &self.token_secret)
            .field("state_dir", &self.state_dir)
            .field("gitlab_api", &self.gitlab_api)
            .field("gitlab_token", &self.gitlab_token.as_ref().map(|_| "***"))
            .finish_non_exhaustive()
    }
}

impl Github {
    pub fn new(http: Arc<dyn HttpClient>, api: impl Into<String>) -> Self {
        Self {
            http,
            api: api.into().trim_end_matches('/').to_string(),
            env_token: None,
            token_secret: None,
            state_dir: None,
            gitlab_api: None,
            gitlab_token: None,
            cache: Arc::default(),
        }
    }

    /// The real API (or [`API_ENV`]'s) through the real client, with the token from the
    /// environment and the state directory from `$XDG_STATE_HOME`.
    pub fn from_env() -> Self {
        let api = std::env::var(API_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| GITHUB_API.to_string());
        let mut gh = Self::new(Arc::new(UreqClient::new()), api);
        gh.env_token = TOKEN_ENV
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
            .map(|v| SecretBytes::new(v.trim()));
        gh.gitlab_api = std::env::var(GITLAB_API_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.trim_end_matches('/').to_string());
        gh.gitlab_token = std::env::var(GITLAB_TOKEN_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| SecretBytes::new(v.trim()));
        gh.state_dir = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .map(|p| p.join("dot"));
        gh
    }

    fn api_request(&self, url: String, token: Option<&SecretBytes>) -> Result<Request> {
        let mut req = Request::new(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = token {
            let Some(t) = t.as_str() else {
                bail!("the GitHub token is not text");
            };
            req = req.header("Authorization", format!("Bearer {}", t.trim()));
        }
        Ok(req)
    }

    /// The latest (non-draft, non-prerelease) release of `repo`.
    pub fn latest(&self, repo: &str, token: Option<&SecretBytes>) -> Result<Arc<Release>> {
        self.release(repo, None, token)
    }

    /// The release of `repo` tagged `tag`, or the latest one.
    pub fn release(
        &self,
        repo: &str,
        tag: Option<&str>,
        token: Option<&SecretBytes>,
    ) -> Result<Arc<Release>> {
        let key = match tag {
            Some(t) => format!("{repo}@{t}"),
            None => repo.to_string(),
        };
        if let Some(r) = self.lock().get(&key) {
            return Ok(r.clone());
        }
        let url = match tag {
            Some(t) => format!("{}/repos/{repo}/releases/tags/{t}", self.api),
            None => format!("{}/repos/{repo}/releases/latest", self.api),
        };
        let resp = self.http.get(&self.api_request(url, token)?)?;
        if !resp.is_success() {
            if let (Some(t), 404) = (tag, resp.status) {
                bail!(
                    "{repo} has no release tagged {t} (or the repository does not exist, or \
                     is private and no token can read it)"
                );
            }
            bail!("{}", api_error(repo, token.is_some(), resp));
        }
        let text = resp.text(16 << 20)?;
        let release: Release = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("unexpected reply from the GitHub API for {repo}: {e}"))?;
        let release = Arc::new(release);
        self.lock().insert(key, release.clone());
        Ok(release)
    }

    /// The latest release of the GitLab project `project` on `host`.
    pub fn latest_gitlab(
        &self,
        host: &str,
        project: &str,
        token: Option<&SecretBytes>,
    ) -> Result<Arc<Release>> {
        self.release_gitlab(host, project, None, token)
    }

    /// The release of the GitLab project `project` on `host` tagged `tag`, or the latest one.
    pub fn release_gitlab(
        &self,
        host: &str,
        project: &str,
        tag: Option<&str>,
        token: Option<&SecretBytes>,
    ) -> Result<Arc<Release>> {
        let key = format!("gitlab:{host}/{project}@{}", tag.unwrap_or(""));
        if let Some(r) = self.lock().get(&key) {
            return Ok(r.clone());
        }
        let api = match &self.gitlab_api {
            Some(a) => a.clone(),
            None => format!("https://{host}/api/v4"),
        };
        let url = match tag {
            Some(t) => format!("{api}/projects/{}/releases/{t}", gitlab_id(project)),
            None => format!(
                "{api}/projects/{}/releases/permalink/latest",
                gitlab_id(project)
            ),
        };
        let mut req = Request::new(url).header("Accept", "application/json");
        if let Some(t) = token {
            let Some(t) = t.as_str() else {
                bail!("the GitLab token is not text");
            };
            req = req.header("PRIVATE-TOKEN", t.trim());
        }
        let resp = self.http.get(&req)?;
        if !resp.is_success() {
            let status = resp.status;
            bail!(
                "{}",
                match status {
                    401 => "GitLab rejected the token (401 Unauthorized)".to_string(),
                    404 => format!(
                        "no published release{} found for {host}/{project} (does the project \
                         exist, is it public or is GITLAB_TOKEN set, and does it publish \
                         releases?)",
                        tag.map(|t| format!(" tagged {t}")).unwrap_or_default()
                    ),
                    _ => format!("GitLab API error for {host}/{project}: HTTP {status}"),
                }
            );
        }
        let text = resp.text(16 << 20)?;
        let release: GitlabRelease = serde_json::from_str(&text).map_err(|e| {
            anyhow::anyhow!("unexpected reply from the GitLab API for {project}: {e}")
        })?;
        let release = Arc::new(Release::from(release));
        self.lock().insert(key, release.clone());
        Ok(release)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Release>>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts downloading `url` (a public asset download: no token is sent).
    pub fn download(&self, url: &str) -> Result<Response> {
        let resp = self
            .http
            .get(&Request::new(url).header("Accept", "application/octet-stream"))?;
        if !resp.is_success() {
            bail!("downloading {url}: HTTP {}", resp.status);
        }
        Ok(resp)
    }

    /// A small text file (a checksum list).
    pub fn download_text(&self, url: &str) -> Result<String> {
        self.download(url)?.text(4 << 20)
    }

    /// Starts downloading `asset` of a release of `repo`. With a token and the asset's id, through
    /// the API (`GET /repos/{repo}/releases/assets/{id}`, `Accept: application/octet-stream`),
    /// which works for private repositories: the API answers with a redirect to a storage host,
    /// which the client follows without the token (it never sends `Authorization` to another
    /// origin). Otherwise from the public `browser_download_url`, without a token.
    pub fn download_asset(
        &self,
        repo: &str,
        asset: &Asset,
        token: Option<&SecretBytes>,
    ) -> Result<Response> {
        let (Some(token), Some(id)) = (token, asset.id) else {
            return self.download(&asset.browser_download_url);
        };
        let url = format!("{}/repos/{repo}/releases/assets/{id}", self.api);
        let mut req = self.api_request(url, Some(token))?;
        req.headers
            .retain(|(k, _)| !k.eq_ignore_ascii_case("accept"));
        let resp = self
            .http
            .get(&req.header("Accept", "application/octet-stream"))?;
        if !resp.is_success() {
            let status = resp.status;
            bail!(
                "downloading {} from {repo}: HTTP {status}{}",
                asset.name,
                match status {
                    401 => ": GitHub rejected the token",
                    403 | 404 => ": the token cannot read the repository",
                    _ => "",
                }
            );
        }
        Ok(resp)
    }
}

/// Why an API request failed, with what to do about it.
fn api_error(repo: &str, had_token: bool, resp: Response) -> String {
    let status = resp.status;
    let exhausted = resp.header("x-ratelimit-remaining") == Some("0");
    let reset = resp
        .header("x-ratelimit-reset")
        .and_then(|s| s.parse::<u64>().ok());
    let mut body = String::new();
    let _ = resp.body.take(64 << 10).read_to_string(&mut body);
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("message")?.as_str().map(str::to_string));
    match status {
        403 | 429 if exhausted => {
            let when = reset
                .and_then(|r| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_secs();
                    Some(format!(
                        " (resets in {} min)",
                        r.saturating_sub(now).div_ceil(60)
                    ))
                })
                .unwrap_or_default();
            let hint = if had_token {
                ""
            } else {
                "; set GITHUB_TOKEN, declare a `github_token` secret, or name a token secret with \
                 `token:`"
            };
            format!("GitHub API rate limit exceeded{when}{hint}")
        }
        401 => "GitHub rejected the token (401 Unauthorized)".to_string(),
        404 => format!(
            "no published release found for {repo} (does the repository exist, and does it \
             publish releases?)"
        ),
        _ => match message {
            Some(m) => format!("GitHub API error for {repo}: HTTP {status}: {m}"),
            None => format!("GitHub API error for {repo}: HTTP {status}"),
        },
    }
}
