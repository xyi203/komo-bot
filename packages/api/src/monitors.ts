import type { MonitorListResponse } from "@komo/protocol";
import { useQuery } from "@tanstack/react-query";
import { get } from "./http";
import { keys } from "./keys";

/** `GET /v1/monitors`：监听与转后台的 `bash` 命令；`all` 带上已结束的（登记里留着的最近几十条）。 */
export const fetchMonitors = (all = false) => get<MonitorListResponse>("/v1/monitors", { all: all || undefined });

/** 有还在跑的就 4 秒取一次（结束了才看得到），否则 15 秒：转后台不走会话的事件流，只能取。 */
export function useMonitors(all = false, enabled = true) {
  return useQuery({
    queryKey: all ? keys.monitorsAll : keys.monitors,
    queryFn: () => fetchMonitors(all),
    select: (data) => data.monitors,
    refetchInterval: (query) => (query.state.data?.monitors.some((x) => x.state !== "ended") ? 4000 : 15_000),
    enabled,
    retry: false,
  });
}
