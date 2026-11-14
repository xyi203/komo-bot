import type {
  MatterActionRequest,
  MatterAddRequest,
  MatterChatResponse,
  MatterDetail,
  MatterListWithWaiting,
  MatterRunsResponse,
  ScheduleSpec,
} from "@komo/protocol";
import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { del, get, post } from "./http";
import { keys } from "./keys";

// 目标（与没有终点的目标，即领域）：docs/web.md §12。线上形状全部来自 `@komo/protocol`。
// 定时是目标的一项配置（docs/redesign.md P8），没有独立的接口：设置 / 去掉 / 立即跑都是目标的动词。

const enc = encodeURIComponent;

export const fetchMatters = (all = false) => get<MatterListWithWaiting>("/v1/matters", { all: all || undefined });

/** 一个目标的详情：带定时、绑定的支线、监听、活动与为它记的记忆（`overview`）。 */
export const fetchMatter = (id: string) => get<MatterDetail>(`/v1/matters/${enc(id)}`);

/** 定时的运行历史一页（新的在前）；`before` 是上一页的 `next_before`。 */
export const fetchMatterRuns = (id: string, before?: string, limit?: number) =>
  get<MatterRunsResponse>(`/v1/matters/${enc(id)}/runs`, { before, limit });

/** 新建一个目标；带 `schedule` 就同时给它一个定时。 */
export const addMatter = (body: MatterAddRequest) => post<MatterDetail>("/v1/matters", body);

/** 目标的动作：`done` / `drop` / `retire` / `reopen` / `park` / `next` / `wait` / `resume` / `edit` / `link` / `unlink` / `schedule` / `unschedule` / `run`。 */
export const actMatter = (id: string, action: string, body: MatterActionRequest) =>
  post<MatterDetail>(`/v1/matters/${enc(id)}/${action}`, body);

/** 设置或修改定时：给出的字段才改；还没有定时时要给全（表达式、时区）。 */
export const scheduleMatter = (id: string, schedule: ScheduleSpec) => actMatter(id, "schedule", { schedule });

/** 给目标新开一段支线并绑定（已经绑着活的支线就原样返回，`created` 为假）。 */
export const bindMatterChat = (id: string) => post<MatterChatResponse>(`/v1/matters/${enc(id)}/chat`);

/** 解除绑定：那段支线留着，变回普通支线。 */
export const unbindMatterChat = (id: string) => del<MatterChatResponse>(`/v1/matters/${enc(id)}/chat`);

/** 目标清单：⌘K 与记忆页要 `all`；`enabled` 假则不取。 */
export function useMatters(all = false, enabled = true) {
  return useQuery({
    queryKey: all ? keys.mattersAll : keys.matters,
    queryFn: () => fetchMatters(all),
    select: (data) => data.matters,
    enabled,
    retry: false,
  });
}

/** 目标页要拿 Gateway 的 `today` 算超期、要 `schedules` 写下一次时间，所以要整份响应（与 `useMatters(true)` 同一份缓存）。 */
export function useMatterList() {
  return useQuery({ queryKey: keys.mattersAll, queryFn: () => fetchMatters(true), retry: false });
}

/** 一个目标的详情。活动里有在跑的或在等你的就 5 秒取一次：定时运行的进展不走任何会话的事件流。 */
export function useMatter(id: string) {
  return useQuery({
    queryKey: keys.matter(id),
    queryFn: () => fetchMatter(id),
    refetchInterval: (query) => (query.state.data?.overview?.activity?.some((item) => item.task.state === "running" || item.waiting) ? 5000 : false),
    retry: false,
  });
}

/** 运行历史一页 20 条，滚到底再取下一页。弹层打开时才取（`enabled`）。 */
export function useMatterRuns(id: string, enabled: boolean) {
  return useInfiniteQuery({
    queryKey: keys.matterRuns(id),
    queryFn: ({ pageParam }) => fetchMatterRuns(id, pageParam, 20),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_before ?? undefined,
    enabled,
    retry: false,
  });
}
