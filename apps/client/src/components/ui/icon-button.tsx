import type { ComponentProps } from "react";
import { Icon, type IconName } from "@/components/icon";
import { cn } from "@/lib/utils";

/**
 * 只有图标的按钮：`label` 同时是读屏名和悬停提示。`size`：默认 32、`sm` 26（消息旁的复制）、`compact` 28（侧栏区头）。
 * 窄屏（≤900）可点区域补到 44（伪元素，外观不变）。
 */
export function IconButton({
  name,
  label,
  size = "default",
  className,
  children,
  ...props
}: { name?: IconName; label: string; size?: "default" | "sm" | "compact" } & Omit<ComponentProps<"button">, "children"> & {
    children?: React.ReactNode;
  }) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      className={cn(
        "grid shrink-0 cursor-pointer place-items-center rounded-control text-muted transition-colors hover:bg-hover-on-white hover:text-text",
        // 窄屏（触屏）：外观不变，用伪元素把可点区域补到 44px
        "narrow:relative narrow:after:absolute narrow:after:content-['']",
        size === "default" && "size-8 narrow:after:-inset-1.5",
        size === "compact" && "size-7 narrow:after:-inset-2",
        size === "sm" && "size-6.5 rounded-sm narrow:after:-inset-2.5",
        className,
      )}
      {...props}
    >
      {name ? <Icon name={name} className={size === "sm" ? "size-3.5" : undefined} /> : children}
    </button>
  );
}
