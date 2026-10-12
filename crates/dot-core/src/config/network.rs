//! Config for the directives that reach the network: `github_release` (install executables from
//! a repository's latest release, on GitHub or GitLab), `download` (a file, an archive or a
//! package from a URL or a release) and `git` (an https checkout kept up to date).
//!
//! The raw shapes here are what serde reads (and what the JSON Schema is generated from); each is
//! validated while parsing, so a bad repository name or path is an error with file and line. The
//! validated specs at the bottom are what the planner sees.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, de::value::MapAccessDeserializer};

use super::raw::is_target_path;
use super::shapes::{Check, Checked, FromScalar, ModeSpec, OrderedMap};
use crate::facts::Os;
use crate::template::is_template;

/// Matches a GitHub repository (`owner/name`) or a template.
pub const GITHUB_REPO_PATTERN: &str = r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$|\{\{|\{%";

/// Matches a `git` repository URL the parser accepts: `https://` without credentials (or
/// `file://`, for local mirrors), or a template.
pub const GIT_URL_PATTERN: &str = r"^(https://[^/?#@]+([/?#].*)?|file://.+)$|\{\{|\{%";

/// Matches an `asset:` platform key: `<os>` or `<os>/<arch>`.
pub const PLATFORM_KEY_PATTERN: &str = r"^(linux|macos|windows)(/[A-Za-z0-9_]+)?$";

/// A GitHub repository name, `owner/name`.
pub fn check_github_repo(repo: &str) -> Result<(), String> {
    let part_ok = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    };
    match repo.split_once('/') {
        Some((owner, name)) if part_ok(owner) && part_ok(name) => Ok(()),
        _ => Err(format!(
            "github_release: `{repo}` is not a GitHub repository; expected `owner/name`"
        )),
    }
}

/// A repository URL for `git`: https only (a new machine has no SSH keys yet), or `file://`.
/// Never with credentials (`https://user:token@host/...`): the URL is on `git`'s command line,
/// which every local user can read, and git saves it in the checkout's `.git/config`.
pub fn check_git_url(url: &str) -> Result<(), String> {
    if let Some(rest) = url.strip_prefix("https://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        if authority.contains('@') {
            // The URL itself is not shown: it may hold a token.
            return Err(
                "git: the repository URL has credentials in it (`user@` or `user:token@`); they \
                 would be on git's command line and saved in .git/config. Leave them out and \
                 let a git credential helper supply them"
                    .into(),
            );
        }
    }
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("file://"));
    match rest {
        Some(r) if !r.trim().is_empty() => Ok(()),
        _ => Err(format!(
            "git: `{url}` is not an https:// URL (SSH and other transports are not supported: a \
             new machine has no keys yet)"
        )),
    }
}

/// A GitLab project path, `group/name` or `group/subgroup/.../name`.
pub fn check_gitlab_project(project: &str) -> Result<(), String> {
    let part_ok = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    };
    let parts: Vec<&str> = project.split('/').collect();
    if parts.len() >= 2 && parts.iter().all(|p| part_ok(p)) {
        Ok(())
    } else {
        Err(format!(
            "`{project}` is not a GitLab project; expected `group/name` (or \
             `group/subgroup/name`)"
        ))
    }
}

/// A host name for `host:` (a self-managed GitLab): letters, digits, `.`, `-`, and a `:port`.
pub fn check_host_name(host: &str) -> Result<(), String> {
    let ok = !host.is_empty()
        && !host.starts_with(['.', '-', ':'])
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{host}` is not a host name (e.g. gitlab.example.com)"
        ))
    }
}

/// Where a release is published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseSource {
    /// GitHub (`DOT_GITHUB_API` for GitHub Enterprise).
    #[default]
    Github,
    /// GitLab: gitlab.com, or a self-managed instance named by `host`.
    Gitlab,
}

impl ReleaseSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ReleaseSource::Github => "github",
            ReleaseSource::Gitlab => "gitlab",
        }
    }
}

/// The default GitLab host.
pub const GITLAB_HOST: &str = "gitlab.com";

/// Checks `repo` (and `host`) for `source`; `what` names the directive.
fn check_release_origin(
    what: &str,
    source: ReleaseSource,
    repo: &str,
    host: Option<&str>,
) -> Result<(), String> {
    match source {
        ReleaseSource::Github => {
            if host.is_some() {
                return Err(format!(
                    "{what}: `host` is for `source: gitlab`; a GitHub Enterprise server is set \
                     with DOT_GITHUB_API"
                ));
            }
            if !is_template(repo) {
                check_github_repo(repo).map_err(|e| e.replace("github_release", what))?;
            }
        }
        ReleaseSource::Gitlab => {
            if !is_template(repo) {
                check_gitlab_project(repo).map_err(|e| format!("{what}: {e}"))?;
            }
            if let Some(h) = host.filter(|h| !is_template(h)) {
                check_host_name(h).map_err(|e| format!("{what}: {e}"))?;
            }
        }
    }
    Ok(())
}

/// One executable to install: the name it gets, and optionally a glob picking the file for it
/// (`godot: 'Godot_v*_linux.x86_64'`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinEntry {
    pub name: String,
    pub from: Option<String>,
}

/// `bin:` — a name, a list of names, or a mapping of the installed name to a glob that picks
/// the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinSpec(pub Vec<BinEntry>);

impl<'de> Deserialize<'de> for BinSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_;
        impl<'de> Visitor<'de> for V_ {
            type Value = BinSpec;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a name, a list of names, or a mapping of name to glob")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<BinSpec, E> {
                Ok(BinSpec(vec![BinEntry {
                    name: v.to_string(),
                    from: None,
                }]))
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<BinSpec, A::Error> {
                let mut out = Vec::new();
                while let Some(name) = seq.next_element::<String>()? {
                    out.push(BinEntry { name, from: None });
                }
                if out.is_empty() {
                    return Err(de::Error::custom("`bin` is empty"));
                }
                Ok(BinSpec(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<BinSpec, A::Error> {
                let m = OrderedMap::<String>::deserialize(MapAccessDeserializer::new(map))?;
                if m.0.is_empty() {
                    return Err(de::Error::custom("`bin` is empty"));
                }
                Ok(BinSpec(
                    m.0.into_iter()
                        .map(|(name, glob)| BinEntry {
                            name,
                            from: Some(glob),
                        })
                        .collect(),
                ))
            }
        }
        d.deserialize_any(V_)
    }
}

impl JsonSchema for BinSpec {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "BinSpec".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "Executables: a name, a list of names, or a mapping of the name to \
                install as to a glob picking the file (`godot: 'Godot_v*_linux.x86_64'`).",
            "anyOf": [
                { "type": "string", "minLength": 1 },
                { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "minProperties": 1
                }
            ]
        })
    }
}

/// An executable name to install: a plain file name.
pub fn check_bin_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(format!(
            "github_release: bin `{name}` must be a plain file name, not a path"
        ));
    }
    Ok(())
}

fn check_target(kind: &str, key: &str, p: &str) -> Result<(), String> {
    if is_template(p) || is_target_path(p) {
        Ok(())
    } else {
        Err(format!(
            "{kind}: {key} `{p}` must be an absolute path or start with `~/`"
        ))
    }
}

/// How a downloaded asset is unpacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
pub enum Extract {
    /// From the asset's file name: `.tar.gz`/`.tgz`, `.tar.xz`/`.txz`, `.zip`, else a raw binary.
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "tar.xz")]
    TarXz,
    #[serde(rename = "zip")]
    Zip,
    /// The asset is the executable itself.
    #[serde(rename = "raw")]
    Raw,
}

impl Extract {
    pub fn as_str(self) -> &'static str {
        match self {
            Extract::Auto => "auto",
            Extract::TarGz => "tar.gz",
            Extract::TarXz => "tar.xz",
            Extract::Zip => "zip",
            Extract::Raw => "raw",
        }
    }
}

/// `<os>` or `<os>/<arch>`: which platforms an `asset:` pattern is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformKey {
    pub os: String,
    pub arch: Option<String>,
}

impl PlatformKey {
    pub fn parse(key: &str) -> Result<PlatformKey, String> {
        let (os, arch) = match key.split_once('/') {
            Some((os, arch)) => (os, Some(arch)),
            None => (key, None),
        };
        if !Os::ALL.contains(&os) {
            return Err(format!(
                "github_release: asset key `{key}`: unknown os `{os}`, expected one of: {} \
                 (optionally followed by `/<arch>`, e.g. `linux/x86_64`)",
                Os::ALL.join(", ")
            ));
        }
        if let Some(a) = arch
            && (a.is_empty() || !a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        {
            return Err(format!(
                "github_release: asset key `{key}`: `{a}` is not an architecture such as \
                 `x86_64` or `aarch64`"
            ));
        }
        Ok(PlatformKey {
            os: os.to_string(),
            arch: arch.map(str::to_string),
        })
    }

    /// Whether this key covers `os`/`arch`; more specific keys rank higher.
    pub fn matches(&self, os: &str, arch: &str) -> Option<u8> {
        if self.os != os {
            return None;
        }
        match &self.arch {
            None => Some(1),
            Some(a) if a == arch => Some(2),
            Some(_) => None,
        }
    }
}

impl fmt::Display for PlatformKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.arch {
            Some(a) => write!(f, "{}/{a}", self.os),
            None => f.write_str(&self.os),
        }
    }
}

/// `asset:` as written: one glob, or a glob per platform.
#[derive(Debug, Clone, PartialEq)]
pub enum AssetSpec {
    Glob(String),
    PerPlatform(Vec<(PlatformKey, String)>),
}

impl AssetSpec {
    /// The glob for `os`/`arch`, or `Err` listing the platforms that have one.
    pub fn for_platform(&self, os: &str, arch: &str) -> Result<&str, String> {
        match self {
            AssetSpec::Glob(g) => Ok(g),
            AssetSpec::PerPlatform(map) => map
                .iter()
                .filter_map(|(k, g)| k.matches(os, arch).map(|rank| (rank, g)))
                .max_by_key(|(rank, _)| *rank)
                .map(|(_, g)| g.as_str())
                .ok_or_else(|| {
                    let keys: Vec<String> = map.iter().map(|(k, _)| k.to_string()).collect();
                    format!(
                        "no `asset:` pattern for {os}/{arch} (patterns are given for: {})",
                        keys.join(", ")
                    )
                }),
        }
    }
}

impl<'de> Deserialize<'de> for AssetSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_;
        impl<'de> Visitor<'de> for V_ {
            type Value = AssetSpec;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a glob, or a mapping of `<os>` or `<os>/<arch>` to a glob")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<AssetSpec, E> {
                if v.trim().is_empty() {
                    return Err(E::custom("github_release: `asset` is empty"));
                }
                Ok(AssetSpec::Glob(v.to_string()))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<AssetSpec, A::Error> {
                let m = OrderedMap::<String>::deserialize(MapAccessDeserializer::new(map))?;
                if m.0.is_empty() {
                    return Err(de::Error::custom(
                        "github_release: `asset` mapping is empty",
                    ));
                }
                let mut out = Vec::new();
                for (k, glob) in m.0 {
                    let key = PlatformKey::parse(&k).map_err(de::Error::custom)?;
                    out.push((key, glob));
                }
                Ok(AssetSpec::PerPlatform(out))
            }
        }
        d.deserialize_any(V_)
    }
}

impl JsonSchema for AssetSpec {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "AssetSpec".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "Which release asset to download: a glob matched against the asset \
                names, or a mapping of `<os>` or `<os>/<arch>` (e.g. `macos`, `linux/aarch64`) \
                to a glob. Default: picked from the OS and architecture.",
            "anyOf": [
                { "type": "string", "minLength": 1 },
                {
                    "type": "object",
                    "propertyNames": { "pattern": PLATFORM_KEY_PATTERN },
                    "additionalProperties": { "type": "string" },
                    "minProperties": 1
                }
            ]
        })
    }
}

fn repo_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "pattern": GITHUB_REPO_PATTERN,
        "description": "GitHub repository, `owner/name`."
    })
}

fn git_url_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "pattern": GIT_URL_PATTERN,
        "description": "Repository URL: https:// (or file://)."
    })
}

fn target_schema(description: &str) -> Schema {
    json_schema!({
        "type": "string",
        "pattern": super::raw::TARGET_PATTERN,
        "description": description
    })
}

fn to_schema(_: &mut SchemaGenerator) -> Schema {
    target_schema("Directory to install into (default: ~/.local/bin).")
}

fn dest_schema(_: &mut SchemaGenerator) -> Schema {
    target_schema("Where the checkout lives.")
}

/// `github_release:` in its mapping form. The scalar form is the repository alone.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubReleaseOpts {
    #[schemars(schema_with = "repo_schema")]
    pub repo: String,
    /// Where the release is published: `github` (default) or `gitlab` (then `repo` is the
    /// project's path, `group/name`).
    #[serde(default)]
    pub source: ReleaseSource,
    /// The GitLab host (default gitlab.com); `source: gitlab` only.
    pub host: Option<String>,
    /// Which asset: a glob, or a glob per platform (default: picked from the OS and CPU).
    pub asset: Option<AssetSpec>,
    /// Executables to install from the asset (default: the repository name): names, or a
    /// mapping of the installed name to a glob matched against the archive's file names.
    pub bin: Option<BinSpec>,
    #[serde(default)]
    #[schemars(schema_with = "to_schema")]
    pub to: Option<String>,
    /// How to unpack the asset (default: auto, from its file name).
    #[serde(default)]
    pub extract: Extract,
    /// Logical name of a secret holding a token for API requests (default: the GITHUB_TOKEN or
    /// GH_TOKEN environment variable; GITLAB_TOKEN for GitLab). Never logged.
    pub token: Option<String>,
    /// Replace an executable of the same name that dot did not install, keeping it as
    /// `<name>.dot-backup` (default: false: such a file is a conflict and the step fails).
    pub replace: Option<bool>,
    /// Install the release's `.deb` (apt) or `.rpm` (dnf, zypper) as this package instead of
    /// taking executables out of an archive. Satisfied while the package is installed from the
    /// latest release (or the `tag:` one).
    pub package: Option<String>,
    /// Install the release with this tag (e.g. `v1.2.3`) instead of the latest one.
    pub tag: Option<String>,
    /// The SHA-256 the chosen asset must have (64 hex digits), checked instead of the checksum
    /// the release publishes. Needs `tag:` (the latest release's asset changes with each
    /// release); with `asset:` per platform, pin one platform per step.
    pub sha256: Option<String>,
}

impl FromScalar for GithubReleaseOpts {
    fn from_scalar(repo: String) -> Self {
        Self {
            repo,
            source: ReleaseSource::Github,
            host: None,
            asset: None,
            bin: None,
            to: None,
            extract: Extract::Auto,
            token: None,
            replace: None,
            package: None,
            tag: None,
            sha256: None,
        }
    }
}

/// A release tag for `github_release: { tag }`: letters, digits, `.`, `_`, `+`, `-` (it goes
/// into an API URL as it is).
pub fn check_release_tag(tag: &str) -> Result<(), String> {
    let ok = !tag.is_empty()
        && !tag.starts_with(['.', '-'])
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
    if ok || is_template(tag) {
        Ok(())
    } else {
        Err(format!(
            "github_release: tag `{tag}` may only use letters, digits, `.`, `_`, `+` and `-`"
        ))
    }
}

impl Check for GithubReleaseOpts {
    fn check(&self) -> Result<(), String> {
        check_release_origin(
            "github_release",
            self.source,
            &self.repo,
            self.host.as_deref(),
        )?;
        if let Some(bin) = &self.bin {
            for b in &bin.0 {
                if !is_template(&b.name) {
                    check_bin_name(&b.name)?;
                }
                if b.from.as_deref().is_some_and(|g| g.trim().is_empty()) {
                    return Err(format!(
                        "github_release: bin `{}`: the glob is empty",
                        b.name
                    ));
                }
            }
        }
        if let Some(to) = &self.to {
            check_target("github_release", "to", to)?;
        }
        if let Some(t) = &self.token
            && t.trim().is_empty()
        {
            return Err("github_release: `token` is empty; name a secret from secrets.yml".into());
        }
        if let Some(t) = &self.tag {
            check_release_tag(t)?;
        }
        if let Some(s) = &self.sha256 {
            check_sha256(s).map_err(|e| e.replacen("download: ", "github_release: ", 1))?;
            if self.tag.is_none() {
                return Err(
                    "github_release: `sha256` needs `tag`: the latest release's asset changes with \
                     every release"
                        .into(),
                );
            }
        }
        if self.extract == Extract::Raw && self.bin.as_ref().is_some_and(|b| b.0.len() > 1) {
            return Err(
                "github_release: `extract: raw` installs the asset itself, so `bin` can name only \
                 one executable"
                    .into(),
            );
        }
        if let Some(p) = &self.package {
            check_package_name("github_release", p)?;
            if self.bin.is_some()
                || self.to.is_some()
                || self.replace.is_some()
                || self.extract != Extract::Auto
            {
                return Err(
                    "github_release: `package` installs a .deb / .rpm through the package manager; \
                     `bin`, `to`, `extract` and `replace` do not apply"
                        .into(),
                );
            }
        }
        Ok(())
    }
}

/// A package name for `package:` (dpkg / rpm): letters, digits, `.`, `_`, `+`, `-`.
fn check_package_name(kind: &str, name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
    if ok || is_template(name) {
        Ok(())
    } else {
        Err(format!("{kind}: `{name}` is not a package name"))
    }
}

/// A `download:` URL: https only (redirects too, when fetched), no credentials in it.
pub fn check_download_url(url: &str) -> Result<(), String> {
    if is_template(url) {
        return Ok(());
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(format!(
            "download: `{url}` is not an https:// URL: a download in the clear can be swapped in \
             transit"
        ));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(format!("download: `{url}` has no host"));
    }
    if authority.contains('@') {
        return Err("download: the URL has credentials in it (`user@`); leave them out".into());
    }
    Ok(())
}

/// A SHA-256 digest: 64 hexadecimal digits.
pub fn check_sha256(v: &str) -> Result<(), String> {
    if is_template(v) || (v.len() == 64 && v.chars().all(|c| c.is_ascii_hexdigit())) {
        Ok(())
    } else {
        Err(format!(
            "download: sha256 `{v}` is not a SHA-256 digest (64 hexadecimal digits)"
        ))
    }
}

fn download_url_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "pattern": r"^https://[^/?#@]+([/?#].*)?$|\{\{|\{%",
        "description": "https:// URL of the file; `{latest}` in it is filled in from `latest`."
    })
}

fn download_dest_schema(_: &mut SchemaGenerator) -> Schema {
    target_schema(
        "Where the file goes; with `extract`, the directory the archive is unpacked into.",
    )
}

/// `download: { release: ... }`: the file is an asset of a project's latest release.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRef {
    /// `owner/name` on GitHub, `group/name` on GitLab.
    pub repo: String,
    /// `github` (default) or `gitlab`.
    #[serde(default)]
    pub source: ReleaseSource,
    /// The GitLab host (default gitlab.com); `source: gitlab` only.
    pub host: Option<String>,
    /// Which asset: a glob, or a glob per platform (default: picked from the OS and CPU).
    pub asset: Option<AssetSpec>,
    /// Logical name of a secret holding a token for the release API (default: GITHUB_TOKEN /
    /// GH_TOKEN, or GITLAB_TOKEN).
    pub token: Option<String>,
}

/// `latest:` — where to ask for the current version, which replaces `{latest}` in `url`
/// (`https://go.dev/VERSION?m=text` -> `go1.23.4`).
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LatestOpts {
    /// https:// URL answering with the version (a small text).
    pub url: String,
    /// A regular expression picking the version out of the answer: its first capture group,
    /// else the whole match (default: the first line, trimmed).
    pub pattern: Option<String>,
}

impl FromScalar for LatestOpts {
    fn from_scalar(url: String) -> Self {
        Self { url, pattern: None }
    }
}

impl Check for LatestOpts {
    fn check(&self) -> Result<(), String> {
        check_download_url(&self.url).map_err(|e| e.replace("download:", "download: latest:"))?;
        if let Some(p) = &self.pattern {
            regex::Regex::new(p).map_err(|e| format!("download: latest: pattern `{p}`: {e}"))?;
        }
        Ok(())
    }
}

impl JsonSchema for ScalarOrChecked<LatestOpts> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "Latest".into()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "Where to ask for the current version, which replaces `{latest}` \
                in `url`: an https URL (its first line), or `{ url, pattern }`.",
            "anyOf": [
                { "type": "string" },
                g.subschema_for::<LatestOpts>()
            ]
        })
    }
}

/// The placeholder in a `download:` URL that `latest:` fills in.
pub const LATEST_PLACEHOLDER: &str = "{latest}";

/// `download:` — a file from an https URL (or a release): saved as a file, unpacked into a
/// directory, or (a `.deb` / `.rpm`) installed through the package manager.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DownloadOpts {
    #[serde(default)]
    #[schemars(schema_with = "download_url_schema")]
    pub url: Option<String>,
    /// Take the file from a project's latest release instead of a fixed `url`.
    pub release: Option<ReleaseRef>,
    /// Ask this URL for the current version, which replaces `{latest}` in `url`.
    pub latest: Option<ScalarOrChecked<LatestOpts>>,
    #[serde(default)]
    #[schemars(schema_with = "download_dest_schema")]
    pub dest: Option<String>,
    /// The file's expected SHA-256 (64 hexadecimal digits): a download that does not have it
    /// is refused. Without it, the step is satisfied while the server says the file has not
    /// changed (ETag / Last-Modified).
    pub sha256: Option<String>,
    /// Where the vendor publishes the file's SHA-256 (https; `{latest}` is filled in as in
    /// `url`): a bare digest (`go1.24.0.linux-amd64.tar.gz.sha256`) or a `sha256sum` list
    /// naming the file. The download is checked against it; no fixed digest is pinned.
    pub sha256_url: Option<String>,
    /// File mode (Unix) of a downloaded file (default "0644").
    pub mode: Option<ModeSpec>,
    /// Unpack an archive into `dest` (a directory): `auto` (from the URL's file name),
    /// `tar.gz`, `tar.xz` or `zip`. Default: the file is saved as it is.
    pub extract: Option<Extract>,
    /// `dest` is root-owned (`/usr/local/...`): written through sudo (default false).
    #[serde(default)]
    pub root: bool,
    /// Install the file (a `.deb` for apt, a `.rpm` for dnf / zypper) as this package instead
    /// of saving it; satisfied while the package is installed.
    pub package: Option<String>,
    /// With `extract`: `dest` is the unpacked tree itself, replaced whole on an update (old
    /// files do not linger), instead of the archive being merged into it. A tree `dot` did not
    /// install is kept as `<dest>.dot-backup`.
    #[serde(default)]
    pub replace: bool,
    /// With `extract`: leading directories of the archive to strip (`1` for `go/...`); each
    /// must be the archive's only top-level entry.
    pub strip: Option<u32>,
    /// With `extract`: executables in the unpacked tree to link into `bin_dir`: paths in the
    /// tree (`bin/zed`, linked by their file name), or a mapping of link name to a glob over
    /// paths in the tree (`godot: 'Godot_v*'`).
    pub bin: Option<BinSpec>,
    /// Where `bin` links go (default: ~/.local/bin).
    #[serde(default)]
    #[schemars(schema_with = "to_schema")]
    pub bin_dir: Option<String>,
}

impl Check for DownloadOpts {
    fn check(&self) -> Result<(), String> {
        match (&self.url, &self.release) {
            (Some(url), None) => {
                check_download_url(url)?;
                match (&self.latest, url.contains(LATEST_PLACEHOLDER)) {
                    (Some(_), false) => {
                        return Err(
                            "download: `latest` fills in `{latest}` in `url`, which has none"
                                .into(),
                        );
                    }
                    (None, true) => {
                        return Err(
                            "download: `url` has `{latest}`: say where to ask for it with \
                             `latest: <url>`"
                                .into(),
                        );
                    }
                    _ => {}
                }
            }
            (None, Some(r)) => {
                check_release_origin("download: release", r.source, &r.repo, r.host.as_deref())?;
                if self.latest.is_some() {
                    return Err("download: `latest` is for `url`; a release is always the \
                                latest"
                        .into());
                }
            }
            _ => return Err("download: expected exactly one of `url` and `release`".into()),
        }
        if let Some(s) = &self.sha256 {
            check_sha256(s)?;
        }
        if let Some(u) = &self.sha256_url {
            if self.sha256.is_some() {
                return Err("download: `sha256` and `sha256_url` cannot be combined".into());
            }
            if self.release.is_some() {
                return Err(
                    "download: `sha256_url` is for `url`; a release's own checksums are used"
                        .into(),
                );
            }
            check_download_url(u)?;
            if u.contains(LATEST_PLACEHOLDER) && self.latest.is_none() {
                return Err(
                    "download: `sha256_url` has `{latest}`, which only `latest:` fills in".into(),
                );
            }
        }
        if self.extract.is_none() && (self.replace || self.strip.is_some() || self.bin.is_some()) {
            return Err(
                "download: `replace`, `strip` and `bin` are for an unpacked archive (`extract`)"
                    .into(),
            );
        }
        if self.bin_dir.is_some() && self.bin.is_none() {
            return Err("download: `bin_dir` is where `bin` links go; there is no `bin`".into());
        }
        if let Some(d) = &self.bin_dir {
            check_target("download", "bin_dir", d)?;
        }
        if let Some(bin) = &self.bin {
            for b in &bin.0 {
                if b.from.is_some() && !is_template(&b.name) {
                    check_bin_name(&b.name).map_err(|e| e.replace("github_release", "download"))?;
                }
                let path = b.from.as_deref().unwrap_or(&b.name);
                if path.trim().is_empty() || path.starts_with('/') || path.contains("..") {
                    return Err(format!(
                        "download: bin `{path}` must be a path inside the unpacked tree"
                    ));
                }
            }
        }
        match (&self.dest, &self.package) {
            (Some(d), None) => check_target("download", "dest", d)?,
            (None, Some(p)) => {
                check_package_name("download", p)?;
                if self.extract.is_some() || self.mode.is_some() || self.root {
                    return Err(
                        "download: `package` installs the file through the package manager; \
                         `extract`, `mode` and `root` do not apply"
                            .into(),
                    );
                }
            }
            _ => return Err("download: expected exactly one of `dest` and `package`".into()),
        }
        if self.extract.is_some() && self.mode.is_some() {
            return Err("download: `mode` is for a saved file, not an unpacked archive".into());
        }
        Ok(())
    }
}

/// The value of a step's `download:` key.
pub type DownloadField = Checked<DownloadOpts>;

/// Where a download goes.
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadTo {
    /// A file, or with `extract` a directory.
    Path(String),
    /// Installed as this package.
    Package(String),
}

/// Where a download comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadFrom {
    /// This URL (with `{latest}` filled in from `latest`, if any).
    Url {
        url: String,
        latest: Option<LatestOpts>,
    },
    /// An asset of a project's latest release.
    Release(ReleaseRef),
}

/// A checked `download:` step.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadSpec {
    pub from: DownloadFrom,
    pub to: DownloadTo,
    pub sha256: Option<String>,
    /// Where the vendor publishes the file's digest.
    pub sha256_url: Option<String>,
    pub mode: Option<ModeSpec>,
    /// `None`: saved as it is.
    pub extract: Option<Extract>,
    pub root: bool,
    /// Replace the unpacked tree whole.
    pub replace: bool,
    /// Leading directories to strip from the archive.
    pub strip: u32,
    /// Executables of the unpacked tree to link into `bin_dir`.
    pub bin: Vec<BinEntry>,
    /// `None`: `~/.local/bin`.
    pub bin_dir: Option<String>,
}

impl From<DownloadOpts> for DownloadSpec {
    fn from(o: DownloadOpts) -> Self {
        Self {
            to: match (o.dest, o.package) {
                (_, Some(p)) => DownloadTo::Package(p),
                (Some(d), None) => DownloadTo::Path(d),
                (None, None) => unreachable!("checked while parsing"),
            },
            from: match (o.url, o.release) {
                (Some(url), _) => DownloadFrom::Url {
                    url,
                    latest: o.latest.map(|l| l.0),
                },
                (None, Some(r)) => DownloadFrom::Release(r),
                (None, None) => unreachable!("checked while parsing"),
            },
            sha256: o.sha256.map(|s| s.to_ascii_lowercase()),
            sha256_url: o.sha256_url,
            mode: o.mode,
            extract: o.extract.filter(|e| *e != Extract::Raw),
            root: o.root,
            replace: o.replace,
            strip: o.strip.unwrap_or(0),
            bin: o.bin.map(|b| b.0).unwrap_or_default(),
            bin_dir: o.bin_dir,
        }
    }
}

/// `git:`
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitOpts {
    #[schemars(schema_with = "git_url_schema")]
    pub repo: String,
    #[schemars(schema_with = "dest_schema")]
    pub dest: String,
    /// Branch to clone and follow (default: the remote's default branch, or the checkout's
    /// current branch).
    pub branch: Option<String>,
    /// Fast-forward an existing clean checkout to the remote (default: true).
    pub update: Option<bool>,
    /// Clone only the last `depth` commits (`git clone --depth`, e.g. 1); updates fetch what is
    /// new on top. Default: the whole history.
    pub depth: Option<u32>,
}

impl Check for GitOpts {
    fn check(&self) -> Result<(), String> {
        if !is_template(&self.repo) {
            check_git_url(&self.repo)?;
        }
        check_target("git", "dest", &self.dest)?;
        if let Some(b) = &self.branch
            && (b.trim().is_empty() || b.starts_with('-'))
        {
            return Err(format!("git: `{b}` is not a branch name"));
        }
        if self.depth == Some(0) {
            return Err("git: `depth` must be at least 1".into());
        }
        Ok(())
    }
}

/// Either the scalar shorthand of `T` or its mapping, checked with `T::check` while the parser
/// still knows where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct ScalarOrChecked<T>(pub T);

impl<'de, T: FromScalar + Check + DeserializeOwned> Deserialize<'de> for ScalarOrChecked<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_<T>(PhantomData<T>);
        impl<'de, T: FromScalar + Check + DeserializeOwned> Visitor<'de> for V_<T> {
            type Value = ScalarOrChecked<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string or a mapping")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                let t = T::from_scalar(v.to_string());
                t.check().map_err(E::custom)?;
                Ok(ScalarOrChecked(t))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                let t = T::deserialize(MapAccessDeserializer::new(map))?;
                t.check().map_err(de::Error::custom)?;
                Ok(ScalarOrChecked(t))
            }
        }
        d.deserialize_any(V_(PhantomData))
    }
}

/// The value of a step's `github_release:` key.
pub type GithubReleaseField = ScalarOrChecked<GithubReleaseOpts>;

/// The value of a step's `git:` key.
pub type GitField = Checked<GitOpts>;

impl JsonSchema for ScalarOrChecked<GithubReleaseOpts> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "GithubRelease".into()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "Install executables from a GitHub repository's latest release (or \
                the one `tag:` names): `owner/name`, or a mapping.",
            "anyOf": [
                { "type": "string", "pattern": GITHUB_REPO_PATTERN },
                g.subschema_for::<GithubReleaseOpts>()
            ]
        })
    }
}

/// The JSON Schema of the `git:` key.
pub fn git_schema(g: &mut SchemaGenerator) -> Schema {
    let mut s = g.subschema_for::<GitOpts>();
    s.insert(
        "description".into(),
        "Clone a repository over https, and fast-forward a clean checkout of it.".into(),
    );
    s
}

/// The JSON Schema of the `download:` key.
pub fn download_schema(g: &mut SchemaGenerator) -> Schema {
    let mut s = g.subschema_for::<DownloadOpts>();
    s.insert(
        "description".into(),
        "A file from an https URL or a release: saved (`dest`), unpacked (`dest` + `extract`), \
         or a .deb / .rpm installed as a package (`package`)."
            .into(),
    );
    s
}

/// A checked `github_release:` step.
#[derive(Debug, Clone, PartialEq)]
pub struct GithubReleaseSpec {
    /// `owner/name` (GitLab: the project's path), possibly templated.
    pub repo: String,
    pub source: ReleaseSource,
    /// GitLab host; `None`: gitlab.com.
    pub host: Option<String>,
    pub asset: Option<AssetSpec>,
    /// Empty: the repository name.
    pub bin: Vec<BinEntry>,
    /// `None`: `~/.local/bin`.
    pub to: Option<String>,
    pub extract: Extract,
    /// Logical secret name of a GitHub token.
    pub token: Option<String>,
    /// Replace (and back up) executables `dot` did not install.
    pub replace: bool,
    /// Install the release's `.deb` / `.rpm` as this package.
    pub package: Option<String>,
    /// The release's tag; `None`: the latest release.
    pub tag: Option<String>,
    /// The asset's pinned SHA-256 (lowercase).
    pub sha256: Option<String>,
}

impl From<GithubReleaseOpts> for GithubReleaseSpec {
    fn from(o: GithubReleaseOpts) -> Self {
        Self {
            repo: o.repo,
            source: o.source,
            host: o.host,
            asset: o.asset,
            bin: o.bin.map(|b| b.0).unwrap_or_default(),
            to: o.to,
            extract: o.extract,
            token: o.token,
            replace: o.replace.unwrap_or(false),
            package: o.package,
            tag: o.tag,
            sha256: o.sha256.map(|s| s.to_ascii_lowercase()),
        }
    }
}

/// A checked `git:` step.
#[derive(Debug, Clone, PartialEq)]
pub struct GitSpec {
    pub repo: String,
    pub dest: String,
    pub branch: Option<String>,
    pub update: bool,
    /// `git clone --depth`.
    pub depth: Option<u32>,
}

impl From<GitOpts> for GitSpec {
    fn from(o: GitOpts) -> Self {
        Self {
            repo: o.repo,
            dest: o.dest,
            branch: o.branch,
            update: o.update.unwrap_or(true),
            depth: o.depth,
        }
    }
}

#[cfg(test)]
mod tests;
