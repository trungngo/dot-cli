# dot

[![CI](https://github.com/trungngo/dot-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/trungngo/dot-cli/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Website: <https://trungngo.github.io/dot-cli/>**

`dot` sets up and maintains a machine from a repository of dotfiles. It is one static binary
for macOS, Linux (Ubuntu desktop and server first) and Windows. It reads bundles of YAML from
your repository, works out what this machine is, and then links, copies and templates files,
installs packages and runs setup steps in dependency order. It can also tell you what it would
change before it changes anything.

```text
$ dot plan
repo: /Users/me/.dot (~/.dot)
profile: macos (matched os=macos)
bundles, in order:
   1. common      implicit; required by git
   2. api-client  selected by profile macos
   3. cli-tools   selected by profile macos
   4. git         selected by profile macos
   5. macos       selected by profile macos
   6. shell       selected by profile macos

asked for by `dot apply`: git_name, git_email

secrets needed (resolved when applying): example_api_token
...
cli-tools (/Users/me/.dot/bundles/cli-tools/bundle.yml)
  package fd, ripgrep
    ~ brew: install fd, ripgrep [pending]
git (/Users/me/.dot/bundles/git/bundle.yml)
  package git
    · brew: git installed [ok]
  template ~/.gitconfig
    ? template ~/.gitconfig <- gitconfig.j2 (new file, 8 lines) (uses git_email, git_name, asked when applying) [unknown]
...
shell (/Users/me/.dot/bundles/shell/bundle.yml)
  link ~/.inputrc, ~/.config/example-shell/aliases.sh
    ~ link ~/.inputrc -> inputrc [pending]
    ~ link ~/.config/example-shell/aliases.sh -> aliases.sh [pending]
  - block ~/.bashrc (skipped: when: os is macos, not linux)
  block ~/.zshrc
    ~ block ~/.zshrc [# >>> dot:shell >>>] (new file, 4 lines) [pending]
```

(`·` is in place, `~` would change, `?` is only known when applying, `-` is skipped here.)

- **Bundles and a graph.** Each bundle (`bundles/<name>/bundle.yml`) is a list of steps with
  `requires` / `after` edges. `dot` resolves them into one order, runs each bundle once, and
  skips the dependents of a bundle that fails.
- **Profiles and facts.** `dot` detects `os`, `distro`, `arch`, `role` (desktop or server),
  `env` (WSL, container, VM, ssh), the package manager and more. Profiles in `dot.yml` pick
  bundles by those facts, and `when:` conditions apply to a single step or a whole bundle.
- **Plan, then apply.** Every directive plans without side effects first. That makes `dot
  plan`, `dot apply --dry-run`, `dot status` (drift) and `dot diff` possible. `dot remove`
  undoes what was recorded.
- **Real directives.** `link`, `copy`, `template`, `block` (a managed block in a file you
  don't own), `package` (apt, dnf, zypper, pacman, apk, brew and casks, snap, winget, scoop,
  VS Code extensions, with per-manager names), `package_repo`, `github_release`, `download`,
  `git`, `service`, `macos_defaults`, `user`, `path` / `env`, `absent`, `shell`.
- **Secrets stay out of git.** Bundles ask for `secret("name")`, and `secrets.yml` says where
  each value lives: an owner-only env file (`envfile`) or LastPass (`lastpass`). Values are
  redacted in all output and logs.
- **Remote machines.** `dot apply --host user@server` installs `dot` on the server, syncs the
  repository, resolves the secrets its bundles need here (LastPass included, unlocked once),
  hands them to the run there on ssh's stdin, in memory only (`--persist-secrets` keeps them),
  forwards your personal variables except per-machine ones, and runs there. `dot cloud-init`
  prints user data that does the same on first boot.

## Install

The installers download the latest release, check it against the release's `SHA256SUMS`, and
install `dot` to `~/.local/bin` (`%LOCALAPPDATA%\Programs\dot` on Windows). No token or root
access is needed.

macOS / Linux:

    curl -fsSL https://raw.githubusercontent.com/trungngo/dot-cli/main/install.sh | sh

Windows (Windows PowerShell 5.1 or PowerShell 7):

    iwr -useb https://raw.githubusercontent.com/trungngo/dot-cli/main/install.ps1 | iex

If `~/.local/bin` is not on your `PATH`, the installer prints the line to add to your shell's
startup file. On a fresh Ubuntu desktop, run `sudo apt install -y curl git` first. You can also
download a binary from the [releases page](https://github.com/trungngo/dot-cli/releases),
or build one from source with `cargo install --locked --git
https://github.com/trungngo/dot-cli dot-cli`. Later, `dot self-update` replaces the binary
with the latest release.

**A pinned version.** `DOT_VERSION` installs one release instead of the latest. Take the
installer from the same tag, so that the script and the binary come from one release:

    curl -fsSL https://raw.githubusercontent.com/trungngo/dot-cli/v0.1.0/install.sh \
      | DOT_VERSION=v0.1.0 sh

    $env:DOT_VERSION = 'v0.1.0'
    iwr -useb https://raw.githubusercontent.com/trungngo/dot-cli/v0.1.0/install.ps1 | iex

**Checking where a binary came from.** `SHA256SUMS` only catches a corrupted download. Every
release asset also has a signed build provenance attestation, which proves it was built by this
repository's release workflow from the release's tag. With the GitHub CLI:

    gh attestation verify dot-universal-apple-darwin.tar.gz -R trungngo/dot-cli

**Bootstrapping a new machine.** With the URL of your dotfiles repository, the installer also
clones it to `~/.dot`, records it as the default repository, and runs `dot apply`:

    curl -fsSL https://raw.githubusercontent.com/trungngo/dot-cli/main/install.sh \
      | sh -s -- https://github.com/you/dotfiles.git

    $env:DOT_DOTFILES_URL = 'https://github.com/you/dotfiles.git'
    iwr -useb https://raw.githubusercontent.com/trungngo/dot-cli/main/install.ps1 | iex

The URL can be `https://` or ssh (`git@github.com:you/dotfiles.git`, `ssh://...`). An ssh URL
uses your ssh keys, which must already be set up on the new machine.

### Private dotfiles

A private dotfiles repository needs credentials for the first clone. Use an ssh URL if the
machine already has a key that can read the repository. Otherwise, for github.com, create a
[fine-grained personal access token](https://github.com/settings/personal-access-tokens/new)
that can read only that repository (**Contents: Read-only**, with a short expiry). Then hand
it to the installer without putting it in shell history or on any command line:

    read -rs GITHUB_TOKEN     # paste the token, then Enter; nothing is echoed
    curl -fsSL https://raw.githubusercontent.com/trungngo/dot-cli/main/install.sh \
      | GITHUB_TOKEN=$GITHUB_TOKEN sh -s -- https://github.com/you/dotfiles.git
    unset GITHUB_TOKEN

    $env:GITHUB_TOKEN = [Net.NetworkCredential]::new('', (Read-Host 'GitHub token' -AsSecureString)).Password
    $env:DOT_DOTFILES_URL = 'https://github.com/you/dotfiles.git'
    iwr -useb https://raw.githubusercontent.com/trungngo/dot-cli/main/install.ps1 | iex
    Remove-Item Env:GITHUB_TOKEN

The installers take the token from `GITHUB_TOKEN`, `GH_TOKEN` or, if the GitHub CLI is logged
in, `gh auth token`. They use it only for an `https://github.com/` URL, and only when git can't
read the repository with its own credentials. It goes to git as a header in git's environment,
for that clone only: never on a command line, in a URL or in any git config. `dot apply` does
not inherit it. Afterwards, set up git credentials for github.com (`gh auth login` and `gh auth
setup-git`, or Git Credential Manager) so that later `git pull`s work.

### Forks and mirrors

`DOT_RELEASE_REPO=owner/name` points the installers, `dot self-update` and `dot apply --host`
at a fork's releases. If the fork is private, they use the token for those downloads, through
the GitHub API only. `DOT_RELEASE_BASE` points the installers at a mirror of the release
assets. The installer scripts list every variable they read in their headers.

### Servers and cloud VMs

- `dot apply --host user@server` sets up a machine over ssh from the one you are on. The token
  never leaves your machine, and the repository is synced over ssh, so this also works for
  private dotfiles. Secrets are resolved on your machine and handed to that run on ssh's stdin:
  the server needs no password manager, and keeps nothing unless you pass `--persist-secrets`.

  ```console
  $ dot apply --host me@box
  dot: me@box: installing dot for linux/x86_64 with install.sh
  Value of MY_MACHINE for me@box (Machine name): box
  dot: me@box: sending 2 secrets (gitlab_token, npm_token) and 2 variables (EMAIL, MY_MACHINE) on ssh's stdin (for that run only)
  dot: me@box: syncing /home/me/dotfiles to /home/me/.dot (rsync)
  ...
  Apply? [Y/n] y
  ...
  dot: me@box: sent 2 secrets (gitlab_token, npm_token) and 2 variables (EMAIL, MY_MACHINE) (for that run only; nothing kept there)
  ```

  `--all-secrets` sends every secret that resolves, `--no-secrets` none; `--dry-run` says what
  it would install, sync and send, and does none of it. A variable declared `per_machine: true`
  (`MY_MACHINE`) is never copied: it is asked for (and, when `persist: local`, kept there), or
  given with `--var`. The server's own `local.env` values win over yours. The run there uses
  the profile planned here; if only the server says which profile it is (its `DOT_PROFILE`),
  `dot` asks before sending secrets to it, and without a terminal wants `--profile`. The `dot`
  there must be this version or newer (`dot self-update` there).

  To install `dot` there from a build on your machine instead of downloading it (before a
  release exists, a private build, a server without outbound access), pass `--dot-binary`
  (also on `dot plan --host` and `dot secrets push`; `DOT_HOST_BINARY` sets the default).
  Nothing is downloaded from GitHub: the binary is checked for the server's OS and CPU, copied
  with scp to `~/.local/bin/dot` (upgrading an older `dot` there, or an earlier build of this
  version: a `dot` there with the same SHA-256 is left alone), and must say it is this version
  there before it replaces anything. Built from a Mac or any other machine, with zig
  and cargo-zigbuild:

  ```console
  $ brew install zig && cargo install --locked cargo-zigbuild    # once
  $ rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
  $ scripts/build-linux.sh x86_64                                # or aarch64, or all
  /path/to/dot-cli/target/x86_64-unknown-linux-musl/release/dot
  $ dot apply --host me@box --dot-binary target/x86_64-unknown-linux-musl/release/dot
  dot: me@box: copying .../release/dot (a linux/x86_64 binary) to /home/me/.local/bin/dot
  ...
  ```
- `dot cloud-init --repo https://github.com/you/dotfiles.git --profile ubuntu` prints
  `#cloud-config` user data that installs `dot` (the same version as the `dot` that printed
  it; `--latest` for the latest release) and applies the profile on first boot. The cloud
  provider can read user data, so it never holds a token: this needs public dotfiles. For
  private dotfiles, use `dot apply --host` after the VM boots, or give the VM its own deploy
  key or credentials from your cloud's secret manager. `--secrets-from-file` embeds a secrets
  file in the user data: the provider can read those secrets too, so `dot` warns when you use
  it. Pushing them after boot (`dot secrets push`, or `dot apply --host`) keeps them out.

### The name `dot`

Graphviz also installs a `dot` command, and your shell runs whichever comes first on `PATH`.
`which -a dot` lists every one it can find. `install.sh` warns you when another `dot` comes
before the one it installed. To keep both, use one of these:

- Put `~/.local/bin` before Graphviz's directory in `PATH`, and call Graphviz by its full path:
  `dot graph --dot | /opt/homebrew/bin/dot -Tsvg > graph.svg`.
- Or install `dot` in its own directory and link it under another name:

      curl -fsSL https://raw.githubusercontent.com/trungngo/dot-cli/main/install.sh \
        | DOT_INSTALL_DIR="$HOME/.local/dot-cli" sh
      ln -s ~/.local/dot-cli/dot ~/.local/bin/dotf

## Quickstart

[`examples/basic`](examples/basic) is a small, complete repository: two profiles, links, a
template with variables, packages named per manager, and a secret kept outside git. CI checks
it on every change. To start your own repository from it:

    git clone --depth 1 https://github.com/trungngo/dot-cli /tmp/dot-cli
    cp -r /tmp/dot-cli/examples/basic ~/.dot && cd ~/.dot && git init
    dot check             # validate dot.yml, secrets.yml and every bundle
    dot plan              # which profile matched, the bundle order, each step's state
    dot apply --dry-run   # what would change
    dot apply             # do it

`dot` finds a repository in `~/.dot` by itself (not `~/.dotfiles`, so an older setup checked
out there, dotbot's say, can stay beside it). Anywhere else, run it from inside the
repository, or record the location once: `echo "DOT_REPO=$HOME/dotfiles" >>
~/.config/dot/local.env`.

In a terminal, `dot apply` shows the plan and asks before it changes anything, asks once for
the sudo password if a step needs root, and asks for the git name and email the example's `git`
bundle declares (`dot apply --dry-run` asks for them too). It keeps the answers in
`~/.config/dot/local.env`. Without a terminal (CI, a script), nothing can be asked: give the
values with `--var git_name='Your Name' --var git_email=you@example.com`, and note that `dot
apply` and `dot remove` then go ahead without a confirmation. On a Mac, the example installs
Homebrew first if it is missing.

The example's `shell` bundle adds a line to `~/.zshrc` (or `~/.bashrc`) that sources `dot env`.
That is how `path:` and `env:` steps reach your shell: `dot` records them, and `dot env --shell
zsh --cache` writes a file for the shell to source. Open a new terminal after the first apply.

A minimal repository is a `dot.yml` (it can be empty: then every bundle is selected), one
bundle, and the file it links:

```yaml
# dot.yml
```

```yaml
# bundles/zsh/bundle.yml
steps:
  - link:
      ~/.zshrc: zshrc          # links bundles/zsh/zshrc, which you create
  - package: [zsh]
    when: { os: linux }
```

## Concepts

| | |
|---|---|
| **Repository** | A directory with `dot.yml` at its root. `dot` finds it through `--repo`, `DOT_REPO`, the current directory or its parents, `DOT_REPO` in `~/.config/dot/local.env` (which the installer writes), and finally `~/.dot`. |
| **Bundle** | `bundles/<name>/bundle.yml`: optional manifest keys (`requires`, `after`, `provides`, `when`, `check`, `implicit`, `vars`, `defaults`, `handlers`), then `steps:`. Each step has exactly one directive, plus optional `when`, `if`, `only_if`, `unless`, `label` and `notify`. |
| **Profile** | A named set of facts in `dot.yml` (`match:`), with the bundles, variables and defaults for that kind of machine. It is detected, or chosen with `--profile` / `DOT_PROFILE`. Without profiles, every bundle is selected. |
| **Facts** | What `dot` detects: `os`, `family`, `distro`, `version`, `codename`, `arch`, `role`, `env`, `pkg`, `init`. `dot facts` prints them. `DOT_FACTS=os=linux,distro=ubuntu` simulates another machine for `dot plan`: the facts it doesn't name are derived (`family=debian`, `pkg=apt`, `init=systemd`) instead of taken from this one. |
| **Variables** | `{{ var }}` in paths, templates and commands. They come from facts, then `dot.yml`, the profile, the bundle, `.env`, `.env.<profile>`, `.env.local` and `~/.config/dot/local.env`, the environment, and finally `--var` (each overrides the previous). Declared variables can be required, typed, prompted for (in the order they are declared) and remembered, and `per_machine: true` keeps a machine's own value from being copied to another by `dot apply --host`. A bundle's own `vars:` are only needed when that bundle runs. `dot vars` shows each value and where it came from. |
| **Secrets** | `secret("name")` in a template or command, and `secret_file:` for whole files. `secrets.yml` maps each name to a provider ref, and `optional: true` lets a machine go without one. `dot secrets check` reports anything missing or unsafe without showing a value; `dot secrets push` (resolved here, LastPass included, kept owner-only there) / `export` / `import` move secrets between machines. |
| **State** | What `dot` made is recorded in `~/.local/state/dot/state.json`. `dot status` reports drift and orphans, `dot diff` shows the differences, and `dot remove <bundle>` undoes the bundle and puts back what was there before. |

Every key of `dot.yml`, `bundle.yml` and `secrets.yml` is listed in
[docs/reference.md](docs/reference.md). [docs/design.md](docs/design.md) covers how `dot` and
each directive behave, and why.

## Commands

| command | what it does |
|---|---|
| `dot apply [bundles]` | Apply the profile's bundles (or the named ones and their dependencies). Useful flags: `--dry-run`, `--yes`, `--upgrade`, `--jobs N`, `--var NAME=value`, and `--host user@host` with `--all-secrets` / `--no-secrets`, `--persist-secrets` (`--force-secrets`), `--dot-binary PATH`. |
| `dot plan [bundles]` | Show the resolved order and every step's state without changing anything. |
| `dot check` | Validate every file in the repository and the bundle graph, with the file and line of each error. A file that doesn't parse (an unknown key, a YAML error) stops at its first error. |
| `dot graph [--dot]` / `dot why <bundle>` | Show the bundle graph, or explain why a bundle is (or isn't) part of a run. |
| `dot vars` / `dot facts` | Show variables with their sources (secrets redacted), or the detected facts. |
| `dot env --shell zsh` | Print a file for your shell to source, with variables and what `path` / `env` steps set up (`--cache` keeps it and prints its path). |
| `dot status` / `dot diff` | Report drift and orphans (exits 1 unless in sync), or show file diffs and missing packages. |
| `dot doctor` | Check the repository, profile, package manager, sudo, secrets, git, clock, state and `PATH`. |
| `dot adopt <path>` | Move an existing file into a bundle and link it back. |
| `dot remove <bundle>` | Undo what `dot` recorded for a bundle. |
| `dot secrets check\|push\|export\|import` | Check secrets, or move them between machines (owner-only, or encrypted with `age`). |
| `dot schema bundle\|repo\|secrets` | Print a JSON Schema. |
| `dot self-update` | Replace `dot` with the latest release, checked against `SHA256SUMS`. |
| `dot cloud-init` | Print cloud-init user data that installs `dot` and applies a profile on first boot. |

`dot <command> --help` lists every option. `dot apply` and `dot remove` ask for confirmation
only in a terminal; without one they go ahead (`--yes` says so explicitly). Output that is not
a terminal (CI, a pipe, `--plain`) is one line per event with a UTC timestamp; `--json` gives
one JSON event per line. Every run also writes a full log under `~/.local/state/dot/logs/`.

## Editor support

Each release publishes JSON Schemas for the three files. With the YAML language server (VS
Code's YAML extension, Neovim, Helix and others), add a line like this to the top of a file to
get completion and validation:

```yaml
# yaml-language-server: $schema=https://github.com/trungngo/dot-cli/releases/latest/download/bundle.schema.json
```

Use `dot.schema.json` for `dot.yml` and `secrets.schema.json` for `secrets.yml`. `dot schema`
prints the same schemas offline.

## Platforms

| platform | status |
|---|---|
| macOS (Apple silicon and Intel) | supported (Homebrew, casks, `defaults`, launchd) |
| Ubuntu desktop and server | supported (apt, snap, systemd); tested in CI in an `ubuntu:24.04` container |
| Windows 10 / 11 | supported (winget, scoop, PowerShell steps, symlinks, or junctions and copies without Developer Mode) |
| WSL | supported, as Linux (it is detected as `env: wsl`) |
| Debian, Fedora, openSUSE, Arch, Alpine | the package managers are there (apt, dnf, zypper, pacman, apk); less tested |

## How it compares

- **dotbot** is a linker with a few more directives, driven by one YAML file. Anything beyond
  links usually lives in shell scripts around it. `dot` adds a bundle graph, per-machine
  profiles and conditions, package managers, secrets, drift detection and undo, and it needs
  no Python.
- **chezmoi** keeps a source state that it renders into your home directory (templates,
  encrypted files, `run_` scripts). `dot` links files by default and focuses on the whole
  machine: packages, vendor repositories, services and system files, all ordered as a graph
  that you can `plan` first.
- **Ansible** is general configuration management for fleets. `dot` is a single binary for
  your own user and machine, with no inventory or Python on the target. `--host` covers the
  "set up my server too" case.

## Documentation

- [docs/reference.md](docs/reference.md): every configuration key, generated from the schemas.
- [docs/design.md](docs/design.md): architecture, and how each part behaves and why.
- [examples/basic](examples/basic): a starting repository.
- [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md), [CHANGELOG.md](CHANGELOG.md).

## License

MIT: see [LICENSE](LICENSE).
