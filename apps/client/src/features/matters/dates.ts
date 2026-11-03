import type { Matter, MatterState } from "@komo/protocol";
import { m } from "@/paraglide/messages.js";
import { getLocale } from "@/paraglide/runtime.js";

// 事项页的日期：Gateway 给的 `due`、`today` 是 `YYYY-MM-DD` 的日历日，不是时刻；按日历日算差，不套时区。

const CLOSED: MatterState[] = ["done", "dropped", "retired"];
export const isClosed = (matter: Pick<Matter, "state">) => CLOSED.includes(matter.state);

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

export interface Due {
  text: string;
  sub: string;
  bad: boolean;
}

/** 截止日期怎么写：日期，加"超期 N 天 / 还有 N 天"（按 Gateway 的 today；结案的只写日期）。 */
export function dueOf(matter: Matter, today: string | undefined): Due | null {
  if (!matter.due) return null;
  if (isClosed(matter) || !today) return { text: formatDay(matter.due), sub: "", bad: false };
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
