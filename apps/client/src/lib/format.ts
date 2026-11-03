// 日期、时间、相对时间与数字一律走 Intl，按当前语言渲染，不手拼字符串（docs/clients.md D9）。
import { getLocale } from "@/paraglide/runtime.js";

export function formatNumber(value: number): string {
  return new Intl.NumberFormat(getLocale()).format(value);
}

export function formatDateTime(iso: string): string {
  return new Intl.DateTimeFormat(getLocale(), { dateStyle: "medium", timeStyle: "short" }).format(new Date(iso));
}

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 365 * 24 * 3600],
  ["month", 30 * 24 * 3600],
  ["day", 24 * 3600],
  ["hour", 3600],
  ["minute", 60],
];

export function formatRelative(iso: string, now = Date.now()): string {
  const seconds = Math.round((new Date(iso).getTime() - now) / 1000);
  const format = new Intl.RelativeTimeFormat(getLocale(), { numeric: "auto" });
  for (const [unit, size] of UNITS) {
    if (Math.abs(seconds) >= size) return format.format(Math.round(seconds / size), unit);
  }
  return format.format(seconds, "second");
}

// ---- 对话与轨迹里的时间、时长、用量 ----

const clockFormat = (locale: string) =>
  new Intl.DateTimeFormat(locale, { hour: "2-digit", minute: "2-digit", hourCycle: "h23" });
const dayClockFormat = (locale: string) =>
  new Intl.DateTimeFormat(locale, { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit", hourCycle: "h23" });

/** 消息旁的时间：今天只写钟点，更早的带上月日。 */
export function formatStamp(iso: string, now = new Date()): string {
  const date = new Date(iso);
  const locale = getLocale();
  return date.toDateString() === now.toDateString() ? clockFormat(locale).format(date) : dayClockFormat(locale).format(date);
}

export function formatClock(date: Date): string {
  return clockFormat(getLocale()).format(date);
}

/** 轨迹详情里的起始时间：到毫秒。 */
export function formatFullTime(iso: string): string {
  return new Intl.DateTimeFormat(getLocale(), {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    fractionalSecondDigits: 3,
    hourCycle: "h23",
  }).format(new Date(iso));
}

/** 轨迹里一次调用或一步的耗时：`65 ms` / `1.4 s`（单位是符号，不翻译）。 */
export function formatMs(ms: number | undefined): string {
  if (ms == null) return "";
  return ms < 1000 ? `${formatNumber(Math.round(ms))} ms` : `${(ms / 1000).toFixed(1)} s`;
}

/** 正在跑的计时：`5m 3s` / `1h 2m`（缩写，两种语言通用）。 */
export function formatElapsed(fromIso: string, to = Date.now()): string {
  const seconds = Math.max(0, Math.round((to - new Date(fromIso).getTime()) / 1000));
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  return h ? `${h}h ${m}m` : m ? `${m}m ${s}s` : `${s}s`;
}

/** token 数的紧凑写法：`950` / `4.4k` / `128k` / `1.2M`。 */
export function formatTokens(n: number): string {
  if (n < 1000) return `${n}`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 100_000) return `${Math.round(n / 1000)}k`;
  return `${(n / 1000).toFixed(1)}k`;
}

/** 把几项连成一句（"a、b、c" / "a, b, c"），按当前语言。 */
export function formatList(items: string[]): string {
  return new Intl.ListFormat(getLocale(), { type: "conjunction", style: "narrow" }).format(items);
}
