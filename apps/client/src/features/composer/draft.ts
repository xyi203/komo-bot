import { useCallback, useState } from "react";
import { readStored, writeStored } from "@/lib/storage";

// 输入框里没发出去的文字按会话存在这个浏览器的 localStorage（`komo.draft.<会话>`）：
// 切到记忆 / 事项页、换会话、刷新或关掉标签页再回来都还在；发出去或删空就删掉这个键。

const keyOf = (session: string) => `komo.draft.${session}`;

function forget(session: string) {
  try {
    window.localStorage.removeItem(keyOf(session));
  } catch {
    // 不记住而已
  }
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
    },
    [session],
  );
  return [text, update] as const;
}
