import type { ChangedFile, RunChanges } from "@komo/protocol";
import { splitDisplay } from "./diff";
import { m } from "@/paraglide/messages.js";
import { cn } from "@/lib/utils";

/** 展示路径：目录淡、文件名墨色，放不下先省目录。 */
export function PathView({ display, className }: { display: string; className?: string }) {
  const [dir, name] = splitDisplay(display);
  return (
    <span title={display} className={cn("flex min-w-0 whitespace-nowrap", className)}>
      <span className="min-w-0 flex-[0_1_auto] overflow-hidden text-ellipsis text-faint">{dir}</span>
      <span className="max-w-full flex-[0_0_auto] overflow-hidden text-ellipsis text-text">{name}</span>
    </span>
  );
}

/** 增删数；二进制写"二进制"，没存下来的写"过大"。 */
export function Counts({ file }: { file: ChangedFile }) {
  if (file.oversized) return <span className="ml-auto shrink-0 text-small text-faint">{m.changes_oversized()}</span>;
  if (file.binary) return <span className="ml-auto shrink-0 text-small text-faint">{m.changes_binary()}</span>;
  return <Totals added={file.added ?? 0} deleted={file.deleted ?? 0} />;
}

export function Totals({ added, deleted }: { added: number; deleted: number }) {
  return (
    <span className="ml-auto flex shrink-0 gap-1.5 font-mono text-small">
      <span className="text-diff-add-ink tabular-nums">+{added}</span>
      <span className="text-diff-del-ink tabular-nums">−{deleted}</span>
    </span>
  );
}

export function ChangeTag({ file }: { file: ChangedFile }) {
  if (file.change === "modified") return null;
  return (
    <span className="shrink-0 rounded-full bg-pill-bg px-1.5 text-small leading-[18px] text-pill-ink">
      {file.change === "added" ? m.changes_tag_added() : m.changes_tag_deleted()}
    </span>
  );
}

export const sumOf = (changes: RunChanges): [number, number] =>
  changes.files.reduce<[number, number]>((acc, f) => [acc[0] + (f.added ?? 0), acc[1] + (f.deleted ?? 0)], [0, 0]);
