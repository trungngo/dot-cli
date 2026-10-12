# A basic dotfiles repository

A small but complete repository for `dot`: two profiles, a few bundles, a template with
variables, and a secret kept outside git. Copy this directory to start your own (`dot` finds a
repository in `~/.dot` by itself):

    git clone --depth 1 https://github.com/trungngo/dot-cli /tmp/dot-cli
    cp -r /tmp/dot-cli/examples/basic ~/.dot && cd ~/.dot && git init
    dot check          # validate every file
    dot plan           # what would happen on this machine
    dot apply --dry-run
    dot apply

Run `dot apply` in a terminal: it asks for your git name and email the first time. Without a
terminal, give them with `--var git_name='Your Name' --var git_email=you@example.com`.

CI runs `dot check` and `dot plan` on it for every change to `dot`, and applies part of it in a
temporary home directory (`crates/dot-cli/tests/examples.rs`), so it keeps working.

## Layout

    dot.yml                     profiles, variables, package names per manager
    secrets.yml                 secret name -> where it lives (no values)
    .env                        committed, non-secret values
    .env.example                the secrets this repository needs (names only)
    bundles/
      common/bundle.yml         implicit: always applied first (Homebrew on macOS, base packages
                                on Linux, ~/.local/bin on PATH)
      shell/bundle.yml          links (inputrc, aliases.sh) and a managed block in ~/.bashrc /
                                ~/.zshrc that also loads `dot env`
      git/bundle.yml            git, its two variables, and ~/.gitconfig from gitconfig.j2
      cli-tools/bundle.yml      fd and ripgrep, named per package manager in dot.yml
      api-client/bundle.yml     a 0600 config file holding a secret
      macos/bundle.yml          macOS preferences, skipped elsewhere (`when: { os: macos }`)

## What it shows

- **Profiles** (`dot.yml`): `macos` matches any Mac, `ubuntu` any Ubuntu machine. `dot plan`
  says which one matched and why; `--profile` or `DOT_PROFILE` picks one by hand.
- **Bundles and ordering**: `common` is `implicit: true`, so every run includes it; `git`
  `requires: [common]` (pulled in and run first), the others say `after: [common]` (ordering
  only). `dot graph` and `dot why <bundle>` show the result.
- **Conditions**: `when:` on a step (`block` into `~/.bashrc` on Linux, `~/.zshrc` on macOS) or
  on a whole bundle (`macos`); `unless:` on a `shell` step (Homebrew is installed only when
  `brew` is not there yet).
- **`PATH` for later steps and for your shell**: `path:` in `common` puts `~/.local/bin` (and
  Homebrew's `bin` on macOS) on `PATH` for the bundles that come after it, in the same run, and
  records it; the `shell` bundle's block sources `dot env --shell zsh --cache`, which is how the
  recorded `PATH` reaches new terminals.
- **Package names per manager**: `package: [fd, ripgrep]` installs `fd-find` with apt and `fd`
  with Homebrew, through `packages:` in `dot.yml`.
- **Variables**: `git_name` and `git_email` are declared in the `git` bundle, `required` with a
  `prompt`: the first `dot apply` that includes that bundle asks for them once and keeps the
  answers in `~/.config/dot/local.env` (`persist: local`). Other bundles run without them
  (`dot apply shell`). `editor` is a plain value in `dot.yml`. `dot vars` shows every value and
  where it came from.
- **Secrets**: `bundles/api-client/config.env.j2` uses `{{ secret("example_api_token") }}`;
  `secrets.yml` maps that name to `EXAMPLE_API_TOKEN` in `~/.config/dot/secrets.env`. To provide
  it:

      mkdir -p ~/.config/dot
      (umask 077; echo 'EXAMPLE_API_TOKEN=...' >> ~/.config/dot/secrets.env)
      dot secrets check

  The secret is `optional: true`, so a machine without it skips that one step instead of
  failing, and `dot secrets check` and `dot doctor` only note that it is not set. The rendered file is written `0600`, and the value never appears in `dot`'s output
  or logs. Switching to another provider (LastPass) is an edit to `secrets.yml`, not to the
  bundle.

## Editor support

Each YAML file starts with a `# yaml-language-server: $schema=...` line pointing at the JSON
Schema published with every release, so editors with the YAML language server complete and
check keys as you type. Every key is listed in [docs/reference.md](../../docs/reference.md).
