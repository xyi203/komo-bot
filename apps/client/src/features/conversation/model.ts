import type { WireEvent } from "@komo/protocol";
import {
  payload,
  thinkingOf,
  type AssistantPayload,
  type MemoryUse,
  type RunPayload,
  type ToolCallPayload,
  type ToolResultPayload,
} from "./events";

// 会话正文 = 事件流的 fold：一轮 = 一条 Run，用户的话在上，过程（召回、过程中的说明、工具调用、插话）
// 收进"用时"那一行，最终回答在下。纯函数，输入是已按 seq 排好的事件，输出每次重算（刷新即重建）。

export type CallStatus = "planned" | "running" | "completed" | "failed" | "uncertain";

export interface Call {
  id: string;
  name: string;
  args: Record<string, unknown>;
  status: CallStatus | string;
  startedAt?: string;
  ms?: number;
  preview?: string;
  /** 结果事件的 id：对话里"查看"用它跳到轨迹里这一条。 */
  resultEvent?: string;
}

export type ProcessItem =
  | { kind: "steer"; id: string; text: string }
  | { kind: "recall"; id: string; uses: MemoryUse[] }
  | { kind: "text"; id: string; text: string }
  | { kind: "think"; id: string; text: string }
  | { kind: "tools"; id: string; calls: Call[] };

export type TurnStatus = "queued" | "running" | "done" | "failed" | "cancelled";

export interface Turn {
  kind: "turn";
  id: string;
  run: string;
  users: { text: string; at: string }[];
  process: ProcessItem[];
  answer: { id: string; text: string; at: string } | null;
  start: string;
  end: string | null;
  status: TurnStatus;
  recall: MemoryUse[];
  reason?: string;
}

export type Item =
  | Turn
  | { kind: "user"; id: string; text: string; at: string }
  /** 投进对话的一句话（`line.reply`，不挂 Run）：停住的运行的提问、定时的结果。 */
  | { kind: "reply"; id: string; text: string; at: string; intervention?: string }
  | { kind: "note"; id: string };

export interface View {
  items: Item[];
  turns: Map<string, Turn>;
  calls: Map<string, Call>;
  /** 还没进终态的 Run。 */
  runs: Map<string, { since: string; waiting: boolean }>;
  stats: { output: number; input: number; cache: number; steps: number };
  /** 最近一次 `memory.injected` 带上的记忆（右栏"带上的记忆"）；那一次什么都没带就是空。 */
  recall: MemoryUse[];
  /** 上一次请求实报的 `input_tokens`（上下文环与右栏"上下文"）。 */
  usage: number | null;
  /** 最近一次 `config.changed` 的模型。 */
  model: string | null;
}

export function foldEvents(events: readonly WireEvent[]): View {
  const view: View = {
    items: [],
    turns: new Map(),
    calls: new Map(),
    runs: new Map(),
    stats: { output: 0, input: 0, cache: 0, steps: 0 },
    recall: [],
    usage: null,
    model: null,
  };
  for (const event of events) apply(view, event);
  return view;
}

function turnOf(view: View, event: WireEvent): Turn | null {
  if (!event.run_id) return null;
  let turn = view.turns.get(event.run_id);
  if (!turn) {
    turn = {
      kind: "turn",
      id: `t-${event.run_id}`,
      run: event.run_id,
      users: [],
      process: [],
      answer: null,
      start: event.at,
      end: null,
      status: "queued",
      recall: [],
    };
    view.turns.set(event.run_id, turn);
    view.items.push(turn);
  }
  return turn;
}

function apply(view: View, event: WireEvent): void {
  const turn = turnOf(view, event);
  switch (event.type) {
    case "message.user": {
      const { text = "" } = payload<{ text: string }>(event);
      if (turn && turn.status === "running" && turn.users.length) turn.process.push({ kind: "steer", id: event.event_id, text });
      else if (turn) turn.users.push({ text, at: event.at });
      else view.items.push({ kind: "user", id: event.event_id, text, at: event.at });
      break;
    }
    case "line.reply": {
      // Router 时代的展示消息，现在是 Komo 往对话里投一句话的载体（docs/redesign.md §2.1）；
      // 停住的运行的提问带 `intervention`（介入句柄），这句话下面放答复按钮
      const { text = "", intervention } = payload<{ text: string; intervention?: string }>(event);
      if (text.trim()) view.items.push({ kind: "reply", id: event.event_id, text, at: event.at, intervention });
      break;
    }
    case "memory.injected": {
      const uses = payload<{ uses: MemoryUse[] }>(event).uses ?? [];
      view.recall = uses;
      if (turn && uses.length) {
        turn.recall = uses;
        turn.process.unshift({ kind: "recall", id: event.event_id, uses });
      }
      break;
    }
    case "message.assistant": {
      const d = payload<AssistantPayload>(event);
      view.stats.output += d.output_tokens ?? 0;
      view.stats.cache += d.cache_read_tokens ?? 0;
      view.stats.input += d.input_tokens ?? 0;
      if (d.input_tokens) view.usage = d.input_tokens;
      if (!turn) break;
      // 上一段"回答"后面又来了一轮：它其实是过程中的说明，挪回过程里
      if (turn.answer) {
        turn.process.push({ kind: "text", id: turn.answer.id, text: turn.answer.text });
        turn.answer = null;
      }
      const calls: ToolCallPayload[] = d.tool_calls ?? [];
      const thought = thinkingOf(d.provider_blocks);
      if (thought) turn.process.push({ kind: "think", id: `${event.event_id}:think`, text: thought });
      if (d.text && d.text.trim()) {
        if (calls.length) turn.process.push({ kind: "text", id: event.event_id, text: d.text });
        else turn.answer = { id: event.event_id, text: d.text, at: event.at };
      }
      for (const tc of calls) {
        let group = turn.process[turn.process.length - 1];
        if (!group || group.kind !== "tools") {
          group = { kind: "tools", id: `g-${tc.call_id}`, calls: [] };
          turn.process.push(group);
        }
        const call: Call = { id: tc.call_id, name: tc.name, args: tc.arguments ?? {}, status: "planned" };
        group.calls.push(call);
        view.calls.set(tc.call_id, call);
        view.stats.steps++;
      }
      break;
    }
    case "tool.started": {
      const call = view.calls.get(payload<{ call_id: string }>(event).call_id ?? "");
      if (call) {
        call.status = "running";
        call.startedAt = event.at;
      }
      break;
    }
    case "tool.result": {
      const d = payload<ToolResultPayload>(event);
      const call = view.calls.get(d.call_id ?? "");
      if (call) {
        call.status = d.status ?? "completed";
        call.ms = d.elapsed_ms;
        call.preview = d.preview;
        call.resultEvent = event.event_id;
      }
      break;
    }
    case "run.started":
      if (event.run_id) view.runs.set(event.run_id, { since: event.at, waiting: false });
      if (turn) {
        turn.status = "running";
        turn.start = event.at;
      }
      break;
    case "run.waiting": {
      const run = event.run_id ? view.runs.get(event.run_id) : undefined;
      if (run) run.waiting = true;
      break;
    }
    case "run.absorbed": {
      // 插话：这条排队的输入被正在跑的那一轮并了进去，话挪到宿主那一轮的过程里
      const host = view.turns.get(payload<RunPayload>(event).into ?? "");
      if (turn && host) {
        for (const user of turn.users) host.process.push({ kind: "steer", id: `${turn.id}-${user.at}`, text: user.text });
        view.items.splice(view.items.indexOf(turn), 1);
        view.turns.delete(turn.run);
      }
      break;
    }
    case "run.completed":
    case "run.abandoned":
      if (event.run_id) view.runs.delete(event.run_id);
      if (turn) {
        turn.status = "done";
        turn.end = event.at;
      }
      break;
    case "run.failed":
      if (event.run_id) view.runs.delete(event.run_id);
      if (turn) {
        turn.status = "failed";
        turn.end = event.at;
        turn.reason = payload<RunPayload>(event).reason;
      }
      break;
    case "run.cancelled":
      if (event.run_id) view.runs.delete(event.run_id);
      if (turn) {
        turn.status = "cancelled";
        turn.end = event.at;
      }
      break;
    case "context.compacted":
      // 只有真换成摘要才标；`decayed`（旧工具结果换短视图）与 `skipped`（没压）不是给人看的边界
      if (payload<{ outcome?: string }>(event).outcome === "compacted") view.items.push({ kind: "note", id: event.event_id });
      break;
    case "config.changed": {
      const { model } = payload<{ model: string }>(event);
      if (model) view.model = model;
      break;
    }
  }
}
