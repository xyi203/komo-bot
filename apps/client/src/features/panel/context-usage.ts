import type { SessionTree, TreeEntry, TreeMessage } from "@komo/api";
import type { ModelMenuEntry } from "@komo/protocol";
import type { View } from "@/features/conversation/model";

// 上下文构成（docs/web.md §6）。原料来自会话树（GET /v1/sessions/{id}/tree）：沿 leafId 取当前分支，压缩过就只算
// 保留下来的部分；系统提示的各段与工具定义取路径上最新的 komo.system；衰减过的工具结果按 context_edit 的替换视图算。
// 各项按字节 / 4 估（komo 自己的窗口保护也这样估），再按上一次请求实报的 input_tokens 等比校准，
// 所以合计是实测、各项比例是估算。

export type PartKey = "messages" | "results" | "tools" | "memory" | "skills" | "project" | "system" | "summary";

/** 分段的颜色是数据色（深到浅），不属于界面 token。 */
export const PART_COLORS: Record<PartKey, string> = {
  messages: "#4f6b38",
  results: "#7f9a63",
  tools: "#b3c59c",
  memory: "#3f4637",
  skills: "#8d9384",
  project: "#aab0a1",
  system: "#c9cdc1",
  summary: "#dfe2d9",
};
export const PART_ORDER: PartKey[] = ["messages", "results", "tools", "memory", "skills", "project", "system", "summary"];

const encoder = new TextEncoder();
const bytes = (x: unknown): number => encoder.encode(typeof x === "string" ? x : JSON.stringify(x ?? "")).length;

interface ToolDef {
  name?: string;
  function?: { name?: string };
}
interface SystemData {
  model?: { sections?: Record<string, unknown>; toolsAdded?: ToolDef[]; toolsRemoved?: ToolDef[] }[];
}
interface CompactionData {
  summary?: string;
  text?: string;
  head?: unknown;
  data?: CompactionData;
}

const toolName = (t: ToolDef) => t.name ?? t.function?.name ?? "";

export interface ContextParts {
  est: Record<PartKey, number>;
  tools: { name: string; tokens: number }[];
}

export function contextParts(tree: SessionTree | null | undefined): ContextParts | null {
  if (!tree?.entries?.length) return null;
  const byId = new Map(tree.entries.map((e) => [e.id, e]));
  const path: TreeEntry[] = [];
  for (let id = tree.leafId; id && byId.has(id); id = byId.get(id)!.parentId) path.push(byId.get(id)!);
  path.reverse();

  let start = 0;
  let summary = 0;
  path.forEach((entry, i) => {
    if (entry.customType !== "komo.compaction") return;
    const outer = (entry.data ?? {}) as CompactionData;
    const d = outer.data ?? outer;
    summary = bytes(d.summary ?? d.text ?? "");
    const head = path.findIndex((x) => x.id === String(d.head ?? ""));
    start = head >= 0 ? head : i + 1;
  });

  // komo.system 是增量：分段为 null 表示删掉，工具按 toolsAdded / toolsRemoved 增减，沿路径叠起来才是当前这份
  const sections: Record<string, unknown> = {};
  const toolMap = new Map<string, ToolDef>();
  for (const entry of path) {
    if (entry.customType !== "komo.system") continue;
    const m = (entry.data as SystemData | undefined)?.model?.[0] ?? {};
    for (const [k, v] of Object.entries(m.sections ?? {})) {
      if (v == null) delete sections[k];
      else sections[k] = v;
    }
    for (const t of m.toolsAdded ?? []) toolMap.set(toolName(t), t);
    for (const t of m.toolsRemoved ?? []) toolMap.delete(toolName(t));
  }
  const tools = [...toolMap.values()];
  const edits = new Map<string, Partial<TreeMessage>>(
    path.filter((e) => e.type === "context_edit" && e.replacement && e.targetId).map((e) => [e.targetId!, e.replacement!]),
  );

  const raw: Record<PartKey, number> = {
    messages: 0,
    results: 0,
    tools: bytes(tools),
    memory: bytes(sections.memory ?? ""),
    skills: bytes(sections.skills ?? ""),
    project: bytes(sections.project ?? ""),
    system: 0,
    summary,
  };
  for (const [k, v] of Object.entries(sections)) if (!["memory", "skills", "project"].includes(k)) raw.system += bytes(v);
  for (const entry of path.slice(start)) {
    if (entry.type !== "message" || !entry.message) continue;
    const message = edits.has(entry.id) ? { ...entry.message, ...edits.get(entry.id) } : entry.message;
    const blocks = typeof message.content === "string" ? [{ type: "text", text: message.content }] : (message.content ?? []);
    const size = blocks.reduce(
      (sum, b) => sum + (b.type === "text" ? bytes(b.text) : b.type === "toolCall" ? bytes(b.name) + bytes(b.arguments) : 0),
      0,
    );
    if (message.role === "toolResult") raw.results += size;
    else if (message.role === "user" && blocks[0]?.text?.startsWith("[已知记忆]")) raw.memory += size;
    else raw.messages += size;
  }
  const est = Object.fromEntries(Object.entries(raw).map(([k, b]) => [k, Math.round(b / 4)])) as Record<PartKey, number>;
  return { est, tools: tools.map((t) => ({ name: toolName(t) || "?", tokens: Math.round(bytes(t) / 4) })) };
}

export interface ContextView {
  win: number;
  total: number;
  /** 有上一次请求实报的用量（没有就全是估算，或还没请求过）。 */
  measured: boolean;
  rows: { key: PartKey; tokens: number }[];
  tools: { name: string; tokens: number }[];
  scale: number;
}

/** 当前会话用的模型的上下文窗口：按事件里最近的模型找，找不到用默认模型。 */
export function windowOf(model: string | null | undefined, models: readonly ModelMenuEntry[] | undefined): number {
  const list = models ?? [];
  const entry = list.find((x) => x.model === model || x.id === model) ?? list.find((x) => x.default);
  return entry?.context_window ?? 0;
}

export function contextView(view: View | undefined, tree: SessionTree | null | undefined, models: readonly ModelMenuEntry[] | undefined): ContextView {
  const win = windowOf(view?.model, models);
  const parts = contextParts(tree);
  const sum = parts ? Object.values(parts.est).reduce((a, b) => a + b, 0) : 0;
  const usage = view?.usage ?? 0;
  const total = usage || sum;
  const scale = parts && usage && sum ? usage / sum : 1;
  const rows = parts
    ? PART_ORDER.map((key) => ({ key, tokens: Math.round(parts.est[key] * scale) })).filter((r) => r.tokens > 0)
    : [];
  return { win, total, measured: !!usage, rows, tools: parts?.tools ?? [], scale };
}

export const pctOf = (n: number, win: number) => (win ? (n / win) * 100 : 0);
/** 占比文字：≥10% 取整，≥0.1% 一位小数，更小写 `<0.1%`。 */
export function pctText(n: number, win: number): string {
  if (!win) return "—";
  const p = pctOf(n, win);
  return p >= 10 ? `${p.toFixed(0)}%` : p >= 0.1 ? `${p.toFixed(1)}%` : "<0.1%";
}
