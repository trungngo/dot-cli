# Contributing to dot

Thanks for helping. Bug reports, fixes, new directives and new platforms are all welcome. For
anything large (a new directive, a new provider or platform, a change to the config format),
please open an issue first so the design can be agreed before you write the code.

Security problems: please don't open a public issue. See [SECURITY.md](SECURITY.md).

## Building

You need a recent stable Rust (the minimum is `rust-version` in `Cargo.toml`, checked in CI).

    cargo build                     # target/debug/dot
    cargo run -- plan --repo examples/basic

## The gates

Every pull request must pass these, and CI runs them on Linux, macOS and Windows:

    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

Also run these when you touch the files involved:

- Windows code (`cfg(windows)`): `cargo clippy --workspace --all-targets --target
  x86_64-pc-windows-msvc -- -D warnings` (install the target first with `rustup target add
  x86_64-pc-windows-msvc`; clippy doesn't need the Windows SDK).
- `install.sh` and `scripts/*.sh`: `sh -n`, `dash -n` and `shellcheck` on each. They are POSIX
  sh, not bash.
- A Linux binary to try on a server (`dot apply --host user@server --dot-binary <path>`):
  `scripts/build-linux.sh [x86_64|aarch64|all]` cross-compiles static musl builds with
  cargo-zigbuild and prints their paths (it says what to install first when zig,
  cargo-zigbuild or the Rust targets are missing).
- `install.ps1`: `cargo test -p dot-cli --test install_ps1` runs it in every PowerShell on the
  machine (`pwsh` on Linux and macOS; Windows PowerShell 5.1 and PowerShell 7 on Windows). It
  is skipped when there is none, so let CI run it if you have no `pwsh`.
- A config struct or its doc comments: regenerate the reference with
  `DOT_BLESS=1 cargo test -p dot-cli --test reference` and commit `docs/reference.md`.

`CARGO_INCREMENTAL=0` keeps `target/` much smaller when disk space is tight.

## Test rules

Tests must never touch the real home directory, use the network, or run real package managers,
`sudo`, `systemctl` or `defaults`. The existing tests show how to follow these rules:

- Temp directories for `HOME`, `XDG_*` and `DOT_STATE_DIR`, and fake tools (`curl`, `git`,
  `apt-get`, `brew`, `gh`, ...) on a temp `PATH`.
- `DOT_FACTS=os=linux,distro=ubuntu,...` to plan or apply for another platform, and
  `DOT_SYSROOT=<dir>` to put root-owned files under a temp directory.
- `DOT_GITHUB_API`, `DOT_GITHUB_URL` and `DOT_URL_REWRITE` to send HTTP to a local stand-in.
- Tokens in fixtures must be obviously fake (`ghp_FAKE...`, `example-token-000`).

Integration tests live in `crates/dot-cli/tests/` and run the built `dot` binary. CI also runs
them in an `ubuntu:24.04` container as an ordinary user.

## Layout

    crates/
      dot-core/        config model and parsing (serde + JSON Schema), facts, variables, the
                       bundle graph, the plan/apply engine, events, the state store
      dot-directives/  one module per directive: plan (pure) and apply (side effects)
      dot-platform/    package managers, service managers, link strategies, sudo, HTTP
      dot-secrets/     the SecretProvider trait and the envfile and lastpass providers
                       (1password: not implemented yet)
      dot-cli/         the `dot` binary: commands, renderers (tty, plain, json, log file),
                       remote apply, self-update, cloud-init
    docs/design.md     how everything works, and why
    docs/reference.md  every config key (generated)
    examples/basic/    the quickstart repository (tested)

## Adding a directive

1. **Config**: add the key to `StepFields` (`crates/dot-core/src/config/raw.rs`) and its options
   struct (next to similar ones in `config/filesys.rs`, `network.rs` or `system.rs`), with
   `#[serde(deny_unknown_fields)]` and doc comments on every field. The doc comments become the
   JSON Schema and `docs/reference.md`. Validate values while parsing, so that errors carry the
   file and line.
2. **Plan**: a module in `crates/dot-directives/src/` that turns the config into `Action`s
   without changing anything. It reports `pending`, `satisfied`, `unknown` or `conflict`, what
   it needs (sudo, the terminal, secrets), and the `resources()` it manages together with their
   `Undo`, so that `dot status`, `dot diff` and `dot remove` work.
3. **Apply**: the side effects, through the injected runner, file system helpers and HTTP
   client, never directly, so that tests can fake them.
4. **Tests**: plan and apply against fakes, including the second run being a no-op (converged),
   plus an end-to-end test in `crates/dot-cli/tests/` if the directive touches the CLI.
5. **Docs**: regenerate `docs/reference.md` (`DOT_BLESS=1 cargo test -p dot-cli --test
   reference`), describe the behaviour in the directive's section of `docs/design.md`, and add
   a line under "Unreleased" in `CHANGELOG.md`.

## Commits and pull requests

- Keep each pull request to one change, with tests. Describe what changed and why.
- Update `CHANGELOG.md` for anything a user would notice.
- By contributing you agree that your contribution is licensed under the MIT license.

## The website

`site/index.html` is one self-contained page. The Pages workflow publishes it to
https://trungngo.github.io/dot-cli/ when it changes on main. Once per repository, set
Settings > Pages > Source to "GitHub Actions".

## Releasing

CI runs on every pull request and on pushes to main. To release:

1. Bump `version` in the workspace `Cargo.toml`, run `cargo build` so `Cargo.lock` follows, and
   give the `CHANGELOG.md` entry its date.
2. Merge that to main once CI is green.
3. Tag the commit and push the tag: `git tag v0.2.0 && git push origin v0.2.0`. The tag must
   match the version (`v0.2.0` for 0.2.0).

The Release workflow then builds the binaries for every platform, attests them, and publishes
the GitHub release with its checksums and JSON Schemas. It refuses to publish when the tag and
the built version differ.
