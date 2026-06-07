#!/bin/sh
# pyq installer — detect the platform, download the matching release asset from
# GitHub, verify its sha256, install the binary, and record the channel so
# `pyq upgrade` knows which line to follow.
#
#   curl -fsSL https://raw.githubusercontent.com/nickbeaulieu/pyq/main/install.sh | sh
#
# Channel and location are overridable by env or flag:
#   PYQ_CHANNEL=canary      (or pass --canary / --stable)
#   PYQ_INSTALL_DIR=~/bin   where the `pyq` binary lands (default ~/.local/bin)
#   PYQ_CONFIG_DIR=~/.pyq   where the channel marker lands (default ~/.pyq)
#
# Wrapped in a function and called last so a partial download can't execute.
set -eu

REPO="nickbeaulieu/pyq"

main() {
    channel="${PYQ_CHANNEL:-stable}"
    install_dir="${PYQ_INSTALL_DIR:-$HOME/.local/bin}"
    config_dir="${PYQ_CONFIG_DIR:-$HOME/.pyq}"

    for arg in "$@"; do
        case "$arg" in
            --canary) channel="canary" ;;
            --stable) channel="stable" ;;
            --dir=*) install_dir="${arg#--dir=}" ;;
            -h|--help) usage; return 0 ;;
            *) die "unknown argument: $arg (try --help)" ;;
        esac
    done
    case "$channel" in
        stable|canary) ;;
        *) die "PYQ_CHANNEL must be 'stable' or 'canary', got '$channel'" ;;
    esac

    need_cmd uname
    need_cmd tar
    need_cmd mktemp
    downloader=""
    if has curl; then downloader="curl"
    elif has wget; then downloader="wget"
    else die "need curl or wget to download"; fi

    target="$(detect_target)"
    say "channel:   $channel"
    say "target:    $target"

    api="https://api.github.com/repos/$REPO/releases"
    case "$channel" in
        stable) api="$api/latest" ;;
        canary) api="$api/tags/canary" ;;
    esac

    json="$(fetch "$api")" || die "could not reach the GitHub release API"
    tar_url="$(asset_url "$json" "-$target.tar.gz")"
    sha_url="$(asset_url "$json" "-$target.tar.gz.sha256")"
    if [ -z "$tar_url" ]; then
        if [ "$channel" = "stable" ]; then
            die "no stable release with an asset for $target yet — try: PYQ_CHANNEL=canary"
        fi
        die "the $channel release has no asset for $target"
    fi
    [ -n "$sha_url" ] || die "the $channel release is missing the .sha256 for $target"

    tmp="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf \"$tmp\"" EXIT

    say "downloading $(basename "$tar_url")"
    download "$tar_url" "$tmp/pyq.tar.gz"
    download "$sha_url" "$tmp/pyq.tar.gz.sha256"
    verify_sha "$tmp/pyq.tar.gz" "$tmp/pyq.tar.gz.sha256"

    tar -xzf "$tmp/pyq.tar.gz" -C "$tmp"
    [ -f "$tmp/pyq" ] || die "release archive did not contain a 'pyq' binary"

    mkdir -p "$install_dir"
    cp "$tmp/pyq" "$install_dir/pyq.new"
    chmod 0755 "$install_dir/pyq.new"
    mv "$install_dir/pyq.new" "$install_dir/pyq"

    mkdir -p "$config_dir"
    printf '%s\n' "$channel" > "$config_dir/channel"

    say ""
    say "installed pyq to $install_dir/pyq"
    "$install_dir/pyq" --version 2>/dev/null || true
    path_hint "$install_dir"
}

usage() {
    cat <<'EOF'
pyq installer

Usage: install.sh [--stable|--canary] [--dir=PATH]

  --stable        install the latest tagged release (default)
  --canary        install the rolling build of main
  --dir=PATH      install location (default: ~/.local/bin)

Env: PYQ_CHANNEL, PYQ_INSTALL_DIR, PYQ_CONFIG_DIR, GITHUB_TOKEN
EOF
}

# Map `uname` to the release's target triple. Linux x86_64 picks musl vs gnu by
# the running libc; everything else has a single triple.
detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$arch" in
        x86_64|amd64) arch="x86_64" ;;
        arm64|aarch64) arch="aarch64" ;;
        *) die "unsupported architecture: $arch" ;;
    esac
    case "$os" in
        Darwin)
            # Only Apple Silicon is published; there's no prebuilt Intel-mac
            # binary, and an arm64 build won't run on Intel (Rosetta is the other
            # direction). Point Intel users at a source build rather than letting
            # them hit a bare "no asset for x86_64-apple-darwin".
            if [ "$arch" = "x86_64" ]; then
                die "no prebuilt binary for Intel macOS (x86_64) — only Apple Silicon (arm64) is published. Build from source: clone the repo and run \`cargo build --release\`."
            fi
            echo "$arch-apple-darwin"
            ;;
        Linux)
            if [ "$arch" = "aarch64" ]; then
                echo "aarch64-unknown-linux-gnu"
            elif is_musl; then
                echo "x86_64-unknown-linux-musl"
            else
                echo "x86_64-unknown-linux-gnu"
            fi
            ;;
        *) die "unsupported OS: $os (pyq ships macOS and Linux builds)" ;;
    esac
}

is_musl() {
    # ldd prints the libc flavor; the musl one says "musl" on stderr.
    (ldd --version 2>&1 || true) | grep -qi musl
}

# Pull the first browser_download_url whose name ends in $1 out of the release
# JSON, without depending on jq.
asset_url() {
    printf '%s\n' "$1" \
        | grep -o '"browser_download_url"[ ]*:[ ]*"[^"]*"' \
        | sed 's/.*"\(https[^"]*\)"/\1/' \
        | grep -- "$2\$" \
        | head -n1
}

verify_sha() {
    expected="$(awk '{print $1}' "$2")"
    if has sha256sum; then
        actual="$(sha256sum "$1" | awk '{print $1}')"
    elif has shasum; then
        actual="$(shasum -a 256 "$1" | awk '{print $1}')"
    else
        die "need sha256sum or shasum to verify the download"
    fi
    [ -n "$expected" ] || die "empty checksum in sidecar"
    [ "$expected" = "$actual" ] \
        || die "checksum mismatch (expected $expected, got $actual) — refusing to install"
}

# Networking — both helpers send the User-Agent the GitHub API requires, plus a
# token (if $GITHUB_TOKEN is set) to lift the unauthenticated rate limit. The
# token branch is spelled out rather than splatted in, to stay quote-safe.
fetch() {
    if [ "$downloader" = "curl" ]; then
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            curl -fsSL -H "User-Agent: pyq-install" -H "Authorization: Bearer $GITHUB_TOKEN" "$1"
        else
            curl -fsSL -H "User-Agent: pyq-install" "$1"
        fi
    else
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            wget -qO- --header="User-Agent: pyq-install" --header="Authorization: Bearer $GITHUB_TOKEN" "$1"
        else
            wget -qO- --header="User-Agent: pyq-install" "$1"
        fi
    fi
}

download() {
    if [ "$downloader" = "curl" ]; then
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            curl -fsSL -H "User-Agent: pyq-install" -H "Authorization: Bearer $GITHUB_TOKEN" -o "$2" "$1"
        else
            curl -fsSL -H "User-Agent: pyq-install" -o "$2" "$1"
        fi
    else
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            wget -qO "$2" --header="User-Agent: pyq-install" --header="Authorization: Bearer $GITHUB_TOKEN" "$1"
        else
            wget -qO "$2" --header="User-Agent: pyq-install" "$1"
        fi
    fi
}

path_hint() {
    case ":$PATH:" in
        *":$1:"*) ;;
        *)
            say ""
            say "note: $1 is not on your PATH. Add it, e.g.:"
            say "  echo 'export PATH=\"$1:\$PATH\"' >> ~/.profile"
            ;;
    esac
}

has() { command -v "$1" >/dev/null 2>&1; }
need_cmd() { has "$1" || die "required command not found: $1"; }
say() { printf '%s\n' "$1" >&2; }
die() { printf 'install: %s\n' "$1" >&2; exit 1; }

main "$@"
