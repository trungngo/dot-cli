# Configuration reference

<!-- Generated from `dot schema repo|bundle|secrets` by crates/dot-cli/tests/reference.rs:
     do not edit by hand. Regenerate with `DOT_BLESS=1 cargo test -p dot-cli --test reference`. -->

Every key `dot` accepts in a dotfiles repository, generated from the JSON Schemas that
`dot schema repo|bundle|secrets` prints and every release publishes. What the directives
do when they run (ordering, idempotence, `unless`, undo and state) is in
[design.md](design.md).

Unknown keys are errors. Paths may start with `~`; most strings are rendered as templates
(`{{ var }}`) first.

For completion and validation in editors with the YAML language server (VS Code's YAML
extension, Neovim, Helix, ...), put the matching line at the top of each file:

    # yaml-language-server: $schema=https://github.com/trungngo/dot-cli/releases/latest/download/dot.schema.json   (dot.yml)
    # yaml-language-server: $schema=https://github.com/trungngo/dot-cli/releases/latest/download/bundle.schema.json   (bundles/<name>/bundle.yml)
    # yaml-language-server: $schema=https://github.com/trungngo/dot-cli/releases/latest/download/secrets.schema.json   (secrets.yml)

Contents:

- [`dot.yml`](#dotyml)
- [`bundles/<name>/bundle.yml`](#bundlesnamebundleyml)
- [`secrets.yml`](#secretsyml)
- [Types](#types)

## `dot.yml`

`dot.yml`, at the root of a dotfiles repository: profiles, the bundle selection, repository-wide variables and defaults, and package names that differ per manager.

Schema: [`dot.schema.json`](https://github.com/trungngo/dot-cli/releases/latest/download/dot.schema.json) (`dot schema repo`).

| key | type | description |
|---|---|---|
| `bundles` | list of string | Bundles applied when neither the command line nor the profile names any (default: all). |
| `bundles_dir` | string | Directory holding the bundles, relative to the repository root (default: `bundles`). |
| `defaults` | [DefaultsFile](#defaultsfile) or null | Option defaults for every bundle. |
| `packages` | map of mapping | Package names that differ per manager: `fd: { apt: fd-find, brew: fd }`. |
| `profiles` | map of [ProfileFile](#profilefile) | Named fact sets with their bundle selection, vars and defaults; auto-detected or pinned. |
| `providers` | map of string | Capability -> bundle providing it, for every profile. |
| `secrets_provider` | string | Default secret provider for profiles that do not set one. |
| `vars` | map of string or bool or number (or a list) or [VarDecl](#vardecl) | Repository-wide variables: values, or declaration mappings. |

## `bundles/<name>/bundle.yml`

A bundle: manifest keys, then an ordered list of steps.

Schema: [`bundle.schema.json`](https://github.com/trungngo/dot-cli/releases/latest/download/bundle.schema.json) (`dot schema bundle`).

| key | type | description |
|---|---|---|
| `after` | list of string | Ordering only: run after these if they are selected, never pulls them in. |
| `check` | string | Optional fast "already satisfied" command: when it exits 0 the bundle's steps are skipped (unless `--force`). |
| `defaults` | [DefaultsFile](#defaultsfile) or null | Option defaults for this bundle's steps, over the profile's and `dot.yml`'s. |
| `description` | string | Free text: what the bundle is for. |
| `handlers` | map of [Step](#step) | Steps run once, after the bundle's steps, only when a step that names them in `notify:` changed something: name -> step (e.g. `restart-sshd: { service: { name: ssh, restart: true } }`). In the order written. |
| `implicit` | bool | Always included (when its `when:` matches), e.g. `common`. Default: `false`. |
| `name` | string | Bundle name; defaults to the name of the directory holding `bundle.yml`. |
| `provides` | list of string | Capabilities this bundle provides. |
| `requires` | list of string | Bundles (or `cap:<capability>`) this one needs; installed first. |
| `requires_if` | list of [RequiresIf](#requiresif) | Conditional requirements. |
| `steps` | list of [Step](#step) | What the bundle does, in order: each step is one directive (`link`, `package`, `template`, `shell`, ...) with optional `when`, `label`, `if`, `only_if`, `unless` and `notify`. |
| `vars` | map of string or bool or number (or a list) or [VarDecl](#vardecl) | Variables for this bundle: values, or declaration mappings. |
| `when` | [When](#when) or null | The whole bundle is skipped unless these facts match. |

## `secrets.yml`

`secrets.yml`, at the root of a dotfiles repository: where each secret lives, by logical name. It holds no values.

Schema: [`secrets.schema.json`](https://github.com/trungngo/dot-cli/releases/latest/download/secrets.schema.json) (`dot schema secrets`).

| key | type | description |
|---|---|---|
| `default_provider` | string | Provider for bare refs. A profile's `secrets_provider` overrides it. Default: `envfile`. |
| `providers` | [ProvidersConfig](#providersconfig) | Settings of each provider. |
| `secrets` | map of [SecretSpec](#secretspec) | Logical secret name → where it lives. |

## Types

### AbsentOnly

What an `absent:` entry removes.

One of:

- `link`: Only a symlink (or junction).

### AbsentOpts

`absent:` options for one path.

| key | type | description |
|---|---|---|
| `only` | [AbsentOnly](#absentonly) or null | `link`: remove it only while it is a symlink (or junction), never a real file or directory someone put there: for retiring a link an old setup made. |
| `pointing_into` | string | With `only: link`: only a link pointing into this directory (`~/dotfiles`), as an old setup made it; a link pointing elsewhere is kept. |
| `root` | bool | Allow a path outside the home directory, removed through sudo when this user cannot (default: false). |

### AptRepo

An apt repository.

| key | type | description |
|---|---|---|
| `architectures` | list of string | Architectures (default: this machine's, e.g. `amd64`); `[]` leaves them out. |
| `components` | list of string | Components (default: `[main]`); `[]` for a flat repository. |
| `fallback_suite` | string | Used instead of the (single) suite when the repository does not publish it (`<url>/dists/<suite>/Release` is not found): a vendor that lags behind new releases. |
| `file` | string | The sources file (default: `/etc/apt/sources.list.d/<name>.sources`). |
| `fingerprint` | string (or a list) or null | The key's expected fingerprint (spaces allowed), or a list of them for a keyring that holds several primary keys (a vendor rotating its key): a downloaded key file holding a key that is not pinned is refused before anything is installed. |
| `if_missing` | bool | Only write the sources file when it does not exist: for packages that maintain their own entry once installed (VS Code). Default: false (kept exactly as configured). |
| `key` | string | URL of the signing key (ASCII-armored or binary); stored dearmored in `keyring`. |
| `keyring` | string | Where the key goes (default: `/etc/apt/keyrings/<name>.gpg`). |
| `replaces` | list of string | Other sources files for the same repository to remove when the source is written (e.g. `/etc/apt/sources.list.d/docker.list` from the vendor's instructions): apt refuses to read any source when two entries for one repository name different `Signed-By` keys. |
| `suites` (required) | string (or a list) | Suite(s) (`Suites:`), usually `"{{ codename }}"`, or a fixed one (`stable`). |
| `url` (required) | string | Base URL (`URIs:`), e.g. `https://download.docker.com/linux/ubuntu`. |

### BlockOpts

`block:` — a managed block of lines in a file `dot` does not own (`~/.bashrc`, ...).

| key | type | description |
|---|---|---|
| `comment` | string | Comment prefix for the marker lines (default "#"). |
| `content` (required) | string | The block's lines (a template; `secret()` only in a file readable by its owner alone). |
| `file` (required) | string | The file (created if absent). |
| `marker` | string | Tells several blocks of one bundle in the same file apart: letters, digits, `.`, `_`, `-`. |
| `root` | bool | The file is root-owned (`/etc/hosts`): read with sudo when needed, written through `sudo install -o root` (default false). |

### BrewTap

A Homebrew tap.

| key | type | description |
|---|---|---|
| `tap` (required) | string | `owner/repo`. |
| `trust` | bool | `brew trust --tap` it, so Homebrew loads its formulae and casks (default: true; only where Homebrew has `brew trust`). |
| `url` | string | Clone URL, when it is not `https://github.com/<owner>/homebrew-<repo>`. |

### CleanOpts

`clean:` options for one directory.

| key | type | description |
|---|---|---|
| `recursive` | bool | Also clean subdirectories (symlinked directories are never entered). Default: `false`. |

### CopyOpts

`copy:` options for one destination.

| key | type | description |
|---|---|---|
| `mode` | string or null | File mode (Unix) for the copies. Default: the source's. |
| `root` | bool | Root-owned copies (`/etc/...`): written through `sudo install -o root` (default false). |
| `src` (required) | string | Source file or directory, relative to the bundle directory. |

### CreateOpts

`create:` options for one path.

| key | type | description |
|---|---|---|
| `file` | bool | An empty file instead of a directory, made only when missing: its content is never touched (`~/.local.zshrc`, sourced by a managed rc file). `dot remove` removes it only while it is still empty (default false). |
| `mode` | string or null | Mode for the directory (Unix only), e.g. "0700". |
| `root` | bool | A root-owned directory (`/opt/...`): made with `sudo install -d -o root` (default false). |

### DefaultsFile

`defaults:` in `dot.yml`, a profile, or a bundle.

| key | type | description |
|---|---|---|
| `link` | [LinkDefaultsFile](#linkdefaultsfile) | Defaults for `link` steps. |
| `shell` | [ShellDefaultsFile](#shelldefaultsfile) | Defaults for `shell` steps. |

### DefaultsType

The value type of a macOS preference, as `defaults write -<type>` takes it.

Type: `string`, `int`, `float`, `bool`.

### DnfRepo

A dnf / yum repository.

| key | type | description |
|---|---|---|
| `baseurl` (required) | string | `baseurl=` (dnf expands `$releasever` and `$basearch` itself). |
| `description` | string | `name=` (default: the step's `name`). |
| `file` | string | The `.repo` file (default: `/etc/yum.repos.d/<name>.repo`). |
| `gpgkey` | string | `gpgkey=`: the signing key's URL; packages are checked against it (`gpgcheck=1`). |
| `if_missing` | bool | Only write the file when it does not exist (default: false). |

### DownloadOpts

`download:` — a file from an https URL (or a release): saved as a file, unpacked into a directory, or (a `.deb` / `.rpm`) installed through the package manager.

| key | type | description |
|---|---|---|
| `bin` | string or list of string or map of string or null | With `extract`: executables in the unpacked tree to link into `bin_dir`: paths in the tree (`bin/zed`, linked by their file name), or a mapping of link name to a glob over paths in the tree (`godot: 'Godot_v*'`). |
| `bin_dir` | string | Where `bin` links go (default: ~/.local/bin). |
| `dest` | string | Where the file goes; with `extract`, the directory the archive is unpacked into. |
| `extract` | [Extract](#extract) or null | Unpack an archive into `dest` (a directory): `auto` (from the URL's file name), `tar.gz`, `tar.xz` or `zip`. Default: the file is saved as it is. |
| `latest` | string or [LatestOpts](#latestopts) or null | Ask this URL for the current version, which replaces `{latest}` in `url`. |
| `mode` | string or null | File mode (Unix) of a downloaded file (default "0644"). |
| `package` | string | Install the file (a `.deb` for apt, a `.rpm` for dnf / zypper) as this package instead of saving it; satisfied while the package is installed. |
| `release` | [ReleaseRef](#releaseref) or null | Take the file from a project's latest release instead of a fixed `url`. |
| `replace` | bool | With `extract`: `dest` is the unpacked tree itself, replaced whole on an update (old files do not linger), instead of the archive being merged into it. A tree `dot` did not install is kept as `<dest>.dot-backup`. Default: `false`. |
| `root` | bool | `dest` is root-owned (`/usr/local/...`): written through sudo (default false). |
| `sha256` | string | The file's expected SHA-256 (64 hexadecimal digits): a download that does not have it is refused. Without it, the step is satisfied while the server says the file has not changed (ETag / Last-Modified). |
| `sha256_url` | string | Where the vendor publishes the file's SHA-256 (https; `{latest}` is filled in as in `url`): a bare digest (`go1.24.0.linux-amd64.tar.gz.sha256`) or a `sha256sum` list naming the file. The download is checked against it; no fixed digest is pinned. |
| `strip` | int | With `extract`: leading directories of the archive to strip (`1` for `go/...`); each must be the archive's only top-level entry. |
| `url` | string | https:// URL of the file; `{latest}` in it is filled in from `latest`. |

### EnvMap

Environment variables for the later steps of the run (and `dot env --shell`).

Type: map of string.

### EnvfileConfig

`envfile` settings.

| key | type | description |
|---|---|---|
| `files_dir` | string | Directory holding `envfile-file:` secrets. Default: `files/` next to `path`. |
| `path` | string | dotenv file holding values. Default `~/.config/dot/secrets.env`. `~` is the home directory; a relative path is relative to `~/.config/dot`. |

### Extract

How a downloaded asset is unpacked.

One of:

- `tar.gz`, `tar.xz`, `zip`
- `auto`: From the asset's file name: `.tar.gz`/`.tgz`, `.tar.xz`/`.txz`, `.zip`, else a raw binary.
- `raw`: The asset is the executable itself.

### GitOpts

`git:`

| key | type | description |
|---|---|---|
| `branch` | string | Branch to clone and follow (default: the remote's default branch, or the checkout's current branch). |
| `depth` | int | Clone only the last `depth` commits (`git clone --depth`, e.g. 1); updates fetch what is new on top. Default: the whole history. |
| `dest` (required) | string | Where the checkout lives. |
| `repo` (required) | string | Repository URL: https:// (or file://). |
| `update` | bool | Fast-forward an existing clean checkout to the remote (default: true). |

### GithubReleaseOpts

`github_release:` in its mapping form. The scalar form is the repository alone.

| key | type | description |
|---|---|---|
| `asset` | string or map of string or null | Which asset: a glob, or a glob per platform (default: picked from the OS and CPU). |
| `bin` | string or list of string or map of string or null | Executables to install from the asset (default: the repository name): names, or a mapping of the installed name to a glob matched against the archive's file names. |
| `extract` | [Extract](#extract) | How to unpack the asset (default: auto, from its file name). |
| `host` | string | The GitLab host (default gitlab.com); `source: gitlab` only. |
| `package` | string | Install the release's `.deb` (apt) or `.rpm` (dnf, zypper) as this package instead of taking executables out of an archive. Satisfied while the package is installed from the latest release (or the `tag:` one). |
| `replace` | bool | Replace an executable of the same name that dot did not install, keeping it as `<name>.dot-backup` (default: false: such a file is a conflict and the step fails). |
| `repo` (required) | string | GitHub repository, `owner/name`. |
| `sha256` | string | The SHA-256 the chosen asset must have (64 hex digits), checked instead of the checksum the release publishes. Needs `tag:` (the latest release's asset changes with each release); with `asset:` per platform, pin one platform per step. |
| `source` | [ReleaseSource](#releasesource) | Where the release is published: `github` (default) or `gitlab` (then `repo` is the project's path, `group/name`). |
| `tag` | string | Install the release with this tag (e.g. `v1.2.3`) instead of the latest one. |
| `to` | string | Directory to install into (default: ~/.local/bin). |
| `token` | string | Logical name of a secret holding a token for API requests (default: the GITHUB_TOKEN or GH_TOKEN environment variable; GITLAB_TOKEN for GitLab). Never logged. |

### LastpassConfig

`lastpass` settings.

| key | type | description |
|---|---|---|
| `username` | string | The account to log in as (`lpass login <username>`) when `lpass` is logged out. |

### LatestOpts

`latest:` — where to ask for the current version, which replaces `{latest}` in `url` (`https://go.dev/VERSION?m=text` -> `go1.23.4`).

| key | type | description |
|---|---|---|
| `pattern` | string | A regular expression picking the version out of the answer: its first capture group, else the whole match (default: the first line, trimmed). |
| `url` (required) | string | https:// URL answering with the version (a small text). |

### LinkDefaultsFile

Option defaults for `link` steps; a step's own options win.

| key | type | description |
|---|---|---|
| `create` | bool | Create missing parent directories of the link. |
| `on_conflict` | [OnConflict](#onconflict) or null | What to do when a file or directory (not a link) is in the way. |
| `relink` | bool | Replace a symlink that points somewhere else. |

### LinkOpts

Options left out stay `None` here and in the checked model: they are filled from the inherited defaults (repo, profile, bundle, then built in) at plan time.

| key | type | description |
|---|---|---|
| `create` | bool | Create missing parent directories of the destination (default: true). |
| `on_conflict` | [OnConflict](#onconflict) or null | What to do when a real file or directory is in the way (default: backup). |
| `relink` | bool | Replace a symlink that points somewhere else (default: true). |
| `src` (required) | string | Source path, relative to the bundle directory. |

### MacosDefaultOpts

One macOS preference (`defaults write <domain> <key> -<type> <value>`).

| key | type | description |
|---|---|---|
| `current_host` | bool | Write the per-host preference (`defaults -currentHost`); default false. |
| `domain` (required) | string | Preference domain, e.g. `com.apple.dock` or `NSGlobalDomain`. |
| `key` (required) | string | The preference's key, e.g. `autohide`. |
| `type` (required) | [DefaultsType](#defaultstype) | The value's type: `string`, `int`, `float` or `bool` (`defaults write -<type>`). |
| `value` (required) | string or number or int or bool | The value, of that type. |

### OnConflict

What a `link` does when a real file or directory is where the link goes.

One of:

- `backup`: Move the existing file or directory to `<dest>.dot-backup` (numbered if taken).
- `overwrite`: Delete the existing file or directory.
- `fail`: Fail the step.

### OnePasswordConfig

`1password` settings (the provider is not implemented yet).

| key | type | description |
|---|---|---|
| `account` | string | The account, e.g. `my.1password.com` (for the provider, once it is implemented). |

### PackageRepoOpts

`package_repo:` — a vendor package repository for this machine's package manager. Only the entry for the manager this machine uses is applied, so one step can cover several platforms.

| key | type | description |
|---|---|---|
| `apt` | [AptRepo](#aptrepo) or null | apt (Debian, Ubuntu): a deb822 `.sources` file with `Signed-By:` its own keyring. |
| `brew` | string or [BrewTap](#brewtap) or null | Homebrew: a tap (`owner/repo`), trusted unless `trust: false`. |
| `dnf` | [DnfRepo](#dnfrepo) or null | dnf / yum (Fedora, RHEL): a `.repo` file in `/etc/yum.repos.d`. |
| `name` (required) | string | Names the repository's files (`/etc/apt/sources.list.d/<name>.sources`, `/etc/apt/keyrings/<name>.gpg`, `/etc/yum.repos.d/<name>.repo`) and its zypper alias: letters, digits, `.`, `_` and `-`. |
| `scoop` | string or [ScoopBucket](#scoopbucket) or null | scoop: a bucket (`extras`, or a name with `url`). |
| `winget` | [WingetSource](#wingetsource) or null | winget: a source (needs an elevated terminal). |
| `zypper` | [ZypperRepo](#zypperrepo) or null | zypper (openSUSE): `zypper addrepo`, the signing key imported with `rpm --import`. |

### PathOpts

`path:` options for one directory.

| key | type | description |
|---|---|---|
| `persist` | [PathPersist](#pathpersist) or null | `user`: on Windows, also add the directory to the user `Path` in the registry (`HKCU\Environment`), so programs started outside a `dot env` shell see it; `dot remove` takes it out again. Nothing to persist on other systems. |
| `position` | [PathPosition](#pathposition) or null | `prepend` (default: in front of the existing PATH) or `append`. |

### PathPersist

Where a `path:` entry is persisted besides `dot`'s own record.

One of:

- `user`: The Windows user `Path` (`HKCU\Environment`).

### PathPosition

Type: `prepend`, `append`.

### Persist

Where a prompted answer is kept.

One of:

- `local`: Written to `~/.config/dot/local.env` and asked once.

### ProfileFile

A kind of machine: which facts it matches, which bundles it applies, and its variables and defaults.

| key | type | description |
|---|---|---|
| `bundles` | list of string | Bundles to apply (default: `dot.yml`'s `bundles`). |
| `defaults` | [DefaultsFile](#defaultsfile) or null | Option defaults for this profile's bundles (over `dot.yml`'s). |
| `description` | string | What the profile is for. |
| `match` | [When](#when) or null | Facts this profile is for. A profile without `match` is only used when named with `--profile` or pinned with `DOT_PROFILE`; `match: {}` matches every machine. |
| `providers` | map of string | Capability -> bundle providing it (over `dot.yml`'s `providers`). |
| `secrets_provider` | string | Secret provider for this profile, e.g. `lastpass` or `envfile`. |
| `vars` | map of string or bool or number (or a list) or [VarDecl](#vardecl) | Variables for this profile (over `dot.yml`'s): values, or declaration mappings. |

### ProvidersConfig

Per-provider settings. Typed, so a misspelt provider or option is an error.

| key | type | description |
|---|---|---|
| `1password` | [OnePasswordConfig](#onepasswordconfig) or null | 1Password, through its CLI (`op`). Not implemented yet: `op://` refs parse, and resolve to their `fallback`. |
| `envfile` | [EnvfileConfig](#envfileconfig) or null | A dotenv file (and a directory of file secrets) outside the repository. |
| `lastpass` | [LastpassConfig](#lastpassconfig) or null | LastPass, through `lastpass-cli` (`lpass`). |

### ReleaseRef

`download: { release: ... }`: the file is an asset of a project's latest release.

| key | type | description |
|---|---|---|
| `asset` | string or map of string or null | Which asset: a glob, or a glob per platform (default: picked from the OS and CPU). |
| `host` | string | The GitLab host (default gitlab.com); `source: gitlab` only. |
| `repo` (required) | string | `owner/name` on GitHub, `group/name` on GitLab. |
| `source` | [ReleaseSource](#releasesource) | `github` (default) or `gitlab`. |
| `token` | string | Logical name of a secret holding a token for the release API (default: GITHUB_TOKEN / GH_TOKEN, or GITLAB_TOKEN). |

### ReleaseSource

Where a release is published.

One of:

- `github`: GitHub (`DOT_GITHUB_API` for GitHub Enterprise).
- `gitlab`: GitLab: gitlab.com, or a self-managed instance named by `host`.

### RequiresIf

A requirement that applies only on matching machines.

| key | type | description |
|---|---|---|
| `requires` (required) | list of string | Bundles (or `cap:<capability>`) required when they do. |
| `when` (required) | [When](#when) | The facts that must match. |

### ScoopBucket

A scoop bucket.

| key | type | description |
|---|---|---|
| `bucket` (required) | string | A known bucket (`extras`, `nerd-fonts`) or any name with `url`. |
| `url` | string | Its git URL, for a bucket scoop does not know. |

### SecretFileOpts

`secret_file:` options for one destination.

| key | type | description |
|---|---|---|
| `allow_loose_mode` | bool | Accept a mode readable by group or others (default: false). |
| `mode` | string or null | File mode (Unix); default "0600". Modes readable by group or others are refused unless `allow_loose_mode` is set. |
| `secret` (required) | string | Logical secret name (see `secrets.yml`); its value is written as the file's content. |

### SecretKind

How a secret is consumed: as a string (`{{ secret("x") }}`) or as a file (`secret_file:`).

Type: `value`, `file`.

### SecretRef

`envfile:KEY`, `envfile-file:rel/path`, `lastpass://Folder/Item#field`, `op://vault/item/field` (1Password: not implemented yet), or a bare name read by the default provider.

Type: string.

### SecretSpec

Where one secret lives.

| key | type | description |
|---|---|---|
| `fallback` | [SecretRef](#secretref) or null | Tried when `ref` has no value or its provider is unavailable. |
| `kind` | [SecretKind](#secretkind) | `value` (a string, the default) or `file` (for `secret_file`). |
| `optional` | bool | A machine may lack it: with no value (and nothing to ask on a non-interactive run, or its provider unavailable) the steps that use it are skipped instead of failing their bundle, and nobody is prompted for it. Default false. |
| `ref` (required) | [SecretRef](#secretref) | Where the value is: `envfile:NAME`, `envfile-file:path`, `lastpass://Folder/Item#field`, `op://vault/item/field` (1Password: not implemented yet, resolves to `fallback`), or a bare name for the default provider. |

### ServiceManagerKind

Which service manager a `service:` step uses.

One of:

- `systemd`, `systemd-user`, `launchd`, `windows`
- `auto`: systemd (system services) on Linux, launchd on macOS, the service control manager on Windows.

### ServiceOpts

`service:` — a service installed, enabled and running (or not).

| key | type | description |
|---|---|---|
| `enable` | bool | Enabled at boot / login (default: true); false disables it. |
| `linger` | bool | systemd-user only: keep the user's services running without a login session (`loginctl enable-linger`); default false. |
| `manager` | [ServiceManagerKind](#servicemanagerkind) or null | Default: auto. |
| `name` (required) | string | Unit name (systemd; `.service` is added when there is no suffix), job label (launchd) or service name (Windows). |
| `restart` | bool | Restart the service (when it is running) every time the step runs: for handlers (`handlers: { restart-sshd: { service: { name: ssh, restart: true } } }`), which run only when a step that notifies them changed something. Default false. |
| `start` | bool | Running now (default: true); false stops it. |
| `unit` | string | A templated unit file (systemd) or property list (launchd) to install, relative to the bundle directory. |

### ShellDefaultsFile

Option defaults for `shell` steps; a step's own options win.

| key | type | description |
|---|---|---|
| `interactive` | bool | Give the command the terminal (stdin) instead of running it with no input. |
| `interpreter` | [ShellInterpreter](#shellinterpreter) or null | The program `shell` steps (and `unless:` checks) run through, unless a step names one. |
| `prelude` | string | Script lines run before every `shell` step's `cmd`, `unless` and `upgrade` (e.g. `set +eu; . "$SDKMAN_DIR/bin/sdkman-init.sh"`), unless a step sets its own. |

### ShellInterpreter

The program a `shell` step's command runs through. Unset: `sh` on Unix; `pwsh` on Windows when installed, else `powershell`.

One of:

- `sh`: `sh -c`
- `bash`: `bash -c`
- `zsh`: `zsh -c`
- `pwsh`: PowerShell 7; multi-line scripts are fine.
- `powershell`: Windows PowerShell 5.1 (Windows only).
- `cmd`: `cmd.exe /C` (Windows only; one line).

### ShellOpts

`shell:` as a mapping (a plain string is just `cmd`).

| key | type | description |
|---|---|---|
| `cmd` (required) | string | The command (a script: several lines are fine), with `{{ variables }}` rendered. |
| `cwd` | string | Working directory; relative paths are from the bundle directory (the default). |
| `env` | map of string | Extra environment variables. |
| `export_sensitive` | list of string | Sensitive variables to export to the command anyway (others are withheld). |
| `interactive` | bool | Hand the terminal to the command (prompts, sudo) instead of capturing its output (default: false). |
| `interpreter` | [ShellInterpreter](#shellinterpreter) or null | The program the command (and `unless`) runs through; default from `defaults:`, else the platform's (`sh`; `pwsh` or `powershell` on Windows). |
| `prelude` | string | Script lines run before `cmd`, `unless` and `upgrade` (default from `defaults: { shell: { prelude } }`): sourcing an init script that defines a shell function (SDKMAN's `sdk`). |
| `sudo` | bool | The command uses `sudo`: the run asks for the password once, up front, and keeps it fresh, instead of the command prompting in the middle of the run (default: false). |
| `unless` | string | Skip the step when this command exits 0. |
| `upgrade` | string | Under `dot apply --upgrade`, run this instead of skipping when `unless` succeeds (the tool is installed): e.g. `rustup update` for a `curl \| sh` installer. Needs `unless`. |

### Step

One step: optional `when` / `label` and exactly one directive key.

| key | type | description |
|---|---|---|
| `absent` | list of string or map of [AbsentOpts](#absentopts) or null | Paths that must not exist (removed if they do). Outside the home directory only with `root: true`. |
| `block` | [BlockOpts](#blockopts) | A managed block of lines in a file dot does not own. |
| `clean` | list of string or map of [CleanOpts](#cleanopts) or null | Directories to remove dangling repo symlinks from. |
| `copy` | map of string or [CopyOpts](#copyopts) | Files copied (not linked): destination -> source (relative to the bundle directory). |
| `create` | list of string or map of [CreateOpts](#createopts) or null | Directories (or, with `file: true`, empty files) to create. |
| `download` | [DownloadOpts](#downloadopts) | A file from an https URL or a release: saved (`dest`), unpacked (`dest` + `extract`), or a .deb / .rpm installed as a package (`package`). |
| `env` | [EnvMap](#envmap) | Environment variables for the later steps of the run (and `dot env --shell`). |
| `git` | [GitOpts](#gitopts) | Clone a repository over https, and fast-forward a clean checkout of it. |
| `github_release` | string or [GithubReleaseOpts](#githubreleaseopts) | Install executables from a GitHub repository's latest release (or the one `tag:` names): `owner/name`, or a mapping. |
| `if` | string | Skip this step unless this expression over variables is true (Jinja, without braces): `godot_mono`, `client_id != ""`. |
| `label` | string | Name shown in output instead of the generated one. |
| `link` | map of string or [LinkOpts](#linkopts) | Symlinks: destination -> source (relative to the bundle directory). |
| `macos_defaults` | [MacosDefaultOpts](#macosdefaultopts) (or a list) | macOS preferences, set with `defaults write` (macOS only). |
| `notify` | string (or a list) | Handlers (the bundle's `handlers:`) to run at the end of the bundle when this step changes something. |
| `only_if` | string | Apply this step only when this command exits 0; asked only when the step has something to do, otherwise the step is skipped. |
| `package` | list of string or mapping | Packages to install: a list of names for this machine's package manager (mapped through dot.yml's `packages:`), or a mapping of package manager to names (plus `upgrade`). |
| `package_repo` | [PackageRepoOpts](#packagerepoopts) | A vendor package repository for this machine's package manager (apt, dnf, zypper, brew tap, scoop bucket, winget source). |
| `path` | list of string or map of [PathOpts](#pathopts) or null | Directories added to PATH for the later steps of the run (and `dot env --shell`). |
| `secret_file` | map of string or [SecretFileOpts](#secretfileopts) | Files holding a secret: destination -> secret name. |
| `service` | string or [ServiceOpts](#serviceopts) | A service installed, enabled and running (systemd, launchd, Windows). |
| `shell` | string or [ShellOpts](#shellopts) | A command run through the platform shell. |
| `template` | map of string or [TemplateOpts](#templateopts) | Files rendered from templates: destination -> source (relative to the bundle directory). |
| `unless` | string | Skip this step when this command exits 0; asked only when the step has something to do. On a `shell` step: the command's own `unless`. |
| `user` | [UserOpts](#useropts) | A user's login shell and supplementary groups. |
| `when` | [When](#when) | Skip this step unless these facts match. |

### TemplateOpts

`template:` options for one destination.

| key | type | description |
|---|---|---|
| `mode` | string or null | File mode (Unix), e.g. "0644". Default: an existing file keeps its mode, a new one gets "0644"; "0600" when the template uses a secret. A template that uses a secret refuses a mode readable by group or others. |
| `root` | bool | A root-owned file (`/etc/...`): written through `sudo install -o root` (default false). |
| `src` (required) | string | Template source, relative to the bundle directory. Rendered with the same variables as the rest of the bundle; `secret()` is allowed. |

### UserOpts

`user:` — the login shell and supplementary groups of a user (default: the one running `dot`).

| key | type | description |
|---|---|---|
| `groups` | list of string | Supplementary groups the user must be in (others are left alone). They take effect at the next login. |
| `name` | string | The user (default: the one running dot). |
| `shell` | string | Login shell: a name found on `PATH` (`zsh`) or an absolute path. Added to `/etc/shells` when missing there. |

### VarDecl

A variable declaration: constraints, not a value (its `default` becomes the value of the layer that declares it).

| key | type | description |
|---|---|---|
| `default` | string or bool or number (or a list) or null | The value when no higher layer sets one. |
| `description` | string | Free text: what the variable is for. |
| `pattern` | string | A regular expression the whole value must match. |
| `per_machine` | bool | The value belongs to one machine (its name, its own disk): `dot apply --host` never copies this machine's value to another one, and asks for (or takes `--var`) the remote's own instead. Default false. |
| `persist` | [Persist](#persist) or null | Where a prompted answer is kept: `local` writes it to `~/.config/dot/local.env`, so it is asked once. Without it the answer lasts one run. |
| `prompt` | string | Question asked (before anything runs) when the variable has no value; a run that cannot ask fails instead, listing every such variable. |
| `required` | bool | Missing (or empty) is an error before anything runs. |
| `sensitive` | bool | Redacted in output and not exported to shell steps unless a step asks. Defaults to true for names that look like credentials (`*token*`, `*password*`, `*secret*`, ...). |
| `type` | [VarType](#vartype) or null | The value's type (default: string). |

### VarType

The type a variable's value is checked against (and converted to, in templates).

One of:

- `string`: Any string (the default).
- `bool`: `true` or `false`.
- `int`: A whole number.
- `list`: A YAML list, or a comma-separated string (from `.env`).
- `path`: A string with `~` expanded to the home directory.

### When

Fact name -> accepted value(s). All keys must match; any listed value matches.

| key | type | description |
|---|---|---|
| `arch` | string (or a list) | CPU architecture: `x86_64` or `aarch64`. |
| `codename` | string (or a list) | Release codename (`VERSION_CODENAME` in /etc/os-release), e.g. `noble`. |
| `distro` | string (or a list) | Linux distribution (`ID` in /etc/os-release), e.g. `ubuntu`, `debian`, `fedora`. |
| `env` | `wsl`, `container`, `vm`, `ssh`, `none`, `!wsl`, `!container`, `!vm`, `!ssh`, `!none` or string (or a list) | Special environment: `wsl`, `container`, `vm`, `ssh`, or `none` for none of them. `!value` (quoted) excludes one. |
| `family` | string (or a list) | Distribution family (`ID_LIKE` in /etc/os-release), e.g. `debian`, `rhel`, `arch`, `alpine`. |
| `init` | `systemd`, `launchd`, `other`, `!systemd`, `!launchd`, `!other` or string (or a list) | Init system: `systemd`, `launchd` or `other`. |
| `os` | `linux`, `macos`, `windows`, `!linux`, `!macos`, `!windows` or string (or a list) | Operating system: `linux`, `macos` or `windows`. |
| `pkg` | `brew`, `apt`, `dnf`, `pacman`, `zypper`, `apk`, `winget`, `scoop`, `!brew`, `!apt`, `!dnf`, `!pacman`, `!zypper`, `!apk`, `!winget`, `!scoop` or string (or a list) | The package manager in use, e.g. `apt`, `brew`, `dnf`, `winget`, `scoop`. |
| `role` | `desktop`, `server`, `!desktop`, `!server` or string (or a list) | `desktop` or `server`: pinned locally (`DOT_ROLE`), else `desktop` when a graphical session or display manager is found. |
| `version` | string (or a list) | OS version: exact (`24.04`) or comparisons that must all hold (`>=22.04, <25`), compared numerically. |

### WingetSource

A winget source.

| key | type | description |
|---|---|---|
| `source` (required) | string | The source's name. |
| `type` | string | `--type` (e.g. `Microsoft.Rest`); winget's default when left out. |
| `url` (required) | string | The source's URL. |

### ZypperRepo

A zypper repository.

| key | type | description |
|---|---|---|
| `gpgkey` | string | The signing key's URL, imported with `rpm --import` before the repository is added. |
| `url` (required) | string | The repository's base URL. |
