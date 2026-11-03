import type { WireEvent } from "@komo/protocol";
import type { MemoryBrief, SessionTree } from "@komo/api";
import { formatList } from "@/lib/format";
import { m } from "@/paraglide/messages.js";
import { payload, thinkingOf, type AssistantPayload, type MemoryUse, type RunPayload, type ToolResultPayload } from "../conversation/events";

// 轨迹：同一份事件加上树上的 `komo.system` 摊平成记录，照 DeepSeek Harness 的 Trajectory。
// 记录是：系统（初始系统提示词、之后的更新）、用户、上下文（召回、压缩、失败）、思考、助手（只调用工具时写"（仅工具调用）"）、
// 工具（`名字 参数 → 输出`，缩进挂在发起它的那一步下）。

export interface Tokens {
  input?: number;
  cache?: number;
  output?: number;
  reasoning?: number;
}

interface RowBase {
  id: string;
  /** 第几轮（会话开始前的系统提示没有）。 */
  n: number | undefined;
  at?: string;
  end?: string;
  /** 在记录表里的序号。 */
  idx: number;
}

export interface UserRow extends RowBase {
  kind: "user";
  text: string;
}
export interface CtxRow extends RowBase {
  kind: "ctx";
  text: string;
  bad?: boolean;
}
export interface ThinkRow extends RowBase {
  kind: "think";
  text: string;
  step: number;
  parent: string;
}
export interface ModelRow extends RowBase {
  kind: "model";
  text: string;
  /** 只调用了工具、没有正文。 */
  plain: boolean;
  body: string;
  thought: string;
  step: number;
  ms: number;
  tokens: Tokens;
  cum: Tokens;
  /** 它派出的调用落在哪一行（详情里能点过去）。 */
  calls: { row: string | undefined; name: string }[];
}
export interface ToolRow extends RowBase {
  kind: "tool";
  name: string;
  args: Record<string, unknown>;
  step: number;
  parent: string;
  ms?: number;
  preview: string;
  status: string;
  bad: boolean;
  live?: boolean;
  /** 没有正文的行也给 text 一个值，免得到处判空。 */
  text: string;
}
export interface SysRow extends RowBase {
  kind: "sys";
  text: string;
  first: boolean;
  changed: [string, string | null][];
  sections: Record<string, string>;
  tools: ToolDef[];
  added: string[];
  removed: string[];
}

export type TraceRow = UserRow | CtxRow | ThinkRow | ModelRow | ToolRow | SysRow;

type Draft<T> = T extends unknown ? Omit<T, "idx"> : never;

export interface ToolDef {
  name?: string;
  description?: string;
  parameters?: unknown;
  function?: { name?: string; description?: string; parameters?: unknown };
}

export interface RunSpan {
  start: string;
  end: string | null;
  n: number;
}

export interface Trace {
  rows: TraceRow[];
  runs: Map<string, RunSpan>;
  turns: number;
  calls: number;
  cum: Tokens;
  /** 还在等模型回话的那几轮：时间概览的模型泳道上多一格进行中的。 */
  waiting: { from: string }[];
  schema: Map<string, ToolDef>;
}

const ms = (iso: string) => Date.parse(iso);

type Section = string | null;
interface SystemModel {
  sections?: Record<string, Section>;
  toolsAdded?: ToolDef[];
  toolsRemoved?: ToolDef[];
}

const toolName = (t: ToolDef) => t.name ?? t.function?.name ?? "";

/** 系统提示从树上取：komo.system 是增量，第一条是初始的整份，之后每条只带改了的段。 */
function systemRows(tree: SessionTree | undefined): { rows: SysRow[]; tools: Map<string, ToolDef> } {
  const tools = new Map<string, ToolDef>();
  if (!tree?.entries.length) return { rows: [], tools };
  const byId = new Map(tree.entries.map((e) => [e.id, e]));
  const path = [];
  for (let id = tree.leafId; id && byId.has(id); id = byId.get(id)!.parentId) path.push(byId.get(id)!);
  path.reverse();
  const rows: SysRow[] = [];
  const sections: Record<string, string> = {};
  for (const entry of path) {
    if (entry.customType !== "komo.system") continue;
    const model = ((entry.data as { model?: SystemModel[] } | undefined)?.model?.[0] ?? {}) as SystemModel;
    const changed = Object.entries(model.sections ?? {});
    for (const [key, value] of changed) {
      if (value == null) delete sections[key];
      else sections[key] = value;
    }
    const added = (model.toolsAdded ?? []).map(toolName);
    const removed = (model.toolsRemoved ?? []).map(toolName);
    for (const tool of model.toolsAdded ?? []) tools.set(toolName(tool), tool);
    for (const name of removed) tools.delete(name);
    const first = rows.length === 0;
    const what = first
      ? m.trace_sys_initial()
      : changed.length && (added.length || removed.length)
        ? m.trace_sys_both()
        : changed.length
          ? m.trace_sys_prompt()
          : m.trace_sys_tools();
    const toolNote: string[] = [];
    if (added.length) toolNote.push(first ? m.trace_sys_tool_count({ count: added.length }) : m.trace_sys_added({ names: formatList(added) }));
    if (removed.length) toolNote.push(m.trace_sys_removed({ names: formatList(removed) }));
    rows.push({
      kind: "sys",
      id: `sys-${entry.id}`,
      n: undefined,
      idx: 0,
      at: entry.timestamp,
      first,
      text: [what, ...(first ? [] : changed.map(([key]) => key)), ...toolNote].join(" · "),
      changed: changed as [string, string | null][],
      sections: { ...sections },
      tools: [...tools.values()],
      added,
      removed,
    });
  }
  return { rows, tools };
}

interface CallRecord {
  name: string;
  args: Record<string, unknown>;
  start: string;
  run: string | null;
  parent: string;
  step: number;
  done?: string;
}

/**
 * 事件 + 会话树 → 记录。`liveRuns` 是还没进终态的 Run（对话那边 fold 出来的同一份），决定哪些调用算"进行中"。
 */
export function traceOf(
  events: readonly WireEvent[],
  liveRuns: ReadonlyMap<string, unknown>,
  tree: SessionTree | undefined,
  memories: ReadonlyMap<string, MemoryBrief>,
): Trace {
  const rows: Draft<TraceRow>[] = [];
  const calls = new Map<string, CallRecord>();
  const last = new Map<string | null, string>();
  const runs = new Map<string, RunSpan>();
  const byRun = new Map<string, { n: number; step: number }>();
  let turn = 0;
  const step = (run: string | null) => {
    const r = run ? byRun.get(run) : undefined;
    return r ? ++r.step : 0;
  };
  const cum: Tokens = { input: 0, cache: 0, output: 0, reasoning: 0 };

  for (const e of events) {
    const run = e.run_id ?? null;
    const n = run ? byRun.get(run)?.n : undefined;
    switch (e.type) {
      case "run.started":
        if (!run) break;
        turn++;
        runs.set(run, { start: e.at, end: null, n: turn });
        byRun.set(run, { n: turn, step: 0 });
        last.set(run, e.at);
        break;
      case "run.completed":
      case "run.failed":
      case "run.cancelled":
      case "run.abandoned": {
        const span = run ? runs.get(run) : undefined;
        if (span) span.end = e.at;
        if (e.type === "run.failed") {
          rows.push({ kind: "ctx", id: e.event_id, n, at: e.at, bad: true, text: m.trace_run_failed({ reason: payload<RunPayload>(e).reason ?? m.trace_no_reason() }) });
        }
        if (e.type === "run.cancelled") rows.push({ kind: "ctx", id: e.event_id, n, at: e.at, text: m.trace_run_cancelled() });
        break;
      }
      case "message.user":
        rows.push({ kind: "user", id: e.event_id, n, at: e.at, text: payload<{ text: string }>(e).text ?? "" });
        last.set(run, e.at);
        break;
      case "memory.injected": {
        const uses = payload<{ uses: MemoryUse[] }>(e).uses ?? [];
        if (uses.length) {
          rows.push({
            kind: "ctx",
            id: e.event_id,
            n,
            at: e.at,
            text: m.trace_recall({ count: uses.length, list: uses.map((u) => memories.get(u.memory)?.content ?? u.memory).join("；") }),
          });
        }
        break;
      }
      case "context.compacted":
        rows.push({ kind: "ctx", id: e.event_id, n, at: e.at, text: m.trace_compacted() });
        break;
      case "message.assistant": {
        const d = payload<AssistantPayload>(e);
        const from = last.get(run) ?? e.at;
        const thought = thinkingOf(d.provider_blocks);
        const tokens: Tokens = { input: d.input_tokens, cache: d.cache_read_tokens, output: d.output_tokens, reasoning: d.reasoning_tokens };
        for (const k of Object.keys(cum) as (keyof Tokens)[]) cum[k] = (cum[k] ?? 0) + (tokens[k] ?? 0);
        const s = step(run);
        if (thought) rows.push({ kind: "think", id: `${e.event_id}:think`, n, step: s, at: from, end: e.at, parent: e.event_id, text: thought });
        const callIds: string[] = [];
        for (const tc of d.tool_calls ?? []) {
          calls.set(tc.call_id, { name: tc.name, args: tc.arguments ?? {}, start: e.at, run, parent: e.event_id, step: s });
          callIds.push(tc.call_id);
        }
        const body = d.text ?? "";
        rows.push({
          kind: "model",
          id: e.event_id,
          n,
          step: s,
          at: from,
          end: e.at,
          ms: ms(e.at) - ms(from),
          text: body.trim() || m.trace_tool_only(),
          plain: !body.trim(),
          body,
          thought,
          tokens,
          cum: { ...cum },
          // 先记调用 id，收尾时换成它们落在哪一行
          calls: callIds.map((id) => ({ row: id, name: calls.get(id)!.name })),
        });
        last.set(run, e.at);
        break;
      }
      case "tool.started": {
        const c = calls.get(payload<{ call_id: string }>(e).call_id ?? "");
        if (c) c.start = e.at;
        break;
      }
      case "tool.result": {
        const d = payload<ToolResultPayload>(e);
        const c = calls.get(d.call_id ?? "");
        if (!c) break;
        c.done = e.event_id;
        const status = d.status ?? "completed";
        rows.push({
          kind: "tool",
          id: e.event_id,
          n: c.run ? byRun.get(c.run)?.n : undefined,
          step: c.step,
          at: c.start,
          end: e.at,
          ms: d.elapsed_ms,
          name: c.name,
          args: c.args,
          parent: c.parent,
          preview: d.preview ?? "",
          status,
          bad: status === "failed" || status === "uncertain",
          text: "",
        });
        last.set(c.run, e.at);
        break;
      }
    }
  }
  for (const [id, c] of calls) {
    if (!c.done && c.run && liveRuns.has(c.run)) {
      rows.push({
        kind: "tool", id: `live-${id}`, n: byRun.get(c.run)?.n, step: c.step, at: c.start, name: c.name, args: c.args,
        parent: c.parent, preview: "", status: "running", bad: false, live: true, text: "",
      });
    }
  }
  // 模型行记下它派出的调用落在哪一行，详情里能点过去
  const toolRow = new Map([...calls].map(([id, c]) => [id, c.done ?? `live-${id}`]));
  for (const r of rows) {
    if (r.kind === "model") r.calls = r.calls.map((c) => ({ row: toolRow.get(c.row ?? ""), name: c.name }));
  }
  // 系统提示：初始那份排在最前，之后的更新按时间插进去
  const sys = systemRows(tree);
  for (const s of sys.rows) {
    if (s.first) {
      rows.unshift(s);
      continue;
    }
    const i = rows.findIndex((r) => r.at && ms(r.at) > ms(s.at!));
    if (i < 0) rows.push(s);
    else rows.splice(i, 0, s);
  }
  const indexed = rows.map((r, idx) => ({ ...r, idx })) as TraceRow[];
  const waiting = [...runs]
    .filter(([run, r]) => !r.end && liveRuns.has(run) && ![...calls.values()].some((c) => c.run === run && !c.done))
    .map(([run]) => ({ from: last.get(run) ?? "" }));
  return { rows: indexed, runs, turns: turn, calls: calls.size, cum, waiting, schema: sys.tools };
}

// ---- 时间概览 ----

export type Lane = 0 | 1 | 2;
const LANE: Record<TraceRow["kind"], Lane> = { user: 0, ctx: 0, sys: 0, think: 1, model: 1, tool: 2 };

export interface Span {
  row?: TraceRow;
  lane: Lane;
  start: number;
  end: number;
  live?: boolean;
}
export interface Overview {
  spans: Span[];
  start: number;
  end: number;
}

/**
 * 概览的定义域：等宽时第 i 条记录占 [i, i+1)；按时长时是真实起止，记录之间没人在跑的空隙抠掉。
 */
export function traceSpans(trace: Trace, byDuration: boolean, now: number): Overview {
  const timed = trace.rows
    .map((row) => ({ row, a: row.at ? ms(row.at) : NaN, b: row.end ? ms(row.end) : row.kind === "tool" && row.live ? now : row.at ? ms(row.at) : NaN }))
    .filter((x) => Number.isFinite(x.a));
  if (!byDuration) {
    const spans: Span[] = timed.map((x, i) => ({ row: x.row, lane: LANE[x.row.kind], start: i, end: i + 1 }));
    trace.waiting.forEach((_, i) => spans.push({ lane: 1, start: spans.length + i, end: spans.length + i + 1, live: true }));
    return { spans, start: 0, end: Math.max(spans.length, 1) };
  }
  type Entry = { row?: TraceRow; a: number; b: number; live?: boolean };
  const all: Entry[] = [...timed, ...trace.waiting.map((w): Entry => ({ a: ms(w.from), b: now, live: true }))].sort((x, y) => x.a - y.a || x.b - y.b);
  let removed = 0;
  let covered: number | null = null;
  const cut = new Map<Entry, number>();
  for (const x of all) {
    if (covered !== null && x.a > covered) removed += x.a - covered;
    cut.set(x, removed);
    covered = covered === null ? x.b : Math.max(covered, x.b);
  }
  const spans: Span[] = all.map((x) => ({
    row: x.row,
    lane: x.row ? LANE[x.row.kind] : 1,
    live: x.live || (x.row?.kind === "tool" && x.row.live),
    start: x.a - cut.get(x)!,
    end: x.b - cut.get(x)!,
  }));
  const start = spans.length ? Math.min(...spans.map((s) => s.start)) : 0;
  const end = Math.max(...spans.map((s) => s.end), start + 1);
  return { spans, start, end };
}

// ---- 记录的一行文字、搜索 ----

export function traceText(row: TraceRow): string {
  if (row.kind === "tool") return `${row.name} ${JSON.stringify(row.args)}`;
  // 一行里不排版，去掉 Markdown 记号
  return row.text.replace(/\*\*|`|^#+\s/gm, "").replace(/\s+/g, " ");
}

/** 工具结果预览的正文（去掉 `[名字 · 状态 · 耗时]` 那一行和 `exit=`）。 */
export const resultLine = (row: ToolRow) =>
  row.preview
    .split("\n")
    .slice(1)
    .filter((l) => !/^exit=/.test(l))
    .join(" ")
    .replace(/\s+/g, " ")
    .trim();

export function searchHits(trace: Trace, query: string): Set<string> | null {
  const q = query.trim().toLowerCase();
  if (!q) return null;
  return new Set(
    trace.rows
      .filter((r) => `${traceText(r)} ${r.kind === "tool" ? resultLine(r) : ""}`.toLowerCase().includes(q))
      .map((r) => r.id),
  );
}
