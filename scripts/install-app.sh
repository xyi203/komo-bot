#!/usr/bin/env bash
# 从本地源码重新构建 macOS 桌面 App（apps/desktop）并换上，同时更新 Gateway 托管的网页（install-local.sh 的前端版）。
#
#   scripts/install-app.sh [--app-dir DIR] [--prefix DIR] [--no-web] [--no-open] [--keep N]
#
#   --app-dir DIR  komo.app 装到哪（默认 ~/Applications）
#   --prefix DIR   komo 二进制所在目录，网页装到 <prefix>/web（默认 $KOMO_PREFIX 或 ~/.local/bin，
#                  与 install-local.sh 一致；Gateway 读的是可执行文件旁边的 web/）
#   --no-web       只换 App，不动 <prefix>/web
#   --no-open      换完不重新打开 App（默认：换之前在跑就重新打开）
#   --keep N       ~/.komo/backups 里 komo-app-* 备份只留最新 N 份（默认 5；0 = 不清理）
#
# 做的事：
#   1. pnpm install --frozen-lockfile，再 pnpm desktop:build（SPA → Electron TS → electron-builder --dir，
#      产物 apps/desktop/release/mac-arm64/komo.app，未签名，本机用）。只改了 Rust 不需要跑这个，用 install-local.sh。
#   2. 新 App 和网页先复制到目标旁边的临时目录；正在跑的 App（按安装路径认，不碰别处的 komo.app）
#      用 AppleScript 正常退出，等它退干净再 mv 换上。旧的 App 与网页挪进 ~/.komo/backups/komo-app-<时间>/。
#   3. 网页是 Gateway 每次请求现读磁盘，不用重启 Gateway；浏览器刷新、桌面 App 重开即生效。
set -euo pipefail

app_dir="${HOME}/Applications"
prefix="${KOMO_PREFIX:-${HOME}/.local/bin}"
install_web=1
open_after=auto
keep=5
while [ $# -gt 0 ]; do
  case "$1" in
    --app-dir) app_dir="$2"; shift 2 ;;
    --app-dir=*) app_dir="${1#--app-dir=}"; shift ;;
    --prefix) prefix="$2"; shift 2 ;;
    --prefix=*) prefix="${1#--prefix=}"; shift ;;
    --no-web) install_web=0; shift ;;
    --no-open) open_after=0; shift ;;
    --keep) keep="$2"; shift 2 ;;
    --keep=*) keep="${1#--keep=}"; shift ;;
    -h|--help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
    *) echo "不认识的参数：$1（--help 看用法）" >&2; exit 2 ;;
  esac
done
case "${keep}" in ''|*[!0-9]*) echo "--keep 要非负整数：${keep}" >&2; exit 2 ;; esac

if [ "$(uname -s)" != Darwin ]; then
  echo "桌面 App 只在 macOS 上构建；网页单独更新可以跑 pnpm --filter @komo/client build 后自己复制。" >&2
  exit 1
fi
if [ "$(uname -m)" != arm64 ]; then
  echo "pnpm desktop:build 固定出 arm64 包（apps/desktop/scripts/package.mjs），这台机器是 $(uname -m)。" >&2
  exit 1
fi

# 非交互 shell 的 PATH 里常常没有 node / pnpm：试几个常见位置，找不到就直说。
for tool in node pnpm; do
  command -v "${tool}" >/dev/null 2>&1 && continue
  for dir in "${PNPM_HOME:-}" "${HOME}/Library/pnpm/bin" "${HOME}/Library/pnpm" "${HOME}/.vite-plus/bin" \
    "${VOLTA_HOME:-${HOME}/.volta}/bin" "${HOME}/.local/bin" /opt/homebrew/bin /usr/local/bin; do
    if [ -n "${dir}" ] && [ -x "${dir}/${tool}" ]; then export PATH="${dir}:${PATH}"; break; fi
  done
  if ! command -v "${tool}" >/dev/null 2>&1; then
    echo "找不到 ${tool}：请先装好 Node ≥ 22 与 pnpm（corepack enable），或把它们加进 PATH。" >&2
    exit 1
  fi
done

root="$(cd "$(dirname "$0")/.." && pwd)"
built_app="${root}/apps/desktop/release/mac-arm64/komo.app"
built_web="${root}/apps/client/dist/client"
dest_app="${app_dir}/komo.app"
dest_web="${prefix}/web"
stamp_ts="$(date +%Y%m%d-%H%M%S)"
backup_root="${HOME}/.komo/backups"
backup="${backup_root}/komo-app-${stamp_ts}"

staged_app="${app_dir}/.komo.app.new-$$"
staged_web="${prefix}/.web.new-$$"
cleanup() { rm -rf "${staged_app}" "${staged_web}"; }
trap cleanup EXIT

echo "==> 构建 $(git -C "${root}" log -1 --format='%h %s' 2>/dev/null | cut -c1-72)"
dirty="$(git -C "${root}" status --porcelain -- apps packages 2>/dev/null | wc -l | tr -d ' ')"
[ "${dirty}" != 0 ] && echo "    apps/ packages/ 下有 ${dirty} 个未提交改动，一并编进去"
started=$(date +%s)
(cd "${root}" && pnpm install --frozen-lockfile && pnpm desktop:build)

# electron-builder 失败时可能留着上一次的产物：确认 .app 是这次出的。
asar="${built_app}/Contents/Resources/app.asar"
if [ ! -f "${asar}" ] || [ "$(stat -f %m "${asar}")" -lt "${started}" ]; then
  echo "构建没产出新的 ${built_app}（app.asar 不存在或早于本次构建）" >&2
  exit 1
fi
if [ ! -f "${built_web}/_shell.html" ]; then
  echo "构建没产出网页：${built_web}/_shell.html 不存在" >&2
  exit 1
fi
version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "${built_app}/Contents/Info.plist")"
echo "    komo.app ${version}，用时 $(( $(date +%s) - started ))s"

# 先把新东西复制到目标旁边（同一个卷，后面的 mv 是改名）；ditto 保留 Frameworks 里的符号链接与签名。
mkdir -p "${app_dir}"
ditto "${built_app}" "${staged_app}"
if [ "${install_web}" = 1 ]; then
  mkdir -p "${prefix}"
  ditto "${built_web}" "${staged_web}"
fi

# 只认装在目标路径上的那个进程；用 AppleScript 退出，走 before-quit，窗口与托盘正常收尾。
main_exe="${dest_app}/Contents/MacOS/komo"
running() { pgrep -xf "${main_exe}" >/dev/null 2>&1; }
was_running=0
if running; then
  was_running=1
  echo "==> 退出正在运行的 ${dest_app}"
  osascript -e "tell application \"${dest_app}\" to quit" >/dev/null || true
  for _ in $(seq 1 30); do running || break; sleep 0.5; done
  if running; then
    echo "${dest_app} 15 秒内没退出（可能有未关的对话框）。手动退出后重跑；什么都没换。" >&2
    exit 1
  fi
fi

mkdir -p "${backup}"
[ -e "${dest_app}" ] && mv "${dest_app}" "${backup}/komo.app"
mv "${staged_app}" "${dest_app}"
echo "==> 已安装 ${dest_app}"
if [ "${install_web}" = 1 ]; then
  [ -e "${dest_web}" ] && mv "${dest_web}" "${backup}/web"
  mv "${staged_web}" "${dest_web}"
  echo "==> 已更新 ${dest_web}（Gateway 现读磁盘，不用重启）"
fi
if [ -z "$(ls -A "${backup}")" ]; then
  rmdir "${backup}"
else
  echo "    旧的在 ${backup}"
fi

if [ "${keep}" -gt 0 ] && [ -d "${backup_root}" ]; then
  # 名字里是时间戳，按名字排序就是按时间。
  old="$(ls -1d "${backup_root}"/komo-app-* 2>/dev/null | sort -r | tail -n +"$(( keep + 1 ))")"
  if [ -n "${old}" ]; then
    echo "==> 清理旧备份（只留最新 ${keep} 份）："
    while IFS= read -r dir; do echo "    ${dir}"; rm -rf "${dir}"; done <<< "${old}"
  fi
fi

if [ "${open_after}" = auto ] && [ "${was_running}" = 1 ] || [ "${open_after}" = 1 ]; then
  echo "==> 重新打开 ${dest_app}"
  open "${dest_app}"
elif [ "${was_running}" = 1 ]; then
  echo "==> 没有重新打开（--no-open）"
fi
