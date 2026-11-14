import type { MemoryBrief, SessionTree } from "@komo/api";
import type { WireEvent } from "@komo/protocol";
import { useEffect, useMemo } from "react";
import { Icon, type IconName } from "@/components/icon";
import { useNow } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { TraceDetail } from "./detail";
import { searchHits, traceOf, traceSpans } from "./model";
import { TraceOverview } from "./overview";
import { RecordList } from "./record-list";
import type { TraceUi } from "./state";

/**
 * 轨迹标签页（web.md §4「轨迹」）：工具栏（时长、轮次、调用）与搜索，时间概览，按轮的记录表，选中一条后右边的详情
 * （窄屏从底下升起）。跑着的时候每秒重算一次，让进行中的那一格和计时走起来。
 */
export function TraceView({
  events,
  liveRuns,
  tree,
  memories,
  ui,
  focusRun,
  compact = false,
}: {
  events: readonly WireEvent[];
  liveRuns: ReadonlyMap<string, unknown>;
  tree: SessionTree | undefined;
  memories: ReadonlyMap<string, MemoryBrief>;
  ui: TraceUi;
  /** 过程面板里开着时：滚到这一轮的轮头。 */
  focusRun?: string;
  /** 窄的容器（过程面板，380 宽）：单列，详情挪到记录表上面成一块，不吸顶也不从底下升起。 */
  compact?: boolean;
}) {
  const now = useNow(liveRuns.size > 0);
  const trace = useMemo(() => traceOf(events, liveRuns, tree, memories), [events, liveRuns, tree, memories]);
  const overview = useMemo(() => traceSpans(trace, ui.byDuration, now), [trace, ui.byDuration, now]);
  const hits = useMemo(() => searchHits(trace, ui.query), [trace, ui.query]);
  const picked = trace.rows.find((r) => r.id === ui.selected);

  const focusN = focusRun ? trace.runs.get(focusRun)?.n : undefined;
  useEffect(() => {
    if (!focusN) return;
    const frame = requestAnimationFrame(() => document.getElementById(`trturn-${focusN}`)?.scrollIntoView({ block: "start" }));
    return () => cancelAnimationFrame(frame);
  }, [focusN]);

  // Esc：先清掉概览上的区间，再关详情
  const { range, selected, setRange, select } = ui;
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || event.defaultPrevented) return;
      if (range) setRange(null);
      else if (selected) select(null);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [range, selected, setRange, select]);

  const turnsAll = trace.turns > 0 && [...trace.runs.values()].every((r) => ui.turnsClosed.has(r.n));
  const folds = trace.rows.filter((r) => r.kind === "model" && r.calls.length);
  const callsAll = folds.length > 0 && folds.every((r) => ui.callsClosed.has(r.id));

  const pick = (id: string) => {
    ui.select(id);
    requestAnimationFrame(() => requestAnimationFrame(() => document.getElementById(`tr-${id}`)?.scrollIntoView({ block: "nearest", behavior: "smooth" })));
  };

  return (
    <div className="pt-1">
      <div className={cn("mb-1.5 flex items-center gap-3 max-[600px]:flex-wrap", compact && "flex-wrap gap-y-1.5")}>
        <div role="toolbar" aria-label={m.trace_toolbar()} className="flex gap-0.5">
          <ToolButton
            icon="clock"
            pressed={ui.byDuration}
            title={ui.byDuration ? m.trace_tool_duration_equal() : m.trace_tool_duration_real()}
            onClick={ui.toggleDuration}
          >
            {m.trace_tool_duration()}
          </ToolButton>
          <ToolButton
            icon={turnsAll ? "plus" : "list"}
            pressed={turnsAll}
            title={turnsAll ? m.trace_tool_turns_expand() : m.trace_tool_turns_collapse()}
            onClick={() => ui.setTurnsClosed(turnsAll ? new Set() : new Set([...trace.runs.values()].map((r) => r.n)))}
          >
            {m.trace_tool_turns()}
          </ToolButton>
          <ToolButton
            icon={callsAll ? "plus" : "list"}
            pressed={callsAll}
            title={callsAll ? m.trace_tool_calls_expand() : m.trace_tool_calls_collapse()}
            onClick={() => ui.setCallsClosed(callsAll ? new Set() : new Set(folds.map((r) => r.id)))}
          >
            {m.trace_toolbar_calls()}
          </ToolButton>
        </div>
        <label className={cn("ml-auto flex h-7.5 w-55 items-center gap-1.5 rounded-control px-2.5 text-faint shadow-ring-control focus-within:shadow-[0_0_0_1px_var(--color-muted)] max-[600px]:w-full", compact && "ml-0 w-full")}>
          <Icon name="search" className="size-3.5" />
          <span className="sr-only">{m.trace_search_label()}</span>
          <input
            type="search"
            value={ui.query}
            onChange={(event) => ui.setQuery(event.target.value)}
            placeholder={m.trace_search_placeholder()}
            className="w-full min-w-0 bg-transparent text-label text-text outline-none"
          />
        </label>
      </div>
      <TraceOverview
        rows={trace.rows}
        model={overview}
        hits={hits}
        range={ui.range}
        selected={ui.selected}
        byDuration={ui.byDuration}
        onSelect={pick}
        onRange={ui.setRange}
      />
      <div
        className={cn(
          "grid items-start gap-4 pl-5.5 max-[600px]:pl-4.5",
          compact ? "grid-cols-1" : picked ? "grid-cols-[minmax(0,1fr)_minmax(320px,400px)] max-[900px]:grid-cols-1" : "grid-cols-1",
        )}
      >
        {compact && picked && (
          <aside aria-label={m.trace_detail_aside()} className="-ml-5.5 flex max-h-[60vh] flex-col overflow-hidden rounded-card bg-surface shadow-lift max-[600px]:-ml-4.5">
            <TraceDetail key={picked.id} row={picked} trace={trace} ui={ui} />
          </aside>
        )}
        <RecordList trace={trace} model={overview} hits={hits} ui={ui} compact={compact} />
        {!compact && picked && (
          <aside
            aria-label={m.trace_detail_aside()}
            className="sticky top-2 mt-2 flex max-h-[calc(100vh-220px)] flex-col overflow-hidden rounded-card bg-surface shadow-lift max-[900px]:fixed max-[900px]:inset-x-0 max-[900px]:top-auto max-[900px]:bottom-0 max-[900px]:z-20 max-[900px]:mt-0 max-[900px]:max-h-[70vh] max-[900px]:rounded-b-none max-[900px]:rounded-t-[16px] max-[900px]:shadow-[0_0_0_1px_rgb(0_0_0/0.12),0_-8px_24px_rgb(0_0_0/0.12)]"
          >
            <TraceDetail key={picked.id} row={picked} trace={trace} ui={ui} />
          </aside>
        )}
      </div>
    </div>
  );
}

function ToolButton({ icon, pressed, title, onClick, children }: { icon: IconName; pressed: boolean; title: string; onClick: () => void; children: string }) {
  return (
    <button
      type="button"
      aria-pressed={pressed}
      title={title}
      onClick={onClick}
      className="inline-flex h-7 cursor-pointer items-center gap-[5px] rounded-control px-2 text-label text-muted hover:bg-hover-on-white hover:text-text aria-pressed:bg-active aria-pressed:text-text"
    >
      <Icon name={icon} className="size-3.5" />
      {children}
    </button>
  );
}
