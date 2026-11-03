import type { ComponentProps, ReactNode } from "react";
import { Icon } from "@/components/icon";
import { IconButton } from "@/components/ui/icon-button";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";

// 事项页与定时页共用的小部件（原型的 `.pill`、`.kv`、`.notice`、`.text-btn`、`.sub-h`、`.detail-top`、表单控件）。

/** 小药丸：`wait` 琥珀、`bad` 红、`off` 灰字，默认纸色。 */
export function Pill({ tone, children, className }: { tone?: "wait" | "bad" | "off"; children: ReactNode; className?: string }) {
  return (
    <span
      className={cn(
        "inline-flex h-5 shrink-0 items-center rounded-full px-1.75 text-small font-medium whitespace-nowrap",
        tone === "wait" ? "bg-pill-wait-bg text-pill-wait-ink" : tone === "bad" ? "bg-pill-bad-bg text-pill-bad-ink" : "bg-pill-bg",
        tone === "off" ? "text-muted" : tone ? "" : "text-pill-ink",
        className,
      )}
    >
      {children}
    </span>
  );
}

/** 详情卡顶部：窄屏左边是返回，右边的药丸靠右，`onClose`（宽屏定时详情）给个关闭叉。 */
export function DetailTop({
  label,
  pill,
  compact,
  onBack,
  onClose,
}: {
  label: ReactNode;
  pill?: ReactNode;
  compact?: boolean;
  onBack?: () => void;
  onClose?: () => void;
}) {
  return (
    <div className="flex min-h-7 items-center gap-2">
      {compact && onBack && <IconButton name="left" label={m.matter_back()} onClick={onBack} className="-ml-2" />}
      <span className="font-medium">{label}</span>
      <span className="ml-auto flex items-center gap-2">
        {pill}
        {onClose && <IconButton name="x" label={m.close()} onClick={onClose} className="-my-1 -mr-2" />}
      </span>
    </div>
  );
}

/** 灰底键值井。 */
export function Kv({ rows, plain, className }: { rows: (readonly [string, ReactNode] | null | false)[]; plain?: boolean; className?: string }) {
  return (
    <dl
      className={cn(
        "grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 text-label leading-5",
        plain ? "mt-3" : "mt-4.5 rounded-control bg-raised px-3 py-2.5",
        className,
      )}
    >
      {rows.map((row) =>
        row ? (
          <div key={row[0]} className="col-span-2 grid grid-cols-subgrid">
            <dt className="py-1 text-muted">{row[0]}</dt>
            <dd className="m-0 min-w-0 py-1 [overflow-wrap:anywhere]">{row[1]}</dd>
          </div>
        ) : null,
      )}
    </dl>
  );
}

export function SubHeading({ children, count }: { children: ReactNode; count?: number }) {
  return (
    <h3 className="mt-4.5 mb-1 text-label font-semibold">
      {children}
      {count !== undefined && <span className="ml-1 font-normal text-muted tabular-nums">{count}</span>}
    </h3>
  );
}

export const Hint = ({ children, className }: { children: ReactNode; className?: string }) => (
  <p className={cn("mt-2.5 text-label leading-5 text-muted", className)}>{children}</p>
);

export const FormError = ({ children }: { children: ReactNode }) => <p className="mt-3 text-label text-negative">{children}</p>;

/** 回执：描边的灰底小块。 */
export function Notice({ children }: { children: ReactNode }) {
  return <div className="mt-3 rounded-control bg-raised px-3 py-2.5 text-label leading-5 shadow-ring">{children}</div>;
}

/** 文字按钮：`quiet` 灰字，`danger` 红字。 */
export function TextButton({ tone, className, ...props }: { tone?: "quiet" | "danger" } & ComponentProps<"button">) {
  return (
    <button
      type="button"
      className={cn(
        "cursor-pointer p-0 disabled:cursor-progress disabled:text-muted",
        tone === "quiet" && "text-label text-muted hover:text-text hover:underline hover:underline-offset-3",
        tone === "danger" && "text-label text-negative hover:underline hover:underline-offset-3",
        !tone && "font-medium text-text underline underline-offset-3",
        className,
      )}
      {...props}
    />
  );
}

/** 详情里的链接：文字加一个小箭头。 */
export function ArrowLink({ children }: { children: ReactNode }) {
  return (
    <span className="inline-flex items-center gap-0.5">
      {children}
      <Icon name="right" className="size-3" />
    </span>
  );
}

export const linkClass = "text-text no-underline hover:underline hover:underline-offset-3";

export const Label = ({ children, htmlFor }: { children: ReactNode; htmlFor?: string }) => (
  <label htmlFor={htmlFor} className="mt-3.5 mb-1.5 block text-label font-medium first:mt-0">
    {children}
  </label>
);

const control =
  "w-full rounded-control bg-surface px-3 text-text shadow-ring-control placeholder:text-faint focus:shadow-[0_0_0_1px_var(--color-text)] focus:outline-0";
export const Input = ({ className, ...props }: ComponentProps<"input">) => <input className={cn("h-9", control, className)} {...props} />;
export const Select = ({ className, ...props }: ComponentProps<"select">) => (
  <select className={cn("h-9 cursor-pointer px-2", control, className)} {...props} />
);

/** 单选一行：原生 radio，选中色跟主色。 */
export function Radio({ children, ...props }: { children: ReactNode } & Omit<ComponentProps<"input">, "type">) {
  return (
    <label className="flex min-h-7.5 cursor-pointer items-center gap-2 text-label">
      <input type="radio" className="m-0 accent-primary" {...props} />
      {children}
    </label>
  );
}
