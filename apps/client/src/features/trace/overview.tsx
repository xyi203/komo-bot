import { useRef, type PointerEvent } from "react";
import { formatFullTime, formatMs } from "@/lib/format";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import type { Overview, TraceRow } from "./model";

const kindName = (kind: TraceRow["kind"]) =>
  ({ user: m.trace_kind_user(), think: m.trace_kind_think(), model: m.trace_kind_model(), tool: m.trace_kind_tool(), ctx: m.trace_kind_ctx(), sys: m.trace_kind_sys() })[kind];

/**
 * 时间概览：输入 / 模型 / 工具三条泳道，墨色深浅区分；正在跑的那格叶绿、失败的红，轮与轮之间一条竖线。
 * 点一格选中那条记录；横向拖一段，记录表只列这段里在活动的记录；Esc、双击或"看全部"清掉。
 */
export function TraceOverview({
  rows,
  model,
  hits,
  range,
  selected,
  byDuration,
  onSelect,
  onRange,
}: {
  rows: readonly TraceRow[];
  model: Overview;
  hits: Set<string> | null;
  range: [number, number] | null;
  selected: string | null;
  byDuration: boolean;
  onSelect: (id: string) => void;
  onRange: (range: [number, number] | null) => void;
}) {
  const track = useRef<HTMLDivElement>(null);
  const drag = useRef<{ x: number; at: number; span: string | null } | null>(null);

  if (!model.spans.length) return <p className="mb-3.5 text-label text-muted">{m.trace_empty_overview()}</p>;

  const total = model.end - model.start;
  const pct = (n: number) => `${((n / total) * 100).toFixed(3)}%`;
  const at = (event: PointerEvent) => {
    const box = track.current!.getBoundingClientRect();
    const f = Math.min(1, Math.max(0, (event.clientX - box.left) / box.width));
    return model.start + f * total;
  };

  const marks = rows
    .filter((row, i) => row.n && row.n !== rows[i - 1]?.n && row.n > 1)
    .map((row) => model.spans.find((s) => s.row === row))
    .filter((s): s is NonNullable<typeof s> => !!s);

  return (
    <div className="grid grid-cols-[32px_minmax(0,1fr)] gap-2 pt-1.5 pb-2 shadow-[inset_0_-1px_0_var(--color-border)]">
      <div aria-hidden="true" className="grid grid-rows-[repeat(3,14px)] pt-[3px] text-small leading-3.5 text-faint">
        <span>{m.trace_lane_input()}</span>
        <span>{m.trace_lane_model()}</span>
        <span>{m.trace_lane_tool()}</span>
      </div>
      <div
        ref={track}
        role="group"
        aria-label={m.trace_overview_label()}
        className="relative h-[46px] cursor-crosshair touch-pan-y select-none"
        onPointerDown={(event) => {
          if (event.button !== 0) return;
          const target = (event.target as HTMLElement).closest<HTMLElement>("[data-span]");
          drag.current = { x: event.clientX, at: at(event), span: target?.dataset.span ?? null };
          event.currentTarget.setPointerCapture(event.pointerId);
        }}
        onPointerMove={(event) => {
          const d = drag.current;
          if (!d || Math.abs(event.clientX - d.x) < 4) return;
          const now = at(event);
          onRange([Math.min(d.at, now), Math.max(d.at, now)]);
        }}
        onPointerUp={(event) => {
          const d = drag.current;
          drag.current = null;
          if (d && Math.abs(event.clientX - d.x) < 4) {
            if (d.span) onSelect(d.span);
            else onRange(null);
          }
        }}
        onDoubleClick={() => onRange(null)}
      >
        {marks.map((s, i) => (
          <span key={i} className="absolute top-0 bottom-0 w-px bg-border" style={{ left: pct(s.start - model.start) }} />
        ))}
        {range && (
          <span
            className="pointer-events-none absolute top-0 bottom-0 bg-[rgb(34_40_28/0.05)] shadow-[inset_1px_0_0_var(--color-faint),inset_-1px_0_0_var(--color-faint)]"
            style={{ left: pct(range[0] - model.start), width: pct(range[1] - range[0]) }}
          />
        )}
        {model.spans.map((s, i) => {
          const row = s.row;
          const inRange = !range || (s.end >= range[0] && s.start <= range[1]);
          const dim = !inRange || (hits && (!row || !hits.has(row.id)));
          const bad = row && (row.kind === "tool" || row.kind === "ctx") && row.bad;
          const tip = row
            ? `${kindName(row.kind)} · ${row.at ? formatFullTime(row.at) : ""}${row.end && row.at ? ` · ${formatMs(Date.parse(row.end) - Date.parse(row.at))}` : ""}`
            : m.trace_waiting_model();
          return (
            <span
              key={row?.id ?? `wait-${i}`}
              data-span={row?.id}
              title={tip}
              className={cn(
                "absolute h-2 min-w-0.5 cursor-pointer rounded-[2px] transition-opacity hover:opacity-100 hover:brightness-[0.85]",
                s.lane === 0 && "top-1.5 bg-muted",
                s.lane === 1 && "top-5 bg-faint opacity-60",
                s.lane === 2 && "top-[34px] bg-control",
                !byDuration && "border-r border-surface",
                s.live && "animate-breathe bg-accent opacity-100",
                bad && "bg-negative opacity-100",
                row && row.id === selected && "z-1 opacity-100 shadow-[0_0_0_1px_var(--color-surface),0_0_0_2px_var(--color-text)]",
                dim && "opacity-[0.18]",
              )}
              style={{ left: pct(s.start - model.start), width: pct(s.end - s.start) }}
            />
          );
        })}
      </div>
    </div>
  );
}
