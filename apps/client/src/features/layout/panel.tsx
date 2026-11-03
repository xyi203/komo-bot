import { useCallback, useState, type ReactNode } from "react";
import { IconButton } from "@/components/ui/icon-button";
import { Sheet, SheetContent, SheetDescription, SheetTitle } from "@/components/ui/sheet";
import { useMediaQuery } from "@/lib/hooks";
import { readStored, writeStored } from "@/lib/storage";
import { m } from "@/paraglide/messages.js";

const KEY = "komo.panelClosed";

/**
 * 右栏的开合：宽屏（> 1180）常驻在对话右边，开关记在本机（`komo.panelClosed`）；
 * 窄屏它是右侧抽屉，每次都从关着开始。
 */
export function usePanel() {
  const docked = useMediaQuery("(min-width: 1181px)");
  const [closed, setClosed] = useState(() => readStored(KEY, (raw) => raw === 1 || raw === "1", false));
  const [drawer, setDrawer] = useState(false);
  const open = docked ? !closed : drawer;
  const toggle = useCallback(() => {
    if (docked) {
      setClosed((previous) => {
        writeStored(KEY, previous ? "0" : "1");
        return !previous;
      });
    } else {
      setDrawer((previous) => !previous);
    }
  }, [docked]);
  return { docked, open, toggle, setDrawer };
}

/** 宽屏时是对话右边的一栏（336），窄屏时是带遮罩的抽屉。 */
export function PanelShell({ panel, children }: { panel: ReturnType<typeof usePanel>; children: ReactNode }) {
  if (panel.docked) {
    return panel.open ? <aside aria-label={m.panel_title()} className="min-h-0 overflow-auto px-4 pt-2 pb-6">{children}</aside> : null;
  }
  return (
    <Sheet open={panel.open} onOpenChange={panel.setDrawer}>
      <SheetContent side="right" className="overflow-auto border-l border-border bg-ground px-4 pt-4 pb-6">
        <div className="mb-2 flex h-10 items-center justify-between">
          <SheetTitle className="text-section-title font-semibold">{m.panel_title()}</SheetTitle>
          <IconButton name="x" label={m.close()} onClick={() => panel.setDrawer(false)} />
        </div>
        <SheetDescription className="sr-only">{m.panel_title()}</SheetDescription>
        {children}
      </SheetContent>
    </Sheet>
  );
}
