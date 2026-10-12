//! System directives end to end: `package_repo` (apt keyring and deb822 source with a suite
//! fallback), `package` upgrades, `user` (shell and groups, with run notices) and `absent`. The
//! built `dot` binary runs with `PATH` pointing at fake `sudo` / `apt-get` / `dpkg-query` /
//! `apt-cache` / `curl` / `gpg` / `getent` / `id` / `usermod` / `groupadd` scripts that record
//! their argv, and `DOT_FACTS` making the host an Ubuntu machine. No real package manager, no
//! sudo, no network, no account changes; HOME, XDG_STATE_HOME and every file a step writes are
//! in a temp dir.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// Fake `sudo`: `-n true` succeeds only when `$FAKE_STATE/sudo-ok` exists; `-n -v` succeeds;
/// anything else runs the command after `-n` (as this user: files land in the temp dir).
const SUDO: &str = r#"#!/bin/sh
echo "sudo $*" >> "$FAKE_LOG"
[ "$1" = "-n" ] && shift
case "$1" in
  true) [ -e "$FAKE_STATE/sudo-ok" ]; exit $? ;;
  -v) exit 0 ;;
esac
exec "$@"
"#;

/// Fake `curl`: `-o FILE URL` writes an ASCII-armored key; a HEAD request (`-fsSIL`) fails with
/// an HTTP error (exit 22) for the `questing` suite and succeeds for anything else.
const CURL: &str = r#"#!/bin/sh
echo "curl $*" >> "$FAKE_LOG"
out=""; head=0; url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift ;;
    -fsSIL) head=1 ;;
    --max-time|--retry) shift ;;
    -*) ;;
    *) url="$1" ;;
  esac
  shift
done
if [ $head = 1 ]; then
  case "$url" in *questing*) exit 22 ;; esac
  exit 0
fi
printf -- '-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nfake\n-----END PGP PUBLIC KEY BLOCK-----\n' > "$out"
"#;

/// Fake `gpg`: `--dearmor` writes a "binary" key to `--output`; `--show-keys` prints the
/// fingerprint in `$FAKE_STATE/fpr` (default: Docker's).
const GPG: &str = r#"#!/bin/sh
echo "gpg $*" >> "$FAKE_LOG"
out=""; mode=""
while [ $# -gt 0 ]; do
  case "$1" in
    --output) out="$2"; shift ;;
    --homedir) shift ;;
    --dearmor) mode=dearmor ;;
    --show-keys) mode=show ;;
  esac
  shift
done
case "$mode" in
  dearmor) printf 'BINARY-KEY' > "$out" ;;
  show)
    fpr=$(cat "$FAKE_STATE/fpr" 2>/dev/null || echo 9DC858229FC7DD38854AE2D88D81803C0EBFCD88)
    echo "pub:-:4096:1:8D81803C0EBFCD88:1:::-:::scESA:"
    echo "fpr:::::::::$fpr:" ;;
esac
"#;

/// Fake `dpkg-query -W -f FORMAT <names>`: installed ones are in `$FAKE_STATE/apt`.
const DPKG_QUERY: &str = r#"#!/bin/sh
echo "dpkg-query $*" >> "$FAKE_LOG"
shift 3
rc=0
for p in "$@"; do
  if grep -qx "$p" "$FAKE_STATE/apt" 2>/dev/null; then printf '%s\tii \n' "$p"; else rc=1; fi
done
exit $rc
"#;

/// Fake `apt-get update|install [...] <names>`: installs and upgrades (`--only-upgrade`) record
/// the candidate, version 2.0.
const APT_GET: &str = r#"#!/bin/sh
echo "apt-get $* (DEBIAN_FRONTEND=$DEBIAN_FRONTEND)" >> "$FAKE_LOG"
case "$1" in
  update) echo "Reading package lists..."; exit 0 ;;
  install)
    shift
    skip=0; up=0
    for a in "$@"; do
      if [ $skip = 1 ]; then skip=0; continue; fi
      case "$a" in
        -o) skip=1 ;;
        --only-upgrade) up=1 ;;
        -*) ;;
        *) [ $up = 1 ] || echo "$a" >> "$FAKE_STATE/apt"; echo 2.0 > "$FAKE_STATE/ver-$a" ;;
      esac
    done
    exit 0 ;;
esac
exit 0
"#;

/// Fake `apt-cache policy <names>`: installed at `$FAKE_STATE/ver-<name>` (default 1.0),
/// candidate 2.0.
const APT_CACHE: &str = r#"#!/bin/sh
echo "apt-cache $*" >> "$FAKE_LOG"
shift
for p in "$@"; do
  echo "$p:"
  if grep -qx "$p" "$FAKE_STATE/apt" 2>/dev/null; then
    echo "  Installed: $(cat "$FAKE_STATE/ver-$p" 2>/dev/null || echo 1.0)"
  else
    echo "  Installed: (none)"
  fi
  echo "  Candidate: 2.0"
done
"#;

/// Fake account database: `getent passwd|group`, `id -nG`, `usermod -s|-aG`, `groupadd`.
const GETENT: &str = r#"#!/bin/sh
echo "getent $*" >> "$FAKE_LOG"
case "$1" in
  passwd) echo "$2:x:1000:1000::/home/$2:$(cat "$FAKE_STATE/shell" 2>/dev/null || echo /bin/bash)" ;;
  group) grep -qx "$2" "$FAKE_STATE/groups-db" 2>/dev/null && echo "$2:x:999:" && exit 0; exit 2 ;;
esac
"#;
const ID: &str = r#"#!/bin/sh
echo "id $*" >> "$FAKE_LOG"
echo "$2 $(tr '\n' ' ' < "$FAKE_STATE/groups" 2>/dev/null)"
"#;
const USERMOD: &str = r#"#!/bin/sh
echo "usermod $*" >> "$FAKE_LOG"
case "$1" in
  -s) echo "$2" > "$FAKE_STATE/shell" ;;
  -aG) echo "$2" >> "$FAKE_STATE/groups" ;;
esac
"#;
const GROUPADD: &str = r#"#!/bin/sh
echo "groupadd $*" >> "$FAKE_LOG"
echo "$1" >> "$FAKE_STATE/groups-db"
"#;

/// An Ubuntu 25.10 (questing) machine with apt.
const UBUNTU: &str =
    "os=linux,family=debian,distro=ubuntu,version=25.10,codename=questing,pkg=apt,role=server";

struct Env {
    _tmp: TempDir,
    root: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in [
            "home",
            "logs",
            "repo/bundles/demo",
            "state",
            "bin",
            "fake",
            "etc",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("repo/dot.yml"), "").unwrap();
        let env = Self { _tmp: tmp, root };
        for (name, body) in [
            ("sudo", SUDO),
            ("curl", CURL),
            ("gpg", GPG),
            ("apt-get", APT_GET),
            ("apt", "#!/bin/sh\nexit 0\n"),
            ("dpkg-query", DPKG_QUERY),
            ("apt-cache", APT_CACHE),
            ("getent", GETENT),
            ("id", ID),
            ("usermod", USERMOD),
            ("groupadd", GROUPADD),
        ] {
            env.script(name, body);
        }
        fs::write(env.fake().join("sudo-ok"), "").unwrap();
        env
    }

    fn script(&self, name: &str, body: &str) {
        let p = self.root.join("bin").join(name);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn fake(&self) -> PathBuf {
        self.root.join("fake")
    }

    fn bundle(&self, yaml: &str) -> PathBuf {
        let p = self.root.join("repo/bundles/demo/bundle.yml");
        fs::write(&p, yaml.replace("ROOT", &self.root.display().to_string())).unwrap();
        p
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.fake().join("calls.log")).unwrap_or_default()
    }

    fn clear_calls(&self) {
        let _ = fs::remove_file(self.fake().join("calls.log"));
    }

    fn dot(&self, args: &[&str]) -> Output {
        let path = format!("{}:/usr/bin:/bin", self.root.join("bin").display());
        Command::new(env!("CARGO_BIN_EXE_dot"))
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("USER", "tester")
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("LC_ALL", "C")
            .env("DOT_FACTS", UBUNTU)
            .env("FAKE_LOG", self.fake().join("calls.log"))
            .env("FAKE_STATE", self.fake())
            .output()
            .unwrap()
    }

    fn plan(&self, bundle: &Path, extra: &[&str]) -> Output {
        let mut args = vec!["plan", "-c", bundle.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.dot(&args)
    }

    fn apply(&self, bundle: &Path, extra: &[&str]) -> Output {
        let logs = self.root.join("logs");
        let mut args = vec![
            "apply",
            "--yes",
            "--plain",
            "-c",
            bundle.to_str().unwrap(),
            "--log-dir",
            logs.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.dot(&args)
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

const DOCKER: &str = r#"steps:
  - package_repo:
      name: docker
      apt:
        url: https://download.docker.com/linux/ubuntu
        key: https://download.docker.com/linux/ubuntu/gpg
        fingerprint: 9DC8 5822 9FC7 DD38 854A  E2D8 8D81 803C 0EBF CD88
        suites: "{{ codename }}"
        fallback_suite: noble
        components: [stable]
        keyring: ROOT/etc/keyrings/docker.gpg
        file: ROOT/etc/sources.list.d/docker.sources
  - package: [docker-ce]
"#;

#[test]
fn an_apt_repository_is_added_with_a_checked_key_and_the_next_install_refreshes() {
    let env = Env::new();
    let bundle = env.bundle(DOCKER);
    // apt's lists were refreshed a moment ago: only the new repository makes it refresh again.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::create_dir_all(env.root.join("state/dot")).unwrap();
    fs::write(
        env.root.join("state/dot/apt-update.stamp"),
        format!("{now}\n"),
    )
    .unwrap();
    let root = env.root.display().to_string();

    let out = env.plan(&bundle, &[]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(
        t.contains(&format!(
            "apt keyring {root}/etc/keyrings/docker.gpg <- https://download.docker.com/linux/ubuntu/gpg (fingerprint checked before installing; sudo)"
        )),
        "{t}"
    );
    assert!(
        t.contains(
            "(suite noble: the repository has nothing for questing; new file, 7 lines; sudo)"
        ),
        "{t}"
    );
    assert!(t.contains("apt: install docker-ce"), "{t}");
    assert!(!env.calls().contains("install -m"), "plan installs nothing");

    env.clear_calls();
    let out = env.apply(&bundle, &[]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert_eq!(
        fs::read(env.root.join("etc/keyrings/docker.gpg")).unwrap(),
        b"BINARY-KEY",
        "the dearmored key"
    );
    assert_eq!(
        fs::read_to_string(env.root.join("etc/sources.list.d/docker.sources")).unwrap(),
        format!(
            "# Managed by dot (package_repo docker); changes are overwritten.\nTypes: deb\n\
             URIs: https://download.docker.com/linux/ubuntu\nSuites: noble\nComponents: stable\n\
             Architectures: {}\nSigned-By: {root}/etc/keyrings/docker.gpg\n",
            if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "amd64"
            }
        )
    );
    let calls = env.calls();
    let order: Vec<&str> = calls
        .lines()
        .filter(|l| {
            l.starts_with("curl")
                || l.starts_with("gpg")
                || l.starts_with("sudo -n install")
                || l.starts_with("apt-get")
        })
        .collect();
    let at = |prefix: &str| {
        order
            .iter()
            .position(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no `{prefix}` in {order:#?}"))
    };
    assert!(at("curl -fsSL") < at("gpg"), "{order:#?}");
    assert!(
        order.iter().any(|l| l.contains("--show-keys")),
        "fingerprint checked: {order:#?}"
    );
    assert!(
        at("gpg") < at("sudo -n install"),
        "checked before installing"
    );
    assert!(
        at("apt-get update") < at("apt-get install"),
        "the fresh stamp was reset by the new repository: {order:#?}"
    );

    // Converged: the keyring's fingerprint is checked again, nothing changes.
    let out = env.apply(&bundle, &[]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("Summary: 0 changed, 2 ok"), "{t}");

    // A key that does not have the fingerprint is refused, and nothing is replaced.
    fs::write(
        env.fake().join("fpr"),
        "1111222233334444555566667777888899990000",
    )
    .unwrap();
    fs::write(env.root.join("etc/keyrings/docker.gpg"), "OLD").unwrap();
    let out = env.apply(&bundle, &[]);
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("refusing to install it"), "{t}");
    assert_eq!(
        fs::read_to_string(env.root.join("etc/keyrings/docker.gpg")).unwrap(),
        "OLD"
    );
}

#[test]
fn upgrades_only_when_asked() {
    let env = Env::new();
    fs::write(env.fake().join("apt"), "jq\n").unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::create_dir_all(env.root.join("state/dot")).unwrap();
    fs::write(
        env.root.join("state/dot/apt-update.stamp"),
        format!("{now}\n"),
    )
    .unwrap();
    let bundle = env.bundle("steps:\n  - package: [jq]\n");

    let out = env.plan(&bundle, &["-v"]);
    let t = text(&out);
    assert!(t.contains("apt: jq installed"), "{t}");
    assert!(
        !env.calls().contains("apt-cache"),
        "install-missing-only by default"
    );

    let out = env.plan(&bundle, &["--upgrade"]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("apt: upgrade jq 1.0 -> 2.0 [sudo]"), "{t}");

    let out = env.apply(&bundle, &["--upgrade"]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(env.calls().contains("--only-upgrade jq"), "{}", env.calls());
    let out = env.plan(&bundle, &["--upgrade", "-v"]);
    assert!(text(&out).contains("apt: jq installed"), "{}", text(&out));

    // `upgrade: true` on the step does the same without the flag.
    fs::remove_file(env.fake().join("ver-jq")).unwrap();
    let bundle = env.bundle("steps:\n  - package: { names: [jq], upgrade: true }\n");
    let out = env.plan(&bundle, &[]);
    assert!(
        text(&out).contains("apt: upgrade jq 1.0 -> 2.0"),
        "{}",
        text(&out)
    );
}

#[test]
fn user_changes_and_absent_paths_end_with_notices() {
    let env = Env::new();
    fs::write(env.root.join("home/.old-tool"), "x").unwrap();
    fs::create_dir_all(env.root.join("outside")).unwrap();
    fs::write(env.root.join("outside/legacy.list"), "deb x").unwrap();
    let bundle = env.bundle(
        "steps:\n  - user: { shell: sh, groups: [docker] }\n  - absent:\n      ~/.old-tool:\n      ROOT/outside/legacy.list: { root: true }\n",
    );

    let out = env.plan(&bundle, &[]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(
        t.contains(
            "login shell of tester: /bin/bash -> /bin/sh [sudo; takes effect at the next login]"
        ),
        "{t}"
    );
    assert!(
        t.contains("groups of tester: add docker [sudo; takes effect at the next login]"),
        "{t}"
    );
    assert!(t.contains("remove ~/.old-tool (file)"), "{t}");

    let out = env.apply(&bundle, &[]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    let calls = env.calls();
    assert!(
        calls.contains("sudo -n usermod -s /bin/sh tester"),
        "{calls}"
    );
    assert!(calls.contains("sudo -n groupadd docker"), "{calls}");
    assert!(
        calls.contains("sudo -n usermod -aG docker tester"),
        "{calls}"
    );
    assert!(!env.root.join("home/.old-tool").exists());
    assert!(!env.root.join("outside/legacy.list").exists());
    let summary = t.find("Summary:").expect("a summary");
    let after = &t[summary..];
    assert!(
        after.contains("Notice: log out and back in: the login shell of tester is now /bin/sh"),
        "{t}"
    );
    assert!(
        after.contains(
            "Notice: log out and back in: tester was added to group docker, which takes effect at the next login"
        ),
        "{t}"
    );
    let log = fs::read_dir(env.root.join("logs"))
        .unwrap()
        .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
        .find(|l| l.contains("run finished"))
        .unwrap();
    assert!(log.contains("notice: log out and back in"), "{log}");

    // Converged: nothing to do, nothing to notice.
    let out = env.dot(&[
        "apply",
        "--yes",
        "--json",
        "-c",
        bundle.to_str().unwrap(),
        "--log-dir",
        env.root.join("logs").to_str().unwrap(),
    ]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    let last = t.lines().rfind(|l| l.contains("run_finished")).unwrap();
    let v: serde_json::Value = serde_json::from_str(last).unwrap();
    assert_eq!(v["changed"], 0, "{last}");
    assert_eq!(v["notices"], serde_json::json!([]), "{last}");

    // Outside the home directory without `root: true`: refused before anything runs.
    let bundle = env.bundle("steps:\n  - absent: [ROOT/outside]\n");
    let out = env.plan(&bundle, &[]);
    let t = text(&out);
    assert!(t.contains("outside the home directory"), "{t}");
    assert!(env.root.join("outside").exists());
}

/// The Docker setup of a server bundle in one go: the old one-line source list goes, the
/// vendor repository comes in with a checked key, packages are installed and upgraded after the
/// refresh the new repository forces, and the user joins the `docker` group.
const DOCKER_HOST: &str = r#"steps:
  - absent:
      ROOT/etc/sources.list.d/docker.list: { root: true }
      "{{ config_dir }}/docker-old": {}
  - package_repo:
      name: docker
      apt:
        url: https://download.docker.com/linux/ubuntu
        key: https://download.docker.com/linux/ubuntu/gpg
        fingerprint: 9DC858229FC7DD38854AE2D88D81803C0EBFCD88
        suites: "{{ codename }}"
        fallback_suite: noble
        components: [stable]
        keyring: ROOT/etc/keyrings/docker.gpg
        file: ROOT/etc/sources.list.d/docker.sources
  - package: { names: [jq, docker-ce], upgrade: true }
  - user: { groups: [docker] }
"#;

#[test]
fn a_repository_packages_a_group_and_removals_in_one_run() {
    let env = Env::new();
    fs::write(env.fake().join("apt"), "jq\n").unwrap();
    fs::create_dir_all(env.root.join("etc/sources.list.d")).unwrap();
    fs::write(
        env.root.join("etc/sources.list.d/docker.list"),
        "deb https://download.docker.com/linux/ubuntu jammy stable\n",
    )
    .unwrap();
    // `config_dir` is `$XDG_CONFIG_HOME`, else `~/.config`, on Linux.
    fs::create_dir_all(env.root.join("home/.config/docker-old/cache")).unwrap();
    fs::write(env.root.join("home/.config/docker-old/cache/x"), "x").unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::create_dir_all(env.root.join("state/dot")).unwrap();
    fs::write(
        env.root.join("state/dot/apt-update.stamp"),
        format!("{now}\n"),
    )
    .unwrap();
    let bundle = env.bundle(DOCKER_HOST);

    let out = env.plan(&bundle, &[]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    for want in [
        "remove ROOT/etc/sources.list.d/docker.list (file)",
        "remove ~/.config/docker-old (directory, 1 entry)",
        "(suite noble: the repository has nothing for questing; new file, 7 lines; sudo)",
        "apt: install docker-ce; upgrade jq 1.0 -> 2.0",
        "groups of tester: add docker [sudo; takes effect at the next login]",
    ] {
        let want = want.replace("ROOT", &env.root.display().to_string());
        assert!(t.contains(&want), "missing `{want}` in:\n{t}");
    }
    assert!(
        !env.calls().contains("sudo -n install"),
        "plan changes nothing"
    );

    env.clear_calls();
    let out = env.apply(&bundle, &[]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("Summary: 4 changed"), "{t}");
    assert!(!env.root.join("etc/sources.list.d/docker.list").exists());
    assert!(!env.root.join("home/.config/docker-old").exists());
    assert!(env.root.join("etc/sources.list.d/docker.sources").exists());
    assert_eq!(
        fs::read_to_string(env.fake().join("apt")).unwrap(),
        "jq\ndocker-ce\n"
    );
    assert_eq!(
        fs::read_to_string(env.fake().join("ver-jq")).unwrap(),
        "2.0\n"
    );
    let calls = env.calls();
    let lines: Vec<&str> = calls.lines().collect();
    let at = |needle: &str| {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no `{needle}` in {lines:#?}"))
    };
    // sudo is checked up front, before anything changes (and again right before each action
    // that needs it: something may have discarded the timestamp since), never asked for.
    assert!(at("sudo -n true") < at("sudo -n install"), "{lines:#?}");
    assert!(!lines.contains(&"sudo -v"), "{lines:#?}");
    // The new repository is in place before the refresh, the refresh before the installs, and
    // the upgrade is decided after it.
    assert!(at("docker.sources") < at("apt-get update"), "{lines:#?}");
    assert!(at("apt-get update") < at("apt-get install"), "{lines:#?}");
    assert!(at("apt-get update") < at("--only-upgrade jq"), "{lines:#?}");
    assert!(at("groupadd docker") < at("usermod -aG docker tester"));
    let summary = &t[t.find("Summary:").unwrap()..];
    assert!(
        summary.contains("Notice: log out and back in: tester was added to group docker"),
        "{t}"
    );

    // Converged: a second run changes nothing and notices nothing.
    env.clear_calls();
    let out = env.apply(&bundle, &[]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("Summary: 0 changed, 4 ok"), "{t}");
    assert!(!t.contains("Notice:"), "{t}");
    let calls = env.calls();
    assert!(
        !calls.contains("sudo -n true"),
        "nothing needs sudo: {calls}"
    );
    assert!(!calls.contains("apt-get"), "{calls}");
}

#[test]
fn bad_facts_overrides_are_a_warning_not_a_crash() {
    let env = Env::new();
    let out = Command::new(env!("CARGO_BIN_EXE_dot"))
        .arg("facts")
        .env("DOT_FACTS", "os=beos")
        .env("HOME", env.root.join("home"))
        .output()
        .unwrap();
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(
        t.contains("warning: DOT_FACTS: unknown value `beos`"),
        "{t}"
    );
    let out = env.dot(&["facts"]);
    let t = text(&out);
    assert!(t.contains("codename"), "{t}");
    assert!(t.contains("questing"), "{t}");
}
