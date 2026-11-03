import { useSyncExternalStore } from "react";

// 一句话的瞬时提示（原型里是 alert）：操作失败时说一声，几秒后自己消失。
export interface Toast {
  id: number;
  text: string;
}

let toasts: Toast[] = [];
let nextId = 1;
const listeners = new Set<() => void>();

function emit() {
  for (const listener of listeners) listener();
}

export function showToast(text: string, ms = 6000): void {
  const id = nextId++;
  toasts = [...toasts, { id, text }];
  emit();
  setTimeout(() => dismissToast(id), ms);
}

export function dismissToast(id: number): void {
  toasts = toasts.filter((t) => t.id !== id);
  emit();
}

export function useToasts(): Toast[] {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => toasts,
    () => toasts,
  );
}
