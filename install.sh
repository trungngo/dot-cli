#!/bin/sh
# Installs `dot` from the latest GitHub release, then (optionally) clones a dotfiles repository
# and applies it.
#
#   curl -fsSL https://raw.githubusercontent.com/trungngo/dot-cli/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/trungngo/dot-cli/main/install.sh \
#     | sh -s -- https://github.com/you/dotfiles.git
#
# To pin a release, take the script from that release's tag and name the same tag in
# DOT_VERSION, so the script and the binary it installs come from one release:
#
#   curl -fsSL https://raw.githubusercontent.com/trungngo/dot-cli/v0.1.0/install.sh \
#     | DOT_VERSION=v0.1.0 sh
#
# dot's releases are public: installing dot needs no token. A GitHub token is only used when
# something private is asked for:
#
# - a private dotfiles repository on github.com: the clone (or pull) is tried without the
#   token first (git's own credentials, no prompt), and only when that fails is it retried with
#   the token;
# - releases from a private fork (DOT_RELEASE_REPO=owner/name): when the public download URL
#   fails, the release is fetched through the GitHub API with the token.
#
# For a private dotfiles repository, without putting the token on any command line or in shell
# history (bash, zsh):
#
#   read -rs GITHUB_TOKEN     # paste the token, then Enter; nothing is echoed
#   curl -fsSL https://raw.githubusercontent.com/trungngo/dot-cli/main/install.sh \
#     | GITHUB_TOKEN=$GITHUB_TOKEN sh -s -- https://github.com/you/dotfiles.git
#   unset GITHUB_TOKEN
#
# 1. Detects the OS and CPU and picks the release asset: dot-<target>.tar.gz, where <target> is
#    x86_64-unknown-linux-musl, aarch64-unknown-linux-musl or universal-apple-darwin.
# 2. Downloads it and the release's SHA256SUMS from
#    https://github.com/<DOT_RELEASE_REPO>/releases/latest/download/ (or
#    .../releases/download/<DOT_VERSION>/) without any credentials,
#    and refuses to install unless the checksum matches. When that fails and there is a token
#    (see "The token" below), through the GitHub API instead (the latest release, then each
#    asset by id); the API answers an asset with a redirect to a storage host, which is fetched
#    without the token. The checksum comes from the same release (and host) as the binary, so it
#    only catches a corrupted or truncated download: it does not prove who built the binary.
#    Trust rests on TLS to github.com (downloads are https-only unless DOT_RELEASE_BASE,
#    DOT_GITHUB_URL or DOT_GITHUB_API say otherwise, which prints a warning).
# 3. Installs the binary as ~/.local/bin/dot.
# 4. With a repository URL (https:// with no credentials in it, or ssh: ssh://... or
#    git@host:owner/name.git, which use your ssh keys and never the token): clones it to
#    ~/.dot (or pulls it, fast-forward only, when it is already there), records it as
#    DOT_REPO in ~/.config/dot/local.env (unless another DOT_REPO is recorded there: that one
#    is kept, with a warning), and runs `dot apply`. For a github.com https URL that git cannot read
#    with its own credentials, the token goes to git as an `http.https://github.com/.extraheader`
#    in git's environment (GIT_CONFIG_COUNT, git 2.31 or later): nothing is written to any git
#    config, so the checkout keeps no credentials and later pulls use git's own (a credential
#    helper, e.g. `gh auth setup-git`).
#    Piped into sh, the script's stdin is the download, not the keyboard: `dot apply` gets the
#    terminal back (/dev/tty) when there is one, so it can confirm the plan, ask for the sudo
#    password once and prompt for variables. Without a terminal it runs non-interactively
#    (pass --yes in DOT_APPLY_ARGS, and sudo must not need a password).
#
# The token: GITHUB_TOKEN, else GH_TOKEN, else `gh auth token` when the GitHub CLI is installed
# and logged in; looked up only when it is needed. A fine-grained personal access token with
# read-only "Contents" on the private repositories is enough. It never goes on a command line
# (ps), into a URL, a log line or a file other than a 0600 header file in a private temp
# directory that is deleted on exit; curl reads its header from that file (-H @file) and only
# requests to the API carry it. curl ignores ~/.curlrc (-q), so no option there (following
# redirects with the header, verbose output) applies. Tracing (sh -x) is turned off. `dot apply`
# runs without it: GITHUB_TOKEN and GH_TOKEN are unset first.
#
# Environment:
#   GITHUB_TOKEN, GH_TOKEN  a GitHub token, for private repositories only (see above)
#   DOT_VERSION       the release to install, e.g. v0.1.0 (default: the latest release)
#   DOT_INSTALL_DIR   where the binary goes (default ~/.local/bin)
#   DOT_DOTFILES_DIR  where the repository is cloned (default ~/.dot)
#   DOT_RELEASE_REPO  the GitHub repository dot is released from (default trungngo/dot-cli;
#                     a fork's owner/name)
#   DOT_RELEASE_BASE  where the release assets are, fetched without a token (default the
#                     GitHub release); for mirrors and tests
#   DOT_GITHUB_URL    the GitHub web host (default https://github.com)
#   DOT_GITHUB_API    the GitHub API (default https://api.github.com)
#   DOT_APPLY_ARGS    extra arguments for `dot apply` (e.g. --yes)
#   XDG_CONFIG_HOME   where local.env goes: $XDG_CONFIG_HOME/dot/local.env (default ~/.config)
#   TMPDIR            where the private temp directory is made (default /tmp)
#
# POSIX sh; needs curl (or wget), tar, and sha256sum, shasum or openssl. Nothing runs as root.

set -eu

# Tracing (sh -x) would print the token and the header made from it: turned off before either is
# read. (Checking for a token first would itself be traced.)
case $- in
*x*)
	set +x
	printf 'install.sh: note: tracing (sh -x) turned off: it would print the GitHub token\n' >&2
	;;
esac

[ -n "${HOME:-}" ] || {
	echo "install.sh: error: HOME is not set" >&2
	exit 1
}

DEFAULT_REPO="trungngo/dot-cli"
REPO="${DOT_RELEASE_REPO:-$DEFAULT_REPO}"
VERSION="${DOT_VERSION:-}"
WEB="${DOT_GITHUB_URL:-https://github.com}"
WEB="${WEB%/}"
API="${DOT_GITHUB_API:-https://api.github.com}"
API="${API%/}"
BIN_DIR="${DOT_INSTALL_DIR:-$HOME/.local/bin}"
DOTFILES_DIR="${DOT_DOTFILES_DIR:-$HOME/.dot}"
CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
if [ -n "$VERSION" ]; then
	PUBLIC_BASE="$WEB/$REPO/releases/download/$VERSION"
	API_RELEASE="releases/tags/$VERSION"
	RAW_URL="https://raw.githubusercontent.com/$REPO/$VERSION/install.sh"
else
	PUBLIC_BASE="$WEB/$REPO/releases/latest/download"
	API_RELEASE="releases/latest"
	RAW_URL="https://raw.githubusercontent.com/$REPO/main/install.sh"
fi

# Set by find_token: the token, and where it came from (for messages; never the value).
TOKEN=""
TOKEN_FROM=""
TOKEN_LOOKED=""
# The private temp directory (made by make_tmp, deleted on exit).
TMP=""

say() { printf 'install.sh: %s\n' "$*" >&2; }
die() {
	say "error: $*"
	exit 1
}
has() { command -v "$1" >/dev/null 2>&1; }

usage() {
	cat >&2 <<'EOF2'
usage: install.sh [https://host/owner/dotfiles.git | git@host:owner/dotfiles.git]
Installs dot to ~/.local/bin; with a repository URL, clones it and runs `dot apply`.
DOT_VERSION=vX.Y.Z installs that release instead of the latest.
A GitHub token (GITHUB_TOKEN, GH_TOKEN or `gh auth token`) is used only for private
repositories: a private dotfiles repository on github.com, or DOT_RELEASE_REPO naming a
private fork.
EOF2
}

# The release repository must be owner/name.
check_repo() {
	case "$REPO" in
	*/*/* | /* | */ | . | ./* | ../* | */. | */.. | *[!A-Za-z0-9_./-]*)
		die "DOT_RELEASE_REPO must be owner/name (got: $REPO)"
		;;
	?*/?*) ;;
	*) die "DOT_RELEASE_REPO must be owner/name (got: $REPO)" ;;
	esac
	case "$VERSION" in
	'') ;;
	v[0-9]*[!A-Za-z0-9._+-]* | v[0-9]*/*) die "DOT_VERSION must be a release tag such as v0.1.0 (got: $VERSION)" ;;
	v[0-9]*) ;;
	*) die "DOT_VERSION must be a release tag such as v0.1.0 (got: $VERSION)" ;;
	esac
}

# How to give this script a token, for when something private failed without one. `what` names
# the private repositories.
token_help() { # what dotfiles-url
	repo_arg=${2:-https://github.com/you/dotfiles.git}
	pin=""
	[ -z "$VERSION" ] || pin="DOT_VERSION=$VERSION "
	if [ "$REPO" = "$DEFAULT_REPO" ]; then
		fetch="curl -fsSL $RAW_URL \\
       | ${pin}GITHUB_TOKEN=\$GITHUB_TOKEN sh -s -- $repo_arg"
	else
		# A private fork's raw file needs the token too; printf is a shell built-in, so the
		# header reaches curl on its stdin, never its argv.
		fetch="printf 'Authorization: Bearer %s\\n' \"\$GITHUB_TOKEN\" \\
       | curl -fsSL -H @- $RAW_URL \\
       | ${pin}DOT_RELEASE_REPO=$REPO GITHUB_TOKEN=\$GITHUB_TOKEN sh -s -- $repo_arg"
	fi
	cat >&2 <<EOF2

If $1 is private, this script needs a GitHub token that can read it.

1. Create a fine-grained personal access token:
     https://github.com/settings/personal-access-tokens/new
   Repository access: "Only select repositories": the private repositories this needs.
   Repository permissions: Contents: Read-only (Metadata: Read-only is added by itself).
   Pick a short expiry.

2. Run, in bash or zsh (the token is read without echo: it stays out of shell history and
   off every command line):

     read -rs GITHUB_TOKEN
     $fetch
     unset GITHUB_TOKEN

Or install the GitHub CLI and run \`gh auth login\`: this script then uses \`gh auth token\`.
EOF2
}

# The GitHub token, from the environment or the GitHub CLI; empty when there is none. Looked up
# once, when first needed.
find_token() {
	[ -z "$TOKEN_LOOKED" ] || return 0
	TOKEN_LOOKED=1
	if [ -n "${GITHUB_TOKEN:-}" ]; then
		TOKEN=$GITHUB_TOKEN TOKEN_FROM=GITHUB_TOKEN
	elif [ -n "${GH_TOKEN:-}" ]; then
		TOKEN=$GH_TOKEN TOKEN_FROM=GH_TOKEN
	elif has gh && TOKEN=$(gh auth token --hostname github.com 2>/dev/null); then
		TOKEN_FROM='gh auth token'
	else
		TOKEN=""
	fi
	# GitHub tokens are letters, digits and underscores (dots and dashes are let through too);
	# anything else (a space, a quote, a line break) would break the header it goes into.
	case "$TOKEN" in
	*[!A-Za-z0-9_.-]*) die "the GitHub token from $TOKEN_FROM holds whitespace or other unexpected characters" ;;
	esac
	[ -n "$TOKEN" ] || TOKEN_FROM=""
}
make_tmp() {
	# Private to this user (mktemp -d makes it 0700).
	TMP=$(mktemp -d "${TMPDIR:-/tmp}/dot-install.XXXXXX") || die "cannot make a temp directory"
	trap 'rm -rf "$TMP"' EXIT
	trap 'rm -rf "$TMP"; exit 130' INT HUP TERM
}

cleanup() {
	rm -rf "$TMP"
	trap - EXIT INT HUP TERM
}

# Writes the API's Authorization header to a 0600 file for `curl -H @file` (printf is a
# built-in: the value is never an argument of another program).
write_auth_header() {
	(
		umask 077
		printf 'Authorization: Bearer %s\n' "$TOKEN" >"$TMP/auth"
	)
}

target() {
	os=$(uname -s)
	arch=$(uname -m)
	case "$arch" in
	x86_64 | amd64) arch=x86_64 ;;
	arm64 | aarch64) arch=aarch64 ;;
	*) die "unsupported CPU: $arch" ;;
	esac
	case "$os" in
	Linux) echo "$arch-unknown-linux-musl" ;;
	Darwin) echo "universal-apple-darwin" ;;
	*) die "unsupported OS: $os (on Windows, use install.ps1)" ;;
	esac
}

proto_for() { # url
	case "$1" in
	https://*) echo '=https' ;;
	*) echo 'all' ;; # an explicit DOT_RELEASE_BASE / DOT_GITHUB_API (a mirror, a test fixture)
	esac
}

# Downloads a URL without any credentials; fails (returns 1) when it cannot.
download() { # url file
	proto=$(proto_for "$1")
	if has curl; then
		# -q (first): ~/.curlrc is not read.
		curl -q -fsSL --proto "$proto" --retry 3 -o "$2" "$1" || return 1
	elif has wget; then
		# wget follows a redirect from https to http unless told not to; a wget without
		# --https-only (busybox) cannot promise https, so it is not used for an https URL.
		if [ "$proto" = '=https' ]; then
			wget --help 2>&1 | grep -q -- '--https-only' ||
				die "this wget cannot refuse a redirect to plain http (no --https-only); install curl"
			wget -q --https-only -O "$2" "$1" || return 1
		else
			wget -q -O "$2" "$1" || return 1
		fi
	else
		die "neither curl nor wget is installed"
	fi
}

# GET on the API with the token; prints "<status> <redirect url>". The Authorization header is
# read from a file and sent to this URL only: curl is not asked to follow redirects (-L), so a
# redirect (to a storage host) is returned to the caller, which fetches it without the header.
# -q (first): ~/.curlrc is not read, so nothing there (`location-trusted`, `verbose`) can send
# the header elsewhere or print it.
api_get() { # url file accept
	curl -q -sS --proto "$(proto_for "$1")" --retry 3 \
		-H @"$TMP/auth" -H "Accept: $3" -H 'X-GitHub-Api-Version: 2022-11-28' \
		-o "$2" -w '%{http_code} %{redirect_url}' "$1"
}

api_status_error() { # status what
	case "$1" in
	401) die "GitHub rejected the token from $TOKEN_FROM (401): it is wrong or expired" ;;
	403 | 404) die "$2: HTTP $1: the token from $TOKEN_FROM cannot read $REPO (a fine-grained token needs read-only \"Contents\" on it)" ;;
	*) die "$2: HTTP $1" ;;
	esac
}

# The id of asset `name` in a release (JSON, as the API returns it). Every asset has
# "url": ".../releases/assets/<id>" before its "browser_download_url": ".../<name>". Splitting
# at commas is enough: strings holding quotes (a release body) have them escaped.
asset_id() { # release.json name
	tr ',' '\n' <"$1" | awk -v want="$2" '
		/"url": *"[^"]*\/releases\/assets\/[0-9]+"/ {
			id = $0; sub(/.*\/releases\/assets\//, "", id); sub(/".*/, "", id)
		}
		/"browser_download_url": *"/ {
			n = $0; sub(/.*"browser_download_url": *"/, "", n); sub(/".*/, "", n)
			sub(/.*\//, "", n)
			if (n == want && id != "") { print id; exit }
			id = ""
		}'
}

# Downloads release asset `id` through the API.
api_asset() { # id file name
	url="$API/repos/$REPO/releases/assets/$1"
	res=$(api_get "$url" "$2" application/octet-stream) || die "cannot reach the GitHub API ($url)"
	code=${res%% *}
	location=${res#* }
	case "$code" in
	200) ;;
	301 | 302 | 303 | 307 | 308)
		[ -n "$location" ] || die "downloading $3: a redirect without a location"
		case "$API" in
		https://*)
			case "$location" in
			https://*) ;;
			*) die "downloading $3: refusing the redirect away from https" ;;
			esac
			;;
		esac
		rm -f "$2"
		# The storage host gets no token: the signed URL is the permission.
		download "$location" "$2" || die "cannot download $3 from GitHub's storage"
		;;
	*) api_status_error "$code" "downloading $3" ;;
	esac
}

sha256() {
	if has sha256sum; then
		sha256sum "$1" | cut -d' ' -f1
	elif has shasum; then
		shasum -a 256 "$1" | cut -d' ' -f1
	elif has openssl; then
		openssl dgst -sha256 "$1" | sed 's/.*= *//'
	else
		die "no sha256sum, shasum or openssl to verify the download with"
	fi
}

# The release through the GitHub API, with the token (a private fork's releases).
fetch_release_api() { # asset
	has curl || die "downloading with a token needs curl (wget would put the token on its command line)"
	case "$API" in
	https://*) ;;
	*) say "warning: DOT_GITHUB_API is not https ($API): the token and the download are not protected in transit" ;;
	esac
	say "using the GitHub token from $TOKEN_FROM for the release of $REPO"
	write_auth_header
	res=$(api_get "$API/repos/$REPO/$API_RELEASE" "$TMP/release.json" application/vnd.github+json) ||
		die "cannot reach the GitHub API ($API)"
	code=${res%% *}
	if [ "$code" = 404 ]; then
		# The token can read the repository, but it has no release (yet)? Otherwise 404 is
		# what GitHub answers a token that cannot read it.
		res=$(api_get "$API/repos/$REPO" "$TMP/repo.json" application/vnd.github+json) ||
			die "cannot reach the GitHub API ($API)"
		if [ "${res%% *}" = 200 ]; then
			[ -z "$VERSION" ] ||
				die "$REPO has no release $VERSION (the token from $TOKEN_FROM can read the repository)"
			die "$REPO has no published release yet (the token from $TOKEN_FROM can read the repository); publish one, or set DOT_RELEASE_BASE to a mirror"
		fi
	fi
	[ "$code" = 200 ] || api_status_error "$code" "looking up release ${VERSION:-latest} of $REPO"
	tag=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$TMP/release.json" | head -n 1)
	id=$(asset_id "$TMP/release.json" "$1")
	[ -n "$id" ] || die "release ${tag:-latest} of $REPO has no $1"
	sums=$(asset_id "$TMP/release.json" SHA256SUMS)
	[ -n "$sums" ] || die "release ${tag:-latest} of $REPO has no SHA256SUMS: refusing to install an unverified binary"
	say "release ${tag:-latest}"
	api_asset "$id" "$TMP/$1" "$1"
	api_asset "$sums" "$TMP/SHA256SUMS" SHA256SUMS
}

# Fetches `asset` and SHA256SUMS into $TMP: from DOT_RELEASE_BASE when set; else from the
# public download URL, without any credentials; else, when that fails and there is a token,
# through the API.
fetch_release() { # asset
	if [ -n "${DOT_RELEASE_BASE:-}" ]; then
		base=$DOT_RELEASE_BASE
		case "$base" in
		https://*) ;;
		*) say "warning: DOT_RELEASE_BASE is not https ($base): the download and its checksum are not protected in transit" ;;
		esac
		download "$base/$1" "$TMP/$1" || die "cannot download $base/$1"
		download "$base/SHA256SUMS" "$TMP/SHA256SUMS" || die "cannot download $base/SHA256SUMS"
		return
	fi
	case "$WEB" in
	https://*) ;;
	*) say "warning: DOT_GITHUB_URL is not https ($WEB): the download and its checksum are not protected in transit" ;;
	esac
	if download "$PUBLIC_BASE/$1" "$TMP/$1"; then
		download "$PUBLIC_BASE/SHA256SUMS" "$TMP/SHA256SUMS" ||
			die "cannot download $PUBLIC_BASE/SHA256SUMS"
		return
	fi
	find_token
	if [ -z "$TOKEN" ]; then
		say "error: cannot download $PUBLIC_BASE/$1"
		if [ -n "$VERSION" ]; then
			say "(no release $VERSION, no network, or $REPO is private)"
		else
			say "(no published release yet, no network, or $REPO is private)"
		fi
		token_help "$REPO" "${REPO_URL:-}"
		exit 1
	fi
	say "cannot download $PUBLIC_BASE/$1; trying the GitHub API"
	fetch_release_api "$1"
}

install_dot() {
	asset="dot-$(target).tar.gz"
	say "downloading $asset"
	fetch_release "$asset"
	# Lines are "<sha256>  <file>" (or "<sha256> *<file>").
	want=$(awk -v a="$asset" '$2 == a || $2 == "*" a { print $1; exit }' "$TMP/SHA256SUMS")
	[ -n "$want" ] || die "SHA256SUMS lists no checksum for $asset"
	have=$(sha256 "$TMP/$asset")
	[ "$have" = "$want" ] || die "checksum mismatch for $asset (expected $want, got $have)"
	mkdir -p "$TMP/x"
	tar -xzf "$TMP/$asset" -C "$TMP/x"
	bin=$(find "$TMP/x" -type f -name dot | head -n 1)
	[ -n "$bin" ] || die "$asset holds no dot binary"
	mkdir -p "$BIN_DIR"
	# Copied next to the destination, then renamed: a running dot is never half-overwritten.
	cp "$bin" "$BIN_DIR/.dot.tmp.$$"
	chmod 0755 "$BIN_DIR/.dot.tmp.$$"
	mv -f "$BIN_DIR/.dot.tmp.$$" "$BIN_DIR/dot"
	say "installed $("$BIN_DIR/dot" --version 2>/dev/null || echo dot) to $BIN_DIR/dot"
	case ":${PATH:-}:" in
	*":$BIN_DIR:"*)
		# Another `dot` (Graphviz's, say) earlier on PATH would be the one a shell runs.
		found=$(command -v dot 2>/dev/null || true)
		if [ -n "$found" ] && [ "$found" != "$BIN_DIR/dot" ]; then
			say "note: $found comes before $BIN_DIR/dot on PATH (Graphviz's dot?); run $BIN_DIR/dot, or put $BIN_DIR first"
		fi
		;;
	*)
		say "note: $BIN_DIR is not on PATH. Add this line to your shell's startup file (~/.zshrc, ~/.bashrc or ~/.profile), then open a new terminal:"
		# shellcheck disable=SC2016 # $PATH is for the startup file, unexpanded
		printf '    export PATH="%s:$PATH"\n' "$BIN_DIR" >&2
		;;
	esac
}

# https:// (no credentials in it), ssh://, or scp-like user@host:path. An ssh URL uses your ssh
# keys (and ssh's own prompts, on the terminal); the token is never used for it.
check_url() { # url
	case "$1" in
	-* | *://-* | *@-*) die "the repository URL must not start with a dash (got: $1)" ;;
	https://*@*) die "put no credentials in the URL; set GITHUB_TOKEN or use a git credential helper" ;;
	https://?*) ;;
	ssh://*:*@*) die "put no password in the URL; ssh uses your keys" ;;
	ssh://?*) ;;
	*://*) die "the repository URL must be https://, ssh:// or user@host:path (got: $1)" ;;
	?*@?*:?*) ;;
	*) die "the repository URL must be https://, ssh:// or user@host:path (got: $1)" ;;
	esac
}

# Whether git can read `url` with its own credentials (a credential helper), without prompting.
git_can_read() { # url
	GIT_TERMINAL_PROMPT=0 GCM_INTERACTIVE=never git ls-remote -q "$1" HEAD >/dev/null 2>&1
}

# How git gets at `url`: TOKEN_FOR_GIT=1 when it needs the token. The token is for github.com
# only, and only when git cannot read the repository without it (a public repository, or a
# credential helper that can, needs none).
decide_git_auth() { # url
	TOKEN_FOR_GIT=""
	case "$1" in
	https://github.com/*) ;;
	*) return 0 ;;
	esac
	# Public, or git's own credentials can read it: no token (not even looked up).
	git_can_read "$1" && return 0
	# No token at hand: git as usual (its credential helper, or a prompt).
	find_token
	[ -n "$TOKEN" ] || return 0
	version=$(git version 2>/dev/null | sed -n 's/^git version \([0-9]*\)\.\([0-9]*\).*/\1 \2/p')
	major=${version%% *}
	minor=${version#* }
	if [ -z "$version" ] || [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 31 ]; }; then
		die "git ${major:-?}.${minor:-?} cannot take the token from its environment (needs 2.31 or later); upgrade git, or set up a git credential helper and unset GITHUB_TOKEN"
	fi
	say "$1 needs credentials: using the GitHub token from $TOKEN_FROM"
	TOKEN_FOR_GIT=1
}

# Runs git; with TOKEN_FOR_GIT, the token goes to git in its environment as an extra header for
# https://github.com/ (never on argv, never into a config file).
git_auth() { # git-args...
	if [ -z "$TOKEN_FOR_GIT" ]; then
		git "$@"
		return
	fi
	b64=$(printf 'x-access-token:%s' "$TOKEN" | base64_line)
	GIT_CONFIG_COUNT=1 \
		GIT_CONFIG_KEY_0='http.https://github.com/.extraheader' \
		GIT_CONFIG_VALUE_0="Authorization: Basic $b64" \
		GIT_TERMINAL_PROMPT=0 \
		git "$@"
}

base64_line() {
	if has base64; then
		base64 | tr -d '\n'
	elif has openssl; then
		openssl base64 -A
	else
		die "no base64 or openssl to hand the token to git with"
	fi
}

clone_repo() { # url
	has git || die "git is needed to clone $1; install it first"
	if [ -d "$DOTFILES_DIR/.git" ]; then
		origin=$(git -C "$DOTFILES_DIR" remote get-url origin 2>/dev/null || true)
		[ "$origin" = "$1" ] ||
			die "$DOTFILES_DIR is a checkout of ${origin:-no remote}, not $1; set DOT_DOTFILES_DIR"
		decide_git_auth "$1"
		say "updating $DOTFILES_DIR"
		git_auth -C "$DOTFILES_DIR" pull --ff-only ||
			die "git pull failed in $DOTFILES_DIR"
	elif [ -e "$DOTFILES_DIR" ]; then
		die "$DOTFILES_DIR exists and is not a git checkout; set DOT_DOTFILES_DIR"
	else
		decide_git_auth "$1"
		say "cloning $1 to $DOTFILES_DIR"
		if ! git_auth clone -- "$1" "$DOTFILES_DIR"; then
			case "$1" in
			https://github.com/*) [ -n "$TOKEN" ] || token_help "$1" "$1" ;;
			esac
			die "git clone $1 failed"
		fi
	fi
	local_env="$CONFIG_HOME/dot/local.env"
	recorded=$(sed -n 's/^DOT_REPO=//p' "$local_env" 2>/dev/null | tail -n 1)
	if [ -z "$recorded" ]; then
		mkdir -p "$CONFIG_HOME/dot"
		printf "DOT_REPO='%s'\n" "$DOTFILES_DIR" >>"$local_env"
		chmod 600 "$local_env"
		say "recorded DOT_REPO=$DOTFILES_DIR in $local_env"
		return
	fi
	# Unquoted, with ~ expanded, as dot reads it.
	recorded=${recorded#[\"\']}
	recorded=${recorded%[\"\']}
	case "$recorded" in
	"~"/*) recorded="$HOME/${recorded#"~"/}" ;;
	esac
	if [ "${recorded%/}" != "${DOTFILES_DIR%/}" ]; then
		say "warning: $local_env already has DOT_REPO=$recorded, left as it is: this run applies $DOTFILES_DIR, but later dot commands outside a repository use $recorded; edit DOT_REPO there to switch"
	fi
}

# Whether the controlling terminal can be opened (not in CI, cron or a container without one).
# `true` is not a special built-in, so a failed redirection does not end the script.
tty_readable() {
	true 2>/dev/null </dev/tty
}

main() {
	case "${1:-}" in
	-h | --help)
		usage
		exit 0
		;;
	esac
	[ "$#" -le 1 ] || {
		usage
		exit 2
	}
	REPO_URL=${1:-}
	# Checked before anything is downloaded.
	check_repo
	[ "$#" -eq 0 ] || check_url "$1"
	make_tmp
	install_dot
	if [ "$#" -eq 1 ]; then
		clone_repo "$1"
	fi
	cleanup
	if [ "$#" -eq 1 ]; then
		# The token was for this script: `dot apply` (and everything it runs, such as gh, which
		# would prefer it to its own login) does not get it.
		unset GITHUB_TOKEN GH_TOKEN
		say "running dot apply"
		# Under `curl ... | sh`, stdin is the pipe the script came from: dot would see no
		# terminal and run non-interactively (no confirmation, no sudo prompt, no variable
		# prompts). Someone is watching when stdout is a terminal; give dot that terminal.
		if [ ! -t 0 ] && [ -t 1 ] && tty_readable; then
			# shellcheck disable=SC2086 # DOT_APPLY_ARGS is a list of arguments on purpose
			exec "$BIN_DIR/dot" apply --repo "$DOTFILES_DIR" ${DOT_APPLY_ARGS:-} </dev/tty
		fi
		# shellcheck disable=SC2086
		exec "$BIN_DIR/dot" apply --repo "$DOTFILES_DIR" ${DOT_APPLY_ARGS:-}
	fi
}

main "$@"
