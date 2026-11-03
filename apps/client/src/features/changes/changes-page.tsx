import { useRunDiff } from "@komo/api";
import type { ChangedFile, RunChanges } from "@komo/protocol";
import { useIsFetching } from "@tanstack/react-query";
import { useEffect } from "react";
import { Icon } from "@/components/icon";
import { errorText } from "@/lib/errors";
import { formatNumber } from "@/lib/format";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { homeDir, parsePatch } from "./diff";
import { ChangeTag, Counts, PathView, sumOf } from "./parts";

const DIFF_ROWS = 5000; // 一个文件最多画这么多行

export interface ChangesUi {
  /** 展开 / 收起的显式选择（`<run>:<index>`）；没选过的文件，最近一轮默认展开、较早的收起。 */
  open: Readonly<Record<string, boolean>>;
  toggle: (key: string, current: boolean) => void;
  /** 从卡片点进来要滚到的那个文件（`cg-<run>-<index>`）。 */
  reviewAt: string | null;
  clearReviewAt: () => void;
}

/**
 * 改动标签页：按轮列出，最近一轮在上、默认展开，较早的收起；一个文件一张卡，展开时才取对比，
 * 画成带新旧行号的 unified diff。
 */
export function ChangesPage({
  runs,
  turnNumbers,
  ui,
}: {
  /** 记得的各轮改动，最近一轮在前。 */
  runs: RunChanges[];
  turnNumbers: ReadonlyMap<string, number>;
  ui: ChangesUi;
}) {
  const fetching = useIsFetching({ queryKey: ["runs"] });
  const { reviewAt, clearReviewAt } = ui;
  // 从卡片点进来的那个文件：上面的对比陆续取回、高度在变，取完之前每次重画都再对一次位置
  useEffect(() => {
    if (!reviewAt) return;
    const frame = requestAnimationFrame(() => document.getElementById(reviewAt)?.scrollIntoView({ block: "start" }));
    if (fetching === 0) clearReviewAt();
    return () => cancelAnimationFrame(frame);
  });

  if (!runs.length) {
    return (
      <div className="mx-auto mt-[30vh] max-w-130 text-center text-read leading-6">
        <p className="mb-1.5">{m.changes_empty()}</p>
        <p className="mb-1.5 text-muted">{m.changes_empty_hint()}</p>
      </div>
    );
  }
  const latest = runs[0]!.run;
  return (
    <div className="mx-auto max-w-260 pt-1">
      {runs.map((changes, turnIndex) => {
        const [added, deleted] = sumOf(changes);
        const turn = turnNumbers.get(changes.run);
        return (
          <section key={changes.run} className={cn(turnIndex > 0 && "mt-6")}>
            <h2 className="mt-1 mb-2 flex items-baseline gap-2 text-label font-medium text-text">
              <span className="shrink-0">{turn ? m.changes_turn({ n: turn }) : m.changes_turn_unknown()}</span>
              <span className="shrink-0 font-normal text-muted tabular-nums">
                {m.changes_turn_files({ count: formatNumber(changes.total) })} · <span className="text-text">+{added}</span>{" "}
                <span className="text-faint">−{deleted}</span>
              </span>
              <span title={changes.cwd} className="ml-auto min-w-0 overflow-hidden font-mono text-small font-normal text-ellipsis whitespace-nowrap text-faint">
                {homeDir(changes.cwd)}
              </span>
            </h2>
            {changes.files.map((file, index) => {
              const key = `${changes.run}:${index}`;
              const open = ui.open[key] ?? changes.run === latest;
              return <FileCard key={file.path} run={changes.run} index={index} file={file} open={open} onToggle={() => ui.toggle(key, open)} />;
            })}
            {changes.total > changes.files.length && (
              <p className="py-1 text-small text-muted">{m.changes_more({ count: changes.total - changes.files.length })}</p>
            )}
          </section>
        );
      })}
      <p className="mt-5 mb-2 text-small leading-[18px] text-faint">{m.changes_foot()}</p>
    </div>
  );
}

function FileCard({ run, index, file, open, onToggle }: { run: string; index: number; file: ChangedFile; open: boolean; onToggle: () => void }) {
  return (
    <section id={`cg-${run}-${index}`} className="mb-2 overflow-hidden rounded-card bg-surface shadow-ring">
      <button
        type="button"
        aria-expanded={open}
        onClick={onToggle}
        className={cn(
          "flex min-h-10 w-full cursor-pointer items-center gap-2 px-3 text-left text-label text-text",
          open ? "bg-raised hover:bg-hover" : "bg-transparent hover:bg-hover-on-white",
        )}
      >
        <Icon name="right" className={cn("size-[15px] text-muted transition-transform", open && "rotate-90")} />
        <Icon name="file" className="size-[15px] text-muted" />
        <PathView display={file.display} className="flex-1 font-mono" />
        <ChangeTag file={file} />
        <Counts file={file} />
      </button>
      {open && <Diff run={run} index={index} />}
    </section>
  );
}

function Diff({ run, index }: { run: string; index: number }) {
  const diff = useRunDiff(run, index, true);
  const note = "m-0 border-t border-rule px-3 py-2.5 text-small text-muted";
  if (diff.isPending) return <p className={note}>{m.changes_diff_loading()}</p>;
  if (diff.isError) return <p className={cn(note, "text-negative")}>{m.changes_diff_error({ message: errorText(diff.error) })}</p>;
  const data = diff.data;
  if (data.status !== "text") {
    const text = data.status === "binary" ? m.changes_diff_binary() : data.status === "oversized" ? m.changes_diff_oversized() : m.changes_diff_identical();
    return <p className={note}>{text}</p>;
  }
  const rows = parsePatch(data.patch ?? "");
  const shown = rows.slice(0, DIFF_ROWS);
  // 新建的文件整段都是加的，不再逐行铺底
  const whole = rows.every((r) => r.t === "+" || r.t === "@");
  const ln = "w-11 shrink-0 pr-2 text-right text-faint tabular-nums select-none max-[600px]:w-[30px] max-[600px]:pr-1";
  return (
    <>
      <div className="overflow-x-auto border-t border-rule py-1 pb-1.5 font-mono text-[12.5px] leading-5">
        {shown.map((row, i) =>
          row.t === "@" ? (
            <div key={i} className="flex min-w-max bg-raised text-faint">
              <span className={ln} />
              <span className={ln} />
              <span className="flex-1 pr-4 pl-4 whitespace-pre">{row.s}</span>
            </div>
          ) : (
            <div
              key={i}
              className={cn("flex min-w-max", row.t === "+" && !whole && "bg-hover", row.t === "-" && "text-faint")}
            >
              <span className={cn(ln, row.t === "+" && "text-text")}>{row.a ?? ""}</span>
              <span className={cn(ln, row.t === "+" && "text-text")}>{row.b ?? ""}</span>
              <span aria-hidden="true" className={cn("w-4 shrink-0 text-center text-faint select-none", row.t === "+" && "text-text")}>
                {row.t === " " ? "" : row.t === "-" ? "−" : "+"}
              </span>
              <span className="flex-1 pr-4 whitespace-pre">{row.s || " "}</span>
            </div>
          ),
        )}
      </div>
      {rows.length > shown.length && <p className={note}>{m.changes_diff_truncated({ shown: DIFF_ROWS, rest: rows.length - shown.length })}</p>}
    </>
  );
}
