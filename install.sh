#!/bin/sh
# rustle installer — https://github.com/EdouardCourty/rustle
#
#   curl -fsSL https://raw.githubusercontent.com/EdouardCourty/rustle/main/install.sh | sh
#
# Prefer to read it first?
#   curl -fsSL https://raw.githubusercontent.com/EdouardCourty/rustle/main/install.sh -o install.sh
#   less install.sh && sh install.sh
#
# What it does: detects your OS/CPU, downloads the matching release archive
# from GitHub, verifies its SHA-256 against the SHA256SUMS of the same
# release, and installs the `rustle` binary. It never uses sudo.
#
# Environment:
#   RUSTLE_VERSION      version to install, e.g. v0.1.0 (default: latest)
#   RUSTLE_INSTALL_DIR  target directory (default: /usr/local/bin as root,
#                       otherwise ~/.local/bin)
#   RUSTLE_REPO         GitHub repo to download from (default below)
#   RUSTLE_BASE_URL     full base URL of the release files (testing)

# Everything lives in functions and runs from the last line: a download cut
# short can never execute half a script.

REPO_DEFAULT="EdouardCourty/rustle"
# Oldest glibc the Linux release is built against (Ubuntu 22.04).
MIN_GLIBC="2.35"

if [ -t 2 ]; then
    BOLD="$(printf '\033[1m')"; RED="$(printf '\033[31m')"
    GREEN="$(printf '\033[32m')"; DIM="$(printf '\033[2m')"; RESET="$(printf '\033[0m')"
else
    BOLD=""; RED=""; GREEN=""; DIM=""; RESET=""
fi

say() { printf '%s\n' "$*" >&2; }
info() { printf '%s==>%s %s\n' "$BOLD" "$RESET" "$*" >&2; }
fail() { printf '%serror:%s %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || fail "'$1' is required but not installed"; }

# download <url> <dest>
download() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 -o "$2" "$1" || fail "download failed: $1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1" || fail "download failed: $1"
    else
        fail "curl or wget is required"
    fi
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        fail "sha256sum or shasum is required to verify the download"
    fi
}

# version_ge <a> <b>: true when a >= b (dotted numbers).
version_ge() {
    [ "$(printf '%s\n%s\n' "$1" "$2" | sort -t. -k1,1n -k2,2n | head -n1)" = "$2" ]
}

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$arch" in
        x86_64 | amd64) arch="x86_64" ;;
        aarch64 | arm64) arch="aarch64" ;;
        *) fail "unsupported CPU architecture: $arch (supported: x86_64, aarch64)" ;;
    esac
    case "$os" in
        Linux)
            check_glibc
            TARGET="${arch}-unknown-linux-gnu"
            ;;
        Darwin)
            # A shell running under Rosetta reports x86_64 on an Apple
            # Silicon machine: install the native build.
            if [ "$arch" = "x86_64" ] &&
                [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = "1" ]; then
                arch="aarch64"
            fi
            TARGET="${arch}-apple-darwin"
            ;;
        *) fail "unsupported OS: $os (supported: Linux, macOS)" ;;
    esac
}

# The Linux binary is dynamically linked against glibc: refuse to install
# one that could not start.
check_glibc() {
    v="$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')"
    if [ -z "$v" ]; then
        # No glibc: musl (Alpine) or something exotic.
        fail "this system does not use glibc (Alpine / musl?). The Linux release needs glibc >= ${MIN_GLIBC}.
       Options: run it in a Debian/Ubuntu container, or build from source:
       https://github.com/${REPO}#installation"
    fi
    if ! version_ge "$v" "$MIN_GLIBC"; then
        fail "glibc $v is too old: the Linux release needs glibc >= ${MIN_GLIBC} (Debian 12, Ubuntu 22.04 or newer).
       Options: upgrade the distribution, or build from source:
       https://github.com/${REPO}#installation"
    fi
}

pick_install_dir() {
    if [ -n "${RUSTLE_INSTALL_DIR:-}" ]; then
        INSTALL_DIR="$RUSTLE_INSTALL_DIR"
    elif [ "$(id -u)" = "0" ]; then
        INSTALL_DIR="/usr/local/bin"
    else
        [ -n "${HOME:-}" ] || fail "HOME is not set: use RUSTLE_INSTALL_DIR"
        INSTALL_DIR="$HOME/.local/bin"
    fi
}

main() {
    REPO="${RUSTLE_REPO:-$REPO_DEFAULT}"
    need uname
    need tar
    need awk
    need sort

    detect_target
    pick_install_dir

    if [ -n "${RUSTLE_BASE_URL:-}" ]; then
        BASE="${RUSTLE_BASE_URL%/}"
    elif [ -n "${RUSTLE_VERSION:-}" ]; then
        v="${RUSTLE_VERSION#v}"
        BASE="https://github.com/${REPO}/releases/download/v${v}"
    else
        BASE="https://github.com/${REPO}/releases/latest/download"
    fi

    ASSET="rustle-${TARGET}.tar.gz"
    TMP="$(mktemp -d 2>/dev/null || mktemp -d -t rustle)" || fail "cannot create a temporary directory"
    trap 'rm -rf "$TMP"' EXIT INT TERM HUP

    info "installing rustle for ${TARGET}"
    say "${DIM}${BASE}/${ASSET}${RESET}"
    download "${BASE}/${ASSET}" "$TMP/$ASSET"
    download "${BASE}/SHA256SUMS" "$TMP/SHA256SUMS"

    expected="$(awk -v f="$ASSET" '{ n=$2; sub(/^\*/, "", n); if (n == f) { print $1; exit } }' "$TMP/SHA256SUMS")"
    [ -n "$expected" ] || fail "$ASSET is not listed in SHA256SUMS"
    actual="$(sha256_of "$TMP/$ASSET")"
    if [ "$expected" != "$actual" ]; then
        fail "checksum mismatch for $ASSET
       expected $expected
       got      $actual"
    fi
    say "checksum ok"

    mkdir -p "$TMP/x"
    tar -xzf "$TMP/$ASSET" -C "$TMP/x" || fail "could not extract $ASSET"
    [ -f "$TMP/x/rustle" ] || fail "the archive does not contain a rustle binary"
    chmod 755 "$TMP/x/rustle"
    # It must run here before it goes anywhere.
    version="$("$TMP/x/rustle" --version 2>&1)" || fail "the downloaded binary does not run on this system: $version"

    mkdir -p "$INSTALL_DIR" 2>/dev/null || true
    [ -w "$INSTALL_DIR" ] || fail "cannot write to $INSTALL_DIR
       Pick another directory (RUSTLE_INSTALL_DIR=\$HOME/bin) or run as root."
    # Copy next to the destination, then rename: a running rustle is
    # replaced atomically.
    cp "$TMP/x/rustle" "$INSTALL_DIR/.rustle.new.$$" || fail "cannot write to $INSTALL_DIR"
    chmod 755 "$INSTALL_DIR/.rustle.new.$$"
    mv -f "$INSTALL_DIR/.rustle.new.$$" "$INSTALL_DIR/rustle" || fail "cannot install into $INSTALL_DIR"

    printf '%s✔%s %s installed to %s/rustle\n' "$GREEN" "$RESET" "$version" "$INSTALL_DIR" >&2

    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *)
            say ""
            say "${INSTALL_DIR} is not in your PATH. Add it:"
            say "  export PATH=\"${INSTALL_DIR}:\$PATH\"   ${DIM}# put this in ~/.profile or ~/.zshrc${RESET}"
            ;;
    esac
    say ""
    say "Next steps:"
    say "  rustle init              ${DIM}# write a config with a fresh API key${RESET}"
    say "  rustle run               ${DIM}# run in the foreground${RESET}"
    say "  rustle service enable    ${DIM}# or: run at boot / login${RESET}"
    say "Update later with: rustle update      Remove with: rustle uninstall"
}

main "$@"
