import { useHealth, useMatters, useMonitors, type MemoryBrief } from "@komo/api";
import type { Matter, MonitorSummary, SessionSummary } from "@komo/protocol";
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

// 每一段的开合只在这一次页面访问里记（原型也是）：换会话回来不弹回默认。
const toggled = new Set<string>();

/** 一段里先列几行，其余收在"显示更多"后面。 */
const LIMIT = 4;

/**
 * 右栏的内容（docs/web.md §6）：在做的事、带上的记忆、上下文（默认收起）、在后台、连接（默认收起）。
 * 样子照新版 Claude 桌面端的右栏：不套卡片，一段一个淡色小标题，一行一条（图标、一行字、行尾淡字），段与段之间一条细线；
 * 第二行的细节收进悬停提示（`title`），点一行进对应的详情页。
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

  // 这段对话在后台跑的：派出去的任务会话、挂的监听、转到后台的 bash 命令；在跑的在前，其余按开始时刻新的在前
  const monitors = useMonitors(true);
  const background: Background[] = [
    ...sessions.filter((s) => s.kind === "task" && s.home === sessionId).map((kid): Background => ({ kind: "task", kid, live: isLive(kid), at: kid.created_at })),
    ...(monitors.data ?? [])
      .filter((x) => x.session === sessionId)
      .map((monitor): Background => ({ kind: "monitor", monitor, live: monitor.state !== "ended", at: monitor.started_at })),
  ].sort((a, b) => Number(b.live) - Number(a.live) || new Date(b.at).getTime() - new Date(a.at).getTime());
  const runningCount = background.filter((x) => x.live).length;
  const now = useNow(runningCount > 0);

  const health = useHealth();
  const telemetry = health.data?.telemetry;

  return (
    <div>
      <Section
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
          <Rows>{matters.data.map((matter) => <MatterRow key={matter.id} matter={matter} />)}</Rows>
        ) : (
          <Empty>{m.panel_matters_empty()}</Empty>
        )}
      </Section>

      <Section
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
          <Rows>
            {uses.map((use) => {
              const memory = memories.get(use.memory);
              const convention = (memory?.application ?? use.application) === "convention";
              const lead = <Mote convention={convention} />;
              if (!memory) return <Row key={use.memory} lead={lead} label={use.memory} title={m.panel_memory_gone()} />;
              const detail = m.panel_memory_detail({
                kind: convention ? m.panel_memory_convention() : m.panel_memory_reference(),
                count: memory.usage?.count ?? 0,
                revision: use.revision ?? 0,
              });
              return (
                <Link key={use.memory} to="/memory/$id" params={{ id: memory.id }} title={`${memory.content}\n${detail}`} className={rowLink}>
                  <RowBody lead={lead} label={memory.content} />
                </Link>
              );
            })}
          </Rows>
        ) : (
          <Empty>{viewLoading ? m.panel_loading_session() : m.panel_memory_empty()}</Empty>
        )}
      </Section>

      <ContextSection context={context} treeStatus={treeStatus} />

      <Section id="bg" title={m.panel_bg()} value={runningCount ? m.panel_bg_running({ count: runningCount }) : String(background.length)}>
        {background.length ? (
          <Rows>
            {background.map((item) => {
              if (item.kind === "monitor") return <MonitorRow key={`monitor-${item.monitor.handle}`} monitor={item.monitor} now={now} />;
              const { kid, live } = item;
              return (
                <Link
                  key={kid.session}
                  to="/sessions/$sessionId"
                  params={{ sessionId: kid.session }}
                  title={
                    live
                      ? m.panel_bg_elapsed({ time: formatElapsed(kid.created_at, now) })
                      : m.panel_bg_done({ when: formatRelative(kid.updated_at) })
                  }
                  className={rowLink}
                >
                  <RowBody
                    lead={live ? <Spinner small /> : <Icon name="term" className="size-4 text-muted" />}
                    label={sessionTitle(kid)}
                    meta={live ? formatElapsed(kid.created_at, now) : formatRelative(kid.updated_at)}
                  />
                </Link>
              );
            })}
          </Rows>
        ) : (
          <Empty>{m.panel_bg_empty()}</Empty>
        )}
      </Section>

      <Section
        id="conn"
        title={m.panel_conn()}
        value={health.isError || telemetry?.last_error ? m.panel_conn_bad() : health.data ? m.panel_conn_ok() : ""}
        defaultOpen={false}
      >
        {health.data ? (
          <>
            <Row
              lead={<Icon name="plug" className="size-4 text-muted" />}
              label={m.panel_conn_gateway({ version: health.data.version })}
              title={m.panel_conn_started({ when: formatDateTime(health.data.started_at) })}
            />
            <Row
              lead={<Icon name="plug" className="size-4 text-muted" />}
              label={m.panel_conn_telemetry()}
              meta={telemetry?.last_error ? m.panel_conn_bad() : telemetry?.last_ok_at ? m.panel_conn_ok() : undefined}
              title={
                telemetry?.last_error
                  ? m.panel_conn_telemetry_error({ message: telemetry.last_error })
                  : telemetry?.last_ok_at
                    ? m.panel_conn_telemetry_ok({ when: formatRelative(telemetry.last_ok_at) })
                    : m.panel_conn_telemetry_off()
              }
            />
            <Empty>{m.panel_conn_note()}</Empty>
          </>
        ) : health.isError ? (
          <Failed message={errorText(health.error)} onRetry={() => void health.refetch()} />
        ) : (
          <Skeleton />
        )}
      </Section>
    </div>
  );
}

// 一行：32 高、8 圆角，悬停铺一层极淡的底；能点的整行是链接
const rowBase = "flex h-8 items-center gap-2.5 rounded-control px-2";
const rowLink = cn(rowBase, "text-text transition-colors hover:bg-hover-on-white");
const footLink = "flex h-8 items-center rounded-control px-2 text-label text-muted transition-colors hover:text-text";

/**
 * 一段：淡色小标题（开合的三角在收起时常显、展开时悬停才出），右边是条数 / 百分比之类的淡字；
 * 段与段之间一条细线。
 */
function Section({
  id,
  title,
  value,
  footer,
  defaultOpen = true,
  children,
}: {
  id: string;
  title: string;
  /** 标题右侧的小字（条数、百分比…）；ReactNode 以便放一条分段小条。 */
  value: ReactNode;
  footer?: ReactNode;
  defaultOpen?: boolean;
  children: ReactNode;
}) {
  const [, rerender] = useState(0);
  const open = toggled.has(id) ? !defaultOpen : defaultOpen;
  const bodyId = `panel-section-${id}`;
  return (
    <section className="border-border py-3 not-first:border-t">
      <button
        type="button"
        aria-expanded={open}
        aria-controls={bodyId}
        onClick={() => {
          if (!toggled.delete(id)) toggled.add(id);
          rerender((n) => n + 1);
        }}
        className="group flex h-7 w-full cursor-pointer items-center gap-1 px-2 text-left text-label text-muted transition-colors hover:text-text"
      >
        <span>{title}</span>
        <Icon
          name="right"
          className={cn("size-3 transition-[transform,opacity]", open && "rotate-90 opacity-0 group-hover:opacity-100 group-focus-visible:opacity-100")}
        />
        <span className="ml-auto flex items-center tabular-nums">{value}</span>
      </button>
      <div id={bodyId} hidden={!open} className="mt-0.5">
        {children}
        {footer}
      </div>
    </section>
  );
}

/** 一串行：超过 LIMIT 行时先收着，末尾一行"显示更多 / 收起"。 */
function Rows({ children }: { children: ReactNode[] }) {
  const [all, setAll] = useState(false);
  const shown = all ? children : children.slice(0, LIMIT);
  return (
    <>
      {shown}
      {children.length > LIMIT && (
        <button type="button" onClick={() => setAll((v) => !v)} className={cn(footLink, "w-full cursor-pointer text-left")}>
          {all ? m.panel_show_less() : m.panel_show_more()}
        </button>
      )}
    </>
  );
}

function RowBody({ lead, label, meta }: { lead: ReactNode; label: ReactNode; meta?: ReactNode }) {
  return (
    <>
      <span className="grid size-4 shrink-0 place-items-center">{lead}</span>
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {meta !== undefined && <span className="shrink-0 text-small text-muted tabular-nums">{meta}</span>}
    </>
  );
}

type Background =
  | { kind: "task"; kid: SessionSummary; live: boolean; at: string }
  | { kind: "monitor"; monitor: MonitorSummary; live: boolean; at: string };

/** 监听 / 转后台的命令：没有详情页，不能点；悬停给种类、短号、状态与脚本开头（停它仍由模型 `monitor_stop`）。 */
function MonitorRow({ monitor, now }: { monitor: MonitorSummary; now: number }) {
  const live = monitor.state !== "ended";
  const kind = monitor.detached ? m.panel_bg_command() : m.panel_bg_monitor();
  const state = live ? m.panel_bg_elapsed({ time: formatElapsed(monitor.started_at, now) }) : m.panel_bg_ended({ reason: monitor.reason ?? "" });
  return (
    <Row
      lead={live ? <Spinner small /> : <Icon name={monitor.detached ? "code" : "eye"} className="size-4 text-muted" />}
      label={monitor.title}
      meta={live ? formatElapsed(monitor.started_at, now) : formatRelative(monitor.started_at)}
      title={`${kind} #${monitor.handle} · ${state}\n${monitor.command}`}
    />
  );
}

/** 不能点的一行（连接、已不在库里的记忆、监听）。 */
function Row({ lead, label, meta, title }: { lead: ReactNode; label: ReactNode; meta?: ReactNode; title?: string }) {
  return (
    <div title={title} className={cn(rowBase, "text-text")}>
      <RowBody lead={lead} label={label} meta={meta} />
    </div>
  );
}

const Empty = ({ children }: { children: ReactNode }) => <p className="px-2 py-1 text-label text-muted">{children}</p>;

function Skeleton() {
  return (
    <div aria-hidden="true" className="px-2 py-1.5">
      <span className="mb-2.5 block h-3.5 w-4/5 rounded-xs bg-active" />
      <span className="block h-3.5 w-3/5 rounded-xs bg-active" />
    </div>
  );
}

function Failed({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="px-2 py-1.5 text-label">
      <p className="text-muted">{m.panel_failed({ message })}</p>
      <button type="button" onClick={onRetry} className="mt-1 cursor-pointer font-medium text-text underline underline-offset-3">
        {m.retry()}
      </button>
    </div>
  );
}

/** 事项一行：标题，行尾是目标的状态（等待用暖色字）；"下一步 / 在等 / 标准"收进悬停提示。 */
function MatterRow({ matter }: { matter: Matter }) {
  const second = matter.waiting_for
    ? m.panel_matter_waiting({ what: matter.waiting_for })
    : matter.next
      ? m.panel_matter_next({ what: matter.next })
      : matter.standard
        ? m.panel_matter_standard({ what: matter.standard })
        : "";
  return (
    <Link to="/matters/$id" params={{ id: matter.id }} title={second ? `${matter.title}\n${second}` : matter.title} className={rowLink}>
      <RowBody
        lead={<Icon name={(matter.kind === "area" ? "clock" : "target") satisfies IconName} className="size-4 text-muted" />}
        label={matter.title}
        meta={
          matter.kind === "goal" ? (
            <span className={cn(matter.state === "waiting" && "text-pill-wait-ink")}>{matterState(matter.state)}</span>
          ) : undefined
        }
      />
    </Link>
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
    <span aria-hidden="true" className={cn("flex h-1.5 overflow-hidden rounded-[3px] bg-active", mini ? "ml-2 inline-flex w-8 align-middle" : "mt-2 mb-2.5")}>
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

function ContextSection({ context, treeStatus }: { context: ContextView; treeStatus: "loading" | "error" | "ready" }) {
  const [toolsOpen, setToolsOpen] = useState(false);
  const { win, total, rows, tools, scale, measured } = context;
  const pct = win ? pctText(total, win) : "—";
  const free = Math.max(0, win - total);
  const row = "grid grid-cols-[12px_minmax(0,1fr)_56px_48px] items-center gap-2 h-7 px-2 text-label";
  return (
    <Section
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
        <div>
          <div className="flex items-baseline justify-between px-2 text-label">
            <span>{m.panel_context_used({ pct })}</span>
            <span className="text-muted tabular-nums">{m.panel_context_of({ used: formatTokens(total), win: win ? formatTokens(win) : "?" })}</span>
          </div>
          <div className="px-2">
            <StackBar context={context} />
          </div>
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
                className="mt-1 flex h-7 w-full cursor-pointer items-center gap-1.5 rounded-control px-2 text-left text-label hover:bg-hover-on-white"
              >
                <Icon name="right" className={cn("size-3 text-muted transition-transform", toolsOpen && "rotate-90")} />
                <span>{m.ctx_tools()}</span>
                <span className="ml-auto text-muted tabular-nums">{tools.length}</span>
              </button>
              {toolsOpen && (
                <div className="pb-1">
                  {[...tools]
                    .sort((a, b) => b.tokens - a.tokens)
                    .map((tool) => (
                      <div key={tool.name} className="flex h-6 items-center justify-between gap-2 pr-2 pl-6.5 text-label">
                        <span className="truncate">{tool.name}</span>
                        <span className="shrink-0 text-right tabular-nums">{formatTokens(Math.round(tool.tokens * scale))}</span>
                      </div>
                    ))}
                </div>
              )}
            </>
          )}
          <p className="mt-2 px-2 text-small text-muted">{m.panel_context_note()}</p>
        </div>
      )}
    </Section>
  );
}
