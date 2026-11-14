import { useSyncExternalStore } from "react";

// 设置弹窗的开关与当前节（照 ⌘K 的 store）。侧栏的齿轮与 `/settings[/节]` 路由只管 `openSettings()`；
// 弹窗本身（`settings-dialog.tsx`）渲染在 `app-shell.tsx` 里。弹窗关着时节内容不挂载，所以每次打开都重新取数、丢掉没提交的表单。
export const SETTINGS_SECTIONS = ["general", "models", "connect", "about"] as const;
export type SettingsSection = (typeof SETTINGS_SECTIONS)[number];

// 旧的六节（`/settings/mcp` 一类的存量链接、桌面壳菜单传来的节名）落到现在的哪一节
const LEGACY: Record<string, SettingsSection> = {
  config: "general",
  index: "general",
  env: "about",
  gateway: "about",
  mcp: "connect",
  channels: "connect",
};

/** 节名认新的四个，也认旧的六个；认不出是 `undefined`。 */
export function settingsSectionOf(value: string | undefined): SettingsSection | undefined {
  if (!value) return undefined;
  if ((SETTINGS_SECTIONS as readonly string[]).includes(value)) return value as SettingsSection;
  return LEGACY[value];
}

interface State {
  open: boolean;
  section: SettingsSection;
}

let state: State = { open: false, section: "general" };
const listeners = new Set<() => void>();

function set(next: State) {
  state = next;
  for (const listener of listeners) listener();
}

export function openSettings(section?: SettingsSection) {
  set({ open: true, section: section ?? state.section });
}
export const closeSettings = () => set({ ...state, open: false });
export const selectSettingsSection = (section: SettingsSection) => set({ ...state, section });

export function useSettingsState(): State {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => state,
    () => state,
  );
}
