## What and why

<!-- What this changes, and the problem it solves. Link the issue if there is one. -->

## Checklist

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` (and `--target x86_64-pc-windows-msvc` for Windows code)
- [ ] `cargo test --workspace`
- [ ] `install.sh` changed: `sh -n`, `dash -n` and `shellcheck` pass
- [ ] Config structs or their doc comments changed: `docs/reference.md` regenerated (`DOT_BLESS=1 cargo test -p dot-cli --test reference`)
- [ ] Tests don't touch the real home directory, the network or real package managers
- [ ] `CHANGELOG.md` updated (under "Unreleased") for anything a user would notice
