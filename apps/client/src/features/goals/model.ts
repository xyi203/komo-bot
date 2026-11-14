import { isApiError } from "@komo/api";
import type { FiringStatus, Matter, MatterSchedule, MatterState, TaskSummary } from "@komo/protocol";
import { errorText } from "@/lib/errors";
import { formatClock } from "@/lib/format";
import { scheduleText } from "@/lib/cron-text";
import { m } from "@/paraglide/messages.js";
import { getLocale } from "@/paraglide/runtime.js";

// 目标页的小算法（docs/web.md §12）：Gateway 给的 `due`、`today` 是 `YYYY-MM-DD` 的日历日，不是时刻，按日历日算差，不套时区。
// 「追踪中」= 进行中 / 等待中（没有终点的目标只有进行中与不再管两个状态）；搁置、完成、放弃、不再管都是「不再追踪」（docs/redesign.md §1.4）。

const CLOSED: MatterState[] = ["done", "dropped", "retired"];
export const isClosed = (matter: Pick<Matter, "state">) => CLOSED.includes(matter.state);
export const isTracked = (matter: Pick<Matter, "state">) => matter.state === "active" || matter.state === "waiting";
/** 没有终点的目标（Gateway 里是「领域」一类）。 */
export const isOngoing = (matter: Pick<Matter, "kind">) => matter.kind === "area";

/** `b` 到 `a` 隔几天（a 晚于 b 为正）。 */
export const dayDiff = (a: string, b: string) => Math.round((Date.parse(`${a}T00:00:00Z`) - Date.parse(`${b}T00:00:00Z`)) / 86_400_000);

/** `YYYY-MM-DD` 写成"11月20日"这样的短日期。 */
export function formatDay(day: string): string {
  return new Intl.DateTimeFormat(getLocale(), { month: "short", day: "numeric", timeZone: "UTC" }).format(new Date(`${day}T00:00:00Z`));
}

/** 一个时刻落在哪一天（本机时区）的短日期。 */
export function formatInstantDay(iso: string): string {
  return new Intl.DateTimeFormat(getLocale(), { month: "short", day: "numeric" }).format(new Date(iso));
}

/** 一个时刻：今天 / 昨天 / 明天只写钟点前缀，其余写月日加钟点（本机时区）。 */
export function formatWhen(iso: string, now = new Date()): string {
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return "";
  const day = Math.round((new Date(at.getFullYear(), at.getMonth(), at.getDate()).getTime() - new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime()) / 86_400_000);
  const prefix = day === 0 ? m.cron_day_today() : day === 1 ? m.cron_day_tomorrow() : day === -1 ? m.cron_day_yesterday() : formatInstantDay(iso);
  return `${prefix} ${formatClock(at)}`;
}

export interface Due {
  text: string;
  sub: string;
  bad: boolean;
}

/** 截止日期怎么写：日期，加"超期 N 天 / 还有 N 天"（按 Gateway 的 today；不再追踪的只写日期）。 */
export function dueOf(matter: Matter, today: string | undefined): Due | null {
  if (!matter.due) return null;
  if (!isTracked(matter) || !today) return { text: formatDay(matter.due), sub: "", bad: false };
  const d = dayDiff(matter.due, today);
  const sub =
    d < 0 ? m.matter_due_overdue({ days: -d }) : d === 0 ? m.matter_due_today() : d === 1 ? m.matter_due_tomorrow() : m.matter_due_in({ days: d });
  return { text: formatDay(matter.due), sub, bad: d < 0 };
}

export function statusLabel(state: MatterState): string {
  return {
    active: m.matter_status_active,
    waiting: m.matter_status_waiting,
    parked: m.matter_status_parked,
    done: m.matter_status_done,
    dropped: m.matter_status_dropped,
    retired: m.matter_status_retired,
  }[state]();
}

/** 说明的第一行（列表里一句话用）。 */
export const firstLine = (text: string | undefined) => (text ?? "").split("\n").find((line) => line.trim())?.trim() ?? "";

/** 列表里的一句话：说明的第一行，加「下一步」或「在等」。 */
export function goalSub(matter: Matter): string {
  const parts = [firstLine(matter.description) || (isOngoing(matter) ? matter.standard : "") || ""];
  if (matter.state === "waiting" && matter.waiting_for) parts.push(m.matter_sub_waiting({ what: matter.waiting_for }));
  else if (matter.state === "active" && matter.next) parts.push(m.matter_sub_next({ what: matter.next }));
  return parts.filter(Boolean).join(" · ");
}

const FIRING_LABEL: Record<FiringStatus, () => string> = {
  queued: m.cron_firing_queued,
  ok: m.cron_firing_ok,
  error: m.cron_firing_error,
  waiting: m.cron_firing_waiting,
  skipped: m.cron_firing_skipped,
};
export const firingLabel = (status: FiringStatus) => FIRING_LABEL[status]();

/** 定时的一句话：节奏（"每天 08:30"）。 */
export const scheduleSummary = (schedule: MatterSchedule) => scheduleText(schedule.job.trigger);

/** 定时在跑：没暂停、没跑完。 */
export const scheduleOn = (schedule: MatterSchedule) => schedule.job.status === "active";

/** 下一次的时间；暂停着、跑完了或排不上都没有。 */
export const nextRunText = (schedule: MatterSchedule): string | null =>
  scheduleOn(schedule) && schedule.job.next_run_at ? formatWhen(schedule.job.next_run_at) : null;

/** 「上次成功，今天 08:30」；没响过是「还没跑过」。 */
export const lastRunText = (schedule: MatterSchedule): string =>
  schedule.last ? m.goal_schedule_last({ status: firingLabel(schedule.last.status), when: formatWhen(schedule.last.scheduled_at) }) : m.cron_never_ran();

/** 活动里一件事的状态：在跑、在等你、做完了、被卡住…… */
export function taskLabel(task: TaskSummary, waiting: boolean): string {
  if (waiting) return m.goal_task_waiting();
  if (task.state === "running") return m.goal_task_running();
  switch (task.outcome) {
    case "done":
      return m.goal_task_done();
    case "blocked":
      return m.goal_task_blocked();
    case "not_completed":
      return m.goal_task_not_completed();
    default:
      return m.goal_task_ended();
  }
}

/** 错误文字：校验失败（时区不对、表达式写错……）时 Gateway 的原话指出了支持值，比通用译文有用。 */
export function serverText(error: unknown): string {
  return isApiError(error) && error.code === "invalid_request" && error.message ? error.message : errorText(error);
}
