import type { ReactNode, RefObject } from "react";

/**
 * 对话页的外框：顶上居中的头像（Komo 标志）与名字，正文区，下面的输入框；`aside` 是桌面壳的内置浏览器（有才出现）。
 * 头像与名字浮在正文上面（不占高度），正文顶上留出它的位置，再叠一段白到透明的渐隐，滚上去的字不会在头像后面被切成半行。
 */
export function ChatLayout({
  name,
  scroller,
  onScroll,
  aside,
  corner,
  composer,
  children,
}: {
  name: string;
  scroller?: RefObject<HTMLDivElement | null>;
  onScroll?: (el: HTMLDivElement) => void;
  aside?: ReactNode;
  /** 右上角的小按钮（桌面壳的浏览器开关）。 */
  corner?: ReactNode;
  composer: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className={aside ? "grid h-full min-h-0 grid-cols-[minmax(0,1fr)_minmax(0,1fr)] overflow-hidden" : "grid h-full min-h-0 grid-cols-[minmax(0,1fr)] overflow-hidden"}>
      <div className="relative flex min-h-0 min-w-0 flex-col">
        <div aria-hidden="true" className="pointer-events-none absolute inset-x-0 top-0 z-10 h-18 bg-linear-to-b from-surface from-30% to-transparent" />
        <header className="pointer-events-none absolute top-2.5 left-1/2 z-20 flex max-w-[60%] -translate-x-1/2 flex-col items-center gap-0.5">
          <span className="grid size-11 place-items-center rounded-full bg-surface shadow-[0_0_0_1px_rgb(0_0_0/0.08),0_2px_8px_rgb(0_0_0/0.06)]">
            <svg viewBox="0 0 28 28" aria-hidden="true" className="size-6.5">
              <circle cx="11" cy="12" r="8" className="fill-accent" />
              <circle cx="20.5" cy="18.5" r="4.5" className="fill-accent" fillOpacity="0.55" />
              <circle cx="20" cy="7" r="2.5" className="fill-accent" fillOpacity="0.3" />
            </svg>
          </span>
          <h1 className="max-w-full truncate rounded-[10px] bg-surface px-2.5 py-px text-small font-medium shadow-[0_0_0_1px_rgb(0_0_0/0.06)]">{name}</h1>
        </header>
        {corner && <div className="absolute top-3 right-4 z-20 flex gap-1">{corner}</div>}
        <div
          ref={scroller}
          onScroll={onScroll ? (event) => onScroll(event.currentTarget) : undefined}
          className="min-h-0 flex-1 overflow-auto px-8 pt-24 pb-6 max-[600px]:px-4 max-[600px]:pt-22 max-[600px]:pb-4"
        >
          <div className="mx-auto max-w-180">{children}</div>
        </div>
        {composer}
      </div>
      {aside}
    </div>
  );
}

/** 加载：最终布局里放平的灰条，不做微光。 */
export function SkeletonThread() {
  const bar = (width: string, extra = "") => <span className={`mb-3 block h-3.5 rounded-xs bg-active ${extra}`} style={{ width }} />;
  return (
    <div className="pt-6">
      <div className="mb-5 flex justify-end">
        <span className="block h-10 rounded-[16px] bg-active" style={{ width: "46%" }} />
      </div>
      {bar("92%")}
      {bar("78%")}
      {bar("40%")}
      {bar("30%", "mt-4.5")}
      {bar("88%")}
      {bar("64%")}
    </div>
  );
}
