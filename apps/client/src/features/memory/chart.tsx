import type { MemoryItem } from "@komo/protocol";
import { useEffect, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { dayDiff, formatDay, inBucket, moteKind, rungOf, stateName, ymd, type Bucket, type MoteKind } from "./model";

const CELL = 22;
const RANK = { always: 0, relevant: 1, paused: 2, out: 3 } as const;

// 点的样子：叶绿只表示"在场"；颜色全走 token。
const DOT: Record<MoteKind, string> = {
  full: "fill-accent",
  ring: "fill-surface stroke-accent stroke-[1.5]",
  pale: "fill-surface stroke-faint stroke-[1.2] [stroke-dasharray:2_2]",
  alert: "fill-surface stroke-negative stroke-[1.5]",
  off: "fill-memory-off",
};

/** 容器宽度（窗口变了重画）。 */
function useWidth() {
  const ref = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    setWidth(Math.round(el.clientWidth));
    const observer = new ResizeObserver(() => setWidth(Math.round(el.clientWidth)));
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  return [ref, width] as const;
}

/**
 * 攒下的记忆：横轴是记下的日子（跨度超过 60 天按周并），一天一格，格里一个点是一条记忆。
 * 颜色是在场程度（同光阶），点的面积按带进对话的次数（开方）；格内从左下往上排，在起作用的在下面。
 * 图例就是筛选：选中一档时其余点淡下去。
 */
export function MemoryChart({
  items,
  bucket,
  searching,
  selected,
  onPick,
}: {
  items: MemoryItem[];
  bucket: Bucket;
  /** 正在看检索结果：所有点都不亮。 */
  searching: boolean;
  selected: string | null;
  onPick: (id: string) => void;
}) {
  const [ref, measured] = useWidth();
  const layout = useMemo(() => {
    if (!items.length) return null;
    const today = ymd(new Date());
    const first = items.reduce((a, x) => (ymd(new Date(x.created_at)) < a ? ymd(new Date(x.created_at)) : a), today);
    const spanDays = dayDiff(today, first) + 1;
    return { first, bin: spanDays > 60 ? 7 : 1, nCols: Math.max(1, Math.ceil(spanDays / (spanDays > 60 ? 7 : 1))) };
  }, [items]);

  if (!layout) return <p ref={ref} className="my-3 rounded-control border border-dashed border-control px-3.5 py-2.5 text-label text-muted">{m.memory_chart_empty()}</p>;

  const { first, bin, nCols } = layout;
  const padL = 4;
  const padR = 4;
  const axisH = 26;
  const W = Math.max(280, measured || 1000);
  const colW = (W - padL - padR) / nCols;
  const per = Math.max(1, Math.min(4, Math.floor((colW - 6) / CELL))); // 每格最多四列，往上堆成柱
  const maxUse = Math.max(1, ...items.map((x) => x.usage.count));
  const cols: MemoryItem[][] = Array.from({ length: nCols }, () => []);
  for (const x of items) cols[Math.min(nCols - 1, Math.floor(dayDiff(ymd(new Date(x.created_at)), first) / bin))].push(x);
  for (const c of cols) c.sort((a, b) => RANK[rungOf(a) ?? "out"] - RANK[rungOf(b) ?? "out"] || b.usage.count - a.usage.count);
  const rows = Math.max(1, ...cols.map((c) => Math.ceil(c.length / per)));
  const H = 26 + rows * CELL + axisH;
  const base = H - axisH;
  const step = Math.ceil(nCols / Math.max(2, Math.floor(W / 84)));

  return (
    <div ref={ref}>
      <svg viewBox={`0 0 ${W} ${H}`} width={W} height={H} role="img" aria-label={m.memory_chart_aria({ count: items.length })} className="block h-auto max-w-full overflow-visible">
        {cols.map((_, ci) =>
          ci ? <line key={ci} x1={padL + ci * colW} x2={padL + ci * colW} y1={8} y2={base} className="stroke-rule" /> : null,
        )}
        <line x1={0} x2={W} y1={base} y2={base} className="stroke-border" />
        {cols.map((c, ci) => {
          const x0 = padL + ci * colW + (colW - Math.min(c.length, per) * CELL) / 2;
          return c.map((x, i) => {
            const cx = x0 + (i % per) * CELL + CELL / 2;
            const cy = base - Math.floor(i / per) * CELL - CELL / 2 - 2;
            const r = 3.5 + 5.5 * Math.sqrt(x.usage.count / maxUse);
            const lit = !searching && inBucket(x, bucket);
            return (
              <circle
                key={x.id}
                cx={cx}
                cy={cy}
                r={r}
                role="button"
                tabIndex={0}
                aria-label={x.content}
                onClick={() => onPick(x.id)}
                onKeyDown={(ev) => (ev.key === "Enter" || ev.key === " ") && onPick(x.id)}
                className={cn(
                  "cursor-pointer transition-opacity hover:stroke-text hover:stroke-[1.5] hover:opacity-100",
                  DOT[moteKind(x)],
                  !lit && "opacity-20",
                  x.id === selected && "stroke-text stroke-2",
                )}
              >
                <title>{m.memory_chart_dot({ content: x.content, state: stateName(x.state), count: x.usage.count })}</title>
              </circle>
            );
          });
        })}
        {cols.map((c, ci) =>
          c.length ? (
            <text key={ci} x={padL + (ci + 0.5) * colW} y={base - Math.ceil(c.length / per) * CELL - 8} textAnchor="middle" className="fill-text text-small font-medium">
              +{c.length}
            </text>
          ) : null,
        )}
        {cols.map((_, ci) => {
          if (ci % step && ci !== nCols - 1) return null;
          const d = new Date(`${first}T00:00:00`);
          d.setDate(d.getDate() + ci * bin);
          const text = ci === nCols - 1 && bin === 1 ? m.memory_chart_today() : bin > 1 ? m.memory_chart_week({ date: formatDay(d) }) : formatDay(d);
          return (
            <text key={ci} x={padL + (ci + 0.5) * colW} y={H - 8} textAnchor="middle" className="fill-muted text-small">
              {text}
            </text>
          );
        })}
      </svg>
    </div>
  );
}
