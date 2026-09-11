#!/usr/bin/env bash
# Install komo from GitHub release binaries.

set -euo pipefail

REPO="${KOMO_REPO:-solren7/komo}"
INSTALL_DIR="/usr/local/bin"
VERSION="${KOMO_VERSION:-latest}"

if [[ -n "${NO_COLOR:-}" ]]; then
    GREEN=""
    BLUE=""
    YELLOW=""
    RED=""
    NC=""
else
    GREEN="\033[0;32m"
    BLUE="\033[0;34m"
    YELLOW="\033[1;33m"
    RED="\033[0;31m"
    NC="\033[0m"
fi

usage() {
    cat <<EOF
Install komo from GitHub releases.

Usage:
  install.sh [version] [--prefix DIR]

Examples:
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/install.sh | bash
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/install.sh | bash -s -- --prefix "\$HOME/.local/bin"
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/install.sh | bash -s -- v0.1.0

Environment:
  KOMO_REPO       GitHub repo, default: ${REPO}
  KOMO_VERSION    Release tag, default: latest
EOF
}

info() {
    printf "${BLUE}%s${NC}\n" "$*"
}

success() {
    printf "${GREEN}✓${NC} %s\n" "$*"
}

warn() {
    printf "${YELLOW}%s${NC}\n" "$*"
}

error() {
    printf "${RED}error:${NC} %s\n" "$*" >&2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)
            if [[ -z "${2:-}" ]]; then
                error "--prefix requires a directory"
                exit 1
            fi
            INSTALL_DIR="$2"
            shift 2
            ;;
        --help | -h)
            usage
            exit 0
            ;;
        -*)
            error "unknown option: $1"
            usage
            exit 1
            ;;
        *)
            VERSION="$1"
            shift
            ;;
    esac
done

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        error "missing required command: $1"
        exit 1
    fi
}

require_cmd curl
require_cmd tar

detect_platform() {
    local os
    local arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin) os="darwin" ;;
        Linux) os="linux" ;;
        *)
            error "unsupported OS: ${os}. Release binaries are built for macOS and Linux."
            exit 1
            ;;
    esac

    case "$arch" in
        arm64 | aarch64) arch="arm64" ;;
        x86_64 | amd64) arch="amd64" ;;
        *)
            error "unsupported architecture: ${arch}"
            exit 1
            ;;
    esac

    printf "%s-%s" "$os" "$arch"
}

latest_tag() {
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" |
        sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' |
        head -n 1
}

resolve_tag() {
    local tag="$VERSION"
    if [[ "$tag" == "latest" || -z "$tag" ]]; then
        tag="$(latest_tag || true)"
        if [[ -z "$tag" ]]; then
            error "could not resolve latest release for ${REPO}"
            exit 1
        fi
    elif [[ "$tag" =~ ^[0-9] ]]; then
        tag="v${tag}"
    fi
    printf "%s" "$tag"
}

sha256_file() {
    local file="$1"
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
    else
        error "missing shasum or sha256sum for checksum verification"
        exit 1
    fi
}

needs_sudo() {
    if [[ -e "$INSTALL_DIR" ]]; then
        [[ ! -w "$INSTALL_DIR" ]]
        return
    fi

    local parent="$INSTALL_DIR"
    while [[ ! -e "$parent" ]]; do
        parent="$(dirname "$parent")"
    done
    [[ ! -w "$parent" ]]
}

run_install_cmd() {
    if needs_sudo; then
        sudo "$@"
    else
        "$@"
    fi
}

PLATFORM="$(detect_platform)"
TAG="$(resolve_tag)"
ASSET="komo-${PLATFORM}.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/komo-install.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT

info "Installing komo ${TAG} for ${PLATFORM}"

curl -fsSL -o "${TMP_DIR}/${ASSET}" "${BASE_URL}/${ASSET}"
curl -fsSL -o "${TMP_DIR}/SHA256SUMS" "${BASE_URL}/SHA256SUMS"

EXPECTED="$(awk -v asset="$ASSET" '$2 == asset { print $1; found = 1; exit } END { exit found ? 0 : 1 }' "${TMP_DIR}/SHA256SUMS")"
ACTUAL="$(sha256_file "${TMP_DIR}/${ASSET}")"
if [[ -z "$EXPECTED" || "$EXPECTED" != "$ACTUAL" ]]; then
    error "checksum verification failed for ${ASSET}"
    exit 1
fi
success "Verified checksum"

tar -xzf "${TMP_DIR}/${ASSET}" -C "$TMP_DIR"
if [[ ! -x "${TMP_DIR}/komo" ]]; then
    error "release archive did not contain an executable komo binary"
    exit 1
fi

if [[ ! -d "$INSTALL_DIR" ]]; then
    run_install_cmd mkdir -p "$INSTALL_DIR"
fi

run_install_cmd cp "${TMP_DIR}/komo" "${INSTALL_DIR}/komo.new"
run_install_cmd chmod +x "${INSTALL_DIR}/komo.new"
run_install_cmd mv -f "${INSTALL_DIR}/komo.new" "${INSTALL_DIR}/komo"
xattr -c "${INSTALL_DIR}/komo" >/dev/null 2>&1 || true

if ! "${INSTALL_DIR}/komo" --version >/dev/null 2>&1; then
    warn "Installed komo, but post-install version check failed."
else
    success "Installed $("${INSTALL_DIR}/komo" --version) to ${INSTALL_DIR}/komo"
fi

if [[ ":$PATH:" != *":${INSTALL_DIR}:"* ]]; then
    warn "${INSTALL_DIR} is not in PATH. Add it to your shell profile before running komo globally."
fi
