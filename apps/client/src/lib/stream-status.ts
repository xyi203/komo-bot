import type { StreamState } from "@komo/api";
import { useSyncExternalStore } from "react";

// 图标栏设置按钮上的连接提示要看当前会话事件流的状态：会话页写进来，图标栏读（两者在路由树的不同位置）。
let state: StreamState | "idle" = "idle";
const listeners = new Set<() => void>();

export function setStreamStatus(next: StreamState | "idle"): void {
  if (next === state) return;
  state = next;
  for (const listener of listeners) listener();
}

export function useStreamStatus(): StreamState | "idle" {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => state,
    () => "idle",
  );
}
