# Changelog

All notable changes to `dot` are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (before 1.0, a minor version may change the config
format; such changes are called out).

## [0.1.0] - Unreleased

### Added

- First public release. `dot` bootstraps and maintains macOS, Ubuntu (desktop and server),
  Windows and WSL machines from a repository of YAML bundles:
  - Profiles matched by detected facts, `when:` conditions, bundles ordered as a dependency
    graph, variables in layers (`dot.yml`, profile, bundle, `.env*`, local, `--var`), prompts
    that are remembered.
  - Directives: `link`, `copy`, `template`, `block`, `create`, `clean`, `absent`, `shell`,
    `package` (apt, dnf, zypper, pacman, apk, Homebrew and casks, snap, winget, scoop, VS Code
    extensions), `package_repo`, `github_release` (GitHub and GitLab), `download`, `git`,
    `service` (systemd, launchd, Windows services), `macos_defaults`, `user`, `path` / `env`,
    `secret_file`, and handlers (`notify:`).
  - `dot plan`, `apply --dry-run`, `status` (drift and orphans), `diff`, `remove` (undo),
    `adopt`, `doctor`, `graph`, `why`, `vars`, `facts`, `env --shell`, `schema`, `check`.
  - Secrets by logical name from an owner-only env file or LastPass (`secrets.yml`), redacted
    everywhere; `dot secrets check|push|export|import`.
  - `dot apply --host` (over ssh), `dot cloud-init` (installs the same version as the `dot`
    that printed it; `--latest` for the latest), `dot self-update`, `--jobs N`.
  - `install.sh` / `install.ps1`: no token needed for dot's public releases; a GitHub token is
    used only for a private dotfiles repository (and only when git cannot read it with its own
    credentials) or a private fork's releases (`DOT_RELEASE_REPO`). `DOT_VERSION=vX.Y.Z`
    installs a given release; ssh repository URLs (`git@host:owner/name.git`) are cloned with
    your ssh keys.
  - `github_release: { tag, sha256 }` to pin a third party's release and its asset's digest.
  - Release assets carry signed build provenance attestations (`gh attestation verify`).
  - JSON Schemas for `dot.yml`, `bundle.yml` and `secrets.yml` published with each release,
    and a configuration reference generated from them (`docs/reference.md`).
- `examples/basic`, a starting repository, checked in CI.
- `dot` finds a repository in `~/.dot` (where the installers clone one, and `dot apply --host`
  puts it on the remote) when nothing else names one. Not `~/.dotfiles`: that is where other
  setups (dotbot) keep theirs, so both can live side by side on one machine and `dot` never
  picks the other one up. `DOT_DOTFILES_DIR` (installers) and `--remote-repo` still choose
  another place. Neither overwrites something else already there: the installers refuse a
  directory that is not this repository's checkout, and `dot apply --host` refuses a remote
  directory that is not empty and holds no `dot.yml`.
- `DOT_FACTS` simulating another machine (`os=linux,distro=ubuntu`) derives the facts it
  doesn't name (`family`, `pkg`, `init`) instead of keeping this machine's.
- `DOT_RELEASE_REPO=owner/name` for forks: the installers, `dot self-update` and `dot apply
  --host` use that repository's releases. `DOT_GITHUB_URL` for the installers (the GitHub web
  host, for GitHub Enterprise and tests).
- A directory `copy` removes files it copied earlier that its source no longer has, while they
  are unedited (an edited one is kept and reported); `dot plan` / `status` / `diff` show them.
- `path: { <dir>: { persist: user } }` also adds the directory to the Windows user `Path`
  (`HKCU\Environment`); `dot remove` takes it out again.
- `dot apply --host` bootstraps a machine with this machine's secrets: the remote's run is
  planned here (from `dot host-info` there), the secrets its bundles use are resolved here
  through any provider (LastPass included, unlocked once) and handed to the run on ssh's stdin
  (through a private FIFO when ssh has a terminal), never on a command line, in the
  environment or in a file. The remote keeps them in memory for that run only;
  `--persist-secrets` keeps them in its envfile (owner-only; `--force-secrets` replaces values
  that differ). `--all-secrets` / `--no-secrets` choose what is sent. `persist: local`
  variables and `--var`s are forwarded too.
- `per_machine: true` on a variable declaration: `dot apply --host` never copies this
  machine's value; it asks for the remote's (or takes `--var`), or stops naming it.
- `dot secrets push user@host [NAME...]` resolves the secrets here, through any provider, and
  keeps them in the remote's envfile under each secret's envfile ref (`--force` replaces
  values that differ; `--dry-run` shows where each would go). A secret without an
  envfile ref in `secrets.yml` is not pushed.
- `dot apply --host --dry-run` installs, syncs and sends nothing; it says what it would do.
- `--dot-binary PATH` (default `DOT_HOST_BINARY`) on `dot apply --host`, `dot plan --host` and
  `dot secrets push` installs or upgrades `dot` on the remote from a file here, over scp, into
  `~/.local/bin/dot`, with nothing downloaded from GitHub and no fallback to a download. The
  file's ELF / Mach-O header must match the remote's OS and CPU; uploaded to a temporary name,
  it must say this version (`--version` there) before it is renamed into place. A newer `dot`
  there, or this very build (the same SHA-256), is left as it is; another build of this version
  is replaced, since every build from source says the same version. `scripts/build-linux.sh`
  cross-compiles the static musl binaries (x86_64, aarch64) with cargo-zigbuild.
- `dot apply --host` / `dot plan --host` / `dot secrets push` need the same or a newer `dot`
  on the remote: an older one stops the run before anything is resolved, with one
  instruction (`<that dot> self-update` there). The probe skips a `dot` that is not this tool
  (graphviz's). The remote run is given the profile planned here; a profile only the remote
  reports is confirmed here before secrets are sent to it (`--profile` without a terminal).
  The remote's own `local.env` values and kept secrets win over this machine's (unless
  `--force-secrets`), a per-machine answer asked for here is kept there, and nothing is kept
  before the remote run is confirmed.
- Homebrew casks that run a `.pkg` installer (Homebrew asks for the sudo password itself: it
  discards cached sudo credentials) never hang the run behind an unseen prompt. An
  interactive run gives them the terminal, the live view paused (their output still goes to the
  log, and to `--json` as events), and the plan says they ask there; a non-interactive run
  allows them only with passwordless sudo, and otherwise stops before applying anything, naming
  them. An installer script counts only with `sudo: true`, as for Homebrew. Other casks fail at
  once if they run `sudo` after all, saying to run that `brew` command by hand; they, formulae
  and queries leave the sudo timestamp alone (`HOMEBREW_NO_SUDO`, Homebrew 7.0.7+) unless sudo
  is passwordless.
  `sudo -n` is checked again before each action that needs root (asking again when a `brew`
  discarded the timestamp), and the sudo keep-alive never waits behind a password prompt.
- `dot plan` and the plan shown before `dot apply` list only what this machine will do or
  cannot do: steps and bundles skipped by `when:` (another OS or profile) and satisfied actions
  are left out. `-v` lists everything.
- Windows builds use the system TLS stack, for `dot self-update` and every HTTPS download.
- CI (fmt, clippy, tests on Linux, macOS and Windows, the minimum Rust version, an Ubuntu
  container) runs on pull requests and on pushes to main. The landing page (`site/`) is published to GitHub Pages when it changes. Pushing a `v*` tag starts the Release
  workflow: it builds every platform's binary, attests them, and publishes the release.

[0.1.0]: https://github.com/trungngo/dot-cli/releases/tag/v0.1.0
