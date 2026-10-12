//! `package` steps and the up-front sudo check, end to end: the built `dot` binary runs with
//! `PATH` pointing at fake `brew` / `apt-get` / `dpkg-query` / `sudo` / `curl` scripts that
//! record their argv. No real package manager, no sudo, no network; HOME and XDG_STATE_HOME are
//! temp dirs.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

struct Env {
    _tmp: TempDir,
    root: PathBuf,
}

/// Fake `sudo`. With `-n`: `true` succeeds only when `$FAKE_STATE/sudo-ok` exists (a valid
/// timestamp, or passwordless sudo); `-k true` only when `$FAKE_STATE/nopasswd` exists
/// (passwordless sudo); `-v` succeeds; anything else runs. `-v`: succeeds. `-A` (askpass): runs
/// the command when passwordless, else asks `$SUDO_ASKPASS`, failing when that fails. Anything
/// else is Homebrew's sudo, which ignores any timestamp: unless passwordless, it asks for the
/// password on the controlling terminal, as the real one does, whatever its stdin is.
const SUDO: &str = r#"#!/bin/sh
echo "sudo $*" >> "$FAKE_LOG"
if [ "$1" = "-n" ]; then
  shift
  case "$1" in
    true) [ -e "$FAKE_STATE/sudo-ok" ]; exit $? ;;
    -k) [ -e "$FAKE_STATE/nopasswd" ]; exit $? ;;
    -v) exit 0 ;;
  esac
  exec "$@"
fi
case "$1" in
  -v) exit 0 ;;
  -A)
    shift
    if [ ! -e "$FAKE_STATE/nopasswd" ]; then
      "$SUDO_ASKPASS" > /dev/null || { echo "sudo: a password is required" >&2; exit 1; }
    fi
    exec "$@" ;;
esac
if [ ! -e "$FAKE_STATE/nopasswd" ]; then
  printf 'Password:' > /dev/tty || exit 1
  read -r pw < /dev/tty || exit 1
  echo "sudo: password typed" >> "$FAKE_LOG"
fi
exec "$@"
"#;

/// Fake `brew`: `list --formula|--cask --versions <names>` prints the installed ones (those in
/// `$FAKE_STATE/brew`) and exits 1 if any is missing; `install` records the names, and how it
/// was run (`brew-env ...`: stdin a terminal or not, `SUDO_ASKPASS`, `HOMEBREW_NO_SUDO`). Cask
/// `pkgapp` runs its `.pkg` installer through sudo the way Homebrew does: refused with
/// `HOMEBREW_NO_SUDO`, `sudo -A` when `SUDO_ASKPASS` is set, else plain `sudo` (which asks on
/// the terminal).
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
    if [ -t 0 ]; then tty=tty; else tty=none; fi
    echo "brew-env $* stdin=$tty askpass=${SUDO_ASKPASS:-} nosudo=${HOMEBREW_NO_SUDO:-}" >> "$FAKE_LOG"
    echo "==> Installing $*"
    for a in "$@"; do
      case "$a" in
        --*) ;;
        pkgapp)
          if [ -n "${HOMEBREW_NO_SUDO:-}" ]; then
            echo "Error: sudo is disabled by HOMEBREW_NO_SUDO." >&2; exit 1
          elif [ -n "${SUDO_ASKPASS:-}" ]; then
            sudo -A installer -pkg x.pkg -target / || exit 1
          else
            sudo installer -pkg x.pkg -target / || exit 1
          fi
          echo "$a" >> "$FAKE_STATE/brew" ;;
        *) echo "$a" >> "$FAKE_STATE/brew" ;;
      esac
    done
    exit 0 ;;
esac
exit 0
"#;

/// Fake `installer` (a `.pkg`'s, run as root by Homebrew).
#[cfg(target_os = "macos")]
const INSTALLER: &str = r#"#!/bin/sh
echo "installer $*" >> "$FAKE_LOG"
"#;

/// Fake `curl` for Homebrew's cask API: `pkgapp` runs a .pkg installer, anything else is an app.
const CURL: &str = r#"#!/bin/sh
echo "curl $*" >> "$FAKE_LOG"
case "$*" in
  */pkgapp.json) echo '{"artifacts":[{"pkg":["x.pkg"]}]}' ;;
  *) echo '{"artifacts":[{"app":["X.app"]}]}' ;;
esac
"#;

/// Fake `dpkg-query -W -f FORMAT <names>`: installed ones are in `$FAKE_STATE/apt`.
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

/// Fake `apt-get update|install -y ... <names>` (options start with `-`, `-o` takes a value).
#[cfg(target_os = "linux")]
const APT_GET: &str = r#"#!/bin/sh
echo "apt-get $* (DEBIAN_FRONTEND=$DEBIAN_FRONTEND)" >> "$FAKE_LOG"
case "$1" in
  update) echo "Reading package lists..."; exit 0 ;;
  install)
    shift
    skip=0
    for a in "$@"; do
      if [ $skip = 1 ]; then skip=0; continue; fi
      case "$a" in -o) skip=1 ;; -*) ;; *) echo "$a" >> "$FAKE_STATE/apt" ;; esac
    done
    exit 0 ;;
esac
exit 0
"#;

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["home", "logs", "repo/bundles/demo", "state", "bin", "fake"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("repo/dot.yml"), "").unwrap();
        let env = Self { _tmp: tmp, root };
        env.script("sudo", SUDO);
        env.script("curl", CURL);
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
        fs::write(&p, yaml).unwrap();
        p
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.fake().join("calls.log")).unwrap_or_default()
    }

    fn dot(&self, args: &[&str]) -> Output {
        let path = format!("{}:/usr/bin:/bin", self.root.join("bin").display());
        Command::new(env!("CARGO_BIN_EXE_dot"))
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("LC_ALL", "C")
            .env("FAKE_LOG", self.fake().join("calls.log"))
            .env("FAKE_STATE", self.fake())
            .output()
            .unwrap()
    }

    /// [`Env::dot`] with extra environment variables.
    fn dot_with(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let path = format!("{}:/usr/bin:/bin", self.root.join("bin").display());
        let mut c = Command::new(env!("CARGO_BIN_EXE_dot"));
        c.args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("LC_ALL", "C")
            .env("FAKE_LOG", self.fake().join("calls.log"))
            .env("FAKE_STATE", self.fake());
        for (k, v) in extra {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    /// `dot <args>` on a new pseudo-terminal that is its controlling terminal (so `/dev/tty`
    /// is that terminal, as in a real session).
    #[cfg(target_os = "macos")]
    fn dot_on_pty(&self, args: &[&str]) -> pty::Pty {
        let path = format!("{}:/usr/bin:/bin", self.root.join("bin").display());
        let mut c = Command::new(env!("CARGO_BIN_EXE_dot"));
        c.args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("LC_ALL", "C")
            .env("TERM", "xterm")
            .env("FAKE_LOG", self.fake().join("calls.log"))
            .env("FAKE_STATE", self.fake());
        pty::Pty::spawn(c)
    }

    fn plan(&self, bundle: &Path) -> Output {
        self.dot(&["plan", "-c", bundle.to_str().unwrap()])
    }

    fn apply(&self, bundle: &Path) -> Output {
        let logs = self.root.join("logs");
        self.dot(&[
            "apply",
            "--yes",
            "--plain",
            "-c",
            bundle.to_str().unwrap(),
            "--log-dir",
            logs.to_str().unwrap(),
        ])
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[cfg(target_os = "macos")]
#[test]
fn brew_and_casks_with_sudo_checked_up_front() {
    let env = Env::new();
    env.script("brew", BREW);
    env.script("installer", INSTALLER);
    fs::write(env.fake().join("brew"), "jq\n").unwrap();
    let bundle = env.bundle(
        "steps:\n  - package: { brew: [fd, jq], cask: [pkgapp, wezterm], apt: [ignored] }\n",
    );

    let out = env.plan(&bundle);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(
        t.contains("brew: install fd (already installed: jq)"),
        "{t}"
    );
    assert!(
        t.contains(
            "brew cask: install pkgapp, wezterm [--adopt for wezterm; pkgapp: Homebrew asks for \
             the sudo password in the terminal]"
        ),
        "{t}"
    );
    assert!(
        t.contains(
            "sudo password asked for in the terminal while applying (Homebrew asks itself) by: \
             demo: package "
        ),
        "{t}"
    );
    assert!(!t.contains("sudo needed"), "dot itself needs no sudo: {t}");
    assert!(
        !env.calls().contains("brew install"),
        "plan installs nothing"
    );

    // Non-interactive and sudo wants a password: fails before anything is installed, even with
    // a valid timestamp (Homebrew discards it).
    for timestamp in [false, true] {
        if timestamp {
            fs::write(env.fake().join("sudo-ok"), "").unwrap();
        }
        let out = env.apply(&bundle);
        let t = text(&out);
        assert!(!out.status.success(), "{t}");
        assert!(
            t.contains("demo: package fd, jq, ... (5 total): Homebrew asks for the sudo password"),
            "{t}"
        );
        assert!(t.contains("non-interactively"), "{t}");
        let calls = env.calls();
        assert!(calls.contains("sudo -n -k true"), "{calls}");
        assert!(!calls.contains("brew install"), "{calls}");
        assert!(!calls.contains("Password"), "{calls}");
    }

    // Passwordless sudo: installs exactly what is missing, adopting the app; the `.pkg` cask
    // streamed like the rest, its sudo free (it will not ask), the formulae without Homebrew's
    // sudo; the app's sudo cannot ask, and Homebrew may use it (passwordless).
    fs::write(env.fake().join("nopasswd"), "").unwrap();
    fs::remove_file(env.fake().join("calls.log")).unwrap();
    let out = env.apply(&bundle);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    let calls = env.calls();
    assert!(calls.contains("sudo -n -k true"), "{calls}");
    assert!(calls.contains("brew install --formula fd\n"), "{calls}");
    assert!(
        calls.contains("brew-env --formula fd stdin=none askpass= nosudo=1\n"),
        "{calls}"
    );
    assert!(
        calls.contains("brew install --cask --adopt wezterm\n"),
        "{calls}"
    );
    assert!(
        calls.contains("brew-env --cask pkgapp stdin=none askpass= nosudo=\n"),
        "{calls}"
    );
    assert!(
        calls.contains(
            "brew-env --cask --adopt wezterm stdin=none askpass=/usr/bin/false nosudo=\n"
        ),
        "{calls}"
    );
    assert!(
        calls.lines().any(|l| l.starts_with("installer -pkg x.pkg")),
        "{calls}"
    );
    assert_eq!(
        calls
            .matches("formulae.brew.sh/api/cask/pkgapp.json")
            .count(),
        1,
        "fetched once per run: {calls}"
    );

    // `-v`: satisfied actions (already installed) are listed only then.
    let out = env.dot(&["plan", "-v", "-c", bundle.to_str().unwrap()]);
    let t = text(&out);
    assert!(t.contains("brew: fd, jq installed"), "{t}");
    assert!(!t.contains("sudo"), "{t}");
}

/// A cask whose metadata does not say it runs sudo, but does: streamed, its sudo fails at once
/// (no password can be typed, or Homebrew's sudo is off) instead of waiting for one, unseen.
#[cfg(target_os = "macos")]
#[test]
fn a_cask_that_runs_sudo_unannounced_fails_fast() {
    let env = Env::new();
    env.script("brew", &BREW.replace("pkgapp)", "pkgapp|sneaky)"));
    env.script("installer", INSTALLER);
    let bundle = env.bundle("steps:\n  - package: { cask: [sneaky] }\n");
    let started = std::time::Instant::now();
    let out = env.apply(&bundle);
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "took {:?}",
        started.elapsed()
    );
    assert!(
        t.contains("Homebrew ran sudo for sneaky, although the cask's metadata does not say"),
        "{t}"
    );
    assert!(
        t.contains("run `brew install --cask sneaky` yourself"),
        "{t}"
    );
    let calls = env.calls();
    assert!(
        calls.contains("brew-env --cask --adopt sneaky stdin=none askpass=/usr/bin/false nosudo=1"),
        "{calls}"
    );
    assert!(!calls.contains("Password"), "{calls}");

    // Without Homebrew's NO_SUDO (Homebrew before 7.0.7 ignores it), the askpass that answers
    // nothing fails it as fast.
    env.script(
        "brew",
        &BREW.replace("pkgapp)", "pkgapp|sneaky)").replace(
            "echo \"brew $*\"",
            "unset HOMEBREW_NO_SUDO; echo \"brew $*\"",
        ),
    );
    let out = env.apply(&bundle);
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("Homebrew ran sudo for sneaky"), "{t}");
    assert!(env.calls().contains("sudo -A installer"), "{}", env.calls());
    assert!(
        !calls.lines().any(|l| l.starts_with("installer ")),
        "{calls}"
    );
}

/// Interactive: the `.pkg` cask gets the terminal (the live view paused), so the password
/// prompt of Homebrew's own sudo shows and is answered there; nothing asks for sudo up front.
/// Its output is still streamed (sudo opens `/dev/tty` itself): never raw on stdout.
#[cfg(target_os = "macos")]
fn pkg_cask_on_the_terminal(extra: &[&str]) {
    let env = Env::new();
    env.script("brew", BREW);
    env.script("installer", INSTALLER);
    let bundle = env.bundle("steps:\n  - package: { cask: [pkgapp, wezterm] }\n");
    let mut args = vec!["apply", "-c", bundle.to_str().unwrap()];
    args.extend_from_slice(extra);
    let mut pty = env.dot_on_pty(&args);
    let at = pty.wait_for("Apply?", 0);
    assert!(
        pty.text()
            .contains("pkgapp: Homebrew asks for the sudo password in the terminal"),
        "{}",
        pty.text()
    );
    pty.send("y\n");
    pty.wait_for("Password:", at);
    pty.send("hunter2\n");
    let status = pty.wait();
    assert!(status.success(), "{status}; screen:\n{}", pty.text());
    let calls = env.calls();
    assert!(
        calls.contains("brew-env --cask pkgapp stdin=none askpass= nosudo=\n"),
        "{calls}"
    );
    assert!(calls.contains("sudo: password typed"), "{calls}");
    assert!(
        calls.lines().any(|l| l.starts_with("installer -pkg x.pkg")),
        "{calls}"
    );
    assert!(
        calls
            .contains("brew-env --cask --adopt wezterm stdin=none askpass=/usr/bin/false nosudo=1"),
        "{calls}"
    );
    assert!(
        !calls.lines().any(|l| l == "sudo -v"),
        "nothing to ask for up front: {calls}"
    );
    // Why the terminal is handed over shows before the prompt.
    let screen = pty.text();
    let why = screen
        .find("Homebrew asks for your sudo password to install pkgapp")
        .unwrap_or_else(|| panic!("{screen}"));
    assert!(why < screen.rfind("Password:").unwrap(), "{screen}");
    // Brew's own lines went through dot (an event, a log line), not straight to the terminal.
    if extra.contains(&"--json") {
        assert!(
            !screen
                .lines()
                .any(|l| l.trim_start().starts_with("==> Installing --cask pkgapp")),
            "{screen}"
        );
        assert!(
            screen
                .lines()
                .any(|l| l.starts_with('{') && l.contains("==> Installing --cask pkgapp")),
            "{screen}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn pkg_cask_gets_the_terminal_tty_renderer() {
    pkg_cask_on_the_terminal(&[]);
}

#[cfg(target_os = "macos")]
#[test]
fn pkg_cask_gets_the_terminal_plain() {
    pkg_cask_on_the_terminal(&["--plain"]);
}

#[cfg(target_os = "macos")]
#[test]
fn pkg_cask_gets_the_terminal_json() {
    pkg_cask_on_the_terminal(&["--json"]);
}

#[cfg(target_os = "linux")]
fn debian_like() -> bool {
    fs::read_to_string("/etc/os-release").is_ok_and(|t| {
        t.lines().any(|l| {
            (l.starts_with("ID=") || l.starts_with("ID_LIKE="))
                && (l.contains("debian") || l.contains("ubuntu"))
        })
    })
}

#[cfg(target_os = "linux")]
#[test]
fn apt_with_sudo_and_daily_refresh() {
    if !debian_like() {
        return;
    }
    let env = Env::new();
    env.script("apt-get", APT_GET);
    env.script("apt", "#!/bin/sh\nexit 0\n");
    env.script("dpkg-query", DPKG_QUERY);
    fs::write(env.fake().join("apt"), "git\n").unwrap();
    let bundle = env.bundle("steps:\n  - package: [fd-find, git]\n");

    let out = env.plan(&bundle);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(
        t.contains("apt: install fd-find (already installed: git)"),
        "{t}"
    );

    // Non-interactive and sudo wants a password: fails before anything is installed.
    let out = env.apply(&bundle);
    assert!(!out.status.success());
    let t = text(&out);
    assert!(t.contains("sudo is needed by demo: "), "{t}");
    assert!(t.contains("dot is running non-interactively"), "{t}");
    assert!(!env.calls().contains("apt-get install"));

    fs::write(env.fake().join("sudo-ok"), "").unwrap();
    let out = env.apply(&bundle);
    assert!(out.status.success(), "{}", text(&out));
    let calls = env.calls();
    assert!(
        calls.contains("apt-get update -q (DEBIAN_FRONTEND=noninteractive)"),
        "{calls}"
    );
    assert!(
        calls.contains(" fd-find (DEBIAN_FRONTEND=noninteractive)"),
        "{calls}"
    );
    let state = fs::read_to_string(env.root.join("state/dot/state.json")).unwrap_or_default();
    assert!(
        state.contains("\"refreshed\""),
        "the stamp lives in the state store: {state}"
    );

    // Refreshed within the day: the next install does not refresh again.
    let bundle = env.bundle("steps:\n  - package: [curl]\n");
    fs::remove_file(env.fake().join("calls.log")).unwrap();
    let out = env.apply(&bundle);
    assert!(out.status.success(), "{}", text(&out));
    let calls = env.calls();
    assert!(!calls.contains("apt-get update"), "{calls}");
    assert!(calls.contains("apt-get install"), "{calls}");
}

#[test]
fn nothing_to_install_never_asks_for_sudo() {
    let env = Env::new();
    // On whatever this is, a step for another platform's manager does nothing.
    let other = if cfg!(target_os = "macos") {
        "apt"
    } else {
        "cask"
    };
    let bundle = env.bundle(&format!("steps:\n  - package: {{ {other}: [x] }}\n"));
    let out = env.apply(&bundle);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!env.calls().contains("sudo"), "{}", env.calls());
}

#[test]
fn bad_package_config_is_a_located_error() {
    let env = Env::new();
    let bundle = env.bundle("steps:\n  - package:\n      brew: [fd]\n      aptt: [fd]\n");
    let out = env.plan(&bundle);
    let t = text(&out);
    assert_eq!(out.status.code(), Some(2), "{t}");
    assert!(t.contains("bundle.yml:3:"), "{t}");
    assert!(t.contains("unknown package manager `aptt`"), "{t}");
}

/// Repo mode: the short form maps names through `dot.yml`'s `packages:` for this machine's
/// manager (the `pkg` fact, found on the fake `PATH`).
#[test]
fn short_form_uses_the_repo_name_map() {
    let env = Env::new();
    let expected = if cfg!(target_os = "macos") {
        env.script(
            "brew",
            "#!/bin/sh\necho \"brew $*\" >> \"$FAKE_LOG\"\nexit 1\n",
        );
        "brew: install fd-brew, jq"
    } else {
        let debian = fs::read_to_string("/etc/os-release")
            .is_ok_and(|t| t.contains("debian") || t.contains("ubuntu"));
        if !debian {
            return;
        }
        env.script("apt", "#!/bin/sh\nexit 0\n");
        env.script("apt-get", "#!/bin/sh\nexit 0\n");
        env.script("dpkg-query", "#!/bin/sh\nexit 1\n");
        "apt: install fd-apt, jq"
    };
    fs::write(
        env.root.join("repo/dot.yml"),
        "packages:\n  fd: { brew: fd-brew, apt: fd-apt }\n",
    )
    .unwrap();
    env.bundle("steps:\n  - package: [fd, jq]\n");
    let repo = env.root.join("repo");
    let out = env.dot(&["plan", "--repo", repo.to_str().unwrap()]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains(expected), "{t}");
}

/// Regression: a package manager installed during the run into a directory that only a `path:`
/// step puts on PATH (Homebrew on a fresh Apple-silicon Mac: `/opt/homebrew/bin` is not on the
/// default PATH) was looked up on dot's own PATH, so every later `package` step failed with
/// "brew is not installed". Programs are now looked up on the bundle's run environment.
#[test]
fn a_manager_installed_earlier_in_the_run_is_found_through_a_path_step() {
    let env = Env::new();
    let brew = r#"#!/bin/sh
echo "brew $*" >> "$FAKE_LOG"
case "$1" in
  list) shift 3; rc=0
    for p in "$@"; do grep -qx "$p" "$FAKE_STATE/brew" 2>/dev/null && echo "$p 1.0" || rc=1; done
    exit $rc ;;
  install) shift; for a in "$@"; do case "$a" in --*) ;; *) echo "$a" >> "$FAKE_STATE/brew" ;; esac; done ;;
esac
exit 0
"#;
    fs::write(env.fake().join("brew-script"), brew).unwrap();
    let bundle = env.bundle(concat!(
        "steps:\n",
        "  - label: Install Homebrew\n",
        "    shell:\n",
        "      cmd: mkdir -p ~/brew/bin && cp \"$FAKE_STATE/brew-script\" ~/brew/bin/brew && chmod +x ~/brew/bin/brew\n",
        "      unless: test -x ~/brew/bin/brew\n",
        "  - label: Homebrew on PATH\n",
        "    path: [~/brew/bin]\n",
        "  - label: Baseline\n",
        "    package: { brew: [fd] }\n",
    ));
    let facts = [(
        "DOT_FACTS",
        "os=macos,arch=aarch64,pkg=brew,role=desktop,init=launchd,distro=,family=,version=26.0,codename=,env=",
    )];
    let logs = env.root.join("logs");
    let args = [
        "apply",
        "--yes",
        "--plain",
        "-c",
        bundle.to_str().unwrap(),
        "--log-dir",
        logs.to_str().unwrap(),
    ];

    // brew is on no PATH dot was started with.
    let out = env.dot_with(&args, &facts);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(!t.contains("is not installed"), "{t}");
    let calls = env.calls();
    assert!(calls.contains("brew install --formula fd"), "{calls}");

    // The next run finds it the same way (the path step is satisfied by then).
    let out = env.dot_with(&args, &facts);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains(" 0 changed"), "{t}");
}

/// A child on a pseudo-terminal of its own: what it shows, and keys typed into it.
#[cfg(target_os = "macos")]
mod pty {
    use std::fs;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    pub struct Pty {
        child: Child,
        screen: Arc<Mutex<Vec<u8>>>,
        keys: fs::File,
    }

    impl Pty {
        pub fn spawn(mut c: Command) -> Self {
            let (mut master, mut slave) = (0, 0);
            // SAFETY: plain out-pointers; null name, termios and window size mean "defaults".
            let rc = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
            // SAFETY: both descriptors were just opened and are owned by nothing else.
            let (master, slave) =
                unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
            let io = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
            c.stdin(io(&slave)).stdout(io(&slave)).stderr(io(&slave));
            // SAFETY: only async-signal-safe calls between fork and exec: a new session, with
            // the terminal (stdin, already the slave side) as its controlling terminal.
            unsafe {
                c.pre_exec(|| {
                    if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = c.spawn().unwrap();
            let _ = slave.as_raw_fd();
            drop(slave);
            let screen = Arc::new(Mutex::new(Vec::new()));
            let mut reader = fs::File::from(master.try_clone().unwrap());
            let sink = screen.clone();
            std::thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = reader.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    sink.lock().unwrap().extend_from_slice(&chunk[..n]);
                }
            });
            Self {
                child,
                screen,
                keys: fs::File::from(master),
            }
        }

        pub fn text(&self) -> String {
            String::from_utf8_lossy(&self.screen.lock().unwrap()).into_owned()
        }

        /// Waits until the screen shows `what` (after `from` bytes); where it ends.
        pub fn wait_for(&mut self, what: &str, from: usize) -> usize {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let t = self.text();
                if let Some(i) = t.get(from..).and_then(|s| s.find(what)) {
                    return from + i + what.len();
                }
                if Instant::now() > deadline {
                    let _ = self.child.kill();
                    panic!("timed out waiting for {what:?}; screen:\n{t}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        pub fn send(&mut self, keys: &str) {
            self.keys.write_all(keys.as_bytes()).unwrap();
        }

        /// Waits for the child to exit (killing it after a timeout: it hangs).
        pub fn wait(&mut self) -> ExitStatus {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if let Some(s) = self.child.try_wait().unwrap() {
                    return s;
                }
                if Instant::now() > deadline {
                    let _ = self.child.kill();
                    panic!("hangs; screen:\n{}", self.text());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}
