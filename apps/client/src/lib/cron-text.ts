import type { Trigger } from "@komo/protocol";
import { m } from "@/paraglide/messages.js";
import { getLocale } from "@/paraglide/runtime.js";
import { formatClock } from "./format";

// 把定时触发写成一句人话（"每天 09:00"、"工作日 18:30"…）。只认常见的几种写法，其余原样显示表达式。
// 五字段 cron：分 时 日 月 周；周 0 与 7 都是周日。

const RANGES: [number, number][] = [
  [0, 59],
  [0, 23],
  [1, 31],
  [1, 12],
  [0, 7],
];
const NAMES: (string[] | null)[] = [null, null, null, "jan feb mar apr may jun jul aug sep oct nov dec".split(" "), "sun mon tue wed thu fri sat".split(" ")];

/** 解析五字段表达式，每个字段展开成取值集合；写得不合法返回 null。 */
export function cronParse(expression: string): { sets: Set<number>[]; raw: string[] } | null {
  const fields = expression.trim().split(/\s+/);
  if (fields.length !== 5) return null;
  const value = (x: string, i: number) => {
    const named = NAMES[i]?.indexOf(x.toLowerCase()) ?? -1;
    return named >= 0 ? named + (i === 3 ? 1 : 0) : Number(x);
  };
  const sets: Set<number>[] = [];
  for (let i = 0; i < 5; i++) {
    const set = new Set<number>();
    for (const part of fields[i].split(",")) {
      const [range, stepText] = part.split("/");
      const step = stepText ? Number(stepText) : 1;
      let lo: number;
      let hi: number | undefined;
      if (range === "*") [lo, hi] = RANGES[i];
      else [lo, hi] = range.split("-").map((x) => value(x, i)) as [number, number | undefined];
      if (hi === undefined) hi = stepText ? RANGES[i][1] : lo;
      if (![lo, hi, step].every(Number.isFinite) || step < 1) return null;
      for (let v = lo; v <= hi; v += step) set.add(i === 4 && v === 7 ? 0 : v);
    }
    sets.push(set);
  }
  return { sets, raw: fields };
}

const weekday = (d: number) =>
  [m.cron_dow_sun, m.cron_dow_mon, m.cron_dow_tue, m.cron_dow_wed, m.cron_dow_thu, m.cron_dow_fri, m.cron_dow_sat][d]();

export function scheduleText(trigger: Trigger): string {
  if (trigger.kind === "at") {
    const at = new Date(trigger.at);
    const day = new Intl.DateTimeFormat(getLocale(), { month: "numeric", day: "numeric" }).format(at);
    return m.cron_once({ when: `${day} ${formatClock(at)}` });
  }
  const parsed = cronParse(trigger.expr);
  if (!parsed) return trigger.expr;
  const [mi, ho, dm, mo, dw] = parsed.raw;
  const single = (x: string) => /^\d+$/.test(x);
  const hm = single(mi) && single(ho) ? `${ho.padStart(2, "0")}:${mi.padStart(2, "0")}` : null;
  let words: string | null = null;
  if (hm && mo === "*") {
    if (dm === "*" && dw === "*") words = m.cron_daily({ time: hm });
    else if (dm === "*" && /^[\d,-]+$/.test(dw))
      words = dw === "1-5" ? m.cron_weekdays({ time: hm }) : m.cron_weekly({ days: [...parsed.sets[4]].sort().map(weekday).join(m.cron_day_separator()), time: hm });
    else if (single(dm) && dw === "*") words = m.cron_monthly({ day: dm, time: hm });
  } else if (single(mi) && ho === "*" && dm === "*" && mo === "*" && dw === "*") words = m.cron_hourly({ minute: mi });
  else if (/^\*\/\d+$/.test(mi) && ho === "*" && dm === "*" && mo === "*" && dw === "*") words = m.cron_every_minutes({ n: mi.slice(2) });
  const localTz = Intl.DateTimeFormat().resolvedOptions().timeZone;
  return (words ?? trigger.expr) + (trigger.tz && trigger.tz !== localTz ? m.cron_tz({ tz: trigger.tz }) : "");
}
