//! `dot apply --host` / `dot plan --host` / `dot secrets push` end to end through the `dot`
//! binary, against fake `ssh` and `lpass` on a temp PATH: the "remote" is a temp directory used
//! as HOME by the command the fake ssh runs, with this `dot` binary pre-installed in its
//! `~/.local/bin`. No real ssh, no LastPass, no network. The fake ssh logs its argv and
//! environment and keeps a copy of everything fed to its stdin, so the tests can tell where each
//! value travelled. PATH holds only the fakes and links to the few tools the run needs, so the
//! test decides how the repository travels: by tar, or by rsync (this machine's, both ends, over
//! the fake ssh) when rsync is linked in. Unix-only: the fakes are shell scripts.
#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// In this machine's envfile.
const TOKEN: &str = "tok-host-9f3e-very-secret";
const OTHER: &str = "other-value-31c8-secret";
/// In (fake) LastPass only.
const LP_TOKEN: &str = "glpat-from-lastpass-77c1";
/// This machine's own value of a per-machine variable: never copied.
const MINE: &str = "laptop-only-machine-name";

const FAKE_SSH: &str = r#"#!/bin/sh
# Fake ssh: logs argv and environment, skips options, runs the command in the fake remote home,
# keeping a copy of what dot is handed on its stdin (anything but rsync's protocol, and not a
# terminal). Options that take a value are skipped
# with it: rsync 3 runs `ssh -l me box rsync --server ...` for `me@box:`, where openrsync
# (macOS) passes `me@box`.
printf 'ssh' >> "$FAKE_LOG"; for a; do printf ' [%s]' "$a" >> "$FAKE_LOG"; done; echo >> "$FAKE_LOG"
env >> "$FAKE_ENV_LOG"
while :; do
  case "$1" in
    -o|-O|-e|-p|-i|-l|-F|-J|-b|-c|-m|-S|-W) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
shift
cd "$FAKE_REMOTE" || exit 1
export HOME="$FAKE_REMOTE" XDG_STATE_HOME="$FAKE_REMOTE/.state"
case "$*" in rsync*) exec /bin/sh -c "$*" ;; esac
if [ -t 0 ]; then exec /bin/sh -c "$*"; fi
tee -a "$FAKE_STDIN" | /bin/sh -c "$*"
"#;

const FAKE_SCP: &str = r#"#!/bin/sh
# Fake scp: logs argv, copies sources into host:dir/ under the fake remote home, keeping modes.
printf 'scp' >> "$FAKE_LOG"; for a; do printf ' [%s]' "$a" >> "$FAKE_LOG"; done; echo >> "$FAKE_LOG"
while :; do
  case "$1" in
    -o) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
n=$#; i=0
for a; do i=$((i+1)); [ $i -eq $n ] && dest=$a; done
dir=${dest#*:}
case "$dir" in /*) target=$dir ;; *) target="$FAKE_REMOTE/$dir" ;; esac
i=0
for a; do i=$((i+1)); [ $i -lt $n ] && cp -p "$a" "$target"; done
exit 0
"#;

/// Fake `lpass`, logged in, with one item; logs each call with the HOME it ran under.
const FAKE_LPASS: &str = r#"#!/bin/sh
echo "HOME=$HOME $*" >> "$FAKE_LPASS_LOG"
case "$1" in
  status|sync) exit 0 ;;
  show) printf '%s\n' '[{"id":"42","name":"GitLab","fullname":"Dev/GitLab","group":"Dev","username":"me","password":"glpat-from-lastpass-77c1","note":""}]' ;;
  *) exit 1 ;;
esac
"#;

/// What the scenario runs besides `dot` and the fakes, on either end: linked from this machine
/// into the test's own PATH (those it has). rsync is not among them: see [`Env::with_rsync`].
const TOOLS: &[&str] = &[
    "sh", "git", "tar", "gzip", "uname", "mkdir", "cp", "mv", "rm", "chmod", "cat", "env", "id",
    "sw_vers", "tee",
];

/// `name` on this machine's PATH (or in `/usr/bin`, `/bin`).
fn host_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain([PathBuf::from("/usr/bin"), PathBuf::from("/bin")])
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

struct Env {
    _tmp: TempDir,
    root: PathBuf,
}

impl Env {
    /// Without rsync on either end: the repository goes as a tar stream.
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let e = Self { _tmp: tmp, root };
        e.write("repo/dot.yml", "");
        e.write(
            "repo/secrets.yml",
            "secrets:\n  tok: { ref: \"envfile:API_TOKEN\" }\n  glab: { ref: \"lastpass://Dev/GitLab#password\", fallback: \"envfile:GITLAB_TOKEN\" }\n  other: { ref: \"envfile:OTHER_TOKEN\" }\n  db: { ref: \"envfile:DB_PASS\", optional: true }\n  gone: { ref: \"envfile:GONE_TOKEN\" }\n",
        );
        e.write(
            "repo/bundles/app/bundle.yml",
            "steps:\n  - template:\n      ~/.config/app/token: token.j2\n",
        );
        e.write("repo/bundles/app/token.j2", "token={{ secret('tok') }}\n");
        e.write(
            "repo/bundles/lp/bundle.yml",
            "steps:\n  - template:\n      ~/.config/app/glab: glab.j2\n",
        );
        e.write("repo/bundles/lp/glab.j2", "{{ secret('glab') }}\n");
        e.write(
            "repo/bundles/who/bundle.yml",
            "vars:\n  EMAIL: { prompt: \"Your email\", persist: local }\n  MY_MACHINE: { prompt: \"Machine name\", persist: local, per_machine: true }\nsteps:\n  - template:\n      ~/.config/app/who: who.j2\n",
        );
        e.write("repo/bundles/who/who.j2", "{{ EMAIL }} {{ MY_MACHINE }}\n");
        e.write(
            "repo/bundles/opt/bundle.yml",
            "steps:\n  - template:\n      ~/.config/app/db: db.j2\n",
        );
        e.write("repo/bundles/opt/db.j2", "{{ secret('db') }}\n");
        e.write(
            "repo/bundles/needsgone/bundle.yml",
            "steps:\n  - template:\n      ~/.config/app/gone: gone.j2\n",
        );
        e.write("repo/bundles/needsgone/gone.j2", "{{ secret('gone') }}\n");
        e.write(
            "repo/bundles/plain/bundle.yml",
            "steps:\n  - create: [~/made]\n",
        );
        e.write(
            "repo/bundles/broken/bundle.yml",
            "steps:\n  - shell: exit 7\n",
        );
        e.write("repo/.gitignore", ".env.local\n");
        e.write("repo/.env.local", "LOCAL_ONLY=1\n");
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(e.p("repo"))
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        };
        git(&["init", "-q"]);
        // Ignored only through `.git/info/exclude`: stays here like `.env.local`.
        e.write("repo/.git/info/exclude", "infoexcl.txt\n");
        e.write("repo/infoexcl.txt", "LOCAL_SECRET=1\n");
        git(&["add", "-A"]);
        e.private(
            "home/.config/dot/secrets.env",
            &format!("API_TOKEN={TOKEN}\nOTHER_TOKEN={OTHER}\n"),
        );
        e.write(
            "home/.config/dot/local.env",
            &format!("EMAIL=me@example.com\nMY_MACHINE={MINE}\n"),
        );
        e.script("bin/ssh", FAKE_SSH);
        e.script("bin/scp", FAKE_SCP);
        e.script("bin/lpass", FAKE_LPASS);
        fs::create_dir_all(e.p("tools")).unwrap();
        fs::create_dir_all(e.p("tmp")).unwrap();
        for t in TOOLS {
            if let Some(p) = host_tool(t) {
                std::os::unix::fs::symlink(p, e.p("tools").join(t)).unwrap();
            }
        }
        // The remote already has this dot (a real remote would get it from install.sh).
        fs::create_dir_all(e.p("remote/.local/bin")).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_dot"), e.p("remote/.local/bin/dot")).unwrap();
        e
    }

    /// With this machine's rsync on both ends; `None` when it has none.
    fn with_rsync() -> Option<Self> {
        let rsync = host_tool("rsync")?;
        let e = Self::new();
        std::os::unix::fs::symlink(rsync, e.p("tools/rsync")).unwrap();
        Some(e)
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.p(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn private(&self, rel: &str, text: &str) {
        self.write(rel, text);
        fs::set_permissions(self.p(rel), fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.p(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn script(&self, rel: &str, text: &str) {
        self.write(rel, text);
        fs::set_permissions(self.p(rel), fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_dot"));
        c.args(args)
            .current_dir(self.p("repo"))
            .env("HOME", self.p("home"))
            .env_remove("DOT_STATE_DIR")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("DOT_RELEASE_BASE")
            .env_remove("DOT_RELEASE_REPO")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN")
            .env("XDG_STATE_HOME", self.p("state"))
            .env(
                "PATH",
                format!("{}:{}", self.p("bin").display(), self.p("tools").display()),
            )
            .env("TMPDIR", self.p("tmp"))
            .env("FAKE_LOG", self.p("ssh.log"))
            .env("FAKE_ENV_LOG", self.p("ssh-env.log"))
            .env("FAKE_STDIN", self.p("ssh-stdin.cap"))
            .env("FAKE_LPASS_LOG", self.p("lpass.log"))
            .env("FAKE_REMOTE", self.p("remote"))
            .env("DOT_ROLE", "server")
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", &self.root)
            .env_remove("DOT_REPO")
            .env_remove("DOT_HOST_BINARY")
            .env_remove("DOT_PROFILE")
            // The process environment overrides variables: none of the scenario's from here.
            .env_remove("EMAIL")
            .env_remove("MY_MACHINE");
        c
    }

    fn dot(&self, args: &[&str]) -> Output {
        self.command(args).stdin(Stdio::null()).output().unwrap()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.p("ssh.log")).unwrap_or_default()
    }

    /// Everything the fake ssh was fed on stdin, all runs together.
    fn stdin(&self) -> String {
        String::from_utf8_lossy(&fs::read(self.p("ssh-stdin.cap")).unwrap_or_default()).into_owned()
    }

    fn lpass_log(&self) -> String {
        fs::read_to_string(self.p("lpass.log")).unwrap_or_default()
    }

    fn reset_logs(&self) {
        for f in ["ssh.log", "ssh-env.log", "ssh-stdin.cap", "lpass.log"] {
            let _ = fs::remove_file(self.p(f));
        }
    }

    /// Fails when `needle` is in any file of the scenario but its source (this machine's
    /// secrets), the copy of ssh's stdin, and the `allowed` paths: not in the ssh argv or
    /// environment logs, not in a log or state file of either end, not in any remote file.
    fn assert_nowhere_but(&self, needle: &str, allowed: &[&str]) {
        let mut allowed: Vec<PathBuf> = allowed.iter().map(|a| self.p(a)).collect();
        allowed.extend(
            [
                "home/.config/dot/secrets.env",
                "home/.config/dot/local.env",
                "ssh-stdin.cap",
                "bin/lpass",
            ]
            .iter()
            .map(|a| self.p(a)),
        );
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let ty = entry.file_type().unwrap();
                if ty.is_symlink() {
                    continue;
                }
                if ty.is_dir() {
                    stack.push(path);
                    continue;
                }
                if allowed.contains(&path) || entry.metadata().unwrap().len() > 4 << 20 {
                    continue;
                }
                let bytes = fs::read(&path).unwrap_or_default();
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                    "`{needle}` is in {}",
                    path.display()
                );
            }
        }
    }
}

/// Every ssh that carries the stream on its stdin asks for no terminal and no escape character
/// there, whatever `~/.ssh/config` says.
fn assert_fed_without_a_terminal(log: &str) {
    let fed: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("--secrets-from -") || l.contains("secrets receive"))
        .collect();
    assert!(!fed.is_empty(), "{log}");
    for l in fed {
        assert!(
            l.starts_with("ssh [-T] [-o] [EscapeChar=none] "),
            "fed without -T: {l}"
        );
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn mode(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn apply_on_a_host_hands_over_what_its_bundles_need_for_that_run_only() {
    apply_on_a_host(&Env::new(), false);
}

#[test]
fn the_repo_goes_by_rsync_when_both_ends_have_it() {
    match Env::with_rsync() {
        Some(e) => apply_on_a_host(&e, true),
        None => eprintln!("no rsync on this machine: only the tar transport is tested"),
    }
}

/// `dot apply --host` of a bundle that needs a secret; the repository goes by rsync or by tar.
fn apply_on_a_host(e: &Env, rsync: bool) {
    let out = e.dot(&["apply", "--host", "me@box", "--yes", "--plain", "app"]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{stdout}\n{stderr}\n{}", e.log());
    // By the transport this PATH allows, and only that one.
    let log = e.log();
    if rsync {
        assert!(stderr.contains("(rsync)"), "{stderr}");
        assert!(
            log.contains("--server") && !log.contains("tar -xzf"),
            "{log}"
        );
    } else {
        assert!(stderr.contains("(tar over ssh)"), "{stderr}");
        assert!(
            log.contains("tar -xzf") && !log.contains("--server"),
            "{log}"
        );
    }
    // The repository went over, without what git ignores.
    assert!(e.p("remote/.dot/bundles/app/token.j2").is_file());
    assert!(e.p("remote/.dot/.git").is_dir());
    assert!(!e.p("remote/.dot/.env.local").exists(), "{}", e.log());
    assert!(!e.p("remote/.dot/infoexcl.txt").exists(), "{}", e.log());
    // Its bundles need `tok` (and only that): resolved here, handed to the run on ssh's stdin,
    // used there for that run.
    assert!(
        stderr.contains("dot: me@box: sending 1 secret (tok) on ssh's stdin (for that run only)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("sent 1 secret (tok) (for that run only; nothing kept there)"),
        "{stderr}"
    );
    assert_fed_without_a_terminal(&log);
    assert_eq!(
        e.read("remote/.config/app/token"),
        format!("token={TOKEN}\n")
    );
    assert!(e.stdin().contains("{\"t\":\"secret\",\"name\":\"tok\""));
    assert!(
        log.contains("apply --secrets-from - --repo"),
        "the run reads its stdin: {log}"
    );
    // Nothing kept there: no secrets file, no file secrets.
    assert!(!e.p("remote/.config/dot/secrets.env").exists());
    assert!(!e.p("remote/.config/dot/files").exists());
    // The remote's output streamed through; nothing ran here.
    assert!(stdout.contains("app"), "{stdout}");
    assert!(!e.p("home/.config/app/token").exists());
    // The value is on no command line, in no environment, log or remote file but the one the
    // bundle writes.
    e.assert_nowhere_but(TOKEN, &["remote/.config/app/token"]);
    // Nothing else was sent: only what the bundles use.
    assert!(!e.stdin().contains("\"name\":\"other\""));
    e.assert_nowhere_but(OTHER, &[]);
}

#[test]
fn exit_codes_come_back_and_nothing_is_handed_over_when_nothing_is_needed() {
    let e = Env::new();
    let out = e.dot(&["apply", "--host", "me@box", "--yes", "-j", "2", "plain"]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}\n{}", e.log());
    // `--jobs` goes to the remote run.
    assert!(
        e.log().contains("--jobs 2") || e.log().contains("[--jobs] [2]"),
        "{}",
        e.log()
    );
    assert!(e.p("remote/made").is_dir());
    assert!(stderr.contains("nothing to send"), "{stderr}");
    assert!(!e.log().contains("--secrets-from"), "{}", e.log());
    assert!(!e.p("remote/.config/dot/secrets.env").exists());
    assert!(!e.log().contains("scp"), "{}", e.log());

    let out = e.dot(&["apply", "--host", "me@box", "--yes", "broken"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));

    // `dot plan --host` plans there and changes nothing; it is handed no secret.
    fs::remove_dir(e.p("remote/made")).unwrap();
    e.reset_logs();
    let out = e.dot(&["plan", "--host", "me@box", "plain", "app"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", text(&out.stderr));
    assert!(stdout.contains("create ~/made"), "{stdout}");
    assert!(!e.p("remote/made").exists());
    assert!(!e.stdin().contains("\"t\":\"secret\""), "{}", e.stdin());

    // Local-only inputs are refused.
    let out = e.dot(&["apply", "--host", "me@box", "--env-file", "x.env"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("--env-file"),
        "{}",
        text(&out.stderr)
    );
    let out = e.dot(&["apply", "--host", "-oProxyCommand=x"]);
    assert!(!out.status.success());
    // The hidden flags are for the remote end only.
    let out = e.dot(&["apply", "--host", "me@box", "--secrets-from", "-"]);
    assert_eq!(out.status.code(), Some(2));
    let out = e.dot(&["apply", "--persist-secrets"]);
    assert_eq!(out.status.code(), Some(2), "needs --host");
}

#[test]
fn a_lastpass_secret_is_resolved_here_and_the_remote_needs_no_password_manager() {
    let e = Env::new();
    let out = e.dot(&["apply", "--host", "me@box", "--yes", "lp"]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}\n{}", e.log());
    assert_eq!(e.read("remote/.config/app/glab"), format!("{LP_TOKEN}\n"));
    // `lpass` ran here only, once logged in: never under the remote's HOME.
    let lp = e.lpass_log();
    let here = format!("HOME={} ", e.p("home").display());
    assert!(lp.contains("show"), "{lp}");
    assert!(lp.lines().all(|l| l.starts_with(&here)), "{lp}");
    e.assert_nowhere_but(LP_TOKEN, &["remote/.config/app/glab"]);
}

#[test]
fn per_machine_variables_are_never_copied() {
    let e = Env::new();
    // No terminal here to ask on, no --var: stops before anything is synced.
    let out = e.dot(&["apply", "--host", "me@box", "--yes", "who"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr
            .contains("MY_MACHINE is per-machine (`per_machine: true`) and has no value on me@box")
            && stderr.contains("give --var MY_MACHINE=<value>"),
        "{stderr}"
    );
    assert!(!e.p("remote/.dot").exists());

    // Given with --var: forwarded with the other personal variable, on stdin only.
    e.reset_logs();
    let out = e.dot(&[
        "apply",
        "--host",
        "me@box",
        "--yes",
        "--var",
        "MY_MACHINE=box-one",
        "who",
    ]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}\n{}", e.log());
    assert_eq!(e.read("remote/.config/app/who"), "me@example.com box-one\n");
    assert!(
        stderr.contains("sending 2 variables (EMAIL, MY_MACHINE) on ssh's stdin"),
        "{stderr}"
    );
    assert!(!e.log().contains("box-one"), "{}", e.log());
    assert!(!e.log().contains("me@example.com"), "{}", e.log());
    // Nothing kept there without --persist-secrets.
    assert!(!e.p("remote/.config/dot/local.env").exists());
    e.assert_nowhere_but(MINE, &[]);

    // Set there already: its own value is used, nothing asked.
    e.write("remote/.config/dot/local.env", "MY_MACHINE=already-there\n");
    let out = e.dot(&["apply", "--host", "me@box", "--yes", "who"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        e.read("remote/.config/app/who"),
        "me@example.com already-there\n"
    );
    e.assert_nowhere_but(MINE, &[]);

    // Any variable the remote's local.env sets is its own: this machine's is not forwarded
    // over it (an explicit --var is).
    e.write(
        "remote/.config/dot/local.env",
        "MY_MACHINE=already-there\nEMAIL=work@corp.com\n",
    );
    e.reset_logs();
    let out = e.dot(&["apply", "--host", "me@box", "--yes", "who"]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        e.read("remote/.config/app/who"),
        "work@corp.com already-there\n"
    );
    assert!(!e.stdin().contains("me@example.com"), "{}", e.stdin());
    assert!(stderr.contains("nothing to send"), "{stderr}");
    let out = e.dot(&[
        "apply",
        "--host",
        "me@box",
        "--yes",
        "--var",
        "EMAIL=given@here",
        "who",
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        e.read("remote/.config/app/who"),
        "given@here already-there\n"
    );
}

#[test]
fn persist_secrets_keeps_them_owner_only_and_merges() {
    let e = Env::new();
    let run = |extra: &[&str]| {
        let mut args = vec![
            "apply",
            "--host",
            "me@box",
            "--yes",
            "--persist-secrets",
            "--var",
            "MY_MACHINE=box-one",
        ];
        args.extend_from_slice(extra);
        args.extend(["app", "who"]);
        let out = e.dot(&args);
        let stderr = text(&out.stderr);
        assert!(out.status.success(), "{stderr}\n{}", e.log());
        stderr
    };
    let stderr = run(&[]);
    // Said by the remote, naming it; only the places written to.
    assert!(
        stderr.contains("dot: me@box: secrets kept in ~/.config/dot/secrets.env: added tok\n"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "dot: me@box: variables kept in ~/.config/dot/local.env: added EMAIL, MY_MACHINE"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("(kept there)"), "{stderr}");
    let kept = e.p("remote/.config/dot/secrets.env");
    assert_eq!(mode(&kept), 0o600);
    assert!(e.read("remote/.config/dot/secrets.env").contains(TOKEN));
    let local = e.read("remote/.config/dot/local.env");
    assert!(
        local.contains("EMAIL") && local.contains("box-one") && !local.contains(MINE),
        "{local}"
    );

    // Another value here: the remote keeps its own unless forced, and the run uses that one
    // (a later run there on its own would anyway). Its EMAIL is its own now: not sent again.
    let newer = "tok-rotated-5521-secret";
    e.private(
        "home/.config/dot/secrets.env",
        &format!("API_TOKEN={newer}\nOTHER_TOKEN={OTHER}\n"),
    );
    let stderr = run(&[]);
    assert!(
        stderr.contains(
            "kept tok (a different value is here, and this run uses it; --force-secrets \
             replaces it)"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("local.env: unchanged MY_MACHINE"),
        "{stderr}"
    );
    assert!(e.read("remote/.config/dot/secrets.env").contains(TOKEN));
    assert_eq!(
        e.read("remote/.config/app/token"),
        format!("token={TOKEN}\n")
    );
    let stderr = run(&["--force-secrets"]);
    assert!(stderr.contains("updated tok"), "{stderr}");
    assert!(stderr.contains("unchanged EMAIL, MY_MACHINE"), "{stderr}");
    assert_eq!(
        e.read("remote/.config/app/token"),
        format!("token={newer}\n")
    );
    assert!(e.read("remote/.config/dot/secrets.env").contains(newer));
    assert_eq!(mode(&kept), 0o600);

    // Later runs there work on their own.
    let out = Command::new(e.p("remote/.local/bin/dot"))
        .args(["apply", "--yes", "--force", "app", "who"])
        .current_dir(e.p("remote"))
        .env("HOME", e.p("remote"))
        .env("XDG_STATE_HOME", e.p("remote/.state"))
        .env("PATH", e.p("tools"))
        .env("DOT_REPO", e.p("remote/.dot"))
        .env("DOT_ROLE", "server")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("DOT_PROFILE")
        .env_remove("EMAIL")
        .env_remove("MY_MACHINE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(e.read("remote/.config/app/who"), "me@example.com box-one\n");
    e.assert_nowhere_but(
        newer,
        &["remote/.config/dot/secrets.env", "remote/.config/app/token"],
    );
}

#[test]
fn all_secrets_or_none() {
    let e = Env::new();
    let out = e.dot(&[
        "apply",
        "--host",
        "me@box",
        "--yes",
        "--all-secrets",
        "--persist-secrets",
        "plain",
    ]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}\n{}", e.log());
    // Every declared secret that resolves here, wherever it lives (glab: LastPass).
    assert!(
        stderr.contains("sending 3 secrets (glab, other, tok)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("not sent (not resolved here): db (") && stderr.contains("gone ("),
        "{stderr}"
    );
    let kept = e.read("remote/.config/dot/secrets.env");
    for v in [TOKEN, OTHER, LP_TOKEN] {
        assert!(kept.contains(v), "{v}");
    }
    assert!(
        kept.contains("GITLAB_TOKEN="),
        "kept under its envfile fallback"
    );

    // None: the remote resolves its own.
    let e = Env::new();
    e.private(
        "remote/.config/dot/secrets.env",
        "API_TOKEN=the-remote-own\n",
    );
    let out = e.dot(&["apply", "--host", "me@box", "--yes", "--no-secrets", "app"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(e.read("remote/.config/app/token"), "token=the-remote-own\n");
    assert!(!e.stdin().contains("\"t\":\"secret\""), "{}", e.stdin());
    e.assert_nowhere_but(TOKEN, &[]);
    assert!(e.lpass_log().is_empty());
}

#[test]
fn an_optional_secret_is_skipped_and_a_missing_one_stops_everything() {
    let e = Env::new();
    let out = e.dot(&["apply", "--host", "me@box", "--yes", "opt"]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("optional secret db is not available here"),
        "{stderr}"
    );
    assert!(!stderr.contains("nothing to send"), "{stderr}");
    assert!(!e.p("remote/.config/app/db").exists());

    let e = Env::new();
    let out = e.dot(&["apply", "--host", "me@box", "--yes", "needsgone", "app"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("1 secret the run on me@box needs could not be resolved here")
            && stderr.contains("gone: `envfile:GONE_TOKEN` not found"),
        "{stderr}"
    );
    // Before anything was synced or sent.
    assert!(!e.p("remote/.dot").exists());
    assert!(e.stdin().is_empty(), "{}", e.stdin());
}

#[test]
fn a_dry_run_installs_syncs_and_sends_nothing() {
    let e = Env::new();
    fs::remove_file(e.p("remote/.local/bin/dot")).unwrap();
    let out = e.dot(&[
        "apply",
        "--host",
        "me@box",
        "--dry-run",
        "--var",
        "MY_MACHINE=box-one",
        "app",
        "who",
        "lp",
    ]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}\n{}", e.log());
    for said in [
        "dry run: nothing is installed, synced, sent or applied there",
        "would install dot for",
        "would resolve here and send 2 secrets: glab, tok (nothing is resolved now)",
        "would forward 2 variables: EMAIL, MY_MACHINE",
        "would run `dot apply --repo",
    ] {
        assert!(stderr.contains(said), "{said}: {stderr}");
    }
    // The command a real run would use.
    let would = stderr.lines().find(|l| l.contains("would run")).unwrap();
    assert!(!would.contains("--dry-run"), "{would}");
    // Without a terminal, a missing per-machine value would stop a real run.
    let out = e.dot(&["apply", "--host", "me@box", "--dry-run", "who"]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains(
            "per-machine variable without a value there: MY_MACHINE (a real run would stop: no \
             terminal to ask on; give --var MY_MACHINE=<value>)"
        ),
        "{stderr}"
    );
    assert!(!e.p("remote/.local/bin/dot").exists());
    assert!(!e.p("remote/.dot").exists());
    assert!(!e.p("remote/.config").exists());
    assert!(e.stdin().is_empty(), "{}", e.stdin());
    assert!(
        e.lpass_log().is_empty(),
        "nothing resolved: {}",
        e.lpass_log()
    );
}

#[test]
fn secrets_push_resolves_here_and_keeps_them_there() {
    let e = Env::new();
    let out = e.dot(&["secrets", "push", "me@box", "--dry-run"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{}", text(&out.stderr));
    for line in [
        "tok -> envfile:API_TOKEN",
        "glab -> envfile:GITLAB_TOKEN",
        "nothing was resolved or sent",
    ] {
        assert!(stdout.contains(line), "{line}: {stdout}");
    }
    assert!(!e.p("remote/.config").exists());
    assert!(e.lpass_log().is_empty());

    let out = e.dot(&["secrets", "push", "me@box", "tok", "glab"]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{stdout}\n{stderr}\n{}", e.log());
    assert!(
        stderr.contains("dot: me@box: pushing 2 secrets (glab, tok) on ssh's stdin\n"),
        "{stderr}"
    );
    assert!(
        stderr.contains("dot: me@box: secrets kept in ~/.config/dot/secrets.env: added glab, tok"),
        "{stderr}"
    );
    assert_fed_without_a_terminal(&e.log());
    assert!(e.log().contains("host-info"), "{}", e.log());
    let kept = e.read("remote/.config/dot/secrets.env");
    assert!(kept.contains(TOKEN) && kept.contains(LP_TOKEN), "pushed");
    assert!(!kept.contains(OTHER), "only the named ones");
    assert_eq!(mode(&e.p("remote/.config/dot/secrets.env")), 0o600);
    assert_eq!(mode(&e.p("remote/.config/dot")), 0o700);
    assert!(e.log().contains("secrets receive"), "{}", e.log());
    e.assert_nowhere_but(TOKEN, &["remote/.config/dot/secrets.env"]);
    e.assert_nowhere_but(LP_TOKEN, &["remote/.config/dot/secrets.env"]);

    // Again: nothing changes. A name that does not resolve here stops it, in push's words.
    let out = e.dot(&["secrets", "push", "me@box", "tok", "glab"]);
    assert!(
        text(&out.stderr).contains("unchanged glab, tok"),
        "{}",
        text(&out.stderr)
    );
    let out = e.dot(&["secrets", "push", "me@box", "gone"]);
    let stderr = text(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("1 secret could not be resolved here, so nothing was pushed to me@box"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("--no-secrets") && !stderr.contains("the run"),
        "{stderr}"
    );
    // A different value there: kept, in push's words.
    e.private(
        "remote/.config/dot/secrets.env",
        &format!("API_TOKEN=remote-own\nGITLAB_TOKEN={LP_TOKEN}\n"),
    );
    let out = e.dot(&["secrets", "push", "me@box", "tok"]);
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("kept tok (a different value is here; --force replaces it)"),
        "{stderr}"
    );
    let out = e.dot(&["secrets", "push", "me@box", "tok", "--force"]);
    assert!(
        text(&out.stderr).contains("updated tok"),
        "{}",
        text(&out.stderr)
    );
    // All of them: what does not resolve is skipped.
    let out = e.dot(&["secrets", "push", "me@box"]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("not sent (not resolved here): db"),
        "{stderr}"
    );
    assert!(e.read("remote/.config/dot/secrets.env").contains(OTHER));
}

// ---------------------------------------------------------------------------------------------
// On a terminal: the per-machine variable is asked for here, the run there confirms on the
// terminal, and what is handed over goes through `dot secrets relay` and a FIFO.

fn openpty() -> (OwnedFd, OwnedFd) {
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
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

/// Everything written to the terminal so far.
struct Screen(Arc<Mutex<Vec<u8>>>);

impl Screen {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    fn wait_for(&self, what: &str, from: usize) -> usize {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let t = self.text();
            if let Some(i) = t.get(from..).and_then(|s| s.find(what)) {
                return from + i + what.len();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what:?}; screen:\n{t}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// `dot <args>` on a pseudo-terminal: `script` answers each prompt (waited for, in order) with
/// a line. Returns its exit status and everything written to the terminal.
fn on_a_terminal(
    e: &Env,
    args: &[&str],
    script: &[(&str, &str)],
) -> (std::process::ExitStatus, String) {
    let (master, slave) = openpty();
    let io = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
    let mut child = e
        .command(args)
        .env("TERM", "dumb")
        .stdin(io(&slave))
        .stdout(io(&slave))
        .stderr(io(&slave))
        .spawn()
        .unwrap();
    drop(slave);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut reader = fs::File::from(master.try_clone().unwrap());
    let sink = buf.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    });
    let screen = Screen(buf);
    let mut keys = fs::File::from(master);
    let mut at = 0;
    for (prompt, answer) in script {
        at = screen.wait_for(prompt, at);
        keys.write_all(answer.as_bytes()).unwrap();
        keys.write_all(b"\n").unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("hangs; screen:\n{}\n{}", screen.text(), e.log());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // What is still in flight to the screen.
    std::thread::sleep(Duration::from_millis(100));
    (status, screen.text())
}

#[test]
fn on_a_terminal_it_asks_here_and_hands_over_through_a_fifo() {
    let e = Env::new();
    let (status, screen) = on_a_terminal(
        &e,
        &["apply", "--host", "me@box", "--plain", "app", "who"],
        &[
            ("Value of MY_MACHINE for me@box (Machine name): ", "box-two"),
            // The run there, on this terminal, confirms.
            ("Apply?", "y"),
        ],
    );
    assert!(status.success(), "{status}; screen:\n{screen}\n{}", e.log());
    assert_eq!(e.read("remote/.config/app/who"), "me@example.com box-two\n");
    assert_eq!(
        e.read("remote/.config/app/token"),
        format!("token={TOKEN}\n")
    );
    // Through the relay's FIFO: the run was given its path, never the values; nothing is left.
    let log = e.log();
    assert!(log.contains("secrets relay"), "{log}");
    assert!(log.contains("apply --secrets-from /"), "{log}");
    assert!(!log.contains("--secrets-from -"), "{log}");
    assert!(
        log.lines()
            .filter(|l| l.contains("secrets relay"))
            .all(|l| l.starts_with("ssh [-T] [-o] [EscapeChar=none] ")),
        "{log}"
    );
    assert!(
        fs::read_dir(e.p("tmp")).unwrap().next().is_none(),
        "left behind: {:?}",
        fs::read_dir(e.p("tmp")).unwrap().collect::<Vec<_>>()
    );
    assert!(!screen.contains(TOKEN), "{screen}");
    e.assert_nowhere_but(TOKEN, &["remote/.config/app/token"]);
    assert!(!log.contains("box-two"), "{log}");
    // The per-machine answer (`persist: local`) is kept there, as a local run keeps it; EMAIL,
    // this machine's, is not.
    assert_eq!(
        e.read("remote/.config/dot/local.env"),
        "MY_MACHINE=\"box-two\"\n"
    );
    assert!(
        screen.contains("variables kept in ~/.config/dot/local.env: added MY_MACHINE"),
        "{screen}"
    );
    assert!(!e.p("remote/.config/dot/secrets.env").exists());
    // So the next run, without a terminal, needs no --var.
    let out = e.dot(&["apply", "--host", "me@box", "--yes", "who"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(e.read("remote/.config/app/who"), "me@example.com box-two\n");
}

#[test]
fn declining_the_run_there_keeps_nothing() {
    let e = Env::new();
    let (status, screen) = on_a_terminal(
        &e,
        &[
            "apply",
            "--host",
            "me@box",
            "--plain",
            "--persist-secrets",
            "--var",
            "MY_MACHINE=box-one",
            "app",
            "who",
        ],
        &[("Apply?", "n")],
    );
    assert!(!status.success(), "{screen}");
    assert!(!e.p("remote/.config/dot/secrets.env").exists(), "{screen}");
    assert!(!e.p("remote/.config/dot/local.env").exists(), "{screen}");
    assert!(!e.p("remote/.config/app/token").exists());
    assert!(
        screen.contains("the run there did not succeed: kept only if it got past its confirmation"),
        "{screen}"
    );
}

#[test]
fn a_profile_the_remote_reports_is_confirmed_here() {
    let e = Env::new();
    e.write(
        "repo/dot.yml",
        "profiles:\n  personal:\n    match: {}\n    bundles: [app]\n  work:\n    bundles: [app, work]\n",
    );
    e.write(
        "repo/bundles/work/bundle.yml",
        "steps:\n  - template:\n      ~/.config/app/work: work.j2\n",
    );
    e.write("repo/bundles/work/work.j2", "{{ secret('other') }}\n");
    // A remote that says it is pinned to `work`.
    e.write("remote/.config/dot/local.env", "DOT_PROFILE=work\n");
    let out = e.dot(&["apply", "--host", "me@box", "--yes"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("profile `work` is the one me@box reports (pinned there; its facts: os=")
            && stderr.contains("what is sent: 2 secrets (other, tok)")
            && stderr.contains("give --profile work to send them (or --no-secrets)"),
        "{stderr}"
    );
    // Before anything was resolved, synced or sent.
    assert!(e.stdin().is_empty(), "{}", e.stdin());
    assert!(!e.p("remote/.dot").exists());
    e.assert_nowhere_but(OTHER, &[]);

    // Named here: sent, and the run there is given that profile.
    let out = e.dot(&["apply", "--host", "me@box", "--yes", "--profile", "work"]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}\n{}", e.log());
    assert!(
        stderr.contains("sending 2 secrets (other, tok)"),
        "{stderr}"
    );
    assert_eq!(e.read("remote/.config/app/work"), format!("{OTHER}\n"));

    // Not pinned there: chosen here from its facts, and the run is given the same one.
    e.write("remote/.config/dot/local.env", "");
    e.reset_logs();
    let out = e.dot(&["apply", "--host", "me@box", "--yes"]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}\n{}", e.log());
    assert!(stderr.contains("sending 1 secret (tok)"), "{stderr}");
    assert!(!e.stdin().contains("\"name\":\"other\""));
    assert!(
        e.log().contains("apply --secrets-from - --repo ")
            && e.log().contains(" --profile personal --yes"),
        "{}",
        e.log()
    );

    // On a terminal: asked here first; no is no.
    e.write("remote/.config/dot/local.env", "DOT_PROFILE=work\n");
    e.reset_logs();
    let (status, screen) = on_a_terminal(
        &e,
        &["apply", "--host", "me@box", "--plain"],
        &[("Send them to me@box? [y/N] ", "n")],
    );
    assert!(!status.success(), "{screen}");
    assert!(
        screen.contains("cancelled: nothing was resolved, synced or sent"),
        "{screen}"
    );
    assert!(e.stdin().is_empty(), "{}", e.stdin());
    assert!(!e.log().contains("relay"), "{}", e.log());
}

#[test]
fn only_this_tool_recent_enough_is_handed_anything() {
    // An impostor first on the remote's PATH (graphviz's `dot`): skipped for ~/.local/bin/dot.
    let e = Env::new();
    e.script(
        "bin/dot",
        "#!/bin/sh\necho \"$*\" >> \"$FAKE_REMOTE/../impostor.log\"\ncase \"$1\" in --version) echo 'dot - graphviz version 2.43.0' ;; *) cat ;; esac\n",
    );
    let out = e.dot(&["secrets", "push", "me@box", "tok"]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}\n{}", e.log());
    assert_eq!(e.read("impostor.log"), "--version\n");
    assert!(e.read("remote/.config/dot/secrets.env").contains(TOKEN));

    // One that claims to be dot: found out before anything is resolved here.
    let e = Env::new();
    e.script(
        "bin/dot",
        concat!(
            "#!/bin/sh\ncase \"$1\" in --version) echo 'dot ",
            env!("CARGO_PKG_VERSION"),
            "' ;; *) cat ;; esac\n"
        ),
    );
    let out = e.dot(&["secrets", "push", "me@box", "tok", "glab"]);
    let stderr = text(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("is not this dot"), "{stderr}");
    assert!(e.lpass_log().is_empty(), "{}", e.lpass_log());
    assert!(e.stdin().is_empty(), "{}", e.stdin());
    assert!(!stderr.contains(TOKEN), "{stderr}");

    // An older dot there: said so, with the one thing to do, before anything is resolved.
    let e = Env::new();
    e.script(
        "remote/.local/bin/dot",
        "#!/bin/sh\ncase \"$1\" in --version) echo 'dot 0.0.1' ;; *) echo \"error: unrecognized subcommand '$1'\" >&2; exit 2 ;; esac\n",
    );
    for args in [
        &["secrets", "push", "me@box", "glab"][..],
        &["apply", "--host", "me@box", "--yes", "--no-secrets", "app"][..],
        &["plan", "--host", "me@box", "app"][..],
    ] {
        let out = e.dot(args);
        let stderr = text(&out.stderr);
        assert!(!out.status.success(), "{args:?}: {stderr}");
        assert!(
            stderr.contains("dot 0.0.1 at ")
                && stderr.contains("is older than this one")
                && stderr.contains("/.local/bin/dot self-update`"),
            "{args:?}: {stderr}"
        );
        assert!(!stderr.contains("--no-secrets"), "{args:?}: {stderr}");
    }
    assert!(e.lpass_log().is_empty(), "{}", e.lpass_log());
    assert!(e.stdin().is_empty(), "{}", e.stdin());
    assert!(!e.p("remote/.dot").exists());
}

#[test]
fn the_relay_gives_up_when_its_session_is_gone() {
    let e = Env::new();
    let mut relay = e
        .command(&["secrets", "relay"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = relay.stdin.take().unwrap();
    stdin.write_all(b"handed-over-bytes").unwrap();
    drop(stdin);
    let mut out = relay.stdout.take().unwrap();
    let mut first = [0u8; 1];
    out.read_exact(&mut first).unwrap();
    assert_eq!(first[0], b'/', "it prints the FIFO's path");
    assert!(fs::read_dir(e.p("tmp")).unwrap().next().is_some());
    // Nobody opens the FIFO, and the session is gone.
    drop(out);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = relay.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "the relay waits on");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
    let mut err = String::new();
    relay
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(
        err.contains("the session that started this relay is gone"),
        "{err}"
    );
    assert!(
        fs::read_dir(e.p("tmp")).unwrap().next().is_none(),
        "the FIFO and its directory are removed"
    );
}

#[test]
fn cloud_init_user_data_embeds_secrets_only_when_asked() {
    let e = Env::new();
    let args = [
        "cloud-init",
        "--repo",
        "https://github.com/me/dotfiles.git",
        "--profile",
        "ubuntu-server",
    ];
    let out = e.dot(&args);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(stdout.starts_with("#cloud-config\n"), "{stdout}");
    assert!(stdout.contains("DOT_APPLY_ARGS='--yes --profile ubuntu-server'"));
    // This dot's release, and install.sh from its tag.
    let v = format!("v{}", env!("CARGO_PKG_VERSION"));
    assert!(
        stdout.contains(&format!("/dot-cli/{v}/install.sh")),
        "{stdout}"
    );
    assert!(stdout.contains(&format!("DOT_VERSION='{v}'")), "{stdout}");
    assert!(!stdout.contains("write_files"), "{stdout}");
    let mut latest = args.to_vec();
    latest.push("--latest");
    let out = e.dot(&latest);
    let stdout = text(&out.stdout);
    assert!(stdout.contains("/dot-cli/main/install.sh"), "{stdout}");
    assert!(!stdout.contains("DOT_VERSION"), "{stdout}");
    assert!(text(&out.stderr).is_empty());

    // The default secrets file, embedded (base64), with a warning on both sides.
    let mut with = args.to_vec();
    with.push("--secrets-from-file");
    let out = e.dot(&with);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(stdout.contains("write_files:"), "{stdout}");
    assert!(stdout.contains("permissions: \"0600\""), "{stdout}");
    assert!(stdout.contains("# WARNING: this user data holds secrets"));
    assert!(
        !stdout.contains(TOKEN),
        "embedded as base64, not in the clear"
    );
    assert!(text(&out.stderr).contains("warning: this user data holds your secrets"));

    let out = e.dot(&["cloud-init", "--repo", "http://x/y", "--profile", "p"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("https://"),
        "{}",
        text(&out.stderr)
    );
}

// ---------------------------------------------------------------------------------------------
// --dot-binary / DOT_HOST_BINARY: dot is installed (or upgraded) there from a file here, over
// scp, and nothing is downloaded.

/// The scenario with no dot on the remote, a dot of this version in `build/dot` here, and every
/// way of downloading one a dead end that leaves a trace: a GitHub token and all of dot's own
/// requests rewritten to a closed port, `curl` and `wget` that log and fail. With this machine's
/// `sha256sum` and `shasum` (those it has), to tell one build from another there.
fn without_dot_there() -> Env {
    let e = Env::new();
    for t in SUM_TOOLS {
        if let Some(p) = host_tool(t) {
            std::os::unix::fs::symlink(p, e.p("tools").join(t)).unwrap();
        }
    }
    fs::remove_file(e.p("remote/.local/bin/dot")).unwrap();
    fs::create_dir_all(e.p("build")).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_dot"), e.p("build/dot")).unwrap();
    for tool in ["curl", "wget"] {
        e.script(
            &format!("bin/{tool}"),
            &format!("#!/bin/sh\necho \"{tool} $*\" >> \"$FAKE_REMOTE/../net.log\"\nexit 7\n"),
        );
    }
    e
}

const SUM_TOOLS: [&str; 2] = ["sha256sum", "shasum"];

/// Whether the remote can say a file's SHA-256 (see [`without_dot_there`]).
fn sums_there() -> bool {
    SUM_TOOLS.iter().any(|t| host_tool(t).is_some())
}

fn offline(e: &Env, args: &[&str], binary_env: Option<&Path>) -> Output {
    let mut c = e.command(args);
    c.env("GITHUB_TOKEN", "ghp_never_used").env(
        "DOT_URL_REWRITE",
        "https://=http://127.0.0.1:9/,http://=http://127.0.0.1:9/",
    );
    if let Some(b) = binary_env {
        c.env("DOT_HOST_BINARY", b);
    }
    c.stdin(Stdio::null()).output().unwrap()
}

/// Nothing went to the network: no curl or wget, no install.sh, no request of dot's own (it
/// would have failed the run).
fn assert_offline(e: &Env) {
    assert!(!e.p("net.log").exists(), "{}", e.read("net.log"));
    assert!(!e.log().contains("[sh -s]"), "{}", e.log());
}

#[test]
fn dot_binary_installs_dot_there_over_scp_and_downloads_nothing() {
    let e = without_dot_there();
    let build = e.p("build/dot");
    let flag = build.display().to_string();
    let out = offline(
        &e,
        &["secrets", "push", "me@box", "tok", "--dot-binary", &flag],
        None,
    );
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}\n{}", e.log());
    assert_offline(&e);
    let log = e.log();
    let installed = e.p("remote/.local/bin/dot");
    let tmp = e.p("remote/.local/bin/.dot.tmp");
    assert!(
        log.contains(&format!(
            "[-q] [{}] [me@box:{}]",
            build.display(),
            tmp.display()
        )),
        "{log}"
    );
    assert!(
        log.contains(&format!("{} --version]", tmp.display())),
        "checked there before the rename: {log}"
    );
    assert!(
        log.contains(&format!(
            "[mv -f {} {}]",
            tmp.display(),
            installed.display()
        )),
        "{log}"
    );
    assert!(!tmp.exists());
    assert_eq!(mode(&installed), 0o755);
    assert_eq!(fs::read(&installed).unwrap(), fs::read(&build).unwrap());
    assert!(stderr.contains("dot: me@box: copying "), "{stderr}");
    // The push itself went through that dot.
    assert!(e.read("remote/.config/dot/secrets.env").contains(TOKEN));

    // DOT_HOST_BINARY, with apply and plan: this version is there now, so nothing is copied.
    for args in [
        &[
            "apply",
            "--host",
            "me@box",
            "--yes",
            "--no-secrets",
            "plain",
        ][..],
        &["plan", "--host", "me@box", "plain"][..],
    ] {
        e.reset_logs();
        let out = offline(&e, args, Some(&build));
        let stderr = text(&out.stderr);
        assert!(out.status.success(), "{args:?}: {stderr}\n{}", e.log());
        assert!(!e.log().contains("scp"), "{args:?}: {}", e.log());
        assert!(
            stderr.contains(" already")
                && stderr.contains("DOT_HOST_BINARY not copied")
                && (!sums_there() || stderr.contains("already, the same build")),
            "{args:?}: {stderr}"
        );
        assert_offline(&e);
    }
    assert!(e.p("remote/made").exists(), "applied there");
}

#[test]
fn dot_binary_upgrades_an_older_dot_there() {
    let e = without_dot_there();
    e.script(
        "remote/.local/bin/dot",
        "#!/bin/sh\ncase \"$1\" in --version) echo 'dot 0.0.1' ;; *) echo \"error: unrecognized subcommand '$1'\" >&2; exit 2 ;; esac\n",
    );
    let build = e.p("build/dot");
    let out = offline(
        &e,
        &[
            "apply",
            "--host",
            "me@box",
            "--yes",
            "--no-secrets",
            "plain",
        ],
        Some(&build),
    );
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}\n{}", e.log());
    assert!(
        stderr.contains("upgrading ") && stderr.contains("(dot 0.0.1)"),
        "{stderr}"
    );
    assert_eq!(
        fs::read(e.p("remote/.local/bin/dot")).unwrap(),
        fs::read(&build).unwrap()
    );
    assert!(e.p("remote/made").exists());
    assert_offline(&e);
}

#[test]
fn dot_binary_replaces_another_build_of_this_version_there() {
    if !sums_there() {
        return;
    }
    // Every build from source says the same version: the file tells them apart.
    let e = without_dot_there();
    e.script(
        "remote/.local/bin/dot",
        concat!(
            "#!/bin/sh\ncase \"$1\" in --version) echo 'dot ",
            env!("CARGO_PKG_VERSION"),
            "' ;; *) echo \"error: an old build\" >&2; exit 2 ;; esac\n"
        ),
    );
    let build = e.p("build/dot");
    let out = offline(
        &e,
        &[
            "apply",
            "--host",
            "me@box",
            "--yes",
            "--no-secrets",
            "--dot-binary",
            &build.display().to_string(),
            "plain",
        ],
        None,
    );
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}\n{}", e.log());
    assert!(
        stderr.contains("replacing ") && stderr.contains("another build"),
        "{stderr}"
    );
    assert_eq!(
        fs::read(e.p("remote/.local/bin/dot")).unwrap(),
        fs::read(&build).unwrap()
    );
    assert!(e.p("remote/made").exists());
    assert_offline(&e);
}

#[test]
fn a_dot_binary_that_is_not_this_dot_for_that_machine_is_refused() {
    // Another CPU (here: riscv64 ELF): refused here, before anything is copied.
    let e = without_dot_there();
    let mut header = vec![0u8; 64];
    header[..8].copy_from_slice(b"\x7fELF\x02\x01\x01\x00");
    header[16] = 3;
    header[18] = 243;
    e.write("build/riscv-dot", "");
    fs::write(e.p("build/riscv-dot"), &header).unwrap();
    let out = offline(
        &e,
        &[
            "apply",
            "--host",
            "me@box",
            "--yes",
            "--no-secrets",
            "--dot-binary",
            &e.p("build/riscv-dot").display().to_string(),
            "plain",
        ],
        None,
    );
    let stderr = text(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("is a linux/riscv binary; me@box is "),
        "{stderr}"
    );
    assert!(!e.log().contains("scp"), "{}", e.log());
    assert!(!e.p("remote/.dot").exists());
    assert_offline(&e);

    // One of this machine's own tools: a binary for it, but not this dot. When a copy of one
    // runs, it says something else; else (macOS stops copies of its system binaries) it does
    // not run. Either way the upload is removed again, nothing is replaced and nothing runs.
    let other = e.p("build/other");
    let runs = ["echo", "git", "tar", "env", "uname"].iter().any(|t| {
        let Some(p) = host_tool(t) else { return false };
        let _ = fs::remove_file(&other);
        fs::copy(&p, &other).unwrap();
        Command::new(&other)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    });
    e.reset_logs();
    let out = offline(&e, &["secrets", "push", "me@box", "tok"], Some(&other));
    let stderr = text(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    if runs {
        assert!(
            stderr.contains(concat!("`dot ", env!("CARGO_PKG_VERSION"), "` (this dot)")),
            "{stderr}"
        );
    } else {
        assert!(stderr.contains("does not run there"), "{stderr}");
    }
    assert!(
        stderr.contains("nothing was replaced on me@box"),
        "{stderr}"
    );
    assert!(e.log().contains("scp"), "{}", e.log());
    assert!(e.log().contains("[rm -f "), "{}", e.log());
    assert!(!e.p("remote/.local/bin/.dot.tmp").exists());
    assert!(!e.p("remote/.local/bin/dot").exists());
    assert!(e.lpass_log().is_empty() && e.stdin().is_empty());
    assert_offline(&e);

    // A file that is not there, or not a binary: refused before ssh is run at all.
    for (value, says) in [
        (e.p("build/missing"), "cannot read it"),
        (e.p("repo/dot.yml"), "not a dot binary"),
    ] {
        e.reset_logs();
        let out = offline(&e, &["plan", "--host", "me@box", "plain"], Some(&value));
        let stderr = text(&out.stderr);
        assert!(!out.status.success(), "{stderr}");
        assert!(
            stderr.contains("DOT_HOST_BINARY ") && stderr.contains(says),
            "{stderr}"
        );
        assert!(e.log().is_empty(), "{}", e.log());
    }
    // A dry run of a push checks it too.
    e.reset_logs();
    let out = offline(
        &e,
        &["secrets", "push", "me@box", "tok", "--dry-run"],
        Some(&e.p("build/missing")),
    );
    let stderr = text(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("DOT_HOST_BINARY ") && stderr.contains("cannot read it"),
        "{stderr}"
    );
    // --dot-binary is for --host.
    let out = e.dot(&["apply", "--dot-binary", "/x", "plain"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("--host"),
        "{}",
        text(&out.stderr)
    );
}
