import { useSyncExternalStore } from "react";
/**
 * 记忆页的筛选与"记一条"是否开着：放在模块里而不是组件里——`/memory` 与 `/memory/$id` 是两条路由，
 * 在它们之间切换（选中一条、点回列表）会重挂整页，筛选不该因此丢。
 */
export interface MemoryView {
  /** 输入框里的字（检索用去抖后的）。 */
  query: string;
  scope: string;
  adding: boolean;
  /** "相关才带"展开了全部 / "不再带"展开了没有。 */
  allClues: boolean;
  showOut: boolean;
  /** 进详情前列表滚到哪。 */
  listScroll: number;
}

let view: MemoryView = { query: "", scope: "", adding: false, allClues: false, showOut: false, listScroll: 0 };
const listeners = new Set<() => void>();

export function patchView(patch: Partial<MemoryView>) {
  view = { ...view, ...patch };
  for (const listener of listeners) listener();
}

export function useMemoryView(): MemoryView {
  return useSyncExternalStore(
    (notify) => {
      listeners.add(notify);
      return () => listeners.delete(notify);
    },
    () => view,
    () => view,
  );
}

export const getView = () => view;
