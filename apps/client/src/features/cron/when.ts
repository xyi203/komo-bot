import { formatClock } from "@/lib/format";
import { m } from "@/paraglide/messages.js";
import { getLocale } from "@/paraglide/runtime.js";

/** 定时页的"什么时候"：今天 / 明天 / 昨天，一周内写星期，其余写月日，后面跟钟点。 */
export function formatWhen(iso: string | null | undefined, now = new Date()): string {
  if (!iso) return "—";
  const date = new Date(iso);
  const startOf = (d: Date) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const diff = Math.round((startOf(date) - startOf(now)) / 86_400_000);
  const locale = getLocale();
  const day =
    diff === 0
      ? m.cron_day_today()
      : diff === 1
        ? m.cron_day_tomorrow()
        : diff === -1
          ? m.cron_day_yesterday()
          : diff > 1 && diff < 7
            ? new Intl.DateTimeFormat(locale, { weekday: "short" }).format(date)
            : new Intl.DateTimeFormat(locale, { month: "short", day: "numeric" }).format(date);
  return `${day} ${formatClock(date)}`;
}
