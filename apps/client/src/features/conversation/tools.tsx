import { useState } from "react";
import { CopyButton } from "@/components/copy-button";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { useConversation } from "./context";
import type { MemoryUse } from "./events";
import { mote, RecallList } from "./recall";

/**
 * 回复下面的一行工具（docs/redesign.md P1）：复制、看过程、用上了 N 条记忆。平时隐藏，悬停或键盘聚焦这条回复时出现
 * （由外层的 `group/msg` 驱动）；`always` 是最后一条回复，常显。没有悬停的设备一直显示。
 * `run` 给了才有"看过程"；`recall` 空（N = 0）不显示记忆那一项；点它在这一行下面展开那几条。
 */
export function ReplyTools({ text, run, recall = [], always }: { text?: string; run?: string; recall?: MemoryUse[]; always: boolean }) {
  const { openProcess, processRun } = useConversation();
  const [memoryOpen, setMemoryOpen] = useState(false);
  const chip =
    "inline-flex h-6 cursor-pointer items-center gap-1.25 rounded-sm px-1.5 text-small text-faint transition-colors narrow:relative narrow:after:absolute narrow:after:-inset-x-0.5 narrow:after:-inset-y-2.5 hover:bg-hover-on-white hover:text-text";
  return (
    <>
      <div
        className={cn(
          "mt-0.5 -ml-1.5 flex h-6 items-center gap-1 text-small text-faint",
          !always && "[@media(hover:hover)]:opacity-0 [@media(hover:hover)]:transition-opacity group-hover/msg:opacity-100 focus-within:opacity-100",
        )}
      >
        {text && <CopyButton text={text} label={m.copy_answer()} className="size-6" />}
        {run && (
          <button type="button" aria-expanded={processRun === run} onClick={() => openProcess(run)} className={chip}>
            {m.reply_view_process()}
          </button>
        )}
        {recall.length > 0 && (
          <button type="button" aria-expanded={memoryOpen} onClick={() => setMemoryOpen((v) => !v)} className={chip}>
            {mote(true)}
            {m.reply_memories({ count: recall.length })}
          </button>
        )}
      </div>
      {memoryOpen && <RecallList uses={recall} />}
    </>
  );
}
