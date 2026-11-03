import type { RunChanges } from "@komo/protocol";
import { useState } from "react";
import { Icon } from "@/components/icon";
import { m } from "@/paraglide/messages.js";
import { ChangeTag, Counts, PathView, Totals, sumOf } from "./parts";

const CARD_ROWS = 4; // 卡片先列这么多行

/**
 * 一轮末尾的卡片：只改了一个文件就是"已编辑 文件名"；多个列出来，先 4 行，其余折叠。
 * 点一行在改动页打开这个文件。
 */
export function ChangesCard({ changes, onReview }: { changes: RunChanges; onReview: (run: string, index: number) => void }) {
  const [all, setAll] = useState(false);
  if (!changes.files.length) return null;
  const head = "flex min-h-11 w-full cursor-pointer items-center gap-2 px-3 text-left text-body text-text hover:bg-hover-on-white";
  const icon = (name: "file" | "code") => (
    <span className="grid size-6 shrink-0 place-items-center rounded-sm text-muted shadow-ring">
      <Icon name={name} className="size-3.5" />
    </span>
  );
  if (changes.files.length === 1) {
    const file = changes.files[0]!;
    return (
      <div className="mt-2.5 mb-0.5 overflow-hidden rounded-card bg-surface shadow-ring">
        <button type="button" className={head} onClick={() => onReview(changes.run, 0)} aria-label={m.changes_card_open({ path: file.display })}>
          {icon("file")}
          <span className="shrink-0 font-medium">
            {file.change === "added" ? m.changes_verb_added() : file.change === "deleted" ? m.changes_verb_deleted() : m.changes_verb_edited()}
          </span>
          <PathView display={file.display} className="text-body" />
          <Counts file={file} />
        </button>
      </div>
    );
  }
  const [added, deleted] = sumOf(changes);
  const rows = all ? changes.files : changes.files.slice(0, CARD_ROWS);
  return (
    <div className="mt-2.5 mb-0.5 overflow-hidden rounded-card bg-surface shadow-ring">
      <button type="button" className={`${head} border-b border-rule`} onClick={() => onReview(changes.run, 0)}>
        {icon("code")}
        <span className="shrink-0 font-medium">{m.changes_card_files({ count: changes.total })}</span>
        <Totals added={added} deleted={deleted} />
      </button>
      <div>
        {rows.map((file, i) => (
          <button
            key={file.path}
            type="button"
            onClick={() => onReview(changes.run, i)}
            className="flex min-h-8.5 w-full cursor-pointer items-center gap-2 border-0 bg-transparent pr-3 pl-11 text-left text-label text-text hover:bg-hover-on-white max-[600px]:pl-3"
          >
            <PathView display={file.display} />
            <ChangeTag file={file} />
            <Counts file={file} />
          </button>
        ))}
      </div>
      {changes.files.length > CARD_ROWS && (
        <button
          type="button"
          onClick={() => setAll((v) => !v)}
          className="block h-8 w-full cursor-pointer border-t border-rule text-small text-muted hover:bg-hover-on-white hover:text-text"
        >
          {all ? m.changes_card_collapse() : m.changes_card_expand({ count: changes.files.length - CARD_ROWS })}
        </button>
      )}
    </div>
  );
}
