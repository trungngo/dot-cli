//! `install.ps1` against a GitHub stand-in on 127.0.0.1 (`DOT_GITHUB_URL` for the public
//! download URLs, `DOT_GITHUB_API` for the API), in every PowerShell there is: Windows
//! PowerShell 5.1 and PowerShell 7 on Windows, `pwsh` elsewhere (skipped when there is none).
//! The two differ in how Invoke-WebRequest follows redirects, so the script downloads through
//! HttpClient and follows them itself. These runs check that the public release is fetched
//! without any token, and that for a private fork's release (`DOT_RELEASE_REPO`) the token
//! reaches the API and only the API: the asset redirect goes to another port (another origin),
//! which must see no Authorization header.
//!
//! In most runs the release's checksum does not match, so the run stops before installing
//! anything; what is checked is what was sent where, and that nothing is left behind. One run
//! goes all the way, as the documented one-liner for a private dotfiles repository does (the
//! script piped into `iex`, the repository in `DOT_DOTFILES_URL`): a fake `git` records the
//! extra header it gets in its environment, and the release's `dot` is a fake too (on Windows,
//! the real one, asked for `apply --help`). `DOT_NO_MODIFY_PATH` keeps the user's PATH as it
//! is.
//!
//! PowerShell runs one at a time (pwsh processes starting together can corrupt the startup
//! profile cache they share), each with its own cache directory, and without the caller's
//! `PSModulePath` (a Windows PowerShell 5.1 started from PowerShell 7 would otherwise load 7's
//! modules and miss its own, `Get-FileHash` among them).

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../install.ps1");

const TOKEN: &str = "github_pat_FAKE0123456789abcdefTOKEN";

/// A fork whose releases are private, as `DOT_RELEASE_REPO` names it.
const FORK: &str = "someone/dot-fork";

/// The fork's releases in the API.
const REPO: &str = "/repos/someone/dot-fork/releases";

/// The public download URLs of the default release repository, on the stand-in.
const PUBLIC: &str = "/trungngo/dot-cli/releases/latest/download";

const ZIP: &str = "dot-x86_64-pc-windows-msvc.zip";

type Route = Box<dyn Fn(&str) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync>;

/// An HTTP/1.1 server answering from `route(path)`, recording each request's head.
struct Server {
    base: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(route: Route) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    head.push_str(&line);
                }
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                log.lock().unwrap().push(head);
                let (status, headers, body) = route(&path);
                let mut out = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                let _ = stream.write_all(out.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        Server { base, seen }
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

/// Held while a PowerShell runs.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

/// The PowerShells on this machine.
fn powershells() -> Vec<&'static str> {
    let _serial = one_at_a_time();
    let candidates: &[&str] = if cfg!(windows) {
        &["powershell", "pwsh"]
    } else {
        &["pwsh"]
    };
    candidates
        .iter()
        .copied()
        .filter(|ps| {
            Command::new(ps)
                .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
                .env_remove("PSModulePath")
                .output()
                .is_ok_and(|o| o.status.success())
        })
        .collect()
}

/// What the API stand-in says about the repository.
#[derive(Clone, Copy, PartialEq)]
enum Repo {
    /// It has a release.
    Released,
    /// The token reads it, but it has no release yet.
    NoRelease,
    /// The token cannot read it (GitHub says 404, as if it did not exist).
    NoAccess,
}

/// A stand-in for GitHub: the public download URLs (of the default repository; the fork's
/// answer 404, as a private repository's do) and the API (of the fork) at one origin, the
/// storage host at another. The checksum in SHA256SUMS is not the asset's.
fn github() -> (Server, Server) {
    let sums = format!("{}  dot-x86_64-pc-windows-msvc.zip\n", "0".repeat(64));
    github_serving(b"not really a zip".to_vec(), sums, Repo::Released)
}

/// The stand-in serving `zip` as the release asset and `sums` as its SHA256SUMS.
fn github_serving(zip: Vec<u8>, sums_body: String, repo: Repo) -> (Server, Server) {
    let zip_for_public = zip.clone();
    let storage = Server::start(Box::new(move |path| {
        if path.starts_with("/signed/asset") {
            (200, vec![], zip.clone())
        } else {
            (404, vec![], vec![])
        }
    }));
    let storage_base = storage.base.clone();
    let public_zip = zip_for_public;
    let public_sums = sums_body.clone();
    let api = Server::start(Box::new(move |path| {
        let latest = format!("{REPO}/latest");
        let asset = format!("{REPO}/assets/11");
        let sums = format!("{REPO}/assets/12");
        let not_found = (404, vec![], b"{\"message\":\"Not Found\"}".to_vec());
        if path == format!("{PUBLIC}/{ZIP}") {
            (200, vec![], public_zip.clone())
        } else if path == format!("{PUBLIC}/SHA256SUMS") {
            (200, vec![], public_sums.clone().into_bytes())
        } else if path == format!("/repos/{FORK}") {
            if repo == Repo::NoAccess {
                not_found
            } else {
                (
                    200,
                    vec![],
                    format!("{{\"full_name\":\"{FORK}\"}}").into_bytes(),
                )
            }
        } else if path == latest && repo != Repo::Released {
            not_found
        } else if path == latest {
            let name = |id| {
                let n = if id == 11 {
                    "dot-x86_64-pc-windows-msvc.zip"
                } else {
                    "SHA256SUMS"
                };
                format!(
                    r#"{{"url":"https://api.github.com{REPO}/assets/{id}","id":{id},"name":"{n}","browser_download_url":"https://github.com/someone/dot-fork/releases/download/v9.9.9/{n}"}}"#
                )
            };
            let body = format!(
                r#"{{"tag_name":"v9.9.9","assets":[{},{}]}}"#,
                name(11),
                name(12)
            );
            (200, vec![], body.into_bytes())
        } else if path == asset {
            let loc = format!("{storage_base}/signed/asset?sig=abc");
            (302, vec![("Location".into(), loc)], vec![])
        } else if path == sums {
            (200, vec![], sums_body.clone().into_bytes())
        } else {
            not_found
        }
    }));
    (api, storage)
}

struct Run {
    out: Output,
    tmp: PathBuf,
}

impl Run {
    fn text(&self) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&self.out.stdout),
            String::from_utf8_lossy(&self.out.stderr)
        )
    }
}

/// Runs install.ps1 in `ps` with `env`, a fake `gh` first on PATH (printing `gh_token`, or
/// failing as when logged out), and a private temp directory.
fn run(ps: &str, root: &Path, env: &[(&str, &str)], gh_token: Option<&str>) -> Run {
    run_as(ps, root, env, gh_token, false)
}

/// [`run`]; with `iex`, the script is piped into Invoke-Expression, as the one-liner does, and
/// what it leaves in the session's environment is printed afterwards.
fn run_as(ps: &str, root: &Path, env: &[(&str, &str)], gh_token: Option<&str>, iex: bool) -> Run {
    let bin = root.join("bin");
    let tmp = root.join("tmp");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&tmp).unwrap();
    if cfg!(windows) {
        let body = match gh_token {
            Some(t) => format!(
                "@echo off\r\nif \"%1 %2\"==\"auth token\" (echo {t}& exit /b 0)\r\nexit /b 1\r\n"
            ),
            None => "@echo off\r\necho not logged in 1>&2\r\nexit /b 1\r\n".to_string(),
        };
        fs::write(bin.join("gh.cmd"), body).unwrap();
    } else {
        let body = match gh_token {
            Some(t) => format!(
                "#!/bin/sh\n[ \"$1 $2\" = 'auth token' ] && {{ echo {t}; exit 0; }}\nexit 1\n"
            ),
            None => "#!/bin/sh\necho not logged in >&2\nexit 1\n".to_string(),
        };
        let p = bin.join("gh");
        fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let path = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let script = SCRIPT.replace('\'', "''");
    let mut c = Command::new(ps);
    c.args(["-NoProfile", "-NonInteractive"]);
    if cfg!(windows) {
        c.args(["-ExecutionPolicy", "Bypass"]);
    }
    let command = if iex {
        format!(
            "$failed = $null\n\
             try {{ [IO.File]::ReadAllText('{script}') | Invoke-Expression }} catch {{ $failed = $_.Exception.Message }}\n\
             Write-Host ('after: git variables left: ' + @(@('GIT_CONFIG_COUNT', 'GIT_CONFIG_KEY_0', 'GIT_CONFIG_VALUE_0', 'GIT_TERMINAL_PROMPT') | Where-Object {{ Test-Path ('Env:' + $_) }}).Count + '; GITHUB_TOKEN set=' + [bool]$env:GITHUB_TOKEN + '; GH_TOKEN there=' + (Test-Path Env:GH_TOKEN))\n\
             if ($failed) {{ [Console]::Error.WriteLine('ERROR: ' + $failed); exit 1 }}"
        )
    } else {
        format!(
            "try {{ & '{script}' }} catch {{ [Console]::Error.WriteLine('ERROR: ' + $_.Exception.Message); exit 1 }}"
        )
    };
    c.arg("-Command").arg(command);
    c.env("PATH", path)
        .env_remove("PSModulePath")
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMP", &tmp)
        .env("TEMP", &tmp)
        .env("TMPDIR", &tmp)
        .env("PROCESSOR_ARCHITECTURE", "AMD64")
        .env_remove("PROCESSOR_ARCHITEW6432")
        .env("DOT_INSTALL_DIR", root.join("install"))
        .env("DOT_DOTFILES_DIR", root.join("dotfiles"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("DOT_NO_MODIFY_PATH", "1")
        .env("FAKE_LOG", root.join("log"))
        .env("LOCALAPPDATA", root.join("appdata"))
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .env_remove("DOT_RELEASE_BASE")
        .env_remove("DOT_RELEASE_REPO")
        .env_remove("DOT_GITHUB_URL")
        .env_remove("DOT_GITHUB_API")
        .env_remove("DOT_DOTFILES_URL")
        .env_remove("DOT_APPLY_ARGS")
        .env_remove("NO_COLOR");
    for (k, v) in env {
        c.env(k, v);
    }
    let _serial = one_at_a_time();
    Run {
        out: c.output().unwrap(),
        tmp,
    }
}

fn assert_token_to_api_only(ps: &str, api: &Server, storage: &Server, r: &Run) {
    let text = r.text();
    assert!(!r.out.status.success(), "{ps}: {text}");
    assert!(text.contains("release v9.9.9"), "{ps}: {text}");
    assert!(
        text.contains("checksum mismatch for dot-x86_64-pc-windows-msvc.zip"),
        "{ps}: {text}"
    );
    assert!(!text.contains(TOKEN), "{ps}: the token was printed: {text}");
    let seen = api.seen();
    let paths: Vec<&str> = seen
        .iter()
        .map(|h| h.split_whitespace().nth(1).unwrap_or(""))
        .collect();
    assert_eq!(
        paths,
        [
            format!("/{FORK}/releases/latest/download/{ZIP}"),
            format!("{REPO}/latest"),
            format!("{REPO}/assets/11"),
            format!("{REPO}/assets/12")
        ],
        "{ps}"
    );
    // The public URL first, without the token.
    assert!(
        !seen[0].to_lowercase().contains("authorization"),
        "{ps}: {}",
        seen[0]
    );
    let seen = &seen[1..];
    for head in seen {
        assert!(
            head.to_lowercase()
                .contains(&format!("authorization: bearer {}", TOKEN.to_lowercase())),
            "{ps}: {head}"
        );
    }
    assert!(
        seen[1]
            .to_lowercase()
            .contains("accept: application/octet-stream")
    );
    let storage_seen = storage.seen();
    assert_eq!(storage_seen.len(), 1, "{ps}: {storage_seen:?}");
    assert!(
        storage_seen[0].starts_with("GET /signed/asset?sig=abc "),
        "{ps}: {storage_seen:?}"
    );
    assert!(
        !storage_seen[0].to_lowercase().contains("authorization"),
        "{ps}: the storage host got the token: {}",
        storage_seen[0]
    );
    let left: Vec<_> = fs::read_dir(&r.tmp).unwrap().collect();
    assert!(left.is_empty(), "{ps}: temp files left: {left:?}");
}

#[test]
fn a_public_install_needs_no_token_and_looks_for_none() {
    let shells = powershells();
    if shells.is_empty() {
        eprintln!("no PowerShell here: skipped");
        return;
    }
    for ps in shells {
        let (api, storage) = github();
        let root = tempfile::tempdir().unwrap();
        let r = run(
            ps,
            root.path(),
            &[
                ("GITHUB_TOKEN", TOKEN),
                ("DOT_GITHUB_URL", &api.base),
                ("DOT_GITHUB_API", &api.base),
            ],
            Some(TOKEN),
        );
        let text = r.text();
        assert!(!r.out.status.success(), "{ps}: {text}");
        assert!(
            text.contains(&format!("checksum mismatch for {ZIP}")),
            "{ps}: {text}"
        );
        assert!(!text.contains("using the GitHub token"), "{ps}: {text}");
        let seen = api.seen();
        let paths: Vec<&str> = seen
            .iter()
            .map(|h| h.split_whitespace().nth(1).unwrap_or(""))
            .collect();
        assert_eq!(
            paths,
            [format!("{PUBLIC}/{ZIP}"), format!("{PUBLIC}/SHA256SUMS")],
            "{ps}"
        );
        for head in &seen {
            assert!(
                !head.to_lowercase().contains("authorization"),
                "{ps}: {head}"
            );
        }
        assert!(storage.seen().is_empty(), "{ps}");
        let left: Vec<_> = fs::read_dir(&r.tmp).unwrap().collect();
        assert!(left.is_empty(), "{ps}: temp files left: {left:?}");
    }
}

#[test]
fn a_private_forks_token_from_the_environment_goes_to_the_api_only() {
    for ps in powershells() {
        let (api, storage) = github();
        let root = tempfile::tempdir().unwrap();
        let r = run(
            ps,
            root.path(),
            &[
                ("GITHUB_TOKEN", TOKEN),
                ("DOT_RELEASE_REPO", FORK),
                ("DOT_GITHUB_URL", &api.base),
                ("DOT_GITHUB_API", &api.base),
            ],
            None,
        );
        assert!(
            r.text().contains(&format!(
                "using the GitHub token from GITHUB_TOKEN for the release of {FORK}"
            )),
            "{ps}: {}",
            r.text()
        );
        assert_token_to_api_only(ps, &api, &storage, &r);
    }
}

#[test]
fn a_private_forks_token_from_gh_auth_token_goes_to_the_api_only() {
    for ps in powershells() {
        let (api, storage) = github();
        let root = tempfile::tempdir().unwrap();
        let r = run(
            ps,
            root.path(),
            &[
                ("DOT_RELEASE_REPO", FORK),
                ("DOT_GITHUB_URL", &api.base),
                ("DOT_GITHUB_API", &api.base),
            ],
            Some(TOKEN),
        );
        assert!(
            r.text().contains(&format!(
                "using the GitHub token from gh auth token for the release of {FORK}"
            )),
            "{ps}: {}",
            r.text()
        );
        assert_token_to_api_only(ps, &api, &storage, &r);
    }
}

#[test]
fn without_a_token_a_private_forks_release_explains_how_to_make_one() {
    for ps in powershells() {
        let (api, _storage) = github();
        let root = tempfile::tempdir().unwrap();
        let r = run(
            ps,
            root.path(),
            &[
                ("DOT_RELEASE_REPO", FORK),
                ("DOT_GITHUB_URL", &api.base),
                ("DOT_GITHUB_API", &api.base),
            ],
            None,
        );
        let text = r.text();
        assert!(!r.out.status.success(), "{ps}: {text}");
        for want in [
            &format!("(HTTP 404: no published release yet, or {FORK} is private)"),
            &format!("If {FORK} is private, this script needs a GitHub token"),
            "https://github.com/settings/personal-access-tokens/new",
            "Contents: Read-only",
            "Read-Host 'GitHub token' -AsSecureString",
            &format!("$env:DOT_RELEASE_REPO = '{FORK}'"),
            "iwr -useb -Headers @{ Authorization = \"Bearer $env:GITHUB_TOKEN\" }",
            "gh auth login",
        ] {
            assert!(text.contains(want), "{ps}: missing {want:?} in:\n{text}");
        }
        assert_eq!(api.seen().len(), 1, "{ps}: only the public URL");
        for head in api.seen() {
            assert!(
                !head.to_lowercase().contains("authorization"),
                "{ps}: {head}"
            );
        }
        let left: Vec<_> = fs::read_dir(&r.tmp).unwrap().collect();
        assert!(left.is_empty(), "{ps}: temp files left: {left:?}");
    }
}

#[test]
fn a_bad_release_repository_is_refused() {
    for ps in powershells() {
        let root = tempfile::tempdir().unwrap();
        let r = run(ps, root.path(), &[("DOT_RELEASE_REPO", "a/b/c")], None);
        let text = r.text();
        assert!(!r.out.status.success(), "{ps}: {text}");
        assert!(
            text.contains("DOT_RELEASE_REPO must be owner/name (got: a/b/c)"),
            "{ps}: {text}"
        );
    }
}

#[test]
fn a_repository_without_a_release_is_not_blamed_on_the_token() {
    for ps in powershells() {
        let sums = String::new();
        let (api, _storage) = github_serving(Vec::new(), sums.clone(), Repo::NoRelease);
        let root = tempfile::tempdir().unwrap();
        let env = [
            ("GITHUB_TOKEN", TOKEN),
            ("DOT_RELEASE_REPO", FORK),
            ("DOT_GITHUB_URL", api.base.as_str()),
            ("DOT_GITHUB_API", api.base.as_str()),
        ];
        let r = run(ps, root.path(), &env, None);
        let text = r.text();
        assert!(!r.out.status.success(), "{ps}: {text}");
        assert!(
            text.contains(&format!("{FORK} has no published release yet")),
            "{ps}: {text}"
        );
        assert!(!text.contains("cannot read"), "{ps}: {text}");
        assert!(!text.contains(TOKEN), "{ps}: {text}");

        let (api, _storage) = github_serving(Vec::new(), sums, Repo::NoAccess);
        let root = tempfile::tempdir().unwrap();
        let env = [
            ("GITHUB_TOKEN", TOKEN),
            ("DOT_RELEASE_REPO", FORK),
            ("DOT_GITHUB_URL", api.base.as_str()),
            ("DOT_GITHUB_API", api.base.as_str()),
        ];
        let r = run(ps, root.path(), &env, None);
        let text = r.text();
        assert!(!r.out.status.success(), "{ps}: {text}");
        assert!(
            text.contains(&format!(
                "HTTP 404: the token from GITHUB_TOKEN cannot read {FORK}"
            )),
            "{ps}: {text}"
        );
        let left: Vec<_> = fs::read_dir(&r.tmp).unwrap().collect();
        assert!(left.is_empty(), "{ps}: temp files left: {left:?}");
    }
}

/// Writes an executable script (Unix) or batch file (Windows) `name` into `bin`.
fn write_tool(bin: &Path, name: &str, unix: &str, windows: &str) {
    if cfg!(windows) {
        fs::write(bin.join(format!("{name}.cmd")), windows).unwrap();
    } else {
        let p = bin.join(name);
        fs::write(&p, unix).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

/// A fake git for a private repository: `version` says 2.45.1; `ls-remote` (the probe) records
/// its environment and fails, as git's own credentials cannot read it; `clone` records the
/// extra-header environment it was given and makes the checkout. Its argv goes to
/// `$FAKE_LOG/git.argv`.
fn fake_git(bin: &Path) {
    write_tool(
        bin,
        "git",
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_LOG/git.argv"
case "$1" in
version) echo "git version 2.45.1" ;;
ls-remote)
	printf 'prompt=%s gcm=%s\n' "${GIT_TERMINAL_PROMPT:-}" "${GCM_INTERACTIVE:-}" >> "$FAKE_LOG/git.probe"
	exit 128 ;;
clone)
	printf 'count=%s key=%s value=%s prompt=%s\n' "${GIT_CONFIG_COUNT:-}" \
		"${GIT_CONFIG_KEY_0:-}" "${GIT_CONFIG_VALUE_0:-}" "${GIT_TERMINAL_PROMPT:-}" \
		>> "$FAKE_LOG/git.env"
	eval "dest=\${$#}"
	mkdir -p "$dest/.git" ;;
esac
exit 0
"#,
        // Redirections first: `echo ...0>>file` would redirect handle 0.
        "@echo off\r\n\
         >>\"%FAKE_LOG%\\git.argv\" echo %*\r\n\
         if \"%1\"==\"version\" (echo git version 2.45.1& exit /b 0)\r\n\
         if \"%1\"==\"ls-remote\" (>>\"%FAKE_LOG%\\git.probe\" echo prompt=%GIT_TERMINAL_PROMPT% gcm=%GCM_INTERACTIVE%& exit /b 128)\r\n\
         if not \"%1\"==\"clone\" exit /b 0\r\n\
         >>\"%FAKE_LOG%\\git.env\" echo count=%GIT_CONFIG_COUNT% key=%GIT_CONFIG_KEY_0% value=%GIT_CONFIG_VALUE_0% prompt=%GIT_TERMINAL_PROMPT%\r\n\
         mkdir \"%~4\\.git\"\r\n\
         exit /b 0\r\n",
    );
}

/// The release asset: a zip holding `dot.exe`. On Windows the real dot (which the one-liner's
/// run asks for `apply --help`); elsewhere a script that answers `--version` and records its
/// argv and the tokens in its environment.
fn release_zip() -> Vec<u8> {
    use std::io::Cursor;
    let (exe, mode) = if cfg!(windows) {
        (fs::read(env!("CARGO_BIN_EXE_dot")).unwrap(), 0o644)
    } else {
        (
            b"#!/bin/sh\n\
              [ \"$1\" = --version ] && { echo 'dot 9.9.9'; exit 0; }\n\
              printf '%s\\n' \"$*\" >> \"$FAKE_LOG/dot.argv\"\n\
              printf 'GITHUB_TOKEN=%s GH_TOKEN=%s\\n' \"${GITHUB_TOKEN-unset}\" \"${GH_TOKEN-unset}\" >> \"$FAKE_LOG/dot.env\"\n"
                .to_vec(),
            0o755,
        )
    };
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().unix_permissions(mode);
    w.start_file("dot.exe", opts).unwrap();
    w.write_all(&exe).unwrap();
    w.finish().unwrap().into_inner()
}

fn base64(b: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[test]
fn the_one_liner_installs_and_clones_private_dotfiles_with_the_token_in_gits_environment() {
    use sha2::{Digest, Sha256};
    for ps in powershells() {
        let zip = release_zip();
        let sum: String = Sha256::digest(&zip)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let sums = format!("{sum}  dot-x86_64-pc-windows-msvc.zip\n");
        let (api, storage) = github_serving(zip, sums, Repo::Released);
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let log = root.join("log");
        fs::create_dir_all(&log).unwrap();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fake_git(&bin);
        let url = "https://github.com/someone/dotfiles.git";
        let r = run_as(
            ps,
            root,
            &[
                ("GITHUB_TOKEN", TOKEN),
                ("DOT_GITHUB_URL", &api.base),
                ("DOT_GITHUB_API", &api.base),
                ("DOT_DOTFILES_URL", url),
                (
                    "DOT_APPLY_ARGS",
                    if cfg!(windows) { "--help" } else { "--yes" },
                ),
            ],
            None,
            true,
        );
        let text = r.text();
        let read = |f: &str| fs::read_to_string(log.join(f)).unwrap_or_default();
        assert!(r.out.status.success(), "{ps}: {text}\n{}", read("git.argv"));
        for want in [
            "installed dot ",
            "https://github.com/someone/dotfiles.git needs credentials: using the GitHub token \
             from GITHUB_TOKEN",
            "cloning https://github.com/someone/dotfiles.git",
            "running dot apply",
        ] {
            assert!(text.contains(want), "{ps}: missing {want:?} in {text}");
        }
        // The release is public: no token went anywhere, and nothing printed it.
        assert!(!text.contains("release v9.9.9"), "{ps}: {text}");
        assert!(!text.contains(TOKEN), "{ps}: {text}");
        let b64 = base64(format!("x-access-token:{TOKEN}").as_bytes());
        assert!(!text.contains(&b64), "{ps}: {text}");
        assert!(storage.seen().is_empty(), "{ps}");
        let seen = api.seen();
        assert_eq!(seen.len(), 2, "{ps}: {seen:?}");
        for head in &seen {
            assert!(
                !head.to_lowercase().contains("authorization"),
                "{ps}: {head}"
            );
        }

        // git: the probe (no prompt, and it failed), a version check, then the clone, with the
        // token only in its environment.
        assert_eq!(read("git.probe").trim_end(), "prompt=0 gcm=never", "{ps}");
        let dotfiles = root.join("dotfiles");
        let argv = read("git.argv");
        assert!(argv.starts_with("ls-remote -q"), "{ps}: {argv}");
        assert!(argv.contains("\nversion"), "{ps}: {argv}");
        assert!(argv.contains(&format!("clone -- {url} ")), "{ps}: {argv}");
        assert!(
            !argv.contains(TOKEN) && !argv.contains(&b64),
            "{ps}: {argv}"
        );
        assert_eq!(
            read("git.env").trim_end(),
            format!(
                "count=1 key=http.https://github.com/.extraheader value=Authorization: Basic {b64} \
                 prompt=0"
            ),
            "{ps}"
        );
        assert!(dotfiles.join(".git").is_dir(), "{ps}");
        let local_env = fs::read_to_string(root.join("config").join("dot").join("local.env"))
            .unwrap_or_else(|e| panic!("{ps}: {e}: {text}"));
        assert!(local_env.starts_with("DOT_REPO='"), "{ps}: {local_env}");

        // The session (the caller's, under iex) gets its environment back: git's header gone,
        // the caller's GITHUB_TOKEN still there for its own `Remove-Item`.
        assert!(
            text.contains(
                "after: git variables left: 0; GITHUB_TOKEN set=True; GH_TOKEN there=False"
            ),
            "{ps}: {text}"
        );
        // `dot apply` ran without the token.
        if cfg!(windows) {
            assert!(text.contains("--repo"), "{ps}: dot apply --help: {text}");
        } else {
            assert_eq!(
                read("dot.argv"),
                format!("apply --repo {} --yes\n", dotfiles.display()),
                "{ps}"
            );
            assert_eq!(
                read("dot.env"),
                "GITHUB_TOKEN=unset GH_TOKEN=unset\n",
                "{ps}"
            );
        }
        let left: Vec<_> = fs::read_dir(&r.tmp).unwrap().collect();
        assert!(left.is_empty(), "{ps}: temp files left: {left:?}");
    }
}

/// Something else already where the dotfiles go (a directory that is not a git checkout, or
/// another repository's checkout) is refused and left as it is: nothing cloned or applied.
#[test]
fn an_existing_directory_that_is_not_this_checkout_is_refused() {
    use sha2::{Digest, Sha256};
    for ps in powershells() {
        for (said, git) in [
            ("exists and is not a git checkout", false),
            ("is a checkout of", true),
        ] {
            let zip = release_zip();
            let sum: String = Sha256::digest(&zip)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let sums = format!("{sum}  dot-x86_64-pc-windows-msvc.zip\n");
            let (api, _storage) = github_serving(zip, sums, Repo::Released);
            let root = tempfile::tempdir().unwrap();
            let root = root.path();
            let log = root.join("log");
            fs::create_dir_all(&log).unwrap();
            let bin = root.join("bin");
            fs::create_dir_all(&bin).unwrap();
            fake_git(&bin);
            let dotfiles = root.join("dotfiles");
            fs::create_dir_all(dotfiles.join("notes")).unwrap();
            if git {
                fs::create_dir_all(dotfiles.join(".git")).unwrap();
            }
            let r = run(
                ps,
                root,
                &[
                    ("DOT_GITHUB_URL", &api.base),
                    ("DOT_GITHUB_API", &api.base),
                    (
                        "DOT_DOTFILES_URL",
                        "https://github.com/someone/dotfiles.git",
                    ),
                ],
                None,
            );
            let text = r.text();
            assert!(!r.out.status.success(), "{ps}: {text}");
            assert!(
                text.contains(said) && text.contains("set DOT_DOTFILES_DIR"),
                "{ps}: {text}"
            );
            let argv = fs::read_to_string(log.join("git.argv")).unwrap_or_default();
            assert!(!argv.contains("clone"), "{ps}: {argv}");
            assert!(!log.join("dot.argv").exists(), "{ps}");
            assert!(dotfiles.join("notes").is_dir(), "{ps}");
        }
    }
}

#[test]
fn dot_version_pins_the_release_and_bad_versions_are_refused() {
    for ps in powershells() {
        let (api, _storage) = github();
        let root = tempfile::tempdir().unwrap();
        let env = [
            ("DOT_VERSION", "v1.2.3"),
            ("DOT_GITHUB_URL", api.base.as_str()),
            ("DOT_GITHUB_API", api.base.as_str()),
        ];
        let r = run(ps, root.path(), &env, None);
        let text = r.text();
        assert!(!r.out.status.success(), "{ps}: {text}");
        assert!(text.contains("no release v1.2.3"), "{ps}: {text}");
        let seen = api.seen();
        assert_eq!(seen.len(), 1, "{ps}: {seen:?}");
        assert!(
            seen[0].starts_with(&format!(
                "GET /trungngo/dot-cli/releases/download/v1.2.3/{ZIP} "
            )),
            "{ps}: {seen:?}"
        );

        for bad in ["1.2.3", "latest", "v1/../x"] {
            let root = tempfile::tempdir().unwrap();
            let r = run(ps, root.path(), &[("DOT_VERSION", bad)], None);
            let text = r.text();
            assert!(!r.out.status.success(), "{ps}: {text}");
            assert!(
                text.contains("DOT_VERSION must be a release tag"),
                "{ps} {bad}: {text}"
            );
        }
    }
}

#[test]
fn repository_urls_are_checked_before_anything_is_downloaded() {
    for ps in powershells() {
        for bad in [
            "http://github.com/a/b",
            "file:///tmp/x",
            "C:\\dotfiles",
            "-oProxyCommand=x",
            "ssh://-oProxyCommand=x/y",
            "ssh://me:pw@host/x",
            "https://tok@github.com/a/b",
        ] {
            let (api, _storage) = github();
            let root = tempfile::tempdir().unwrap();
            let env = [
                ("DOT_DOTFILES_URL", bad),
                ("DOT_GITHUB_URL", api.base.as_str()),
            ];
            let r = run(ps, root.path(), &env, None);
            assert!(!r.out.status.success(), "{ps} {bad}: {}", r.text());
            assert!(api.seen().is_empty(), "{ps} {bad}: downloaded first");
        }
    }
}

#[test]
fn an_ssh_url_never_gets_the_token_and_another_recorded_repository_is_kept() {
    use sha2::{Digest, Sha256};
    for ps in powershells() {
        let zip = release_zip();
        let sum: String = Sha256::digest(&zip)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let sums = format!("{sum}  dot-x86_64-pc-windows-msvc.zip\n");
        let (api, _storage) = github_serving(zip, sums, Repo::Released);
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let log = root.join("log");
        fs::create_dir_all(&log).unwrap();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fake_git(&bin);
        let local_env = root.join("config").join("dot").join("local.env");
        fs::create_dir_all(local_env.parent().unwrap()).unwrap();
        fs::write(&local_env, "DOT_REPO='/elsewhere/dotfiles'\n").unwrap();
        let url = "git@github.com:someone/dotfiles.git";
        let r = run(
            ps,
            root,
            &[
                ("GITHUB_TOKEN", TOKEN),
                ("DOT_GITHUB_URL", &api.base),
                ("DOT_GITHUB_API", &api.base),
                ("DOT_DOTFILES_URL", url),
                (
                    "DOT_APPLY_ARGS",
                    if cfg!(windows) { "--help" } else { "--yes" },
                ),
            ],
            Some(TOKEN),
        );
        let text = r.text();
        let read = |f: &str| fs::read_to_string(log.join(f)).unwrap_or_default();
        assert!(r.out.status.success(), "{ps}: {text}");
        assert!(!text.contains("using the GitHub token"), "{ps}: {text}");
        assert!(read("git.probe").is_empty(), "{ps}: probed");
        let argv = read("git.argv");
        assert!(argv.contains(&format!("clone -- {url} ")), "{ps}: {argv}");
        assert!(!read("git.env").contains("extraheader"), "{ps}");
        // Write-Warning wraps long lines at the console width on Windows.
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("already has DOT_REPO=/elsewhere/dotfiles, left as it is"),
            "{ps}: {text}"
        );
        assert_eq!(
            fs::read_to_string(&local_env).unwrap(),
            "DOT_REPO='/elsewhere/dotfiles'\n"
        );
    }
}
