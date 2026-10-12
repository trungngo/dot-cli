#!/bin/sh
# Builds `dot` for Linux servers on this machine (a Mac, or any other host): a static musl
# binary per CPU, cross-compiled with cargo-zigbuild (zig as the C toolchain and linker). Hand
# one to `dot apply --host` (or `dot plan --host`, `dot secrets push`) with --dot-binary, and dot
# is installed (or upgraded) on the server from it over scp, with nothing downloaded:
#
#   scripts/build-linux.sh                 # x86_64 (the default)
#   scripts/build-linux.sh aarch64         # or: x86_64 aarch64, or: all
#   dot apply --host me@server --dot-binary target/x86_64-unknown-linux-musl/release/dot
#
# DOT_HOST_BINARY=<path> in the environment does the same as --dot-binary <path>. The binary must
# be the same version as the `dot` that installs it: build both from the same checkout.
#
# One-time setup (the script says which of these is missing):
#
#   brew install zig                       # or https://ziglang.org/download/ (or name one
#                                          # with CARGO_ZIGBUILD_ZIG_PATH)
#   cargo install --locked cargo-zigbuild
#   rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
#
# The binaries go where cargo puts them: <target dir>/<arch>-unknown-linux-musl/release/dot,
# the target dir being CARGO_TARGET_DIR or target/ in the checkout. Their paths are printed, one
# per line, on stdout; progress goes to stderr.

set -eu

say() { printf '%s\n' "build-linux.sh: $*" >&2; }

usage() {
    cat >&2 <<'EOF'
usage: scripts/build-linux.sh [x86_64|aarch64|all]...

Builds dot as a static Linux (musl) binary for each CPU named (default: x86_64) with
cargo zigbuild, and prints the path of each one. Use it with:

  dot apply --host user@server --dot-binary <path>
EOF
}

arches=
for a in "$@"; do
    case "$a" in
        -h | --help)
            usage
            exit 0
            ;;
        x86_64 | amd64) a=x86_64 ;;
        aarch64 | arm64) a=aarch64 ;;
        all) a="x86_64 aarch64" ;;
        *)
            say "unknown CPU \`$a\` (x86_64, aarch64 or all)"
            usage
            exit 2
            ;;
    esac
    for one in $a; do
        case " $arches " in
            *" $one "*) ;;
            *) arches="${arches:+$arches }$one" ;;
        esac
    done
done
[ -n "$arches" ] || arches=x86_64

targets=
for a in $arches; do
    targets="${targets:+$targets }$a-unknown-linux-musl"
done

# In the checkout, so its toolchain (not the caller's directory's) answers what follows.
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

# What is missing, all of it at once, each with the command that sets it up.
missing=
need() {
    missing="${missing}
  $1"
}
if ! command -v cargo >/dev/null 2>&1; then
    need "cargo (Rust): https://rustup.rs"
else
    if ! command -v cargo-zigbuild >/dev/null 2>&1; then
        need "cargo-zigbuild: cargo install --locked cargo-zigbuild"
    fi
    # cargo-zigbuild also takes zig from CARGO_ZIGBUILD_ZIG_PATH (a path, or a command such as
    # `python3 -m ziglang`): trusted as it is.
    if [ -z "${CARGO_ZIGBUILD_ZIG_PATH:-}" ] && ! command -v zig >/dev/null 2>&1; then
        if [ "$(uname -s)" = Darwin ]; then
            need "zig: brew install zig"
        else
            need "zig: your package manager, or https://ziglang.org/download/"
        fi
    fi
    if command -v rustup >/dev/null 2>&1; then
        installed=$(rustup target list --installed 2>/dev/null || true)
        absent=
        for t in $targets; do
            case "
$installed
" in
                *"
$t
"*) ;;
                *) absent="${absent:+$absent }$t" ;;
            esac
        done
        if [ -n "$absent" ]; then
            need "the Rust standard library for $absent: rustup target add $absent"
        fi
    fi
fi
if [ -n "$missing" ]; then
    say "cannot build yet; one-time setup, missing:$missing"
    exit 1
fi

target_dir=${CARGO_TARGET_DIR:-$root/target}
case "$target_dir" in
    /*) ;;
    *) target_dir="$root/$target_dir" ;;
esac

set --
for t in $targets; do
    set -- "$@" --target "$t"
done
say "building dot for $targets"
cargo zigbuild --release --locked -p dot-cli "$@" >&2

for t in $targets; do
    bin="$target_dir/$t/release/dot"
    if [ ! -f "$bin" ]; then
        say "cargo finished, but there is no $bin"
        exit 1
    fi
    printf '%s\n' "$bin"
done
say "done: dot apply --host user@server --dot-binary <path above>"
