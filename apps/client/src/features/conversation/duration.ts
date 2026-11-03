import { m } from "@/paraglide/messages.js";

/** 一轮用了多久：`2分50秒` / `2m 50s`。 */
export function formatDuration(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  const min = Math.floor(s / 60);
  const h = Math.floor(min / 60);
  if (h) return m.duration_h_m({ h, m: min % 60 });
  if (min) return m.duration_m_s({ m: min, s: s % 60 });
  return m.duration_s({ s });
}
