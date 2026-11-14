import type { Delta } from "@komo/api";
import { Spinner } from "@/components/spinner";
import { Icon } from "@/components/icon";
import { useNow } from "@/lib/hooks";
import { formatElapsed } from "@/lib/format";
import { sessionTitle } from "@/features/sessions/title";
import type { SessionSummary } from "@komo/protocol";
import { Link } from "@tanstack/react-router";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import type { View } from "./model";
import { AttachmentList, splitAttachments } from "./attachments";
import { ReplyView } from "./reply";
import { TurnView, UserBubble } from "./turn";

export interface PendingSend {
  id: string;
  text: string;
  state: "sending" | "sent" | "failed";
  error?: string;
}

/** 对话标签页：按轮排下来，末尾是还没确认收到的待发气泡和这段对话派出、还在跑的任务会话。 */
export function Thread({
  view,
  delta,
  pending,
  kids,
  isHome,
  onRetry,
}: {
  view: View;
  delta: Delta | null;
  pending: readonly PendingSend[];
  kids: readonly SessionSummary[];
  isHome: boolean;
  onRetry: (id: string) => void;
}) {
  const lastTurn = [...view.items].reverse().find((item) => item.kind === "turn");
  const lastReply = [...view.items].reverse().find((item) => item.kind === "reply");
  const empty = !view.items.length && !pending.length && !view.runs.size;
  const now = useNow(kids.length > 0);
  return (
    <>
      {empty && (
        <div className="mx-auto mt-[30vh] max-w-130 text-center text-read leading-6">
          <p className="mb-1.5">{isHome ? m.thread_empty_home() : m.thread_empty()}</p>
          <p className="mb-1.5 text-muted">{m.thread_empty_hint()}</p>
        </div>
      )}
      {view.items.map((item) => {
        if (item.kind === "turn") return <TurnView key={item.id} turn={item} delta={delta} last={item === lastTurn} />;
        if (item.kind === "user") return <UserBubble key={item.id} text={item.text} at={item.at} standalone />;
        if (item.kind === "reply") return <ReplyView key={item.id} text={item.text} at={item.at} last={item === lastReply} intervention={item.intervention} />;
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
