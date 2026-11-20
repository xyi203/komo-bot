import { useCallback, useRef, useState, type RefObject } from "react";
import { readStored, writeStored } from "@/lib/storage";
import { m } from "@/paraglide/messages.js";

// 过程面板的宽度：拖左边缘调，记在这个浏览器的 localStorage（`komo.processWidth`），所有对话共用一个。
// 拖动中只改面板上的 CSS 变量 `--proc-w`，不重渲染面板（改动对比、轨迹可能很长）；松手再写进 state 与 localStorage。

const KEY = "komo.processWidth";
export const PANEL_DEFAULT = 380;
const MIN = 320;
const STEP = 16;
// 留给对话列的最小宽度：面板再宽就把正文挤得没法读
const CHAT_MIN = 420;

const clamp = (width: number, max: number) => Math.round(Math.min(Math.max(width, MIN), Math.max(MIN, max)));

export function usePanelWidth() {
  const [width, setWidth] = useState(() =>
    readStored(KEY, (raw) => (typeof raw === "number" && Number.isFinite(raw) ? clamp(raw, Infinity) : undefined), PANEL_DEFAULT),
  );
  const commit = useCallback((next: number) => {
    setWidth(next);
    writeStored(KEY, String(next));
  }, []);
  return [width, commit] as const;
}

/** 面板最多能多宽：网格里占列时给对话列留 `CHAT_MIN`，浮在上面时（≤1180）留一小条看得见底下的对话。 */
function maxWidth(panel: HTMLElement): number {
  const grid = panel.parentElement;
  if (getComputedStyle(panel).position === "fixed" || !grid) return window.innerWidth - 48;
  // 网格里除了面板还有对话列，可能还有内置浏览器：浏览器那列和对话列平分剩下的
  const others = grid.children.length - 1;
  return grid.getBoundingClientRect().width - CHAT_MIN * Math.max(1, others);
}

/**
 * 面板左边缘的拖拽条（窄屏 ≤900 面板盖满整屏，不出现）。拖动改宽，双击回默认，聚焦后左右方向键每次 16px。
 */
export function ResizeEdge({ panel, width, onWidth }: { panel: RefObject<HTMLElement | null>; width: number; onWidth: (width: number) => void }) {
  const drag = useRef<{ x: number; width: number; max: number; last: number } | null>(null);
  const set = (next: number) => panel.current?.style.setProperty("--proc-w", `${next}px`);
  return (
    <div
      role="separator"
      aria-orientation="vertical"
      aria-label={m.proc_resize()}
      title={m.proc_resize()}
      aria-valuenow={width}
      aria-valuemin={MIN}
      tabIndex={0}
      className="group/edge absolute inset-y-0 -left-1.5 z-10 w-3 cursor-col-resize touch-none outline-none max-[900px]:hidden"
      onPointerDown={(event) => {
        if (event.button !== 0 || !panel.current) return;
        event.preventDefault();
        event.currentTarget.setPointerCapture(event.pointerId);
        drag.current = { x: event.clientX, width, max: maxWidth(panel.current), last: width };
        document.body.style.cursor = "col-resize";
        document.body.style.userSelect = "none";
      }}
      onPointerMove={(event) => {
        const d = drag.current;
        if (!d) return;
        // 面板在右边：往左拖变宽
        d.last = clamp(d.width + d.x - event.clientX, d.max);
        set(d.last);
      }}
      onPointerUp={(event) => {
        const d = drag.current;
        if (!d) return;
        drag.current = null;
        event.currentTarget.releasePointerCapture(event.pointerId);
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
        onWidth(d.last);
      }}
      onPointerCancel={() => {
        drag.current = null;
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
        set(width);
      }}
      onDoubleClick={() => {
        set(PANEL_DEFAULT);
        onWidth(PANEL_DEFAULT);
      }}
      onKeyDown={(event) => {
        if (!panel.current || (event.key !== "ArrowLeft" && event.key !== "ArrowRight")) return;
        event.preventDefault();
        const next = clamp(width + (event.key === "ArrowLeft" ? STEP : -STEP), maxWidth(panel.current));
        set(next);
        onWidth(next);
      }}
    >
      {/* 悬停、拖动、聚焦时亮出一条细线，平时只有面板自己的左边框 */}
      <span className="pointer-events-none absolute inset-y-0 left-1/2 w-0.5 -translate-x-1/2 bg-transparent transition-colors group-hover/edge:bg-control group-focus-visible/edge:bg-focus group-active/edge:bg-focus" />
    </div>
  );
}
