import type { MemoryItem, MemoryScope } from "@komo/protocol";
import { getLocale } from "@/paraglide/runtime.js";
import { m } from "@/paraglide/messages.js";

/** 在场光阶：每条记忆按离 prompt 的远近落在四档之一（图上点的排位）。 */
export type Rung = "always" | "relevant" | "paused" | "out";

export const RUNGS: { key: Rung; test: (item: MemoryItem) => boolean }[] = [
  { key: "always", test: (x) => x.state === "active" && x.application === "convention" },
  { key: "relevant", test: (x) => x.state === "active" && x.application !== "convention" },
  { key: "paused", test: (x) => x.state === "candidate" || x.state === "contested" },
  { key: "out", test: (x) => x.state === "forgotten" || x.state === "superseded" || x.state === "archived" },
];

export const rungOf = (item: MemoryItem): Rung | undefined => RUNGS.find((r) => r.test(item))?.key;

/**
 * 等你过目：候选且你还没确认。确认过的候选不算——服务端的 `confirm` 只改确认状态、不把候选转成生效
 * （`docs/memory.md` 附录 C.3.1），那样的条目既不再等你、也还没进召回，归到"暂停中"。
 */
export const isPending = (item: MemoryItem) => item.state === "candidate" && item.confirmation !== "user_confirmed";

/** 记忆页的分组（版式见 `docs/web.md` §11）。 */
export type Group = "pending" | "paused" | "always" | "relevant" | "out";
export const GROUPS: Group[] = ["pending", "paused", "always", "relevant", "out"];
export function groupOf(item: MemoryItem): Group {
  if (isPending(item)) return "pending";
  if (item.state === "candidate" || item.state === "contested") return "paused";
  if (item.state === "active") return item.application === "convention" ? "always" : "relevant";
  return "out";
}

/** 光点的样子：实心 = 约定，空心环 = 线索，虚线 = 候选，红环 = 有争议，灰点 = 不再带。 */
export type MoteKind = "full" | "ring" | "alert" | "pale" | "off";
export const moteKind = (item: MemoryItem): MoteKind =>
  item.state === "active" ? (item.application === "convention" ? "full" : "ring") : item.state === "contested" ? "alert" : item.state === "candidate" ? "pale" : "off";

export const isLive = (item: MemoryItem) => item.state === "active" || item.state === "candidate" || item.state === "contested";

export const stateName = (state: MemoryItem["state"]) =>
  ({
    active: m.memory_state_active,
    candidate: m.memory_state_candidate,
    contested: m.memory_state_contested,
    superseded: m.memory_state_superseded,
    forgotten: m.memory_state_forgotten,
    archived: m.memory_state_archived,
  })[state]();
export const kindName = (kind: MemoryItem["kind"]) =>
  ({ preference: m.memory_kind_preference, fact: m.memory_kind_fact, experience: m.memory_kind_experience })[kind]();
export const provenanceName = (p: MemoryItem["provenance"]) =>
  ({ user_statement: m.memory_prov_user_statement, tool_observation: m.memory_prov_tool_observation, model_inference: m.memory_prov_model_inference })[p]();
export const usageName = (item: MemoryItem) =>
  item.application === "convention" ? m.memory_use_convention() : item.application === "reference" ? m.memory_use_reference() : m.memory_use_unclassified();

/** 作用域的一行文本键（同 `?scope=`）：筛选与"记一条"的选项用它。 */
export const scopeKey = (s: MemoryScope): string =>
  s.kind === "personal" ? "personal" : `${s.kind}:${s.kind === "project" ? s.project_id : s.kind === "environment" ? s.instance_id : s.agent_id}`;
export const scopeName = (s: MemoryScope): string =>
  s.kind === "personal" ? m.memory_scope_personal() : s.kind === "project" ? s.project_id : s.kind === "environment" ? m.memory_scope_environment({ id: s.instance_id }) : m.memory_scope_agent({ id: s.agent_id });
export function scopeFromKey(key: string): MemoryScope {
  const [kind, ...rest] = key.split(":");
  const id = rest.join(":");
  if (kind === "project") return { kind, project_id: id };
  if (kind === "environment") return { kind, instance_id: id };
  if (kind === "agent") return { kind, agent_id: id };
  return { kind: "personal" };
}

/** 清单里实际出现的作用域（去重），个人在前、其余按名字。 */
export function scopesIn(items: MemoryItem[], withPersonal = false): [string, string][] {
  const scopes = new Map<string, string>();
  if (withPersonal) scopes.set("personal", scopeName({ kind: "personal" }));
  for (const item of items) scopes.set(scopeKey(item.scope), scopeName(item.scope));
  return [...scopes].sort((a, b) => (a[0] === "personal" ? -1 : b[0] === "personal" ? 1 : a[1].localeCompare(b[1])));
}

const byUsage = (a: MemoryItem, b: MemoryItem) => b.usage.count - a.usage.count || Date.parse(b.updated_at) - Date.parse(a.updated_at);
const byNewest = (a: MemoryItem, b: MemoryItem) => Date.parse(b.created_at) - Date.parse(a.created_at);
const byChanged = (a: MemoryItem, b: MemoryItem) => Date.parse(b.updated_at) - Date.parse(a.updated_at);
const ORDER: Record<Group, (a: MemoryItem, b: MemoryItem) => number> = { pending: byNewest, paused: byChanged, always: byUsage, relevant: byUsage, out: byChanged };

/** 按分组分好的行（可再限一个作用域）：等你过目的新的在前，约定与线索按带上次数，其余按最近变动。 */
export function groupRows(all: MemoryItem[], scope: string): Record<Group, MemoryItem[]> {
  const groups: Record<Group, MemoryItem[]> = { pending: [], paused: [], always: [], relevant: [], out: [] };
  for (const item of all) if (!scope || scopeKey(item.scope) === scope) groups[groupOf(item)].push(item);
  for (const key of GROUPS) groups[key].sort(ORDER[key]);
  return groups;
}

/** 页头那句概况：记着几件事，几条等你过目。 */
export function lead(all: MemoryItem[]): string {
  const live = all.filter((x) => x.state === "active").length;
  const pending = all.filter(isPending).length;
  if (!live && !pending) return m.memory_finding_none();
  return pending ? m.memory_lead_pending({ live, pending }) : m.memory_lead_clear({ live });
}

// ---- 日期（Intl，按当前语言）----
const DAY = 86_400_000;
export const ymd = (d: Date) => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
export const dayDiff = (a: string, b: string) => Math.round((Date.parse(`${a}T00:00:00Z`) - Date.parse(`${b}T00:00:00Z`)) / DAY);
export const formatDay = (d: Date | string) => new Intl.DateTimeFormat(getLocale(), { month: "numeric", day: "numeric" }).format(new Date(d));
