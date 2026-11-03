import type { SessionSummary } from "@komo/protocol";
import { useState, type KeyboardEvent } from "react";
import { Icon } from "@/components/icon";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { errorText } from "@/lib/errors";
import { m } from "@/paraglide/messages.js";
import { sessionTitle } from "./title";
import { isArchived } from "./model";
import { useSessionActions } from "./actions";

/**
 * 会话行尾的"…"和它的菜单（照 Claude Code 的会话菜单：只有文字和快捷键，不放图标；删除只用红字）：
 * 重命名 R、分叉 F、归档 A、删除 D；归档与删除先确认一次，原因写在菜单里。菜单开着时直接按字母，Esc 关。
 * 置顶、标未读、移到分组这几项 Gateway 没有对应的状态，没做（docs/web.md §3）。
 */
export function SessionMenu({ session, onRename }: { session: SessionSummary; onRename: () => void }) {
  const actions = useSessionActions();
  const [open, setOpen] = useState(false);
  const [confirm, setConfirm] = useState<"archive" | "delete" | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const archived = isArchived(session);
  const isMain = session.kind === "main";

  const reset = () => {
    setConfirm(null);
    setBusy(false);
    setError("");
  };
  const change = (next: boolean) => {
    setOpen(next);
    if (!next) reset();
  };
  const ask = (which: "archive" | "delete") => {
    setConfirm(which);
    setError("");
  };
  const run = async (task: () => Promise<void>) => {
    setBusy(true);
    setError("");
    try {
      await task();
      change(false);
    } catch (cause) {
      setBusy(false);
      setError(errorText(cause));
    }
  };

  const shortcut = (event: KeyboardEvent) => {
    if (confirm || event.metaKey || event.ctrlKey || event.altKey) return;
    const key = event.key.toLowerCase();
    const act =
      key === "r" ? () => { change(false); onRename(); }
      : key === "f" ? () => { change(false); void actions.fork(session.session); }
      : key === "a" && !isMain && !archived ? () => ask("archive")
      : key === "d" && !isMain ? () => ask("delete")
      : null;
    if (!act) return;
    event.preventDefault();
    act();
  };

  return (
    <DropdownMenu open={open} onOpenChange={change}>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          aria-label={m.session_menu_more({ title: sessionTitle(session) })}
          className="absolute top-[5px] right-1 grid size-6 cursor-pointer place-items-center rounded-sm text-muted opacity-0 group-hover/row:opacity-100 hover:bg-active hover:text-text focus-visible:opacity-100 data-[state=open]:bg-active data-[state=open]:text-text data-[state=open]:opacity-100 [@media(hover:none)]:opacity-100"
        >
          <Icon name="more" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent
        size="compact"
        align="start"
        onKeyDown={shortcut}
        // 改名的输入框要接着拿焦点，别被菜单关闭时还给"…"抢走
        onCloseAutoFocus={(event) => event.preventDefault()}
        className={confirm ? "w-58" : undefined}
      >
        {confirm === "archive" && (
          <div className="px-2 pt-2 pb-1.5">
            <p className="mb-2.5 text-label text-muted">{m.session_archive_body()}</p>
            <div className="flex gap-2">
              <Button variant="primary" className="h-7" disabled={busy} autoFocus onClick={() => void run(() => actions.archive(session.session))}>
                {m.session_archive()}
              </Button>
              <Button className="h-7" onClick={reset}>
                {m.cancel()}
              </Button>
            </div>
          </div>
        )}
        {confirm === "delete" && (
          <div className="px-2 pt-2 pb-1.5">
            <p className="mb-2.5 text-label text-muted">
              {archived ? m.session_delete_body_archived() : m.session_delete_body()}
            </p>
            <div className="flex gap-2">
              <Button
                className="h-7 text-negative shadow-[0_0_0_1px_var(--color-negative)] hover:bg-negative-pale"
                disabled={busy}
                autoFocus
                onClick={() => void run(() => actions.purge(session))}
              >
                {m.session_delete()}
              </Button>
              <Button className="h-7" onClick={reset}>
                {m.cancel()}
              </Button>
            </div>
          </div>
        )}
        {!confirm && (
          <>
            <DropdownMenuItem size="compact" onSelect={onRename}>
              <span>{m.session_rename()}</span>
              <Kbd>R</Kbd>
            </DropdownMenuItem>
            <DropdownMenuItem size="compact" onSelect={() => void actions.fork(session.session)}>
              <span>{m.session_fork()}</span>
              <Kbd>F</Kbd>
            </DropdownMenuItem>
            {!isMain && (
              <>
                <DropdownMenuSeparator className="mx-1.5 my-1" />
                {!archived && (
                  <DropdownMenuItem size="compact" onSelect={(event) => { event.preventDefault(); ask("archive"); }}>
                    <span>{m.session_archive()}</span>
                    <Kbd>A</Kbd>
                  </DropdownMenuItem>
                )}
                <DropdownMenuItem size="compact" danger onSelect={(event) => { event.preventDefault(); ask("delete"); }}>
                  <span>{m.session_delete()}</span>
                  <Kbd>D</Kbd>
                </DropdownMenuItem>
              </>
            )}
          </>
        )}
        {error && <p className="mx-2 mt-1.5 mb-1 text-label text-negative">{error}</p>}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function Kbd({ children }: { children: string }) {
  return <kbd className="ml-auto font-sans text-label text-faint">{children}</kbd>;
}
