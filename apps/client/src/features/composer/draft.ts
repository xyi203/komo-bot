import { useCallback, useState, useSyncExternalStore } from "react";
import { readStored, writeStored } from "@/lib/storage";

// 输入框里没发出去的文字按会话存在这个浏览器的 localStorage（`komo.draft.<会话>`）：
// 切到记忆 / 事项页、换会话、刷新或关掉标签页再回来都还在；发出去或删空就删掉这个键。

const PREFIX = "komo.draft.";
const keyOf = (session: string) => `${PREFIX}${session}`;

function forget(session: string) {
  try {
    window.localStorage.removeItem(keyOf(session));
  } catch {
    // 不记住而已
  }
}

// 哪些会话框里还有没发的东西（文字或附件）。侧栏与 ⌘K 本来不列一句话都没发过的空会话，
// 有草稿的照列（行尾标"草稿"）：新建对话打了字、没发就切走，还能点回来接着写。
// 只在集合真有变化时通知，打字不会让侧栏逐键重画。
const withText = new Set<string>(scanStored());
const withFiles = new Set<string>();
let drafted: ReadonlySet<string> = new Set(withText);
const listeners = new Set<() => void>();

function scanStored(): string[] {
  if (typeof window === "undefined") return [];
  try {
    const out: string[] = [];
    for (let i = 0; i < window.localStorage.length; i++) {
      const key = window.localStorage.key(i);
      if (key?.startsWith(PREFIX)) out.push(key.slice(PREFIX.length));
    }
    return out;
  } catch {
    return [];
  }
}

function mark(set: Set<string>, session: string, has: boolean) {
  if (set.has(session) === has) return;
  if (has) set.add(session);
  else set.delete(session);
  drafted = new Set([...withText, ...withFiles]);
  listeners.forEach((listener) => listener());
}

/** 附件那边（`use-attachments.ts`）报告某个会话框里有没有待发的附件。 */
export const markFileDraft = (session: string, has: boolean) => mark(withFiles, session, has);

const subscribe = (listener: () => void) => {
  listeners.add(listener);
  return () => void listeners.delete(listener);
};

/** 有草稿的会话 id 集合（文字或附件）。 */
export function useDraftedSessions(): ReadonlySet<string> {
  return useSyncExternalStore(
    subscribe,
    () => drafted,
    () => drafted,
  );
}

/** 某个会话的文字草稿：初值从 localStorage 读，每次改动写回。 */
export function useDraftText(session: string) {
  const [text, setText] = useState(() => readStored<string>(keyOf(session), (raw) => (typeof raw === "string" ? raw : undefined), ""));
  const update = useCallback(
    (next: string) => {
      setText(next);
      // 存成 JSON 字符串：readStored 会先试 JSON.parse，裸存 "123" 读回来就成了数字
      if (next) writeStored(keyOf(session), JSON.stringify(next));
      else forget(session);
      mark(withText, session, next !== "");
    },
    [session],
  );
  return [text, update] as const;
}
