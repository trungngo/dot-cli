//! `dot self-update` end to end, against a GitHub stand-in on 127.0.0.1 (`DOT_GITHUB_API`):
//! a copy of the built binary in a temp dir replaces itself with the release's. Unix-only: the
//! "new release" is a shell script (and the fake `gh` too). PATH holds only a fake `gh`, so no
//! real GitHub CLI is ever asked for a token.
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

/// Held while this test binary writes an executable or starts a process. A process forked by
/// one test while another test still has an executable open for writing inherits that
/// descriptor until it execs; running the executable meanwhile fails with ETXTBSY ("Text file
/// busy").
static EXEC: Mutex<()> = Mutex::new(());

fn exec_lock() -> std::sync::MutexGuard<'static, ()> {
    EXEC.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `exe` with no arguments.
fn run_plain(exe: &Path) -> Output {
    let _exec = exec_lock();
    Command::new(exe).output().unwrap()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn tar_gz(name: &str, data: &[u8]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o755);
    h.set_entry_type(tar::EntryType::Regular);
    b.append_data(&mut h, name, data).unwrap();
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(&b.into_inner().unwrap()).unwrap();
    e.finish().unwrap()
}

/// This machine's asset name, as `install.sh` builds it.
fn asset() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => "dot-universal-apple-darwin.tar.gz".into(),
        (_, arch) => format!("dot-{arch}-unknown-linux-musl.tar.gz"),
    }
}

/// Serves release `tag` of trungngo/dot-cli, with `archive` as this machine's asset and
/// `sum` (default: the archive's own) as its checksum in SHA256SUMS.
fn serve(tag: &str, archive: Vec<u8>, sum: Option<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let asset = asset();
    let release = serde_json::json!({
        "tag_name": tag,
        "assets": [
            {"name": asset, "browser_download_url": format!("{base}/dl/{asset}")},
            {"name": "SHA256SUMS", "browser_download_url": format!("{base}/dl/SHA256SUMS")},
        ]
    })
    .to_string();
    let sum = sum.unwrap_or_else(|| hex(&Sha256::digest(&archive)));
    let sums = format!("{sum}  {asset}\n");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
            }
            let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
            let (status, body): (&str, Vec<u8>) = match path.as_str() {
                "/repos/trungngo/dot-cli/releases/latest" => ("200 OK", release.clone().into()),
                "/dl/SHA256SUMS" => ("200 OK", sums.clone().into()),
                p if p == format!("/dl/{asset}") => ("200 OK", archive.clone()),
                _ => ("404 Not Found", b"{}".to_vec()),
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    base
}

fn run(exe: &Path, api: &str, args: &[&str]) -> Output {
    run_with_gh(exe, api, args, None)
}

/// Runs `dot self-update` with only a fake `gh` on PATH: it prints `gh_token` for `gh auth
/// token`, or fails as when logged out.
fn run_with_gh(exe: &Path, api: &str, args: &[&str], gh_token: Option<&str>) -> Output {
    let _exec = exec_lock();
    let bin = exe.parent().unwrap().join("fakebin");
    fs::create_dir_all(&bin).unwrap();
    let script = match gh_token {
        Some(t) => format!(
            "#!/bin/sh\n[ \"$*\" = 'auth token --hostname github.com' ] && {{ echo {t}; exit 0; }}\nexit 1\n"
        ),
        None => "#!/bin/sh\nexit 1\n".to_string(),
    };
    fs::write(bin.join("gh"), script).unwrap();
    fs::set_permissions(bin.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(exe)
        .arg("self-update")
        .args(args)
        .env("PATH", &bin)
        .env("DOT_GITHUB_API", api)
        .env_remove("DOT_RELEASE_REPO")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .output()
        .unwrap();
    fs::remove_dir_all(&bin).unwrap();
    out
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// A copy of the built `dot` that may replace itself.
fn copy_of_dot(dir: &Path) -> PathBuf {
    let exe = dir.join("dot");
    let _exec = exec_lock();
    fs::copy(env!("CARGO_BIN_EXE_dot"), &exe).unwrap();
    exe
}

#[test]
fn self_update_replaces_the_running_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let exe = copy_of_dot(&dir);
    let new = b"#!/bin/sh\necho 'dot 99.0.0 (the new one)'\n";
    let api = serve("v99.0.0", tar_gz("dot", new), None);
    let current = env!("CARGO_PKG_VERSION");

    let out = run(&exe, &api, &["--check"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        text(&out),
        format!("update available: dot {current} -> 99.0.0 (run `dot self-update`)\n")
    );
    assert_eq!(
        fs::read(&exe).unwrap(),
        fs::read(env!("CARGO_BIN_EXE_dot")).unwrap()
    );

    let out = run(&exe, &api, &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        text(&out),
        format!("dot {current} -> 99.0.0 ({})\n", exe.display())
    );
    let after = run_plain(&exe);
    assert_eq!(text(&after), "dot 99.0.0 (the new one)\n");
    let names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["dot"], "nothing left behind");
}

#[test]
fn a_corrupted_download_is_not_installed() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = copy_of_dot(&tmp.path().canonicalize().unwrap());
    let archive = tar_gz("dot", b"#!/bin/sh\necho new\n");
    // SHA256SUMS lists the whole archive; half of it arrives.
    let sum = hex(&Sha256::digest(&archive));
    let api = serve("v99.0.0", archive[..archive.len() / 2].to_vec(), Some(sum));
    let out = run(&exe, &api, &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("checksum mismatch"), "{}", text(&out));
    assert_eq!(
        fs::read(&exe).unwrap(),
        fs::read(env!("CARGO_BIN_EXE_dot")).unwrap()
    );
}

/// An HTTP server on 127.0.0.1 answering from `route(path)` with `(status, location, body)`
/// and recording each request's head.
fn listen(
    route: impl Fn(&str) -> (u16, Option<String>, Vec<u8>) + Send + 'static,
) -> (String, Arc<Mutex<Vec<String>>>) {
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
            let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
            log.lock().unwrap().push(head);
            let (status, location, body) = route(&path);
            let location = location
                .map(|l| format!("Location: {l}\r\n"))
                .unwrap_or_default();
            let head = format!(
                "HTTP/1.1 {status} X\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (base, seen)
}

#[test]
fn a_token_from_gh_takes_the_release_through_the_api() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let exe = copy_of_dot(&dir);
    let archive = tar_gz("dot", b"#!/bin/sh\necho 'dot 99.0.0 (private)'\n");
    let sums = format!("{}  {}\n", hex(&Sha256::digest(&archive)), asset());
    // The storage host: another origin (another port), where the API's redirects lead.
    let (storage, storage_seen) = listen(move |path| match path {
        "/signed/archive?sig=1" => (200, None, archive.clone()),
        "/signed/sums?sig=2" => (200, None, sums.clone().into_bytes()),
        _ => (404, None, Vec::new()),
    });
    let asset = asset();
    let release = serde_json::json!({
        "tag_name": "v99.0.0",
        "assets": [
            // A private repository's browser URLs need a browser session: never used.
            {"name": asset, "id": 11, "browser_download_url": "https://github.invalid/a"},
            {"name": "SHA256SUMS", "id": 12, "browser_download_url": "https://github.invalid/s"},
        ]
    })
    .to_string();
    let (api, api_seen) = listen(move |path| match path {
        "/repos/trungngo/dot-cli/releases/latest" => (200, None, release.clone().into()),
        "/repos/trungngo/dot-cli/releases/assets/11" => (
            302,
            Some(format!("{storage}/signed/archive?sig=1")),
            Vec::new(),
        ),
        "/repos/trungngo/dot-cli/releases/assets/12" => (
            302,
            Some(format!("{storage}/signed/sums?sig=2")),
            Vec::new(),
        ),
        _ => (404, None, b"{}".to_vec()),
    });
    let token = "gho_FAKEtoken0123456789";

    let out = run_with_gh(&exe, &api, &[], Some(token));
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains(token), "{}", text(&out));
    let after = run_plain(&exe);
    assert_eq!(text(&after), "dot 99.0.0 (private)\n");

    let api_seen = api_seen.lock().unwrap().clone();
    assert_eq!(api_seen.len(), 3, "{api_seen:?}");
    for head in &api_seen {
        assert!(
            head.to_lowercase()
                .contains(&format!("authorization: bearer {}", token.to_lowercase())),
            "{head}"
        );
    }
    assert!(
        api_seen[1..].iter().all(|h| h
            .to_lowercase()
            .contains("accept: application/octet-stream")),
        "{api_seen:?}"
    );
    let storage_seen = storage_seen.lock().unwrap().clone();
    assert_eq!(storage_seen.len(), 2, "{storage_seen:?}");
    for head in &storage_seen {
        assert!(
            !head.to_lowercase().contains("authorization"),
            "the storage host got the token: {head}"
        );
    }
}

#[test]
fn without_a_token_a_failed_lookup_says_how_to_give_one() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = copy_of_dot(&tmp.path().canonicalize().unwrap());
    // What the API answers for a private repository without a token.
    let (api, seen) = listen(|_| (404, None, br#"{"message":"Not Found"}"#.to_vec()));
    let out = run(&exe, &api, &["--check"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("(trungngo/dot-cli; if it is private"),
        "{}",
        text(&out)
    );
    for head in seen.lock().unwrap().iter() {
        assert!(!head.to_lowercase().contains("authorization"), "{head}");
    }
}
