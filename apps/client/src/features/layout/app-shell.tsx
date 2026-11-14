import { isApiError, useChats } from "@komo/api";
import { Outlet, useRouterState } from "@tanstack/react-router";
import { useEffect } from "react";
import { Toaster } from "@/components/toaster";
import { Unauthorized } from "@/components/unauthorized";
import { ChatColumn } from "@/features/chats/chat-column";
import { Palette } from "@/features/palette/palette";
import { togglePalette } from "@/features/palette/store";
import { SettingsDialog } from "@/features/settings/settings-dialog";
import { openSettings, settingsSectionOf } from "@/features/settings/store";
import { cn } from "@/lib/utils";
import { isChatPath } from "./paths";
import { Rail } from "./rail";

// C2（Electron 壳）给界面的约定见 `komo-desktop.d.ts`：`window.komoDesktop?.onCommandPalette(fn)` 返回取消函数；也会在 window 上派发
// DOM 事件 `komo-desktop:command-palette`。没有桌面壳（`komoDesktop` 不存在）时什么也不做。

/** ⌘K（Windows / Linux 上 Ctrl+K）开关搜索弹窗，并接上桌面壳的菜单命令。 */
function usePaletteShortcut() {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && !event.altKey && !event.repeat && event.key.toLowerCase() === "k") {
        event.preventDefault();
        togglePalette();
      }
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("komo-desktop:command-palette", togglePalette);
    const desktop = window.komoDesktop;
    const cancel = desktop?.onCommandPalette?.(togglePalette);
    // 桌面壳的 ⌘, 与菜单栏图标"设置…"：开这里的设置弹窗，不再跳到壳的连接设置页
    const cancelSettings = desktop?.onOpenSettings?.((section) => openSettings(settingsSectionOf(section)));
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("komo-desktop:command-palette", togglePalette);
      cancel?.();
      cancelSettings?.();
    };
  }, []);
}

/**
 * 外壳（docs/web.md §2）：64 的图标栏 + 264 的对话列 + 主区；不在对话页（目标、记忆）时没有对话列。
 * 主区放当前路由的页面；没配对（401）时整块换成配对提示。≤900 先不处理窄屏（5d 做）：对话列收起，图标栏留着。
 */
export function AppShell() {
  const chats = useChats();
  const chatPage = isChatPath(useRouterState({ select: (s) => s.location.pathname }));
  usePaletteShortcut();
  const unauthorized = chats.isError && isApiError(chats.error) && chats.error.unauthorized;
  return (
    <>
      {/* 桌面壳的拖窗口条（只在 macOS 桌面壳里显示，见 styles.css）：要排在最前，后面的按钮才能从拖动区里挖出来 */}
      <div aria-hidden="true" className="titlebar" />
      <div className={cn("grid h-svh overflow-hidden", chatPage ? "grid-cols-[64px_264px_minmax(0,1fr)] max-[900px]:grid-cols-[64px_minmax(0,1fr)]" : "grid-cols-[64px_minmax(0,1fr)]")}>
        <Rail />
        {chatPage && <ChatColumn />}
        <main className="flex min-h-0 min-w-0 flex-col overflow-y-auto bg-surface">{unauthorized ? <Unauthorized /> : <Outlet />}</main>
      </div>
      <Palette />
      <SettingsDialog />
      <Toaster />
    </>
  );
}
