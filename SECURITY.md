# Security

## Reporting a vulnerability

Please report security problems privately, not in a public issue. Use GitHub's
[private vulnerability reporting](https://github.com/trungngo/dot-cli/security/advisories/new)
(the "Report a vulnerability" button on the repository's Security tab). Include what you found,
how to reproduce it, and what an attacker could do with it. You should get an answer within a
week. Please give us time to release a fix before you disclose the problem.

Only the latest release is supported: fixes ship as a new release, and `dot self-update`
installs it.

## What dot does with secrets

`dot` is built so that secret values never need to be in your dotfiles repository.

- **By name only.** Bundles refer to secrets by logical name (`secret("github_token")`,
  `secret_file:`), and `secrets.yml` maps each name to a provider reference (an owner-only env
  file, `envfile`, or LastPass, `lastpass`). `secrets.yml` holds no values.
- **Resolved late.** Planning (`dot plan`, `dot check`) resolves no secrets. A run resolves only
  the secrets of the steps that have work to do, once per provider.
- **Redacted.** Values are held in memory types that print as `***` and are zeroed when dropped.
  They are redacted from output and log files. Diffs never show content rendered with a
  secret, and URLs holding one are recorded and shown as `<secret:name>`. Sensitive variables
  are not exported to shell steps unless a step asks for them.
- **Written privately.** Templates that use a secret and `secret_file` targets default to mode
  `0600`, and a template that uses a secret refuses a mode that group or others could read. On
  Windows, such files get an explicit ACL for you, `SYSTEM` and `Administrators` only.
- **Checked.** `dot` refuses a secrets file that others can read (or, on Windows, one whose ACL
  grants anyone else access), and one inside a git work tree unless git ignores it. `dot secrets
  check` reports missing, undeclared and unused secrets, unsafe permissions, and committed
  `.env*` values for keys marked `# secret`. It never prints a value.
- **Moved safely.** `dot apply --host user@host` and `dot secrets push user@host` resolve
  secrets on your machine and hand them to `dot` on the other one on the ssh session's stdin
  (or, when ssh has a terminal, through a FIFO in a private directory): never on a command
  line, in the environment, in a URL, in a log or in a temporary file, and never through the
  repository. The ssh that carries them never gets a terminal there (`-T`, whatever
  `~/.ssh/config` says) and has no escape character. They go only to a `dot` that proves it
  is this tool (`dot host-info`), not to whatever else is called `dot` there. Which secrets
  go is planned here; when the remote's own report (its pinned profile) would decide that,
  `dot` asks first, and without a terminal refuses unless `--profile` names it. `dot apply
  --host` keeps them in memory for that run unless you pass `--persist-secrets`; kept secrets
  are written owner-only, and only once the run there is confirmed. `dot secrets export` / `import`
  make and restore a passphrase-encrypted ([age](https://age-encryption.org)) backup.

## The installers and tokens

`install.sh` and `install.ps1` download `dot` from the public GitHub releases without any
credentials. They use a GitHub token (`GITHUB_TOKEN`, `GH_TOKEN` or `gh auth token`) only when
something private needs one: a private dotfiles repository that git can't read with its own
credentials, or releases from a private fork (`DOT_RELEASE_REPO`). When they do:

- The token never goes on a command line (where `ps` shows it), into a URL, into a log line, or
  into a git config. It goes to git as an extra header in git's environment, for that clone or
  pull only. It goes to curl through a `0600` header file in a private temp directory, which is
  deleted on exit.
- Only GitHub API requests carry it. Redirects to GitHub's storage host are followed without
  it, and never from https to plain http.
- `~/.curlrc` is ignored, shell tracing (`sh -x`) is turned off, and `dot apply` runs with
  `GITHUB_TOKEN` and `GH_TOKEN` removed from its environment.

An ssh repository URL (`git@github.com:...`, `ssh://...`) uses your ssh keys; the token is
never looked up for it.

`dot self-update` and `dot apply --host` read the token from the same places. `--host`
downloads `dot` for the remote machine locally and copies it over, so the token never leaves
your machine. `dot cloud-init` never puts a token in user data, because the cloud provider can
read it. For the same reason, `dot cloud-init --secrets-from-file` warns you: it embeds your
secrets file in the user data, where the provider (and, on most clouds, anything on the
instance that can reach the metadata service) can read it. Push secrets after boot instead
(`dot secrets push`, `dot apply --host`) unless that is acceptable.

## Trust model

- Downloads are https-only and verified against the release's `SHA256SUMS`. That file comes
  from the same release as the binary, so it catches a corrupted or truncated download, not a
  compromised release. Every release asset also has a signed build provenance attestation:
  `gh attestation verify <file> -R trungngo/dot-cli` proves the file was built by this
  repository's release workflow from the release's tag. The release workflow pins third-party
  actions to commit SHAs, and publishes a release only once all its assets are uploaded.
- Piping an installer into a shell runs whatever the server sends. You can download
  `install.sh` or `install.ps1` first, read it, and then run it. The installer on `main` and
  the latest release can differ for a while; to take both from one release, fetch the script
  from the release's tag and set `DOT_VERSION` to that tag (see the README).
- A dotfiles repository is code: its `shell` steps, templates and package repositories run with
  your privileges, and with root for steps that need `sudo`. Only apply repositories you trust,
  and run `dot plan` (or `dot apply --dry-run`) to see what a change will do.
- Pin `package_repo` apt keys with `fingerprint`: a pinned key that doesn't match is refused
  before anything is installed, and a key that isn't pinned is shown as "not pinned" in the
  plan. Keys are fetched over https only.
- `github_release` and `download: { release }` install a third party's latest release by
  default. The asset is checked against the checksum the release publishes (GitHub's asset
  digest, or a checksum file among the assets) when there is one, which, like `SHA256SUMS`
  above, catches corruption but not a compromised release, and a release that publishes none is
  installed unverified (the run says so). To control what gets installed, pin it:
  `github_release: { tag: v1.2.3, sha256: <digest> }`, or `download: { url, sha256 }` for a
  plain URL. A pinned digest that doesn't match installs nothing.
