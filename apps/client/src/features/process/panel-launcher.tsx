import { useEffect } from "react";
import { Icon, type IconName } from "@/components/icon";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { PanelShell, panelNavClass } from "./panel-shell";

export interface PanelEntry {
  /** 单字母快捷键（小写）：入口开着、焦点不在输入框里时按它直接打开。 */
  key: string;
  icon: IconName;
  label: string;
  /** 悬停说明；不可用时写不可用的原因。 */
  hint: string;
  disabled?: boolean;
  /** 已经开着（浏览器那一列）。 */
  active?: boolean;
  onOpen: () => void;
}

/** 焦点在能打字的地方时，字母键归输入，不当快捷键。 */
function typing(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return target.isContentEditable || target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.tagName === "SELECT";
}

/**
 * 面板入口（右上角的侧栏开关打开）：右侧一栏里居中一列可以打开的面板，每项带单字母快捷键；
 * 这段对话还用不上的（没说过话、没改过文件）置灰并在悬停里说原因。选一项就由它换成对应面板。
 */
export function PanelLauncher({ entries, onClose }: { entries: readonly PanelEntry[]; onClose: () => void }) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.metaKey || event.ctrlKey || event.altKey || event.repeat || event.isComposing || typing(event.target)) return;
      if (event.key === "Escape") {
        event.preventDefault();
        onClose();
        return;
      }
      const entry = entries.find((e) => e.key === event.key.toLowerCase());
      if (!entry || entry.disabled) return;
      event.preventDefault();
      entry.onOpen();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [entries, onClose]);

  return (
    <PanelShell label={m.panel_launcher_title()}>
      <header className="flex h-14 shrink-0 items-center justify-end pr-3 max-[900px]:pr-1.5">
        <button type="button" aria-label={m.panel_close()} title={m.panel_close()} onClick={onClose} className={panelNavClass}>
          <Icon name="x" />
        </button>
      </header>
      <div className="flex min-h-0 flex-1 flex-col items-center justify-center overflow-y-auto px-6 pb-24">
        <h2 className="mb-3 text-body font-medium">{m.panel_launcher_title()}</h2>
        <ul className="w-full max-w-68">
          {entries.map((entry) => (
            <li key={entry.key}>
              <button
                type="button"
                title={entry.hint}
                disabled={entry.disabled}
                aria-pressed={entry.active}
                aria-keyshortcuts={entry.key.toUpperCase()}
                onClick={entry.onOpen}
                className={cn(
                  "group/entry flex h-9 w-full cursor-pointer items-center gap-3 rounded-control px-2.5 text-body text-text transition-colors hover:bg-hover-on-white disabled:cursor-default disabled:text-faint disabled:hover:bg-transparent max-[900px]:h-11",
                  entry.active && "bg-active hover:bg-active",
                )}
              >
                <Icon name={entry.icon} className="size-4 shrink-0 text-muted group-disabled/entry:text-faint" />
                <span className="min-w-0 flex-1 truncate text-left">{entry.label}</span>
                <kbd className="grid h-5 min-w-5 place-items-center rounded-xs bg-raised px-1 font-sans text-small text-muted group-disabled/entry:opacity-60 max-[900px]:hidden">
                  {entry.key.toUpperCase()}
                </kbd>
              </button>
            </li>
          ))}
        </ul>
      </div>
    </PanelShell>
  );
}
