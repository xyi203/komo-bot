import { useSyncExternalStore } from "react";

// ⌘K 搜索弹窗的开关。侧栏的"搜索"按钮、⌘K 快捷键与桌面壳的菜单命令都只管 `openPalette()` / `togglePalette()`；
// 弹窗本身（`palette.tsx`）渲染在 `app-shell.tsx` 里。
let open = false;
let lastToggle = 0;
const listeners = new Set<() => void>();

function set(next: boolean) {
  if (next === open) return;
  open = next;
  for (const listener of listeners) listener();
}

export const openPalette = () => set(true);
export const closePalette = () => set(false);

/**
 * 开关。同一次按键可能同时从几处进来（桌面壳的回调、DOM 事件、页面自己的 keydown），
 * 150 ms 内的重复触发只算一次，免得一开一关又回原样。
 */
export function togglePalette() {
  const now = Date.now();
  if (now - lastToggle < 150) return;
  lastToggle = now;
  set(!open);
}

export function usePaletteOpen(): boolean {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => open,
    () => false,
  );
}
