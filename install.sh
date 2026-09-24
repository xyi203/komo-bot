#!/usr/bin/env bash
# 从 GitHub release 装上 komo（darwin / linux 的 arm64 与 amd64）。
#
# 资产名（komo-<os>-<arch>.tar.gz）与校验和文件（SHA256SUMS）由
# .github/workflows/release.yml 产出，`komo update` 认的是同一套——
# 这里、那里、还有 crates/komo/src/update.rs 三处必须一致。

set -euo pipefail

REPO="${KOMO_REPO:-xyi203/komo-bot}"
INSTALL_DIR="${KOMO_INSTALL_DIR:-$HOME/.local/bin}"
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
从 GitHub release 安装 komo。

用法：
  install.sh [版本] [--prefix 目录]

例子：
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/install.sh | bash
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/install.sh | bash -s -- v0.8.0
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/install.sh | bash -s -- --prefix /usr/local/bin

环境变量：
  KOMO_REPO         GitHub 仓库，默认：${REPO}
  KOMO_VERSION      发布标签，默认：latest
  KOMO_INSTALL_DIR  安装目录，默认：${INSTALL_DIR}
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
                error "--prefix 后面要跟一个目录"
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
            error "认不出的选项：$1"
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
        error "缺命令：$1"
        exit 1
    fi
}

require_cmd curl
require_cmd tar

# 与 release.yml 的矩阵一一对应；`komo update` 里那份映射在 update.rs。
detect_platform() {
    local os
    local arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin) os="darwin" ;;
        Linux) os="linux" ;;
        *)
            error "没有为 ${os} 构建的发布包（只有 macOS 与 Linux）"
            exit 1
            ;;
    esac

    case "$arch" in
        arm64 | aarch64) arch="arm64" ;;
        x86_64 | amd64) arch="amd64" ;;
        *)
            error "没有为 ${arch} 构建的发布包（只有 arm64 与 amd64）"
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
            error "问不出 ${REPO} 的最新发布——仓库不存在，或者还没有发布过"
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
        error "校验和要 shasum 或 sha256sum，两个都没有"
        exit 1
    fi
}

# `--prefix /usr/local/bin` 这类目录得 sudo；默认的 ~/.local/bin 与已有写权限的目录不用。
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

info "装 komo ${TAG}（${PLATFORM}）到 ${INSTALL_DIR}"

curl -fsSL -o "${TMP_DIR}/${ASSET}" "${BASE_URL}/${ASSET}"
curl -fsSL -o "${TMP_DIR}/SHA256SUMS" "${BASE_URL}/SHA256SUMS"

EXPECTED="$(awk -v asset="$ASSET" '$2 == asset { print $1; found = 1; exit } END { exit found ? 0 : 1 }' "${TMP_DIR}/SHA256SUMS")"
ACTUAL="$(sha256_file "${TMP_DIR}/${ASSET}")"
if [[ -z "$EXPECTED" || "$EXPECTED" != "$ACTUAL" ]]; then
    error "${ASSET} 的校验和对不上——没有装上去"
    exit 1
fi
success "校验和过了"

tar -xzf "${TMP_DIR}/${ASSET}" -C "$TMP_DIR"
if [[ ! -x "${TMP_DIR}/komo" ]]; then
    error "发布包里没有可执行的 komo"
    exit 1
fi

if [[ ! -d "$INSTALL_DIR" ]]; then
    run_install_cmd mkdir -p "$INSTALL_DIR"
fi

# 先落到同目录的 .new，再 mv 过去：换上去是一次 rename，不会留下半个二进制。
run_install_cmd cp "${TMP_DIR}/komo" "${INSTALL_DIR}/komo.new"
run_install_cmd chmod +x "${INSTALL_DIR}/komo.new"
run_install_cmd mv -f "${INSTALL_DIR}/komo.new" "${INSTALL_DIR}/komo"
xattr -c "${INSTALL_DIR}/komo" >/dev/null 2>&1 || true

if ! "${INSTALL_DIR}/komo" --version >/dev/null 2>&1; then
    warn "装上了，但装完试跑 --version 失败——这份二进制在这台机器上跑不起来。"
else
    success "已装 $("${INSTALL_DIR}/komo" --version) 到 ${INSTALL_DIR}/komo"
fi

if [[ ":$PATH:" != *":${INSTALL_DIR}:"* ]]; then
    warn "${INSTALL_DIR} 不在 PATH 里，先把它加进 shell 配置，才好在哪儿都用 komo。"
fi
