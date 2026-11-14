import type { MemoryBrief } from "@komo/api";
import type { InterventionSummary, RunChanges } from "@komo/protocol";
import { createContext, useContext } from "react";

export type ProcessTab = "steps" | "changes" | "trace";

/** 对话里的子组件要用到会话页的东西：记忆内容、每轮改动，以及打开过程面板的几个入口。 */
export interface ConversationContext {
  memories: ReadonlyMap<string, MemoryBrief>;
  changes: ReadonlyMap<string, RunChanges>;
  /** 过程面板（右侧滑出）当前看的那一轮；没开是 null。 */
  processRun: string | null;
  /** 打开过程面板并定位到这一轮（回复下的"看过程"）。 */
  openProcess: (run: string, tab?: ProcessTab) => void;
  /** 过程面板里一次调用的"查看"：切到轨迹，选中这条，详情开在概述。 */
  inspect: (resultEvent: string) => void;
  /** 改动卡片的一行：打开面板的改动标签，展开这个文件、滚到它。 */
  review: (run: string, index: number) => void;
  /** 指向这段对话、还没答复的介入（停住的运行在等你）：最后一句投进来的话下面给答复按钮。 */
  interventions: readonly InterventionSummary[];
}

const Context = createContext<ConversationContext | null>(null);
export const ConversationProvider = Context.Provider;

export function useConversation(): ConversationContext {
  const value = useContext(Context);
  if (!value) throw new Error("useConversation 要在 ConversationProvider 里用");
  return value;
}
