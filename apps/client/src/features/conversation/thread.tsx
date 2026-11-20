import type { Delta } from "@komo/api";
import { Spinner } from "@/components/spinner";
import { Icon } from "@/components/icon";
import { LiveMark } from "./inline-process";
import { useNow } from "@/lib/hooks";
import { formatElapsed } from "@/lib/format";
import { sessionTitle } from "@/features/sessions/title";
import type { SessionSummary } from "@komo/protocol";
import { Link } from "@tanstack/react-router";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import type { Turn, View } from "./model";
import { AttachmentList, splitAttachments } from "./attachments";
import { ReplyView } from "./reply";
import { TurnView } from "./turn";
import { UserBubble } from "./user-bubble";

export interface PendingSend {
  id: string;
  text: string;
  state: "sending" | "sent" | "failed";
  error?: string;
}

/**
 * 对话正文：按轮排下来，末尾是还没确认收到的待发气泡和这段对话派出、还在跑的任务会话。在跑的那一轮自己画过程与
 * Komo 标志（`turn.tsx`）；有 Run 在跑却还没折出那一轮时，末尾先放一个标志。
 * 回复下的悬停工具最后一条常显（没有 Run 在跑时）。
 */
export function Thread({
  view,
  delta,
  pending,
  kids,
  isHome,
  busy,
  onRetry,
}: {
  view: View;
  delta: Delta | null;
  pending: readonly PendingSend[];
  kids: readonly SessionSummary[];
  isHome: boolean;
  /** 这段对话此刻有 Run 在跑。停止在输入框（发送键在跑时变成停止）。 */
  busy: boolean;
  onRetry: (id: string) => void;
}) {
  // 最后一条 Komo 的话（一轮的回复或投进来的话）：它下面的工具常显
  const lastSpoken = [...view.items].reverse().find((item) => (item.kind === "turn" && item.answer) || item.kind === "reply");
  const empty = !view.items.length && !pending.length && !view.runs.size;
  const now = useNow(kids.length > 0);
  const liveTurn = [...view.items].reverse().find((item): item is Turn => item.kind === "turn" && (item.status === "running" || item.status === "queued"));
  return (
    <>
      {empty && (
        <div className="mx-auto mt-[30vh] max-w-130 text-center text-read leading-6">
          <p className="mb-1.5">{isHome ? m.thread_empty_home() : m.thread_empty()}</p>
          <p className="mb-1.5 text-muted">{isHome ? m.thread_empty_hint_home() : m.thread_empty_hint()}</p>
        </div>
      )}
      {view.items.map((item) => {
        if (item.kind === "turn") return <TurnView key={item.id} turn={item} delta={delta} always={item === lastSpoken && !busy} />;
        if (item.kind === "user") return <UserBubble key={item.id} text={item.text} at={item.at} standalone />;
        if (item.kind === "reply") return <ReplyView key={item.id} text={item.text} always={item === lastSpoken && !busy} intervention={item.intervention} />;
        return (
          <p key={item.id} className="mb-3.5 text-label text-muted">
            {m.thread_compacted()}
          </p>
        );
      })}
      {pending.map((p) => {
        const { attachments, text } = splitAttachments(p.text);
        return (
          <div key={p.id} className="mt-6 mb-2 flex flex-col items-end gap-1">
            <div className={cn("flex w-full flex-col items-end gap-1", p.state !== "failed" && "opacity-60")}>
              <AttachmentList attachments={attachments} />
              {text.trim() && (
                <div className={cn("max-w-[80%] rounded-[16px] bg-raised px-3.5 py-2 text-read leading-6 break-words whitespace-pre-wrap max-[600px]:max-w-[88%]", p.state === "failed" && "outline outline-negative -outline-offset-1")}>
                  {text}
                </div>
              )}
            </div>
            <span className="text-small text-muted">
              {p.state === "failed" ? (
                <>
                  {m.send_failed({ message: p.error ?? "" })} ·{" "}
                  <button type="button" className="cursor-pointer font-medium text-text underline underline-offset-3" onClick={() => onRetry(p.id)}>
                    {m.retry()}
                  </button>
                </>
              ) : (
                m.send_sending()
              )}
            </span>
          </div>
        );
      })}
      {busy && !liveTurn && <LiveMark />}
      {kids.map((kid) => (
        <Link
          key={kid.session}
          to="/sessions/$sessionId"
          params={{ sessionId: kid.session }}
          className="group/kid mt-1 mb-2 flex items-center gap-2.5 text-read leading-6 text-muted"
        >
          <Spinner />
          <span className="text-text group-hover/kid:underline group-hover/kid:underline-offset-3">{sessionTitle(kid)}</span>
          <span className="tabular-nums">{formatElapsed(kid.created_at, now)}</span>
          <Icon name="right" />
        </Link>
      ))}
    </>
  );
}
