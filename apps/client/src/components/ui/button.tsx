import type { ComponentProps } from "react";
import { cn } from "@/lib/utils";

/** 页面里的按钮（原型的 `.btn`）：默认白底描边，`primary` 近黑实心，`danger` 红描边。 */
export function Button({
  variant = "default",
  size = "default",
  className,
  type = "button",
  ...props
}: { variant?: "default" | "primary" | "danger"; size?: "default" | "sm" } & ComponentProps<"button">) {
  return (
    <button
      type={type}
      className={cn(
        "inline-flex shrink-0 cursor-pointer items-center justify-center gap-1.5 rounded-control font-medium whitespace-nowrap transition-[background-color,box-shadow,transform] hover:bg-hover-on-white active:scale-97 disabled:cursor-progress disabled:text-muted",
        size === "default" ? "h-8 px-3" : "h-7 px-2.5 text-label",
        variant === "default" && "bg-surface text-text shadow-ring-control",
        variant === "primary" && "bg-primary text-white shadow-none hover:bg-primary-hover",
        variant === "danger" && "bg-surface text-negative shadow-[0_0_0_1px_var(--color-negative)] hover:bg-negative-pale",
        className,
      )}
      {...props}
    />
  );
}
