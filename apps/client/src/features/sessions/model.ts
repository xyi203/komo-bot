import type { SessionSummary } from "@komo/protocol";
import { sessionUnfinished } from "@komo/api";

// 会话列表（分组、排序、筛选）已随旧侧栏删掉；这里只留还有人引用的几个小判断（对话页、⌘K 弹窗）。

/** "已归档"目前映射到 `deleted` 与 `closing`（docs/web.md §9）。 */
export const isArchived = (s: SessionSummary) => s.state === "deleted" || s.state === "closing";

/** 没跑完的（在跑或停在等待上）。 */
export const isLive = sessionUnfinished;

export const dirOf = (s: SessionSummary) => s.workdir ?? "";
/** 目录的显示名：最后一段；空串是"默认目录"（调用方换成译文）。 */
export const dirName = (dir: string) => (dir ? dir.replace(/\/+$/, "").split("/").pop() || dir : "");

/** 还没有任何事件落账的会话（建了但一句话都没发）：`applied_seq` 仍是 0。 */
export const isEmpty = (s: SessionSummary) => s.applied_seq === 0;
