import { useSessionStream, useSessions } from "@komo/api";
import type { SseFrame } from "@komo/protocol";
import { createFileRoute } from "@tanstack/react-router";
import { formatDateTime, formatNumber } from "@/lib/format";
import { m } from "@/paraglide/messages.js";

export const Route = createFileRoute("/_app/sessions/$sessionId")({ component: SessionStream });

function SessionStream() {
  const { sessionId } = Route.useParams();
  const sessions = useSessions();
  const summary = sessions.data?.find((session) => session.session === sessionId);
  const { frames, state } = useSessionStream(sessionId);
  const stateText =
    state === "open" ? m.stream_open() : state === "connecting" ? m.stream_connecting() : m.stream_reconnecting();

  return (
    <div className="mx-auto flex max-w-4xl flex-col gap-4 px-6 py-6">
      <header>
        <h1 className="text-page-title font-semibold tracking-tight">{summary?.title || m.session_untitled()}</h1>
        <p className="font-mono text-small text-faint">{sessionId}</p>
        {sessions.data && !summary && <p className="text-small text-negative">{m.session_not_found()}</p>}
        {summary && <p className="text-small text-faint">{formatDateTime(summary.updated_at)}</p>}
      </header>
      <section>
        <h2 className="flex items-baseline gap-3 text-section-title font-semibold">
          {m.stream_title()}
          <span className="text-small font-normal text-muted">{stateText}</span>
          <span className="text-small font-normal text-faint">{m.stream_frame_count({ count: formatNumber(frames.length) })}</span>
        </h2>
        {frames.length === 0 ? (
          <p className="mt-2 text-body text-faint">{m.stream_empty()}</p>
        ) : (
          <ol className="mt-2 divide-y divide-rule rounded-card shadow-ring">
            {frames.map((frame, index) => (
              <FrameRow key={`${frame.id}-${index}`} frame={frame} />
            ))}
          </ol>
        )}
      </section>
    </div>
  );
}

function FrameRow({ frame }: { frame: SseFrame }) {
  const label = frame.event === "event" ? frame.data.type : frame.event;
  const detail = frame.event === "event" ? frame.data.data : "data" in frame ? frame.data : undefined;
  return (
    <li className="flex gap-3 px-3 py-1.5">
      <span className="w-12 shrink-0 text-right font-mono text-small text-faint">{frame.id}</span>
      <span className="w-40 shrink-0 truncate font-mono text-small text-text">{label}</span>
      <span className="min-w-0 flex-1 truncate font-mono text-small text-muted">
        {detail === undefined ? "" : JSON.stringify(detail)}
      </span>
    </li>
  );
}
