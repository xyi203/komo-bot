import { Markdown } from "@/components/markdown";
import { ChangesCard } from "@/features/changes/changes-card";
import { m } from "@/paraglide/messages.js";
import { UserBubble } from "./user-bubble";
import { InlineProcess, LiveMark } from "./inline-process";
import { useConversation } from "./context";
import type { Turn } from "./model";
import { ReplyTools } from "./tools";
import type { Delta } from "@komo/api";

/**
 * 一轮（一条 Run）：用户的话在上，Komo 的回复在下，回复下面一行悬停工具（`tools.tsx`）。
 * 过程（思考、工具调用、中间说的话）按时间顺序排在回复上方（[`InlineProcess`]，照 Claude Desktop）；
 * 跑着时回复末尾是 Komo 轨道（大圆不动、两个小圆绕着转）。召回、轨迹、改动的细节在右侧的过程面板（`features/process/`）。
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
  const mine = live && !turn.answer && delta?.run === turn.run ? delta : null;
  const streaming = !!mine?.text;

  return (
    <section className="group/msg mb-5">
      {turn.users.map((u, i) => (
        <UserBubble key={i} text={u.text} at={u.at} />
      ))}
      <InlineProcess turn={turn} streaming={streaming} thinking={mine?.thinking} />
      {turn.answer ? (
        <Markdown text={turn.answer.text} className="mt-4" />
      ) : (
        streaming && <Markdown text={mine?.text ?? ""} className="streaming mt-4" />
      )}
      {live && <LiveMark />}
      {card}
      {turn.status === "failed" && <p className="mt-4 mb-1 text-label text-negative">{m.turn_failed_note({ reason: turn.reason ?? m.turn_no_reason() })}</p>}
      {turn.status === "cancelled" && <p className="mt-4 mb-1 text-label text-muted">{m.turn_cancelled_note()}</p>}
      {showTools && <ReplyTools text={turn.answer?.text} run={hasProcess ? turn.run : undefined} recall={turn.recall} always={always} />}
    </section>
  );
}
