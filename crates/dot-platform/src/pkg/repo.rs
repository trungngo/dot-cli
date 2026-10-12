//! Package repositories (`package_repo`): the files, argv and output parsers for apt, dnf,
//! zypper, Homebrew taps, scoop buckets and winget sources. Everything here is pure or goes
//! through a [`Runner`], so tests assert exact file contents and argv; the directive
//! (`dot-directives`' `package_repo`) decides what to do with them.
//!
//! Downloads (signing keys) and the "does the vendor publish this suite" probe go through `curl`
//! on the runner, like Homebrew's cask API lookups: `dot` itself has no HTTP client for this,
//! and a key must be written to a file byte for byte (a binary keyring is not text).

use std::path::Path;

use anyhow::{Context as _, Result, bail};

use crate::exec::{Cmd, Runner};

/// Where apt keyrings go (`/etc/apt/keyrings/<name>.gpg`), as Debian recommends for keys of
/// third-party repositories.
pub const APT_KEYRING_DIR: &str = "/etc/apt/keyrings";
/// Where apt sources go (`/etc/apt/sources.list.d/<name>.sources`).
pub const APT_SOURCES_DIR: &str = "/etc/apt/sources.list.d";
/// Where dnf / yum repositories go (`/etc/yum.repos.d/<name>.repo`).
pub const YUM_REPOS_DIR: &str = "/etc/yum.repos.d";

/// The first line of every file `dot` writes for a repository.
pub fn managed_header(name: &str) -> String {
    format!("# Managed by dot (package_repo {name}); changes are overwritten.\n")
}

/// Debian's name for the architecture `dot` reports as the `arch` fact (Rust's names).
pub fn dpkg_arch(arch: &str) -> Option<&'static str> {
    Some(match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" | "armv7" => "armhf",
        "x86" | "i386" | "i686" => "i386",
        "powerpc64" | "powerpc64le" => "ppc64el",
        "riscv64" => "riscv64",
        "s390x" => "s390x",
        "loongarch64" => "loong64",
        _ => return None,
    })
}

/// An apt source in deb822 form (`/etc/apt/sources.list.d/<name>.sources`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deb822 {
    pub name: String,
    pub url: String,
    pub suites: Vec<String>,
    /// Empty for a flat repository.
    pub components: Vec<String>,
    /// Empty: not restricted.
    pub architectures: Vec<String>,
    /// The keyring the repository is signed with.
    pub signed_by: Option<String>,
}

impl Deb822 {
    /// The file's content.
    pub fn render(&self) -> String {
        let mut s = managed_header(&self.name);
        s.push_str("Types: deb\n");
        s.push_str(&format!("URIs: {}\n", self.url));
        s.push_str(&format!("Suites: {}\n", self.suites.join(" ")));
        if !self.components.is_empty() {
            s.push_str(&format!("Components: {}\n", self.components.join(" ")));
        }
        if !self.architectures.is_empty() {
            s.push_str(&format!(
                "Architectures: {}\n",
                self.architectures.join(" ")
            ));
        }
        if let Some(k) = &self.signed_by {
            s.push_str(&format!("Signed-By: {k}\n"));
        }
        s
    }

    /// The same source in apt's one-line format, for a `.list` file: one `deb` line per suite,
    /// `arch=` and `signed-by=` in its options.
    pub fn render_list(&self) -> String {
        let mut s = managed_header(&self.name);
        let mut opts = Vec::new();
        if !self.architectures.is_empty() {
            opts.push(format!("arch={}", self.architectures.join(",")));
        }
        if let Some(k) = &self.signed_by {
            opts.push(format!("signed-by={k}"));
        }
        let opts = if opts.is_empty() {
            String::new()
        } else {
            format!("[{}] ", opts.join(" "))
        };
        for suite in &self.suites {
            let mut line = format!("deb {opts}{} {suite}", self.url);
            for c in &self.components {
                line.push(' ');
                line.push_str(c);
            }
            s.push_str(&line);
            s.push('\n');
        }
        s
    }

    /// The file's content in `format`.
    pub fn render_as(&self, format: SourceFormat) -> String {
        match format {
            SourceFormat::Deb822 => self.render(),
            SourceFormat::OneLine => self.render_list(),
        }
    }

    /// The same source with `suite` as its only suite.
    pub fn with_suite(&self, suite: &str) -> Deb822 {
        Deb822 {
            suites: vec![suite.to_string()],
            ..self.clone()
        }
    }
}

/// How apt reads a sources file: by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    /// `.sources`
    Deb822,
    /// `.list`
    OneLine,
}

impl SourceFormat {
    /// `None`: apt ignores a file with this name.
    pub fn of(path: &Path) -> Option<SourceFormat> {
        match path.extension()?.to_str()? {
            "sources" => Some(SourceFormat::Deb822),
            "list" => Some(SourceFormat::OneLine),
            _ => None,
        }
    }
}

/// How apt reads a keyring: `.asc` is ASCII-armored, `.gpg` binary (anything else it ignores).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyFormat {
    Binary,
    Armored,
}

impl KeyFormat {
    pub fn of(path: &Path) -> Option<KeyFormat> {
        match path.extension()?.to_str()? {
            "gpg" => Some(KeyFormat::Binary),
            "asc" => Some(KeyFormat::Armored),
            _ => None,
        }
    }
}

/// One repository a sources file lists: its URI (no trailing `/`) and suites. Disabled deb822
/// stanzas (`Enabled: no`) and comments are left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSource {
    pub uri: String,
    pub suites: Vec<String>,
}

/// The repositories in a sources file of `format` (`deb` and `deb-src` alike).
pub fn parse_sources(text: &str, format: SourceFormat) -> Vec<ListedSource> {
    let trim = |u: &str| u.trim_end_matches('/').to_string();
    match format {
        SourceFormat::OneLine => text
            .lines()
            .map(|l| l.split('#').next().unwrap_or("").trim())
            .filter_map(|l| {
                let mut words = l.split_whitespace();
                if !matches!(words.next()?, "deb" | "deb-src") {
                    return None;
                }
                let mut uri = words.next()?;
                if uri.starts_with('[') {
                    // Options: `[arch=amd64 signed-by=/x]`, possibly over several words.
                    while !uri.ends_with(']') {
                        uri = words.next()?;
                    }
                    uri = words.next()?;
                }
                Some(ListedSource {
                    uri: trim(uri),
                    suites: vec![words.next()?.to_string()],
                })
            })
            .collect(),
        SourceFormat::Deb822 => {
            let mut out = Vec::new();
            for stanza in text.split("\n\n") {
                let mut uris: Vec<String> = Vec::new();
                let mut suites: Vec<String> = Vec::new();
                let mut enabled = true;
                for line in stanza.lines() {
                    if line.starts_with('#') || line.starts_with(char::is_whitespace) {
                        continue;
                    }
                    let Some((k, v)) = line.split_once(':') else {
                        continue;
                    };
                    match k.trim().to_ascii_lowercase().as_str() {
                        "uris" => uris = v.split_whitespace().map(trim).collect(),
                        "suites" => suites = v.split_whitespace().map(str::to_string).collect(),
                        "enabled" => enabled = !v.trim().eq_ignore_ascii_case("no"),
                        _ => {}
                    }
                }
                if enabled {
                    out.extend(uris.into_iter().map(|uri| ListedSource {
                        uri,
                        suites: suites.clone(),
                    }));
                }
            }
            out
        }
    }
}

/// The sources files apt reads besides `managed`: `sources.list` next to its directory and every
/// `.list` / `.sources` file in it. `managed` is `<dir>/sources.list.d/<name>.<ext>`.
pub fn other_source_files(managed: &Path) -> Vec<std::path::PathBuf> {
    let Some(dir) = managed.parent() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(etc_apt) = dir.parent() {
        let main = etc_apt.join("sources.list");
        if main.is_file() {
            out.push(main);
        }
    }
    let mut listed: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p != managed && SourceFormat::of(p).is_some() && p.is_file())
        .collect();
    listed.sort();
    out.extend(listed);
    out
}

/// The files among `others` that list repository `uri` with one of `suites`: with a different
/// `Signed-By` apt refuses to read any source at all, and at best they are duplicates.
pub fn duplicate_sources(
    uri: &str,
    suites: &[String],
    others: &[std::path::PathBuf],
) -> Vec<std::path::PathBuf> {
    let uri = uri.trim_end_matches('/');
    others
        .iter()
        .filter(|p| {
            let format = SourceFormat::of(p).unwrap_or(SourceFormat::OneLine);
            let Ok(text) = std::fs::read_to_string(p) else {
                return false;
            };
            parse_sources(&text, format)
                .iter()
                .any(|s| s.uri == uri && s.suites.iter().any(|x| suites.contains(x)))
        })
        .cloned()
        .collect()
}

/// The URL whose presence says a repository publishes `suite`: `<url>/dists/<suite>/Release`,
/// or `<url>/<suite>Release` for a flat repository (a suite ending in `/`).
pub fn release_url(url: &str, suite: &str) -> String {
    let base = url.trim_end_matches('/');
    if suite.ends_with('/') {
        format!("{base}/{}Release", suite.trim_start_matches('/'))
    } else {
        format!("{base}/dists/{suite}/Release")
    }
}

/// A dnf / yum `.repo` file. Without a key, packages are not signature-checked.
pub fn dnf_repo_file(name: &str, description: &str, baseurl: &str, gpgkey: Option<&str>) -> String {
    let mut s = managed_header(name);
    s.push_str(&format!(
        "[{name}]\nname={description}\nbaseurl={baseurl}\nenabled=1\n"
    ));
    match gpgkey {
        Some(k) => s.push_str(&format!("gpgcheck=1\ngpgkey={k}\n")),
        None => s.push_str("gpgcheck=0\n"),
    }
    s
}

/// Whether a key file is ASCII-armored (apt wants binary keyrings under `.gpg` names).
pub fn is_armored(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    String::from_utf8_lossy(head).contains("-----BEGIN PGP PUBLIC KEY BLOCK-----")
}

/// A fingerprint as `gpg` prints it in `fpr` records: upper case, no spaces.
pub fn normalize_fingerprint(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Whether `s` (a normalized fingerprint) is a whole OpenPGP fingerprint: 40 hex digits (v4)
/// or 64 (v5 / v6). Key ids (a fingerprint's last 16 or 8 digits) are not: they name a key
/// without pinning it.
pub fn is_full_fingerprint(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// One primary key in `gpg --with-colons` output, with its subkeys' fingerprints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpgKey {
    pub fingerprint: String,
    pub subkeys: Vec<String>,
}

/// The primary keys (`pub` / `sec` records, each followed by its `fpr`) and their subkeys
/// (`sub` / `ssb`) in `gpg --with-colons` output.
pub fn parse_gpg_keys(colons: &str) -> Vec<GpgKey> {
    enum Next {
        Nothing,
        Primary,
        Subkey,
    }
    let mut keys: Vec<GpgKey> = Vec::new();
    let mut next = Next::Nothing;
    for line in colons.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        match fields[0] {
            "pub" | "sec" => {
                keys.push(GpgKey {
                    fingerprint: String::new(),
                    subkeys: Vec::new(),
                });
                next = Next::Primary;
            }
            "sub" | "ssb" => next = Next::Subkey,
            "fpr" => {
                let fpr = normalize_fingerprint(fields.get(9).copied().unwrap_or(""));
                match (&next, keys.last_mut()) {
                    (Next::Primary, Some(k)) => k.fingerprint = fpr,
                    (Next::Subkey, Some(k)) => k.subkeys.push(fpr),
                    _ => {}
                }
                next = Next::Nothing;
            }
            _ => {}
        }
    }
    keys
}

/// Checks that a key file holds exactly one primary key, and that its fingerprint is
/// `expected` in full: apt trusts every key in a `Signed-By` keyring, so a second key (from a
/// tampered download) must never ride along, and neither a subkey nor a key id counts as the
/// pinned key. `Err`: why not, for a message.
pub fn check_single_key(expected: &str, keys: &[GpgKey]) -> Result<(), String> {
    check_pinned_keys(&[expected.to_string()], keys)
}

/// Checks that every primary key a key file holds is one of `pinned` (whole fingerprints),
/// and that it holds at least one: a vendor rotating its key ships both in one keyring
/// (cli.github.com), and pins both; apt trusts every key in a `Signed-By` keyring, so any key
/// not pinned (from a tampered download) is refused. A pinned key the file lacks is fine (the
/// old one of a finished rotation). `Err`: why not, for a message.
pub fn check_pinned_keys(pinned: &[String], keys: &[GpgKey]) -> Result<(), String> {
    let want: Vec<String> = pinned.iter().map(|p| normalize_fingerprint(p)).collect();
    if let Some((bad, _)) = pinned
        .iter()
        .zip(&want)
        .find(|(_, w)| !is_full_fingerprint(w))
    {
        return Err(format!(
            "`{bad}` is not a whole fingerprint (40 or 64 hexadecimal digits)"
        ));
    }
    let shown = || want.join(" or ");
    if keys.is_empty() {
        return Err("it holds no readable key".into());
    }
    let unpinned: Vec<&GpgKey> = keys
        .iter()
        .filter(|k| !want.contains(&k.fingerprint))
        .collect();
    match (keys, unpinned.as_slice()) {
        (_, []) => Ok(()),
        ([k], _) if want.iter().any(|w| k.subkeys.contains(w)) => Err(format!(
            "{} is one of its subkeys, not its primary key {}",
            shown(),
            k.fingerprint
        )),
        ([k], _) => Err(format!("its key is {}, not {}", k.fingerprint, shown())),
        (many, _) => Err(format!(
            "it holds {} keys ({}), and only {} may be trusted",
            many.len(),
            many.iter()
                .map(|k| k.fingerprint.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            shown()
        )),
    }
}

/// `gpg` with a throwaway home (nothing is imported into the user's keyring) and no questions.
fn gpg(home: &Path) -> Cmd {
    Cmd::new("gpg").args([
        "--batch".to_string(),
        "--no-tty".to_string(),
        "--homedir".to_string(),
        home.display().to_string(),
    ])
}

/// Lists the keys in `key` (armored or not) with their fingerprints, importing nothing.
pub fn gpg_show_keys(home: &Path, key: &Path) -> Cmd {
    gpg(home).args([
        "--show-keys".to_string(),
        "--with-colons".to_string(),
        "--with-fingerprint".to_string(),
        key.display().to_string(),
    ])
}

/// Converts an ASCII-armored key to a binary keyring.
pub fn gpg_dearmor(home: &Path, input: &Path, output: &Path) -> Cmd {
    gpg(home).args([
        "--yes".to_string(),
        "--output".to_string(),
        output.display().to_string(),
        "--dearmor".to_string(),
        input.display().to_string(),
    ])
}

/// The keys in `key`, by `gpg --show-keys`. `home` must be a private (0700) directory; it is
/// left empty.
pub fn key_list(runner: &dyn Runner, home: &Path, key: &Path) -> Result<Vec<GpgKey>> {
    let cmd = gpg_show_keys(home, key);
    let out = runner
        .output(&cmd)
        .with_context(|| format!("cannot run `{}`", cmd.display()))?;
    if !out.success() {
        return Err(out.failure(&cmd));
    }
    Ok(parse_gpg_keys(&out.stdout))
}

/// `install -d -m 0755 <dir>` (as root through the caller).
pub fn install_dir(dir: &Path) -> Cmd {
    Cmd::new("install")
        .args(["-d", "-m", "0755"])
        .arg(dir.display().to_string())
}

/// `install -m <mode> <src> <dest>` (as root through the caller): the file lands whole, with
/// root as its owner.
pub fn install_file(src: &Path, dest: &Path, mode: u32) -> Cmd {
    Cmd::new("install").args([
        "-m".to_string(),
        format!("{mode:04o}"),
        src.display().to_string(),
        dest.display().to_string(),
    ])
}

/// Downloads `url` to `dest` with `curl`: https only (redirects too), HTTP errors fail.
pub fn download(runner: &dyn Runner, url: &str, dest: &Path) -> Result<()> {
    if !url.starts_with("https://") {
        bail!("refusing to download {url}: signing keys are only fetched over https");
    }
    let cmd = Cmd::new("curl").args([
        "-fsSL".to_string(),
        // https only, redirects included, TLS 1.2 or later.
        "--proto".to_string(),
        "=https".to_string(),
        "--proto-redir".to_string(),
        "=https".to_string(),
        "--tlsv1.2".to_string(),
        "--retry".to_string(),
        "2".to_string(),
        "--max-time".to_string(),
        "120".to_string(),
        "-o".to_string(),
        dest.display().to_string(),
        url.to_string(),
    ]);
    let out = runner
        .output(&cmd)
        .with_context(|| format!("cannot run `{}`", cmd.display()))?;
    if !out.success() {
        bail!(
            "downloading {url} failed: curl {}{}",
            out.describe_exit(),
            out.last_line()
                .map(|l| format!(": {l}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// Whether `url` exists, by an HTTP HEAD request with `curl`: `Ok(false)` for an HTTP error
/// (404, 403: the repository does not publish it), `Err` when it cannot be told (offline, DNS,
/// no curl).
pub fn url_exists(runner: &dyn Runner, url: &str) -> Result<bool, String> {
    // A repository may be served over http (apt checks its signatures); nothing else.
    let cmd = Cmd::new("curl").args([
        "-fsSIL",
        "--proto",
        "=http,https",
        "--proto-redir",
        "=http,https",
        "--max-time",
        "20",
        url,
    ]);
    match runner.output(&cmd) {
        Ok(o) if o.success() => Ok(true),
        // curl: 22 = the server answered with an HTTP error.
        Ok(o) if o.code == Some(22) => Ok(false),
        Ok(o) => Err(format!(
            "curl {}{}",
            o.describe_exit(),
            o.last_line().map(|l| format!(": {l}")).unwrap_or_default()
        )),
        Err(e) => Err(format!("cannot run curl: {e}")),
    }
}

/// How repository keys are downloaded and suites probed. Injectable, so tests never use the
/// network; the real one is [`CurlRepoFetcher`].
pub trait RepoFetcher: Send + Sync {
    /// Downloads `url` to `dest`, byte for byte.
    fn download(&self, url: &str, dest: &Path) -> Result<()>;
    /// Whether `url` exists (HTTP HEAD): `Err` when that cannot be told.
    fn exists(&self, url: &str) -> Result<bool, String>;
}

/// [`download`] and [`url_exists`] with `curl` on a [`Runner`].
pub struct CurlRepoFetcher {
    runner: std::sync::Arc<dyn Runner>,
}

impl CurlRepoFetcher {
    pub fn new(runner: std::sync::Arc<dyn Runner>) -> Self {
        Self { runner }
    }
}

impl RepoFetcher for CurlRepoFetcher {
    fn download(&self, url: &str, dest: &Path) -> Result<()> {
        download(self.runner.as_ref(), url, dest)
    }

    fn exists(&self, url: &str) -> Result<bool, String> {
        url_exists(self.runner.as_ref(), url)
    }
}

/// `brew tap`: the taps, lower case.
pub fn parse_brew_taps(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// `brew trust --json v1`: the trusted taps, lower case (entries are names, or objects with a
/// `name`).
pub fn parse_trusted_taps(json: &str) -> Result<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(json.trim())
        .map_err(|e| anyhow::anyhow!("cannot read `brew trust --json v1` output: {e}"))?;
    Ok(v.get("taps")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
        .filter_map(|t| match t {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Object(o) => o.get("name")?.as_str().map(str::to_string),
            _ => None,
        })
        .map(|s| s.to_ascii_lowercase())
        .collect())
}

/// The first column of a table printed under a line of dashes (`scoop bucket list`, `winget
/// source list`), lower case.
pub fn parse_table_names(text: &str) -> Vec<String> {
    super::table_rows(text)
        .into_iter()
        .map(|r| r[0].to_ascii_lowercase())
        .collect()
}

/// `zypper repos --uri`: `(alias, uri)` of each repository, from its `|` table (columns found by
/// their header, `Alias` and `URI`).
pub fn parse_zypper_repos(text: &str) -> Vec<(String, String)> {
    let mut alias_col = None;
    let mut uri_col = None;
    let mut out = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split('|').map(str::trim).collect();
        if cols.len() < 2 {
            continue;
        }
        if alias_col.is_none() {
            alias_col = cols.iter().position(|c| *c == "Alias");
            uri_col = cols.iter().position(|c| *c == "URI");
            continue;
        }
        if let (Some(a), Some(u)) = (alias_col, uri_col)
            && let (Some(alias), Some(uri)) = (cols.get(a), cols.get(u))
        {
            out.push((alias.to_string(), uri.to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::fake::FakeRunner;

    #[test]
    fn deb822_sources() {
        let d = Deb822 {
            name: "docker".into(),
            url: "https://download.docker.com/linux/ubuntu".into(),
            suites: vec!["noble".into()],
            components: vec!["stable".into()],
            architectures: vec!["amd64".into()],
            signed_by: Some("/etc/apt/keyrings/docker.gpg".into()),
        };
        assert_eq!(
            d.render(),
            "# Managed by dot (package_repo docker); changes are overwritten.\n\
             Types: deb\n\
             URIs: https://download.docker.com/linux/ubuntu\n\
             Suites: noble\n\
             Components: stable\n\
             Architectures: amd64\n\
             Signed-By: /etc/apt/keyrings/docker.gpg\n"
        );
        let flat = Deb822 {
            suites: vec!["./".into()],
            components: vec![],
            architectures: vec![],
            signed_by: None,
            ..d.clone()
        };
        let text = flat.render();
        assert!(text.contains("Suites: ./\n"));
        assert!(!text.contains("Components") && !text.contains("Architectures"));
        assert!(!text.contains("Signed-By"));
        assert_eq!(d.with_suite("jammy").suites, ["jammy"]);
    }

    #[test]
    fn one_line_sources_for_list_files() {
        let d = Deb822 {
            name: "docker".into(),
            url: "https://download.docker.com/linux/ubuntu".into(),
            suites: vec!["noble".into()],
            components: vec!["stable".into()],
            architectures: vec!["amd64".into(), "arm64".into()],
            signed_by: Some("/etc/apt/keyrings/docker.asc".into()),
        };
        assert_eq!(
            d.render_as(SourceFormat::OneLine),
            "# Managed by dot (package_repo docker); changes are overwritten.\n\
             deb [arch=amd64,arm64 signed-by=/etc/apt/keyrings/docker.asc] \
             https://download.docker.com/linux/ubuntu noble stable\n"
        );
        let flat = Deb822 {
            suites: vec!["./".into(), "b".into()],
            components: vec![],
            architectures: vec![],
            signed_by: None,
            ..d.clone()
        };
        assert_eq!(
            flat.render_list().lines().skip(1).collect::<Vec<_>>(),
            [
                "deb https://download.docker.com/linux/ubuntu ./",
                "deb https://download.docker.com/linux/ubuntu b"
            ]
        );
        // What dot writes, apt (and dot) reads back.
        for f in [SourceFormat::OneLine, SourceFormat::Deb822] {
            assert_eq!(
                parse_sources(&d.render_as(f), f),
                [ListedSource {
                    uri: "https://download.docker.com/linux/ubuntu".into(),
                    suites: vec!["noble".into()]
                }]
            );
        }
        assert_eq!(
            SourceFormat::of(Path::new("/x/a.list")),
            Some(SourceFormat::OneLine)
        );
        assert_eq!(SourceFormat::of(Path::new("/x/a.txt")), None);
        assert_eq!(
            KeyFormat::of(Path::new("/k/d.asc")),
            Some(KeyFormat::Armored)
        );
        assert_eq!(
            KeyFormat::of(Path::new("/k/d.gpg")),
            Some(KeyFormat::Binary)
        );
    }

    #[test]
    fn other_entries_for_the_same_repository_are_found() {
        let one_line = "# Docker's instructions\n\
            deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu/ noble stable\n\
            deb-src http://archive.ubuntu.com/ubuntu noble main # sources\n";
        assert_eq!(
            parse_sources(one_line, SourceFormat::OneLine),
            [
                ListedSource {
                    uri: "https://download.docker.com/linux/ubuntu".into(),
                    suites: vec!["noble".into()]
                },
                ListedSource {
                    uri: "http://archive.ubuntu.com/ubuntu".into(),
                    suites: vec!["noble".into()]
                }
            ]
        );
        let deb822 = "Types: deb\nURIs: http://archive.ubuntu.com/ubuntu/ http://mirror/ubuntu\n\
            Suites: noble noble-updates\nComponents: main\nSigned-By:\n -----BEGIN PGP PUBLIC KEY BLOCK-----\n .\n\n\
            Types: deb\nURIs: https://off.example\nSuites: x\nEnabled: no\n";
        let found = parse_sources(deb822, SourceFormat::Deb822);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].uri, "http://archive.ubuntu.com/ubuntu");
        assert_eq!(found[0].suites, ["noble", "noble-updates"]);

        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("sources.list.d");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            tmp.path().join("sources.list"),
            "deb http://archive.ubuntu.com/ubuntu noble main\n",
        )
        .unwrap();
        std::fs::write(d.join("docker.list"), one_line).unwrap();
        std::fs::write(d.join("docker.list.save"), one_line).unwrap();
        std::fs::write(d.join("docker.sources"), "managed").unwrap();
        std::fs::write(d.join("ubuntu.sources"), deb822).unwrap();
        let others = other_source_files(&d.join("docker.sources"));
        assert_eq!(
            others,
            [
                tmp.path().join("sources.list"),
                d.join("docker.list"),
                d.join("ubuntu.sources")
            ],
            "apt ignores other extensions; the managed file is not an 'other'"
        );
        assert_eq!(
            duplicate_sources(
                "https://download.docker.com/linux/ubuntu",
                &["noble".into()],
                &others
            ),
            [d.join("docker.list")]
        );
        assert!(
            duplicate_sources(
                "https://download.docker.com/linux/ubuntu",
                &["jammy".into()],
                &others
            )
            .is_empty()
        );
    }

    #[test]
    fn release_urls() {
        assert_eq!(
            release_url("https://download.docker.com/linux/ubuntu/", "questing"),
            "https://download.docker.com/linux/ubuntu/dists/questing/Release"
        );
        assert_eq!(
            release_url("https://x.example/repo", "./"),
            "https://x.example/repo/./Release"
        );
    }

    #[test]
    fn dnf_repo_files() {
        assert_eq!(
            dnf_repo_file(
                "gh",
                "GitHub CLI",
                "https://cli.github.com/packages/rpm",
                Some("https://k")
            ),
            "# Managed by dot (package_repo gh); changes are overwritten.\n[gh]\nname=GitHub CLI\n\
             baseurl=https://cli.github.com/packages/rpm\nenabled=1\ngpgcheck=1\ngpgkey=https://k\n"
        );
        assert!(dnf_repo_file("x", "x", "u", None).ends_with("gpgcheck=0\n"));
    }

    #[test]
    fn architectures() {
        assert_eq!(dpkg_arch("x86_64"), Some("amd64"));
        assert_eq!(dpkg_arch("aarch64"), Some("arm64"));
        assert_eq!(dpkg_arch("wasm32"), None);
    }

    #[test]
    fn keys_and_fingerprints() {
        assert!(is_armored(
            b"\n-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nmQINBF...\n"
        ));
        assert!(!is_armored(&[0x99, 0x02, 0x0d, 0x04]));
        let colons = "pub:-:4096:1:8D81803C0EBFCD88:1487788586:::-:::scESA::::::23::0:\n\
                      fpr:::::::::9DC858229FC7DD38854AE2D88D81803C0EBFCD88:\n\
                      uid:-::::1487792064::B5A08F01796E7F521861B449372D4C35C2D4F6E6::Docker Release (CE deb) <docker@docker.com>::::::::::0:\n\
                      sub:-:4096:1:7EA0A9C3F273FCD8:1487788586::::::s::::::23:\n\
                      fpr:::::::::D3306A018370199E527AE7997EA0A9C3F273FCD8:\n";
        let keys = parse_gpg_keys(colons);
        assert_eq!(
            keys,
            [GpgKey {
                fingerprint: "9DC858229FC7DD38854AE2D88D81803C0EBFCD88".into(),
                subkeys: vec!["D3306A018370199E527AE7997EA0A9C3F273FCD8".into()],
            }]
        );
        assert_eq!(
            check_single_key("9DC8 5822 9FC7 DD38 854A  E2D8 8D81 803C 0EBF CD88", &keys),
            Ok(())
        );
        assert_eq!(
            check_single_key("9dc858229fc7dd38854ae2d88d81803c0ebfcd88", &keys),
            Ok(())
        );
        // A key id, even a long one, does not pin the key.
        let e = check_single_key("8d81803c0ebfcd88", &keys).unwrap_err();
        assert!(e.contains("not a whole fingerprint"), "{e}");
        // A subkey's fingerprint is not the primary key's.
        let e = check_single_key("D3306A018370199E527AE7997EA0A9C3F273FCD8", &keys).unwrap_err();
        assert!(e.contains("one of its subkeys"), "{e}");
        let e = check_single_key("1111222233334444555566667777888899990000", &keys).unwrap_err();
        assert!(
            e.contains("its key is 9DC858229FC7DD38854AE2D88D81803C0EBFCD88"),
            "{e}"
        );
        assert!(check_single_key("9DC858229FC7DD38854AE2D88D81803C0EBFCD88", &[]).is_err());
    }

    #[test]
    fn a_second_key_in_the_file_is_refused_even_with_the_right_one() {
        // Docker's key plus another primary key, as a tampered download could serve.
        let colons = "pub:-:4096:1:8D81803C0EBFCD88:1487788586:::-:::scESA::::::23::0:\n\
                      fpr:::::::::9DC858229FC7DD38854AE2D88D81803C0EBFCD88:\n\
                      uid:-::::1487792064::B5A0::Docker Release (CE deb) <docker@docker.com>::::::::::0:\n\
                      pub:-:255:22:0123456789ABAAAA:1700000000:::-:::scESC::::::ed25519:::0:\n\
                      fpr:::::::::0123456789ABCDEF0123456789ABCDEF0123AAAA:\n\
                      uid:-::::1700000000::C0FF::Someone Else <x@example.com>::::::::::0:\n";
        let keys = parse_gpg_keys(colons);
        assert_eq!(keys.len(), 2);
        let e = check_single_key("9DC858229FC7DD38854AE2D88D81803C0EBFCD88", &keys).unwrap_err();
        assert!(e.contains("holds 2 keys"), "{e}");
        // Both pinned (a vendor rotating keys): trusted. One pinned key missing is fine too.
        let both = [
            "9DC858229FC7DD38854AE2D88D81803C0EBFCD88".to_string(),
            "0123 4567 89AB CDEF 0123  4567 89AB CDEF 0123 AAAA".to_string(),
        ];
        assert_eq!(check_pinned_keys(&both, &keys), Ok(()));
        assert_eq!(check_pinned_keys(&both, &keys[..1]), Ok(()));
        let e = check_pinned_keys(&both[..1], &keys).unwrap_err();
        assert!(e.contains("holds 2 keys"), "{e}");
        assert!(
            e.contains("0123456789ABCDEF0123456789ABCDEF0123AAAA"),
            "{e}"
        );
        // v5 / v6 fingerprints are 64 digits.
        assert!(is_full_fingerprint(&"A".repeat(64)));
        assert!(!is_full_fingerprint(&"A".repeat(48)));
    }

    #[test]
    fn gpg_argv_never_touches_the_users_keyring() {
        let h = Path::new("/tmp/h");
        assert_eq!(
            gpg_show_keys(h, Path::new("/tmp/k")).line(),
            "gpg --batch --no-tty --homedir /tmp/h --show-keys --with-colons --with-fingerprint /tmp/k"
        );
        assert_eq!(
            gpg_dearmor(h, Path::new("/tmp/k.asc"), Path::new("/tmp/k.gpg")).line(),
            "gpg --batch --no-tty --homedir /tmp/h --yes --output /tmp/k.gpg --dearmor /tmp/k.asc"
        );
        assert_eq!(
            install_file(Path::new("/tmp/a"), Path::new("/etc/x"), 0o644).line(),
            "install -m 0644 /tmp/a /etc/x"
        );
        assert_eq!(
            install_dir(Path::new("/etc/apt/keyrings")).line(),
            "install -d -m 0755 /etc/apt/keyrings"
        );
    }

    #[test]
    fn curl_download_and_probe() {
        let r = FakeRunner::with_programs(&["curl"]);
        download(&r, "https://k.example/key", Path::new("/tmp/d/key")).unwrap();
        assert_eq!(
            r.lines(),
            [
                "curl -fsSL --proto =https --proto-redir =https --tlsv1.2 --retry 2 --max-time 120 \
              -o /tmp/d/key https://k.example/key"
            ]
        );
        let e = download(&r, "http://k.example/key", Path::new("/k")).unwrap_err();
        assert!(e.to_string().contains("only fetched over https"), "{e}");
        assert_eq!(r.lines().len(), 1, "an http key is not even fetched");
        r.on("curl -fsSL", 22, "");
        let e = download(&r, "https://k.example/key", Path::new("/k")).unwrap_err();
        assert!(e.to_string().contains("code 22"), "{e}");

        r.on(
            "curl -fsSIL --proto =http,https --proto-redir =http,https --max-time 20 https://r/dists/noble/Release",
            0,
            "",
        );
        r.on(
            "curl -fsSIL --proto =http,https --proto-redir =http,https --max-time 20 https://r/dists/questing/Release",
            22,
            "",
        );
        r.on("curl -fsSIL --proto =http,https --proto-redir =http,https --max-time 20 https://offline", 6, "");
        assert_eq!(url_exists(&r, "https://r/dists/noble/Release"), Ok(true));
        assert_eq!(
            url_exists(&r, "https://r/dists/questing/Release"),
            Ok(false)
        );
        assert!(
            url_exists(&r, "https://offline")
                .unwrap_err()
                .contains("code 6")
        );
        r.remove_program("curl");
        assert!(
            url_exists(&r, "https://x")
                .unwrap_err()
                .contains("cannot run curl")
        );
    }

    #[test]
    fn listing_parsers() {
        assert_eq!(
            parse_brew_taps("homebrew/core\nHashiCorp/tap\n\n"),
            ["homebrew/core", "hashicorp/tap"]
        );
        assert_eq!(
            parse_trusted_taps(r#"{"taps":["HashiCorp/tap",{"name":"a/b"}],"formulae":[]}"#)
                .unwrap(),
            ["hashicorp/tap", "a/b"]
        );
        assert!(parse_trusted_taps("Error").is_err());
        let scoop = "\nName   Source                                   Updated            Manifests\n\
                     ----   ------                                   -------            ---------\n\
                     main   https://github.com/ScoopInstaller/Main   2026-09-01 10:00:00     1400\n\
                     Extras https://github.com/ScoopInstaller/Extras 2026-09-01 10:00:00     2000\n";
        assert_eq!(parse_table_names(scoop), ["main", "extras"]);
        let zypper = "Repository priorities are without effect. ...\n\n\
                      # | Alias    | Name       | Enabled | GPG Check | Refresh | URI\n\
                      --+----------+------------+---------+-----------+---------+-------------------\n\
                      1 | repo-oss | Main (OSS) | Yes     | (r ) Yes  | Yes     | http://download.opensuse.org/tumbleweed/repo/oss/\n\
                      2 | gh       | gh         | Yes     | (r ) Yes  | Yes     | https://cli.github.com/packages/rpm\n";
        assert_eq!(
            parse_zypper_repos(zypper),
            [
                (
                    "repo-oss".to_string(),
                    "http://download.opensuse.org/tumbleweed/repo/oss/".to_string()
                ),
                ("gh".into(), "https://cli.github.com/packages/rpm".into())
            ]
        );
    }
}
