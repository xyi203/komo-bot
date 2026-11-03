import type { ComponentProps, ReactNode } from "react";
import { Icon } from "@/components/icon";
import { IconButton } from "@/components/ui/icon-button";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";

// 事项、记忆、定时三页共用的小部件（原型的 `.pill`、`.kv`、`.notice`、`.text-btn`、`.sub-h`、`.detail-top`、表单控件）。
// 版式照 Claude 桌面的设置页：一节一个标题，下面一行一项，行与行之间只有一条细线，不再叠表头线、节线与灰底井。

/** 列表：行间一条细线，首行上、末行下都不画。 */
export const listClass = "relative z-0 divide-y divide-rule";
/** 列表的一行：悬停与选中的底色画在伪元素上，向两侧各探出 8px，不挤正文。 */
export const rowClass =
  "relative py-3.5 before:absolute before:inset-x-[-8px] before:inset-y-0.5 before:-z-10 before:rounded-control before:transition-colors hover:before:bg-hover-on-white";
export const rowOnClass = "before:bg-active hover:before:bg-active";

/** 一节的标题（节标题字号，计数灰字）。 */
export function SectionTitle({ children, count, className }: { children: ReactNode; count?: ReactNode; className?: string }) {
  return (
    <h2 className={cn("m-0 text-section-title font-semibold", className)}>
      {children}
      {count !== undefined && <span className="ml-1.5 text-body font-normal text-muted tabular-nums">{count}</span>}
    </h2>
  );
}

/** 列名：灰色小字，跟在节标题那一行里，底对齐（英文在窄列里会折成两行，随内容长高，不定高）。 */
export const columnLabelClass = "text-small leading-4 text-muted max-[600px]:hidden";

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

/** 键值：一行一项，左边灰色标签、右边值，行间细线。 */
export function Kv({ rows, className }: { rows: (readonly [string, ReactNode] | null | false)[]; className?: string }) {
  return (
    <dl className={cn("mt-4 divide-y divide-rule border-t border-rule text-label leading-5", className)}>
      {rows.map((row) =>
        row ? (
          <div key={row[0]} className="flex items-baseline justify-between gap-4 py-2.5">
            <dt className="shrink-0 text-muted">{row[0]}</dt>
            <dd className="m-0 min-w-0 text-right [overflow-wrap:anywhere]">{row[1]}</dd>
          </div>
        ) : null,
      )}
    </dl>
  );
}

export function SubHeading({ children, count }: { children: ReactNode; count?: number }) {
  return (
    <h3 className="mt-6 mb-1 text-label font-semibold">
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
