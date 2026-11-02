import { useHealth, useSessions } from "@komo/api";
import { Link, Outlet, createFileRoute } from "@tanstack/react-router";
import { LanguageSwitch } from "@/components/language-switch";
import { Unauthorized } from "@/components/unauthorized";
import { formatRelative } from "@/lib/format";
import { errorText } from "@/lib/errors";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { isApiError } from "@komo/api";
import type { SessionSummary } from "@komo/protocol";
import { Button } from "@/components/ui/button";

export const Route = createFileRoute("/_app")({ component: AppShell });

function AppShell() {
  const sessions = useSessions();
  const health = useHealth();
  return (
    <div className="grid h-svh grid-cols-[280px_minmax(0,1fr)] overflow-hidden max-md:grid-cols-1">
      <aside className="flex min-h-0 flex-col border-r border-border bg-ground max-md:hidden">
        <header className="px-4 pt-4 pb-3">
          <Link to="/" className="text-section-title font-semibold tracking-tight">
            {m.app_name()}
          </Link>
          {health.data && (
            <p className="text-small text-faint">
              {m.gateway_version({ version: health.data.version, protocol: health.data.protocol_version })}
            </p>
          )}
        </header>
        <h2 className="px-4 pb-1 text-label font-medium text-muted">{m.sessions_title()}</h2>
        <nav className="min-h-0 flex-1 overflow-y-auto px-2 pb-2">
          {sessions.isPending && <p className="px-2 py-2 text-small text-faint">{m.sessions_loading()}</p>}
          {sessions.isError && !(isApiError(sessions.error) && sessions.error.unauthorized) && (
            <div className="space-y-2 px-2 py-2 text-small text-negative">
              <p>{errorText(sessions.error)}</p>
              <Button onClick={() => void sessions.refetch()}>{m.retry()}</Button>
            </div>
          )}
          {sessions.data?.length === 0 && <p className="px-2 py-2 text-small text-faint">{m.sessions_empty()}</p>}
          <ul>{sessions.data?.map((session) => <SessionRow key={session.session} session={session} />)}</ul>
        </nav>
        <footer className="border-t border-border px-4 py-3">
          <LanguageSwitch />
        </footer>
      </aside>
      <main className="min-h-0 overflow-y-auto bg-surface">
        {sessions.isError && isApiError(sessions.error) && sessions.error.unauthorized ? <Unauthorized /> : <Outlet />}
      </main>
    </div>
  );
}

function SessionRow({ session }: { session: SessionSummary }) {
  const running = session.current_state === "running";
  const waiting = session.current_state === "waiting";
  return (
    <li>
      <Link
        to="/sessions/$sessionId"
        params={{ sessionId: session.session }}
        className="flex flex-col gap-0.5 rounded-control px-2 py-1.5 hover:bg-hover"
        activeProps={{ className: "bg-active" }}
      >
        <span className="flex items-center gap-2 text-body">
          <span
            className={cn(
              "size-1.5 shrink-0 rounded-full",
              running ? "bg-accent" : waiting ? "bg-wait-dot" : "bg-transparent",
            )}
            title={running ? m.session_state_running() : waiting ? m.session_state_waiting() : undefined}
          />
          <span className="truncate">{session.title || m.session_untitled()}</span>
          {session.kind === "task" && (
            <span className="rounded-full bg-pill-bg px-1.5 text-small text-pill-ink">{m.session_kind_task()}</span>
          )}
        </span>
        <span className="pl-3.5 text-small text-faint">{m.session_updated({ time: formatRelative(session.updated_at) })}</span>
      </Link>
    </li>
  );
}
