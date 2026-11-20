import { useChats, useConfigIssueCount, usePendingMemories } from "@komo/api";
import { Link, useRouterState } from "@tanstack/react-router";
import type { ComponentProps, ReactNode } from "react";
import { Icon, type IconName } from "@/components/icon";
import { openPalette, usePaletteOpen } from "@/features/palette/store";
import { openSettings, useSettingsState } from "@/features/settings/store";
import { useConnection } from "@/lib/connection";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { isChatPath } from "./paths";

const button =
  "relative grid size-10 shrink-0 cursor-pointer place-items-center rounded-card text-muted no-underline transition-colors hover:bg-hover hover:text-text mac:h-13 mac:w-16 mac:content-center mac:gap-0.5";
const on = "bg-active text-text [&_svg]:stroke-[1.9]";
/** 桌面壳里图标下面的短标签（网页上图标栏只有 64 宽，不显示）；读屏已有按钮的 aria-label，这里不重复读。 */
function Label({ children }: { children: string }) {
  return (
    <span aria-hidden="true" className="hidden text-[11px] leading-3.5 mac:block">
      {children}
    </span>
  );
}

/**
 * 图标栏（64 宽）：Komo 标志、对话（带在等你的条数）、搜索、目标、记忆，底部设置。
 * 搜索打开 ⌘K 弹窗，设置打开设置弹窗；连接出了问题时设置图标上带一个点（取代旧侧栏底部的连接状态行）。
 */
export function Rail() {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const waiting = useChats().data?.waitingCount ?? 0;
  const pending = usePendingMemories().data?.length ?? 0;
  const paletteOpen = usePaletteOpen();
  const settingsOpen = useSettingsState().open;
  return (
    <nav aria-label={m.nav_label()} className="rail flex min-h-0 flex-col items-center gap-1.5 border-r border-border bg-ground py-3">
      <svg viewBox="0 0 28 28" aria-hidden="true" className="rail-logo size-6.5 shrink-0">
        <circle cx="11" cy="12" r="8" className="fill-accent" />
        <circle cx="20.5" cy="18.5" r="4.5" className="fill-accent" fillOpacity="0.55" />
        <circle cx="20" cy="7" r="2.5" className="fill-accent" fillOpacity="0.3" />
      </svg>
      {/* 网页上图标组竖直居中；桌面壳里紧跟在标志下面，不然加宽后的灰栏上下各空一大截 */}
      <div className="my-auto flex flex-col gap-1.5 mac:mt-6 mac:gap-1">
        <RailLink to="/" icon="chat" short={m.nav_chats()} label={waiting > 0 ? m.nav_chats_waiting({ count: waiting }) : m.nav_chats()} active={isChatPath(pathname)}>
          {waiting > 0 && (
            <span
              aria-hidden="true"
              className="absolute top-[3px] right-0.5 h-4 mac:top-0.5 mac:right-3.5 min-w-4 rounded-full bg-wait-dot px-1 text-center text-[10px] leading-4 font-bold text-white tabular-nums shadow-[0_0_0_2px_var(--color-ground)]"
            >
              {waiting}
            </span>
          )}
        </RailLink>
        <button
          type="button"
          aria-label={m.nav_search()}
          title={m.nav_search()}
          aria-pressed={paletteOpen}
          onClick={openPalette}
          className={cn(button, paletteOpen && on)}
        >
          <Icon name="search" />
          <Label>{m.nav_search()}</Label>
        </button>
        <RailLink to="/goals" icon="target" short={m.nav_goals()} label={m.nav_goals()} active={pathname.startsWith("/goals")} />
        <RailLink to="/memory" icon="leaf" short={m.nav_memory()} label={pending > 0 ? m.nav_memory_pending({ count: pending }) : m.nav_memory()} active={pathname.startsWith("/memory")}>
          {pending > 0 && (
            <span aria-hidden="true" className="absolute top-2 right-2 size-2 rounded-full bg-wait-dot mac:top-1.5 mac:right-5 shadow-[0_0_0_2px_var(--color-ground)]" />
          )}
        </RailLink>
      </div>
      <Settings open={settingsOpen} />
    </nav>
  );
}

function RailLink({
  to,
  icon,
  short,
  label,
  active,
  children,
}: {
  to: ComponentProps<typeof Link>["to"];
  icon: IconName;
  short: string;
  label: string;
  active: boolean;
  children?: ReactNode;
}) {
  return (
    <Link to={to} aria-label={label} title={label} aria-current={active ? "page" : undefined} className={cn(button, active && on)}>
      <Icon name={icon} />
      <Label>{short}</Label>
      {children}
    </Link>
  );
}

/** 设置入口上的提醒：连不上与重连中的点 2c 已做；配置检查有提醒时也带一个黄点（连接问题优先，红点盖过黄点）。图标栏与底部标签栏共用。 */
export function useSettingsProblem() {
  const connection = useConnection();
  const issues = useConfigIssueCount();
  const problem = connection ?? (issues > 0 ? "config" : null);
  const label = problem === "down" ? m.conn_down() : problem === "retrying" ? m.conn_retrying() : problem === "config" ? m.nav_settings_issues({ count: issues }) : m.nav_settings();
  return { problem, label };
}

function Settings({ open }: { open: boolean }) {
  const { problem, label } = useSettingsProblem();
  return (
    <button type="button" aria-label={label} title={label} aria-pressed={open} onClick={() => openSettings()} className={cn(button, open && on)}>
      <Icon name="gear" />
      <Label>{m.nav_settings()}</Label>
      {problem && (
        <span
          aria-hidden="true"
          className={cn("absolute top-2 right-2 size-2 mac:top-1.5 mac:right-5 rounded-full shadow-[0_0_0_2px_var(--color-ground)]", problem === "down" ? "bg-negative" : "bg-wait-dot")}
        />
      )}
    </button>
  );
}
