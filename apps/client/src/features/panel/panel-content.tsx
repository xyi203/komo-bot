import { useHealth, useMatters, type MemoryBrief } from "@komo/api";
import type { Matter, SessionSummary } from "@komo/protocol";
import { Link } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { Icon, type IconName } from "@/components/icon";
import { Mote } from "@/components/mote";
import { Spinner } from "@/components/spinner";
import type { View } from "@/features/conversation/model";
import { isLive } from "@/features/sessions/model";
import { sessionTitle } from "@/features/sessions/title";
import { errorText } from "@/lib/errors";
import { formatDateTime, formatElapsed, formatRelative, formatTokens } from "@/lib/format";
import { useNow } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { PART_COLORS, pctOf, pctText, type ContextView } from "./context-usage";

// 每张卡片的开合只在这一次页面访问里记（原型也是）：换会话回来不弹回默认。
const toggled = new Set<string>();

/**
 * 右栏的内容（docs/web.md §6）：在做的事、带上的记忆、上下文（默认收起）、在后台、连接（默认收起）。
 * 外壳（宽屏常驻、≤1180 变抽屉、整体收起与记忆）在 `features/layout/panel.tsx`。
 */
export function PanelContent({
  view,
  viewLoading,
  context,
  treeStatus,
  memories,
  sessions,
  sessionId,
}: {
  view: View | undefined;
  viewLoading: boolean;
  context: ContextView;
  treeStatus: "loading" | "error" | "ready";
  memories: ReadonlyMap<string, MemoryBrief>;
  sessions: readonly SessionSummary[];
  sessionId: string;
}) {
  const matters = useMatters(false);
  const uses = view?.recall ?? [];
  const activeCount = [...memories.values()].filter((x) => x.state === "active").length;

  // 只列这段对话派出去的任务会话：在跑的在前，结束的给出用了多久
  const kids = sessions
    .filter((s) => s.kind === "task" && s.home === sessionId)
    .sort((a, b) => Number(isLive(b)) - Number(isLive(a)) || new Date(b.created_at).getTime() - new Date(a.created_at).getTime());
  const runningKids = kids.filter(isLive);
  const now = useNow(runningKids.length > 0);

  const health = useHealth();
  const telemetry = health.data?.telemetry;

  return (
    <div>
      <Card
        id="matters"
        title={m.panel_matters()}
        value={matters.data ? String(matters.data.length) : ""}
        footer={
          <Link to="/matters" className={footLink}>
            {m.panel_matters_all()}
          </Link>
        }
      >
        {matters.isPending ? (
          <Skeleton />
        ) : matters.isError ? (
          <Failed message={errorText(matters.error)} onRetry={() => void matters.refetch()} />
        ) : matters.data.length ? (
          matters.data.map((matter) => <MatterRow key={matter.id} matter={matter} />)
        ) : (
          <Empty>{m.panel_matters_empty()}</Empty>
        )}
      </Card>

      <Card
        id="memory"
        title={m.panel_memory()}
        value={String(uses.length)}
        footer={
          <Link to="/memory" className={footLink}>
            {m.panel_memory_all({ count: activeCount })}
          </Link>
        }
      >
        {uses.length ? (
          uses.map((use) => {
            const memory = memories.get(use.memory);
            const convention = (memory?.application ?? use.application) === "convention";
            return (
              <Row key={use.memory} lead={<Mote convention={convention} className="mt-1.5" />}>
                {memory ? memory.content : use.memory}
                <Line2>
                  {memory
                    ? m.panel_memory_detail({
                        kind: convention ? m.panel_memory_convention() : m.panel_memory_reference(),
                        count: memory.usage?.count ?? 0,
                        revision: use.revision ?? 0,
                      })
                    : m.panel_memory_gone()}
                </Line2>
              </Row>
            );
          })
        ) : (
          <Empty>{viewLoading ? m.panel_loading_session() : m.panel_memory_empty()}</Empty>
        )}
      </Card>

      <ContextCard context={context} treeStatus={treeStatus} />

      <Card id="bg" title={m.panel_bg()} value={runningKids.length ? m.panel_bg_running({ count: runningKids.length }) : String(kids.length)}>
        {kids.length ? (
          kids.map((kid) => {
            const live = isLive(kid);
            return (
              <Row key={kid.session} lead={live ? <Spinner small className="mx-0.5 mt-1" /> : <Icon name="term" className="mt-0.5 size-4 text-muted" />}>
                <Link to="/sessions/$sessionId" params={{ sessionId: kid.session }} className="text-text hover:underline hover:underline-offset-3">
                  {sessionTitle(kid)}
                </Link>
                <Line2 className="tabular-nums">
                  {live
                    ? m.panel_bg_elapsed({ time: formatElapsed(kid.created_at, now) })
                    : m.panel_bg_done({ when: formatRelative(kid.updated_at) })}
                </Line2>
              </Row>
            );
          })
        ) : (
          <Empty>{m.panel_bg_empty()}</Empty>
        )}
      </Card>

      <Card
        id="conn"
        title={m.panel_conn()}
        value={health.isError || telemetry?.last_error ? m.panel_conn_bad() : health.data ? m.panel_conn_ok() : ""}
        defaultOpen={false}
      >
        {health.data ? (
          <>
            <Row lead={<Icon name="plug" className="mt-0.5 size-4 text-muted" />}>
              {m.panel_conn_gateway({ version: health.data.version })}
              <Line2 className="tabular-nums">{m.panel_conn_started({ when: formatDateTime(health.data.started_at) })}</Line2>
            </Row>
            <Row lead={<Icon name="plug" className="mt-0.5 size-4 text-muted" />}>
              {m.panel_conn_telemetry()}
              <Line2>
                {telemetry?.last_error
                  ? m.panel_conn_telemetry_error({ message: telemetry.last_error })
                  : telemetry?.last_ok_at
                    ? m.panel_conn_telemetry_ok({ when: formatRelative(telemetry.last_ok_at) })
                    : m.panel_conn_telemetry_off()}
              </Line2>
            </Row>
            <Empty>{m.panel_conn_note()}</Empty>
          </>
        ) : health.isError ? (
          <Failed message={errorText(health.error)} onRetry={() => void health.refetch()} />
        ) : (
          <Skeleton />
        )}
      </Card>
    </div>
  );
}

const footLink = "block pt-1.5 text-small text-text underline-offset-3 no-underline hover:underline";

function Card({
  id,
  title,
  value,
  footer,
  defaultOpen = true,
  children,
}: {
  id: string;
  title: string;
  /** 卡片头右侧的小字（条数、百分比…）；ReactNode 以便放一条分段小条。 */
  value: ReactNode;
  footer?: ReactNode;
  defaultOpen?: boolean;
  children: ReactNode;
}) {
  const [, rerender] = useState(0);
  const open = toggled.has(id) ? !defaultOpen : defaultOpen;
  const bodyId = `panel-card-${id}`;
  return (
    <section className="mb-3 rounded-card bg-surface shadow-lift">
      <button
        type="button"
        aria-expanded={open}
        aria-controls={bodyId}
        onClick={() => {
          if (!toggled.delete(id)) toggled.add(id);
          rerender((n) => n + 1);
        }}
        className="flex h-11 w-full cursor-pointer items-center gap-1.5 rounded-card px-3.5 text-left hover:bg-hover-on-white"
      >
        <span className="font-medium">{title}</span>
        <Icon name="down" className={cn("size-3.5 text-muted transition-transform", !open && "-rotate-90")} />
        <span className="ml-auto flex items-center text-label text-muted tabular-nums">{value}</span>
      </button>
      <div id={bodyId} hidden={!open} className="px-3.5 pb-3">
        {children}
        {footer}
      </div>
    </section>
  );
}

function Row({ lead, children }: { lead: ReactNode; children: ReactNode }) {
  return (
    <div className="flex items-start gap-2.5 py-1.75">
      <span className="flex shrink-0 items-start">{lead}</span>
      <div className="min-w-0 flex-1">{children}</div>
    </div>
  );
}

const Line2 = ({ children, className }: { children: ReactNode; className?: string }) => (
  <div className={cn("text-small text-muted", className)}>{children}</div>
);
const Empty = ({ children }: { children: ReactNode }) => <p className="mt-1 mb-1.5 text-label text-muted">{children}</p>;

function Skeleton() {
  return (
    <div aria-hidden="true" className="py-1.5">
      <span className="mb-2.5 block h-3.5 w-4/5 rounded-xs bg-active" />
      <span className="block h-3.5 w-3/5 rounded-xs bg-active" />
    </div>
  );
}

function Failed({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="py-1.5 text-label">
      <p className="text-muted">{m.panel_failed({ message })}</p>
      <button type="button" onClick={onRetry} className="mt-1 cursor-pointer font-medium text-text underline underline-offset-3">
        {m.retry()}
      </button>
    </div>
  );
}

function MatterRow({ matter }: { matter: Matter }) {
  const second = matter.waiting_for
    ? m.panel_matter_waiting({ what: matter.waiting_for })
    : matter.next
      ? m.panel_matter_next({ what: matter.next })
      : matter.standard
        ? m.panel_matter_standard({ what: matter.standard })
        : "";
  const state = matterState(matter.state);
  return (
    <Row lead={<Icon name={(matter.kind === "area" ? "clock" : "target") satisfies IconName} className="mt-0.5 size-4 text-muted" />}>
      <div className="flex items-center gap-2">
        <span className="min-w-0 truncate">{matter.title}</span>
        {matter.kind === "goal" && (
          <span
            className={cn(
              "inline-flex h-5 shrink-0 items-center rounded-full px-1.75 text-small font-medium whitespace-nowrap",
              matter.state === "waiting" ? "bg-pill-wait-bg text-pill-wait-ink" : "bg-pill-bg text-pill-ink",
            )}
          >
            {state}
          </span>
        )}
      </div>
      {second && <Line2 className="truncate">{second}</Line2>}
    </Row>
  );
}

function matterState(state: string): string {
  switch (state) {
    case "active":
      return m.matter_state_active();
    case "waiting":
      return m.matter_state_waiting();
    case "parked":
      return m.matter_state_parked();
    default:
      return state;
  }
}

function StackBar({ context, mini = false }: { context: ContextView; mini?: boolean }) {
  if (!context.win) return null;
  return (
    <span aria-hidden="true" className={cn("flex h-1.5 overflow-hidden rounded-[3px] bg-active", mini ? "ml-2 inline-flex w-11 align-middle" : "mt-2 mb-2.5")}>
      {context.rows.length ? (
        context.rows.map((row) => (
          <span key={row.key} className="block h-full not-first:ml-px" style={{ width: `${pctOf(row.tokens, context.win)}%`, background: PART_COLORS[row.key] }} />
        ))
      ) : (
        <span className="block h-full bg-accent" style={{ width: `${pctOf(context.total, context.win)}%` }} />
      )}
    </span>
  );
}

const partLabel = (key: string): string => {
  switch (key) {
    case "messages":
      return m.ctx_messages();
    case "results":
      return m.ctx_results();
    case "tools":
      return m.ctx_tools();
    case "memory":
      return m.ctx_memory();
    case "skills":
      return m.ctx_skills();
    case "project":
      return m.ctx_project();
    case "system":
      return m.ctx_system();
    default:
      return m.ctx_summary();
  }
};

function ContextCard({ context, treeStatus }: { context: ContextView; treeStatus: "loading" | "error" | "ready" }) {
  const [toolsOpen, setToolsOpen] = useState(false);
  const { win, total, rows, tools, scale, measured } = context;
  const pct = win ? pctText(total, win) : "—";
  const free = Math.max(0, win - total);
  const row = "grid grid-cols-[12px_minmax(0,1fr)_56px_48px] items-center gap-2 h-7";
  return (
    <Card
      id="context"
      title={m.panel_context()}
      defaultOpen={false}
      value={
        measured ? (
          <>
            {pct}
            <StackBar context={context} mini />
          </>
        ) : (
          "—"
        )
      }
    >
      {!measured ? (
        <Empty>{m.panel_context_none()}</Empty>
      ) : (
        <>
          <div className="flex items-baseline justify-between">
            <span>{m.panel_context_used({ pct })}</span>
            <span className="text-muted tabular-nums">{m.panel_context_of({ used: formatTokens(total), win: win ? formatTokens(win) : "?" })}</span>
          </div>
          <StackBar context={context} />
          {rows.length ? (
            rows.map((r) => (
              <div key={r.key} className={row}>
                <span className="size-2.5 rounded-[2px]" style={{ background: PART_COLORS[r.key] }} />
                <span className="truncate">{partLabel(r.key)}</span>
                <span className="text-right tabular-nums">{formatTokens(r.tokens)}</span>
                <span className="text-right text-muted tabular-nums">{pctText(r.tokens, win)}</span>
              </div>
            ))
          ) : (
            <Empty>{treeStatus === "error" ? m.panel_context_tree_error() : m.panel_context_tree_loading()}</Empty>
          )}
          {win > 0 && (
            <div className={row}>
              <span className="size-2.5 rounded-[2px] bg-active" />
              <span>{m.ctx_free()}</span>
              <span className="text-right tabular-nums">{formatTokens(free)}</span>
              <span className="text-right text-muted tabular-nums">{pctText(free, win)}</span>
            </div>
          )}
          {tools.length > 0 && (
            <>
              <button
                type="button"
                aria-expanded={toolsOpen}
                onClick={() => setToolsOpen((v) => !v)}
                className="mt-1.5 flex h-7.5 w-full cursor-pointer items-center gap-1.5 text-left"
              >
                <Icon name="right" className={cn("size-3.5 text-muted transition-transform", toolsOpen && "rotate-90")} />
                <span>{m.ctx_tools()}</span>
                <span className="ml-auto text-muted tabular-nums">{tools.length}</span>
              </button>
              {toolsOpen && (
                <div className="pb-1">
                  {[...tools]
                    .sort((a, b) => b.tokens - a.tokens)
                    .map((tool) => (
                      <div key={tool.name} className="flex h-6 items-center justify-between gap-2 pl-5 text-label">
                        <span className="truncate">{tool.name}</span>
                        <span className="shrink-0 text-right tabular-nums">{formatTokens(Math.round(tool.tokens * scale))}</span>
                      </div>
                    ))}
                </div>
              )}
            </>
          )}
          <p className="mt-2 text-small text-muted">{m.panel_context_note()}</p>
        </>
      )}
    </Card>
  );
}
