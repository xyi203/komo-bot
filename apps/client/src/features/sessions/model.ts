import type { SessionSummary } from "@komo/protocol";
import { sessionUnfinished } from "@komo/api";

// 侧栏会话：分组（会话树 / 工作目录 / 单列表）× 排序（最近更新 / 最近创建）× 筛选（隐藏已归档 / 全部 / 仅已归档）。

export type GroupMode = "tree" | "workdir" | "flat";
export type SortMode = "updated" | "created";
export type FilterMode = "active" | "all" | "archived";

export const GROUP_MODES: readonly GroupMode[] = ["tree", "workdir", "flat"];
export const SORT_MODES: readonly SortMode[] = ["updated", "created"];
export const FILTER_MODES: readonly FilterMode[] = ["active", "all", "archived"];

/** 超过上限给"显示全部 N 个"。 */
export const PAGE_ROWS: Record<GroupMode, number> = { tree: 12, workdir: 5, flat: 20 };

/** "已归档"目前映射到 `deleted` 与 `closing`（docs/web.md §9）。 */
export const isArchived = (s: SessionSummary) => s.state === "deleted" || s.state === "closing";

export const isRunning = (s: SessionSummary) => s.current_state === "running" || s.current_state === "queued" || s.current_state === "accepted";
export const isWaiting = (s: SessionSummary) => s.current_state === "waiting";
/** 没跑完的（在跑或停在等待上）：算"N 个在跑"。 */
export const isLive = sessionUnfinished;

export interface ListOptions {
  sort: SortMode;
  filter: FilterMode;
}

export const passes = (s: SessionSummary, filter: FilterMode) =>
  filter === "all" || (filter === "archived" ? isArchived(s) : !isArchived(s));

const sortTime = (s: SessionSummary, sort: SortMode) => new Date(sort === "created" ? s.created_at : s.updated_at).getTime();
export const bySort = (sort: SortMode) => (a: SessionSummary, b: SessionSummary) => sortTime(b, sort) - sortTime(a, sort);

/** 一个会话派出的子任务（任务会话），最近创建的在上。 */
export const kidsOf = (sessions: readonly SessionSummary[], parent: SessionSummary, filter: FilterMode) =>
  sessions
    .filter((k) => k.kind === "task" && k.home === parent.session && passes(k, filter))
    .sort((a, b) => new Date(b.created_at).getTime() - new Date(a.created_at).getTime());

export const dirOf = (s: SessionSummary) => s.workdir ?? "";
/** 目录的显示名：最后一段；空串是"默认目录"（调用方换成译文）。 */
export const dirName = (dir: string) => (dir ? dir.replace(/\/+$/, "").split("/").pop() || dir : "");

export interface Group {
  key: string;
  /** 工作目录分组的目录（空串是默认目录）；其余分组没有。 */
  dir?: string;
  kind: "recent" | "flat" | "dir";
  sessions: SessionSummary[];
}

export interface Grouped {
  /** 主会话始终在最上（会话树与工作目录分组下）。 */
  main: SessionSummary | undefined;
  groups: Group[];
}

/** 还没有任何事件落账的会话（建了但一句话都没发）：`applied_seq` 仍是 0。 */
export const isEmpty = (s: SessionSummary) => s.applied_seq === 0;

export function groupSessions(sessions: readonly SessionSummary[], homeId: string | undefined, mode: GroupMode, options: ListOptions): Grouped {
  // 空会话不进侧栏（主会话除外，它始终在最上）；发出第一句后 run 事件会让列表重取，它再出现
  const visible = sessions.filter((s) => passes(s, options.filter) && (!isEmpty(s) || s.session === homeId));
  const order = bySort(options.sort);
  if (mode === "flat") return { main: undefined, groups: [{ key: "flat", kind: "flat", sessions: visible.sort(order) }] };
  // 任务会话挂在父会话下；父会话被筛掉了才自己出现在顶层
  const top = (s: SessionSummary) => s.kind !== "task" || !sessions.some((p) => p.session === s.home && passes(p, options.filter));
  const main = visible.find((s) => s.session === homeId);
  const rest = visible.filter((s) => s.session !== homeId && top(s)).sort(order);
  if (mode === "tree") return { main, groups: [{ key: "tree", kind: "recent", sessions: rest }] };
  // 按工作目录：每个目录一组，按组里最近的那个会话排
  const dirs = new Map<string, SessionSummary[]>();
  for (const s of rest) {
    const dir = dirOf(s);
    dirs.set(dir, [...(dirs.get(dir) ?? []), s]);
  }
  const groups = [...dirs.entries()]
    .sort((a, b) => sortTime(b[1][0]!, options.sort) - sortTime(a[1][0]!, options.sort))
    .map(([dir, list]): Group => ({ key: `dir:${dir}`, kind: "dir", dir, sessions: list }));
  return { main, groups: groups.length ? groups : [{ key: "dir:", kind: "dir", dir: "", sessions: [] }] };
}
