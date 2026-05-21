#!/bin/sh
set -eu

REPO="world-in-progress/c-two"
VERSION="${C3_VERSION:-latest}"
TARGET="${C3_TARGET:-}"
BIN_DIR="${C3_INSTALL_DIR:-}"
PRINT_TARGET=0

usage() {
    cat <<'USAGE'
Install the c3 CLI from GitHub Releases.

Usage:
  install-c3.sh [OPTIONS]

Options:
  -b, --bin-dir DIR     Install c3 into DIR.
  --version VERSION     Install c3-vVERSION instead of the latest release.
  --target TARGET       Download a specific release target asset.
  --print-target        Print the detected release target and exit.
  -h, --help            Print this help.

Environment:
  C3_VERSION            Default version when --version is not passed.
  C3_TARGET             Default target when --target is not passed.
  C3_INSTALL_DIR        Default install directory when --bin-dir is not passed.
  C3_RELEASE_BASE_URL   Override release asset base URL for mirrors or tests.
USAGE
}

err() {
    printf 'c3 installer: %s\n' "$*" >&2
    exit 1
}

need_arg() {
    option=$1
    shift
    if [ "$#" -eq 0 ] || [ -z "$1" ]; then
        err "$option requires a value"
    fi
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        -b|--bin-dir)
            shift
            need_arg "--bin-dir" "$@"
            BIN_DIR=$1
            ;;
        --version)
            shift
            need_arg "--version" "$@"
            VERSION=$1
            ;;
        --target)
            shift
            need_arg "--target" "$@"
            TARGET=$1
            ;;
        --print-target)
            PRINT_TARGET=1
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        *)
            err "unknown option: $1"
            ;;
    esac
    shift
done

detect_target() {
    os=$(printf '%s' "${C3_INSTALLER_OS:-$(uname -s)}" | tr '[:upper:]' '[:lower:]')
    arch=$(printf '%s' "${C3_INSTALLER_ARCH:-$(uname -m)}" | tr '[:upper:]' '[:lower:]')

    case "$arch" in
        x86_64|amd64)
            arch="x86_64"
            ;;
        aarch64|arm64)
            arch="aarch64"
            ;;
        *)
            err "unsupported CPU architecture: $arch"
            ;;
    esac

    case "$os" in
        linux)
            printf '%s-unknown-linux-gnu\n' "$arch"
            ;;
        darwin)
            printf '%s-apple-darwin\n' "$arch"
            ;;
        *)
            err "unsupported operating system: $os"
            ;;
    esac
}

default_bin_dir() {
    if command -v id >/dev/null 2>&1 && [ "$(id -u)" = "0" ]; then
        printf '%s\n' "/usr/local/bin"
        return
    fi
    if [ -z "${HOME:-}" ]; then
        err "HOME is not set; pass --bin-dir"
    fi
    printf '%s\n' "$HOME/.local/bin"
}

download() {
    url=$1
    dest=$2
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$dest" "$url"
        return
    fi
    if command -v wget >/dev/null 2>&1; then
        wget -q -O "$dest" "$url"
        return
    fi
    err "curl or wget is required"
}

verify_checksum() {
    checksum_file=$1
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$checksum_file" >/dev/null
        return
    fi
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c "$checksum_file" >/dev/null
        return
    fi
    err "sha256sum or shasum is required"
}

release_base_url() {
    if [ -n "${C3_RELEASE_BASE_URL:-}" ]; then
        printf '%s\n' "${C3_RELEASE_BASE_URL%/}"
        return
    fi
    if [ "$VERSION" = "latest" ]; then
        printf 'https://github.com/%s/releases/latest/download\n' "$REPO"
        return
    fi
    case "$VERSION" in
        c3-v*)
            tag=$VERSION
            ;;
        *)
            tag="c3-v$VERSION"
            ;;
    esac
    printf 'https://github.com/%s/releases/download/%s\n' "$REPO" "$tag"
}

if [ -z "$TARGET" ]; then
    TARGET=$(detect_target)
fi

if [ "$PRINT_TARGET" = "1" ]; then
    printf '%s\n' "$TARGET"
    exit 0
fi

if [ -z "$BIN_DIR" ]; then
    BIN_DIR=$(default_bin_dir)
fi

asset="c3-$TARGET"
base_url=$(release_base_url)
tmpdir=$(mktemp -d 2>/dev/null || mktemp -d -t c3-install)
trap 'rm -rf "$tmpdir"' EXIT INT HUP TERM

download "$base_url/$asset" "$tmpdir/$asset"
download "$base_url/$asset.sha256" "$tmpdir/$asset.sha256"

(
    cd "$tmpdir"
    verify_checksum "$asset.sha256"
)

chmod 755 "$tmpdir/$asset"
if ! "$tmpdir/$asset" --version >/dev/null 2>&1; then
    err "downloaded c3 binary failed to run on this host; check --target"
fi

mkdir -p "$BIN_DIR"
install_path="$BIN_DIR/c3"
cp "$tmpdir/$asset" "$install_path"
chmod 755 "$install_path"

printf 'Installed c3 to %s\n' "$install_path"
"$install_path" --version
