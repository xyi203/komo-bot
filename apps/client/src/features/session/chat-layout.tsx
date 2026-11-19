import { Link } from "@tanstack/react-router";
import type { ReactNode, RefObject } from "react";
import { Icon } from "@/components/icon";
import { useConnection } from "@/lib/connection";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";

/**
 * 对话页的外框：左上角的名字，正文区，下面的输入框；`aside` 是桌面壳的内置浏览器（有才出现）。
 * 名字照 Claude 桌面端放左上角、不加框，`title` 给了就用它代替纯文字（对话页是带下拉箭头的对话菜单）。
 * 名字浮在正文上面（不占高度），与右上角的按钮同高对齐；正文顶上留出它的位置，再叠一段白到透明的渐隐，滚上去的字不会在名字后面被切成半行。
 */
export function ChatLayout({
  name,
  title,
  scroller,
  onScroll,
  aside,
  panel,
  corner,
  composer,
  children,
}: {
  name: string;
  /** 代替纯文字名字的可点标题（对话菜单的触发器）。 */
  title?: ReactNode;
  scroller?: RefObject<HTMLDivElement | null>;
  onScroll?: (el: HTMLDivElement) => void;
  aside?: ReactNode;
  /** 过程面板（有才出现）：宽 380 占一列；≤1180 时面板自己浮在上面，不占列。 */
  panel?: ReactNode;
  /** 右上角的小按钮（桌面壳的浏览器开关）。 */
  corner?: ReactNode;
  composer: ReactNode;
  children: ReactNode;
}) {
  return (
    <div
      className={cn(
        "grid h-full min-h-0 overflow-hidden",
        aside
          ? panel
            ? "grid-cols-[minmax(0,1fr)_minmax(0,1fr)_380px] max-[1180px]:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]"
            : "grid-cols-[minmax(0,1fr)_minmax(0,1fr)]"
          : panel
            ? "grid-cols-[minmax(0,1fr)_380px] max-[1180px]:grid-cols-[minmax(0,1fr)]"
            : "grid-cols-[minmax(0,1fr)]",
      )}
    >
      <div className="relative flex min-h-0 min-w-0 flex-col">
        <div aria-hidden="true" className="pointer-events-none absolute inset-x-0 top-0 z-10 h-14 bg-linear-to-b from-surface from-30% to-transparent" />
        {/* 窄屏：左上角返回对话列表（单栏下对话页是全屏的，没有别处可回） */}
        <Link
          to="/chats"
          aria-label={m.nav_back_chats()}
          title={m.nav_back_chats()}
          className="absolute top-2 left-1 z-20 hidden size-11 place-items-center rounded-full text-muted no-underline transition-colors active:bg-hover narrow:grid"
        >
          <Icon name="left" className="size-5" />
        </Link>
        <header className="absolute top-3 left-4 z-20 flex h-8 max-w-[60%] items-center narrow:top-2.5 narrow:left-12">
          <h1 className={cn("max-w-full min-w-0 text-body font-medium", title ? "flex" : "truncate px-2")}>{title ?? name}</h1>
        </header>
        {corner && <div className="absolute top-3 right-4 z-20 flex gap-1 narrow:top-2.5 narrow:right-2">{corner}</div>}
        <div
          ref={scroller}
          onScroll={onScroll ? (event) => onScroll(event.currentTarget) : undefined}
          className="min-h-0 flex-1 overflow-auto px-8 pt-14 pb-6 max-[600px]:px-4 max-[600px]:pb-4"
        >
          <div className="mx-auto max-w-180">{children}</div>
        </div>
        <ConnectionNotice />
        {composer}
      </div>
      {aside}
      {panel}
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

/** 输入框上方的一行连接提示：连不上或事件流断了才出现，连上后自己消失；不挡输入，发送失败有各自的重试。 */
function ConnectionNotice() {
  const problem = useConnection();
  if (!problem) return null;
  return (
    <p role="status" className="shrink-0 px-8 pb-2 text-center text-label text-muted max-[600px]:px-3">
      <span aria-hidden="true" className={cn("mr-2 inline-block size-1.5 rounded-full align-middle", problem === "down" ? "bg-negative" : "bg-wait-dot")} />
      {problem === "down" ? m.conn_notice_down() : m.conn_retrying()}
    </p>
  );
}
