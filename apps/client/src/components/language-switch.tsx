import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages.js";
import { cn } from "@/lib/utils";

const NAMES = {
  "zh-CN": () => m.language_zh_cn(),
  en: () => m.language_en(),
} as const;

/** 语言切换：偏好存在这个浏览器里（localStorage），不进 Gateway。 */
export function LanguageSwitch() {
  const { locale, locales, select } = useLocale();
  return (
    <div className="flex items-center gap-2 text-small text-muted">
      <span>{m.language_label()}</span>
      <div role="group" aria-label={m.language_label()} className="flex gap-1">
        {locales.map((code) => (
          <button
            key={code}
            type="button"
            aria-pressed={code === locale}
            onClick={() => select(code)}
            className={cn(
              "rounded-control px-2 py-0.5 hover:bg-hover",
              code === locale && "bg-active font-medium text-text",
            )}
          >
            {NAMES[code]()}
          </button>
        ))}
      </div>
    </div>
  );
}
