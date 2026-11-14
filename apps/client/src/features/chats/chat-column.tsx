import type { ChatRow } from "@komo/protocol";
import { useChats } from "@komo/api";
import { Link, useRouterState } from "@tanstack/react-router";
import { Icon } from "@/components/icon";
import { errorText } from "@/lib/errors";
import { formatChatTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { ChatMenu } from "./chat-menu";
import { chatKeyOf } from "@/features/layout/paths";
import { m } from "@/paraglide/messages.js";

const row = "grid w-full cursor-pointer grid-cols-[minmax(0,1fr)_auto] items-center gap-x-2 rounded-inner px-2.5 py-2 text-left text-text no-underline transition-colors hover:bg-hover";

/**
 * 对话列（264 宽，只在对话页出现）：顶上"对话"与新对话的加号；主对话固定最上，下面是支线（在等你的在前，顺序由 `GET /v1/chats` 给）。
 * 一行：标题、最后一句话、时间；在等你的带黄圈，在跑的带橙点。
 */
export function ChatColumn() {
  const chats = useChats();
  const current = chatKeyOf(useRouterState({ select: (s) => s.location.pathname }));
  const main = chats.data?.chats.find((chat) => chat.main);
  const branches = chats.data?.chats.filter((chat) => !chat.main) ?? [];
  return (
    <aside aria-label={m.chats_title()} className="flex min-h-0 flex-col border-r border-border bg-ground max-[900px]:hidden">
      <header className="flex h-14 shrink-0 items-center pr-2 pl-4 text-section-title font-semibold">
        {m.chats_title()}
        <Link
          to="/chats/new"
          aria-label={m.chats_new()}
          title={m.chats_new()}
          className="ml-auto grid size-8 cursor-pointer place-items-center rounded-control text-muted no-underline transition-colors hover:bg-hover hover:text-text"
        >
          <Icon name="plus" className="size-4" />
        </Link>
      </header>
      <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
        {!chats.data && chats.isPending ? (
          <Skeleton />
        ) : !chats.data ? (
          <div className="mx-3 my-7 text-center text-label text-faint">
            <p className="mb-2">{m.chats_error({ message: errorText(chats.error) })}</p>
            <button type="button" className="cursor-pointer font-medium text-text underline underline-offset-3" onClick={() => void chats.refetch()}>
              {m.retry()}
            </button>
          </div>
        ) : (
          <>
            {main && <ChatItem chat={main} current={current === "main"} />}
            <p className="mx-2.5 mt-2.5 mb-1 text-small text-faint">{m.chats_branches()}</p>
            {current === "new" && (
              <Link to="/chats/new" aria-current="page" className={cn(row, "bg-active")}>
                <span className="truncate font-medium">{m.nav_new_session()}</span>
              </Link>
            )}
            {branches.map((chat) => (
              <ChatItem key={chat.id} chat={chat} current={current === chat.id} />
            ))}
            {branches.length === 0 && current !== "new" && (
              <div className="mx-5 my-6 text-center text-label text-faint">
                <b className="mb-1 block text-body font-semibold text-text">{m.chats_empty_title()}</b>
                {m.chats_empty_body()}
                <Link
                  to="/chats/new"
                  className="mt-3 inline-flex h-8 cursor-pointer items-center rounded-full bg-active px-3.5 text-label text-text no-underline transition-colors hover:bg-control"
                >
                  {m.chats_new()}
                </Link>
              </div>
            )}
          </>
        )}
      </div>
    </aside>
  );
}

function ChatItem({ chat, current }: { chat: ChatRow; current: boolean }) {
  const title = chat.main ? m.chats_main() : chat.title || m.session_untitled();
  const status = chat.waiting ? m.chats_waiting() : chat.running ? m.chats_running() : "";
  const body = (
    <>
      <span className="truncate font-medium">
        {chat.goal && (
          <span title={m.chats_goal({ title: chat.goal.title })} className="mr-1 inline-grid align-[-2px] text-faint">
            <Icon name="flag" className="size-3.25" />
            <span className="sr-only">{m.chats_goal({ title: chat.goal.title })}</span>
          </span>
        )}
        {title}
      </span>
      <span className="text-right text-small text-faint tabular-nums">{chat.lastMessage ? formatChatTime(chat.lastMessageAt) : ""}</span>
      <span className="truncate text-small leading-[18px] text-faint">{chat.lastMessage || m.chats_no_message()}</span>
      <span className="flex justify-end" title={status || undefined}>
        {chat.waiting ? (
          <span role="img" aria-label={status} className="size-2 rounded-full border-2 border-wait-dot" />
        ) : chat.running ? (
          <span role="img" aria-label={status} className="size-2 animate-breathe rounded-full bg-accent" />
        ) : null}
      </span>
    </>
  );
  const className = cn(row, current && "bg-active");
  if (chat.main) {
    return (
      <Link to="/" aria-current={current ? "page" : undefined} className={className}>
        {body}
      </Link>
    );
  }
  // 「…」菜单不能放进链接里：悬停、键盘聚焦或菜单开着时盖在时间的位置
  return (
    <div className="group/row relative">
      <Link to="/chats/$id" params={{ id: chat.id }} aria-current={current ? "page" : undefined} className={cn(className, "group-has-[:focus-visible]/row:[&>span:nth-child(2)]:invisible group-has-[[data-state=open]]/row:[&>span:nth-child(2)]:invisible [@media(hover:hover)]:group-hover/row:[&>span:nth-child(2)]:invisible")}>
        {body}
      </Link>
      <ChatMenu
        id={chat.id}
        title={title}
        current={current}
        className="absolute top-1 right-1.5 [@media(hover:hover)]:opacity-0 [@media(hover:hover)]:group-hover/row:opacity-100 [@media(hover:hover)]:group-has-[:focus-visible]/row:opacity-100 [@media(hover:hover)]:data-[state=open]:opacity-100"
      />
    </div>
  );
}

/** 加载：最终布局里放平的灰条，不做微光。 */
function Skeleton() {
  return (
    <div aria-hidden="true" className="px-2.5 pt-1">
      {[0, 1, 2].map((i) => (
        <div key={i} className="py-2">
          <span className="mb-1.5 block h-3.5 w-2/5 rounded-xs bg-active" />
          <span className="block h-3 w-4/5 rounded-xs bg-active" />
        </div>
      ))}
    </div>
  );
}
