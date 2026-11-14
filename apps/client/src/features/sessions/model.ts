import type { SessionSummary } from "@komo/protocol";
import { sessionUnfinished } from "@komo/api";

// 会话列表（分组、排序、筛选）已随旧侧栏删掉；这里只留对话页还在用的两个小判断。

/** "已归档"目前映射到 `deleted` 与 `closing`（docs/web.md §9）。 */
export const isArchived = (s: SessionSummary) => s.state === "deleted" || s.state === "closing";

/** 没跑完的（在跑或停在等待上）。 */
export const isLive = sessionUnfinished;
