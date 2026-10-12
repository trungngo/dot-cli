# dot — design

`dot` is a single Rust binary that sets up and maintains macOS, Linux and Windows machines from
a repository of dotfiles. It replaces the usual combination of a linker (dotbot, stow) and the
shell scripts around it with one tool that knows the machine, orders the work as a graph,
installs packages, keeps secrets out of git, and can say what it would change before it changes
anything.

This document describes how `dot` works today and why. Every configuration key is in
[reference.md](reference.md), generated from the JSON Schemas.

Contents: [Goals](#goals) · [Architecture](#architecture) · [Facts and profiles](#facts-and-profiles)
· [The repository](#the-repository) · [Bundles and the graph](#bundles-and-the-graph) ·
[Steps](#steps) · [Directives](#directives) · [Variables](#variables) · [Secrets](#secrets) ·
[State and operations](#state-and-operations) · [Privileges](#privileges) ·
[Windows and WSL](#windows-and-wsl) · [Output](#output) · [Remote machines](#remote-machines) ·
[Distribution](#distribution) · [Testing hooks](#testing-hooks) · [Limits](#limits)

## Goals

| problem with a linker plus scripts | what `dot` does |
|---|---|
| options (`defaults:`) do not carry across files | defaults in `dot.yml`, a profile and a bundle, inherited |
| a failed step prints no reason | live output, a full log file for every run, and a hint for known failures |
| stdin is `/dev/null`, so sudo inside a step fails | the terminal is inherited; sudo is asked for once, up front, and kept alive |
| a missing platform file is a silent no-op | `when:` conditions; `dot plan` shows what runs where, and why not |
| desktop or server is guessed from the hostname | a `role` fact from the session and boot target, or pinned locally |
| modules listed by hand, ordering by file names | bundle manifests with dependencies, resolved as a graph |
| Windows detected but not supported | native Windows: PowerShell steps, winget and scoop, link fallbacks |
| bootstrap needs Python, git, submodules | one static binary, `curl \| sh` or `iwr \| iex` |
| secrets committed to the repository | secrets resolved from outside git, by logical name |

Non-goals: fleets and inventories (that is Ansible), and compatibility with dotbot's format.

## Architecture

A Cargo workspace with one binary, `dot`:

    crates/
      dot-core/        config model and parsing, facts, variables, the bundle graph, the
                       plan/apply engine, events, the state store
      dot-directives/  one module per directive
      dot-platform/    package managers, service managers, link strategies, sudo, HTTP
      dot-secrets/     the SecretProvider trait; the envfile and lastpass providers
      dot-cli/         commands, renderers (tty, plain, json, log file), remote apply,
                       self-update, cloud-init

Every directive is split into a pure **plan** and a side-effecting **apply**. Planning turns a
step's configuration into `Action`s and asks each one for its status (`satisfied`, `pending`,
`unknown`, `conflict`) without changing anything. That split is what makes `dot plan`, `dot
apply --dry-run`, `dot status` (drift) and `dot diff` cheap and safe. Just before an action is
applied, it is planned again, so a step sees what earlier steps did.

Each action also names the **resources** it manages (`link:~/.zshrc`, `pkg:apt:fd-find`,
`block:~/.bashrc [dot:shell]`, ...), how to **undo** them, and a **diff** of desired against
actual. The engine records them in the state store, which `dot status`, `dot diff` and `dot
remove` read.

The engine emits **events** (run started, plan ready, bundle and step started and finished,
output lines, progress, needs the terminal); renderers subscribe to them.

Main crates used: `clap`, `serde` with `serde_yaml_ng`, `schemars`, `minijinja` (strict
undefined), `dotenvy`, `petgraph`, `indicatif` and `console`, `portable-pty`, `ureq` (rustls; the
system TLS stack on Windows), `secrecy` and `zeroize`, `age`.

## Facts and profiles

Facts are detected at startup and used by every `when:`, every package name mapping and every
profile match:

| fact | examples | source |
|---|---|---|
| `os` | `linux`, `macos`, `windows` | the build target |
| `arch` | `x86_64`, `aarch64` | the build target |
| `distro` / `version` | `ubuntu` / `24.04` | `/etc/os-release` (the product version on macOS and Windows) |
| `family` | `debian`, `rhel`, `arch`, `alpine`, `suse` | `ID_LIKE`, then `ID` |
| `codename` | `noble` | `VERSION_CODENAME`, else `UBUNTU_CODENAME` |
| `role` | `desktop`, `server` | `DOT_ROLE` or local config, else a graphical session or a display manager with a graphical boot target |
| `env` | `wsl`, `container`, `vm`, `ssh` (several at once) | `/proc/version`, `/.dockerenv` and cgroups, `systemd-detect-virt`, `SSH_CONNECTION` |
| `pkg` | `apt`, `brew`, `dnf`, `winget`, ... | the first manager on `PATH`, the distro family's own first |
| `init` | `systemd`, `launchd`, `other` | `systemd` only when `/run/systemd/system` exists |

In WSL, `pkg` ignores Windows programs reached through interop, and `windows_home` is the
Windows profile directory as a Linux path.

`version` conditions may compare (`>=22.04, <25`); every `when:` value may be negated (`env:
"!container"`); `env: none` matches a machine with no special environment. Values of closed
facts (`os`, `role`, `env`, `pkg`, `init`) are checked when the file is parsed, so a typo is an
error rather than a condition that never matches.

`DOT_FACTS=os=linux,distro=ubuntu,...` overrides detected facts, to plan a configuration for
another platform or to run tests against fake tools. Simulating another machine (another `os`,
or another `distro`) drops what was detected about this one and derives what the override
leaves out: `family` from `distro`, `pkg` and `init` from `os` and `family`.

**Profiles** in `dot.yml` are named kinds of machine: facts to `match`, the `bundles` to apply,
and their own `vars`, `defaults`, capability `providers` and `secrets_provider`. The most
specific matching profile wins (a tie is an error that names both); `--profile` or
`DOT_PROFILE` picks one by hand; a profile without `match` is only used when named. A
repository without profiles applies `dot.yml`'s `bundles`, or every bundle.

Adding a platform means implementing `PackageManager`, `ServiceManager` or `LinkStrategy` and
a detection rule, not changing the engine.

## The repository

    dot.yml                 profiles, bundle selection, repository-wide vars and defaults,
                            package names per manager
    secrets.yml             logical secret name -> provider ref (no values)
    .env, .env.<profile>    committed non-secret values
    .env.example            the secrets and variables a machine needs (names only)
    .env.local              this machine's values (ignored by git)
    bundles/<name>/
      bundle.yml            manifest keys, then the ordered steps
      ...                   files the bundle links, copies or templates

`dot` finds the repository through `--repo`, then `DOT_REPO`, then the current directory and
its parents, then `DOT_REPO` in `~/.config/dot/local.env` (which the installers write), and
finally `~/.dot`. Never `~/.dotfiles`: that is where other setups (dotbot, stow) usually keep
theirs, so a machine moving to `dot` can keep its old setup checked out beside the new one.

The files are YAML (the 1.2 core schema: `no` is a string; booleans are `true` and `false`),
parsed into typed structures with unknown keys rejected: a typo is an error with its file and
line. File modes are quoted strings. The JSON Schemas are generated from the same structures,
published with every release, and turned into reference.md. `dot check` parses and validates
every file and the graph without applying anything.

## Bundles and the graph

A bundle's manifest keys sit above its `steps:`:

```yaml
requires: [common]            # pulled in and applied first; its failure skips this bundle
after: [zsh]                  # ordering only: never pulls zsh in, and zsh failing is fine
provides: [node]              # a capability others can require as cap:node
when: { role: desktop }       # the whole bundle, only on matching machines
requires_if:
  - when: { os: macos }
    requires: [homebrew]
check: command -v fnm         # exits 0: already satisfied, the steps are skipped
implicit: true                # always included (when its `when:` matches)
```

Resolution: the selection (command line, else profile, else `dot.yml`), plus implicit bundles;
bundles whose `when:` does not match are dropped (a required one that does not match is an
error showing the chain); requirements are added transitively; capabilities go to the
provider the profile names; then a topological sort (ties alphabetical) with cycles reported.
`dot graph` and `dot why <bundle>` show the result; `--no-deps` applies only the named bundles.

Execution: each bundle once, in order. When a bundle fails, the bundles that require it
(directly or not) are skipped, and the rest go on. `--jobs N` runs independent bundles at the
same time: a bundle starts once everything it `requires` or comes `after` has finished. Steps
that need the terminal or root, and secret resolution, run one at a time; package managers
hold their own lock. Output stays in bundle order (see [Output](#output)).

`path:` and `env:` changes are visible to the later steps of the bundle that makes them and to
the bundles that `requires` or `after` it, at any `--jobs`: a bundle that needs another's
`PATH` must say so.

## Steps

Each step has exactly one directive key, plus optional:

- `when:` facts that must match (checked while planning);
- `if:` an expression over variables (Jinja without braces: `godot_mono`, `codename in
  ["jammy", "noble"]`);
- `only_if:` / `unless:` a command that must succeed / fail. They run only when the step has
  work or cannot be planned, while planning and again right before applying, with the bundle's
  interpreter, directory, variables and `PATH`. A skipped step says why;
- `label:` a name for output;
- `notify:` handlers to run when this step changes something.

**Handlers** are steps in the bundle's `handlers:` map (any directive; typically `service: {
name: ssh, restart: true }`). They run once each, in the order written, after the bundle's
steps, when a step that notifies them changed something. A notification whose handler did not
run (a later step or the handler failed, or the run was interrupted) is kept in the state store
and fires on the next run.

**Defaults** for `link` (`create`, `relink`, `on_conflict`) and `shell` (`interpreter`,
`interactive`, `prelude`) can be set in `dot.yml`, a profile or a bundle; a step's own options
win.

## Directives

Every directive is idempotent: a second run finds everything satisfied and changes nothing.
Paths take `~` and variables; relative sources are relative to the bundle directory.

### Files

- **`link`** makes symlinks to files in the bundle. A real file or directory in the way is
  backed up (`<dest>.dot-backup`, the default), overwritten or a conflict (`on_conflict`); a
  link pointing elsewhere is replaced unless `relink: false`; missing parents are created. On
  Windows without symlink rights, a directory becomes a junction and a file a copy, recorded
  as such (see [Windows and WSL](#windows-and-wsl)).
- **`copy`** copies a file or directory (with `mode`), satisfied while the content matches. A
  directory copy also prunes what its source lost: a file an earlier run copied that the source
  no longer has is removed while it still holds what `dot` wrote (then the directories left
  empty); one edited since is kept, reported, and no longer `dot`'s. Files in the destination
  that `dot` never copied are left alone. `dot plan` and `dot status` count the stale files,
  and `dot diff` names them.
- **`template`** renders a file from a minijinja template with the bundle's variables (and
  `secret()`). A new file gets `0644`, an existing one keeps its mode, and a file that uses a
  secret defaults to `0600` and refuses a mode others can read. A destination may be a pattern
  (`~/.config/JetBrains/*/options/x.xml`): one file per match of the directories that exist.
- **`block`** keeps a block of lines between markers (`# >>> dot:<bundle> >>>`) in a file
  `dot` does not own (`~/.bashrc`, `/etc/hosts`), creating the file if it is missing. Only the
  block is compared, diffed and removed.
- **`create`** makes directories (with `mode`), or with `file: true` an empty file only when
  missing, whose content is never touched.
- **`secret_file`** writes a secret's value as a file, `0600` by default (a looser mode needs
  `allow_loose_mode`). Its content is never hashed, shown or backed up.
- **`clean`** removes dangling symlinks into the repository from a directory (`recursive`
  optional). **`absent`** removes paths; `only: link` removes a path only while it is a
  symlink (and `pointing_into:` only one into a given directory), for retiring links an old
  setup made. Both refuse the home directory and anything above it, the repository, and `/`;
  a path outside home needs `root: true`.

`template`, `copy`, `block`, `create` and `download` take `root: true` for root-owned files
(see [Privileges](#privileges)). Before `dot` first writes over a file it did not make, the
original is saved in the state directory, and `dot remove` puts it back.

### Packages

- **`package`** installs packages with this machine's manager: apt, dnf, zypper, pacman, apk,
  Homebrew (formulae and casks), snap, winget, scoop, and VS Code extensions on every OS. A
  list of names is mapped per manager through `dot.yml`'s `packages:` (`fd: { apt: fd-find,
  brew: fd }`); a mapping names packages per manager, and only this machine's entry applies.
  Only missing packages are installed; `upgrade: true` on the step (or `dot apply --upgrade`)
  also upgrades outdated ones (apt leaves held packages alone). apt refreshes its lists at most
  once a day, and after a repository changes. Names may be patterns (`dotnet-sdk-*`: the newest
  match). A snap falls back to the distro's manager where snapd is missing, unless `fallback:
  false`. `winget: [{ name, scope: machine }]` installs machine-wide. The query runs while
  planning, so the plan names exactly the missing packages.
- **`package_repo`** adds a vendor repository for this machine's manager: an apt keyring and
  deb822 source, a dnf `.repo` file, a zypper repository, a Homebrew tap (trusted with `brew
  trust` where Homebrew has it), a scoop bucket or a winget source. apt keys are fetched over
  https and checked against `fingerprint` (the whole fingerprint; a list for a vendor rotating
  its key) before anything is installed; a key without one is "not pinned" in the plan. A
  suite the vendor does not publish yet can have a `fallback_suite`. Another sources file for
  the same repository is a conflict unless `replaces:` names it.
- **`github_release`** installs executables from a GitHub (or GitLab) project's latest release,
  or the one `tag:` names: the asset for this OS and CPU (or `asset:`), unpacked (`.tar.gz`,
  `.tar.xz`, `.zip`, or a plain executable), into `~/.local/bin` by default. `bin:` names the
  executables, or maps an installed name to a glob over the archive (`godot: 'Godot_v*'`). The
  download is checked against the release's published checksum (GitHub's asset digest, or a
  checksum file among the assets) when there is one, or against the step's `sha256:`. It is
  satisfied while the recorded tag and asset are installed; an executable of the same name that
  `dot` did not install is a conflict unless `replace: true`. `package:` installs the release's
  `.deb` / `.rpm` through the package manager instead. Offline, what is installed is kept. A
  token (`token:` secret, `GITHUB_TOKEN` / `GH_TOKEN`, a `github_token` secret, or
  `GITLAB_TOKEN`) goes only to the API.
- **`download`** fetches a file over https: saved (`dest`, `mode`), unpacked into a directory
  (`extract`, `strip`; `replace: true` swaps the whole tree so nothing of an old version stays;
  `bin:` links executables from it), or installed as a `.deb` / `.rpm` (`package:`). The URL can
  come from a release (`release:`) or contain the current version (`latest:` asks a URL for it,
  `https://go.dev/VERSION?m=text`). With `sha256:` (or `sha256_url:`) the file is checked;
  without it, the step is satisfied while the server says the file is unchanged (ETag,
  Last-Modified). Archive entries outside the destination are refused.

### Commands and system

- **`shell`** runs a command through `sh` (or `bash`, `zsh`, `pwsh`, `powershell`, `cmd`:
  `interpreter:`). `unless:` skips it when a check succeeds, and under `--upgrade` `upgrade:`
  runs instead (`rustup update`). `sudo: true` means the command uses sudo, so the password is
  asked for up front; `interactive: true` hands it the terminal. `prelude:` runs first in the
  same script (sourcing a shell function). Declared variables are exported to it, sensitive
  ones only when `export_sensitive` names them; secrets reach it as environment variables,
  never on the command line. PowerShell runs `-NoProfile -NonInteractive -EncodedCommand`, and
  the step fails with the last native command's exit code. A `shell` step without `unless`
  runs on every apply.
- **`git`** clones a repository (https or `file://`) and fast-forwards a clean checkout; local
  changes, a diverged branch or another remote are a conflict, left alone. `depth: 1` makes a
  shallow clone. Credentials in the URL are refused: private repositories authenticate through
  a git credential helper.
- **`service`** installs (from a templated unit file or property list), enables and starts a
  systemd, systemd user, launchd or Windows service; a changed unit is reloaded and a running
  service restarted. `linger: true` keeps a user's services running without a login. Where
  systemd is not running (containers, WSL without systemd), the step is a conflict that says so.
- **`macos_defaults`** writes preferences with `defaults write`, comparing the current value
  first.
- **`user`** sets a user's login shell (adding it to `/etc/shells` when missing) and adds
  supplementary groups (creating missing ones). The change takes effect at the next login, and
  the run ends with a notice saying so. Not on Windows.
- **`path`** and **`env`** add directories to `PATH` and set environment variables for the
  later steps of the run, and record them. `dot env --shell zsh` (or `bash`, `fish`, `pwsh`)
  prints a file for the shell to source with those, plus the environment-style variables
  (upper case) of `dot.yml`, the profile and `.env`; `--cache` keeps it in the cache directory,
  rewrites it only when something it comes from changed, and prints its path, for `source
  "$(dot env --shell zsh --cache)"` in a shell's startup file.
  `persist: user` on a `path` entry also adds the directory to the Windows user `Path` in the
  registry (`HKCU\Environment`), so programs started outside such a shell see it; `dot remove`
  takes it out again unless it was there before. Elsewhere it has nothing to persist.

## Variables

Layers, lowest to highest precedence:

1. facts, `home` and the known folders (`config_dir`, `data_dir`, `cache_dir`, `state_dir`,
   `documents`, `homebrew_prefix`, and on Windows `appdata`, `localappdata`, `userprofile`,
   `programfiles`, `winget_links`; `none` where not applicable)
2. `vars:` in `dot.yml`
3. `vars:` in the profile
4. `vars:` in the bundle (seen by that bundle only)
5. `.env` (committed)
6. `.env.<profile>` (committed)
7. `.env.local` and `~/.config/dot/local.env` (this machine)
8. the process environment
9. `--env-file` and `--var NAME=value`

`{{ var }}` works in paths, templates, commands and most other strings. Undefined variables
are errors (with the file and line), never empty strings. `.env` files use dotenv syntax with
`${VAR}` interpolation.

A variable can be declared instead of given a value: `type` (`string`, `bool`, `int`, `list`,
`path`), `required`, `pattern`, `default`, `sensitive`, `description`, `prompt` and `persist:
local`. A declaration is checked for the bundles that see it, so a variable declared in a
bundle is needed only when that bundle runs. Before anything runs, `dot apply` asks for every
prompted variable without a value, in the order they are declared, and with `persist: local`
keeps the answer in `local.env`. A run without a terminal cannot ask: it fails before
anything runs, listing everything missing, and `--var` gives the values instead. `dot plan`
shows the steps that use an unanswered variable as unknown.

`per_machine: true` marks a value that belongs to one machine (its name, its disks):
`dot apply --host` never copies this machine's value of it to another one (see
[Remote machines](#remote-machines)).

Variables whose names look like credentials (`*token*`, `*password*`, ...) are sensitive by
default: redacted in output and `dot vars`, and not exported to shell steps. `dot vars` shows
every value and the layer it came from.

## Secrets

The rule: **bundles ask for a secret by logical name; `secrets.yml` says where it lives.**
Moving a secret to another provider is an edit to `secrets.yml`, never to a bundle.

```yaml
# secrets.yml: committed, holds no values
default_provider: envfile
providers:
  envfile: { path: "~/.config/dot/secrets.env" }
  # lastpass: { username: me@example.com }
secrets:
  gitlab_token: { ref: "envfile:GITLAB_TOKEN" }
  vpn_profile: { ref: "envfile-file:vpn/work.ovpn", kind: file }
  npm_token: { ref: "lastpass://Dev/npm#password", fallback: "envfile:NPM_TOKEN" }
  api_token: { ref: "envfile:API_TOKEN", optional: true }
```

Bundles use `{{ secret("gitlab_token") }}` in templates and commands, and `secret_file: {
~/.config/vpn/work.ovpn: vpn_profile }` for files.

| provider | refs | notes |
|---|---|---|
| `envfile` | `envfile:KEY`, `envfile-file:path` | a dotenv file (default `~/.config/dot/secrets.env`) and a directory of file secrets next to it; must be owner-only and outside git (or ignored) |
| `lastpass` | `lastpass://Folder/Item#password\|username\|notes\|field=<name>\|attach=<file>` | wraps `lpass`, installed by a bundle; logs in on the terminal when needed |
| `1password` | `op://vault/item/field` | not implemented yet: the refs parse, and resolve to their `fallback` |

A profile's `secrets_provider` picks the provider for bare refs (for example LastPass on
desktops, the env file on servers).

Lifecycle: planning records which secrets each step needs and resolves none, so `dot plan`
never asks for a password. A run resolves only the secrets of steps that have work to do: one
unlock and one batched lookup per provider, then fallbacks, then a prompt when there is a
terminal. A missing secret fails only the bundles that need it (and their dependents); an
`optional: true` secret with no value skips the steps that use it. A broken configuration (an
undeclared name, a bad ref, a secrets file others can read) stops the run before anything is
applied. `dot status` and `dot diff` use the secrets that can be resolved without a prompt, and
list the rest as "not checked".

Values are held in types that print as `***` and are zeroed when dropped, and every resolved
value is registered with the redactor that all output and logs pass through. Content rendered
with a secret is never diffed or shown.

Guardrails: a secrets file (or file secret) that group or others can read, or on Windows whose
ACL grants anyone but the user, `SYSTEM` and `Administrators`, is refused with the command
that fixes it, as is one inside a git work tree unless git ignores it. A committed `.env*` file
holding a value for a key `.env.example` marks `# secret` stops the run. `dot secrets check`
reports missing (required or optional), undeclared and unused secrets and unsafe permissions,
never a value. `dot secrets push user@host [NAME...]` resolves secrets here, through any
provider (so a LastPass-only secret can be pushed too), and keeps them in the other machine's
envfile, under each secret's envfile ref (its `ref:` or `fallback:`), owner-only, over ssh's
stdin (see [Remote machines](#remote-machines)); a different value already there is replaced
only with `--force`. It checks the `dot` there (installing it when missing) before anything is
unlocked or resolved here. `dot secrets export` / `import` make and restore an `age`-encrypted
backup.

SSH private keys are not synced: generate one per machine.

## State and operations

State lives in `$DOT_STATE_DIR`, else `$XDG_STATE_HOME/dot` or `~/.local/state/dot` (Unix), or
`%LOCALAPPDATA%\dot` (Windows): one versioned `state.json`, written atomically under a file
lock, owner-only. It holds the recorded resources, the `path` / `env` record, due handlers,
bundles applied by name, and sections of individual directives (installed releases, downloads,
link fallbacks). A corrupt file is set aside; one from a newer `dot` is never written. Saved
originals are under `backups/`, logs under `logs/`.

- **Records.** A real run records every resource of an applied or already satisfied action,
  with its bundle, and whether `dot` made it (`owned`).
- **`dot status [bundles]`** reports drift (anything not satisfied, and planning errors) and
  orphans (records the plan no longer produces: a bundle removed or deselected, a step
  deleted). Steps that manage nothing (`shell` without `unless`) are listed as "run on every
  apply", not as drift. Exit 1 unless in sync.
- **`dot diff [bundles]`** prints unified diffs for files and blocks, old and new link targets,
  and missing or outdated packages.
- **`dot remove <bundle>`** undoes a bundle's records, newest first: links are removed while
  they point at the source, files while they hold what `dot` wrote, blocks are taken out, and
  saved originals are put back. Anything changed since is left in place and reported. Packages
  are uninstalled only with `--packages`, and only those `dot` installed. A bundle other
  selected bundles require is refused unless `--cascade`. Git checkouts, services, preferences
  and users get a note saying what to undo by hand.
- **`dot adopt <path>`** moves an existing file or directory into a bundle, appends a `link`
  step to its `bundle.yml` (as text, keeping comments), and links it back; any failure half-way
  restores both.
- **`dot doctor`** checks the repository and profile, the package manager, sudo or elevation,
  the secrets provider and file permissions, git, the clock, the state store, stale locks and
  `PATH`.
- **Locks.** `dot apply`, `dot remove` and `dot adopt` hold a run lock: a second run fails at
  once, naming the one in progress. Read-only commands never wait.

## Privileges

`dot` runs as your user. Root is needed by package managers, system services, lingering,
`user`, root-owned files and `shell` steps marked `sudo: true`. When a run has any such
pending action, `dot` asks for the sudo password once, before applying anything, keeps the
timestamp fresh, and runs those commands with `sudo -n`, so nothing prompts mid-run. Without a
terminal, sudo must not need a password.

Homebrew does not reuse that timestamp: every `brew` command (but a few read-only ones)
discards it first, and a cask whose metadata needs root (a `.pkg`, a keyboard layout, an
installer script or install steps with `sudo: true`: Homebrew's `requires_sudo?`) then has
Homebrew's own `sudo` ask for the password. So such casks are not covered by the up-front
question. An interactive run gives them the terminal (the live view pauses, the plan says
"Homebrew asks for the sudo password in the terminal") and the password is typed there, once
per `brew` command; their output is still streamed (sudo asks on `/dev/tty` itself), so it stays
in the log and out of `--json`'s stdout. A non-interactive run needs passwordless sudo for them
(`sudo -n -k true`), checked before anything is applied. Other casks get an askpass that
answers nothing, so a `sudo` they run anyway fails at once instead of waiting, unseen, for a
password, and the error says to run that `brew` command by hand; unless sudo is passwordless
they also get `HOMEBREW_NO_SUDO=1`, as formulae and queries always do, which keeps Homebrew
7.0.7 and later from discarding the timestamp. Because something may still
have discarded it, `sudo -n true` is checked again right before each action that needs root:
an interactive run asks for the password again when it fails, a non-interactive one fails that
action with the reason. The keep-alive skips its refresh while a program has the terminal for
a password, and never holds the end of the run up.

`root: true` files are written to a private temp directory and installed with `sudo -n
install -m <mode> -o root`; a root-owned file this user cannot read is read with `sudo -n cat`.
They are satisfied only when content, mode and owner match. `dot remove` compares and restores
them the same way.

On Windows, "root" means an elevated terminal: machine-wide winget installs, system services,
winget sources and `absent` paths marked `root: true` need one, and a run that is not elevated
stops before anything runs, listing them.

## Windows and WSL

- Links: symlinks when the account can make them (Developer Mode or elevation), else a
  junction for a directory and a copy for a file. The choice is probed once per run with a real
  attempt. Copies are recorded with the digest `dot` wrote: a copy still holding it is `dot`'s
  own (updated when the source changes, replaced by a link once symlinks work); an edited copy
  is a file in the way.
- `shell` steps default to `pwsh` when installed, else Windows PowerShell. Secrets reach them
  as `$env:` variables.
- `path:` entries with `persist: user` are written to the user `Path` in the registry (keeping
  its `REG_EXPAND_SZ` type, never rewriting it after a failed read), and `WM_SETTINGCHANGE` is
  broadcast so new terminals started from Explorer see them.
- Program lookup follows `PATHEXT`; after a winget or scoop install, the registry `PATH` is read
  again so later steps find what was installed.
- Secret files get their own protected ACL for the user, `SYSTEM` and `Administrators`.
- Paths that are absolute only on another OS (`/usr/local/go` on Windows) are errors only when
  a step that runs here uses them, so one bundle can serve several platforms.
- WSL is Linux with `env: wsl`. A link from the Linux filesystem onto a Windows drive becomes a
  copy (a file) or a conflict (a directory): Windows programs cannot read WSL links.

## Output

| renderer | when |
|---|---|
| tty | an interactive terminal: the plan and a confirmation, then a live tree with spinners, byte progress and a window of each step's output that collapses on success |
| plain | not a terminal, `TERM=dumb` or `--plain`: one line per event with a UTC timestamp |
| json | `--json`: one event per line |
| log file | always: full output, redacted, in `<state>/logs/` (the last runs are kept) |

A failed step shows its last output lines, where in the log it is, which bundles were skipped
because of it, and a hint when the failure is a known one (a held dpkg lock, an unknown apt
package, an untrusted tap, sudo without a terminal, DNS, TLS, a GitHub rate limit, Homebrew
missing, ...). `-v` shows every step, `-vv` every output line, `-q` only failures. Programs run
on a pseudo-terminal where possible, so they keep their colours and progress bars; `NO_COLOR`
and `--color` are honoured. With `--jobs`, the plain and log renderers hold later bundles'
events until earlier ones finish, so the output reads as a sequential run.

`dot apply` and `dot remove` confirm the plan only in a terminal; without one (or with
`--yes`) they go ahead.

## Remote machines

**`dot apply --host [user@]host`** (and `dot plan --host`) sets up another machine over ssh,
through one shared connection, with this machine's secrets and personal variables:

1. One probe: the remote OS and CPU, its home directory, rsync, and an installed `dot`: the
   first of `command -v dot` and `~/.local/bin/dot` whose `--version` says `dot <version>` (so
   graphviz's `dot` earlier on the PATH is skipped). A remote whose shell is not a POSIX `sh`
   (Windows) is refused in one line. An installed `dot` older than this one stops the run
   with one instruction, `<that dot> self-update` there.
2. `dot` is installed when missing: this binary when it runs there (same OS and CPU, static on
   Linux); else, when this machine has a GitHub token, the remote's release asset is downloaded
   here, checked, and copied over, so the token never leaves this machine; else the remote runs
   `install.sh` itself, without a token. With **`--dot-binary PATH`** (or `DOT_HOST_BINARY`),
   none of these: see [A dot binary from here](#a-dot-binary-from-here).
3. `dot host-info` there reports its facts, its pinned profile (`DOT_PROFILE`) and the names
   (never the values) its `local.env` sets; it also proves that the `dot` there is this tool.
   The remote's run is planned **here** from those: its profile, its bundles, its variables
   (this machine's own `local.env` takes no part). Facts a newer `dot` reports that this one
   does not know are left out, with a note. The run there is given the profile planned here
   (`--profile`), so the plan and the run cannot disagree. When that profile is the one the
   remote reported (no `--profile` here) and secrets or sensitive variables would be sent, the
   remote's own report chose them: this side names the profile and the facts and asks before
   resolving anything; without a terminal (or with `--yes`) it refuses unless `--profile`
   names that profile.
4. **Secrets are resolved here**, through this machine's providers (its profile's
   `secrets_provider`), each unlocked once, so the remote needs no password manager and no
   login. Which ones: by default those the remote's bundles use (read statically, as `dot
   check` reads them); `--all-secrets`: every one `secrets.yml` declares that resolves;
   `--no-secrets`: none (the remote resolves its own). A required secret that cannot be
   resolved stops everything before anything is synced or sent, listing every such name; an
   `optional: true` one is skipped with a note.
5. **Variables are forwarded**: `persist: local` variables with a value in this machine's
   `local.env` that the remote's `local.env` does not set (its own value wins, unless
   `--force-secrets`), and every `--var`, become `--var`s of the run there. A `per_machine:
   true` variable is never forwarded from `local.env`; when the run there needs it and the
   remote has no value, it is asked for here ("Value of MY_MACHINE for box"), taken from
   `--var`, or, without a terminal, the run stops before anything is synced, naming it. An
   answer asked for here is the remote's own value: when the variable is `persist: local` it is
   kept in the remote's `local.env`, as a local run keeps it, so it is asked once.
6. The repository is synced to `~/.dot` (or `--remote-repo`) with rsync when both ends
   have it, else as a tar stream of the tracked and unignored files. Files git ignores (such as
   `.env.local`) never travel. A target that exists, is not empty and holds no `dot.yml` is
   refused (`rsync --delete` would replace whatever is there).
7. `dot apply` runs there with the forwarded flags, on a terminal (`ssh -t`) when this side has
   one, so the plan can be confirmed and sudo asked for. Its output streams through and its exit
   code is `dot`'s.

What was resolved and forwarded is handed to that run as one stream on the ssh session's stdin
(the hidden `--secrets-from -`; that ssh gets `-T -o EscapeChar=none`, so a `RequestTTY` in
`~/.ssh/config` cannot give the stream a terminal that would echo or interpret it): never on a
command line (`ps`, shell history), in the
environment, in a URL, in a log or in a temporary file. Each record is a line of JSON naming
the secret or variable, then its bytes, so file secrets travel as they are. When this side has
a terminal, stdin is the terminal, so the stream goes through a second ssh session instead:
`dot secrets relay` there makes a FIFO in a new owner-only directory and prints its path,
reads the stream into memory, and writes it into the FIFO once the run (`--secrets-from
<fifo>`) opens it; both remove it, and the relay gives up as soon as its session is gone. The
bytes pass through memory and the kernel only. The remote `dot` keeps the values in memory for
that run (they come before any provider and are registered with its redactor, as are variables
flagged sensitive); the only files written are those its bundles write (`secret_file`
outputs, templates). `dot` there must be this version or newer.

`--persist-secrets` keeps them instead, so later runs there work on their own: each secret in
the remote's `~/.config/dot/secrets.env` or `files/` (under its envfile ref; one without any is
used for that run only, with a note), the forwarded `persist: local` variables in its
`local.env`; written atomically, files `0600`, directories `0700`. Nothing is written until the
run there is confirmed (declining its "Apply?" keeps nothing). A different value already there
is kept unless `--force-secrets`; for a secret, that run uses the kept value too, so a later
run there on its own gives the same result (a variable is only sent over one the remote holds
when given with `--var`, which wins for that run, as it does locally). The remote reports, under its host name, what was added,
updated, unchanged or kept, never a value.

The summary says what was sent (names and counts). `--dry-run` installs, syncs, sends and
applies nothing: it says what it would do and send, planned from what is already there (an
installed `dot`, else the probe's OS and CPU). `dot plan --host` is handed the variables, never
a secret.

### A dot binary from here

`--dot-binary PATH` (on `dot apply --host`, `dot plan --host` and `dot secrets push`; the
default comes from `DOT_HOST_BINARY`) installs `dot` there from a file on this machine, for a
remote that cannot or should not download it: no release yet, a private build, no outbound
network there. Nothing is fetched from GitHub, here or there, and nothing falls back to a
download or to `install.sh`: a problem with the file stops the run.

- A newer `dot` there is used: nothing is copied.
- A `dot` there that says `dot <this version>` (the probe's, or `~/.local/bin/dot` when
  another one comes first on the PATH there) is used when it is this very build: its SHA-256
  there (`sha256sum`, or `shasum -a 256` on macOS) is the file's. Every build from source says
  the same version until a release bumps it, so the version alone cannot tell an earlier build
  from this one; a different build is replaced. With neither tool there, the version decides.
- Otherwise (none there, an older one, or another build) the file is installed where
  `install.sh` would put it, `~/.local/bin/dot`, replacing what is there:
  1. Its header must say it runs on the remote's OS and CPU as the probe found them: an ELF's
     `e_machine` (`x86_64`, `aarch64`, 64-bit only) for Linux, a Mach-O executable's `cputype`
     for macOS (any slice of a universal binary). It is read here, without a `file` command; an archive, a script or a
     binary for another platform is refused before anything is copied, naming the build that
     is needed.
  2. `scp` uploads it to `~/.local/bin/.dot.tmp`, which is made `0755` and run there with
     `--version`. Unless it says `dot <this version>`, the upload is removed and nothing is
     replaced: one that does not run at all (a dynamically linked build for another libc) is
     reported with what the remote said. The messages name where the file came from
     (`--dot-binary` or `DOT_HOST_BINARY`).
  3. `mv -f` renames it into place: a `dot` already there is replaced atomically.

`--dry-run` (on `dot secrets push` too) checks the file and says what would be copied; it
copies nothing. A Linux build
must be static: `scripts/build-linux.sh [x86_64|aarch64|all]` cross-compiles one with
`cargo zigbuild` (zig as the linker) for `<arch>-unknown-linux-musl`, from any machine, and
prints its path.

**`dot cloud-init --repo <https url> --profile <p>`** prints `#cloud-config` user data that, on
first boot and as the image's default user, installs `dot` with `install.sh` and applies the
profile non-interactively. It installs the same release as the `dot` that printed it
(`DOT_VERSION`, with `install.sh` from that tag; `--latest` for the latest). User data is
readable by the cloud provider, so it never holds a token, and the repository must be public.
`--secrets-from-file` embeds a secrets file (root-only until it is moved into place), under a
warning in the document and on stderr.

## Distribution

Releases are built by `.github/workflows/release.yml` for a `v*` tag: static musl binaries for
x86_64 and aarch64 Linux, a universal macOS binary, and x86_64 and aarch64 Windows zips, plus
the three JSON Schemas and a `SHA256SUMS` over everything. Every asset gets a signed build
provenance attestation (`gh attestation verify <file> -R trungngo/dot-cli`). The release is
created as a draft and published only once every asset is uploaded. Third-party actions are
pinned to commit SHAs.

`install.sh` (POSIX sh) and `install.ps1` (Windows PowerShell 5.1 and PowerShell 7):

1. Download the asset for this machine and `SHA256SUMS` from the public release (the latest,
   or `DOT_VERSION`), without any credentials, and refuse to install unless the checksum
   matches. Downloads are https-only; `install.ps1` follows redirects itself, so it behaves the
   same in both PowerShells.
2. Install `dot` to `~/.local/bin` (`%LOCALAPPDATA%\Programs\dot`, added to the user `PATH`).
3. With a repository URL (https, or ssh with the machine's own keys): clone it to `~/.dot`
   (or fast-forward it), record `DOT_REPO` in `~/.config/dot/local.env` (keeping, with a
   warning, another one already recorded), and run `dot apply` on the terminal when there is
   one.

A GitHub token (`GITHUB_TOKEN`, `GH_TOKEN`, or `gh auth token`) is looked up only when
something private needs it:

- A private dotfiles repository on github.com over https: git is first asked, without a
  prompt, whether its own credentials can read it (`git ls-remote`). Only when they cannot is
  the token handed to git, as an extra header in git's environment (`GIT_CONFIG_COUNT`, git
  2.31 or later), for that clone only. Nothing is written to any git config.
- A private fork's releases (`DOT_RELEASE_REPO=owner/name`): only when the public download
  fails is the release fetched through the GitHub API with the token. The token goes to curl
  through a `0600` header file in a private temp directory, and only API requests carry it:
  redirects to GitHub's storage host are fetched without it.

The token is never on a command line, in a URL, in a message or in `dot apply`'s environment;
`~/.curlrc` is ignored and shell tracing is turned off. The recommended token is a
fine-grained one that can only read the repositories it is for, with a short expiry, until a
git credential helper (`gh auth setup-git`, Git Credential Manager) is set up.

`dot self-update` replaces the binary with the latest release (or a fork's), checked against
`SHA256SUMS`, renamed into place atomically (on Windows the running executable is moved aside
first). It uses a token only for the API, where it lifts the anonymous rate limit or reaches a
private fork.

## Testing hooks

These environment variables exist for tests and sandboxes; the test suite never touches the
real home directory, network, package managers or sudo.

| variable | effect |
|---|---|
| `DOT_FACTS` | override facts (see [Facts and profiles](#facts-and-profiles)) |
| `DOT_STATE_DIR` | the state directory |
| `DOT_SYSROOT=<dir>` | root-owned files go under `<dir>`, written as this user without sudo |
| `DOT_PATH_FIRST=<dirs>` | directories kept ahead of every `path:` entry (a harness's fakes) |
| `DOT_GITHUB_API`, `DOT_GITLAB_API` | the release APIs (also GitHub Enterprise) |
| `DOT_URL_REWRITE=from=to,...` | send download requests to a local stand-in |
| `DOT_SNAPD=1\|0` | whether snapd counts as available |
| `DOT_RELEASE_BASE`, `DOT_GITHUB_URL` | where the installers download from |
| `DOT_HOST_BINARY=<path>` | the default `--dot-binary` for `--host` and `dot secrets push` |

## Limits

- The plan judges each step against the machine as it is: a step that only works after an
  earlier step has run (a `git` clone where an earlier `absent` removes an old link) shows a
  conflict until that step has run.
- Templates manage whole files; an application that also writes its own settings file needs a
  `block` (for line-based files) or its own mechanism.
- Conflicting packages are left to the package manager.
- System `service` units are not moved under `DOT_SYSROOT`.
- Not implemented yet: the 1Password provider, secrets provider plugins, `dot secrets
  migrate`, `dot init` (scaffold a repository and a pre-commit secrets check), bundles from
  other repositories, and packages for more distributions than the managers listed.
