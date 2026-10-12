//! `dot self-update`: replace the running `dot` with the latest GitHub release.
//!
//! The latest release of [`release_repo`] ([`DEFAULT_REPO`], or a fork named by
//! `DOT_RELEASE_REPO`) is looked up through the GitHub API (`DOT_GITHUB_API`). The releases are
//! public, so no token is needed; when there is one ([`github_token`]: `GITHUB_TOKEN`, else
//! `GH_TOKEN`, else `gh auth token` when the GitHub CLI is logged in) it is sent to the API (a
//! private fork needs it, and it lifts the API's rate limit for anonymous requests). With a
//! token, the assets are downloaded through the API too (`/releases/assets/{id}`); the API
//! redirects to a storage host, which gets no token (the HTTP client never sends
//! `Authorization` to another origin).
//! The token is held as a [`SecretBytes`] and never printed. The asset for this machine is the
//! one `install.sh` / `install.ps1` fetch ([`asset_name`]); it must be listed in the release's
//! `SHA256SUMS` with a matching checksum. As with the install scripts, the checksum comes from
//! the same release, so it catches corruption and truncation, not a malicious release; trust
//! rests on TLS to github.com.
//!
//! The new binary is unpacked into a private directory next to the running one (same file
//! system) and renamed over it, so the path never holds a half-written file. Windows cannot
//! replace a running executable, but can rename it: the old one is renamed away first
//! (`.dot.exe.old`, deleted by the next update) and the new one takes its name.
//!
//! Nothing here reaches the network in tests: [`Updater`] takes a [`Github`] whose HTTP client
//! is injected.

use std::cmp::Ordering;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::Args;
use dot_core::SecretBytes;
use dot_core::config::Extract;
use dot_directives::github_release::{Asset, Github, Release, TOKEN_ENV, Want, extract_archive};
use sha2::{Digest, Sha256};

/// Where `dot` is released, unless `DOT_RELEASE_REPO` names another repository (a fork).
pub const DEFAULT_REPO: &str = "trungngo/dot-cli";

/// The GitHub repository `dot` is released from: `DOT_RELEASE_REPO` (`owner/name`), else
/// [`DEFAULT_REPO`].
pub fn release_repo() -> Result<String> {
    release_repo_from(std::env::var("DOT_RELEASE_REPO").ok())
}

/// [`release_repo`] with the variable's value injected.
pub fn release_repo_from(value: Option<String>) -> Result<String> {
    let Some(v) = value.filter(|v| !v.trim().is_empty()) else {
        return Ok(DEFAULT_REPO.to_string());
    };
    let v = v.trim();
    let part = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    };
    match v.split_once('/') {
        Some((owner, name)) if part(owner) && part(name) => Ok(v.to_string()),
        _ => bail!("DOT_RELEASE_REPO must be owner/name (got: {v})"),
    }
}

/// The checksum list every release carries.
pub const SUMS: &str = "SHA256SUMS";

/// A GitHub token for the release repository: `GITHUB_TOKEN`, else `GH_TOKEN`, else what `gh auth token`
/// prints when the GitHub CLI is installed and logged in to github.com.
pub fn github_token() -> Option<SecretBytes> {
    token_from(|k| std::env::var(k).ok(), gh_auth_token)
}

/// [`github_token`] with the environment and the GitHub CLI injected.
pub fn token_from(
    env: impl Fn(&str) -> Option<String>,
    gh: impl FnOnce() -> Option<String>,
) -> Option<SecretBytes> {
    TOKEN_ENV
        .iter()
        .find_map(|k| env(k).filter(|v| !v.trim().is_empty()))
        .or_else(|| gh().filter(|v| !v.trim().is_empty()))
        .map(|v| SecretBytes::new(v.trim()))
}

/// `gh auth token --hostname github.com`: the token on stdout (a pipe, never an argument).
fn gh_auth_token() -> Option<String> {
    let out = std::process::Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

#[derive(Args)]
pub struct SelfUpdateArgs {
    /// Only report whether a newer release exists; change nothing.
    #[arg(long)]
    pub check: bool,
    /// Install the latest release even when it is not newer than this one.
    #[arg(long)]
    pub force: bool,
}

/// The release asset for a platform, named exactly as `install.sh` / `install.ps1` (and the
/// release workflow) name it. `os` / `arch` as in [`std::env::consts`].
pub fn asset_name(os: &str, arch: &str) -> Option<String> {
    let arch = match arch {
        "x86_64" | "aarch64" => arch,
        _ => return None,
    };
    match os {
        "linux" => Some(format!("dot-{arch}-unknown-linux-musl.tar.gz")),
        "macos" => Some("dot-universal-apple-darwin.tar.gz".to_string()),
        "windows" => Some(format!("dot-{arch}-pc-windows-msvc.zip")),
        _ => None,
    }
}

/// A release version: `v1.2.3` or `1.2.3`; anything after the numbers (`-rc.1`) is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().trim_start_matches(['v', 'V']);
        let core = s.split(['-', '+']).next()?;
        let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
        let major = parts.next()??;
        let minor = parts.next().unwrap_or(Some(0))?;
        let patch = parts.next().unwrap_or(Some(0))?;
        if parts.next().is_some() {
            return None;
        }
        Some(Version(major, minor, patch))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The checksum `sums` (a `sha256sum` listing) gives for `file`.
pub fn find_sum(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        (name == file && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

/// How the running binary is replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swap {
    /// Rename the new file over the old one (Unix: a running binary may be replaced).
    Rename,
    /// Rename the running binary away first, then the new one into place (Windows).
    RenameAway,
}

impl Swap {
    pub fn native() -> Swap {
        if cfg!(windows) {
            Swap::RenameAway
        } else {
            Swap::Rename
        }
    }
}

/// What [`Updater::check`] found.
#[derive(Debug)]
pub struct Checked {
    pub current: Version,
    pub latest: Version,
    pub release: std::sync::Arc<Release>,
}

impl Checked {
    pub fn newer(&self) -> bool {
        self.latest.cmp(&self.current) == Ordering::Greater
    }
}

pub struct Updater {
    pub github: Github,
    /// The release repository ([`release_repo`]).
    pub repo: String,
    /// Sent to the API only.
    pub token: Option<SecretBytes>,
    pub current: Version,
    /// The release asset for this machine ([`asset_name`]).
    pub asset: String,
    /// The binary to replace.
    pub exe: PathBuf,
    pub swap: Swap,
}

impl Updater {
    /// The latest release, and whether it is newer than this binary.
    pub fn check(&self) -> Result<Checked> {
        let repo = &self.repo;
        let release = self
            .github
            .latest(repo, self.token.as_ref())
            .with_context(|| {
                if self.token.is_some() {
                    format!("looking up the latest release of dot ({repo})")
                } else {
                    format!(
                        "looking up the latest release of dot ({repo}; if it is private, or \
                         GitHub's rate limit for anonymous requests was reached: set \
                         GITHUB_TOKEN to a token that can read it, or log in with `gh auth \
                         login`)"
                    )
                }
            })?;
        let latest = Version::parse(&release.tag_name).with_context(|| {
            format!(
                "the latest release's tag `{}` is not a version",
                release.tag_name
            )
        })?;
        Ok(Checked {
            current: self.current,
            latest,
            release,
        })
    }

    /// Downloads this machine's asset from `release`, checks it against the release's
    /// `SHA256SUMS`, and puts the binary in it in place of [`Updater::exe`].
    pub fn install(&self, release: &Release) -> Result<()> {
        let dir = self
            .exe
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        remove_leftovers(&self.exe);
        let staging = tempfile::Builder::new()
            .prefix(".dot-update-")
            .tempdir_in(dir)
            .with_context(|| {
                format!(
                    "cannot write next to {} (install dot somewhere you can write, e.g. \
                     ~/.local/bin, or update it the way it was installed)",
                    self.exe.display()
                )
            })?;
        let new = fetch_binary(
            &self.github,
            &self.repo,
            self.token.as_ref(),
            release,
            &self.asset,
            staging.path(),
        )?;
        replace(&self.exe, &new, self.swap)
    }
}

/// Downloads `asset` (as [`asset_name`] names it) from `release` of `repo` into `dir` (with a
/// token, through the API: the token goes to the API only), checks it against the release's
/// `SHA256SUMS`, and unpacks the `dot` binary in it, executable, as `dir/dot.new` (`dot.exe.new`
/// for Windows). Nothing is unpacked unless the checksum matches.
pub fn fetch_binary(
    github: &Github,
    repo: &str,
    token: Option<&SecretBytes>,
    release: &Release,
    asset: &str,
    dir: &Path,
) -> Result<PathBuf> {
    let find = |name: &str| release.assets.iter().find(|a| a.name == name);
    let Some(found) = find(asset) else {
        bail!("release {} has no {asset}", release.tag_name);
    };
    let Some(sums) = find(SUMS) else {
        bail!(
            "release {} has no {SUMS}: refusing to install an unverified binary",
            release.tag_name
        );
    };
    let sums = github.download_asset(repo, sums, token)?.text(4 << 20)?;
    let Some(want) = find_sum(&sums, &found.name) else {
        bail!(
            "{SUMS} of release {} lists no checksum for {}",
            release.tag_name,
            found.name
        );
    };
    let archive = dir.join(&found.name);
    let have = download(github, repo, token, found, &archive)?;
    if have != want {
        let _ = fs::remove_file(&archive);
        bail!(
            "checksum mismatch for {} (expected {want}, got {have}): not installed",
            found.name
        );
    }
    let format = if found.name.ends_with(".zip") {
        Extract::Zip
    } else {
        Extract::TarGz
    };
    let bin = if asset.contains("windows") {
        "dot.exe"
    } else {
        "dot"
    };
    let wanted = [Want {
        installed: format!("{bin}.new"),
        names: vec![bin.to_string()],
        glob: None,
    }];
    let out = dir.join("out");
    fs::create_dir(&out).context("creating the staging directory")?;
    let new = extract_archive(&archive, &found.name, format, &wanted, &out)?
        .pop()
        .context("nothing extracted")?;
    make_executable(&new)?;
    Ok(new)
}

/// Streams `asset` to `to`, returning its SHA-256 (hex).
fn download(
    github: &Github,
    repo: &str,
    token: Option<&SecretBytes>,
    asset: &Asset,
    to: &Path,
) -> Result<String> {
    let name = &asset.name;
    let resp = github.download_asset(repo, asset, token)?;
    let mut body = resp.body;
    let mut file =
        BufWriter::new(File::create(to).with_context(|| format!("creating {}", to.display()))?);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = body
            .read(&mut buf)
            .with_context(|| format!("downloading {name}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", to.display()))?;
    }
    file.flush()
        .with_context(|| format!("writing {}", to.display()))?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Where a Windows update renames the running binary to (see [`Swap::RenameAway`]).
fn renamed_away(exe: &Path) -> PathBuf {
    let name = exe
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dot".into());
    exe.with_file_name(format!(".{name}.old"))
}

/// Deletes what an earlier update left behind: the renamed-away binary (still running then, so
/// it could not be deleted) and staging directories of an interrupted update.
fn remove_leftovers(exe: &Path) {
    let _ = fs::remove_file(renamed_away(exe));
    let Some(dir) = exe.parent() else {
        return;
    };
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(".dot-update-")
                && e.file_type().is_ok_and(|t| t.is_dir())
            {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Puts `new` (on the same file system) in place of `exe`.
pub fn replace(exe: &Path, new: &Path, swap: Swap) -> Result<()> {
    match swap {
        Swap::Rename => {
            fs::rename(new, exe).with_context(|| format!("replacing {}", exe.display()))
        }
        Swap::RenameAway => {
            let old = renamed_away(exe);
            // A leftover still in use (another dot running) cannot be deleted: use a fresh name.
            let old = if old.exists() && fs::remove_file(&old).is_err() {
                old.with_extension(format!("{}.old", std::process::id()))
            } else {
                old
            };
            fs::rename(exe, &old)
                .with_context(|| format!("moving the running {} aside", exe.display()))?;
            if let Err(e) = fs::rename(new, exe) {
                // Put the old one back rather than leave nothing there.
                let _ = fs::rename(&old, exe);
                return Err(e).with_context(|| format!("replacing {}", exe.display()));
            }
            // Fails while it is still running (always, on Windows): deleted by the next update.
            let _ = fs::remove_file(&old);
            Ok(())
        }
    }
}

/// The running binary, symlinks resolved (a link in `~/.local/bin` to the real file is kept).
fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot find the running dot binary")?;
    if cfg!(windows) {
        Ok(exe)
    } else {
        fs::canonicalize(&exe).with_context(|| format!("resolving {}", exe.display()))
    }
}

pub fn self_update(a: SelfUpdateArgs) -> Result<ExitCode> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let Some(asset) = asset_name(os, arch) else {
        bail!("no dot releases are built for {os}/{arch}");
    };
    let github = Github::from_env();
    let updater = Updater {
        token: github_token(),
        github,
        repo: release_repo()?,
        current: Version::parse(env!("CARGO_PKG_VERSION")).context("this dot's version")?,
        asset,
        exe: current_exe()?,
        swap: Swap::native(),
    };
    run(&updater, &a, &mut std::io::stdout())
}

/// `dot self-update` with `updater`, reporting to `out`.
pub fn run(updater: &Updater, a: &SelfUpdateArgs, out: &mut dyn Write) -> Result<ExitCode> {
    let checked = updater.check()?;
    let (current, latest) = (checked.current, checked.latest);
    if a.check {
        let _ = if checked.newer() {
            writeln!(
                out,
                "update available: dot {current} -> {latest} (run `dot self-update`)"
            )
        } else {
            writeln!(
                out,
                "dot {current} is up to date (latest release: {latest})"
            )
        };
        return Ok(ExitCode::SUCCESS);
    }
    if !checked.newer() && !a.force {
        let _ = writeln!(
            out,
            "dot {current} is up to date (latest release: {latest}; --force reinstalls it)"
        );
        return Ok(ExitCode::SUCCESS);
    }
    updater.install(&checked.release)?;
    let _ = writeln!(out, "dot {current} -> {latest} ({})", updater.exe.display());
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    use dot_platform::http::{HttpClient, Request, Response};

    use super::*;

    const API: &str = "https://api.test";
    const REPO: &str = DEFAULT_REPO;
    const DL: &str = "https://dl.test/v0.9.0";

    /// Serves fixed bodies by URL and records what was asked for.
    #[derive(Default)]
    struct Fixture {
        bodies: BTreeMap<String, (u16, Vec<u8>)>,
        asked: Mutex<Vec<Request>>,
    }

    impl HttpClient for Fixture {
        fn get(&self, req: &Request) -> Result<Response> {
            self.asked.lock().unwrap().push(req.clone());
            let Some((status, body)) = self.bodies.get(&req.url) else {
                bail!("no fixture for {}", req.url);
            };
            Ok(Response {
                status: *status,
                headers: Vec::new(),
                content_length: Some(body.len() as u64),
                body: Box::new(Cursor::new(body.clone())),
            })
        }
    }

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_entry_type(tar::EntryType::Regular);
            b.append_data(&mut h, path, *data).unwrap();
        }
        let tar = b.into_inner().unwrap();
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(&tar).unwrap();
        e.finish().unwrap()
    }

    fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (path, data) in files {
            w.start_file(*path, ::zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    struct Setup {
        _tmp: tempfile::TempDir,
        exe: PathBuf,
        fixture: Arc<Fixture>,
    }

    /// A release `v0.9.0` with `asset` (holding `archive`) and a `SHA256SUMS` giving `sum`
    /// for it (`None`: its real checksum), and an installed binary saying "old".
    fn setup(asset: &str, archive: Vec<u8>, sum: Option<&str>, exe_name: &str) -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let exe = bin.join(exe_name);
        fs::write(&exe, "old").unwrap();
        let sum = sum.map_or_else(|| sha(&archive), str::to_string);
        let release = serde_json::json!({
            "tag_name": "v0.9.0",
            "assets": [
                {"name": asset, "browser_download_url": format!("{DL}/{asset}")},
                {"name": "dot-other.tar.gz", "browser_download_url": format!("{DL}/other")},
                {"name": SUMS, "browser_download_url": format!("{DL}/{SUMS}")},
            ]
        });
        let mut bodies = BTreeMap::new();
        bodies.insert(
            format!("{API}/repos/{REPO}/releases/latest"),
            (200, release.to_string().into_bytes()),
        );
        bodies.insert(format!("{DL}/{asset}"), (200, archive));
        bodies.insert(
            format!("{DL}/{SUMS}"),
            (
                200,
                format!("{}  dot-other.tar.gz\n{sum}  {asset}\n", "0".repeat(64)).into_bytes(),
            ),
        );
        Setup {
            _tmp: tmp,
            exe,
            fixture: Arc::new(Fixture {
                bodies,
                ..Fixture::default()
            }),
        }
    }

    fn updater(s: &Setup, asset: &str, current: &str, swap: Swap) -> Updater {
        Updater {
            github: Github::new(s.fixture.clone(), API),
            repo: REPO.into(),
            token: Some(SecretBytes::new("tok")),
            current: Version::parse(current).unwrap(),
            asset: asset.into(),
            exe: s.exe.clone(),
            swap,
        }
    }

    fn run_with(u: &Updater, check: bool, force: bool) -> Result<String> {
        let mut out = Vec::new();
        run(u, &SelfUpdateArgs { check, force }, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    const LINUX: &str = "dot-x86_64-unknown-linux-musl.tar.gz";

    #[test]
    fn asset_names_match_the_install_scripts() {
        let cases = [
            ("linux", "x86_64", "dot-x86_64-unknown-linux-musl.tar.gz"),
            ("linux", "aarch64", "dot-aarch64-unknown-linux-musl.tar.gz"),
            ("macos", "x86_64", "dot-universal-apple-darwin.tar.gz"),
            ("macos", "aarch64", "dot-universal-apple-darwin.tar.gz"),
            ("windows", "x86_64", "dot-x86_64-pc-windows-msvc.zip"),
            ("windows", "aarch64", "dot-aarch64-pc-windows-msvc.zip"),
        ];
        let sh = include_str!("../../../install.sh");
        let ps1 = include_str!("../../../install.ps1");
        for (os, arch, want) in cases {
            assert_eq!(asset_name(os, arch).as_deref(), Some(want));
        }
        // The scripts build the same names.
        assert!(sh.contains(r#"asset="dot-$(target).tar.gz""#));
        assert!(sh.contains(r#"echo "$arch-unknown-linux-musl""#));
        assert!(sh.contains(r#"echo "universal-apple-darwin""#));
        assert!(ps1.contains(r#"$asset = "dot-$arch-pc-windows-msvc.zip""#));
        assert_eq!(asset_name("linux", "riscv64"), None);
        assert_eq!(asset_name("freebsd", "x86_64"), None);
    }

    #[test]
    fn versions_and_checksum_lists() {
        assert_eq!(Version::parse("v1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("0.10.0-rc.1"), Some(Version(0, 10, 0)));
        assert_eq!(Version::parse("2"), Some(Version(2, 0, 0)));
        assert_eq!(Version::parse("latest"), None);
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
        let h = "a".repeat(64);
        let sums = format!("{h}  a.tar.gz\n{} *b.zip\nnot a line\n", "B".repeat(64));
        assert_eq!(find_sum(&sums, "a.tar.gz"), Some(h));
        assert_eq!(find_sum(&sums, "b.zip"), Some("b".repeat(64)));
        assert_eq!(find_sum(&sums, "a.tar"), None);
    }

    #[test]
    fn check_only_reports() {
        let s = setup(LINUX, tar_gz(&[("dot", b"new")]), None, "dot");
        let text = run_with(&updater(&s, LINUX, "0.1.0", Swap::Rename), true, false).unwrap();
        assert_eq!(
            text,
            "update available: dot 0.1.0 -> 0.9.0 (run `dot self-update`)\n"
        );
        assert_eq!(fs::read_to_string(&s.exe).unwrap(), "old");
        let asked = s.fixture.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1, "only the API: {asked:?}");
        assert_eq!(asked[0].get_header("authorization"), Some("Bearer tok"));

        let text = run_with(&updater(&s, LINUX, "0.9.0", Swap::Rename), true, false).unwrap();
        assert_eq!(text, "dot 0.9.0 is up to date (latest release: 0.9.0)\n");
    }

    #[test]
    fn update_replaces_the_binary_and_prints_old_and_new_versions() {
        let s = setup(
            LINUX,
            tar_gz(&[("dot-0.9.0/dot", b"new"), ("README", b"x")]),
            None,
            "dot",
        );
        let u = updater(&s, LINUX, "0.1.0", Swap::Rename);
        let text = run_with(&u, false, false).unwrap();
        assert_eq!(text, format!("dot 0.1.0 -> 0.9.0 ({})\n", s.exe.display()));
        assert_eq!(fs::read_to_string(&s.exe).unwrap(), "new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&s.exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        let left: Vec<_> = fs::read_dir(s.exe.parent().unwrap()).unwrap().collect();
        assert_eq!(left.len(), 1, "no staging left behind");
        // The token only ever went to the API.
        for r in s.fixture.asked.lock().unwrap().iter() {
            let to_api = r.url.starts_with(API);
            assert_eq!(r.get_header("authorization").is_some(), to_api, "{r:?}");
        }

        // Up to date now: nothing is downloaded, unless forced.
        let s2 = setup(LINUX, tar_gz(&[("dot", b"newer")]), None, "dot");
        let u = updater(&s2, LINUX, "0.9.0", Swap::Rename);
        assert!(
            run_with(&u, false, false)
                .unwrap()
                .contains("is up to date")
        );
        assert_eq!(fs::read_to_string(&s2.exe).unwrap(), "old");
        run_with(&u, false, true).unwrap();
        assert_eq!(fs::read_to_string(&s2.exe).unwrap(), "newer");
    }

    #[test]
    fn a_checksum_mismatch_changes_nothing() {
        let wrong = "f".repeat(64);
        let s = setup(LINUX, tar_gz(&[("dot", b"evil")]), Some(&wrong), "dot");
        let err = run_with(&updater(&s, LINUX, "0.1.0", Swap::Rename), false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert_eq!(fs::read_to_string(&s.exe).unwrap(), "old");
        let left: Vec<_> = fs::read_dir(s.exe.parent().unwrap()).unwrap().collect();
        assert_eq!(left.len(), 1, "staging removed");
    }

    #[test]
    fn a_release_without_this_machines_asset_is_an_error() {
        let s = setup(LINUX, tar_gz(&[("dot", b"new")]), None, "dot");
        let other = "dot-aarch64-unknown-linux-musl.tar.gz";
        let err = run_with(&updater(&s, other, "0.1.0", Swap::Rename), false, false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&format!("release v0.9.0 has no {other}")),
            "{err}"
        );
        assert_eq!(fs::read_to_string(&s.exe).unwrap(), "old");
    }

    #[test]
    fn windows_renames_the_running_binary_away() {
        let asset = "dot-x86_64-pc-windows-msvc.zip";
        let s = setup(asset, zip(&[("dot.exe", b"new exe")]), None, "dot.exe");
        // A leftover from an earlier update is cleaned up.
        let old = s.exe.with_file_name(".dot.exe.old");
        fs::write(&old, "older").unwrap();
        let u = updater(&s, asset, "0.1.0", Swap::RenameAway);
        run_with(&u, false, false).unwrap();
        assert_eq!(fs::read_to_string(&s.exe).unwrap(), "new exe");
        // Deletable here (not running), so it is gone; on Windows it stays until next time.
        assert!(!old.exists());
    }

    #[test]
    fn the_token_comes_from_the_environment_then_gh() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                vars.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let gh = || Some("from-gh\n".to_string());
        let t = |v: Option<SecretBytes>| v.map(|s| s.as_str().unwrap().to_string());
        assert_eq!(
            t(token_from(
                env(&[("GITHUB_TOKEN", " a "), ("GH_TOKEN", "b")]),
                gh
            )),
            Some("a".into())
        );
        assert_eq!(
            t(token_from(
                env(&[("GITHUB_TOKEN", " "), ("GH_TOKEN", "b")]),
                gh
            )),
            Some("b".into())
        );
        assert_eq!(t(token_from(env(&[]), gh)), Some("from-gh".into()));
        assert_eq!(t(token_from(env(&[]), || None)), None);
        assert_eq!(t(token_from(env(&[]), || Some("\n".into()))), None);
        // gh is not asked when the environment has one.
        let asked = std::cell::Cell::new(false);
        token_from(env(&[("GH_TOKEN", "b")]), || {
            asked.set(true);
            None
        });
        assert!(!asked.get());
    }

    #[test]
    fn with_a_token_assets_come_through_the_api() {
        // As the API lists them: each asset with its id.
        let archive = tar_gz(&[("dot", b"new")]);
        let s = setup(LINUX, archive.clone(), None, "dot");
        let release = serde_json::json!({
            "tag_name": "v0.9.0",
            "assets": [
                {"name": LINUX, "id": 11, "browser_download_url": format!("{DL}/{LINUX}")},
                {"name": SUMS, "id": 12, "browser_download_url": format!("{DL}/{SUMS}")},
            ]
        });
        let mut bodies = s.fixture.bodies.clone();
        bodies.insert(
            format!("{API}/repos/{REPO}/releases/latest"),
            (200, release.to_string().into_bytes()),
        );
        let assets = format!("{API}/repos/{REPO}/releases/assets");
        bodies.insert(format!("{assets}/11"), (200, archive.clone()));
        bodies.insert(
            format!("{assets}/12"),
            (200, format!("{}  {LINUX}\n", sha(&archive)).into_bytes()),
        );
        let fixture = Arc::new(Fixture {
            bodies,
            ..Fixture::default()
        });
        let mut u = updater(&s, LINUX, "0.1.0", Swap::Rename);
        u.github = Github::new(fixture.clone(), API);
        run_with(&u, false, false).unwrap();
        assert_eq!(fs::read_to_string(&s.exe).unwrap(), "new");
        let asked = fixture.asked.lock().unwrap().clone();
        let urls: Vec<&str> = asked.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                format!("{API}/repos/{REPO}/releases/latest"),
                format!("{assets}/12"),
                format!("{assets}/11"),
            ]
        );
        for r in &asked[1..] {
            assert_eq!(r.get_header("authorization"), Some("Bearer tok"), "{r:?}");
            assert_eq!(r.get_header("accept"), Some("application/octet-stream"));
            // Debug output (logs, errors) never shows it.
            assert!(!format!("{r:?}").contains("tok\""), "{r:?}");
        }

        // Without a token, the public URLs (a private repository answers 404 there).
        let s = setup(LINUX, tar_gz(&[("dot", b"new")]), None, "dot");
        let mut u = updater(&s, LINUX, "0.1.0", Swap::Rename);
        u.token = None;
        run_with(&u, false, false).unwrap();
        for r in s.fixture.asked.lock().unwrap().iter() {
            assert!(r.get_header("authorization").is_none(), "{r:?}");
        }
    }

    #[test]
    fn without_a_token_a_failed_lookup_says_how_to_give_one() {
        let s = setup(LINUX, tar_gz(&[("dot", b"new")]), None, "dot");
        let mut bodies = s.fixture.bodies.clone();
        bodies.insert(
            format!("{API}/repos/{REPO}/releases/latest"),
            (404, b"{\"message\":\"Not Found\"}".to_vec()),
        );
        let mut u = updater(&s, LINUX, "0.1.0", Swap::Rename);
        u.github = Github::new(
            Arc::new(Fixture {
                bodies,
                ..Fixture::default()
            }),
            API,
        );
        u.token = None;
        let err = format!("{:#}", run_with(&u, true, false).unwrap_err());
        assert!(err.contains(&format!("({REPO}; if it is private")), "{err}");
        assert!(err.contains("set GITHUB_TOKEN"), "{err}");
        assert!(err.contains("gh auth login"), "{err}");
    }

    #[test]
    fn the_release_repository_is_the_default_or_an_owner_and_name() {
        assert_eq!(release_repo_from(None).unwrap(), DEFAULT_REPO);
        assert_eq!(release_repo_from(Some(" ".into())).unwrap(), DEFAULT_REPO);
        assert_eq!(
            release_repo_from(Some("someone/dot-fork".into())).unwrap(),
            "someone/dot-fork"
        );
        assert_eq!(
            release_repo_from(Some("a_b/c.d-e".into())).unwrap(),
            "a_b/c.d-e"
        );
        for bad in [
            "someone", "a/b/c", "/abs", "a/", "a/..", "./b", "a b/c", "a/b?c",
        ] {
            let err = release_repo_from(Some(bad.into())).unwrap_err().to_string();
            assert!(err.contains("owner/name"), "{bad}: {err}");
        }
    }

    #[test]
    fn a_fork_is_looked_up_in_its_own_repository() {
        let s = setup(LINUX, tar_gz(&[("dot", b"new")]), None, "dot");
        let mut bodies = s.fixture.bodies.clone();
        let release = bodies[&format!("{API}/repos/{REPO}/releases/latest")].clone();
        bodies.insert(
            format!("{API}/repos/someone/dot-fork/releases/latest"),
            release,
        );
        let mut u = updater(&s, LINUX, "0.1.0", Swap::Rename);
        let fixture = Arc::new(Fixture {
            bodies,
            ..Fixture::default()
        });
        u.github = Github::new(fixture.clone(), API);
        u.repo = "someone/dot-fork".into();
        run_with(&u, true, false).unwrap();
        let asked = fixture.asked.lock().unwrap();
        assert_eq!(
            asked[0].url,
            format!("{API}/repos/someone/dot-fork/releases/latest")
        );
    }

    #[test]
    fn rename_away_puts_the_old_binary_back_when_the_new_one_cannot_go_in() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("dot.exe");
        fs::write(&exe, "old").unwrap();
        let missing = tmp.path().join("nope");
        assert!(replace(&exe, &missing, Swap::RenameAway).is_err());
        assert_eq!(fs::read_to_string(&exe).unwrap(), "old");
    }
}
