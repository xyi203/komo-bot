import { isApiError, useHomeSession, useSessions } from "@komo/api";
import type { SessionSummary } from "@komo/protocol";
import { Link, useParams } from "@tanstack/react-router";
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { Icon } from "@/components/icon";
import { IconButton } from "@/components/ui/icon-button";
import { useDraftedSessions } from "@/features/composer/draft";
import { errorText } from "@/lib/errors";
import { usePersistedChoice, usePersistedSet } from "@/lib/storage";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { useSidebarContext } from "@/features/layout/sidebar-context";
import { useSessionActions } from "./actions";
import {
  FILTER_MODES,
  GROUP_MODES,
  PAGE_ROWS,
  SORT_MODES,
  dirName,
  groupSessions,
  isArchived,
  isEmpty,
  isLive,
  isRunning,
  isWaiting,
  kidsOf,
  type FilterMode,
  type Group,
} from "./model";
import { SessionMenu } from "./session-menu";
import { sessionTitle } from "./title";
import { ViewMenu, type ViewPrefs } from "./view-menu";

/** 会话区：区头（"会话"、视图菜单、新对话）和分组的会话列表。 */
export function SessionSection() {
  const actions = useSessionActions();
  const [group, setGroup] = usePersistedChoice("komo.group", GROUP_MODES, "tree");
  const [sort, setSort] = usePersistedChoice("komo.sort", SORT_MODES, "updated");
  const [filter, setFilter] = usePersistedChoice("komo.filter", FILTER_MODES, "active");
  const prefs: ViewPrefs = { group, sort, filter, setGroup, setSort, setFilter };
  return (
    <>
      <div className="relative px-2">
        <div className="flex h-9 items-center gap-0.5 pl-2">
          <h2 className="mr-auto text-label font-medium text-muted">{m.sessions_title()}</h2>
          <ViewMenu prefs={prefs} />
          <IconButton name="plus" label={m.nav_new_session()} size="compact" className="hover:bg-hover" onClick={() => void actions.create()} />
        </div>
      </div>
      <SessionList prefs={prefs} />
    </>
  );
}

function SessionList({ prefs }: { prefs: ViewPrefs }) {
  const sessions = useSessions(true);
  const home = useHomeSession();
  const [closed, toggleClosed] = usePersistedSet("komo.closed");
  const [navOpen, toggleNavOpen] = usePersistedSet("komo.navOpen");
  const [showMore, setShowMore] = useState<ReadonlySet<string>>(new Set());
  const [renaming, setRenaming] = useState<string | null>(null);
  const { sessionId } = useParams({ strict: false });
  const drafted = useDraftedSessions();

  // 打开某个子任务时它的父会话自动展开
  const all = sessions.data;
  const parent = all?.find((s) => s.session === sessionId)?.home;
  useEffect(() => {
    if (parent && !navOpen.has(parent)) toggleNavOpen(parent, true);
  }, [parent, navOpen, toggleNavOpen]);

  const unauthorized = sessions.isError && isApiError(sessions.error) && sessions.error.unauthorized;
  if (sessions.isPending) {
    return (
      <div className="min-h-0 flex-1 overflow-auto px-2 pb-3">
        {[72, 56, 64, 48, 60, 72, 56, 64].map((width, i) => (
          <div key={i} className="flex h-8.5 items-center px-2">
            <span className="block h-3 rounded-xs bg-active" style={{ width: `${width}%` }} />
          </div>
        ))}
      </div>
    );
  }
  if (sessions.isError) {
    if (unauthorized) return <div className="flex-1" />; // 外壳整页给配对提示
    return (
      <div className="m-2 rounded-control border border-dashed border-control p-3 text-label text-muted">
        <p className="mb-1.5">{m.sessions_error({ message: errorText(sessions.error) })}</p>
        <button type="button" className="cursor-pointer font-medium text-text underline underline-offset-3" onClick={() => void sessions.refetch()}>
          {m.retry()}
        </button>
      </div>
    );
  }

  if (!all) return null;
  const grouped = groupSessions(all, home.data?.session, prefs.group, prefs, drafted);
  const rowContext: RowContext = {
    all,
    drafted,
    filter: prefs.filter,
    currentId: sessionId,
    renaming,
    setRenaming,
    navOpen,
    toggleNavOpen,
  };
  return (
    <div className="min-h-0 flex-1 overflow-auto px-2 pb-3">
      {grouped.main && (
        <section>
          <ul>
            <SessionTreeRow session={grouped.main} ctx={rowContext} />
          </ul>
        </section>
      )}
      {grouped.groups.map((group) => (
        <GroupBlock
          key={group.key}
          group={group}
          mode={prefs.group}
          filter={prefs.filter}
          ctx={rowContext}
          closed={closed.has(group.key)}
          toggleClosed={() => toggleClosed(group.key)}
          more={showMore.has(group.key)}
          toggleMore={() =>
            setShowMore((previous) => {
              const next = new Set(previous);
              if (!next.delete(group.key)) next.add(group.key);
              return next;
            })
          }
        />
      ))}
    </div>
  );
}

interface RowContext {
  all: readonly SessionSummary[];
  /** 框里有没发草稿的会话：一句话都没发过的那种在行尾标"草稿"。 */
  drafted: ReadonlySet<string>;
  filter: FilterMode;
  currentId: string | undefined;
  renaming: string | null;
  setRenaming: (id: string | null) => void;
  navOpen: ReadonlySet<string>;
  toggleNavOpen: (id: string) => void;
}

function GroupBlock({
  group,
  mode,
  filter,
  ctx,
  closed,
  toggleClosed,
  more,
  toggleMore,
}: {
  group: Group;
  mode: "tree" | "workdir" | "flat";
  filter: FilterMode;
  ctx: RowContext;
  closed: boolean;
  toggleClosed: () => void;
  more: boolean;
  toggleMore: () => void;
}) {
  const actions = useSessionActions();
  const cap = PAGE_ROWS[mode];
  const label =
    group.kind === "dir"
      ? dirName(group.dir ?? "") || m.workdir_default()
      : group.kind === "flat"
        ? filter === "active" ? m.group_flat_active() : filter === "archived" ? m.group_archived() : m.group_all()
        : filter === "active" ? m.group_recent() : filter === "archived" ? m.group_archived() : m.group_all();
  const none = filter === "active" ? m.sessions_none_active() : filter === "archived" ? m.sessions_none_archived() : m.sessions_none_all();
  const shown = more ? group.sessions : group.sessions.slice(0, cap);
  const dirTitle = group.dir === undefined ? undefined : group.dir || "~/.komo/workspaces";
  return (
    <section className="mt-4 first:mt-0">
      <div className="flex h-8 items-center pr-1 pl-2">
        <button
          type="button"
          aria-expanded={!closed}
          title={dirTitle}
          onClick={toggleClosed}
          className="group/toggle inline-flex cursor-pointer items-center gap-1 text-label font-medium text-muted"
        >
          {label} <span className="ml-0.5 font-normal tabular-nums">{group.sessions.length}</span>
          <Icon name="down" className={cn("size-3.5 transition-transform", closed && "-rotate-90")} />
        </button>
        {group.kind === "dir" && (
          <IconButton
            name="plus"
            size="compact"
            label={m.workdir_new({ name: label })}
            title={m.workdir_new({ name: dirTitle ?? label })}
            className="ml-auto size-7 hover:bg-hover"
            onClick={() => void actions.create(group.dir || undefined)}
          />
        )}
      </div>
      {!closed && (
        <ul>
          {group.sessions.length === 0 && <li className="px-2 py-1 text-label text-muted">{none}</li>}
          {shown.map((s) =>
            mode === "flat" ? <SessionRow key={s.session} session={s} ctx={ctx} flat /> : <SessionTreeRow key={s.session} session={s} ctx={ctx} />,
          )}
          {group.sessions.length > cap && (
            <li>
              <button type="button" onClick={toggleMore} className="mx-2 my-1 cursor-pointer text-label font-medium text-muted underline underline-offset-3">
                {more ? m.sessions_collapse() : m.sessions_show_all({ count: group.sessions.length })}
              </button>
            </li>
          )}
        </ul>
      )}
    </section>
  );
}

/** 有子任务的会话：名字后面一个 ▸，收起时下面一行"N 个子任务"，展开后直接列出全部子任务。 */
function SessionTreeRow({ session, ctx }: { session: SessionSummary; ctx: RowContext }) {
  const kids = kidsOf(ctx.all, session, ctx.filter);
  if (!kids.length) return <SessionRow session={session} ctx={ctx} />;
  const open = ctx.navOpen.has(session.session);
  const live = kids.filter(isLive).length;
  return (
    <>
      <ParentRow session={session} ctx={ctx} open={open} kidCount={kids.length} />
      {open ? (
        kids.map((kid) => <SessionRow key={kid.session} session={kid} ctx={ctx} child />)
      ) : (
        <li>
          <button
            type="button"
            aria-expanded={false}
            onClick={() => ctx.toggleNavOpen(session.session)}
            className="relative flex h-7.5 w-full cursor-pointer items-center gap-1 rounded-control pr-2 pl-7 text-left text-label text-muted before:absolute before:top-1.5 before:left-3.5 before:h-[9px] before:w-2 before:rounded-bl-xs before:border-b before:border-l before:border-control before:content-[''] hover:bg-hover hover:text-text"
          >
            {m.sessions_kids({ count: kids.length })}
            <Icon name="right" className="size-3 stroke-2" />
            {live > 0 && (
              <span className="ml-auto inline-flex items-center gap-1">
                <StateDot running />
                {m.sessions_kids_live({ count: live })}
              </span>
            )}
          </button>
        </li>
      )}
    </>
  );
}

const ROW =
  "flex h-8.5 w-full items-center gap-2 rounded-control px-2 text-left transition-colors hover:bg-hover group-hover/row:bg-hover group-hover/row:pr-9 group-has-data-[state=open]/row:bg-hover group-has-data-[state=open]/row:pr-9 [@media(hover:none)]:pr-9";
const CURRENT = "data-[status=active]:bg-active data-[status=active]:font-medium";

function StateDot({ running, waiting }: { running?: boolean; waiting?: boolean }) {
  if (!running && !waiting) return null;
  return (
    <span
      role="img"
      aria-label={running ? m.session_state_running() : m.session_state_waiting()}
      className={cn("size-1.5 shrink-0 rounded-full", running ? "bg-accent" : "bg-wait-dot")}
    />
  );
}

function SessionRow({ session, ctx, child = false, flat = false }: { session: SessionSummary; ctx: RowContext; child?: boolean; flat?: boolean }) {
  const title = sessionTitle(session);
  const archived = isArchived(session);
  const tag =
    archived && ctx.filter !== "archived"
      ? m.session_tag_archived()
      : flat && session.kind === "task"
        ? m.session_tag_task()
        : isEmpty(session) && ctx.drafted.has(session.session)
          ? m.session_tag_draft()
          : null;
  const sidebar = useSidebarContext();
  if (ctx.renaming === session.session) {
    return (
      <li>
        <RenameRow session={session} child={child} onDone={() => ctx.setRenaming(null)} />
      </li>
    );
  }
  return (
    <li className="group/row relative">
      <Link
        to="/sessions/$sessionId"
        params={{ sessionId: session.session }}
        title={title}
        onClick={sidebar.closeDrawer}
        className={cn(
          ROW,
          CURRENT,
          child && "relative pl-7 text-muted before:absolute before:top-2 before:left-3.5 before:h-[9px] before:w-2 before:rounded-bl-xs before:border-b before:border-l before:border-control before:content-['']",
        )}
      >
        <span className={cn("min-w-0 flex-1 truncate", archived && "text-muted")}>{title}</span>
        {tag && <span className="text-small text-faint">{tag}</span>}
        <StateDot running={isRunning(session)} waiting={isWaiting(session)} />
      </Link>
      <SessionMenu session={session} onRename={() => ctx.setRenaming(session.session)} />
    </li>
  );
}

function ParentRow({ session, ctx, open, kidCount }: { session: SessionSummary; ctx: RowContext; open: boolean; kidCount: number }) {
  const title = sessionTitle(session);
  const sidebar = useSidebarContext();
  const actions = useSessionActions();
  if (ctx.renaming === session.session) {
    return (
      <li>
        <RenameRow session={session} onDone={() => ctx.setRenaming(null)} />
      </li>
    );
  }
  const current = ctx.currentId === session.session;
  return (
    <li className="group/row relative">
      {/* 整行可点；名字是键盘入口（Link），▸ 单独开合子任务 */}
      <div
        onClick={() => {
          actions.open(session.session);
          sidebar.closeDrawer?.();
        }}
        className={cn(ROW, "cursor-pointer gap-0.5", current && "bg-active font-medium", isArchived(session) && "text-muted")}
      >
        <Link
          to="/sessions/$sessionId"
          params={{ sessionId: session.session }}
          title={title}
          onClick={(event) => event.stopPropagation()}
          className="min-w-0 truncate"
        >
          {title}
        </Link>
        <button
          type="button"
          aria-expanded={open}
          aria-label={open ? m.sessions_kids_collapse({ count: kidCount }) : m.sessions_kids_expand({ count: kidCount })}
          onClick={(event) => {
            event.stopPropagation();
            ctx.toggleNavOpen(session.session);
          }}
          className="grid size-5 shrink-0 cursor-pointer place-items-center rounded-xs text-muted hover:bg-active hover:text-text"
        >
          <Icon name="right" className={cn("size-3 stroke-2 transition-transform", open && "rotate-90")} />
        </button>
        <span className="ml-1.5 flex">
          <StateDot running={isRunning(session)} waiting={isWaiting(session)} />
        </span>
      </div>
      <SessionMenu session={session} onRename={() => ctx.setRenaming(session.session)} />
    </li>
  );
}

/** 行内改名：Enter 存、Esc 放弃、失焦也存。 */
function RenameRow({ session, child = false, onDone }: { session: SessionSummary; child?: boolean; onDone: () => void }) {
  const actions = useSessionActions();
  const input = useRef<HTMLInputElement>(null);
  const finished = useRef(false);
  useEffect(() => {
    input.current?.focus();
    input.current?.select();
  }, []);
  const finish = (save: boolean) => {
    if (finished.current) return;
    finished.current = true;
    if (save && input.current) void actions.rename(session, input.current.value);
    onDone();
  };
  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter" && !event.nativeEvent.isComposing) {
      event.preventDefault();
      finish(true);
    } else if (event.key === "Escape") {
      event.preventDefault();
      finish(false);
    }
  };
  return (
    <div className={cn("flex h-8.5 items-center rounded-control bg-hover px-2", child && "pl-7")}>
      <input
        ref={input}
        defaultValue={session.title}
        placeholder={sessionTitle(session)}
        aria-label={m.session_rename_label()}
        maxLength={120}
        onKeyDown={onKeyDown}
        onBlur={() => finish(true)}
        className="h-6.5 min-w-0 flex-1 rounded-xs bg-surface px-1.5 text-text shadow-[0_0_0_2px_var(--color-focus)] outline-none"
      />
    </div>
  );
}

