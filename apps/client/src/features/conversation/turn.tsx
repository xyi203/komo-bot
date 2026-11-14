import { CopyButton } from "@/components/copy-button";
import { Markdown } from "@/components/markdown";
import { ChangesCard } from "@/features/changes/changes-card";
import { formatStamp } from "@/lib/format";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { AttachmentList, splitAttachments } from "./attachments";
import { useConversation } from "./context";
import type { Turn } from "./model";
import { ReplyTools } from "./tools";
import type { Delta } from "@komo/api";

export function UserBubble({ text, at, standalone = false }: { text: string; at?: string; standalone?: boolean }) {
  const { attachments, text: words } = splitAttachments(text);
  return (
    <div className={cn("flex flex-col items-end gap-1", standalone && "mt-6 mb-2")}>
      <AttachmentList attachments={attachments} />
      {words.trim() && (
        <div className="max-w-[80%] rounded-[16px] bg-raised px-3.5 py-2 text-read leading-6 break-words whitespace-pre-wrap max-[600px]:max-w-[88%]">{words}</div>
      )}
      <div className="mt-0.5 flex items-center gap-1.5 text-small text-muted">
        <span className="tabular-nums">{at ? formatStamp(at) : ""}</span>
        {/* 复制的是用户写的话；只有附件时复制整段（含存放路径） */}
        <CopyButton text={words.trim() ? words : text} />
      </div>
    </div>
  );
}

/**
 * 一轮（一条 Run）：用户的话在上，Komo 的回复在下，回复下面一行悬停工具（`tools.tsx`）。
 * 过程（思考、工具调用、召回、插话）不在对话正文里，在右侧的过程面板（`features/process/`）；正在跑时的"正在输入"由 `Thread` 画在消息末尾。
 * 失败 / 被取消时在回复的位置给一句说明。
 */
export function TurnView({ turn, delta, always }: { turn: Turn; delta: Delta | null; always: boolean }) {
  const { changes, review } = useConversation();
  const live = turn.status === "running" || turn.status === "queued";
  const changesOfRun = changes.get(turn.run);
  const card = changesOfRun && !live ? <ChangesCard changes={changesOfRun} onReview={review} /> : null;
  // 有过程可看（思考、调用、召回……）才有"看过程"；只有一句回复的一轮没有
  const hasProcess = turn.process.length > 0 || !!changesOfRun?.files.length;
  const showTools = !live && (!!turn.answer || hasProcess);

  return (
    <section className="group/msg mb-5">
      {turn.users.map((u, i) => (
        <UserBubble key={i} text={u.text} at={u.at} />
      ))}
      {turn.answer ? (
        <Markdown text={turn.answer.text} className="mt-4" />
      ) : (
        live && delta && <Markdown text={delta.text} className="streaming mt-4" />
      )}
      {card}
      {turn.status === "failed" && <p className="mt-4 mb-1 text-label text-negative">{m.turn_failed_note({ reason: turn.reason ?? m.turn_no_reason() })}</p>}
      {turn.status === "cancelled" && <p className="mt-4 mb-1 text-label text-muted">{m.turn_cancelled_note()}</p>}
      {showTools && <ReplyTools text={turn.answer?.text} run={hasProcess ? turn.run : undefined} recall={turn.recall} always={always} />}
    </section>
  );
}
