import type {
  MatterActionRequest,
  MatterAddRequest,
  MatterDetail,
  MatterListResponse,
  MemoryListResponse,
} from "@komo/protocol";
import { useQuery } from "@tanstack/react-query";
import { get, post } from "./http";
import { keys } from "./keys";

// 事项：目标与领域（docs/web.md §12）。线上形状全部来自 `@komo/protocol`。

export const fetchMatters = (all = false) => get<MatterListResponse>("/v1/matters", { all: all || undefined });

/** 新建一件事项。 */
export const addMatter = (body: MatterAddRequest) => post<MatterDetail>("/v1/matters", body);

/** 事项的动作：`done` / `drop` / `retire` / `reopen` / `park` / `next` / `wait` / `resume` / `edit` / `link` / `unlink`。 */
export const actMatter = (id: string, action: string, body: MatterActionRequest) =>
  post<MatterDetail>(`/v1/matters/${encodeURIComponent(id)}/${action}`, body);

/** 事项清单：右栏只要没结案的（`all` 为假），⌘K 与事项页要全部。`enabled` 假则不取。 */
export function useMatters(all = false, enabled = true) {
  return useQuery({
    queryKey: all ? keys.mattersAll : keys.matters,
    queryFn: () => fetchMatters(all),
    select: (data) => data.matters,
    enabled,
    retry: false,
  });
}

/** 事项页要拿 Gateway 的 `today` 算超期，所以要整份响应（与 `useMatters(true)` 同一份缓存）。 */
export function useMatterList() {
  return useQuery({ queryKey: keys.mattersAll, queryFn: () => fetchMatters(true), retry: false });
}

/** 带归档的全部记忆：事项页"为它记的记忆"按 `belongs_to` 归拢。key 在 `keys.memories` 前缀下，记忆有变动时一起失效。 */
export function useOwnedMemories() {
  return useQuery({
    queryKey: [...keys.memories, "all"] as const,
    queryFn: () => get<MemoryListResponse>("/v1/memories", { include_archived: true, limit: 500 }),
    select: (data) => data.memories,
    retry: false,
  });
}
