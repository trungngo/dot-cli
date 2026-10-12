//! Failure hints: what a failed step's error and output say, in terms of what to do about it.
//!
//! [`RULES`] is a table of regular expressions over the error message and the step's last
//! output lines (what the failure block shows), each with a hint. The first rule that matches
//! wins, so specific rules come before general ones (a cask's `.pkg` installer failing on sudo
//! before sudo failing without a terminal). Every renderer prints the hint under the failure
//! block; `--json` has it as `hint` on `step_finished`.

use std::sync::OnceLock;

use regex::Regex;

/// One known failure.
pub struct Rule {
    /// Short name, for tests and the log.
    pub name: &'static str,
    /// Matched (case-insensitively) against the error message and each output line.
    pub pattern: &'static str,
    pub hint: &'static str,
}

pub const RULES: &[Rule] = &[
    Rule {
        // `github_release`, and `download` from a release: the checksum is the release's own
        // (named in parentheses), not a pinned `sha256:`.
        name: "release-checksum",
        pattern: r"sha256 mismatch: expected \S+ \(",
        hint: "the downloaded asset does not match the checksum its release publishes: the \
               download was corrupted or cut short, or the release is being re-uploaded. Run \
               again; if it persists, report it to the project (nothing was installed)",
    },
    Rule {
        name: "sha256-mismatch",
        pattern: r"sha256 mismatch|checksum mismatch|does not match (its|the release's) sha256",
        hint: "the file downloaded is not the one pinned: a new version may have replaced it \
               under the same URL, or the download was corrupted. Check the new file (`curl -fsSL \
               <url> | sha256sum`) and update `sha256:`, or drop `sha256:` to follow upstream",
    },
    Rule {
        name: "download-http",
        pattern: r"downloading https?://\S+: HTTP (4\d\d|5\d\d)",
        hint: "the download URL answered with an HTTP error: check `url:` (versioned file names \
               change with each release: `release:` or `latest:` on `download` follow them), \
               or try again later for a 5xx",
    },
    Rule {
        name: "archive-escape",
        pattern: r"has an entry outside its directory|zip.*(invalid|outside).*path",
        hint: "the archive would write outside the destination, which `dot` refuses on purpose: \
               make sure `url:` is the archive you expect",
    },
    Rule {
        name: "package-file-deps",
        pattern: r"dependency problems|unmet dependencies|depends: \S+.* but it is not (going to be )?installable|nothing provides|problem: conflicting requests",
        hint: "the package file needs packages this system cannot install: add their \
               repository first (`package_repo`), install them with a `package` step before this \
               one, or pick the build for this release (`asset:` on `github_release`)",
    },
    Rule {
        name: "package-file-arch",
        pattern: r"package architecture \(\S+\) does not match system|intended for a different architecture|wrong architecture",
        hint: "the package file is for another CPU architecture: point `url:` at the build for \
               this machine (`{{ arch }}` in the URL), or narrow `asset:` on `github_release`",
    },
    Rule {
        name: "systemd-not-running",
        pattern: r"system has not been booted with systemd|failed to connect to bus|systemd is not running|not booted with systemd|cannot manage service .*systemd",
        hint: "systemd is not running here (a container, or WSL without systemd): in WSL set \
               `[boot]` `systemd=true` in /etc/wsl.conf and run `wsl --shutdown`; elsewhere skip \
               the step with `when: { init: systemd }`",
    },
    Rule {
        name: "dpkg-lock",
        pattern: r"could not get lock /var/lib/(dpkg|apt)|unable to acquire the dpkg frontend lock|dpkg frontend lock|waiting for cache lock|is another process using it\?",
        hint: "another apt/dpkg process holds the package lock (on a fresh machine usually \
               unattended-upgrades): wait for it to finish (`ps aux | grep -E 'apt|dpkg'`), then \
               run `dot apply` again",
    },
    Rule {
        name: "apt-no-package",
        pattern: r"unable to locate package|has no installation candidate|no packages found matching",
        hint: "apt does not know this package: the package lists may be stale (`sudo apt-get \
               update`), the name may differ on this release (map it in dot.yml's `packages:`), \
               or its repository is missing (add it with a `package_repo` step before this one)",
    },
    Rule {
        name: "brew-missing",
        pattern: r"\b(brew|homebrew) is not installed",
        hint: "Homebrew is not installed: install it from https://brew.sh, or add a step before \
               this one that does (examples/basic's `common` bundle has one), with \
               `path: [\"{{ homebrew_prefix }}/bin\"]` so later steps find `brew`",
    },
    Rule {
        name: "brew-untrusted-tap",
        pattern: r"untrusted tap|tap .* is not trusted|refusing to (load|install) .* from .*tap",
        hint: "Homebrew does not trust this tap: add it with a `package_repo: { brew: ... }` step \
               (which trusts it unless `trust: false`), or run `brew trust --tap <tap>` once",
    },
    Rule {
        name: "brew-adopt-conflict",
        pattern: r"it seems there is already an app at|already an app at .* installed|the existing app is different",
        hint: "an app of the same name is already installed outside Homebrew and differs from the \
               cask's: quit it and move it out of /Applications (or remove it), then run again; \
               `brew install --cask --force <name>` replaces it",
    },
    Rule {
        name: "cask-pkg-sudo",
        pattern: r"running installer for .* with sudo|installer: must be run as root|\.pkg.*(sudo|password)|sudo.*\.pkg",
        hint: "this cask runs a .pkg installer, which needs sudo: run `dot apply` from a terminal \
               (not with --yes / no TTY) so sudo is asked for, or install it once by hand with \
               `brew install --cask <name>`",
    },
    Rule {
        name: "sudo-no-tty",
        pattern: r"sudo: a terminal is required|sudo: no tty present|sudo: a password is required|sudo: .*askpass",
        hint: "sudo wanted a password but had no terminal to ask on: run `dot apply` \
               interactively (sudo is asked for once, up front), run `sudo -v` first, or mark \
               the step `sudo: true` so `dot` asks before it runs",
    },
    Rule {
        name: "permission-system-dir",
        pattern: r"(permission denied|operation not permitted|eacces|read-only file system).*(/etc/|/usr/)|(/etc/|/usr/)\S*.*(permission denied|operation not permitted|eacces)",
        hint: "writing under /etc or /usr needs root: mark the entry `root: true` (`template`, \
               `copy`, `block`, `create`, `download`), or use a `shell` step with `sudo: true` \
               (sudo is asked for once, up front); on macOS /usr (outside /usr/local) is \
               read-only",
    },
    Rule {
        name: "dns",
        pattern: r"could not resolve host|temporary failure in name resolution|name or service not known|nodename nor servname|no such host|failed to lookup address|dns error|getaddrinfo",
        hint: "a host name could not be resolved: check the network connection and DNS \
               (`/etc/resolv.conf`; in WSL or a container, the host's DNS settings or a VPN), \
               then run again",
    },
    Rule {
        name: "tls",
        pattern: r"certificate verify failed|ssl certificate problem|unable to get local issuer certificate|self[- ]signed certificate|invalid peer certificate|unknownissuer|x509: |certificate has expired|tls handshake|ssl_connect|schannel",
        hint: "TLS certificate verification failed: install or update the CA certificates \
               (`ca-certificates`), check the system clock, or, behind an HTTPS-inspecting \
               proxy, add its CA to the system store",
    },
    Rule {
        name: "github-rate-limit",
        pattern: r"api rate limit exceeded|rate limit exceeded|x-ratelimit-remaining: 0|secondary rate limit",
        hint: "GitHub's API rate limit was hit (60 requests an hour without a token): set \
               GITHUB_TOKEN (or GH_TOKEN), or declare a `github_token` secret in secrets.yml",
    },
    Rule {
        name: "git-dirty",
        pattern: r"has local changes; commit or stash|your local changes to the following files would be overwritten|please commit your changes or stash them|untracked working tree files would be overwritten",
        hint: "the checkout has local changes that `dot` will not overwrite: commit or stash \
               them (`git -C <dir> stash`), or discard them, then run again",
    },
    Rule {
        name: "snap-unavailable",
        pattern: r"snap.*cannot communicate with server|cannot communicate with server.*snap|system does not fully support snapd|snapd is not available|snapd.*not running|cannot run snap|snap: command not found",
        hint: "snapd does not run here (containers, WSL without systemd): install the package \
               with the distro's manager instead (`package` falls back to it when snapd is \
               absent), or skip the step with `when: { env: \"!container\" }`",
    },
    Rule {
        name: "winget-elevation",
        pattern: r"0x8a150056|0x800702e4|requires elevation|requires administrator|run as administrator|administrator privileges|elevated (terminal|prompt|shell)",
        hint: "this needs an elevated terminal: run `dot apply` from a terminal started with \
               \"Run as administrator\" (machine-scope winget installs and winget sources need it)",
    },
];

fn compiled() -> &'static [Regex] {
    static RE: OnceLock<Vec<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        RULES
            .iter()
            .map(|r| Regex::new(&format!("(?i){}", r.pattern)).expect("hint rule compiles"))
            .collect()
    })
}

/// The rule that matches the error or one of the output lines, if any.
pub fn rule<'a>(
    error: Option<&'a str>,
    output: impl IntoIterator<Item = &'a str>,
) -> Option<&'static Rule> {
    let texts: Vec<&str> = error.into_iter().chain(output).collect();
    compiled()
        .iter()
        .zip(RULES)
        .find(|(re, _)| texts.iter().any(|t| re.is_match(t)))
        .map(|(_, r)| r)
}

/// The hint for a failed step.
pub fn hint<'a>(
    error: Option<&'a str>,
    output: impl IntoIterator<Item = &'a str>,
) -> Option<&'static str> {
    rule(error, output).map(|r| r.hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(error: &str, output: &[&str]) -> Option<&'static str> {
        rule(Some(error), output.iter().copied()).map(|r| r.name)
    }

    #[test]
    fn every_rule_compiles_and_names_are_unique() {
        assert_eq!(compiled().len(), RULES.len());
        let mut names: Vec<&str> = RULES.iter().map(|r| r.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), RULES.len());
    }

    #[test]
    fn known_failures_get_their_hint() {
        let cases: &[(&str, &[&str], &str)] = &[
            (
                "command exited with code 100",
                &[
                    "E: Could not get lock /var/lib/dpkg/lock-frontend. It is held by process 1234 (unattended-upgr)",
                ],
                "dpkg-lock",
            ),
            (
                "apt-get install failed",
                &["E: Unable to locate package fd-find"],
                "apt-no-package",
            ),
            (
                "exit 100",
                &["E: Package 'neovim' has no installation candidate"],
                "apt-no-package",
            ),
            (
                "brew install failed",
                &["Error: Refusing to load formula from untrusted tap acme/tools"],
                "brew-untrusted-tap",
            ),
            ("brew: brew is not installed", &[], "brew-missing"),
            ("Homebrew is not installed", &[], "brew-missing"),
            (
                "brew install --cask failed",
                &["Error: It seems there is already an App at '/Applications/Foo.app'."],
                "brew-adopt-conflict",
            ),
            (
                "brew install --cask failed",
                &[
                    "==> Running installer for zoom with sudo; the password may be necessary.",
                    "sudo: a terminal is required to read the password",
                ],
                "cask-pkg-sudo",
            ),
            (
                "command exited with code 1",
                &["sudo: a terminal is required to read the password; either use the -S option"],
                "sudo-no-tty",
            ),
            (
                "command exited with code 1",
                &["cp: cannot create regular file '/etc/sysctl.d/99-x.conf': Permission denied"],
                "permission-system-dir",
            ),
            (
                "creating /usr/local/share/x: Permission denied (os error 13)",
                &[],
                "permission-system-dir",
            ),
            (
                "command exited with code 6",
                &["curl: (6) Could not resolve host: github.com"],
                "dns",
            ),
            (
                "downloading https://x: dns error: failed to lookup address information",
                &[],
                "dns",
            ),
            (
                "command exited with code 60",
                &["curl: (60) SSL certificate problem: unable to get local issuer certificate"],
                "tls",
            ),
            ("io: invalid peer certificate: UnknownIssuer", &[], "tls"),
            (
                "GitHub API rate limit exceeded (resets in 12 min)",
                &[],
                "github-rate-limit",
            ),
            (
                "~/src/x has local changes; commit or stash them",
                &[],
                "git-dirty",
            ),
            (
                "command exited with code 1",
                &[
                    "error: Your local changes to the following files would be overwritten by merge:",
                ],
                "git-dirty",
            ),
            (
                "snap install failed",
                &[
                    "error: cannot communicate with server: Post http://localhost/v2/snaps/x: dial unix /run/snapd.socket",
                ],
                "snap-unavailable",
            ),
            ("winget install failed: 0x8A150056", &[], "winget-elevation"),
            (
                "https://dl.test/go.tar.gz: sha256 mismatch: expected 49bf, downloaded 0a1b; \
                 nothing installed",
                &[],
                "sha256-mismatch",
            ),
            (
                "fzf-0.56.0-linux_amd64.tar.gz: sha256 mismatch: expected 49bf (checksums.txt), \
                 downloaded 0a1b; nothing installed",
                &[],
                "release-checksum",
            ),
            (
                "downloading https://dl.test/tool_1.2_amd64.deb: HTTP 404",
                &[],
                "download-http",
            ),
            (
                "the archive has an entry outside its directory (../etc/passwd); refusing to \
                 unpack it",
                &[],
                "archive-escape",
            ),
            (
                "command exited with code 100",
                &[
                    "The following packages have unmet dependencies:",
                    " tool : Depends: libfoo2 but it is not installable",
                ],
                "package-file-deps",
            ),
            (
                "command exited with code 1",
                &["E: Package architecture (arm64) does not match system (amd64)"],
                "package-file-arch",
            ),
            (
                "command exited with code 1",
                &[
                    "System has not been booted with systemd as init system (PID 1). Can't operate.",
                    "Failed to connect to bus: Host is down",
                ],
                "systemd-not-running",
            ),
            (
                "cannot manage service ssh: init is `other`, not systemd",
                &[],
                "systemd-not-running",
            ),
            (
                "winget source add failed",
                &["This operation requires administrator privileges."],
                "winget-elevation",
            ),
        ];
        for (error, output, want) in cases {
            assert_eq!(name(error, output), Some(*want), "{error} / {output:?}");
        }
    }

    #[test]
    fn unknown_failures_get_none() {
        assert_eq!(
            name("command exited with code 2", &["make: *** [all] Error 2"]),
            None
        );
        assert_eq!(hint(None, []), None);
    }
}
