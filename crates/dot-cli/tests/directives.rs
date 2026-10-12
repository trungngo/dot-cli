//! The directives end to end, through the built binary: one temp repository whose bundle uses
//! `package` (fake `brew` / `apt-get` on a temp `PATH`), `github_release` (a fixture HTTP server
//! on 127.0.0.1 that rejects anonymous API calls), `git` (a local bare repository over
//! `file://`), `template` with a secret, `copy`, `block`, `secret_file`, `path` / `env`, a
//! `shell` step with `sudo: true`, and `service` (fake `launchctl`, or `systemctl` + `loginctl`
//! with lingering through sudo). Secrets come from an `envfile` in the temp HOME, including the
//! repository-wide `github_token`.
//!
//! Checked: a dry run changes nothing; the real run primes sudo up front through a fake `sudo`,
//! sends the token only to the API, never shows a secret on the console or in the log, and
//! records `path` / `env` for `dot env --shell`; a second run (the shell now sourcing that file,
//! `GITHUB_TOKEN` from the environment) changes nothing and needs no sudo; a third picks up a new
//! commit. Nothing runs a real package manager, service manager or sudo, touches the real home,
//! or leaves the machine (the only socket is the loopback fixture).
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const API_TOKEN: &str = "tok-e2e-4c1d9b-api-secret";
const GH_TOKEN: &str = "ghp_e2e0a7f3c2secret";
const PEM: &str =
    "-----BEGIN KEY-----\nMIIBe2eSecretKeyLineOne\nMIIBe2eSecretKeyLineTwo\n-----END KEY-----\n";

/// Fake `sudo`: `-n true` and `-n -v` succeed (credentials are "cached"); anything else runs
/// the command after `-n`. Every call is logged.
const SUDO: &str = r#"#!/bin/sh
echo "sudo $*" >> "$FAKE_LOG"
[ "$1" = "-n" ] && shift
case "$1" in
  true|-v) exit 0 ;;
esac
exec "$@"
"#;

/// Fake `brew list --formula --versions <names>` / `brew install <names>`: installed formulae
/// are listed in `$FAKE_STATE/brew`.
#[cfg(target_os = "macos")]
const BREW: &str = r#"#!/bin/sh
echo "brew $*" >> "$FAKE_LOG"
case "$1" in
  list)
    shift 3
    rc=0
    for p in "$@"; do
      if grep -qx "$p" "$FAKE_STATE/brew" 2>/dev/null; then echo "$p 1.0"; else rc=1; fi
    done
    exit $rc ;;
  install)
    shift
    for a in "$@"; do case "$a" in --*) ;; *) echo "$a" >> "$FAKE_STATE/brew" ;; esac; done
    exit 0 ;;
esac
exit 0
"#;

/// Fake launchd: `print` succeeds (running) once the job was bootstrapped.
#[cfg(target_os = "macos")]
const LAUNCHCTL: &str = r#"#!/bin/sh
echo "launchctl $*" >> "$FAKE_LOG"
case "$1" in
  print-disabled) exit 0 ;;
  print)
    if [ -e "$FAKE_STATE/launchd-running" ]; then echo "state = running"; exit 0; fi
    exit 113 ;;
  bootstrap|kickstart) touch "$FAKE_STATE/launchd-running" ;;
esac
exit 0
"#;

/// Fake `dpkg-query -W -f FORMAT <names>`: installed packages are in `$FAKE_STATE/apt`.
#[cfg(target_os = "linux")]
const DPKG_QUERY: &str = r#"#!/bin/sh
echo "dpkg-query $*" >> "$FAKE_LOG"
shift 3
rc=0
for p in "$@"; do
  if grep -qx "$p" "$FAKE_STATE/apt" 2>/dev/null; then printf '%s\tii \n' "$p"; else rc=1; fi
done
exit $rc
"#;

/// Fake `apt-get update|install -y ... <names>` (`-o` takes a value).
#[cfg(target_os = "linux")]
const APT_GET: &str = r#"#!/bin/sh
echo "apt-get $*" >> "$FAKE_LOG"
case "$1" in
  install)
    shift
    skip=0
    for a in "$@"; do
      if [ $skip = 1 ]; then skip=0; continue; fi
      case "$a" in -o) skip=1 ;; -*) ;; *) echo "$a" >> "$FAKE_STATE/apt" ;; esac
    done ;;
esac
exit 0
"#;

/// Fake `systemctl --user`: enabled / active once told so.
#[cfg(target_os = "linux")]
const SYSTEMCTL: &str = r#"#!/bin/sh
echo "systemctl $*" >> "$FAKE_LOG"
[ "$1" = "--user" ] && shift
case "$1" in
  is-enabled)
    if [ -e "$FAKE_STATE/enabled" ]; then echo enabled; exit 0; fi
    echo disabled; exit 1 ;;
  is-active)
    if [ -e "$FAKE_STATE/active" ]; then echo active; exit 0; fi
    echo inactive; exit 3 ;;
  enable) touch "$FAKE_STATE/enabled" ;;
  start) touch "$FAKE_STATE/active" ;;
esac
exit 0
"#;

/// Fake `loginctl show-user <u> --property=Linger --value` / `enable-linger <u>`.
#[cfg(target_os = "linux")]
const LOGINCTL: &str = r#"#!/bin/sh
echo "loginctl $*" >> "$FAKE_LOG"
case "$1" in
  show-user) if [ -e "$FAKE_STATE/linger" ]; then echo yes; else echo no; fi ;;
  enable-linger) touch "$FAKE_STATE/linger" ;;
esac
exit 0
"#;

/// A request the fixture server saw: path and `Authorization` header.
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A GitHub API stand-in on 127.0.0.1: `GET /repos/acme/tool/releases/latest` (only with
/// `Bearer GH_TOKEN`; anonymous calls get GitHub's rate-limit answer) and the asset downloads.
struct Hub {
    base: String,
    seen: Seen,
}

impl Hub {
    fn start(archive: Vec<u8>) -> Hub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let digest = hex(&Sha256::digest(&archive));
        let assets: Vec<Value> = [
            "linux-x86_64",
            "linux-aarch64",
            "darwin-x86_64",
            "darwin-arm64",
        ]
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": format!("tool-1.2.3-{p}.tar.gz"),
                "browser_download_url": format!("{base}/dl/tool-1.2.3-{p}.tar.gz"),
                "size": archive.len(),
                "digest": format!("sha256:{digest}"),
            })
        })
        .collect();
        let release = serde_json::json!({ "tag_name": "v1.2.3", "assets": assets }).to_string();
        let seen: Seen = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    continue;
                }
                let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut auth = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':')
                        && k.eq_ignore_ascii_case("authorization")
                    {
                        auth = Some(v.trim().to_string());
                    }
                }
                log.lock().unwrap().push((path.clone(), auth.clone()));
                let authorized = auth.as_deref() == Some(&format!("Bearer {GH_TOKEN}"));
                let (status, extra, body): (&str, &str, Vec<u8>) = match path.as_str() {
                    "/repos/acme/tool/releases/latest" if authorized => {
                        ("200 OK", "", release.clone().into_bytes())
                    }
                    "/repos/acme/tool/releases/latest" => (
                        "403 Forbidden",
                        "x-ratelimit-remaining: 0\r\n",
                        br#"{"message":"API rate limit exceeded"}"#.to_vec(),
                    ),
                    p if p.starts_with("/dl/tool-1.2.3-") => ("200 OK", "", archive.clone()),
                    _ => ("404 Not Found", "", b"{}".to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        Hub { base, seen }
    }

    fn seen(&self) -> Vec<(String, Option<String>)> {
        self.seen.lock().unwrap().clone()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct Env {
    _tmp: TempDir,
    root: PathBuf,
    hub: Hub,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["home", "state", "logs", "bin", "fake", "build/tool-1.2.3"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        // The release archive: tool-1.2.3/tool inside a .tar.gz.
        let tool = root.join("build/tool-1.2.3/tool");
        fs::write(&tool, "#!/bin/sh\necho \"tool 1.2.3\"\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let archive = root.join("build/tool.tar.gz");
        let st = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(root.join("build"))
            .arg("tool-1.2.3")
            .status()
            .unwrap();
        assert!(st.success());
        let hub = Hub::start(fs::read(&archive).unwrap());
        let e = Self {
            _tmp: tmp,
            root,
            hub,
        };
        e.fakes();
        e.remote();
        e.repo();
        e
    }

    fn fakes(&self) {
        self.script("sudo", SUDO);
        #[cfg(target_os = "macos")]
        {
            self.script("brew", BREW);
            self.script("launchctl", LAUNCHCTL);
        }
        #[cfg(target_os = "linux")]
        {
            self.script("apt-get", APT_GET);
            self.script("apt", "#!/bin/sh\nexit 0\n");
            self.script("dpkg-query", DPKG_QUERY);
            self.script("systemctl", SYSTEMCTL);
            self.script("loginctl", LOGINCTL);
        }
    }

    fn script(&self, name: &str, body: &str) {
        let p = self.root.join("bin").join(name);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        p
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// git with no user or system configuration.
    fn git(&self, dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("HOME", self.home())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", text(&out));
    }

    /// `remote.git`: a bare repository with one commit on `main`, pushed from `work`.
    fn remote(&self) {
        let work = self.root.join("work");
        fs::create_dir_all(&work).unwrap();
        self.git(&work, &["init", "-q", "-b", "main"]);
        fs::write(work.join("README.md"), "proj\n").unwrap();
        self.git(&work, &["add", "."]);
        self.git(&work, &["commit", "-q", "-m", "init"]);
        self.git(&self.root, &["clone", "-q", "--bare", "work", "remote.git"]);
    }

    fn push_commit(&self, file: &str) {
        let work = self.root.join("work");
        fs::write(work.join(file), "new\n").unwrap();
        self.git(&work, &["add", "."]);
        self.git(&work, &["commit", "-q", "-m", file]);
        self.git(&work, &["push", "-q", "../remote.git", "main"]);
    }

    fn repo(&self) {
        self.write("repo/dot.yml", "vars:\n  user_name: tester\n");
        self.write(
            "repo/secrets.yml",
            concat!(
                "default_provider: envfile\n",
                "secrets:\n",
                "  api_token: { ref: \"envfile:API_TOKEN\" }\n",
                "  github_token: { ref: \"envfile:GH_API_TOKEN\" }\n",
                "  deploy_key: { ref: \"envfile-file:deploy.pem\", kind: file }\n",
            ),
        );
        let secrets = self.write(
            "home/.config/dot/secrets.env",
            &format!("API_TOKEN={API_TOKEN}\nGH_API_TOKEN={GH_TOKEN}\n"),
        );
        fs::set_permissions(&secrets, fs::Permissions::from_mode(0o600)).unwrap();
        let pem = self.write("home/.config/dot/files/deploy.pem", PEM);
        fs::set_permissions(&pem, fs::Permissions::from_mode(0o600)).unwrap();
        self.write("home/.profile", "# mine\n");

        let remote = self.root.join("remote.git");
        self.write(
            "repo/bundles/e2e/bundle.yml",
            &format!(
                r#"steps:
  - package: [jq]
  - github_release: acme/tool
  - git: {{ repo: "file://{remote}", dest: ~/src/proj }}
  - template:
      ~/.config/app/credentials: credentials.j2
  - copy:
      ~/.config/app/static.txt: static.txt
  - block:
      file: ~/.profile
      content: "export APP_HOME={{{{ home }}}}/app"
  - secret_file:
      ~/.ssh/deploy.pem: deploy_key
  - path: [~/.local/bin]
  - env: {{ APP_MODE: e2e }}
  - shell: {{ cmd: "tool > {{{{ home }}}}/tool-out", unless: "test -e {{{{ home }}}}/tool-out" }}
  - shell: {{ cmd: "sudo -n touch {{{{ home }}}}/sudo-ran", sudo: true, unless: "test -e {{{{ home }}}}/sudo-ran" }}
  - service: {{ name: com.example.e2e, unit: e2e.plist }}
    when: {{ os: macos }}
  - service: {{ name: e2e, manager: systemd-user, unit: e2e.service, linger: true }}
    when: {{ os: linux }}
"#,
                remote = remote.display()
            ),
        );
        self.write(
            "repo/bundles/e2e/credentials.j2",
            "user={{ user_name }}\ntoken={{ secret(\"api_token\") }}\n",
        );
        self.write("repo/bundles/e2e/static.txt", "static bytes\n");
        self.write("repo/bundles/e2e/e2e.plist", "<plist>{{ home }}</plist>\n");
        self.write(
            "repo/bundles/e2e/e2e.service",
            "[Service]\nExecStart={{ home }}/.local/bin/tool\n",
        );
    }

    fn dot(&self, args: &[&str], extra_env: &[(&str, String)]) -> Output {
        let path = format!("{}:/usr/bin:/bin", self.root.join("bin").display());
        let mut c = Command::new(env!("CARGO_BIN_EXE_dot"));
        c.args(args)
            .current_dir(self.root.join("repo"))
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.home())
            .env("USER", "tester")
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("LC_ALL", "C")
            .env("DOT_GITHUB_API", &self.hub.base)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", &self.root)
            .env("FAKE_LOG", self.root.join("fake/calls.log"))
            .env("FAKE_STATE", self.root.join("fake"))
            // The fake systemctl stands in for a running systemd, even in a container.
            .env("DOT_FACTS", "init=systemd");
        for (k, v) in extra_env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    fn apply(&self, extra: &[&str], extra_env: &[(&str, String)]) -> Run {
        let logs = self.root.join("logs");
        let mut args = vec![
            "apply",
            "e2e",
            "--yes",
            "--json",
            "--log-dir",
            logs.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        let out = self.dot(&args, extra_env);
        Run::new(out)
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.root.join("fake/calls.log")).unwrap_or_default()
    }

    fn clear_calls(&self) {
        let _ = fs::remove_file(self.root.join("fake/calls.log"));
    }

    /// Everything the log files hold.
    fn logs(&self) -> String {
        let mut all = String::new();
        for e in fs::read_dir(self.root.join("logs")).unwrap() {
            all.push_str(&fs::read_to_string(e.unwrap().path()).unwrap_or_default());
        }
        all
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn mode(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// One `dot apply --json` run: its events and its full output.
struct Run {
    out: Output,
    events: Vec<Value>,
}

impl Run {
    fn new(out: Output) -> Self {
        let events = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        Self { out, events }
    }

    fn text(&self) -> String {
        text(&self.out)
    }

    fn event(&self, name: &str) -> &Value {
        self.events
            .iter()
            .find(|e| e["event"] == name)
            .unwrap_or_else(|| panic!("no {name} event: {}", self.text()))
    }

    /// Step kind -> the statuses of its actions in the plan.
    fn statuses(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for b in self.event("plan_ready")["bundles"].as_array().unwrap() {
            for s in b["steps"].as_array().unwrap() {
                let kind = s["kind"].as_str().unwrap().to_string();
                let actions = s["actions"].as_array().cloned().unwrap_or_default();
                let entry = out.entry(kind).or_default();
                entry.extend(
                    actions
                        .iter()
                        .map(|a| a["status"].as_str().unwrap().to_string()),
                );
            }
        }
        out
    }

    fn summary(&self) -> (u64, u64) {
        let f = self.event("run_finished");
        (
            f["changed"].as_u64().unwrap(),
            f["failed"].as_u64().unwrap(),
        )
    }
}

#[test]
fn every_directive_applies_converges_and_keeps_secrets_out_of_sight() {
    let e = Env::new();
    let home = e.home();
    let h = home.display().to_string();

    // A dry run plans everything and changes nothing.
    let dry = e.apply(&["--dry-run"], &[]);
    assert!(dry.out.status.success(), "{}", dry.text());
    assert!(!home.join(".config/app").exists());
    assert!(!home.join("src").exists());
    assert!(!home.join(".local/bin/tool").exists());
    assert!(!e.root.join("state/dot/state.json").exists());
    assert!(!e.calls().contains("install"), "{}", e.calls());
    assert!(!e.hub.seen().iter().any(|(p, _)| p.starts_with("/dl/")));
    assert_eq!(
        fs::read_to_string(home.join(".profile")).unwrap(),
        "# mine\n"
    );
    e.clear_calls();

    // The real run.
    let run = e.apply(&[], &[]);
    let out = run.text();
    assert!(run.out.status.success(), "{out}");
    let plan = run.event("plan_ready");
    assert_eq!(plan["sudo"], serde_json::json!(["e2e"]), "{plan}");
    let secrets: Vec<&str> = plan["secrets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(secrets, ["api_token", "deploy_key", "github_token"]);
    let statuses = run.statuses();
    // Anonymous lookups are refused, and the token secret is only resolved to apply.
    assert_eq!(statuses["github_release"], ["unknown"], "{out}");
    assert_eq!(statuses["secret_file"], ["unknown"]);
    assert_eq!(statuses["template"], ["unknown"]);
    assert_eq!(run.summary().1, 0, "{out}");

    // package
    #[cfg(target_os = "macos")]
    assert!(
        e.calls().contains("brew install --formula jq"),
        "{}",
        e.calls()
    );
    #[cfg(target_os = "linux")]
    assert!(
        e.calls().lines().any(|l| l.starts_with("sudo -n ")
            && l.contains("apt-get install")
            && l.ends_with(" jq")),
        "{}",
        e.calls()
    );
    // github_release: verified, installed, runnable by later steps through `path:`.
    let tool = home.join(".local/bin/tool");
    assert_eq!(mode(&tool), 0o755);
    assert_eq!(
        fs::read_to_string(home.join("tool-out")).unwrap(),
        "tool 1.2.3\n"
    );
    let seen = e.hub.seen();
    assert!(
        seen.iter()
            .any(|(p, a)| p == "/repos/acme/tool/releases/latest"
                && a.as_deref() == Some(&format!("Bearer {GH_TOKEN}"))),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .filter(|(p, _)| p.starts_with("/dl/"))
            .all(|(_, a)| a.is_none()),
        "the token went to a download: {seen:?}"
    );
    // git
    assert_eq!(
        fs::read_to_string(home.join("src/proj/README.md")).unwrap(),
        "proj\n"
    );
    // template with a secret: 0600.
    let creds = home.join(".config/app/credentials");
    assert_eq!(
        fs::read_to_string(&creds).unwrap(),
        format!("user=tester\ntoken={API_TOKEN}\n")
    );
    assert_eq!(mode(&creds), 0o600);
    // copy
    assert_eq!(
        fs::read_to_string(home.join(".config/app/static.txt")).unwrap(),
        "static bytes\n"
    );
    // block: the user's lines stay.
    assert_eq!(
        fs::read_to_string(home.join(".profile")).unwrap(),
        format!("# mine\n# >>> dot:e2e >>>\nexport APP_HOME={h}/app\n# <<< dot:e2e <<<\n")
    );
    // secret_file
    let pem = home.join(".ssh/deploy.pem");
    assert_eq!(fs::read_to_string(&pem).unwrap(), PEM);
    assert_eq!(mode(&pem), 0o600);
    // sudo: asked for once, up front, before the step that uses it.
    let calls = e.calls();
    let primed = calls.find("sudo -n true").expect(&calls);
    let used = calls.find("sudo -n touch").expect(&calls);
    assert!(primed < used, "{calls}");
    assert!(home.join("sudo-ran").exists());
    // service
    #[cfg(target_os = "macos")]
    {
        assert_eq!(
            fs::read_to_string(home.join("Library/LaunchAgents/com.example.e2e.plist")).unwrap(),
            format!("<plist>{h}</plist>\n")
        );
        assert!(calls.contains("launchctl bootstrap gui/"), "{calls}");
        assert!(calls.contains("launchctl kickstart gui/"), "{calls}");
    }
    #[cfg(target_os = "linux")]
    {
        assert!(
            home.join(".config/systemd/user/e2e.service").exists(),
            "{calls}"
        );
        assert!(
            calls.contains("sudo -n loginctl enable-linger tester"),
            "{calls}"
        );
        assert!(
            calls.contains("systemctl --user start e2e.service"),
            "{calls}"
        );
    }
    // No secret reaches the console or the log.
    let logs = e.logs();
    assert!(!logs.is_empty());
    for secret in [
        API_TOKEN,
        GH_TOKEN,
        "MIIBe2eSecretKeyLineOne",
        "MIIBe2eSecretKeyLineTwo",
    ] {
        assert!(!out.contains(secret), "{secret} on the console: {out}");
        assert!(!logs.contains(secret), "{secret} in the log");
    }

    // `dot env --shell`: the recorded PATH and variables, sourceable (twice) by a shell.
    let env_out = e.dot(&["env", "--shell", "bash"], &[]);
    assert!(env_out.status.success(), "{}", text(&env_out));
    let script = e.write("env.sh", &String::from_utf8_lossy(&env_out.stdout));
    let sourced = Command::new("sh")
        .arg("-c")
        .arg(format!(
            ". '{0}'; . '{0}'; tool; echo \"$APP_MODE|$PATH\"",
            script.display()
        ))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&sourced.stdout),
        format!("tool 1.2.3\ne2e|{h}/.local/bin:/usr/bin:/bin\n"),
        "{}",
        text(&sourced)
    );

    // Second run, in a shell that sourced `dot env` and has GITHUB_TOKEN: nothing to do, and no
    // sudo needed.
    e.clear_calls();
    let shell_env = [
        (
            "PATH",
            format!(
                "{}:{h}/.local/bin:/usr/bin:/bin",
                e.root.join("bin").display()
            ),
        ),
        ("APP_MODE", "e2e".to_string()),
        ("GITHUB_TOKEN", GH_TOKEN.to_string()),
    ];
    let again = e.apply(&[], &shell_env);
    let out = again.text();
    assert!(again.out.status.success(), "{out}");
    assert_eq!(again.summary(), (0, 0), "{out}");
    let statuses = again.statuses();
    for kind in [
        "package",
        "github_release",
        "git",
        "copy",
        "block",
        "path",
        "env",
        "shell",
        "service",
    ] {
        assert!(
            statuses[kind].iter().all(|s| s == "satisfied"),
            "{kind}: {:?}\n{out}",
            statuses[kind]
        );
    }
    assert!(again.event("plan_ready").get("sudo").is_none(), "{out}");
    assert!(!e.calls().contains("sudo"), "{}", e.calls());
    assert!(!out.contains(GH_TOKEN) && !e.logs().contains(GH_TOKEN));

    // A new upstream commit: only `git` changes.
    e.push_commit("NEWS.md");
    let third = e.apply(&[], &shell_env);
    assert!(third.out.status.success(), "{}", third.text());
    assert_eq!(third.summary(), (1, 0), "{}", third.text());
    assert_eq!(third.statuses()["git"], ["pending"]);
    assert!(home.join("src/proj/NEWS.md").exists());
}
