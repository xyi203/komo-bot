#!/usr/bin/env bash
# 从本地源码编译并安装 komo，然后重启 Gateway（docs/komo_bot.md §13.6 的本地版）。
#
#   scripts/install-local.sh [--prefix DIR] [--no-restart] [--no-web | --web-only]
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
#   3. 网页壳（docs/clients.md D5）：pnpm 构建 apps/client，产物拷成 <prefix>/komo-web/local-<时间>/，
#      再把 <prefix>/web 这个符号链接用一次 rename 指过去（与 install.sh、update.rs 同一布局），
#      清掉被换下的那一代。Gateway 每次请求都读文件，换网页不用重启；--web-only 只做这一步。
#      找不到 pnpm 时跳过并提示（--web-only 时报错）。
#   4. komo gateway restart。在 komo 的 bash 工具里跑时，Gateway 会等这条命令的结果记进
#      会话之后再重启；声明了 resume_after_restart 的监听重启后自动重挂（docs/monitor.md §5）。
set -euo pipefail

prefix="${KOMO_PREFIX:-${HOME}/.local/bin}"
restart=1
web=1
binary=1
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) prefix="$2"; shift 2 ;;
    --prefix=*) prefix="${1#--prefix=}"; shift ;;
    --no-restart) restart=0; shift ;;
    --no-web) web=0; shift ;;
    --web-only) web=1; binary=0; shift ;;
    -h|--help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
    *) echo "不认识的参数：$1（--help 看用法）" >&2; exit 2 ;;
  esac
done

# 非交互 shell 的 PATH 里常常没有 ~/.cargo/bin：找得到就加进来，找不到就直说。
if [ "${binary}" = 1 ] && ! command -v cargo >/dev/null 2>&1; then
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

# 网页壳：构建 → komo-web/<代次>/ → 一次 rename 换上 web 符号链接。
install_web() {
  if ! command -v pnpm >/dev/null 2>&1; then
    if [ "${binary}" = 0 ]; then
      echo "找不到 pnpm，装不了网页（corepack enable 或装 pnpm 后再试）。" >&2
      exit 1
    fi
    echo "==> 找不到 pnpm，跳过网页；${prefix}/web 保持原样" >&2
    return
  fi
  echo "==> 构建网页（apps/client）"
  (cd "${root}" && pnpm install --frozen-lockfile && pnpm --filter @komo/client build)
  local built_web="${root}/apps/client/dist/client"
  if [ ! -f "${built_web}/_shell.html" ]; then
    echo "构建产物里没有 ${built_web}/_shell.html，不装。" >&2
    exit 1
  fi
  mkdir -p "${prefix}/komo-web"
  if [ -d "${prefix}/web" ] && [ ! -L "${prefix}/web" ]; then
    echo "${prefix}/web 是个真目录，不是 komo 装的符号链接——先把它挪走再装。" >&2
    exit 1
  fi
  local generation="local-$(date +%Y%m%d%H%M%S)"
  cp -R "${built_web}" "${prefix}/komo-web/${generation}.tmp"
  mv "${prefix}/komo-web/${generation}.tmp" "${prefix}/komo-web/${generation}"
  local previous
  previous="$(readlink "${prefix}/web" 2>/dev/null || true)"
  ln -sfn "komo-web/${generation}" "${prefix}/web.new-$$"
  if [ "$(uname)" = Darwin ]; then
    mv -fh "${prefix}/web.new-$$" "${prefix}/web"
  else
    mv -fT "${prefix}/web.new-$$" "${prefix}/web"
  fi
  case "${previous}" in
    komo-web/?*) [ "${previous}" != "komo-web/${generation}" ] && rm -rf "${prefix:?}/${previous}" ;;
  esac
  echo "==> 已安装网页 ${prefix}/web -> komo-web/${generation}（不用重启，刷新页面即生效）"
}

if [ "${binary}" = 0 ]; then
  install_web
  exit 0
fi

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

if [ "${web}" = 1 ]; then
  install_web
fi

if [ "${restart}" = 1 ]; then
  echo "==> 重启 Gateway"
  "${target}" gateway restart
else
  echo "==> 没有重启 Gateway（--no-restart）；正在跑的还是旧版本"
fi
