import { useHealth, useInboxCount } from "@komo/api";
import { Link } from "@tanstack/react-router";
import type { ComponentProps } from "react";
import { Icon, type IconName } from "@/components/icon";
import { IconButton } from "@/components/ui/icon-button";
import { openPalette } from "@/features/palette/store";
import { openSettings } from "@/features/settings/store";
import { useSessionActions } from "@/features/sessions/actions";
import { SessionSection } from "@/features/sessions/session-list";
import { useStreamStatus } from "@/lib/stream-status";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { useSidebarContext } from "./sidebar-context";

/**
 * 侧栏：标志、顶部入口（新对话、搜索、待处理、事项、记忆、定时）、会话区、底部用户行（连接状态与设置）。
 * 入口各指一页（C1 的 B、C 段逐页接上；现在是占位路由），⌘K 搜索弹窗由 B 段接到 `openPalette`。
 */
export function Sidebar() {
  const { closeDrawer, narrow } = useSidebarContext();
  return (
    <>
      <div className="traffic-clear flex h-12 items-center gap-2 px-4">
        <svg viewBox="0 0 28 28" aria-hidden="true" className="size-5.5">
          <circle cx="11" cy="12" r="8" fill="#4f6b38" />
          <circle cx="20.5" cy="18.5" r="4.5" fill="#4f6b38" fillOpacity="0.55" />
          <circle cx="20" cy="7" r="2.5" fill="#4f6b38" fillOpacity="0.3" />
        </svg>
        <span className="text-section-title font-semibold tracking-tight">{m.app_name()}</span>
        {narrow && <IconButton name="x" label={m.nav_close()} className="ml-auto" onClick={closeDrawer} />}
      </div>
      <TopNav />
      <SessionSection />
      <UserRow />
    </>
  );
}

const item =
  "flex h-8.5 w-full cursor-pointer items-center gap-2.5 rounded-control px-2 text-left text-text no-underline transition-colors hover:bg-hover [&>svg]:text-muted";

function TopNav() {
  const { closeDrawer } = useSidebarContext();
  const inbox = useInboxCount();
  return (
    <ul className="px-2 pt-1 pb-3">
      <li>
        <NewSession className={item} />
      </li>
      <li>
        <button
          type="button"
          className={item}
          onClick={() => {
            closeDrawer?.();
            openPalette();
          }}
        >
          <Icon name="search" />
          {m.nav_search()}
          <kbd className="ml-auto font-sans text-small text-faint">⌘K</kbd>
        </button>
      </li>
      <NavLink to="/inbox" icon="inbox" label={m.nav_inbox()}>
        {inbox > 0 && (
          <span className="ml-auto h-4.5 min-w-5 rounded-[9px] bg-negative px-1.5 text-center text-[11px] leading-[18px] font-semibold text-white tabular-nums">
            {inbox}
          </span>
        )}
      </NavLink>
      <NavLink to="/matters" icon="target" label={m.nav_matters()} />
      <NavLink to="/memory" icon="leaf" label={m.nav_memory()} />
      <NavLink to="/cron" icon="clock" label={m.nav_cron()} />
    </ul>
  );
}

function NewSession({ className }: { className: string }) {
  const actions = useSessionActions();
  const { closeDrawer } = useSidebarContext();
  return (
    <button
      type="button"
      className={className}
      onClick={() => {
        closeDrawer?.();
        void actions.create();
      }}
    >
      <Icon name="plus" />
      {m.nav_new_session()}
    </button>
  );
}

function NavLink({ to, icon, label, children }: { to: ComponentProps<typeof Link>["to"]; icon: IconName; label: string; children?: React.ReactNode }) {
  const { closeDrawer } = useSidebarContext();
  return (
    <li>
      <Link to={to} className={item} onClick={closeDrawer}>
        <Icon name={icon} />
        {label}
        {children}
      </Link>
    </li>
  );
}

/** 底部：Gateway 版本与连接状态（已连接 / 事件流重连中 / 连不上），设置入口。 */
function UserRow() {
  const health = useHealth();
  const stream = useStreamStatus();
  const { closeDrawer } = useSidebarContext();
  const state = health.isPending ? "loading" : health.isError ? "down" : stream === "reconnecting" ? "retrying" : "ok";
  const text =
    state === "loading" ? m.conn_connecting()
    : state === "down" ? m.conn_down()
    : state === "retrying" ? m.conn_retrying()
    : m.conn_ok({ version: health.data?.version ?? "" });
  return (
    <div className="flex h-13 items-center gap-2 border-t border-border pr-2 pl-4">
      <span aria-hidden="true" className="grid size-6 place-items-center rounded-full bg-active text-small font-semibold text-text">
        Y
      </span>
      <span className="flex min-w-0 flex-col text-body leading-4 font-medium">
        Yi
        <span className="truncate text-small font-normal text-muted">{text}</span>
      </span>
      <span
        role="img"
        aria-label={state === "ok" ? m.conn_label_ok() : m.conn_label_down()}
        className={cn("ml-auto size-1.5 shrink-0 rounded-full", state === "ok" ? "bg-accent" : state === "loading" ? "bg-control" : "bg-negative")}
      />
      <button
        type="button"
        aria-label={m.nav_settings()}
        title={m.nav_settings()}
        onClick={() => {
          closeDrawer?.();
          openSettings();
        }}
        className="grid size-8 shrink-0 cursor-pointer place-items-center rounded-control text-muted hover:bg-hover hover:text-text"
      >
        <Icon name="gear" />
      </button>
    </div>
  );
}
