import { Icon, type IconName } from "@/components/icon";
import { formatElapsed, formatMs, formatStamp, formatTokens } from "@/lib/format";
import { formatDuration } from "@/features/conversation/duration";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { resultLine, traceText, type Overview, type Trace, type TraceRow } from "./model";
import type { TraceUi } from "./state";

const KIND_ICON: Record<TraceRow["kind"], IconName> = { user: "chat", think: "think", model: "leaf", tool: "term", ctx: "list", sys: "gear" };
export const kindLabel = (kind: TraceRow["kind"]) =>
  ({ user: m.trace_kind_user(), think: m.trace_kind_think(), model: m.trace_kind_model(), tool: m.trace_kind_tool(), ctx: m.trace_kind_ctx(), sys: m.trace_kind_sys() })[kind];

export function KindIcon({ row, className }: { row: TraceRow; className?: string }) {
  const bad = (row.kind === "tool" || row.kind === "ctx") && row.bad;
  return (
    <span
      title={kindLabel(row.kind)}
      className={cn(
        "grid size-5.5 shrink-0 place-items-center rounded-sm bg-raised text-muted",
        row.kind === "user" && "bg-text text-surface",
        row.kind === "model" && "bg-active text-text",
        (row.kind === "think" || row.kind === "sys" || row.kind === "ctx") && "bg-transparent shadow-ring",
        bad && "bg-pill-bad-bg text-pill-bad-ink shadow-none",
        className,
      )}
    >
      <Icon name={KIND_ICON[row.kind]} className="size-[13px]" />
    </span>
  );
}

/** 记录表的列：图标与内容占一列，后面是输入 / 输出 / 思考 token 与时间。详情开着时 token 三列收起，只留时间。 */
const COLS = "grid grid-cols-[minmax(0,1fr)_52px_52px_52px_64px] items-center gap-2";
const COLS_NARROW = "grid-cols-[minmax(0,1fr)_0_0_0_56px] gap-x-1.5 gap-y-0";
/** 窄屏（≤900）同样只留时间一列。 */
const COLS_SMALL = "max-[900px]:grid-cols-[minmax(0,1fr)_0_0_0_56px] max-[900px]:gap-x-1.5";

/**
 * 记录表：按轮分段，轮头吸顶（时间、用时，收起时写几个步骤几个工具调用）。
 */
export function RecordList({
  trace,
  model,
  hits,
  ui,
  compact = false,
}: {
  trace: Trace;
  model: Overview;
  hits: Set<string> | null;
  ui: TraceUi;
  /** 窄的容器（过程面板）：token 三列一直收起，只留时间。 */
  compact?: boolean;
}) {
  const { range } = ui;
  const narrow = compact || ui.selected !== null;
  const inRange = range
    ? new Set(model.spans.filter((s) => s.row && s.end >= range[0] && s.start <= range[1]).map((s) => s.row!.id))
    : null;
  const visible = trace.rows.filter((r) => (!hits || hits.has(r.id)) && (!inRange || inRange.has(r.id)));
  // 收起的调用：父那一步收起了，它的工具行不列（搜索和拖选时照列，免得命中的看不见）
  const shown = hits || inRange ? visible : visible.filter((r) => !(r.kind === "tool" && ui.callsClosed.has(r.parent)));

  const out: React.ReactNode[] = [];
  let current: number | undefined;
  for (const row of shown) {
    const n = row.n ?? 0;
    if (n !== current) {
      current = n;
      if (n) out.push(<TurnHeader key={`turn-${n}`} n={n} trace={trace} ui={ui} narrow={narrow} />);
    }
    if (n && ui.turnsClosed.has(n) && !hits && !inRange) continue;
    out.push(<RecordRow key={row.id} row={row} ui={ui} narrow={narrow} />);
  }

  return (
    <div className="pb-4">
      {range && inRange && (
        <p className="mt-2 text-small text-muted">
          {m.trace_range_note({ count: inRange.size })} ·{" "}
          <button type="button" className="cursor-pointer font-medium text-text underline underline-offset-3" onClick={() => ui.setRange(null)}>
            {m.trace_range_clear()}
          </button>
        </p>
      )}
      {out.length ? out : <p className="mb-3.5 text-label text-muted">{m.trace_no_match()}</p>}
    </div>
  );
}

function TurnHeader({ n, trace, ui, narrow }: { n: number; trace: Trace; ui: TraceUi; narrow: boolean }) {
  const run = [...trace.runs.values()].find((x) => x.n === n);
  const closed = ui.turnsClosed.has(n);
  const inTurn = trace.rows.filter((x) => x.n === n);
  const summary = closed
    ? ` · ${m.trace_turn_summary({ steps: inTurn.filter((x) => x.kind === "model").length, calls: inTurn.filter((x) => x.kind === "tool").length })}`
    : "";
  return (
    <div
      id={`trturn-${n}`}
      className={cn(
        COLS,
        narrow && COLS_NARROW,
        COLS_SMALL,
        "sticky top-0 z-2 mt-1 min-h-8.5 bg-surface px-1 text-small text-faint shadow-[inset_0_-1px_0_var(--color-border)]",
      )}
    >
      <button
        type="button"
        aria-expanded={!closed}
        onClick={() => ui.toggleTurn(n)}
        className="flex min-w-0 cursor-pointer items-center gap-1.5 text-left text-label font-medium text-text"
      >
        <Icon name="down" className={cn("size-3 text-faint transition-transform", closed && "-rotate-90")} />
        <span className="shrink-0 whitespace-nowrap">{m.trace_turn({ n })}</span>
        <span className="truncate text-small font-normal text-faint tabular-nums">
          {run ? `${formatStamp(run.start)}${run.end ? ` · ${formatDuration(Date.parse(run.end) - Date.parse(run.start))}` : ` · ${m.trace_turn_running()}`}` : ""}
          {summary}
        </span>
      </button>
      <Col token narrow={narrow}>{m.trace_col_input()}</Col>
      <Col token narrow={narrow}>{m.trace_col_output()}</Col>
      <Col token narrow={narrow}>{m.trace_col_think()}</Col>
      <Col>{m.trace_col_time()}</Col>
    </div>
  );
}

function Col({ children, token = false, narrow = false }: { children?: React.ReactNode; token?: boolean; narrow?: boolean }) {
  return (
    <span className={cn("text-right text-small whitespace-nowrap text-faint tabular-nums", token && (narrow ? "invisible" : "max-[900px]:invisible"))}>
      {children}
    </span>
  );
}

const tok = (n: number | undefined) => (n ? formatTokens(n) : "");

function RecordRow({ row, ui, narrow }: { row: TraceRow; ui: TraceUi; narrow: boolean }) {
  const selected = row.id === ui.selected;
  const bad = (row.kind === "tool" || row.kind === "ctx") && row.bad;
  const closedCalls = row.kind === "model" && ui.callsClosed.has(row.id);

  let text: React.ReactNode;
  if (row.kind === "tool") {
    text = (
      <>
        <span className="max-w-[42%] min-w-0 overflow-hidden font-mono text-small text-ellipsis whitespace-nowrap text-text">
          {row.name} {JSON.stringify(row.args)}
        </span>
        <span aria-hidden="true" className="shrink-0 text-small text-faint">→</span>
        <span className="min-w-0 flex-1 overflow-hidden font-mono text-small text-ellipsis whitespace-nowrap text-muted">
          {row.live ? m.trace_running() : resultLine(row) || m.trace_no_output()}
        </span>
      </>
    );
  } else {
    text = (
      <span className={cn("min-w-0 overflow-hidden text-label text-ellipsis whitespace-nowrap text-text", row.kind === "model" && row.plain && "text-muted")}>
        {traceText(row)}
      </span>
    );
  }

  const cols =
    row.kind === "model"
      ? [tok((row.tokens.input ?? 0) + (row.tokens.cache ?? 0)), tok(row.tokens.output), tok(row.tokens.reasoning), formatMs(row.ms)]
      : row.kind === "tool"
        ? ["", "", "", row.live && row.at ? formatElapsed(row.at) : row.bad ? (row.status === "failed" ? m.trace_failed() : m.trace_uncertain_short()) : formatMs(row.ms)]
        : ["", "", "", ""];

  return (
    <div
      id={`tr-${row.id}`}
      className={cn("relative shadow-[inset_0_-1px_0_var(--color-rule)]", selected && "bg-hover-on-white shadow-[inset_2px_0_0_var(--color-text),inset_0_-1px_0_var(--color-rule)]")}
    >
      <button
        type="button"
        aria-pressed={selected}
        onClick={() => ui.select(row.id)}
        className={cn(COLS, narrow && COLS_NARROW, COLS_SMALL, "min-h-8.5 w-full cursor-pointer bg-transparent px-1 text-left hover:bg-hover-on-white")}
      >
        <span className={cn("flex min-w-0 items-center gap-2", row.kind === "tool" && "pl-4.5")}>
          <KindIcon row={row} />
          {text}
          {closedCalls && <span className="shrink-0 text-small text-faint">{m.trace_calls_folded({ count: row.calls.length })}</span>}
        </span>
        {cols.map((c, i) => (
          <span
            key={i}
            className={cn(
              "text-right text-small whitespace-nowrap text-faint tabular-nums",
              i === 3 && bad && "text-negative",
              i < 3 && (narrow ? "invisible" : "max-[900px]:invisible"),
            )}
          >
            {c}
          </span>
        ))}
      </button>
      {row.kind === "model" && row.calls.length > 0 && (
        <button
          type="button"
          aria-expanded={!closedCalls}
          aria-label={closedCalls ? m.trace_calls_expand() : m.trace_calls_collapse()}
          onClick={() => ui.toggleCall(row.id)}
          className="absolute top-[7px] -left-5.5 grid size-5 cursor-pointer place-items-center rounded-xs text-faint hover:bg-hover-on-white hover:text-text max-[600px]:-left-5"
        >
          <Icon name="down" className={cn("size-3 transition-transform", closedCalls && "-rotate-90")} />
        </button>
      )}
    </div>
  );
}
