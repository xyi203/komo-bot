#!/usr/bin/env bash
# 从本地源码编译并安装 komo，然后重启 Gateway（docs/komo_bot.md §13.6 的本地版）。
#
#   scripts/install-local.sh [--prefix DIR] [--no-restart]
#
# 一条命令做完以前要模型摸索好几轮的事：
#   1. cargo build --release --locked -p komo。在 git worktree 里跑时默认共用主检出的
#      target/（CARGO_TARGET_DIR 没设才这样），依赖不用每个 worktree 冷编一遍。共用 target/ 时
#      cargo 可能把别的 worktree 编出来的 workspace crate 当成现成的复用（docs/storage-durable.md
#      §13.3），装上去的就不是这份源码。所以 target/ 里记一个 `.install-local-root` 戳，上一次
#      是别的检出编的就先 touch 本检出的 crates/*/src/lib.rs 与 crates/komo/src/main.rs，逼
#      cargo 重编 workspace crate（第三方依赖不受影响）；同一个检出连续安装不 touch，增量照旧。
#      cargo 不在 PATH 里（非交互 shell 常见）时，用 ${CARGO_HOME:-~/.cargo}/bin/cargo。
#   2. 新二进制先试跑 --version，再复制成 <prefix>/komo.new、mv 换上——不覆写正在跑的
#      Gateway 用着的那个 inode；旧的留一份 komo.bak。
#   3. komo gateway restart。在 komo 的 bash 工具里跑时，Gateway 会等这条命令的结果记进
#      会话之后再重启；声明了 resume_after_restart 的监听重启后自动重挂（docs/monitor.md §5）。
set -euo pipefail

prefix="${KOMO_PREFIX:-${HOME}/.local/bin}"
restart=1
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) prefix="$2"; shift 2 ;;
    --prefix=*) prefix="${1#--prefix=}"; shift ;;
    --no-restart) restart=0; shift ;;
    -h|--help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
    *) echo "不认识的参数：$1（--help 看用法）" >&2; exit 2 ;;
  esac
done

# 非交互 shell 的 PATH 里常常没有 ~/.cargo/bin：找得到就加进来，找不到就直说。
if ! command -v cargo >/dev/null 2>&1; then
  cargo_bin="${CARGO_HOME:-${HOME}/.cargo}/bin"
  if [ -x "${cargo_bin}/cargo" ]; then
    export PATH="${cargo_bin}:${PATH}"
  else
    echo "找不到 cargo：PATH 里没有，${cargo_bin}/cargo 也不存在。" >&2
    echo "请先安装 Rust 工具链（https://rustup.rs），或把 cargo 所在目录加进 PATH / 设置 CARGO_HOME。" >&2
    exit 1
  fi
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
  common="$(git -C "${root}" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
  if [ -n "${common}" ]; then
    export CARGO_TARGET_DIR="$(dirname "${common}")/target"
  else
    export CARGO_TARGET_DIR="${root}/target"
  fi
fi

# 上一次不是这个检出编的（或没有戳）：让 cargo 认为本检出的 workspace crate 变过。
stamp="${CARGO_TARGET_DIR}/.install-local-root"
if [ "$(cat "${stamp}" 2>/dev/null || true)" != "${root}" ]; then
  echo "==> ${CARGO_TARGET_DIR} 上次不是 ${root} 编的，touch crate 入口，避免复用别处的产物"
  touch "${root}"/crates/*/src/lib.rs "${root}/crates/komo/src/main.rs"
fi

echo "==> 编译 $(git -C "${root}" log -1 --format='%h %s' 2>/dev/null | cut -c1-72)"
echo "    target: ${CARGO_TARGET_DIR}"
started=$(date +%s)
(cd "${root}" && cargo build --release --locked -p komo)
mkdir -p "${CARGO_TARGET_DIR}"
printf '%s' "${root}" > "${stamp}"
built="${CARGO_TARGET_DIR}/release/komo"
version="$("${built}" --version)"
echo "    ${version}，用时 $(( $(date +%s) - started ))s"

mkdir -p "${prefix}"
target="${prefix}/komo"
backup=""
if [ -e "${target}" ]; then
  cp -p "${target}" "${target}.bak"
  backup="（旧的在 ${target}.bak）"
fi
cp "${built}" "${target}.new"
chmod +x "${target}.new"
mv -f "${target}.new" "${target}"
echo "==> 已安装 ${target}${backup}"

if [ "${restart}" = 1 ]; then
  echo "==> 重启 Gateway"
  "${target}" gateway restart
else
  echo "==> 没有重启 Gateway（--no-restart）；正在跑的还是旧版本"
fi
