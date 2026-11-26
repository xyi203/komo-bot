import { useRef, type CSSProperties, type ReactNode } from "react";
import { ResizeEdge, usePanelWidth } from "./resize";

/**
 * 对话右侧那一栏的外壳：过程面板和面板入口共用，宽度同一个（拖左边缘调、记在本机）。
 * >1180 占网格一列；≤1180 浮在对话上面；窄屏 ≤900 盖满整屏、从右滑入。
 */
export function PanelShell({ label, children }: { label: string; children: ReactNode }) {
  const ref = useRef<HTMLElement>(null);
  const [width, setWidth] = usePanelWidth();
  return (
    <aside
      ref={ref}
      aria-label={label}
      style={{ "--proc-w": `${width}px` } as CSSProperties}
      className="relative flex min-h-0 w-(--proc-w) max-w-[calc(100vw-720px)] flex-col border-l border-border bg-surface max-[1180px]:fixed max-[1180px]:inset-y-0 max-[1180px]:right-0 max-[1180px]:z-20 max-[1180px]:max-w-[calc(100vw-48px)] max-[1180px]:shadow-[-16px_0_48px_rgb(0_0_0/0.12)] max-[900px]:inset-x-0 max-[900px]:top-[var(--app-top,0px)] max-[900px]:bottom-auto max-[900px]:z-30 max-[900px]:h-[var(--app-height,100dvh)] max-[900px]:w-full max-[900px]:max-w-none max-[900px]:animate-[sheet-in-right_220ms_var(--ease-out-expo)] max-[900px]:border-l-0 max-[900px]:shadow-none"
    >
      <ResizeEdge panel={ref} width={width} onWidth={setWidth} />
      {children}
    </aside>
  );
}

/** 面板标题栏里的小按钮（上一轮 / 下一轮 / 关闭）。 */
export const panelNavClass =
  "grid size-7 max-[900px]:size-11 cursor-pointer place-items-center rounded-control text-muted transition-colors hover:bg-hover-on-white hover:text-text disabled:cursor-default disabled:opacity-35 disabled:hover:bg-transparent";
