import { Markdown } from "@/components/markdown";
import { m } from "@/paraglide/messages.js";
import type { Turn } from "./model";
import { segments } from "./process";
import { useEffect, useRef, useState } from "react";
import { Icon } from "@/components/icon";
import { cn } from "@/lib/utils";
import { StepsFold } from "./steps";

/**
 * 一轮的过程放进回复里，照 Claude Desktop 的做法：思考与工具调用按时间顺序排在回复上方，
 * 相邻的并成一组、一组一行（点开看每一步），中间说的话照常穿插在组之间；在跑的那组标题扫光，
 * 写的是此刻在做什么。还没有任何过程时是一行「正在思考」（排队时「排队中」）。
 *
 * 召回在回复下面的「用了 N 条记忆」里，插话已经是上面的用户气泡，这里不重复。
 * 右侧过程面板仍在（回复下的「查看过程」）：那里有轨迹、改动与这一轮的完整统计。
 */
export function InlineProcess({ turn, streaming, thinking = "" }: { turn: Turn; streaming: boolean; thinking?: string }) {
  const live = turn.status === "running" || turn.status === "queued";
  const parts = segments(turn.process).filter((part) => part.kind === "steps" || part.kind === "text");
  const lastIndex = parts.length - 1;
  // 这一轮的思考正在流进来：由下面的思考框说话，上一组过程不再标「在跑」
  const thinkingLive = live && !!thinking;
  // 在跑、回复还没流出来、思考也没在流、最后也不是一组在跑的过程：补一行「正在思考」
  const pending = live && !streaming && !thinkingLive && parts[lastIndex]?.kind !== "steps";
  if (!parts.length && !pending && !thinkingLive) return null;
  return (
    <div className="mt-3">
      {parts.map((part, i) =>
        part.kind === "steps" ? (
          <div key={part.id} className="process-in">
            <StepsFold seg={part} live={live && i === lastIndex && !streaming && !thinkingLive} />
          </div>
        ) : part.kind === "text" ? (
          <Markdown key={part.id} text={part.text} className="process-in my-3" />
        ) : null,
      )}
      {thinkingLive && <LiveThinking text={thinking} answering={streaming} />}
      {pending && <LiveRow label={turn.status === "queued" ? m.live_queued() : m.cat_thinking_live()} />}
    </div>
  );
}

/**
 * 正在流进来的思考（照 Claude Desktop）：标题行「正在思考」扫光，下面一个限高的小框，文字边来边往上滚，
 * 顶上渐隐。回复正文开始流出来时标题变成「思考过」、框自动收起（用户自己点过就听用户的）。
 * 这一轮结束（`message.assistant` 到了）它就换成过程里那一组的一行。
 */
function LiveThinking({ text, answering }: { text: string; answering: boolean }) {
  const [toggled, setToggled] = useState<boolean | null>(null);
  const open = toggled ?? !answering;
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = box.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [text, open]);
  return (
    <div className="process-in my-1">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setToggled(!open)}
        className="flex min-h-7 w-full cursor-pointer items-center gap-2 text-left text-body text-faint hover:text-text"
      >
        <Icon name={open ? "chev-up" : "think"} />
        <span className={cn(!answering && "text-shimmer")}>{answering ? m.cat_thinking_done() : m.cat_thinking_live()}</span>
      </button>
      {open && (
        <div
          ref={box}
          className="thinking-box mt-1 ml-6 max-h-30 overflow-hidden text-label leading-5 whitespace-pre-wrap text-muted"
        >
          {text.trim()}
        </div>
      )}
    </div>
  );
}

/** 还没有过程时的那一行：与组头同样的排法（思考图标 + 文字），文字扫光。 */
function LiveRow({ label }: { label: string }) {
  return (
    <div role="status" className="process-in flex min-h-7 items-center gap-2 text-body text-faint">
      <Icon name="think" />
      <span className="text-shimmer">{label}</span>
    </div>
  );
}

/** 一轮在跑时回复末尾的 Komo 轨道：大圆不动，两个小圆绕着它转（`styles.css` 的 `.live-orbit`）。 */
export function LiveMark() {
  return (
    <span role="img" aria-label={m.typing_label()} className="live-orbit mt-3 block">
      <b />
      <span>
        <i />
        <i />
      </span>
    </span>
  );
}
