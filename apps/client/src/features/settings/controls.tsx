import type { ComponentProps, ReactNode } from "react";
import { cn } from "@/lib/utils";

// 设置各节共用的表单控件与行（原型 styles.css 的 .switch / .input / .select / .tag / .kstate / .notice）。
// 模型节与其余六节都从这里取，样式只在这一处。

/** 一项设置的一行（照 Claude 桌面设置页，不是卡片）：左边名字与说明，右边控件；行间一条细线。 */
export function SettingsCard({ children, className }: { children: ReactNode; className?: string }) {
  return <section className={cn("min-w-0 border-b border-rule py-4 last:border-b-0 max-[600px]:py-3.5", className)}>{children}</section>;
}

/** 卡片里的一行：左文右控件；窄屏控件落到文字下面（`inline` 的开关除外）。 */
export function CardRow({ title, desc, control, inline, mono }: { title: ReactNode; desc?: ReactNode; control?: ReactNode; inline?: boolean; mono?: boolean }) {
  return (
    <div className={cn("flex items-center gap-6 max-[600px]:gap-3", !inline && "max-[600px]:flex-col max-[600px]:items-stretch")}>
      <div className="min-w-0 flex-1">
        <h4 className={cn("m-0 text-read leading-5.5 font-normal", mono && "font-mono")}>{title}</h4>
        {desc && <p className="mt-1 text-label leading-5 text-muted">{desc}</p>}
      </div>
      {control && <div className="flex shrink-0 items-center gap-2 max-[600px]:[&_select]:min-w-0 max-[600px]:[&_select]:flex-1">{control}</div>}
    </div>
  );
}

/** 面板（键值、操作条、一句空状态）：不画框，直接落在节标题下面。 */
export function Panel({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cn("min-w-0 py-1", className)}>{children}</div>;
}

/** 节头右边按钮的一行：右上角的操作。 */
export function SectionActions({ children }: { children: ReactNode }) {
  return <div className="mb-3 flex flex-wrap items-center justify-end gap-2">{children}</div>;
}

/** 表单项：标签、控件、一句提示。 */
export function Field({ label, htmlFor, hint, children, className }: { label: ReactNode; htmlFor?: string; hint?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <div className={cn("flex min-w-0 flex-col gap-1.5", className)}>
      <label htmlFor={htmlFor} className="text-label font-medium text-text">
        {label}
      </label>
      {children}
      {hint && <p className="text-small leading-4.5 text-muted">{hint}</p>}
    </div>
  );
}

const fieldBase =
  "h-9 min-w-0 rounded-control bg-surface px-3 text-text shadow-ring-control outline-none placeholder:text-faint focus:shadow-[0_0_0_2px_var(--color-primary)] disabled:cursor-progress disabled:text-muted";

export function TextInput({ mono, className, ...props }: { mono?: boolean } & ComponentProps<"input">) {
  return <input className={cn(fieldBase, mono && "font-mono text-label", className)} {...props} />;
}

/** 数字框：数字用等宽的对齐（`tabular-nums`），宽度 120。 */
export function NumberInput({ className, ...props }: ComponentProps<"input">) {
  return <input type="number" className={cn(fieldBase, "w-30 tabular-nums max-[600px]:w-auto max-[600px]:flex-1", className)} {...props} />;
}

/** 原生下拉（原型就是原生 select，手机上的选择器也更好用）。 */
export function NativeSelect({ className, ...props }: ComponentProps<"select">) {
  return <select className={cn(fieldBase, "h-9 min-w-55 cursor-pointer pr-2", className)} {...props} />;
}

/** 开关：一改就生效，所以 `disabled` 期间要给一个"在写"的外观（半透明）。 */
export function Switch({ checked, onChange, label, disabled }: { checked: boolean; onChange: (next: boolean) => void; label: string; disabled?: boolean }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={cn(
        "relative h-6 w-10 shrink-0 cursor-pointer rounded-xl border-0 p-0 transition-colors disabled:cursor-progress disabled:opacity-60",
        checked ? "bg-primary" : "bg-control",
      )}
    >
      <i
        className={cn(
          "absolute top-0.75 left-0.75 size-4.5 rounded-full bg-white shadow-[0_1px_2px_rgb(0_0_0/0.2)] transition-transform duration-150 ease-out-expo",
          checked && "translate-x-4",
        )}
      />
    </button>
  );
}

/** 两段（或多段）切换：模型节的"对话 / 嵌入"、"第三方 / 自定义"。 */
export function Segmented<T extends string>({
  value,
  options,
  onChange,
  label,
  disabled,
}: {
  value: T;
  options: { value: T; label: ReactNode }[];
  onChange: (next: T) => void;
  label: string;
  disabled?: boolean;
}) {
  return (
    <div role="radiogroup" aria-label={label} className="inline-flex rounded-control bg-raised p-0.5">
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          role="radio"
          aria-checked={option.value === value}
          disabled={disabled}
          onClick={() => onChange(option.value)}
          className={cn(
            "h-7 cursor-pointer rounded-sm px-3 text-label whitespace-nowrap text-muted transition-colors disabled:cursor-not-allowed disabled:opacity-60",
            option.value === value ? "bg-surface font-medium text-text shadow-ring" : "hover:text-text",
          )}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

/** 小标签（"自定义""OAuth""嵌入"）。 */
export function Tag({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <span className={cn("ml-1.5 inline-flex h-4.5 items-center rounded-sm px-1.5 text-[11px] font-normal text-muted shadow-ring", className)}>{children}</span>
  );
}

/** 状态药丸：对外（琥珀）、只读（灰）、错误（红）。 */
export function Pill({ tone = "plain", children }: { tone?: "plain" | "wait" | "bad"; children: ReactNode }) {
  return (
    <span
      className={cn(
        "inline-flex h-5 items-center rounded-full px-2 text-small whitespace-nowrap",
        tone === "plain" && "bg-pill-bg text-pill-ink",
        tone === "wait" && "bg-pill-wait-bg text-pill-wait-ink",
        tone === "bad" && "bg-pill-bad-bg text-pill-bad-ink",
      )}
    >
      {children}
    </span>
  );
}

/** 状态点 + 文字：ok 叶绿、warn 琥珀、bad 红、其余灰点；`spin` 时换成转圈。 */
export function KState({ tone = "idle", children }: { tone?: "ok" | "warn" | "bad" | "idle"; children: ReactNode }) {
  return (
    <span className={cn("inline-flex items-center gap-1.5 text-small text-muted", tone === "warn" && "text-pill-wait-ink", tone === "bad" && "text-negative")}>
      <i
        className={cn(
          "size-1.75 rounded-full bg-faint",
          tone === "ok" && "bg-accent",
          tone === "warn" && "bg-wait-dot",
          tone === "bad" && "bg-negative",
        )}
      />
      {children}
    </span>
  );
}

/** 写完之后的一句回执（灰底），出错用红字。 */
export function Notice({ children, bad, className }: { children: ReactNode; bad?: boolean; className?: string }) {
  return (
    <div className={cn("mt-3 rounded-control bg-raised px-3 py-2.5 text-label leading-5 shadow-ring [&_p+p]:mt-1.5", bad && "text-negative", className)}>{children}</div>
  );
}

/** 表单下面的失败原因。 */
export function FormError({ children, className }: { children: ReactNode; className?: string }) {
  return <p className={cn("mt-2.5 text-label leading-5 text-negative", className)}>{children}</p>;
}

/** 一句提示（灰色小字）。 */
export function Hint({ children, bad, className }: { children: ReactNode; bad?: boolean; className?: string }) {
  return <p className={cn("mt-2.5 text-label leading-5 text-muted", bad && "text-negative", className)}>{children}</p>;
}

/** 虚线的"添加…"整行按钮。 */
export function AddCard({ children, ...props }: ComponentProps<"button">) {
  return (
    <button
      type="button"
      className="mt-3 flex h-12 w-full cursor-pointer items-center justify-center gap-1.5 rounded-card border border-dashed border-control bg-transparent text-muted transition-colors hover:bg-hover-on-white hover:text-text"
      {...props}
    >
      {children}
    </button>
  );
}

/** 键值块：一行一项，左边 `--muted` 的键、右边值，行间细线（和设置行同一套）。 */
export function KeyValues({ rows }: { rows: ([ReactNode, ReactNode] | null | false)[] }) {
  return (
    <dl className="m-0">
      {rows.map((row) =>
        row ? (
          <div key={String(row[0])} className="grid grid-cols-[128px_minmax(0,1fr)] gap-x-4 border-b border-rule py-3 last:border-b-0 max-[600px]:grid-cols-[88px_minmax(0,1fr)]">
            <dt className="text-muted">{row[0]}</dt>
            <dd className="m-0 min-w-0 [overflow-wrap:anywhere]">{row[1]}</dd>
          </div>
        ) : null,
      )}
    </dl>
  );
}

/** 行内的文字按钮（"恢复默认""改值"）。 */
export function TextButton({ className, quiet, ...props }: { quiet?: boolean } & ComponentProps<"button">) {
  return (
    <button
      type="button"
      className={cn("cursor-pointer text-label font-medium underline-offset-3 hover:underline disabled:cursor-progress disabled:text-muted", quiet ? "text-muted hover:text-text" : "text-text", className)}
      {...props}
    />
  );
}
