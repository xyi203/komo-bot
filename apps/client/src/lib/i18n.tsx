// 多语言骨架（docs/clients.md D9）：文案走 Paraglide 的消息函数（messages/*.json，漏翻与键名写错在编译期报错），
// 语言偏好**只存在这个浏览器的 localStorage**（Paraglide 的 localStorage 策略），不进 Gateway；
// 没选过就跟系统语言：`zh*` 用中文，其余用英文。日期、数字交给 Intl（见 format.ts）。
import {
  baseLocale,
  defineCustomClientStrategy,
  getLocale,
  locales,
  setLocale,
  type Locale,
} from "@/paraglide/runtime.js";
import { createContext, Fragment, useContext, useMemo, useState, type ReactNode } from "react";

/** 系统语言 → 支持的语言：`zh*` 是中文，其余是英文。 */
export function systemLocale(languages: readonly string[]): Locale {
  const first = languages[0]?.toLowerCase() ?? "";
  return first.startsWith("zh") ? "zh-CN" : "en";
}

// strategy 里的 `custom-system`：没有存过偏好时读系统语言；写入是空操作（"跟随系统"不是一次选择）。
defineCustomClientStrategy("custom-system", {
  getLocale: () =>
    typeof navigator === "undefined" ? undefined : systemLocale(navigator.languages ?? [navigator.language]),
  setLocale: () => {},
});

interface LocaleContext {
  locale: Locale;
  locales: readonly Locale[];
  select: (locale: Locale) => void;
}

const Context = createContext<LocaleContext | null>(null);

/**
 * 当前语言与切换。切换不刷新页面：改写偏好后让整棵子树按新语言重新渲染（`key`），
 * 消息函数在渲染时现读当前语言。
 */
export function LocaleProvider({ children }: { children: ReactNode }) {
  const [locale, setCurrent] = useState<Locale>(() => (typeof window === "undefined" ? baseLocale : getLocale()));
  const value = useMemo<LocaleContext>(
    () => ({
      locale,
      locales,
      select: (next) => {
        setLocale(next, { reload: false });
        document.documentElement.lang = next;
        setCurrent(next);
      },
    }),
    [locale],
  );
  if (typeof document !== "undefined") document.documentElement.lang = locale;
  return (
    <Context.Provider value={value}>
      <Fragment key={locale}>{children}</Fragment>
    </Context.Provider>
  );
}

export function useLocale(): LocaleContext {
  const value = useContext(Context);
  if (!value) throw new Error("useLocale 要在 LocaleProvider 里用");
  return value;
}
