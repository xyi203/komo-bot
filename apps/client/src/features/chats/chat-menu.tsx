import { archiveSession, keys, renameSession } from "@komo/api";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Icon } from "@/components/icon";
import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from "@/components/ui/dropdown-menu";
import { errorText } from "@/lib/errors";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";

/**
 * 支线对话的「…」菜单：改名、归档（主对话没有）。对话列每行悬停时出现，支线对话页右上角也放一个。
 * 改名调 `POST /v1/sessions/{id}/name`，归档调 `POST /v1/sessions/{id}/delete`（软删除，内容还在，只是不再出现在对话列）；
 * 归档的是当前打开的这段就回到主对话 `/`。两个动作都是小弹窗，失败时错误留在弹窗里，不关。
 */
export function ChatMenu({ id, title, current, className }: { id: string; title: string; current: boolean; className?: string }) {
  const [dialog, setDialog] = useState<"rename" | "archive" | null>(null);
  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <button
            type="button"
            aria-label={m.chat_menu()}
            title={m.chat_menu()}
            className={cn(
              "grid size-7 cursor-pointer place-items-center rounded-control text-muted transition-colors hover:bg-hover-on-white hover:text-text data-[state=open]:bg-hover-on-white data-[state=open]:text-text",
              className,
            )}
          >
            <Icon name="more" />
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" size="compact">
          <DropdownMenuItem size="compact" onSelect={() => setDialog("rename")}>
            <Icon name="edit" className="size-4 text-muted" />
            {m.chat_rename()}
          </DropdownMenuItem>
          <DropdownMenuItem size="compact" danger onSelect={() => setDialog("archive")}>
            <Icon name="trash" className="size-4" />
            {m.chat_archive()}
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <Dialog open={dialog !== null} onOpenChange={(open) => !open && setDialog(null)}>
        <DialogContent className="w-[min(400px,calc(100vw-32px))] p-5">
          {dialog === "rename" && <RenameForm id={id} title={title} onDone={() => setDialog(null)} />}
          {dialog === "archive" && <ArchiveConfirm id={id} title={title} current={current} onDone={() => setDialog(null)} />}
        </DialogContent>
      </Dialog>
    </>
  );
}

function useRefreshChats() {
  const queryClient = useQueryClient();
  return () => {
    void queryClient.invalidateQueries({ queryKey: keys.chats });
    void queryClient.invalidateQueries({ queryKey: keys.sessions.all });
  };
}

function RenameForm({ id, title, onDone }: { id: string; title: string; onDone: () => void }) {
  const refresh = useRefreshChats();
  // 没有标题时传进来的是「未命名的对话」这句占位文案，不当成现有标题预填
  const [name, setName] = useState(title === m.session_untitled() ? "" : title);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const next = name.trim();
  const submit = async () => {
    if (!next || busy) return;
    if (next === title) return onDone();
    setBusy(true);
    setError("");
    try {
      await renameSession(id, next);
      refresh();
      onDone();
    } catch (failure) {
      setError(errorText(failure));
      setBusy(false);
    }
  };
  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <DialogTitle className="mb-3 text-section-title font-semibold">{m.chat_rename()}</DialogTitle>
      <DialogDescription className="sr-only">{m.chat_rename_hint()}</DialogDescription>
      <input
        autoFocus
        value={name}
        maxLength={120}
        aria-label={m.chat_rename_hint()}
        placeholder={m.chat_rename_hint()}
        onChange={(event) => setName(event.target.value)}
        onFocus={(event) => event.currentTarget.select()}
        className="h-9 w-full rounded-control bg-surface px-3 text-body shadow-[0_0_0_1px_rgb(0_0_0/0.2)] outline-none focus:shadow-[0_0_0_1px_var(--color-muted)]"
      />
      {error && <p className="mt-2 text-label text-negative">{m.chat_action_failed({ message: error })}</p>}
      <div className="mt-4 flex justify-end gap-2">
        <DialogClose asChild>
          <Button type="button">
            {m.cancel()}
          </Button>
        </DialogClose>
        <Button type="submit" variant="primary" disabled={!next || busy}>
          {m.chat_save()}
        </Button>
      </div>
    </form>
  );
}

function ArchiveConfirm({ id, title, current, onDone }: { id: string; title: string; current: boolean; onDone: () => void }) {
  const refresh = useRefreshChats();
  const navigate = useNavigate();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const archive = async () => {
    setBusy(true);
    setError("");
    try {
      // 还在跑的 Run 一并取消：用户确认过了，不让会话停在"关闭中"
      await archiveSession(id, { now: true });
      refresh();
      onDone();
      if (current) void navigate({ to: "/" });
    } catch (failure) {
      setError(errorText(failure));
      setBusy(false);
    }
  };
  return (
    <>
      <DialogTitle className="mb-2 text-section-title font-semibold">{m.chat_archive_title({ title })}</DialogTitle>
      <DialogDescription className="text-read leading-6 text-muted">{m.chat_archive_body()}</DialogDescription>
      {error && <p className="mt-2 text-label text-negative">{m.chat_action_failed({ message: error })}</p>}
      <div className="mt-4 flex justify-end gap-2">
        <DialogClose asChild>
          <Button type="button">
            {m.cancel()}
          </Button>
        </DialogClose>
        <Button type="button" variant="danger" disabled={busy} onClick={() => void archive()}>
          {m.chat_archive()}
        </Button>
      </div>
    </>
  );
}
