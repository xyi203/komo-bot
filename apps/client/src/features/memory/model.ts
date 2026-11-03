import type { MemoryItem, MemoryScope } from "@komo/protocol";
import { getLocale } from "@/paraglide/runtime.js";
import { m } from "@/paraglide/messages.js";

/** 在场光阶：每条记忆按离 prompt 的远近落在四档之一（图例兼筛选）。 */
export type Rung = "always" | "relevant" | "paused" | "out";
/** 筛选：`live` = 在起作用（前两档一起亮）。 */
export type Bucket = "live" | Rung;

export const RUNGS: { key: Rung; test: (item: MemoryItem) => boolean }[] = [
  { key: "always", test: (x) => x.state === "active" && x.application === "convention" },
  { key: "relevant", test: (x) => x.state === "active" && x.application !== "convention" },
  { key: "paused", test: (x) => x.state === "candidate" || x.state === "contested" },
  { key: "out", test: (x) => x.state === "forgotten" || x.state === "superseded" || x.state === "archived" },
];

export const rungOf = (item: MemoryItem): Rung | undefined => RUNGS.find((r) => r.test(item))?.key;
export const inBucket = (item: MemoryItem, bucket: Bucket) => (bucket === "live" ? item.state === "active" : rungOf(item) === bucket);
/** 一条记忆该落在哪个筛选里（生效的都归"在起作用"）。 */
export const bucketOf = (item: MemoryItem): Bucket => (item.state === "active" ? "live" : (rungOf(item) ?? "live"));

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

/** 列表的行：在起作用的约定在前、再按带上次数；不再带的按最近变动。检索结果保持检索的排序。 */
export function listRows(all: MemoryItem[], bucket: Bucket, scope: string): MemoryItem[] {
  const rows = all.filter((x) => inBucket(x, bucket) && (!scope || scopeKey(x.scope) === scope));
  return rows.sort((a, b) =>
    bucket === "out"
      ? Date.parse(b.updated_at) - Date.parse(a.updated_at)
      : Number(b.application === "convention") - Number(a.application === "convention") || b.usage.count - a.usage.count,
  );
}

/** 页头那句发现：有争议 > 候选 > 在起作用的里还有几条没过目 / 都确认过。 */
export function finding(all: MemoryItem[]): string {
  const contested = all.filter((x) => x.state === "contested").length;
  const candidate = all.filter((x) => x.state === "candidate").length;
  const live = all.filter((x) => x.state === "active");
  const unseen = live.filter((x) => x.confirmation !== "user_confirmed").length;
  if (contested) return m.memory_finding_contested({ count: contested });
  if (candidate) return m.memory_finding_candidate({ count: candidate });
  if (!live.length) return m.memory_finding_none();
  if (!unseen) return m.memory_finding_confirmed({ count: live.length });
  return unseen === live.length ? m.memory_finding_all_unseen({ count: live.length }) : m.memory_finding_some_unseen({ live: live.length, unseen });
}

// ---- 日期（Intl，按当前语言）----
const DAY = 86_400_000;
export const ymd = (d: Date) => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
export const dayDiff = (a: string, b: string) => Math.round((Date.parse(`${a}T00:00:00Z`) - Date.parse(`${b}T00:00:00Z`)) / DAY);
export const formatDay = (d: Date | string) => new Intl.DateTimeFormat(getLocale(), { month: "numeric", day: "numeric" }).format(new Date(d));
