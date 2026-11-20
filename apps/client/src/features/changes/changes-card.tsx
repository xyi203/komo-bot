import type { RunChanges } from "@komo/protocol";
import { useState } from "react";
import { Icon } from "@/components/icon";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { ChangeTag, Counts, PathView, Totals, sumOf } from "./parts";

/**
 * 一轮末尾的改动摘要：一行淡字，不做卡片，免得每条回复都压一块框。
 * 只改了一个文件就是"已编辑 文件名 +a −d"；多个是"N 个文件已更改 +a −d"，旁边的箭头就地展开文件列表。
 * 点摘要或某一行在改动页打开对应文件。
 */
export function ChangesCard({ changes, onReview }: { changes: RunChanges; onReview: (run: string, index: number) => void }) {
  const [open, setOpen] = useState(false);
  if (!changes.files.length) return null;
  const chip =
    "inline-flex h-6 min-w-0 cursor-pointer items-center gap-1.5 rounded-sm px-1.5 text-small text-faint transition-colors hover:bg-hover-on-white hover:text-text";
  const single = changes.files.length === 1 ? changes.files[0]! : null;
  const [added, deleted] = sumOf(changes);
  return (
    <div className="mt-3 -ml-1.5">
      <div className="flex items-center">
        {single ? (
          <button type="button" className={chip} onClick={() => onReview(changes.run, 0)} aria-label={m.changes_card_open({ path: single.display })}>
            <Icon name="edit" className="size-3.5 shrink-0" />
            <span className="shrink-0">
              {single.change === "added" ? m.changes_verb_added() : single.change === "deleted" ? m.changes_verb_deleted() : m.changes_verb_edited()}
            </span>
            <PathView display={single.display} />
            <Counts file={single} />
          </button>
        ) : (
          <>
            <button type="button" className={chip} onClick={() => onReview(changes.run, 0)}>
              <Icon name="edit" className="size-3.5 shrink-0" />
              <span className="shrink-0">{m.changes_card_files({ count: changes.total })}</span>
              <Totals added={added} deleted={deleted} />
            </button>
            <button type="button" aria-expanded={open} aria-label={open ? m.changes_card_collapse() : m.changes_card_expand()} onClick={() => setOpen((v) => !v)} className={cn(chip, "px-1")}>
              <Icon name="right" className={cn("size-3.5 transition-transform", open && "rotate-90")} />
            </button>
          </>
        )}
      </div>
      {open && !single && (
        <div className="mt-0.5 ml-3.25 border-l border-rule pl-1.5">
          {changes.files.map((file, i) => (
            <button
              key={file.path}
              type="button"
              onClick={() => onReview(changes.run, i)}
              className="flex h-6 w-full max-w-120 cursor-pointer items-center gap-2 rounded-sm px-1.5 text-left text-small hover:bg-hover-on-white"
            >
              <PathView display={file.display} />
              <ChangeTag file={file} />
              <Counts file={file} />
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
