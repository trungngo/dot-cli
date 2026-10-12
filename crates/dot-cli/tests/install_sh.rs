//! `install.sh`, run for real with a PATH holding only the tools it needs and fakes of `curl`,
//! `wget`, `git` and `gh`; no network.
//!
//! - The download fallback: without curl, wget must refuse a redirect from https to plain http
//!   (`--https-only`), and a wget that cannot (busybox) is not used for https.
//! - Public by default: the release comes from the public download URL and a public dotfiles
//!   repository is cloned without any token; a token at hand is not even looked up.
//! - A private dotfiles repository: git gets the token as an extra header in its environment,
//!   only when it cannot read the repository without it.
//! - A private fork's release (`DOT_RELEASE_REPO`): when the public URL fails, the token goes to
//!   the GitHub API only, from a 0600 header file (never on a command line), and the asset
//!   redirect to the storage host is fetched without it.
//! - Without a token, a failure says how to make one.
#![cfg(unix)]

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../install.sh");

/// A token no real service would issue; its base64 form (as git gets it) is checked too.
const TOKEN: &str = "github_pat_FAKE0123456789abcdefTOKEN";

/// A fake wget that logs its arguments and fails (nothing is downloaded); `--help` lists
/// `--https-only` only when `modern`.
fn fake_wget(dir: &Path, modern: bool) {
    let help = if modern {
        "  --https-only    only follow secure HTTPS links"
    } else {
        "BusyBox v1.36 multi-call binary"
    };
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = --help ]; then echo '{help}'; exit 0; fi\necho \"$@\" >> \"$WGET_LOG\"\nexit 8\n"
    );
    write_exe(&dir.join("wget"), &script);
}

/// Held while this test binary writes an executable or starts a process. A process forked by
/// one test while another test still has an executable open for writing inherits that
/// descriptor until it execs; running the executable meanwhile fails with ETXTBSY ("Text file
/// busy").
static EXEC: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn exec_lock() -> std::sync::MutexGuard<'static, ()> {
    EXEC.lock().unwrap_or_else(|e| e.into_inner())
}

fn write_exe(p: &Path, script: &str) {
    let _exec = exec_lock();
    fs::write(p, script).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Links the real `tools` into `bin`; a missing optional one (`?name`) is skipped.
fn link_tools(bin: &Path, tools: &[&str]) {
    for tool in tools {
        let (tool, optional) = match tool.strip_prefix('?') {
            Some(t) => (t, true),
            None => (*tool, false),
        };
        let found = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
            .iter()
            .map(|d| Path::new(d).join(tool))
            .find(|p| p.exists());
        match found {
            Some(p) => std::os::unix::fs::symlink(p, bin.join(tool)).unwrap(),
            None if optional => {}
            None => panic!("{tool} not found"),
        }
    }
}

fn run_wget(modern: bool) -> (String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    // Only what the script needs up to the download: no curl.
    link_tools(&bin, &["uname", "mktemp", "rm", "grep", "cat"]);
    fake_wget(&bin, modern);
    let log = tmp.path().join("wget.log");
    let _exec = exec_lock();
    let out = Command::new("/bin/sh")
        .arg(SCRIPT)
        .env_clear()
        .env("PATH", &bin)
        .env("HOME", tmp.path().join("home"))
        .env("WGET_LOG", &log)
        .output()
        .unwrap();
    assert!(!out.status.success(), "the fake wget downloads nothing");
    (
        fs::read_to_string(&log).unwrap_or_default(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn wget_is_told_to_stay_on_https() {
    let (log, stderr) = run_wget(true);
    assert!(log.contains("--https-only"), "{log}\n{stderr}");
    assert!(
        log.contains("https://github.com/trungngo/dot-cli/releases/latest/download/dot-"),
        "{log}"
    );
    assert!(stderr.contains("cannot download"), "{stderr}");
}

#[test]
fn a_wget_without_https_only_is_not_used_for_https() {
    let (log, stderr) = run_wget(false);
    assert!(log.is_empty(), "{log}");
    assert!(stderr.contains("install curl"), "{stderr}");
}

/// The fork whose releases are private, as `DOT_RELEASE_REPO` names it.
const FORK: &str = "someone/dot-fork";

/// A fake curl serving the release of `$DOT_RELEASE_REPO` (default trungngo/dot-cli)
/// through the public download URLs and, for a token, through the API (asset 11 redirects to a
/// storage host, asset 12 is answered directly). `$FAKE/private`: the public URLs answer 404;
/// `$FAKE/no-release`: the repository has no release (404 for the latest, 200 for the
/// repository); `$FAKE/no-access`: the token cannot read it (404 for both). It logs its argv,
/// each request as `auth|none <url>`, the headers it was given, and whether each header file
/// was 0600.
const FAKE_CURL: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE/log/curl.argv"
out= w= url= hdrs= fail=
while [ $# -gt 0 ]; do
	case "$1" in
	-o) out=$2; shift ;;
	-w) w=$2; shift ;;
	-H)
		case "$2" in
		@-) hdrs="$hdrs$(cat)
" ;;
		@*)
			f=${2#@}
			if [ -n "$(find "$f" -perm 0600)" ]; then echo "0600 $f" >> "$FAKE/log/curl.hfiles"
			else echo "BAD-MODE $f" >> "$FAKE/log/curl.hfiles"; fi
			hdrs="$hdrs$(cat "$f")
" ;;
		*) hdrs="$hdrs$2
" ;;
		esac
		shift ;;
	--proto | --retry) shift ;;
	-fsSL) fail=1 ;;
	-*) ;;
	*) url=$1 ;;
	esac
	shift
done
case "$hdrs" in *Authorization:*) auth=auth ;; *) auth=none ;; esac
printf '%s %s\n' "$auth" "$url" >> "$FAKE/log/curl.requests"
printf '%s' "$hdrs" >> "$FAKE/log/curl.headers"
repo=${DOT_RELEASE_REPO:-trungngo/dot-cli}
code=200 loc= src=
api=https://api.github.com/repos/$repo/releases
pub=https://github.com/$repo/releases/latest/download
[ -z "${DOT_VERSION:-}" ] || pub=https://github.com/$repo/releases/download/$DOT_VERSION
case "$url" in
"$api/latest" | "$api/tags/${DOT_VERSION:-none}")
	if [ -f "$FAKE/no-release" ] || [ -f "$FAKE/no-access" ]; then code=404
	else src=$FAKE/release.json; fi ;;
"https://api.github.com/repos/$repo") [ ! -f "$FAKE/no-access" ] || code=404 ;;
"$api/assets/11") code=302 loc='https://objects.example.com/signed/asset?sig=abc' ;;
"$api/assets/12") src=$FAKE/SHA256SUMS ;;
https://objects.example.com/signed/asset*) src=$FAKE/asset ;;
"$pub/SHA256SUMS") src=$FAKE/SHA256SUMS ;;
"$pub"/dot-*) src=$FAKE/asset ;;
*) code=404 ;;
esac
case "$url" in "$pub"/*) [ ! -f "$FAKE/private" ] || code=404 ;; esac
if [ -n "$fail" ] && [ "$code" != 200 ]; then
	echo "curl: (22) The requested URL returned error: $code" >&2
	exit 22
fi
if [ -n "$src" ]; then cp "$src" "$out"; else : > "$out"; fi
[ -z "$w" ] || printf '%s %s' "$code" "$loc"
exit 0
"#;

/// A fake git: `version` says `$FAKE_GIT_VERSION` (default 2.45.1); `ls-remote` (the probe)
/// and `clone` fail for a private repository (`$FAKE/private-dotfiles`) unless given the extra
/// header; `clone` records the extra-header environment it was given and makes the checkout;
/// `-C <dir> remote get-url origin` prints `<dir>/.git/origin-url`.
const FAKE_GIT: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE/log/git.argv"
denied() {
	[ -f "$FAKE/private-dotfiles" ] && [ -z "${GIT_CONFIG_VALUE_0:-}" ]
}
case "$1" in
version) echo "git version ${FAKE_GIT_VERSION:-2.45.1}" ;;
-C) [ "$3 $4" != "remote get-url" ] || cat "$2/.git/origin-url" 2>/dev/null ;;
ls-remote)
	printf 'prompt=%s gcm=%s\n' "${GIT_TERMINAL_PROMPT:-}" "${GCM_INTERACTIVE:-}" >> "$FAKE/log/git.probe"
	if denied; then echo "fatal: could not read Username: terminal prompts disabled" >&2; exit 128; fi ;;
clone)
	printf 'count=%s key=%s value=%s prompt=%s\n' "${GIT_CONFIG_COUNT:-}" \
		"${GIT_CONFIG_KEY_0:-}" "${GIT_CONFIG_VALUE_0:-}" "${GIT_TERMINAL_PROMPT:-}" \
		>> "$FAKE/log/git.env"
	if denied; then echo "fatal: Authentication failed" >&2; exit 128; fi
	eval "dest=\${$#}"
	mkdir -p "$dest/.git" ;;
esac
exit 0
"#;

/// A fake GitHub CLI: `auth token` prints `$FAKE_GH_TOKEN`, or fails as when logged out.
const FAKE_GH: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE/log/gh.argv"
if [ "$1 $2" = "auth token" ] && [ -n "${FAKE_GH_TOKEN:-}" ]; then
	echo "$FAKE_GH_TOKEN"
	exit 0
fi
echo "no oauth token found for github.com" >&2
exit 1
"#;

/// The `dot` in the release: `--version`, and everything else logged, with the tokens in its
/// environment.
const FAKE_DOT: &str = "#!/bin/sh\n[ \"$1\" = --version ] && { echo 'dot 9.9.9'; exit 0; }\nprintf '%s\\n' \"$*\" >> \"$FAKE/log/dot.argv\"\nprintf 'GITHUB_TOKEN=%s GH_TOKEN=%s\\n' \"${GITHUB_TOKEN-unset}\" \"${GH_TOKEN-unset}\" >> \"$FAKE/log/dot.env\"\n";

fn this_asset() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => "dot-universal-apple-darwin.tar.gz",
        (_, "aarch64") => "dot-aarch64-unknown-linux-musl.tar.gz",
        _ => "dot-x86_64-unknown-linux-musl.tar.gz",
    }
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

/// A home, a PATH of real tools plus the fakes, a private TMPDIR, and the fake release.
struct Sandbox {
    _tmp: tempfile::TempDir,
    fake: PathBuf,
    bin: PathBuf,
    home: PathBuf,
    tmpdir: PathBuf,
}

impl Sandbox {
    fn new(with_gh: bool) -> Sandbox {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (fake, bin, home, tmpdir) = (
            root.join("fake"),
            root.join("bin"),
            root.join("home"),
            root.join("tmp"),
        );
        for d in [&fake, &fake.join("log"), &bin, &home, &tmpdir] {
            fs::create_dir_all(d).unwrap();
        }
        link_tools(
            &bin,
            &[
                "uname",
                "mktemp",
                "rm",
                "grep",
                "cat",
                "awk",
                "sed",
                "tr",
                "cut",
                "tar",
                "find",
                "head",
                "tail",
                "mkdir",
                "cp",
                "chmod",
                "mv",
                "base64",
                "?gzip",
                "?sha256sum",
                "?shasum",
                "?openssl",
            ],
        );
        write_exe(&bin.join("curl"), FAKE_CURL);
        write_exe(&bin.join("git"), FAKE_GIT);
        if with_gh {
            write_exe(&bin.join("gh"), FAKE_GH);
        }
        let asset = this_asset();
        let archive = tar_gz("dot", FAKE_DOT.as_bytes());
        let sum: String = Sha256::digest(&archive)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        fs::write(fake.join("asset"), &archive).unwrap();
        fs::write(
            fake.join("SHA256SUMS"),
            format!("{}  dot-other.tar.gz\n{sum}  {asset}\n", "0".repeat(64)),
        )
        .unwrap();
        // As the API answers: pretty-printed, the release first, each asset's API url before
        // its name, and an uploader with its own url and id.
        let asset_json = |id: u32, name: &str| {
            format!(
                r#"{{
      "url": "https://api.github.com/repos/someone/dot-fork/releases/assets/{id}",
      "id": {id},
      "name": "{name}",
      "uploader": {{ "url": "https://api.github.com/users/someone", "id": 7 }},
      "browser_download_url": "https://github.com/someone/dot-fork/releases/download/v9.9.9/{name}"
    }}"#
            )
        };
        let release = format!(
            r#"{{
  "url": "https://api.github.com/repos/someone/dot-fork/releases/99",
  "assets_url": "https://api.github.com/repos/someone/dot-fork/releases/99/assets",
  "id": 99,
  "tag_name": "v9.9.9",
  "body": "Notes, with \"quotes\", commas and \"url\": \"x/releases/assets/5\"",
  "assets": [
    {},
    {},
    {}
  ]
}}"#,
            asset_json(10, "dot-other.tar.gz"),
            asset_json(11, asset),
            asset_json(12, "SHA256SUMS"),
        );
        fs::write(fake.join("release.json"), release).unwrap();
        Sandbox {
            _tmp: tmp,
            fake,
            bin,
            home,
            tmpdir,
        }
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.run_sh(&[], args, env)
    }

    /// Runs the script with `sh_flags` for sh itself (e.g. `-x`).
    fn run_sh(&self, sh_flags: &[&str], args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new("/bin/sh");
        c.args(sh_flags)
            .arg(SCRIPT)
            .args(args)
            .env_clear()
            .env("PATH", &self.bin)
            .env("HOME", &self.home)
            .env("TMPDIR", &self.tmpdir)
            .env("FAKE", &self.fake);
        for (k, v) in env {
            c.env(k, v);
        }
        let _exec = exec_lock();
        c.output().unwrap()
    }

    fn log(&self, name: &str) -> String {
        fs::read_to_string(self.fake.join("log").join(name)).unwrap_or_default()
    }

    /// Every command line any fake saw.
    fn argv(&self) -> String {
        ["curl.argv", "git.argv", "gh.argv", "dot.argv"]
            .iter()
            .map(|f| self.log(f))
            .collect()
    }

    fn assert_temp_removed(&self) {
        let left: Vec<_> = fs::read_dir(&self.tmpdir).unwrap().collect();
        assert!(left.is_empty(), "temp files left: {left:?}");
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn base64(s: &str) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = s.as_bytes();
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

const DOTFILES: &str = "https://github.com/someone/dotfiles.git";

const PUB: &str = "https://github.com/trungngo/dot-cli/releases/latest/download";

/// The token is on no command line and in no message; no curl reads ~/.curlrc (`-q` must come
/// first), where `location-trusted` would send the header to the storage host and `verbose`
/// would print it.
fn assert_token_hidden(s: &Sandbox, err: &str) {
    let b64 = base64(&format!("x-access-token:{TOKEN}"));
    let argv = s.argv();
    assert!(!argv.contains(TOKEN) && !argv.contains(&b64), "{argv}");
    assert!(!err.contains(TOKEN) && !err.contains(&b64), "{err}");
    for line in s.log("curl.argv").lines() {
        assert!(line.starts_with("-q "), "{line}");
    }
}

/// The dotfiles were cloned into ~/.dot, recorded, and applied without the token in
/// `dot apply`'s environment.
fn assert_cloned_and_applied(s: &Sandbox) {
    let home = s.home.display();
    assert!(
        s.log("git.argv")
            .contains(&format!("clone -- {DOTFILES} {home}/.dot")),
        "{}",
        s.log("git.argv")
    );
    assert!(s.home.join(".local/bin/dot").exists());
    assert_eq!(s.log("dot.argv"), format!("apply --repo {home}/.dot\n"));
    // The token was the installer's: `dot apply` (and what it runs) does not inherit it.
    assert_eq!(s.log("dot.env"), "GITHUB_TOKEN=unset GH_TOKEN=unset\n");
    let local_env = fs::read_to_string(s.home.join(".config/dot/local.env")).unwrap();
    assert!(!local_env.contains(TOKEN), "{local_env}");
    s.assert_temp_removed();
}

#[test]
fn a_public_install_needs_no_token_and_looks_for_none() {
    // A token at hand (gh logged in, and in the environment): neither is used.
    for env in [
        vec![("FAKE_GH_TOKEN", TOKEN)],
        vec![("GITHUB_TOKEN", TOKEN)],
    ] {
        let s = Sandbox::new(true);
        let out = s.run(&[DOTFILES], &env);
        let err = stderr(&out);
        assert!(out.status.success(), "{err}");
        assert_eq!(
            s.log("curl.requests"),
            format!("none {PUB}/{}\nnone {PUB}/SHA256SUMS\n", this_asset())
        );
        assert!(!s.log("curl.headers").contains("Authorization"));
        assert!(s.log("gh.argv").is_empty(), "gh was asked for a token");
        assert!(!err.contains("using the GitHub token"), "{err}");
        // git was asked whether it can read the repository, without prompting, and cloned
        // with its own credentials.
        assert_eq!(s.log("git.probe"), "prompt=0 gcm=never\n");
        assert_eq!(s.log("git.env"), "count= key= value= prompt=\n");
        assert_token_hidden(&s, &err);
        assert_cloned_and_applied(&s);
    }
}

/// A private dotfiles repository with a token from `env`: the release is public (no token),
/// and git gets the token in its environment only.
fn private_dotfiles(s: &Sandbox, env: &[(&str, &str)], from: &str) {
    fs::write(s.fake.join("private-dotfiles"), "").unwrap();
    let out = s.run(&[DOTFILES], env);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}\n{}", s.log("git.argv"));
    assert!(
        err.contains(&format!(
            "{DOTFILES} needs credentials: using the GitHub token from {from}"
        )),
        "{err}"
    );
    assert!(
        !s.log("curl.requests").contains("auth "),
        "{}",
        s.log("curl.requests")
    );
    let b64 = base64(&format!("x-access-token:{TOKEN}"));
    assert_eq!(
        s.log("git.env"),
        format!(
            "count=1 key=http.https://github.com/.extraheader value=Authorization: Basic {b64} \
             prompt=0\n"
        )
    );
    assert_token_hidden(s, &err);
    assert_cloned_and_applied(s);
}

#[test]
fn a_private_dotfiles_repository_gets_the_token_in_gits_environment_only() {
    let s = Sandbox::new(true);
    private_dotfiles(&s, &[("GITHUB_TOKEN", TOKEN)], "GITHUB_TOKEN");
    assert!(
        s.log("gh.argv").is_empty(),
        "gh is not asked when a token is set"
    );

    let s = Sandbox::new(false);
    private_dotfiles(&s, &[("GH_TOKEN", TOKEN)], "GH_TOKEN");

    let s = Sandbox::new(true);
    private_dotfiles(&s, &[("FAKE_GH_TOKEN", TOKEN)], "gh auth token");
    assert_eq!(s.log("gh.argv"), "auth token --hostname github.com\n");
}

#[test]
fn without_a_token_a_private_dotfiles_repository_explains_how_to_make_one() {
    let s = Sandbox::new(false);
    fs::write(s.fake.join("private-dotfiles"), "").unwrap();
    let out = s.run(&[DOTFILES], &[]);
    let err = stderr(&out);
    assert!(!out.status.success(), "{err}");
    for want in [
        &format!("If {DOTFILES} is private, this script needs a GitHub token"),
        "https://github.com/settings/personal-access-tokens/new",
        "Contents: Read-only",
        "read -rs GITHUB_TOKEN",
        "curl -fsSL https://raw.githubusercontent.com/trungngo/dot-cli/main/install.sh",
        &format!("| GITHUB_TOKEN=$GITHUB_TOKEN sh -s -- {DOTFILES}"),
        "gh auth login",
        &format!("git clone {DOTFILES} failed"),
    ] {
        assert!(err.contains(want), "missing {want:?} in:\n{err}");
    }
    assert!(!s.home.join(".dot").exists());
    s.assert_temp_removed();
}

/// A private fork's release with a token from `env`: the public URL fails, the API gets the
/// token, nothing else does, and nothing is left behind.
fn private_fork_install(s: &Sandbox, env: &[(&str, &str)], from: &str) {
    fs::write(s.fake.join("private"), "").unwrap();
    let mut env = env.to_vec();
    env.push(("DOT_RELEASE_REPO", FORK));
    let out = s.run(&[DOTFILES], &env);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}\n{}", s.log("curl.requests"));
    assert!(
        err.contains(&format!(
            "using the GitHub token from {from} for the release of {FORK}"
        )),
        "{err}"
    );
    assert!(err.contains("release v9.9.9"), "{err}");
    assert_token_hidden(s, &err);

    // Only API requests carry it; the public URL and the storage host get none.
    let api = format!("https://api.github.com/repos/{FORK}/releases");
    assert_eq!(
        s.log("curl.requests"),
        format!(
            "none https://github.com/{FORK}/releases/latest/download/{}\n\
             auth {api}/latest\n\
             auth {api}/assets/11\n\
             none https://objects.example.com/signed/asset?sig=abc\n\
             auth {api}/assets/12\n",
            this_asset()
        )
    );
    assert!(
        s.log("curl.headers")
            .contains(&format!("Authorization: Bearer {TOKEN}\n")),
        "the API got the right token"
    );
    assert!(
        s.log("curl.headers")
            .contains("Accept: application/octet-stream")
    );
    // From a 0600 file, deleted afterwards (with the rest of the temp directory).
    let hfiles = s.log("curl.hfiles");
    assert_eq!(hfiles.lines().count(), 3, "{hfiles}");
    for line in hfiles.lines() {
        let path = line
            .strip_prefix("0600 ")
            .unwrap_or_else(|| panic!("{hfiles}"));
        assert!(path.starts_with(s.tmpdir.to_str().unwrap()), "{path}");
        assert!(!Path::new(path).exists(), "{path} left behind");
    }
    // The dotfiles are public: git gets no token.
    assert_eq!(s.log("git.env"), "count= key= value= prompt=\n");
    assert_cloned_and_applied(s);
}

#[test]
fn a_private_forks_release_comes_through_the_api_with_the_token() {
    let s = Sandbox::new(true);
    private_fork_install(&s, &[("GITHUB_TOKEN", TOKEN)], "GITHUB_TOKEN");
    assert!(s.log("gh.argv").is_empty());

    let s = Sandbox::new(false);
    private_fork_install(&s, &[("GH_TOKEN", TOKEN)], "GH_TOKEN");

    let s = Sandbox::new(true);
    private_fork_install(&s, &[("FAKE_GH_TOKEN", TOKEN)], "gh auth token");
    assert_eq!(s.log("gh.argv"), "auth token --hostname github.com\n");
}

#[test]
fn without_a_token_a_private_forks_release_explains_how_to_make_one() {
    let s = Sandbox::new(false); // no gh at all
    fs::write(s.fake.join("private"), "").unwrap();
    let out = s.run(&[DOTFILES], &[("DOT_RELEASE_REPO", FORK)]);
    let err = stderr(&out);
    assert!(!out.status.success(), "{err}");
    for want in [
        &format!("cannot download https://github.com/{FORK}/releases/latest/download/dot-"),
        &format!("If {FORK} is private, this script needs a GitHub token"),
        "https://github.com/settings/personal-access-tokens/new",
        "Contents: Read-only",
        "read -rs GITHUB_TOKEN",
        "printf 'Authorization: Bearer %s\\n' \"$GITHUB_TOKEN\"",
        &format!("curl -fsSL -H @- https://raw.githubusercontent.com/{FORK}/main/install.sh"),
        &format!("| DOT_RELEASE_REPO={FORK} GITHUB_TOKEN=$GITHUB_TOKEN sh -s -- {DOTFILES}"),
        "gh auth login",
    ] {
        assert!(err.contains(want), "missing {want:?} in:\n{err}");
    }
    assert!(
        !s.log("curl.requests").contains("auth "),
        "no token was sent"
    );
    assert!(s.log("git.argv").is_empty(), "nothing cloned");
    s.assert_temp_removed();
}

#[test]
fn the_token_goes_to_github_com_only() {
    // Another host: git uses its own credentials, even with a token at hand, and is not probed.
    let s = Sandbox::new(false);
    let url = "https://gitlab.com/someone/dotfiles.git";
    let out = s.run(&[url], &[("GITHUB_TOKEN", TOKEN)]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(s.log("git.env"), "count= key= value= prompt=\n");
    assert!(s.log("git.probe").is_empty());

    // A git too old for GIT_CONFIG_COUNT would ignore it: refused, with what to do.
    let s = Sandbox::new(false);
    fs::write(s.fake.join("private-dotfiles"), "").unwrap();
    let out = s.run(
        &[DOTFILES],
        &[("GITHUB_TOKEN", TOKEN), ("FAKE_GIT_VERSION", "2.25.1")],
    );
    let err = stderr(&out);
    assert!(!out.status.success());
    assert!(err.contains("git 2.25 cannot take the token"), "{err}");
    assert!(s.log("git.env").is_empty());
    s.assert_temp_removed();

    // A token that would break the header it goes into, when it is needed.
    let s = Sandbox::new(false);
    fs::write(s.fake.join("private-dotfiles"), "").unwrap();
    let out = s.run(&[DOTFILES], &[("GITHUB_TOKEN", "abc\ndef")]);
    assert!(
        stderr(&out).contains("unexpected characters"),
        "{}",
        stderr(&out)
    );
    assert!(s.log("git.env").is_empty(), "nothing cloned");
}

#[test]
fn the_release_repository_must_be_owner_and_name() {
    for bad in ["someone", "a/b/c", "/abs", "a/", "a/..", "a b/c", "a/b;c"] {
        let s = Sandbox::new(false);
        let out = s.run(&[], &[("DOT_RELEASE_REPO", bad)]);
        let err = stderr(&out);
        assert!(!out.status.success(), "{bad}: {err}");
        assert!(
            err.contains("DOT_RELEASE_REPO must be owner/name"),
            "{bad}: {err}"
        );
        assert!(s.log("curl.argv").is_empty(), "{bad}");
    }
}

#[test]
fn a_repository_without_a_release_is_not_blamed_on_the_token() {
    // The token reads the fork, which has no release yet.
    let s = Sandbox::new(false);
    fs::write(s.fake.join("private"), "").unwrap();
    fs::write(s.fake.join("no-release"), "").unwrap();
    let env = [("GITHUB_TOKEN", TOKEN), ("DOT_RELEASE_REPO", FORK)];
    let out = s.run(&[DOTFILES], &env);
    let err = stderr(&out);
    assert!(!out.status.success(), "{err}");
    assert!(
        err.contains(&format!("{FORK} has no published release yet")),
        "{err}"
    );
    assert!(!err.contains("cannot read"), "{err}");
    assert_eq!(
        s.log("curl.requests"),
        format!(
            "none https://github.com/{FORK}/releases/latest/download/{}\n\
             auth https://api.github.com/repos/{FORK}/releases/latest\n\
             auth https://api.github.com/repos/{FORK}\n",
            this_asset()
        )
    );
    s.assert_temp_removed();

    // The token cannot read it: GitHub says 404 to both; the token is what to fix.
    let s = Sandbox::new(false);
    fs::write(s.fake.join("private"), "").unwrap();
    fs::write(s.fake.join("no-access"), "").unwrap();
    let out = s.run(&[DOTFILES], &env);
    let err = stderr(&out);
    assert!(!out.status.success(), "{err}");
    assert!(
        err.contains(&format!(
            "HTTP 404: the token from GITHUB_TOKEN cannot read {FORK}"
        )),
        "{err}"
    );
    s.assert_temp_removed();
}

#[test]
fn tracing_is_turned_off_so_the_token_is_not_printed() {
    let s = Sandbox::new(false);
    fs::write(s.fake.join("private-dotfiles"), "").unwrap();
    let out = s.run_sh(&["-x"], &[DOTFILES], &[("GITHUB_TOKEN", TOKEN)]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("tracing (sh -x) turned off"), "{err}");
    let b64 = base64(&format!("x-access-token:{TOKEN}"));
    assert!(!err.contains(TOKEN) && !err.contains(&b64), "{err}");
    assert!(s.home.join(".local/bin/dot").exists());

    // Also when the token is GH_TOKEN and goes to the API for a private fork.
    let s = Sandbox::new(false);
    fs::write(s.fake.join("private"), "").unwrap();
    let out = s.run_sh(
        &["-x"],
        &[],
        &[("GH_TOKEN", TOKEN), ("DOT_RELEASE_REPO", FORK)],
    );
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(!err.contains(TOKEN), "{err}");
}

#[test]
fn dot_version_pins_the_release() {
    let s = Sandbox::new(false);
    let out = s.run(&[], &[("DOT_VERSION", "v1.2.3")]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    let base = "https://github.com/trungngo/dot-cli/releases/download/v1.2.3";
    assert_eq!(
        s.log("curl.requests"),
        format!("none {base}/{}\nnone {base}/SHA256SUMS\n", this_asset())
    );

    // Through the API (a private fork), the release of that tag.
    let s = Sandbox::new(false);
    fs::write(s.fake.join("private"), "").unwrap();
    let env = [
        ("GITHUB_TOKEN", TOKEN),
        ("DOT_RELEASE_REPO", FORK),
        ("DOT_VERSION", "v1.2.3"),
    ];
    let out = s.run(&[], &env);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        s.log("curl.requests").contains(&format!(
            "auth https://api.github.com/repos/{FORK}/releases/tags/v1.2.3\n"
        )),
        "{}",
        s.log("curl.requests")
    );

    for bad in ["1.2.3", "latest", "v1/../x", "v1 2", "v1;x"] {
        let s = Sandbox::new(false);
        let out = s.run(&[], &[("DOT_VERSION", bad)]);
        let err = stderr(&out);
        assert!(!out.status.success(), "{bad}: {err}");
        assert!(
            err.contains("DOT_VERSION must be a release tag"),
            "{bad}: {err}"
        );
        assert!(s.log("curl.argv").is_empty(), "{bad}");
    }
}

#[test]
fn an_ssh_url_is_cloned_with_ssh_and_never_gets_the_token() {
    for url in [
        "git@github.com:someone/dotfiles.git",
        "ssh://git@github.com/someone/dotfiles.git",
    ] {
        let s = Sandbox::new(true);
        // Even for a private repository, with a token at hand: ssh keys, not the token.
        fs::write(s.fake.join("private-dotfiles"), "").unwrap();
        let out = s.run(&[url], &[("GITHUB_TOKEN", TOKEN), ("FAKE_GH_TOKEN", TOKEN)]);
        let err = stderr(&out);
        assert!(
            !out.status.success(),
            "the fake denies it without a header: {err}"
        );
        assert!(s.log("git.probe").is_empty(), "{url}: probed");
        assert!(s.log("gh.argv").is_empty(), "{url}: gh asked for a token");
        assert_eq!(s.log("git.env"), "count= key= value= prompt=\n", "{url}");
        assert!(
            s.log("git.argv").contains(&format!("clone -- {url} ")),
            "{url}"
        );
        assert!(!err.contains("using the GitHub token"), "{err}");
        assert!(!err.contains("needs a GitHub token"), "{err}");

        // A public one clones and applies.
        let s = Sandbox::new(false);
        let out = s.run(&[url], &[]);
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(s.home.join(".dot/.git").is_dir());
    }
    for bad in [
        "-oProxyCommand=x",
        "ssh://-oProxyCommand=x/y",
        "ssh://me:pw@host/x",
        "file:///tmp/x",
        "/tmp/x",
        "http://github.com/a/b",
    ] {
        let s = Sandbox::new(false);
        let out = s.run(&[bad], &[]);
        assert!(!out.status.success(), "{bad}");
        assert!(
            s.log("curl.argv").is_empty(),
            "{bad}: checked after downloading"
        );
    }
}

/// The default checkout is ~/.dot, beside an older setup's ~/.dotfiles (dotbot's), which is
/// left exactly as it is; DOT_DOTFILES_DIR still chooses another place.
#[test]
fn the_checkout_goes_to_dot_beside_an_old_dotfiles_or_where_dot_dotfiles_dir_says() {
    let s = Sandbox::new(false);
    let old = s.home.join(".dotfiles");
    fs::create_dir_all(old.join(".git")).unwrap();
    fs::write(old.join("install.conf.yaml"), "- link: {}\n").unwrap();
    let out = s.run(&[DOTFILES], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_cloned_and_applied(&s);
    assert!(s.home.join(".dot/.git").is_dir());
    assert_eq!(
        fs::read_to_string(old.join("install.conf.yaml")).unwrap(),
        "- link: {}\n"
    );
    assert_eq!(
        fs::read_dir(&old).unwrap().count(),
        2,
        "~/.dotfiles was touched"
    );
    let home = s.home.display();
    let local_env = fs::read_to_string(s.home.join(".config/dot/local.env")).unwrap();
    assert!(local_env.contains(&format!("{home}/.dot")), "{local_env}");
    assert!(!local_env.contains(".dotfiles"), "{local_env}");

    let s = Sandbox::new(false);
    let elsewhere = s.home.join("src/my-dots");
    let out = s.run(
        &[DOTFILES],
        &[("DOT_DOTFILES_DIR", elsewhere.to_str().unwrap())],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(elsewhere.join(".git").is_dir());
    assert!(!s.home.join(".dot").exists());
    assert_eq!(
        s.log("dot.argv"),
        format!("apply --repo {}\n", elsewhere.display())
    );
}

/// Something else already at ~/.dot (another repository's checkout, a directory, a file) is
/// refused and left as it is: nothing cloned, recorded or applied. The same repository's
/// checkout is updated.
#[test]
fn an_existing_dot_that_is_not_this_checkout_is_refused() {
    let other = "https://github.com/someone/other.git";
    type Make = fn(&Path);
    let cases: [(&str, Make); 3] = [
        (
            "is a checkout of https://github.com/someone/other.git, not",
            |d| {
                fs::create_dir_all(d.join(".git")).unwrap();
                fs::write(
                    d.join(".git/origin-url"),
                    "https://github.com/someone/other.git\n",
                )
                .unwrap();
            },
        ),
        ("exists and is not a git checkout", |d| {
            fs::create_dir_all(d.join("notes")).unwrap();
        }),
        ("exists and is not a git checkout", |d| {
            fs::write(d, "a file\n").unwrap();
        }),
    ];
    for (said, make) in cases {
        let s = Sandbox::new(false);
        let dot = s.home.join(".dot");
        make(&dot);
        let out = s.run(&[DOTFILES], &[]);
        let err = stderr(&out);
        assert!(!out.status.success(), "{said}: {err}");
        assert!(err.contains(said), "{err}");
        assert!(err.contains("set DOT_DOTFILES_DIR"), "{err}");
        assert!(
            !s.log("git.argv").contains("clone"),
            "{}",
            s.log("git.argv")
        );
        assert!(s.log("dot.argv").is_empty());
        assert!(!s.home.join(".config/dot/local.env").exists());
        if dot.is_file() {
            assert_eq!(fs::read_to_string(&dot).unwrap(), "a file\n");
        } else if said.contains("checkout of") {
            assert!(
                fs::read_to_string(dot.join(".git/origin-url"))
                    .unwrap()
                    .contains(other)
            );
        } else {
            assert!(dot.join("notes").is_dir());
        }
    }

    // This repository's own checkout: pulled, not cloned again.
    let s = Sandbox::new(false);
    let dot = s.home.join(".dot");
    fs::create_dir_all(dot.join(".git")).unwrap();
    fs::write(dot.join(".git/origin-url"), format!("{DOTFILES}\n")).unwrap();
    let out = s.run(&[DOTFILES], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let git = s.log("git.argv");
    assert!(
        git.contains(&format!("-C {} pull --ff-only", dot.display())),
        "{git}"
    );
    assert!(!git.contains("clone"), "{git}");
}

#[test]
fn another_recorded_repository_is_kept_with_a_warning() {
    let s = Sandbox::new(false);
    let local_env = s.home.join(".config/dot/local.env");
    fs::create_dir_all(local_env.parent().unwrap()).unwrap();
    fs::write(&local_env, "DOT_REPO='~/old-dotfiles'\n").unwrap();
    let out = s.run(&[DOTFILES], &[]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    let home = s.home.display();
    assert!(
        err.contains(&format!(
            "already has DOT_REPO={home}/old-dotfiles, left as it is"
        )),
        "{err}"
    );
    assert_eq!(
        fs::read_to_string(&local_env).unwrap(),
        "DOT_REPO='~/old-dotfiles'\n"
    );
    assert_eq!(s.log("dot.argv"), format!("apply --repo {home}/.dot\n"));

    // The same repository, however it is spelled: no warning.
    let s = Sandbox::new(false);
    let local_env = s.home.join(".config/dot/local.env");
    fs::create_dir_all(local_env.parent().unwrap()).unwrap();
    fs::write(&local_env, "DOT_REPO=\"~/.dot/\"\n").unwrap();
    let out = s.run(&[DOTFILES], &[]);
    assert!(out.status.success());
    assert!(!stderr(&out).contains("warning"), "{}", stderr(&out));
}

#[test]
fn a_bin_dir_off_path_comes_with_the_line_to_add() {
    let s = Sandbox::new(false);
    let out = s.run(&[], &[]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(
        err.contains(&format!(
            "    export PATH=\"{}/.local/bin:$PATH\"\n",
            s.home.display()
        )),
        "{err}"
    );
}
