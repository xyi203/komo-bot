import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

/**
 * 悬停（或键盘聚焦）时在下方出现的深色提示：一句话，右边可带快捷键（淡一档）。纯 CSS，不挂 portal：
 * 只用在顶栏这种上方不裁切的地方。触屏（没有悬停）不显示，免得点一下就粘着。
 * 包着的按钮自己要有 `aria-label`；提示只是视觉上的，对读屏隐藏。
 */
export function Tooltip({ label, shortcut, children, className }: { label: ReactNode; shortcut?: string; children: ReactNode; className?: string }) {
  return (
    <span className={cn("group/tip relative inline-grid", className)}>
      {children}
      <span
        aria-hidden
        className="pointer-events-none absolute top-full left-1/2 z-50 mt-1.5 flex -translate-x-1/2 items-center gap-2.5 rounded-[8px] bg-primary px-2.5 py-1.5 text-label leading-4 whitespace-nowrap text-white opacity-0 transition-opacity delay-0 group-hover/tip:opacity-100 group-hover/tip:delay-300 group-has-[:focus-visible]/tip:opacity-100 [@media(hover:none)]:hidden"
      >
        <span>{label}</span>
        {shortcut && <kbd className="font-sans text-white/60">{shortcut}</kbd>}
      </span>
    </span>
  );
}
