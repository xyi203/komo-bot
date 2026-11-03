import type { ReactNode } from "react";
import { NavToggle } from "@/features/layout/nav-toggle";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";

/** 整页的外框与页头（待处理、事项、记忆、定时共用）：窄屏（≤900）"打开导航"按钮单占一行。 */
export function Page({
  title,
  finding,
  actions,
  bare,
  children,
}: {
  title: string;
  finding?: ReactNode;
  actions?: ReactNode;
  /** 不画页头（窄屏详情盖住整页时）。 */
  bare?: boolean;
  children: ReactNode;
}) {
  return (
    <section aria-label={title} className="px-8 pt-7 pb-10 max-[600px]:px-4 max-[600px]:pt-4 max-[600px]:pb-8">
      <header hidden={bare} className="mb-5 grid grid-cols-[minmax(0,1fr)_auto] items-start gap-x-3">
        <div className="col-span-full -mt-3 -ml-2 empty:hidden">
          <NavToggle />
        </div>
        <div className="min-w-0">
          <h1 className="text-page-title font-semibold tracking-tight">{title}</h1>
          {finding && <p className="mt-0.5 text-read text-muted">{finding}</p>}
        </div>
        {actions}
      </header>
      {children}
    </section>
  );
}

/** 一页里的"列表 + 详情"：宽屏并排，列表只占左边；窄屏（≤1180）详情盖住列表（由页面决定渲染哪一个）。 */
export function PageBody({ columns, children }: { columns: string; children: ReactNode }) {
  return <div className={cn("grid items-start gap-6", columns)}>{children}</div>;
}

/** 右边的详情卡：宽屏贴顶；窄屏去掉卡片外观，自己撑满。 */
export function DetailCard({ label, compact, children }: { label: string; compact?: boolean; children: ReactNode }) {
  return (
    <aside aria-label={label} className={cn("min-w-0", compact ? "max-w-160" : "sticky top-0 rounded-card bg-surface shadow-lift")}>
      {children}
    </aside>
  );
}

/** 虚线框：空与没有选中时的说明。 */
export function EmptyFrame({ children, className }: { children: ReactNode; className?: string }) {
  return <p className={cn("my-3 rounded-control border border-dashed border-control px-3.5 py-2.5 text-label text-muted", className)}>{children}</p>;
}

/** 加载：最终布局里放平的灰条，不做微光。 */
export function SkeletonRows({ count = 5, className }: { count?: number; className?: string }) {
  return (
    <div aria-hidden="true">
      {Array.from({ length: count }, (_, i) => (
        <div key={i} className={cn("flex h-14 items-center border-b border-rule", className)}>
          <span className="block h-3 rounded-xs bg-active" style={{ width: `${[72, 56, 64, 48, 60][i % 5]}%` }} />
        </div>
      ))}
    </div>
  );
}

/** 出错留在出问题的那一块：一句原因与重试。 */
export function BlockError({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="m-2 rounded-control border border-dashed border-control p-3 text-label text-muted">
      <p className="mb-1.5">{message}</p>
      <button type="button" onClick={onRetry} className="cursor-pointer font-medium text-text underline underline-offset-3">
        {m.retry()}
      </button>
    </div>
  );
}
