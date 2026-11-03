import type {
  CronDeleteResponse,
  CronListResponse,
  ManualCronRunResponse,
  UpdateCronRequest,
} from "@komo/protocol";
import { useQuery } from "@tanstack/react-query";
import { get, post, request } from "./http";
import { keys } from "./keys";

// 定时任务（docs/web.md §13）。新建与修改定义走对话或 `komo cron`，这里只有读、现在跑一次、暂停 / 恢复、删除。

export const fetchCron = () => get<CronListResponse>("/v1/cron");

/** 手动触发一次：用独立的请求键，不冒充定时触发。 */
export const runCron = (id: string, requestKey: string) =>
  post<ManualCronRunResponse>(`/v1/cron/${encodeURIComponent(id)}/run`, { request_key: requestKey });

export const patchCron = (id: string, body: UpdateCronRequest) =>
  request<unknown>("PATCH", `/v1/cron/${encodeURIComponent(id)}`, { body });

export const deleteCron = (id: string) => request<CronDeleteResponse>("DELETE", `/v1/cron/${encodeURIComponent(id)}`);

/** 任务清单（⌘K 与事项页挂触发器要用）。`enabled` 假则不取。 */
export function useCronJobs(enabled = true) {
  return useQuery({ queryKey: keys.cron, queryFn: fetchCron, select: (data) => data.jobs, enabled, retry: false });
}

/** 整份响应：任务加上每个任务的下一次与最近一次触发（定时页用，与 `useCronJobs` 同一份缓存）。 */
export function useCronList() {
  return useQuery({ queryKey: keys.cron, queryFn: fetchCron, retry: false });
}
