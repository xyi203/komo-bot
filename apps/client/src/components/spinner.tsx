import { cn } from "@/lib/utils";

/** 转圈：只表示"在跑"。`sm` 是行内小号。 */
export function Spinner({ small = false, className }: { small?: boolean; className?: string }) {
  return (
    <span
      aria-hidden="true"
      className={cn(
        "inline-block shrink-0 animate-spinner rounded-full border-accent-pale border-t-accent align-middle",
        small ? "size-3 border-[1.5px]" : "size-3.5 border-2",
        className,
      )}
    />
  );
}
