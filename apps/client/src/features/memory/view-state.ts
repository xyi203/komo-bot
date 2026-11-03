import { useSyncExternalStore } from "react";
import type { Bucket } from "./model";

/**
 * 记忆页的筛选与"记一条"是否开着：放在模块里而不是组件里——`/memory` 与 `/memory/$id` 是两条路由，
 * 在它们之间切换（选中一条、点回列表）会重挂整页，筛选不该因此丢。
 */
export interface MemoryView {
  bucket: Bucket;
  /** 输入框里的字（检索用去抖后的）。 */
  query: string;
  scope: string;
  adding: boolean;
  /** 窄屏进详情前列表滚到哪。 */
  listScroll: number;
}

let view: MemoryView = { bucket: "live", query: "", scope: "", adding: false, listScroll: 0 };
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
