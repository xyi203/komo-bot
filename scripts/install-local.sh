#!/usr/bin/env bash
# 从本地源码编译并安装 komo，然后重启 Gateway（docs/komo_bot.md §13.6 的本地版）。
#
#   scripts/install-local.sh [--prefix DIR] [--no-restart]
#
# 一条命令做完以前要模型摸索好几轮的事：
#   1. cargo build --release --locked -p komo。在 git worktree 里跑时默认共用主检出的
#      target/（CARGO_TARGET_DIR 没设才这样），依赖不用每个 worktree 冷编一遍。
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

root="$(cd "$(dirname "$0")/.." && pwd)"
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
  common="$(git -C "${root}" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
  if [ -n "${common}" ]; then
    export CARGO_TARGET_DIR="$(dirname "${common}")/target"
  else
    export CARGO_TARGET_DIR="${root}/target"
  fi
fi

echo "==> 编译 $(git -C "${root}" log -1 --format='%h %s' 2>/dev/null | cut -c1-72)"
echo "    target: ${CARGO_TARGET_DIR}"
started=$(date +%s)
(cd "${root}" && cargo build --release --locked -p komo)
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
