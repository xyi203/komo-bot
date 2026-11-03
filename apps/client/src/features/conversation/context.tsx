import type { MemoryBrief } from "@komo/api";
import type { RunChanges } from "@komo/protocol";
import { createContext, useContext } from "react";

/** 对话里的子组件要用到会话页的东西：记忆内容、每轮改动，以及"查看"（跳到轨迹）与"看改动"（跳到改动页）。 */
export interface ConversationContext {
  memories: ReadonlyMap<string, MemoryBrief>;
  changes: ReadonlyMap<string, RunChanges>;
  /** 对话里一次调用的"查看"：切到轨迹，选中这条，详情开在概述。 */
  inspect: (resultEvent: string) => void;
  /** 改动卡片的一行：到改动页，展开这个文件、滚到它。 */
  review: (run: string, index: number) => void;
}

const Context = createContext<ConversationContext | null>(null);
export const ConversationProvider = Context.Provider;

export function useConversation(): ConversationContext {
  const value = useContext(Context);
  if (!value) throw new Error("useConversation 要在 ConversationProvider 里用");
  return value;
}
