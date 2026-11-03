import type { Call, ProcessItem } from "./model";

// 过程分组照 DeepSeek Harness 的 Chat（ui-chat/conversation-nodes）：同一轮里相邻的思考与工具调用并成一组，
// 回复、插话把组截断；回复始终可见，收起一轮只收过程。组标题按工具类别次数排序取前三。

export type Category = "thinking" | "read" | "search" | "write" | "edit" | "commands" | "code" | "subagents" | "tools";

const TOOL_CATEGORY: Record<string, Category> = {
  bash: "commands",
  read: "read",
  rg: "search",
  write: "write",
  edit: "edit",
  codemode: "code",
  task: "subagents",
};

export const categoryOf = (name: string): Category => TOOL_CATEGORY[name] ?? "tools";

export const callBusy = (call: Call) => call.status === "running" || call.status === "planned";
export const callBad = (call: Call) => call.status === "failed" || call.status === "uncertain";

/** 一行详情：按这份字段顺序取第一个非空的字符串（所有工具同一份顺序，不按工具定制）。 */
const DETAIL_FIELDS = [
  "title", "description", "objective", "task", "task_name", "name", "question", "prompt", "message",
  "command", "cmd", "queries", "query", "pattern", "url", "uri", "file_path", "path", "target", "action", "status",
];

export function callDetail(call: Call): string {
  for (const key of DETAIL_FIELDS) {
    const raw = call.args[key];
    const value = Array.isArray(raw) && raw.every((x) => typeof x === "string") ? raw.join(", ") : raw;
    if (typeof value === "string" && value.trim()) return value.replace(/\s+/g, " ").trim();
  }
  return call.name;
}

/** 思考的预览：最近一个首行以换行结束的段落的首行。 */
export function thinkPreview(text: string): string {
  const paragraphs = text.split(/\n\s*\n/).filter((p) => p.trim());
  for (let i = paragraphs.length - 1; i >= 0; i--) {
    const p = paragraphs[i]!.trim();
    if (p.includes("\n") || i < paragraphs.length - 1 || /\n\s*$/.test(text)) return p.split("\n")[0]!.replace(/\*\*/g, "");
  }
  return paragraphs[0]?.split("\n")[0]!.replace(/\*\*/g, "") ?? "";
}

export type Step = { kind: "think"; id: string; text: string } | { kind: "call"; id: string; call: Call };
export type Segment = { kind: "steps"; id: string; steps: Step[] } | Extract<ProcessItem, { kind: "steer" | "recall" | "text" }>;

/** 把一轮的过程切成段：相邻的思考与调用并成一组，回复、插话、召回把组截断。 */
export function segments(process: ProcessItem[]): Segment[] {
  const out: Segment[] = [];
  for (const item of process) {
    if (item.kind !== "think" && item.kind !== "tools") {
      out.push(item);
      continue;
    }
    let seg = out[out.length - 1];
    if (!seg || seg.kind !== "steps") {
      seg = { kind: "steps", id: `s-${item.id}`, steps: [] };
      out.push(seg);
    }
    if (seg.kind !== "steps") continue;
    if (item.kind === "think") seg.steps.push(item);
    else seg.steps.push(...item.calls.map((call): Step => ({ kind: "call", id: call.id, call })));
  }
  return out;
}

export interface GroupSummary {
  /** 图标和标题的类别（结束的组取次数最多的那一类）。 */
  icon: Category;
  /** 结束的组：按次数降序取前三的类别；跑着的组：当前在跑的那一类（一个）。 */
  categories: Category[];
  /** 跑着的组才有：多于三类（标题末尾加"等"）。 */
  more: boolean;
  /** 跑着的组：在跑的调用的详情，或最近一段思考。 */
  detail?: string;
}

/**
 * 组标题的数据：结束了的按类别次数降序（同数按首次出现）取前三；在跑的写当前在跑的那个调用，
 * 没有就"正在分析请求"。文字由界面按语言拼。
 */
export function groupSummary(seg: Extract<Segment, { kind: "steps" }>, live: boolean): GroupSummary {
  const calls = seg.steps.filter((s): s is Extract<Step, { kind: "call" }> => s.kind === "call").map((s) => s.call);
  if (live) {
    const busy = calls.filter(callBusy).sort((a, b) => (a.startedAt ?? "").localeCompare(b.startedAt ?? "")).at(-1);
    if (busy) {
      const cat = categoryOf(busy.name);
      return { icon: cat, categories: [cat], more: false, detail: callDetail(busy) };
    }
    const think = [...seg.steps].reverse().find((s) => s.kind === "think");
    const last = think?.kind === "think"
      ? think.text.split(/\n\s*\n/).filter((p) => p.trim()).at(-1)?.replace(/\*\*/g, "").replace(/\s+/g, " ").trim()
      : undefined;
    return { icon: "thinking", categories: ["thinking"], more: false, detail: last || undefined };
  }
  const counts = new Map<Category, number>();
  for (const call of calls) {
    const cat = categoryOf(call.name);
    counts.set(cat, (counts.get(cat) ?? 0) + 1);
  }
  // sort 是稳定的：同数保留首次出现的顺序
  const ranked = [...counts].sort((a, b) => b[1] - a[1]).map(([cat]) => cat);
  if (!ranked.length) return { icon: "thinking", categories: ["thinking"], more: false };
  return { icon: ranked[0]!, categories: ranked.slice(0, 3), more: ranked.length > 3 };
}
