import { isApiError, useSessions } from "@komo/api";
import { Outlet } from "@tanstack/react-router";
import { useCallback, useEffect, useMemo, useState } from "react";
import { Toaster } from "@/components/toaster";
import { Sheet, SheetContent, SheetDescription, SheetTitle } from "@/components/ui/sheet";
import { Unauthorized } from "@/components/unauthorized";
import { Palette } from "@/features/palette/palette";
import { togglePalette } from "@/features/palette/store";
import { SettingsDialog } from "@/features/settings/settings-dialog";
import { useMediaQuery } from "@/lib/hooks";
import { m } from "@/paraglide/messages.js";
import { Sidebar } from "./sidebar";
import { SidebarProvider, type SidebarContext } from "./sidebar-context";

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
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("komo-desktop:command-palette", togglePalette);
      cancel?.();
    };
  }, []);
}

/**
 * 外壳（docs/web.md §2）：1440 宽是 280 的侧栏加主区；≤900 侧栏变左侧抽屉（Radix Dialog，焦点圈在里面、Esc 关）。
 * 主区放当前路由的页面；没配对（401）时整块换成配对提示。
 */
export function AppShell() {
  const sessions = useSessions(true);
  usePaletteShortcut();
  const narrow = useMediaQuery("(max-width: 900px)");
  const [drawer, setDrawer] = useState(false);
  const closeDrawer = useCallback(() => setDrawer(false), []);
  const context = useMemo<SidebarContext>(
    () => ({ narrow, openDrawer: () => setDrawer(true), closeDrawer: narrow ? closeDrawer : undefined }),
    [narrow, closeDrawer],
  );
  const unauthorized = sessions.isError && isApiError(sessions.error) && sessions.error.unauthorized;
  const sidebarClass = "flex min-h-0 flex-col border-r border-border bg-ground";
  return (
    <SidebarProvider value={context}>
      <div className="grid h-svh grid-cols-[280px_minmax(0,1fr)] overflow-hidden max-[900px]:grid-cols-1">
        {narrow ? (
          <Sheet open={drawer} onOpenChange={setDrawer}>
            <SheetContent side="left" className={sidebarClass} aria-label={m.nav_label()}>
              <SheetTitle className="sr-only">{m.nav_label()}</SheetTitle>
              <SheetDescription className="sr-only">{m.nav_label()}</SheetDescription>
              <Sidebar />
            </SheetContent>
          </Sheet>
        ) : (
          <aside aria-label={m.nav_label()} className={sidebarClass}>
            <Sidebar />
          </aside>
        )}
        <main className="flex min-h-0 min-w-0 flex-col overflow-y-auto bg-surface">{unauthorized ? <Unauthorized /> : <Outlet />}</main>
      </div>
      <Palette />
      <SettingsDialog />
      <Toaster />
    </SidebarProvider>
  );
}
