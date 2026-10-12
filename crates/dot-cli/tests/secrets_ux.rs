//! Secrets and variables UX, end to end through the `dot` binary: prompted variables in
//! a non-interactive run, `dot env --shell`, `dot secrets push` (against a fake `ssh` on a temp
//! PATH that runs commands in a temp remote home, where this `dot` is installed, and logs its
//! argv), and `dot secrets export --age` / `import`. HOME, the state dir and PATH all point into temp dirs; no real ssh, no
//! network. Unix-only: fakes are shell scripts and modes are checked.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

const TOKEN: &str = "tok-9c1f-very-secret";
const PEM: &[u8] = b"-----BEGIN KEY-----\nsecret-pem-bytes\x00\xff\n";

const FAKE_SSH: &str = r#"#!/bin/sh
# Fake ssh: logs argv, skips options, runs the command in the fake remote home.
printf 'ssh' >> "$FAKE_LOG"; for a; do printf ' [%s]' "$a" >> "$FAKE_LOG"; done; echo >> "$FAKE_LOG"
while :; do
  case "$1" in
    -o|-O|-e|-p|-i|-l|-F|-J|-b|-c|-m|-S|-W) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
shift
cd "$FAKE_REMOTE" && HOME="$FAKE_REMOTE" XDG_STATE_HOME="$FAKE_REMOTE/.state" exec /bin/sh -c "$*"
"#;

const FAKE_SCP: &str = r#"#!/bin/sh
# Fake scp: logs argv, copies sources into host:dir/ under the fake remote home, keeping modes.
printf 'scp' >> "$FAKE_LOG"; for a; do printf ' [%s]' "$a" >> "$FAKE_LOG"; done; echo >> "$FAKE_LOG"
while [ "${1#-}" != "$1" ]; do shift; done
n=$#; i=0
for a; do i=$((i+1)); [ $i -eq $n ] && dest=$a; done
dir=${dest#*:}
case "$dir" in /*) target=$dir ;; *) target="$FAKE_REMOTE/$dir" ;; esac
i=0
for a; do i=$((i+1)); [ $i -lt $n ] && cp -p "$a" "$target"; done
exit 0
"#;

struct Env {
    _tmp: TempDir,
    root: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["home", "state", "logs", "repo/bundles/app", "remote", "bin"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let e = Self { _tmp: tmp, root };
        e.write("repo/dot.yml", "vars:\n  editor: nvim\n");
        e.write("repo/bundles/app/bundle.yml", "steps: []\n");
        e.write(
            "repo/secrets.yml",
            "secrets:\n  tok: { ref: \"envfile:API_TOKEN\" }\n  vpn: { ref: \"envfile-file:vpn/work.ovpn\", kind: file }\n",
        );
        e.script("bin/ssh", FAKE_SSH);
        e.script("bin/scp", FAKE_SCP);
        // The remote has this dot (`dot secrets push` installs one where it is missing).
        fs::create_dir_all(e.p("remote/.local/bin")).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_dot"), e.p("remote/.local/bin/dot")).unwrap();
        e
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.p(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn private(&self, rel: &str, bytes: &[u8]) {
        let p = self.p(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn script(&self, rel: &str, text: &str) {
        self.write(rel, text);
        fs::set_permissions(self.p(rel), fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn seed_secrets(&self) {
        self.private(
            "home/.config/dot/secrets.env",
            format!("API_TOKEN=\"{TOKEN}\"\n").as_bytes(),
        );
        self.private("home/.config/dot/files/vpn/work.ovpn", PEM);
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_dot"));
        c.args(args)
            .current_dir(self.p("repo"))
            .env("HOME", self.p("home"))
            .env_remove("DOT_STATE_DIR")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_CACHE_HOME")
            .env("XDG_STATE_HOME", self.p("state"))
            .env("PATH", format!("{}:/usr/bin:/bin", self.p("bin").display()))
            .env("FAKE_LOG", self.p("ssh.log"))
            .env("FAKE_REMOTE", self.p("remote"))
            .env("DOT_ROLE", "server")
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", &self.root)
            .env_remove("DOT_REPO")
            .env_remove("DOT_HOST_BINARY")
            .env_remove("DOT_PROFILE")
            .stdin(Stdio::null());
        c
    }

    fn dot(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn mode(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

// ---------------------------------------------------------------------------------------------
// Prompted variables

#[test]
fn a_non_interactive_run_lists_every_variable_it_would_ask_for() {
    let e = Env::new();
    e.write(
        "repo/dot.yml",
        concat!(
            "vars:\n",
            "  email: { prompt: \"Your e-mail\", persist: local }\n",
            "  gpg_pin: { prompt: \"GPG PIN\", sensitive: true }\n",
            "  name: { required: true }\n",
        ),
    );
    let logs = e.p("logs");
    let out = e.dot(&["apply", "--yes", "--log-dir", logs.to_str().unwrap()]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains("missing required variables: email, gpg_pin, name"),
        "{err}"
    );
    assert!(
        err.contains("there is no terminal to ask for email, gpg_pin here"),
        "{err}"
    );
    assert!(!e.p("home/.config/dot/local.env").exists());

    // Answered in local.env: asked no more.
    e.write(
        "home/.config/dot/local.env",
        "email=me@example.com\ngpg_pin=1234\nname=Me\n",
    );
    let out = e.dot(&["apply", "--yes", "--log-dir", logs.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out.stderr));

    // `dot plan` names what apply would ask for, and asks nothing.
    fs::remove_file(e.p("home/.config/dot/local.env")).unwrap();
    let out = e.dot(&["plan"]);
    assert!(
        text(&out.stdout).contains("asked for by `dot apply`: email, gpg_pin"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn plan_shows_steps_using_unanswered_prompts_as_unknown() {
    let e = Env::new();
    e.write(
        "repo/dot.yml",
        concat!(
            "vars:\n",
            "  email: { prompt: \"Your e-mail\", persist: local, required: true }\n",
            "  pin: { prompt: \"PIN\", sensitive: true }\n",
            "  greeting: \"hi {{ email }}\"\n",
        ),
    );
    e.write(
        "repo/bundles/app/bundle.yml",
        concat!(
            "steps:\n",
            "  - create: [~/plain]\n",
            "  - template:\n",
            "      ~/.config/app/credentials: credentials.j2\n",
            "  - shell: { cmd: \"echo {{ greeting }}\", unless: \"false\" }\n",
        ),
    );
    e.write("repo/bundles/app/credentials.j2", "user={{ email }}\n");
    let out = e.dot(&["plan", "app"]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(!stdout.contains("undefined"), "{stdout}");
    assert!(
        stdout.contains("asked for by `dot apply`: email, pin"),
        "{stdout}"
    );
    assert!(stdout.contains("create ~/plain [pending]"), "{stdout}");
    assert!(
        stdout.contains("(uses email, asked when applying) [unknown]"),
        "{stdout}"
    );
    assert!(
        stdout.contains("(uses greeting, asked when applying) [unknown]"),
        "{stdout}"
    );
    assert!(!e.p("home/plain").exists());

    // A variable nothing supplies is still an error.
    e.write("repo/bundles/app/credentials.j2", "user={{ nobody }}\n");
    let out = e.dot(&["plan", "app"]);
    assert!(!out.status.success());
    assert!(text(&out.stdout).contains("undefined variable `nobody`"));
}

// ---------------------------------------------------------------------------------------------
// dot env

#[test]
fn env_writes_non_sensitive_vars_and_recorded_paths() {
    let e = Env::new();
    e.write(
        "repo/dot.yml",
        concat!(
            "vars:\n",
            "  DOT_TEST_EDITOR: nvim\n",
            "  QUOTE: \"it's\"\n",
            "  workspace_dir: /p\n",
            "  api_password: { default: hunter2-pw }\n",
            "  pin: { default: \"9911\", sensitive: true }\n",
            "  derived: \"pin={{ pin }}\"\n",
        ),
    );
    e.write(
        "state/dot/env.json",
        r#"{"path":[{"dir":"/opt/go/bin","position":"prepend","bundle":"go"}],"vars":[{"name":"GOPATH","value":"/opt/go","bundle":"go"}]}"#,
    );
    let out = e.dot(&["env", "--shell", "zsh"]);
    let zsh = text(&out.stdout);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(zsh.contains("export DOT_TEST_EDITOR='nvim'\n"), "{zsh}");
    assert!(zsh.contains("export QUOTE='it'\\''s'\n"), "{zsh}");
    // A config variable for templates is not an environment variable of every shell.
    assert!(!zsh.contains("workspace_dir"), "{zsh}");
    assert!(zsh.contains("export GOPATH='/opt/go'\n"), "{zsh}");
    assert!(zsh.contains("PATH='/opt/go/bin'"), "{zsh}");
    for hidden in ["hunter2-pw", "9911", "api_password", "derived"] {
        assert!(!zsh.contains(hidden), "{hidden} leaked: {zsh}");
    }

    let target = e.p("env.fish");
    let out = e.dot(&["env", "--shell", "fish", "-o", target.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let fish = fs::read_to_string(&target).unwrap();
    assert!(fish.contains("set -gx DOT_TEST_EDITOR 'nvim'\n"), "{fish}");
    assert!(
        fish.contains("or set -gx PATH '/opt/go/bin' $PATH"),
        "{fish}"
    );

    let out = e.dot(&["env", "--shell", "tcsh"]);
    assert!(!out.status.success());

    // `--cache`: written once, its path printed; written again only when an input changes.
    let cached = |e: &Env| {
        let out = e.dot(&["env", "--shell", "zsh", "--cache"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        PathBuf::from(text(&out.stdout).trim())
    };
    let path = cached(&e);
    assert_eq!(fs::read_to_string(&path).unwrap(), zsh);
    fs::write(&path, "# stale on purpose\n").unwrap();
    assert_eq!(cached(&e), path);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "# stale on purpose\n",
        "fresh: not written again"
    );
    std::thread::sleep(std::time::Duration::from_millis(1100));
    e.write("repo/dot.yml", "vars:\n  DOT_TEST_EDITOR: vim\n");
    cached(&e);
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("export DOT_TEST_EDITOR='vim'"),
        "dot.yml changed: written again"
    );
}

// ---------------------------------------------------------------------------------------------
// push

#[test]
fn push_keeps_every_secret_there_owner_only_and_never_puts_a_value_in_argv() {
    let e = Env::new();
    e.seed_secrets();
    let out = e.dot(&["secrets", "push", "me@box"]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(
        stderr.contains("dot: me@box: pushing 2 secrets (tok, vpn) on ssh's stdin\n"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "dot: me@box: secrets kept in ~/.config/dot/secrets.env, ~/.config/dot/files: \
             added tok, vpn"
        ),
        "{stderr}"
    );
    let remote_env = e.p("remote/.config/dot/secrets.env");
    let remote_pem = e.p("remote/.config/dot/files/vpn/work.ovpn");
    assert!(
        fs::read_to_string(&remote_env)
            .unwrap()
            .contains(&format!("API_TOKEN=\"{TOKEN}\"")),
        "{}",
        fs::read_to_string(&remote_env).unwrap()
    );
    assert_eq!(fs::read(&remote_pem).unwrap(), PEM);
    assert_eq!(mode(&remote_env), 0o600);
    assert_eq!(mode(&remote_pem), 0o600);
    assert_eq!(mode(&e.p("remote/.config/dot/files/vpn")), 0o700);

    // One probe, then `dot secrets receive` fed on its stdin: no scp, no value in any argv.
    let log = fs::read_to_string(e.p("ssh.log")).unwrap();
    assert!(
        log.lines()
            .any(|l| l.starts_with("ssh [-T] [-o] [EscapeChar=none] ")
                && l.ends_with("/.local/bin/dot secrets receive]")),
        "{log}"
    );
    assert!(!log.contains("scp"), "{log}");
    assert!(
        !log.contains(TOKEN) && !log.contains("secret-pem-bytes"),
        "{log}"
    );

    // Dry run: where each one would be kept; nothing resolved or sent.
    fs::remove_file(e.p("ssh.log")).unwrap();
    let out = e.dot(&["secrets", "push", "--dry-run", "me@box"]);
    assert!(out.status.success());
    let stdout = text(&out.stdout);
    assert!(stdout.contains("tok -> envfile:API_TOKEN"), "{stdout}");
    assert!(
        stdout.contains("vpn -> envfile-file:vpn/work.ovpn"),
        "{stdout}"
    );
    assert!(!e.p("ssh.log").exists());

    // Guardrails still apply here: a group-readable secrets file stops it.
    fs::set_permissions(
        e.p("home/.config/dot/secrets.env"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let out = e.dot(&["secrets", "push", "me@box"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("chmod 600"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn push_reports_a_failing_ssh() {
    let e = Env::new();
    e.seed_secrets();
    e.script(
        "bin/ssh",
        "#!/bin/sh\necho 'ssh: connect: refused' >&2\nexit 255\n",
    );
    let out = e.dot(&["secrets", "push", "me@box"]);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("refused"), "{err}");
    assert!(!err.contains(TOKEN), "{err}");
    let out = e.dot(&["secrets", "push", "-oProxyCommand=x"]);
    assert!(!out.status.success());
}

// ---------------------------------------------------------------------------------------------
// export / import

#[test]
fn export_then_import_restores_every_secret_owner_only() {
    let e = Env::new();
    e.seed_secrets();
    e.write("pass", "correct horse battery\n");
    let pass = e.p("pass");
    let archive = e.p("backup.age");
    let out = e.dot(&[
        "secrets",
        "export",
        "--age",
        "-o",
        archive.to_str().unwrap(),
        "--passphrase-file",
        pass.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let sealed = fs::read(&archive).unwrap();
    assert!(sealed.starts_with(b"age-encryption.org/v1\n"));
    assert!(!sealed.windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()));
    assert_eq!(mode(&archive), 0o600);

    // A new machine: empty home.
    fs::remove_dir_all(e.p("home/.config/dot")).unwrap();
    let out = e.dot(&[
        "secrets",
        "import",
        archive.to_str().unwrap(),
        "--passphrase-file",
        pass.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("restored ~/.config/dot/secrets.env"));
    let env = e.p("home/.config/dot/secrets.env");
    let pem = e.p("home/.config/dot/files/vpn/work.ovpn");
    assert!(fs::read_to_string(&env).unwrap().contains(TOKEN));
    assert_eq!(fs::read(&pem).unwrap(), PEM);
    assert_eq!(mode(&env), 0o600);
    assert_eq!(mode(&pem), 0o600);

    // Existing secrets are not overwritten without --force.
    let out = e.dot(&[
        "secrets",
        "import",
        archive.to_str().unwrap(),
        "--passphrase-file",
        pass.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("--force"),
        "{}",
        text(&out.stderr)
    );
    let out = e.dot(&[
        "secrets",
        "import",
        "--force",
        archive.to_str().unwrap(),
        "--passphrase-file",
        pass.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));

    // The wrong passphrase restores nothing.
    e.write("wrong", "nope\n");
    let wrong = e.p("wrong");
    let out = e.dot(&[
        "secrets",
        "import",
        "--force",
        archive.to_str().unwrap(),
        "--passphrase-file",
        wrong.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("wrong passphrase"),
        "{}",
        text(&out.stderr)
    );

    // To stdout and back through stdin.
    let out = e.dot(&[
        "secrets",
        "export",
        "--passphrase-file",
        pass.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    fs::remove_dir_all(e.p("home/.config/dot")).unwrap();
    let mut child = e
        .command(&[
            "secrets",
            "import",
            "--passphrase-file",
            pass.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child.stdin.take().unwrap().write_all(&out.stdout).unwrap();
    }
    let done = child.wait_with_output().unwrap();
    assert!(done.status.success(), "{}", text(&done.stderr));
    assert!(fs::read_to_string(&env).unwrap().contains(TOKEN));
}

#[test]
fn export_without_a_terminal_or_secrets_says_why() {
    let e = Env::new();
    let out = e.dot(&["secrets", "export", "-o", "/dev/null"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("nothing to export"),
        "{}",
        text(&out.stderr)
    );
    e.seed_secrets();
    let out = e.dot(&["secrets", "export", "-o", "/dev/null"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("--passphrase-file"),
        "{}",
        text(&out.stderr)
    );
}
