use std::collections::HashMap;
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

use dot_core::facts::Os;
use dot_platform::exec::Output;
use dot_platform::http::Response;

use super::*;
use crate::Builtins;
use crate::github_release::Github;
use crate::testutil::Fixture;
use crate::testutil_sys::fake_builtins;
use dot_core::{EventSink, Options, engine};

const URL: &str = "https://dl.test/tool";
const API: &str = "https://api.test";
const GITLAB_API: &str = "https://gitlab.test/api/v4";

/// An in-memory server: one body and ETag per URL; a request whose `If-None-Match` is the
/// current ETag gets 304.
#[derive(Default)]
struct FakeHttp {
    files: Mutex<HashMap<String, (Vec<u8>, String)>>,
    log: Mutex<Vec<Request>>,
    offline: Mutex<bool>,
}

impl FakeHttp {
    fn serve(&self, url: &str, body: impl Into<Vec<u8>>, etag: &str) {
        self.files
            .lock()
            .unwrap()
            .insert(url.to_string(), (body.into(), etag.to_string()));
    }

    fn requests(&self) -> Vec<Request> {
        self.log.lock().unwrap().clone()
    }
}

impl HttpClient for FakeHttp {
    fn get(&self, req: &Request) -> anyhow::Result<dot_platform::http::Response> {
        self.log.lock().unwrap().push(req.clone());
        if *self.offline.lock().unwrap() {
            anyhow::bail!("GET {}: connection refused", req.url);
        }
        let Some((body, etag)) = self.files.lock().unwrap().get(&req.url).cloned() else {
            return Ok(Response {
                status: 404,
                headers: Vec::new(),
                content_length: None,
                body: Box::new(Cursor::new(Vec::new())),
            });
        };
        let status = if req.get_header("if-none-match") == Some(etag.as_str()) {
            304
        } else {
            200
        };
        Ok(Response {
            status,
            headers: vec![("etag".into(), etag)],
            content_length: Some(body.len() as u64),
            body: Box::new(Cursor::new(if status == 200 { body } else { Vec::new() })),
        })
    }
}

fn setup(
    f: &Fixture,
) -> (
    Builtins,
    Arc<FakeHttp>,
    Arc<dot_platform::exec::fake::FakeExec>,
) {
    let http = Arc::new(FakeHttp::default());
    let (mut b, fake) = fake_builtins();
    b.downloads = Downloads::new(http.clone());
    b.downloads.state_dir = Some(f.home().join(".state"));
    // Release lookups go to the same in-memory server.
    b.github = Github::new(http.clone(), API);
    b.github.gitlab_api = Some(GITLAB_API.into());
    // SAFETY: getuid cannot fail.
    b.root_uid = unsafe { libc::getuid() };
    (b, http, fake)
}

/// Plans as a new run does: nothing asked of the server yet.
fn plan(f: &Fixture, b: &Builtins, step: usize) -> Vec<Box<dyn Action>> {
    new_run(b);
    f.plan_by(step, &f.context(), b).unwrap()
}

/// Forgets what this run already asked the servers.
fn new_run(b: &Builtins) {
    b.downloads.checked.lock().unwrap().clear();
    b.downloads.latest.lock().unwrap().clear();
}

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn a_file_is_downloaded_then_kept_while_the_server_says_unchanged() {
    let f = Fixture::new(&format!(
        "steps:\n  - download: {{ url: {URL}, dest: ~/.local/bin/tool, mode: \"0755\" }}\n"
    ));
    let (b, http, _) = setup(&f);
    http.serve(URL, "v1", "\"e1\"");
    let actions = plan(&f, &b, 0);
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert_eq!(
        actions[0].describe(),
        format!("download {URL} -> ~/.local/bin/tool (not downloaded yet; not pinned (no sha256))")
    );
    assert!(
        http.requests().is_empty(),
        "planning a new file asks nothing"
    );
    assert_eq!(f.apply_one(actions[0].as_ref()).unwrap(), Outcome::Changed);
    let dest = f.home().join(".local/bin/tool");
    assert_eq!(fs::read_to_string(&dest).unwrap(), "v1");
    assert_eq!(
        fs::metadata(&dest).unwrap().permissions().mode() & 0o7777,
        0o755
    );
    // No temporary files left next to it.
    assert_eq!(fs::read_dir(dest.parent().unwrap()).unwrap().count(), 1);
    // Recorded in the state store; what `dot remove` undoes: the file, while it holds v1.
    let store = dot_core::StateStore::at(f.home().join(".state"));
    let downloads = &store.read().sections[dot_core::sections::DOWNLOADS];
    assert_eq!(downloads[dest.display().to_string()]["sha256"], sha(b"v1"));
    let r = actions[0].resources();
    assert_eq!(r[0].key(), "file:~/.local/bin/tool");
    assert_eq!(r[0].preserve.as_deref(), Some(dest.as_path()));
    assert!(matches!(
        &r[0].undo,
        Some(dot_core::Undo::RemoveFiles { files, root: false })
            if files[0].sha256.as_deref() == Some(sha(b"v1").as_str())
    ));
    assert_eq!(actions[0].diff().unwrap(), None);

    // Planned again: a conditional request, answered 304.
    let actions = plan(&f, &b, 0);
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    let last = http.requests().pop().unwrap();
    assert_eq!(last.get_header("if-none-match"), Some("\"e1\""));

    // A new file upstream is an update; offline, what is there is kept.
    http.serve(URL, "v2", "\"e2\"");
    let actions = plan(&f, &b, 0);
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert!(actions[0].describe().contains("update: changed upstream"));
    assert_eq!(
        actions[0].diff().unwrap(),
        Some(dot_core::Diff::Note(format!(
            "~/.local/bin/tool <- {URL}: update: changed upstream"
        )))
    );
    *http.offline.lock().unwrap() = true;
    let actions = plan(&f, &b, 0);
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    assert!(
        actions[0]
            .describe()
            .contains("not checked for a newer file"),
        "{}",
        actions[0].describe()
    );
    *http.offline.lock().unwrap() = false;
    f.apply_one(plan(&f, &b, 0)[0].as_ref()).unwrap();
    assert_eq!(fs::read_to_string(&dest).unwrap(), "v2");

    // Edited locally: downloaded again.
    fs::write(&dest, "mine").unwrap();
    let actions = plan(&f, &b, 0);
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert!(
        actions[0]
            .describe()
            .contains("changed since it was downloaded")
    );
}

#[test]
fn a_vendors_published_digest_is_checked_without_pinning_one() {
    // go.dev: `go<ver>.<os>-<arch>.tar.gz.sha256` next to the file holds its bare digest.
    let f = Fixture::new(&format!(
        "steps:\n  - download: {{ url: '{URL}/{{latest}}.tgz', latest: {URL}/VERSION, \
         sha256_url: '{URL}/{{latest}}.tgz.sha256', dest: ~/tool.tgz }}\n"
    ));
    let (b, http, _) = setup(&f);
    http.serve(&format!("{URL}/VERSION"), "go1.99\n", "\"v\"");
    http.serve(&format!("{URL}/go1.99.tgz"), "tarball", "\"t\"");
    http.serve(
        &format!("{URL}/go1.99.tgz.sha256"),
        format!("{}\n", sha(b"other")),
        "\"s\"",
    );
    let actions = plan(&f, &b, 0);
    let d = actions[0].describe();
    assert!(!d.contains("not pinned"), "{d}");
    let err = f.apply_err(actions[0].as_ref());
    assert!(err.contains("sha256 mismatch"), "{err}");
    assert!(err.contains(&format!("{URL}/go1.99.tgz.sha256")), "{err}");
    assert!(!f.home().join("tool.tgz").exists());
    // A sha256sum list naming the file works too.
    http.serve(
        &format!("{URL}/go1.99.tgz.sha256"),
        format!(
            "{}  other.tgz\n{}  go1.99.tgz\n",
            sha(b"x"),
            sha(b"tarball")
        ),
        "\"s2\"",
    );
    let actions = plan(&f, &b, 0);
    assert_eq!(f.apply_one(actions[0].as_ref()).unwrap(), Outcome::Changed);
    assert_eq!(
        fs::read_to_string(f.home().join("tool.tgz")).unwrap(),
        "tarball"
    );
    // Both kinds of digest at once is a mistake.
    let e = dot_core::config::parse_bundle(
        &format!(
            "steps:\n  - download: {{ url: {URL}, dest: ~/x, sha256: {}, sha256_url: {URL}.sha256 }}\n",
            sha(b"x")
        ),
        std::path::Path::new("/b/bundle.yml"),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("cannot be combined"), "{e}");
}

#[test]
fn a_pinned_digest_is_checked_before_anything_is_written() {
    let f = Fixture::new(&format!(
        "steps:\n  - download: {{ url: {URL}, dest: ~/tool, sha256: {} }}\n",
        sha(b"good")
    ));
    let (b, http, _) = setup(&f);
    http.serve(URL, "evil", "\"x\"");
    let actions = plan(&f, &b, 0);
    let err = f.apply_err(actions[0].as_ref());
    assert!(err.contains("sha256 mismatch"), "{err}");
    assert!(!f.home().join("tool").exists());
    http.serve(URL, "good", "\"x\"");
    f.apply_one(actions[0].as_ref()).unwrap();
    // Satisfied without asking the server: the digest pins it.
    let n = http.requests().len();
    let actions = plan(&f, &b, 0);
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    assert!(!actions[0].describe().contains("not pinned"));
    assert_eq!(http.requests().len(), n);
}

fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (name, data) in entries {
        let mut h = tar::Header::new_gnu();
        let raw = &mut h.as_gnu_mut().unwrap().name;
        raw[..name.len()].copy_from_slice(name.as_bytes());
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        b.append(&h, *data).unwrap();
    }
    b.into_inner().unwrap().finish().unwrap()
}

#[test]
fn an_archive_is_unpacked_into_a_directory() {
    let url = "https://dl.test/go.tar.gz";
    let f = Fixture::new(&format!(
        "steps:\n  - download: {{ url: {url}, dest: ~/opt, extract: auto }}\n"
    ));
    let (b, http, _) = setup(&f);
    http.serve(
        url,
        tar_gz(&[("go/bin/go", b"#!go"), ("go/VERSION", b"1.23")]),
        "\"g\"",
    );
    let dest = f.home().join("opt");
    fs::create_dir_all(dest.join("go")).unwrap();
    fs::write(dest.join("go/old"), "kept").unwrap();
    fs::write(dest.join("go/VERSION"), "1.22").unwrap();
    let actions = plan(&f, &b, 0);
    assert!(
        actions[0]
            .describe()
            .starts_with(&format!("download {url} and unpack it (tar.gz) into ~/opt")),
        "{}",
        actions[0].describe()
    );
    f.apply_one(actions[0].as_ref()).unwrap();
    assert_eq!(fs::read_to_string(dest.join("go/VERSION")).unwrap(), "1.23");
    assert_eq!(fs::read_to_string(dest.join("go/bin/go")).unwrap(), "#!go");
    assert_eq!(fs::read_to_string(dest.join("go/old")).unwrap(), "kept");
    assert_eq!(plan(&f, &b, 0)[0].status().unwrap(), Status::Satisfied);
    // Merged into what was there: `dot remove` says what to do by hand.
    let r = actions[0].resources();
    assert_eq!(r[0].key(), "archive:~/opt");
    assert!(
        matches!(&r[0].undo, Some(dot_core::Undo::Manual { note }) if note.contains("go.tar.gz"))
    );

    // An entry reaching outside the directory is refused, and nothing is installed.
    http.serve(url, tar_gz(&[("../evil", b"x")]), "\"h\"");
    let actions = plan(&f, &b, 0);
    let err = f.apply_err(actions[0].as_ref());
    assert!(err.contains("outside"), "{err}");
    assert!(!f.home().join("evil").exists());
}

#[test]
fn a_root_file_is_installed_through_sudo() {
    let f = Fixture::new(&format!(
        "steps:\n  - download: {{ url: {URL}, dest: ~/usr/local/bin/tool, mode: \"0755\", root: true }}\n"
    ));
    let (b, http, fake) = setup(&f);
    http.serve(URL, "v1", "\"e1\"");
    let actions = plan(&f, &b, 0);
    assert!(actions[0].needs_sudo());
    assert!(actions[0].describe().ends_with("as root)"));
    let r = actions[0].resources();
    assert_eq!(r[0].key(), "file:~/usr/local/bin/tool");
    assert_eq!(
        r[0].preserve, None,
        "saved by apply, through sudo if need be"
    );
    assert!(matches!(
        &r[0].undo,
        Some(dot_core::Undo::RemoveFiles { root: true, .. })
    ));
    f.apply_one(actions[0].as_ref()).unwrap();
    let calls = fake.calls();
    let dest = f.home().join("usr/local/bin/tool");
    assert!(
        calls
            .last()
            .unwrap()
            .starts_with("sudo -n install -m 0755 -o root -g root /")
            && calls
                .last()
                .unwrap()
                .ends_with(&format!(" {}", dest.display())),
        "{calls:?}"
    );
}

#[test]
fn a_deb_is_installed_through_apt_and_then_satisfied() {
    let url = "https://dl.test/tool_1.0_amd64.deb";
    let f = Fixture::new(&format!(
        "steps:\n  - download: {{ url: {url}, package: tool }}\n"
    ));
    let (b, http, fake) = setup(&f);
    http.serve(url, "deb-bytes", "\"d1\"");
    let mut ctx = f.context();
    ctx.facts.pkg = Some("apt".into());
    fake.on("dpkg-query -W", Output::exit(1, ""));
    let actions = f.plan_by(0, &ctx, &b).unwrap();
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert!(actions[0].needs_sudo());
    // A package like any other: `pkg:apt:tool`, uninstalled by `dot remove --packages`.
    let r = actions[0].resources();
    assert_eq!(r[0].key(), "pkg:apt:tool");
    assert!(matches!(
        &r[0].undo,
        Some(dot_core::Undo::Uninstall { manager, name }) if manager == "apt" && name == "tool"
    ));
    assert_eq!(
        actions[0].diff().unwrap(),
        Some(dot_core::Diff::Packages {
            manager: "apt".into(),
            missing: vec!["tool".into()],
            outdated: Vec::new(),
        })
    );
    assert!(
        actions[0].describe().starts_with(&format!(
            "download {url} and install it as package tool (apt) (not installed"
        )),
        "{}",
        actions[0].describe()
    );
    fake.clear_calls();
    f.apply_one(actions[0].as_ref()).unwrap();
    let install = fake
        .calls()
        .into_iter()
        .find(|c| c.contains("apt-get"))
        .unwrap();
    assert!(
        install.starts_with("sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y /")
            && install.ends_with("/tool.deb"),
        "{install}"
    );
    fake.on("dpkg-query -W", Output::ok("install ok installed\t1.0"));
    let actions = f.plan_by(0, &ctx, &b).unwrap();
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    assert!(actions[0].describe().contains("tool 1.0 installed"));
    // A newer file upstream: an update.
    http.serve(url, "deb-2", "\"d2\"");
    b.downloads.checked.lock().unwrap().clear();
    assert_eq!(
        f.plan_by(0, &ctx, &b).unwrap()[0].status().unwrap(),
        Status::Pending
    );
    // rpm systems use dnf; others cannot install packages from files.
    ctx.facts.pkg = Some("dnf".into());
    let actions = f.plan_by(0, &ctx, &b).unwrap();
    assert!(actions[0].describe().contains("(dnf)"));
    ctx.facts.pkg = Some("brew".into());
    ctx.facts.os = Os::Macos;
    let err = format!("{:#}", f.plan_by(0, &ctx, &b).err().unwrap());
    assert!(err.contains("package manager is brew"), "{err}");
}

#[test]
fn parse_errors() {
    let f = Fixture::new("steps: []\n");
    for (yaml, want) in [
        ("{ url: 'http://x/y', dest: ~/y }", "not an https:// URL"),
        (
            "{ url: 'file:///etc/passwd', dest: ~/y }",
            "not an https:// URL",
        ),
        ("{ url: 'https://u:p@x/y', dest: ~/y }", "credentials"),
        (
            "{ url: 'https://x/y' }",
            "exactly one of `dest` and `package`",
        ),
        (
            "{ url: 'https://x/y', dest: ~/y, package: y }",
            "exactly one of",
        ),
        (
            "{ url: 'https://x/y', dest: ~/y, sha256: abc }",
            "64 hexadecimal",
        ),
        (
            "{ url: 'https://x/y', dest: rel }",
            "must be an absolute path",
        ),
        (
            "{ url: 'https://x/y.deb', package: y, root: true }",
            "do not apply",
        ),
        (
            "{ url: 'https://x/y', dest: ~/y, extract: zip, mode: \"0600\" }",
            "`mode` is for a saved file",
        ),
    ] {
        let file = f.root().join("bad.yml");
        fs::write(&file, format!("steps:\n  - download: {yaml}\n")).unwrap();
        let err = dot_core::config::load_bundle(&file)
            .unwrap_err()
            .to_string();
        assert!(err.contains(want), "{yaml}: {err}");
        assert!(err.contains("bad.yml:2:"), "{err}");
    }
}

#[test]
fn a_secret_in_the_url_is_never_shown_or_recorded() {
    let f = Fixture::new(
        "steps:\n  - download: { url: \"https://dl.test/tool?token={{ secret('tok') }}\", dest: ~/tool }\n",
    );
    let (b, http, _) = setup(&f);
    let real = "https://dl.test/tool?token=S3CRETVALUE";
    http.serve(real, "v1", "\"e1\"");
    // Planning: a placeholder, and nothing is asked of the server.
    let actions = plan(&f, &b, 0);
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert!(
        actions[0]
            .describe()
            .starts_with("download https://dl.test/tool?token=<secret:tok> -> ~/tool"),
        "{}",
        actions[0].describe()
    );
    assert!(http.requests().is_empty());
    // Applying: requested with the value, shown and recorded without it.
    new_run(&b);
    let actions = crate::testutil_sys::plan_applying_by(&f, &b, 0, &[("tok", "S3CRETVALUE")]);
    assert!(!actions[0].describe().contains("S3CRETVALUE"));
    f.apply_one(actions[0].as_ref()).unwrap();
    assert_eq!(http.requests().last().unwrap().url, real);
    assert_eq!(fs::read_to_string(f.home().join("tool")).unwrap(), "v1");
    let state = fs::read_to_string(f.home().join(".state/state.json")).unwrap();
    assert!(!state.contains("S3CRETVALUE"), "{state}");
    assert!(state.contains("token=<secret:tok>"), "{state}");
    // Satisfied on the next run, asking with the value.
    new_run(&b);
    let actions = crate::testutil_sys::plan_applying_by(&f, &b, 0, &[("tok", "S3CRETVALUE")]);
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    assert!(
        actions[0].diff().unwrap().is_none(),
        "nothing to show, and never the value"
    );
    // A failure names the URL without the value.
    fs::write(f.home().join("tool"), "edited").unwrap();
    *http.offline.lock().unwrap() = true;
    new_run(&b);
    let actions = crate::testutil_sys::plan_applying_by(&f, &b, 0, &[("tok", "S3CRETVALUE")]);
    let d = format!("{:?}", actions[0].diff().unwrap());
    assert!(!d.contains("S3CRETVALUE"), "{d}");
    let err = f.apply_err(actions[0].as_ref());
    assert!(!err.contains("S3CRETVALUE"), "{err}");
    assert!(err.contains("<secret:tok>"), "{err}");
}

#[test]
fn a_tree_follows_the_latest_version_and_is_replaced_whole() {
    let f = Fixture::new(
        "steps:\n  - download:\n      url: 'https://dl.test/{latest}.linux-amd64.tar.gz'\n      latest: { url: 'https://dl.test/VERSION?m=text', pattern: '^(go[0-9.]+)' }\n      dest: ~/sdk/go\n      extract: tar.gz\n      strip: 1\n      replace: true\n      bin: [bin/go]\n",
    );
    let (b, http, _) = setup(&f);
    let version = "https://dl.test/VERSION?m=text";
    http.serve(version, "go1.23.4\ntime 2024-12-03\n", "\"v1\"");
    http.serve(
        "https://dl.test/go1.23.4.linux-amd64.tar.gz",
        tar_gz(&[
            ("go/bin/go", b"#!go1.23"),
            ("go/VERSION", b"go1.23.4"),
            ("go/src/old.go", b"old"),
        ]),
        "\"a\"",
    );
    let actions = plan(&f, &b, 0);
    let d = actions[0].describe();
    assert!(
        d.starts_with(
            "download https://dl.test/go1.23.4.linux-amd64.tar.gz and unpack it (tar.gz) as ~/sdk/go (latest: go1.23.4; not downloaded yet"
        ),
        "{d}"
    );
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    f.apply_one(actions[0].as_ref()).unwrap();
    let go = f.home().join("sdk/go");
    assert_eq!(fs::read_to_string(go.join("bin/go")).unwrap(), "#!go1.23");
    assert!(go.join("src/old.go").exists());
    let link = f.home().join(".local/bin/go");
    assert_eq!(fs::read_link(&link).unwrap(), go.join("bin/go"));
    // No staging left next to it.
    let names: Vec<String> = fs::read_dir(f.home().join("sdk"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["go"]);
    assert_eq!(plan(&f, &b, 0)[0].status().unwrap(), Status::Satisfied);
    // What `dot remove` undoes: the tree while it is as unpacked, and the link.
    let r = plan(&f, &b, 0)[0].resources();
    assert_eq!(r[0].key(), "tree:~/sdk/go");
    assert!(matches!(
        &r[0].undo,
        Some(dot_core::Undo::RemoveTree {
            fingerprint: Some(_),
            root: false,
            ..
        })
    ));
    assert_eq!(r[1].key(), "link:~/.local/bin/go");

    // A new version: the old tree goes whole, nothing of it lingers.
    http.serve(version, "go1.24.0\n", "\"v2\"");
    http.serve(
        "https://dl.test/go1.24.0.linux-amd64.tar.gz",
        tar_gz(&[("go/bin/go", b"#!go1.24"), ("go/VERSION", b"go1.24.0")]),
        "\"b\"",
    );
    let actions = plan(&f, &b, 0);
    assert!(
        actions[0]
            .describe()
            .contains("update: was downloaded from"),
        "{}",
        actions[0].describe()
    );
    f.apply_one(actions[0].as_ref()).unwrap();
    assert_eq!(fs::read_to_string(go.join("bin/go")).unwrap(), "#!go1.24");
    assert!(!go.join("src").exists());
    assert_eq!(fs::read_link(&link).unwrap(), go.join("bin/go"));
    // A removed link is put back without downloading again.
    fs::remove_file(&link).unwrap();
    let n = http.requests().len();
    let actions = plan(&f, &b, 0);
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    f.apply_one(actions[0].as_ref()).unwrap();
    assert!(link.is_symlink());
    assert!(
        !http.requests()[n..]
            .iter()
            .any(|r| r.url.ends_with(".tar.gz") && r.get_header("if-none-match").is_none()),
        "not downloaded again: {:?}",
        http.requests()
    );

    // Offline, the version cannot be asked: what is there is kept.
    *http.offline.lock().unwrap() = true;
    let actions = plan(&f, &b, 0);
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    assert!(actions[0].describe().contains("latest version unknown"));
}

#[test]
fn a_tree_that_was_there_is_kept_and_dot_remove_puts_it_back() {
    let url = "https://dl.test/zed-linux-x86_64.tar.gz";
    let f = Fixture::new(&format!(
        "steps:\n  - download: {{ url: {url}, dest: ~/.local/zed.app, extract: auto, strip: 1, replace: true, bin: {{ zed: bin/zed }} }}\n"
    ));
    let (b, http, _) = setup(&f);
    http.serve(
        url,
        tar_gz(&[
            ("zed.app/bin/zed", b"#!zed"),
            ("zed.app/libexec/zed-editor", b"editor"),
        ]),
        "\"z\"",
    );
    let app = f.home().join(".local/zed.app");
    fs::create_dir_all(app.join("bin")).unwrap();
    fs::write(app.join("bin/zed"), "my own zed").unwrap();
    let s = engine::run(
        std::slice::from_ref(&f.bundle),
        &b,
        &f.context(),
        &EventSink::new(),
        Options::default(),
    );
    assert!(s.success(), "{s:?}");
    assert_eq!(
        fs::read_to_string(app.join("libexec/zed-editor")).unwrap(),
        "editor"
    );
    let backup = f.home().join(".local/zed.app.dot-backup");
    assert_eq!(
        fs::read_to_string(backup.join("bin/zed")).unwrap(),
        "my own zed"
    );
    assert_eq!(
        fs::read_link(f.home().join(".local/bin/zed")).unwrap(),
        app.join("bin/zed")
    );

    let remover = crate::undo::Remover {
        links: Arc::new(dot_platform::link::SystemLinks::new()),
        packages: dot_platform::pkg::Packages::default(),
        system: b.system.clone(),
        store: dot_core::StateStore::at(&f.context().state_dir),
        home: f.home().to_path_buf(),
        uninstall: false,
    };
    // An edit inside the tree: left alone.
    let editor = app.join("libexec/zed-editor");
    let when = fs::metadata(&editor).unwrap().modified().unwrap();
    fs::write(&editor, "patched").unwrap();
    let removals = remover.plan(&f.bundle.name);
    let tree = removals
        .iter()
        .find(|r| r.key == "tree:~/.local/zed.app")
        .unwrap();
    let v = tree.check().unwrap();
    assert!(
        matches!(&v, crate::undo::Verdict::Conflict(m) if m.contains("changed since dot unpacked it")),
        "{v:?}"
    );
    // As unpacked (content and time): removed, and the tree that was there is put back.
    fs::write(&editor, "editor").unwrap();
    fs::File::options()
        .write(true)
        .open(&editor)
        .unwrap()
        .set_modified(when)
        .unwrap();
    for r in remover.plan(&f.bundle.name) {
        let v = r.check().unwrap();
        assert!(
            matches!(v, crate::undo::Verdict::Undo(_)),
            "{}: {v:?}",
            r.key
        );
        r.apply(&mut |_, _| {}).unwrap();
    }
    assert!(!f.home().join(".local/bin/zed").exists());
    assert_eq!(
        fs::read_to_string(app.join("bin/zed")).unwrap(),
        "my own zed"
    );
    assert!(!backup.exists());
}

#[test]
fn a_release_asset_is_picked_and_checked_against_the_release() {
    // GitHub: the asset's own digest pins it.
    let f = Fixture::new(
        "steps:\n  - download:\n      release: { repo: zed-industries/zed, asset: 'zed-linux-x86_64.tar.gz' }\n      dest: ~/.local/zed.app\n      extract: auto\n      strip: 1\n      replace: true\n      bin: [bin/zed]\n",
    );
    let (mut b, http, _) = setup(&f);
    let tarball = tar_gz(&[("zed.app/bin/zed", b"#!zed")]);
    let asset = "https://dl.test/v0.200.0/zed-linux-x86_64.tar.gz";
    let publish = |digest: &str| {
        http.serve(
            &format!("{API}/repos/zed-industries/zed/releases/latest"),
            serde_json::json!({
                "tag_name": "v0.200.0",
                "assets": [
                    { "name": "zed-linux-x86_64.tar.gz", "browser_download_url": asset,
                      "size": tarball.len(), "digest": format!("sha256:{digest}") },
                    { "name": "zed-macos-aarch64.dmg", "browser_download_url": "https://dl.test/x" }
                ]
            })
            .to_string(),
            "\"r\"",
        );
    };
    http.serve(asset, tarball.clone(), "\"z\"");
    publish(&sha(b"something else"));
    let actions = plan(&f, &b, 0);
    let d = actions[0].describe();
    assert!(
        d.contains(
            "(release zed-industries/zed v0.200.0 (zed-linux-x86_64.tar.gz); not downloaded yet)"
        ),
        "{d}"
    );
    assert!(!d.contains("not pinned"), "{d}");
    let err = f.apply_err(actions[0].as_ref());
    assert!(
        err.contains("zed-linux-x86_64.tar.gz: sha256 mismatch: expected"),
        "{err}"
    );
    assert!(!f.home().join(".local/zed.app").exists());
    // The right digest (a new run: nothing cached).
    b.github = Github::new(http.clone(), API);
    publish(&sha(&tarball));
    let actions = plan(&f, &b, 0);
    f.apply_one(actions[0].as_ref()).unwrap();
    assert!(f.home().join(".local/bin/zed").is_symlink());
    assert_eq!(plan(&f, &b, 0)[0].status().unwrap(), Status::Satisfied);
}

#[test]
fn a_package_from_a_gitlab_release() {
    // glab on Ubuntu: GitLab releases only, the version in every asset name.
    let f = Fixture::new(
        "steps:\n  - download:\n      release: { repo: gitlab-org/cli, source: gitlab, asset: 'glab_*_linux_amd64.deb' }\n      package: glab\n",
    );
    let (b, http, fake) = setup(&f);
    let dl = "https://gitlab.com/gitlab-org/cli/-/releases/v1.116.0/downloads";
    let deb = format!("{dl}/glab_1.116.0_linux_amd64.deb");
    http.serve(&deb, "deb-bytes", "\"d\"");
    http.serve(
        &format!("{dl}/checksums.txt"),
        format!("{}  glab_1.116.0_linux_amd64.deb\n", sha(b"deb-bytes")),
        "\"c\"",
    );
    http.serve(
        &format!("{GITLAB_API}/projects/gitlab-org%2Fcli/releases/permalink/latest"),
        serde_json::json!({
            "tag_name": "v1.116.0",
            "assets": { "links": [
                { "name": "glab_1.116.0_linux_amd64.deb", "url": "https://x/y", "direct_asset_url": deb },
                { "name": "glab_1.116.0_linux_arm64.deb", "url": format!("{dl}/arm64.deb") },
                { "name": "checksums.txt", "url": format!("{dl}/checksums.txt") }
            ] }
        })
        .to_string(),
        "\"r\"",
    );
    let mut ctx = f.context();
    ctx.facts.pkg = Some("apt".into());
    fake.on("dpkg-query -W", Output::exit(1, ""));
    b.downloads.checked.lock().unwrap().clear();
    let actions = f.plan_by(0, &ctx, &b).unwrap();
    let d = actions[0].describe();
    assert!(
        d.starts_with(&format!(
            "download {deb} and install it as package glab (apt) (release gitlab.com/gitlab-org/cli v1.116.0 (glab_1.116.0_linux_amd64.deb); not installed"
        )),
        "{d}"
    );
    f.apply_one(actions[0].as_ref()).unwrap();
    assert!(
        fake.calls()
            .iter()
            .any(|c| c.contains("apt-get install -y /")),
        "{:?}",
        fake.calls()
    );
    assert!(
        f.recorder
            .events()
            .iter()
            .any(|e| matches!(e, dot_core::Event::Output { line, .. } if line.contains("sha256 verified (checksums.txt)"))),
    );
}

#[test]
fn a_root_tree_is_swapped_in_through_sudo() {
    let url = "https://dl.test/go.tar.gz";
    let f = Fixture::new(&format!(
        "steps:\n  - download: {{ url: {url}, dest: ~/usr/local/go, extract: auto, strip: 1, replace: true, root: true }}\n"
    ));
    let (b, http, fake) = setup(&f);
    http.serve(url, tar_gz(&[("go/bin/go", b"#!go")]), "\"g\"");
    let dest = f.home().join("usr/local/go");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("old"), "from an earlier go").unwrap();
    // An earlier `dot` install: replaced, not kept.
    StateFile::in_dir(&f.home().join(".state"))
        .record(
            &dest.display().to_string(),
            Record {
                url: "https://dl.test/go-old.tar.gz".into(),
                sha256: "0".repeat(64),
                etag: None,
                last_modified: None,
                tree: None,
            },
        )
        .unwrap();
    let actions = plan(&f, &b, 0);
    assert!(actions[0].needs_sudo());
    fake.clear_calls();
    f.apply_one(actions[0].as_ref()).unwrap();
    let calls = fake.calls();
    let d = dest.display().to_string();
    let new = f
        .home()
        .join(format!("usr/local/.go.dot-new-{}", std::process::id()));
    let old = f
        .home()
        .join(format!("usr/local/.go.dot-old-{}", std::process::id()));
    let want = [
        format!("sudo -n rm -rf -- {}", new.display()),
        "sudo -n cp -R ".to_string(),
        format!("sudo -n chown -R root:root {}", new.display()),
        format!("sudo -n mv {d} {}", old.display()),
        format!("sudo -n mv {} {d}", new.display()),
        format!("sudo -n rm -rf -- {}", old.display()),
    ];
    assert_eq!(calls.len(), want.len(), "{calls:?}");
    for (c, w) in calls.iter().zip(&want) {
        assert!(c.starts_with(w.as_str()), "{c} / {w}");
    }
}
