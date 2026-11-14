import { useState } from "react";
import { Icon } from "@/components/icon";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { useConversation } from "./context";
import type { MemoryUse } from "./events";

type Memories = ReturnType<typeof useConversation>["memories"];

function recallParts(uses: MemoryUse[], memories: Memories) {
  const mems = uses.map((u) => memories.get(u.memory));
  const conventions = mems.filter((x) => x?.application === "convention").length;
  return { mems, conventions, references: mems.length - conventions };
}

/** "带上 2 条约定、1 条参考"：过程面板里召回一行的标题。 */
function recallSummary(uses: MemoryUse[], memories: Memories): string {
  const { conventions, references } = recallParts(uses, memories);
  const parts = [conventions && m.recall_conventions({ count: conventions }), references && m.recall_references({ count: references })].filter(Boolean);
  return m.recall_brought({ parts: parts.join(m.recall_separator()) });
}

/** 约定是实心点，参考是空心圈。 */
export const mote = (convention: boolean) => (
  <span aria-hidden="true" className={cn("inline-block size-2 shrink-0 rounded-full", convention ? "bg-accent" : "border-[1.5px] border-accent")} />
);

/** 这一轮带上的几条记忆，一条一行（回复下"用上了 N 条记忆"展开的，与过程面板里的召回共用）。 */
export function RecallList({ uses }: { uses: MemoryUse[] }) {
  const { memories } = useConversation();
  const { mems } = recallParts(uses, memories);
  return (
    <div className="mt-1.5 mb-1 border-l border-border py-1">
      {uses.map((use, i) => {
        const memory = mems[i];
        return (
          <div key={use.memory} className="grid grid-cols-[10px_minmax(0,1fr)] gap-2 py-1 pl-3.5 text-body">
            <span className="mt-[7px]">{mote(memory?.application === "convention")}</span>
            <span>{memory ? memory.content : <span className="text-muted">{m.recall_missing({ id: use.memory })}</span>}</span>
          </div>
        );
      })}
    </div>
  );
}

/** 过程面板里的召回：一行摘要，点开列出那几条。 */
export function RecallFold({ uses }: { uses: MemoryUse[] }) {
  const { memories } = useConversation();
  const [open, setOpen] = useState(false);
  return (
    <div className="mb-2">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className="inline-flex cursor-pointer items-center gap-1 py-0.5 text-left text-body leading-6 text-accent hover:text-text"
      >
        <span className="mr-1">{mote(true)}</span>
        {recallSummary(uses, memories)}
        <Icon name="right" className={cn("size-3.5 transition-transform", open && "rotate-90")} />
      </button>
      {open && <RecallList uses={uses} />}
    </div>
  );
}
