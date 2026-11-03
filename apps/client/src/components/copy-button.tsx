import { useEffect, useRef, useState } from "react";
import { Icon } from "@/components/icon";
import { m } from "@/paraglide/messages.js";
import { cn } from "@/lib/utils";

/** 复制：写进剪贴板后图标变墨色停 1.2 秒（原型的 `.done`）。剪贴板不可用（非安全上下文、权限被拒）时不变色。 */
export function CopyButton({ text, label, className }: { text: string; label?: string; className?: string }) {
  const [done, setDone] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);
  const name = label ?? m.copy();
  return (
    <button
      type="button"
      aria-label={name}
      title={name}
      onClick={() => {
        void navigator.clipboard?.writeText(text).then(
          () => {
            setDone(true);
            clearTimeout(timer.current);
            timer.current = setTimeout(() => setDone(false), 1200);
          },
          () => {},
        );
      }}
      className={cn(
        "grid size-6.5 shrink-0 cursor-pointer place-items-center rounded-sm text-muted transition-colors hover:bg-hover-on-white hover:text-text",
        done && "text-text",
        className,
      )}
    >
      <Icon name="copy" className="size-3.5" />
    </button>
  );
}
