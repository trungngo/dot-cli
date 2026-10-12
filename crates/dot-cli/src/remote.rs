//! `dot apply --host [user@]host` and `dot plan --host`: run `dot` on another machine over ssh.
//!
//! 1. **Probe** the remote in one `ssh`: `uname -sm` (its release target), `$HOME`, whether it
//!    has `rsync`, and whether (and which) `dot` is installed: the first of `command -v dot` and
//!    `~/.local/bin/dot` (which counts even when a non-interactive shell's PATH leaves it out)
//!    whose `--version` says `dot <version>` (graphviz's `dot` does not). An older `dot` there
//!    stops the run (`dot self-update` there updates it).
//! 2. **Install `dot`** when it is missing: the local binary is copied (`scp`) when it runs
//!    there (same OS and CPU, and a static build on Linux). Otherwise, when this machine has a
//!    GitHub token (`GITHUB_TOKEN`, `GH_TOKEN` or `gh auth token`, as for `dot self-update`;
//!    needed only for a private fork's releases, `DOT_RELEASE_REPO`), the release binary for the
//!    remote's target is downloaded here (through the API, checked against the release's
//!    `SHA256SUMS`, exactly as `dot self-update` does) and copied over: the token never leaves
//!    this machine. Without a token, or with `DOT_RELEASE_BASE` (a mirror), the remote runs
//!    `install.sh` (embedded in this binary, fed to `sh -s` on ssh's stdin, with this side's
//!    `DOT_RELEASE_REPO` and `DOT_RELEASE_BASE`) for its own target, which fetches the public
//!    release asset and checks it against the release's checksums, without any token. A `dot`
//!    already there is used as it is. With `--dot-binary PATH` (default `DOT_HOST_BINARY`),
//!    none of that: that file is copied instead (see [`binary`]), unless the `dot` there is
//!    newer, or is this very build (the same SHA-256: every build from source says the same
//!    version); an older one, or another build, is replaced. Its header must name the remote's OS
//!    and CPU (checked here, before anything is copied); uploaded to a temporary name next to
//!    `~/.local/bin/dot`, it must say `dot <this version>` there before it is renamed into
//!    place. Nothing is downloaded, and nothing falls back to a download.
//! 3. **Plan here, resolve here** (before anything is synced; secrets for `apply` only, unless
//!    `--no-secrets`): `dot host-info` there reports its facts, pinned profile and the names in
//!    its `local.env`; this machine plans the remote's run against them (which bundles, so which
//!    secrets, see [`send`]) and resolves those secrets through its own providers (LastPass,
//!    envfile, ...), unlocking each once. Missing required secrets stop the run before anything
//!    is synced or sent. `persist: local` variables (from
//!    this machine's `local.env`) and `--var`s are forwarded, except `per_machine: true` ones,
//!    whose value is asked for here, or given with `--var`.
//! 4. **Sync the repository** to `~/.dot` (or `--remote-repo`), refused when that exists, is not
//!    empty and holds no `dot.yml` (never overwriting something else): `rsync -az --delete` when
//!    both ends have rsync, excluding exactly what git ignores here (`git ls-files --others
//!    --ignored --exclude-standard --directory`: `.gitignore` files, `.git/info/exclude` and the
//!    global `core.excludesFile` alike) plus the repository's `.gitignore` files as filters
//!    (ignored files on the remote, such as its `.env.local`, are left alone); else a tar stream
//!    over ssh of what git tracks or would track (`git ls-files --cached --others
//!    --exclude-standard`) plus `.git` itself. Ignored local files (`.env.local`, secrets) never
//!    travel.
//! 5. **Run** `dot apply` (or `dot plan`) there, with a terminal (`ssh -t`) when this side has
//!    one, so the plan can be confirmed and sudo asked for; its output streams through and its
//!    exit code is `dot`'s. What was resolved and forwarded travels on that ssh session's stdin
//!    (`--secrets-from -`, see [`crate::handoff`]); with a terminal, stdin is the terminal, so it
//!    goes through a second session instead (`dot secrets relay`), into a FIFO in a private
//!    directory there that the run reads (`--secrets-from <fifo>`): never on a command line, in
//!    the environment or in a file. The remote keeps it in memory for that run, unless
//!    `--persist-secrets`.
//!
//! `--dry-run` installs nothing, syncs nothing and sends nothing: it says what would be done and
//! sent (names only), from what is already there.
//!
//! Every ssh shares one connection (`ControlMaster`), so a password or key passphrase is asked
//! for once. Everything but the relay's second session goes through an injectable [`Runner`]:
//! tests fake ssh, scp and rsync.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use clap::Args;
use dot_core::SecretBytes;
use dot_directives::github_release::Github;
use dot_platform::exec::{Cmd, Output, Runner, SystemRunner};

use crate::RepoArgs;
use binary::HostBinary;

/// `install.sh`, fed to the remote's `sh` when it has no `dot`.
pub const INSTALL_SH: &str = include_str!("../../../install.sh");

/// Where the repository goes on the remote, under its home: install.sh's default.
pub const DEFAULT_REMOTE_REPO: &str = ".dot";

/// What the remote prints when the sync target holds something other than a dotfiles
/// repository.
const NOT_A_DOT_REPO: &str = "dot:not-a-dot-repo";

/// ssh options for a session fed bytes on its stdin: no terminal there (`-T` overrides a
/// `RequestTTY` in `~/.ssh/config`) and no escape character.
const FED: [&str; 3] = ["-T", "-o", "EscapeChar=none"];

#[derive(Args, Clone, Default)]
pub struct HostArgs {
    /// Run on another machine over ssh instead: `[user@]host` as ssh understands it (aliases
    /// from ~/.ssh/config work). Installs dot there if needed, resolves here the secrets its
    /// bundles need and forwards your variables (on ssh's stdin, for that run only), syncs the
    /// repository, then runs there.
    #[arg(long, value_name = "[USER@]HOST")]
    pub host: Option<String>,
    /// With --host: where the repository goes on the remote (default: ~/.dot).
    #[arg(long, value_name = "DIR", requires = "host")]
    pub remote_repo: Option<String>,
    /// With --host: install (or upgrade) dot there from this file, copied over with scp to
    /// ~/.local/bin/dot, instead of downloading it: nothing is fetched from GitHub. It must be a
    /// dot of this version built for the remote's OS and CPU (for a Linux server, a static musl
    /// build: scripts/build-linux.sh). Not copied when the dot there is this same build (same
    /// SHA-256) or newer. Default: DOT_HOST_BINARY.
    #[arg(long, value_name = "PATH", requires = "host", value_parser = crate::path_arg())]
    pub dot_binary: Option<PathBuf>,
}

/// An OS and CPU `dot` releases are built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub os: &'static str,
    pub arch: &'static str,
}

impl Target {
    /// From `uname -sm` (`Linux x86_64`, `Darwin arm64`).
    pub fn from_uname(s: &str) -> Result<Target> {
        let mut words = s.split_whitespace();
        let (Some(sys), Some(machine)) = (words.next(), words.next()) else {
            bail!("unexpected `uname -sm` output: {s:?}");
        };
        let os = match sys {
            "Linux" => "linux",
            "Darwin" => "macos",
            other => bail!("{other} is not supported by --host (Linux and macOS are)"),
        };
        let arch = match machine {
            "x86_64" | "amd64" => "x86_64",
            "aarch64" | "arm64" => "aarch64",
            other => bail!("CPU {other} is not supported (x86_64 and aarch64 are)"),
        };
        Ok(Target { os, arch })
    }

    /// The platform this binary runs on, when a copy of it also runs on another machine of
    /// the same platform: any macOS build, and static (musl) Linux builds, as released. A
    /// glibc build may need a newer libc than the remote has.
    pub fn this_binary() -> Option<Target> {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x86_64",
            "aarch64" => "aarch64",
            _ => return None,
        };
        if cfg!(target_os = "macos") {
            Some(Target { os: "macos", arch })
        } else if cfg!(all(target_os = "linux", target_env = "musl")) {
            Some(Target { os: "linux", arch })
        } else {
            None
        }
    }
}

/// What the probe found on the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub target: Target,
    pub home: String,
    pub rsync: bool,
    /// `dot`'s path and `--version` output, when installed.
    pub dot: Option<(String, String)>,
}

/// The probe, run by the remote's `sh`: one line each for `uname -sm`, `$HOME`, rsync, then
/// `dot`'s path and version when there is one: the first candidate that says it is `dot
/// <version>`.
const PROBE: &str = r#"uname -sm
printf '%s\n' "$HOME"
if command -v rsync >/dev/null 2>&1; then echo rsync; else echo no-rsync; fi
for d in "$(command -v dot 2>/dev/null)" "$HOME/.local/bin/dot"; do
  [ -n "$d" ] && [ -x "$d" ] || continue
  v=$("$d" --version 2>/dev/null) || continue
  case "$v" in "dot "[0-9]*) printf '%s\n%s\n' "$d" "$v"; break ;; esac
done"#;

/// `dot X.Y.Z` (any suffix after a `-` or `+` ignored) as numbers.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().strip_prefix("dot ").unwrap_or(v.trim());
    let core = v.split(['-', '+', ' ']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

impl Probe {
    pub fn parse(out: &str) -> Result<Probe> {
        let mut lines = out.lines().map(str::trim);
        let uname = lines.next().context("the probe printed nothing")?;
        let target = Target::from_uname(uname)?;
        let home = lines
            .next()
            .filter(|h| h.starts_with('/'))
            .context("the remote's $HOME is not an absolute path")?
            .to_string();
        let rsync = lines.next() == Some("rsync");
        let dot = match (lines.next(), lines.next()) {
            (Some(path), version) if !path.is_empty() => {
                Some((path.to_string(), version.unwrap_or("unknown").to_string()))
            }
            _ => None,
        };
        Ok(Probe {
            target,
            home,
            rsync,
            dot,
        })
    }
}

/// POSIX single quotes for the remote shell.
pub fn sh_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The remote `sh` command that prints [`NOT_A_DOT_REPO`] when `dir` exists, is not an empty
/// directory and holds no `dot.yml`.
fn sync_target_check(dir: &str) -> String {
    format!(
        "if [ -e {d} ] && [ ! -f {d}/dot.yml ] && [ -n \"$(ls -A {d} 2>/dev/null)\" ]; \
         then echo {NOT_A_DOT_REPO}; fi",
        d = sh_quote(dir)
    )
}

/// A remote path rsync and scp may be given: they hand it to the remote shell, so nothing that
/// shell would interpret.
fn safe_remote_path(p: &str) -> bool {
    p.starts_with('/')
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/-+@=,".contains(c))
}

/// Refuses what ssh would read as an option, and blanks.
pub fn check_host(host: &str) -> Result<()> {
    if host.is_empty() || host.starts_with('-') || host.chars().any(char::is_whitespace) {
        bail!("`{host}` is not a host (expected [user@]host)");
    }
    Ok(())
}

/// What a copied binary must say there before it replaces anything (`--dot-binary`).
struct Verify<'a> {
    /// `dot <version>`, its `--version`'s first line.
    version: &'a str,
    /// The remote's platform.
    target: Target,
    /// Where the binary was named: `--dot-binary` or `DOT_HOST_BINARY`.
    from: &'static str,
}

/// With `--dot-binary`, what [`Remote::ensure_dot`] does, and what it says about it.
enum Given {
    /// The `dot` there will do: its path.
    Use(String, String),
    /// The binary is copied over.
    Copy(Option<String>),
}

/// A connection to one host.
pub struct Remote {
    pub runner: Arc<dyn Runner>,
    pub host: String,
    /// Options for every ssh and scp (and rsync's ssh): connection sharing.
    pub ssh_opts: Vec<String>,
    /// Give the remote run a terminal (`ssh -t`): this side has one.
    pub tty: bool,
    /// This binary's platform, when it may be copied ([`Target::this_binary`]).
    pub local: Option<Target>,
    /// This binary.
    pub exe: PathBuf,
    /// `DOT_RELEASE_BASE` for the remote install.sh (a mirror; tests).
    pub release_base: Option<String>,
    /// The repository dot is released from ([`crate::selfupdate::release_repo`]).
    pub release_repo: String,
    /// Where dot's releases are downloaded from, here, for a remote this binary cannot be copied
    /// to.
    pub github: Github,
    /// This machine's GitHub token for them (a private fork's releases need it); asked for only
    /// when dot has to be installed there. It is used here, never sent there.
    pub github_token: fn() -> Option<SecretBytes>,
    /// `--dot-binary`: what to install there instead (no download, no install.sh).
    pub dot_binary: Option<HostBinary>,
    /// Holds the connection-sharing socket.
    _control: Option<tempfile::TempDir>,
}

impl Remote {
    /// The real ssh, sharing one connection for the whole run.
    pub fn connect(host: &str) -> Result<Remote> {
        check_host(host)?;
        let mut ssh_opts = Vec::new();
        let mut control = None;
        if cfg!(unix) {
            // A short path: a unix socket's path is limited to ~100 bytes.
            if let Ok(dir) = tempfile::Builder::new()
                .prefix("dot-ssh-")
                .tempdir_in("/tmp")
            {
                ssh_opts = vec![
                    "-o".into(),
                    "ControlMaster=auto".into(),
                    "-o".into(),
                    format!("ControlPath={}/%C", dir.path().display()),
                    "-o".into(),
                    "ControlPersist=60".into(),
                ];
                control = Some(dir);
            }
        }
        Ok(Remote {
            runner: Arc::new(SystemRunner),
            host: host.to_string(),
            ssh_opts,
            tty: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            local: Target::this_binary(),
            exe: std::env::current_exe().context("cannot find this dot binary")?,
            release_base: std::env::var("DOT_RELEASE_BASE")
                .ok()
                .filter(|v| !v.is_empty()),
            release_repo: crate::selfupdate::release_repo()?,
            github: Github::from_env(),
            github_token: crate::selfupdate::github_token,
            dot_binary: None,
            _control: control,
        })
    }

    /// A remote for tests: `runner` runs everything, no connection sharing.
    #[cfg(all(test, unix))]
    pub fn with_runner(host: &str, runner: Arc<dyn Runner>, exe: PathBuf) -> Remote {
        Remote {
            runner,
            host: host.to_string(),
            ssh_opts: Vec::new(),
            tty: false,
            local: None,
            exe,
            release_base: None,
            release_repo: crate::selfupdate::DEFAULT_REPO.to_string(),
            github: Github::new(
                Arc::new(dot_platform::http::UreqClient::new()),
                "http://127.0.0.1:9",
            ),
            github_token: || None,
            dot_binary: None,
            _control: None,
        }
    }

    fn say(&self, msg: impl AsRef<str>) {
        eprintln!("dot: {}: {}", self.host, msg.as_ref());
    }

    /// `ssh <opts> host <command>`.
    fn ssh(&self, command: &str) -> Cmd {
        Cmd::new("ssh")
            .args(self.ssh_opts.iter().cloned())
            .arg(&self.host)
            .arg(command)
    }

    /// `ssh` fed `stdin`: never a terminal there, whatever `~/.ssh/config` says (`RequestTTY`),
    /// so the bytes reach the command as they are (no echo, no line editing, no control keys)
    /// and no escape character is read from them.
    fn ssh_fed(&self, command: &str, stdin: Vec<u8>) -> Cmd {
        let mut c = Cmd::new("ssh")
            .args(FED.iter().copied())
            .args(self.ssh_opts.iter().cloned())
            .arg(&self.host)
            .arg(command);
        c.stdin = Some(stdin);
        c
    }

    /// Runs `cmd`, failing unless it exits 0.
    fn check(&self, cmd: &Cmd) -> Result<Output> {
        let out = self
            .runner
            .output(cmd)
            .with_context(|| format!("cannot run `{}`", cmd.program))?;
        if !out.success() {
            bail!(
                "`{}` {}{}",
                cmd.line(),
                out.describe_exit(),
                out.last_line()
                    .map(|l| format!(": {l}"))
                    .unwrap_or_default()
            );
        }
        Ok(out)
    }

    pub fn probe(&self) -> Result<Probe> {
        let cmd = self.ssh(&format!("sh -c {}", sh_quote(PROBE)));
        let out = self.runner.output(&cmd).context("cannot run `ssh`")?;
        if !out.success() {
            // 255: ssh itself failed (no connection, refused key): its own message says why.
            if out.code == Some(255) {
                bail!(
                    "cannot reach {}{}",
                    self.host,
                    out.last_line()
                        .map(|l| format!(": {l}"))
                        .unwrap_or_default()
                );
            }
            bail!(
                "could not run the probe on {} ({}; it needs a POSIX `sh`: --host supports Linux \
                 and macOS remotes)",
                self.host,
                out.describe_exit()
            );
        }
        Probe::parse(&out.stdout).with_context(|| format!("probing {}", self.host))
    }

    /// Makes sure `dot` is installed there; returns its path.
    pub fn ensure_dot(&self, p: &Probe) -> Result<String> {
        if let Some(bin) = &self.dot_binary {
            return match self.with_binary(p, bin)? {
                Given::Use(path, note) => {
                    self.say(note);
                    Ok(path)
                }
                Given::Copy(note) => self.install_binary(p, bin, note),
            };
        }
        let ours = env!("CARGO_PKG_VERSION");
        if let Some((path, version)) = &p.dot {
            let shown = send::printable(version);
            match (parse_version(version), parse_version(ours)) {
                (Some(there), Some(here)) if there < here => bail!(
                    "{shown} at {path} on {} is older than this one ({ours}): update it there \
                     (`{path} self-update`), or pass --dot-binary with a dot {ours} built for \
                     {}/{}, then run this again",
                    self.host,
                    p.target.os,
                    p.target.arch
                ),
                (Some(there), Some(here)) if there > here => self.say(format!(
                    "using {path} ({shown}, newer than this one, {ours}; should this fail, \
                     `dot self-update` updates this one)"
                )),
                (Some(_), Some(_)) => {}
                _ => self.say(format!("using {path} ({shown}; this is {ours})")),
            }
            return Ok(path.clone());
        }
        let bin_dir = format!("{}/.local/bin", p.home);
        let dot = format!("{bin_dir}/dot");
        if !safe_remote_path(&bin_dir) {
            bail!("cannot install dot into {bin_dir}: an unusual path for scp");
        }
        if self.local == Some(p.target) {
            self.say(format!("copying this dot to {dot}"));
            self.copy_binary(&self.exe, &bin_dir, &dot, None)?;
            return Ok(dot);
        }
        let token = match &self.release_base {
            // A mirror needs no token.
            Some(_) => None,
            None => (self.github_token)(),
        };
        if let Some(token) = token {
            self.say(format!(
                "downloading dot for {}/{} here (the GitHub token stays on this machine)",
                p.target.os, p.target.arch
            ));
            let Some(asset) = crate::selfupdate::asset_name(p.target.os, p.target.arch) else {
                bail!(
                    "no dot releases are built for {}/{}",
                    p.target.os,
                    p.target.arch
                );
            };
            let staging = tempfile::Builder::new()
                .prefix("dot-remote-")
                .tempdir()
                .context("creating a temporary directory")?;
            let release = self
                .github
                .latest(&self.release_repo, Some(&token))
                .context("looking up the latest release of dot")?;
            let bin = crate::selfupdate::fetch_binary(
                &self.github,
                &self.release_repo,
                Some(&token),
                &release,
                &asset,
                staging.path(),
            )?;
            self.say(format!("copying dot {} to {dot}", release.tag_name));
            self.copy_binary(&bin, &bin_dir, &dot, None)?;
            return Ok(dot);
        }
        self.say(format!(
            "installing dot for {}/{} with install.sh",
            p.target.os, p.target.arch
        ));
        let mut env = Vec::new();
        if self.release_repo != crate::selfupdate::DEFAULT_REPO {
            env.push(format!("DOT_RELEASE_REPO={}", sh_quote(&self.release_repo)));
        }
        if let Some(base) = &self.release_base {
            env.push(format!("DOT_RELEASE_BASE={}", sh_quote(base)));
        }
        let cmd = if env.is_empty() {
            "sh -s".to_string()
        } else {
            format!("env {} sh -s", env.join(" "))
        };
        let c = self.ssh_fed(&cmd, INSTALL_SH.as_bytes().to_vec());
        let out = self.runner.output(&c).context("cannot run `ssh`")?;
        for line in out.stderr.lines() {
            eprintln!("  {line}");
        }
        if !out.success() {
            bail!(
                "install.sh failed there ({}){}{}",
                out.describe_exit(),
                if self.release_base.is_none() {
                    "; if dot's releases are private (a private fork): with GITHUB_TOKEN set \
                     here, or `gh auth login` done here, dot is downloaded here and copied over"
                } else {
                    ""
                },
                format_args!(
                    "; or pass --dot-binary with a dot {} built here for {}/{} ({})",
                    env!("CARGO_PKG_VERSION"),
                    p.target.os,
                    p.target.arch,
                    binary::rust_target(p.target)
                )
            );
        }
        Ok(dot)
    }

    /// With `--dot-binary` (`bin`): whether a `dot` there will do, or `bin` is to be copied.
    /// A newer `dot` than this one will do; one of this version will when it is this very build
    /// (the same SHA-256; every build from source says the same version, so a different one is
    /// replaced), or when there is no `sha256sum` or `shasum` there to tell. The `dot` the probe
    /// found is asked first, then `~/.local/bin/dot` when that is another file: an earlier copy
    /// may be there, behind a `dot` first on the PATH.
    fn with_binary(&self, p: &Probe, bin: &HostBinary) -> Result<Given> {
        let ours = format!("dot {}", env!("CARGO_PKG_VERSION"));
        let dot = format!("{}/.local/bin/dot", p.home);
        let Some((path, version)) = &p.dot else {
            return Ok(Given::Copy(None));
        };
        let shown = send::printable(version);
        if let (Some(there), Some(here)) = (parse_version(version), parse_version(&ours))
            && there > here
        {
            return Ok(Given::Use(
                path.clone(),
                format!(
                    "using {path} ({shown}, newer than this one, {ours}): {} not copied over it",
                    bin.from
                ),
            ));
        }
        // Whether the dot at `at` is `bin` (or cannot be told from it): `Some` note to use it.
        let same = |at: &str, behind: Option<&str>| -> Option<String> {
            let behind = behind
                .map(|b| format!("; {b}, first on the PATH there, is {shown}"))
                .unwrap_or_default();
            match self.sum_at(at) {
                Some(sum) if sum == bin.sha256 => Some(format!(
                    "{at} is {ours} already, the same build{behind}: {} not copied",
                    bin.from
                )),
                Some(_) => None,
                None => Some(format!(
                    "{at} is {ours} already (no sha256sum or shasum there to tell whether it is \
                     the same build){behind}: {} not copied",
                    bin.from
                )),
            }
        };
        let this_version = version.trim() == ours;
        if this_version && let Some(note) = same(path, None) {
            return Ok(Given::Use(path.clone(), note));
        }
        if *path != dot
            && self.version_at(&dot).as_deref() == Some(ours.as_str())
            && let Some(note) = same(&dot, Some(path))
        {
            return Ok(Given::Use(dot.clone(), note));
        }
        let what = if this_version {
            format!("{shown}, another build")
        } else {
            shown.to_string()
        };
        Ok(Given::Copy(Some(if *path == dot {
            format!(
                "{} {path} ({what}) with {}",
                if this_version {
                    "replacing"
                } else {
                    "upgrading"
                },
                bin.from
            )
        } else {
            format!(
                "{path} there is {what}: installing {} as {dot} for this run",
                bin.from
            )
        })))
    }

    /// `<dot> --version` there, when it runs.
    fn version_at(&self, dot: &str) -> Option<String> {
        let out = self
            .runner
            .output(&self.ssh(&format!("{} --version 2>/dev/null", sh_quote(dot))))
            .ok()?;
        out.success()
            .then(|| out.stdout.lines().next().unwrap_or("").trim().to_string())
    }

    /// The SHA-256 of the file `f` there (lowercase hex), when `sha256sum` (Linux) or `shasum`
    /// (macOS) is there to say it.
    fn sum_at(&self, f: &str) -> Option<String> {
        let f = sh_quote(f);
        let out = self
            .runner
            .output(&self.ssh(&format!(
                "sha256sum {f} 2>/dev/null || shasum -a 256 {f} 2>/dev/null"
            )))
            .ok()?;
        let sum = out.stdout.split_whitespace().next()?.to_ascii_lowercase();
        (out.success() && sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit()))
            .then_some(sum)
    }

    /// Where `--dot-binary` goes there (`~/.local/bin` and the `dot` in it), once its header
    /// says it runs there; refused before anything is copied.
    fn binary_dest(&self, p: &Probe, bin: &HostBinary) -> Result<(String, String)> {
        let bin_dir = format!("{}/.local/bin", p.home);
        if !safe_remote_path(&bin_dir) {
            bail!("cannot install dot into {bin_dir}: an unusual path for scp");
        }
        bin.check(p.target, &self.host)?;
        let dot = format!("{bin_dir}/dot");
        Ok((bin_dir, dot))
    }

    /// Copies `--dot-binary` to `~/.local/bin/dot` there, once its header says it runs there
    /// and, uploaded, it says it is this version.
    fn install_binary(&self, p: &Probe, bin: &HostBinary, note: Option<String>) -> Result<String> {
        let (bin_dir, dot) = self.binary_dest(p, bin)?;
        if let Some(n) = note {
            self.say(n);
        }
        self.say(format!(
            "copying {} ({}) to {dot}",
            bin.path.display(),
            bin.kind.describe()
        ));
        self.copy_binary(
            &bin.path,
            &bin_dir,
            &dot,
            Some(Verify {
                version: &format!("dot {}", env!("CARGO_PKG_VERSION")),
                target: p.target,
                from: bin.from,
            }),
        )?;
        Ok(dot)
    }

    /// Copies the binary `local` to `dot` (in `bin_dir`) there: to a temporary name first, then
    /// renamed into place. With `verify` (a version and the remote's platform), the copy must
    /// say that version (`--version`, run there) before it replaces anything; otherwise it is
    /// removed.
    fn copy_binary(
        &self,
        local: &Path,
        bin_dir: &str,
        dot: &str,
        verify: Option<Verify<'_>>,
    ) -> Result<()> {
        let tmp = format!("{bin_dir}/.dot.tmp");
        let (t, d) = (sh_quote(&tmp), sh_quote(dot));
        self.check(&self.ssh(&format!("mkdir -p {}", sh_quote(bin_dir))))?;
        self.check(
            &Cmd::new("scp")
                .args(self.ssh_opts.iter().cloned())
                .args(["-q".to_string(), local.display().to_string()])
                .arg(format!("{}:{tmp}", self.host)),
        )?;
        let Some(Verify {
            version: want,
            target,
            from,
        }) = verify
        else {
            self.check(&self.ssh(&format!("chmod 755 {t} && mv -f {t} {d}")))?;
            return Ok(());
        };
        let out = self
            .runner
            .output(&self.ssh(&format!("chmod 755 {t} && {t} --version")))
            .context("cannot run `ssh`")?;
        let said = out.stdout.lines().next().unwrap_or("").trim().to_string();
        if !out.success() || said != want {
            let _ = self.runner.output(&self.ssh(&format!("rm -f {t}")));
            // 255: ssh itself failed (the connection dropped), not the binary.
            if out.code == Some(255) {
                bail!(
                    "cannot reach {} to run the copy of {from} {} there{}; nothing was replaced \
                     there",
                    self.host,
                    local.display(),
                    out.last_line()
                        .map(|l| format!(": {}", send::printable(l)))
                        .unwrap_or_default()
                );
            }
            let why = if out.success() {
                format!(
                    "says `{}`, not `{want}` (this dot): build it from the same version",
                    send::printable(&said)
                )
            } else {
                format!(
                    "does not run there (`--version` {}{}){}",
                    out.describe_exit(),
                    out.last_line()
                        .map(|l| format!(": {}", send::printable(l)))
                        .unwrap_or_default(),
                    if target.os == "linux" {
                        "; a Linux server needs a static (musl) build"
                    } else {
                        ""
                    }
                )
            };
            bail!(
                "{from} {} {why}; nothing was replaced on {}",
                local.display(),
                self.host
            );
        }
        self.check(&self.ssh(&format!("mv -f {t} {d}")))?;
        Ok(())
    }

    /// Copies the repository at `root` to `dir` on the remote.
    pub fn sync(&self, p: &Probe, root: &Path, dir: &str) -> Result<()> {
        if !safe_remote_path(dir) {
            bail!(
                "cannot sync to {dir}: only letters, digits and `._/-+@=,` are supported in the \
                 remote path"
            );
        }
        self.check_sync_target(dir)?;
        if p.rsync && self.runner.on_path("rsync") {
            self.say(format!("syncing {} to {dir} (rsync)", root.display()));
            let mut ssh = vec!["ssh".to_string()];
            ssh.extend(self.ssh_opts.iter().cloned());
            // What git ignores here, however it is ignored, stays here (and is left alone
            // there).
            let ignored = ignored_files(self.runner.as_ref(), root)?;
            let excludes = if ignored.is_empty() {
                None
            } else {
                Some(exclude_file(&ignored)?)
            };
            let mut args = vec![
                "-az".to_string(),
                "--delete".to_string(),
                // Every directory's .gitignore excludes (and protects, on the remote) its
                // ignored files.
                "--filter=:- .gitignore".to_string(),
            ];
            if let Some(f) = &excludes {
                args.push(format!("--exclude-from={}", f.path().display()));
            }
            args.extend([
                "-e".to_string(),
                ssh.join(" "),
                format!("{}/", root.display()),
                format!("{}:{dir}/", self.host),
            ]);
            self.check(&Cmd::new("rsync").args(args))?;
        } else {
            self.say(format!(
                "syncing {} to {dir} (tar over ssh)",
                root.display()
            ));
            let files = repo_files(self.runner.as_ref(), root)?;
            let bytes = tar_gz(root, &files)?;
            let c = self.ssh_fed(
                &format!("mkdir -p {d} && tar -xzf - -C {d}", d = sh_quote(dir)),
                bytes,
            );
            self.check(&c)?;
        }
        Ok(())
    }

    /// Refuses to sync into `dir` when something else is there: anything but a missing or
    /// empty directory, or one holding `dot.yml` (an earlier sync, or a checkout of a dotfiles
    /// repository). The sync replaces what is there (`rsync --delete`), so an unrelated `~/.dot`
    /// (or an old dotbot `~/.dotfiles` named with `--remote-repo`) is never overwritten.
    fn check_sync_target(&self, dir: &str) -> Result<()> {
        let out = self.check(&self.ssh(&sync_target_check(dir)))?;
        if out.stdout.lines().any(|l| l.trim() == NOT_A_DOT_REPO) {
            bail!(
                "{dir} there exists and holds no dot.yml: not overwriting it; pass \
                 --remote-repo to sync somewhere else"
            );
        }
        Ok(())
    }

    /// What `dot host-info` reports there (`dot` is installed): also proves that `dot` is this
    /// tool, recent enough.
    pub fn host_info(&self, dot: &str) -> Result<HostInfo> {
        let out = self
            .runner
            .output(&self.ssh(&format!("{} host-info", sh_quote(dot))))
            .context("cannot run ssh")?;
        let info = out
            .stdout
            .lines()
            .rev()
            .find_map(|l| serde_json::from_str::<HostInfo>(l).ok());
        if let Some(i) = info.filter(|_| out.success()) {
            return Ok(i);
        }
        let said = format!("{}\n{}", out.stdout, out.stderr);
        if said.contains("unrecognized subcommand") || said.contains("unexpected argument") {
            bail!(
                "{dot} on {} does not know `host-info`: it is older than this dot ({}); update \
                 it there (`{dot} self-update`), then run this again",
                self.host,
                env!("CARGO_PKG_VERSION")
            );
        }
        if out.success() {
            bail!(
                "{dot} on {} is not this dot: `{dot} host-info` printed no report",
                self.host
            );
        }
        let lines: Vec<String> = out
            .stderr
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(send::printable)
            .collect();
        bail!(
            "`{dot} host-info` failed on {} ({}){}",
            self.host,
            out.describe_exit(),
            if lines.is_empty() {
                String::new()
            } else {
                format!(":\n  {}", lines.join("\n  "))
            }
        )
    }

    /// Runs `dot <args>` there on a terminal (when this side has one); its exit code. With
    /// `handoff`, the run reads it (`--secrets-from`, added after the subcommand): on this ssh
    /// session's stdin, or, when stdin is the terminal, through [`Remote::relay`].
    pub fn run(&self, dot: &str, args: &[String], handoff: Option<&[u8]>) -> Result<Option<i32>> {
        let Some(bytes) = handoff else {
            return self.run_plain(dot, args, None);
        };
        if self.tty {
            return self.relay(dot, args, bytes);
        }
        let mut args = args.to_vec();
        args.splice(1..1, ["--secrets-from".to_string(), "-".to_string()]);
        self.run_plain(dot, &args, Some(bytes))
    }

    fn run_plain(&self, dot: &str, args: &[String], stdin: Option<&[u8]>) -> Result<Option<i32>> {
        let mut words = vec![sh_quote(dot)];
        words.extend(args.iter().map(|a| sh_quote(a)));
        let mut c = match stdin {
            Some(bytes) => self.ssh_fed(&words.join(" "), bytes.to_vec()),
            None if self.tty => Cmd::new("ssh")
                .arg("-t")
                .args(self.ssh_opts.iter().cloned())
                .arg(&self.host)
                .arg(words.join(" ")),
            None => self.ssh(&words.join(" ")),
        };
        let code = self.runner.interactive(&c).context("cannot run ssh");
        if let Some(b) = c.stdin.as_mut() {
            zeroize::Zeroize::zeroize(b);
        }
        code
    }

    /// The run on a terminal, with `bytes` handed over through a second session: `dot secrets
    /// relay` there makes a FIFO in a new private directory and prints its path; the run is
    /// started with `--secrets-from <fifo>`, and the relay feeds it what this side writes to
    /// that session's stdin. The bytes never touch a disk there.
    fn relay(&self, dot: &str, args: &[String], bytes: &[u8]) -> Result<Option<i32>> {
        use std::io::BufRead as _;
        use std::process::{Command, Stdio};
        let mut child = Command::new("ssh")
            .args(FED)
            .args(&self.ssh_opts)
            .arg(&self.host)
            .arg(format!("{} secrets relay", sh_quote(dot)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("cannot run ssh")?;
        let mut line = String::new();
        // Kept open until the relay is done: closing it is how the relay there learns that this
        // side gave up on it.
        let mut out = child.stdout.take().map(std::io::BufReader::new);
        let read = out.as_mut().map(|o| o.read_line(&mut line));
        let fifo = line.trim().to_string();
        if !matches!(read, Some(Ok(n)) if n > 0) || !safe_remote_path(&fifo) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("`{dot} secrets relay` did not start there");
        }
        let mut stdin = child.stdin.take().context("ssh has no stdin")?;
        let data = zeroize::Zeroizing::new(bytes.to_vec());
        // The relay reads it all, then waits for the run to open the FIFO.
        let feeder = std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
        let mut args = args.to_vec();
        args.splice(1..1, ["--secrets-from".to_string(), fifo]);
        let code = self.run_plain(dot, &args, None);
        // The relay is done once the run has read everything; one that is still waiting (the
        // run stopped before reading) is stopped.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        let _ = feeder.join();
        drop(out);
        code
    }

    /// `dot secrets receive` there, fed `bytes`: keeps them in its envfile. Its exit code.
    pub fn receive(&self, dot: &str, bytes: &[u8]) -> Result<Option<i32>> {
        self.run_plain(
            dot,
            &["secrets".to_string(), "receive".to_string()],
            Some(bytes),
        )
    }

    /// Closes the shared connection.
    pub fn close(&self) {
        if self._control.is_some() {
            let c = Cmd::new("ssh")
                .arg("-O")
                .arg("exit")
                .args(self.ssh_opts.iter().cloned())
                .arg(&self.host);
            let _ = self.runner.output(&c);
        }
    }
}

/// The files to send: what git tracks or would track (not ignored), plus `.git`; everything
/// but `.git` when this is not a git checkout (or git is missing).
pub fn repo_files(runner: &dyn Runner, root: &Path) -> Result<Vec<PathBuf>> {
    let ls = Cmd::new("git").args([
        "-C".to_string(),
        root.display().to_string(),
        "ls-files".into(),
        "-z".into(),
        "--cached".into(),
        "--others".into(),
        "--exclude-standard".into(),
    ]);
    let mut out: Vec<PathBuf> = Vec::new();
    match runner.output(&ls) {
        Ok(o) if o.success() => {
            out.extend(
                o.stdout
                    .split('\0')
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    // Deleted but still in the index: nothing to send.
                    .filter(|p| std::fs::symlink_metadata(root.join(p)).is_ok()),
            );
            walk(root, Path::new(".git"), &mut out)?;
        }
        _ => {
            for entry in std::fs::read_dir(root)? {
                let name = entry?.file_name();
                if name != ".git" {
                    walk(root, Path::new(&name), &mut out)?;
                }
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// What git ignores in the repository at `root`, by any rule (`.gitignore`, `.git/info/exclude`,
/// `core.excludesFile`): paths relative to it, an ignored directory as `dir/`. Empty when this
/// is not a git checkout (the `.gitignore` filters still apply).
pub fn ignored_files(runner: &dyn Runner, root: &Path) -> Result<Vec<String>> {
    let ls = Cmd::new("git").args([
        "-C".to_string(),
        root.display().to_string(),
        "ls-files".into(),
        "-z".into(),
        "--others".into(),
        "--ignored".into(),
        "--exclude-standard".into(),
        "--directory".into(),
    ]);
    Ok(match runner.output(&ls) {
        Ok(o) if o.success() => o
            .stdout
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    })
}

/// An rsync exclude file naming exactly `paths` (anchored at the transfer's root, wildcards
/// escaped).
pub fn exclude_file(paths: &[String]) -> Result<tempfile::NamedTempFile> {
    let mut f = tempfile::Builder::new()
        .prefix("dot-rsync-exclude-")
        .tempfile()
        .context("creating a temporary file")?;
    for p in paths {
        let mut line = String::from("/");
        for c in p.chars() {
            if matches!(c, '*' | '?' | '[' | '\\') {
                line.push('\\');
            }
            line.push(c);
        }
        writeln!(f, "{line}").context("writing the rsync exclude list")?;
    }
    f.flush()?;
    Ok(f)
}

/// Adds `rel` (under `root`) and, for a directory, everything under it (links not followed).
fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let Ok(meta) = std::fs::symlink_metadata(root.join(rel)) else {
        return Ok(());
    };
    if !meta.is_dir() {
        out.push(rel.to_path_buf());
        return Ok(());
    }
    for entry in std::fs::read_dir(root.join(rel))? {
        walk(root, &rel.join(entry?.file_name()), out)?;
    }
    Ok(())
}

/// A gzipped tar of `files` (relative to `root`), links kept as links.
pub fn tar_gz(root: &Path, files: &[PathBuf]) -> Result<Vec<u8>> {
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    b.follow_symlinks(false);
    for f in files {
        b.append_path_with_name(root.join(f), f)
            .with_context(|| format!("archiving {}", f.display()))?;
    }
    let mut gz = b.into_inner().context("archiving the repository")?;
    gz.flush()?;
    Ok(gz.finish()?)
}

/// What `dot host-info` prints (one JSON line): what this machine needs to know to plan a run
/// there. Names only: never a value from its `local.env`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostInfo {
    /// `dot --version` there.
    pub version: String,
    pub home: String,
    /// Its facts, as `DOT_FACTS` takes them ([`facts_string`]).
    pub facts: String,
    /// `DOT_PROFILE` there (its environment, else its `local.env`).
    pub profile: Option<String>,
    /// The variable names its `local.env` gives a value (not `DOT_REPO` / `DOT_PROFILE` /
    /// `DOT_ROLE`).
    #[serde(default)]
    pub local_env: Vec<String>,
}

impl HostInfo {
    /// What the probe alone tells (`dot` is not installed there yet): OS, CPU and home.
    pub fn from_probe(p: &Probe) -> HostInfo {
        HostInfo {
            version: String::new(),
            home: p.home.clone(),
            facts: format!("os={},arch={}", p.target.os, p.target.arch),
            profile: None,
            local_env: Vec::new(),
        }
    }

    /// Its facts, and the names of those this dot does not know (a newer dot there), which are
    /// left out.
    pub fn facts(&self) -> Result<(dot_core::Facts, Vec<String>)> {
        use dot_core::facts::{Fact, Os, Role};
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        for pair in self
            .facts
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            let name = pair.split_once('=').map_or(pair, |(k, _)| k).trim();
            if Fact::ALL.iter().any(|f| f.as_str() == name) {
                known.push(pair);
            } else {
                unknown.push(send::printable(name));
            }
        }
        let mut f = dot_core::Facts::new(Os::Linux, "x86_64", Role::Desktop);
        f.apply_overrides(&known.join(","))
            .map_err(|e| anyhow::anyhow!("the facts reported there: {}", send::printable(&e)))?;
        Ok((f, unknown))
    }
}

/// `facts` as `fact=value` pairs, `env` values joined with `+` ([`dot_core::Facts::apply_overrides`]).
pub fn facts_string(facts: &dot_core::Facts) -> String {
    dot_core::facts::Fact::ALL
        .iter()
        .filter_map(|f| {
            let v = facts.values(*f);
            (!v.is_empty()).then(|| format!("{f}={}", v.join("+")))
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// `dot host-info`.
pub fn host_info() -> Result<ExitCode> {
    let inputs = crate::commands::inputs(&RepoArgs::default())?;
    let local = inputs.local_control().map_err(anyhow::Error::msg)?;
    let facts = crate::commands::detect(inputs.role_override(&local).map_err(anyhow::Error::msg)?);
    let profile = inputs
        .env
        .get("DOT_PROFILE")
        .filter(|p| !p.is_empty())
        .or_else(|| local.get("DOT_PROFILE"))
        .cloned();
    let file = dot_core::vars::EnvFile {
        path: inputs.local_env(),
        rank: dot_core::vars::Rank::Local,
        label: "local.env".into(),
        required: false,
    };
    let mut local_env: Vec<String> = dot_core::vars::load_env_files(&[file], &[])
        .map_err(anyhow::Error::msg)?
        .into_iter()
        .flat_map(|l| l.values.into_iter())
        .filter(|(k, v)| {
            !dot_core::workspace::CONTROL_KEYS.contains(&k.as_str())
                && !dot_core::vars::value_string(v).is_empty()
        })
        .map(|(k, _)| k)
        .collect();
    local_env.sort();
    local_env.dedup();
    let info = HostInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        home: inputs.home.display().to_string(),
        facts: facts_string(&facts),
        profile,
        local_env,
    };
    println!("{}", serde_json::to_string(&info)?);
    Ok(ExitCode::SUCCESS)
}

/// Works out, here, what to hand the run there and which profile it runs, from what
/// [`Remote::host_info`] reported. With `dry_run`, it only says what it would send (resolving
/// nothing, asking nothing) and hands over nothing.
pub type Prepare<'a> = Box<dyn FnMut(&HostInfo, bool) -> Result<send::Prepared> + 'a>;

/// What `dot apply --host` / `dot plan --host` does.
pub struct Job<'a> {
    pub remote: &'a Remote,
    /// `apply` or `plan`.
    pub command: &'static str,
    /// The local repository's root.
    pub root: PathBuf,
    pub remote_repo: Option<String>,
    /// Arguments after `dot <command> --repo <dir>`: profile, flags, bundles.
    pub args: Vec<String>,
    /// `dot apply --dry-run`: nothing is installed, synced, sent or applied there.
    pub dry_run: bool,
    /// What to hand over; `None`: nothing (and `dot host-info` is not asked).
    pub prepare: Option<Prepare<'a>>,
}

impl Job<'_> {
    pub fn run(&mut self) -> Result<ExitCode> {
        let r = self.remote;
        let probe = r.probe()?;
        let dir = match &self.remote_repo {
            Some(d) if d.starts_with('/') => d.clone(),
            Some(d) => format!("{}/{}", probe.home, d.trim_start_matches("~/")),
            None => format!("{}/{DEFAULT_REMOTE_REPO}", probe.home),
        };
        let mut args = vec![self.command.to_string(), "--repo".to_string(), dir.clone()];
        args.extend(self.args.iter().cloned());
        if self.dry_run {
            return self.dry(&probe, &dir, &args);
        }
        let dot = r.ensure_dot(&probe)?;
        // Worked out (and anything missing reported) before the repository is synced.
        let prepared = match self.prepare.as_mut() {
            Some(prepare) => {
                let info = r.host_info(&dot)?;
                prepare(&info, false)?
            }
            None => send::Prepared::default(),
        };
        // The run there plans with the profile planned here.
        with_profile(&mut args, prepared.profile.as_deref());
        r.sync(&probe, &self.root, &dir)?;
        let code = match prepared.handoff.filter(|h| !h.is_empty()) {
            Some(h) => {
                let bytes = h.encode()?;
                let code = r.run(&dot, &args, Some(&bytes))?;
                let keeps = h.persist || h.vars.iter().any(|v| v.keep);
                r.say(format!(
                    "sent {} ({})",
                    h.describe(),
                    match (keeps, code == Some(0)) {
                        (false, _) => "for that run only; nothing kept there",
                        (true, true) if h.persist => "kept there",
                        (true, true) => "for that run only; per-machine answers kept there",
                        (true, false) => {
                            "the run there did not succeed: kept only if it got past its \
                             confirmation"
                        }
                    }
                ));
                code
            }
            None => r.run(&dot, &args, None)?,
        };
        r.close();
        Ok(match code {
            Some(0) => ExitCode::SUCCESS,
            Some(c) => ExitCode::from(u8::try_from(c).unwrap_or(1).max(1)),
            None => ExitCode::FAILURE,
        })
    }

    /// `--dry-run`: says what would be done; changes nothing there and sends nothing.
    fn dry(&mut self, probe: &Probe, dir: &str, args: &[String]) -> Result<ExitCode> {
        let r = self.remote;
        r.say("dry run: nothing is installed, synced, sent or applied there");
        // With --dot-binary: the dot there that would be used (planned from), or the binary
        // checked against the remote's platform as it would be before copying.
        let existing = match &r.dot_binary {
            None => probe.dot.clone(),
            Some(bin) => match r.with_binary(probe, bin)? {
                Given::Use(path, note) => {
                    r.say(note);
                    Some((path, String::new()))
                }
                Given::Copy(note) => {
                    let (_, dot) = r.binary_dest(probe, bin)?;
                    if let Some(n) = note {
                        r.say(n);
                    }
                    r.say(format!(
                        "would copy {} ({}) to {dot}",
                        bin.path.display(),
                        bin.kind.describe()
                    ));
                    None
                }
            },
        };
        let info = match &existing {
            Some((path, version)) => {
                if !version.is_empty() {
                    r.say(format!("would use {path} ({version})"));
                }
                match r.host_info(path) {
                    Ok(i) => i,
                    Err(e) => {
                        r.say(format!("{e:#}; planning from its OS and CPU only"));
                        HostInfo::from_probe(probe)
                    }
                }
            }
            None if r.dot_binary.is_some() => HostInfo::from_probe(probe),
            None => {
                r.say(format!(
                    "would install dot for {}/{} (planning from its OS and CPU only)",
                    probe.target.os, probe.target.arch
                ));
                HostInfo::from_probe(probe)
            }
        };
        r.say(format!("would sync {} to {dir}", self.root.display()));
        let mut args: Vec<String> = args.iter().filter(|a| *a != "--dry-run").cloned().collect();
        if let Some(prepare) = self.prepare.as_mut() {
            with_profile(&mut args, prepare(&info, true)?.profile.as_deref());
        }
        let shown: Vec<String> = args.iter().map(|a| sh_quote(a)).collect();
        r.say(format!("would run `dot {}` there", shown.join(" ")));
        r.close();
        Ok(ExitCode::SUCCESS)
    }
}

/// With `--host`: which secrets go to the run, and whether the remote keeps them.
#[derive(Args, Clone, Default)]
pub struct SendArgs {
    /// With --host: send every secret secrets.yml declares that resolves here, not only those
    /// the remote's bundles use.
    #[arg(long, requires = "host", conflicts_with = "no_secrets")]
    pub all_secrets: bool,
    /// With --host: send no secrets (the remote resolves its own).
    #[arg(long, requires = "host")]
    pub no_secrets: bool,
    /// With --host: keep what is sent on the remote, so later runs there work on their own:
    /// secrets in its ~/.config/dot/secrets.env and files/ (owner-only), forwarded `persist:
    /// local` variables in its local.env. Default: for that run only, in memory.
    #[arg(long, requires = "host")]
    pub persist_secrets: bool,
    /// With --persist-secrets: replace values the remote already holds that differ.
    #[arg(long, requires = "persist_secrets")]
    pub force_secrets: bool,
}

/// Refuses local-only options with --host.
fn check_local_only(repo: &RepoArgs) -> Result<()> {
    if !repo.env_files.is_empty() {
        bail!("--env-file names a local file; it cannot be used with --host");
    }
    Ok(())
}

/// The local repository's root, found as every command finds it.
pub(crate) fn local_root(repo: &RepoArgs) -> Result<PathBuf> {
    let inputs = crate::commands::inputs(repo)?;
    let local = inputs.local_control().map_err(anyhow::Error::msg)?;
    let located = dot_core::repo::find_repo(
        inputs.repo.as_deref(),
        inputs.env.get("DOT_REPO").map(String::as_str),
        &inputs.cwd,
        local.get("DOT_REPO").map(String::as_str),
        &inputs.home,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(located.root)
}

/// `--profile`, for the remote (`--var`s travel with what is handed over, never on its command
/// line).
fn profile_flag(repo: &RepoArgs) -> Vec<String> {
    match &repo.profile {
        Some(p) => vec!["--profile".to_string(), p.clone()],
        None => Vec::new(),
    }
}

/// Adds `--profile <profile>` after `<command> --repo <dir>` unless a profile is given already.
fn with_profile(args: &mut Vec<String>, profile: Option<&str>) {
    if let Some(p) = profile
        && !args.iter().any(|a| a == "--profile")
    {
        let at = args.len().min(3);
        args.splice(at..at, ["--profile".to_string(), p.to_string()]);
    }
}

/// Whether this side can ask questions (a terminal, and not `--yes`).
fn interactive(yes: bool) -> bool {
    !yes && std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// `dot apply --host`.
pub fn apply(a: &crate::ApplyArgs, host: &str) -> Result<ExitCode> {
    check_local_only(&a.repo)?;
    let root = local_root(&a.repo)?;
    let mut args = profile_flag(&a.repo);
    for (on, flag) in [
        (a.yes, "--yes"),
        (a.dry_run, "--dry-run"),
        (a.force, "--force"),
        (a.upgrade, "--upgrade"),
        (a.no_deps, "--no-deps"),
        (a.quiet, "--quiet"),
        (a.plain, "--plain"),
        (a.json, "--json"),
    ] {
        if on {
            args.push(flag.to_string());
        }
    }
    if a.jobs > 1 {
        args.extend(["--jobs".to_string(), a.jobs.to_string()]);
    }
    if a.verbose > 0 {
        args.push(format!("-{}", "v".repeat(usize::from(a.verbose))));
    }
    match a.color {
        crate::render::ColorChoice::Auto => {}
        crate::render::ColorChoice::Always => args.extend(["--color".into(), "always".into()]),
        crate::render::ColorChoice::Never => args.extend(["--color".into(), "never".into()]),
    }
    args.extend(a.bundles.iter().cloned());
    let given = HostBinary::given(a.remote.dot_binary.clone())?;
    let mut remote = Remote::connect(host)?;
    remote.dot_binary = given;
    let sender = send::Sender {
        host: host.to_string(),
        root: root.clone(),
        repo: a.repo.clone(),
        bundles: a.bundles.clone(),
        no_deps: a.no_deps,
        secrets: if a.send.no_secrets {
            send::Which::None
        } else if a.send.all_secrets {
            send::Which::All
        } else {
            send::Which::Needed
        },
        persist: a.send.persist_secrets,
        force: a.send.force_secrets,
        interactive: interactive(a.yes),
        ask_per_machine: true,
    };
    Job {
        remote: &remote,
        command: "apply",
        root,
        remote_repo: a.remote.remote_repo.clone(),
        args,
        dry_run: a.dry_run,
        prepare: Some(Box::new(move |info, dry| sender.prepare(info, dry))),
    }
    .run()
}

/// `dot plan --host`: the run there gets the forwarded variables, never a secret (planning
/// resolves none).
pub fn plan(a: &crate::PlanArgs, host: &str) -> Result<ExitCode> {
    check_local_only(&a.repo)?;
    let root = local_root(&a.repo)?;
    let mut args = profile_flag(&a.repo);
    for (on, flag) in [
        (a.force, "--force"),
        (a.upgrade, "--upgrade"),
        (a.no_deps, "--no-deps"),
        (a.json, "--json"),
    ] {
        if on {
            args.push(flag.to_string());
        }
    }
    args.extend(a.bundles.iter().cloned());
    let given = HostBinary::given(a.remote.dot_binary.clone())?;
    let mut remote = Remote::connect(host)?;
    remote.dot_binary = given;
    let sender = send::Sender {
        host: host.to_string(),
        root: root.clone(),
        repo: a.repo.clone(),
        bundles: a.bundles.clone(),
        no_deps: a.no_deps,
        secrets: send::Which::None,
        persist: false,
        force: false,
        // To confirm sending sensitive variables to a profile the remote reported.
        interactive: interactive(false),
        ask_per_machine: false,
    };
    Job {
        remote: &remote,
        command: "plan",
        root,
        remote_repo: a.remote.remote_repo.clone(),
        args,
        dry_run: false,
        prepare: Some(Box::new(move |info, dry| sender.prepare(info, dry))),
    }
    .run()
}

pub mod binary;
pub mod send;

#[cfg(all(test, unix))]
mod tests;
