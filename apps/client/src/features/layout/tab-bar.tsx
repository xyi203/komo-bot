import { useChats, usePendingMemories } from "@komo/api";
import { Link, useRouterState } from "@tanstack/react-router";
import type { ComponentProps, ReactNode } from "react";
import { Icon, type IconName } from "@/components/icon";
import { openPalette, usePaletteOpen } from "@/features/palette/store";
import { openSettings, useSettingsState } from "@/features/settings/store";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { isChatPath } from "./paths";
import { useSettingsProblem } from "./rail";

const item =
  "relative flex min-h-14 min-w-0 cursor-pointer flex-col items-center justify-center gap-0.5 text-[11px] leading-4 text-muted no-underline transition-colors active:bg-hover";
const on = "text-text [&_svg]:stroke-[1.9]";

/**
 * 窄屏的底部标签栏（图标栏在手机上的样子）：对话、搜索、目标、记忆、设置。
 * 角标与图标栏一致：对话上是在等你的条数，记忆上是待过目的点，设置上是连接 / 配置的提醒点。
 * 底部留出 iOS 的安全区（主屏幕横条）。
 */
export function TabBar() {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const waiting = useChats().data?.waitingCount ?? 0;
  const pending = usePendingMemories().data?.length ?? 0;
  const paletteOpen = usePaletteOpen();
  const settingsOpen = useSettingsState().open;
  const { problem, label: settingsLabel } = useSettingsProblem();
  const dot = "absolute top-2 left-1/2 ml-2.5 size-2 rounded-full shadow-[0_0_0_2px_var(--color-ground)]";
  return (
    <nav aria-label={m.nav_label()} className="grid shrink-0 grid-cols-5 border-t border-border bg-ground pb-[env(safe-area-inset-bottom)]">
      <Tab to="/chats" icon="chat" text={m.nav_chats()} label={waiting > 0 ? m.nav_chats_waiting({ count: waiting }) : m.nav_chats()} active={isChatPath(pathname)}>
        {waiting > 0 && (
          <span
            aria-hidden="true"
            className="absolute top-1.5 left-1/2 ml-1 h-4 min-w-4 rounded-full bg-wait-dot px-1 text-center text-[10px] leading-4 font-bold text-white tabular-nums shadow-[0_0_0_2px_var(--color-ground)]"
          >
            {waiting}
          </span>
        )}
      </Tab>
      <button type="button" aria-label={m.nav_search()} aria-pressed={paletteOpen} onClick={openPalette} className={cn(item, paletteOpen && on)}>
        <Icon name="search" />
        {m.nav_search()}
      </button>
      <Tab to="/goals" icon="target" text={m.nav_goals()} label={m.nav_goals()} active={pathname.startsWith("/goals")} />
      <Tab to="/memory" icon="leaf" text={m.nav_memory()} label={pending > 0 ? m.nav_memory_pending({ count: pending }) : m.nav_memory()} active={pathname.startsWith("/memory")}>
        {pending > 0 && <span aria-hidden="true" className={cn(dot, "bg-wait-dot")} />}
      </Tab>
      <button type="button" aria-label={settingsLabel} aria-pressed={settingsOpen} onClick={() => openSettings()} className={cn(item, settingsOpen && on)}>
        <Icon name="gear" />
        {m.nav_settings()}
        {problem && <span aria-hidden="true" className={cn(dot, problem === "down" ? "bg-negative" : "bg-wait-dot")} />}
      </button>
    </nav>
  );
}

function Tab({
  to,
  icon,
  text,
  label,
  active,
  children,
}: {
  to: ComponentProps<typeof Link>["to"];
  icon: IconName;
  text: string;
  label: string;
  active: boolean;
  children?: ReactNode;
}) {
  return (
    <Link to={to} aria-label={label} aria-current={active ? "page" : undefined} className={cn(item, active && on)}>
      <Icon name={icon} />
      {text}
      {children}
    </Link>
  );
}
