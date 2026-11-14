import { useChats, useInterventions, useMemoryIndex, useModels, useSessionEvents, useSessions, useSessionTree } from "@komo/api";
import type { InterventionSummary } from "@komo/protocol";
import { useEffect, useLayoutEffect, useMemo, useRef } from "react";
import { IconButton } from "@/components/ui/icon-button";
import { BrowserPane, desktopBrowser, useBrowserOpen } from "@/features/browser/browser-pane";
import { contextView } from "@/features/composer/context-usage";
import { Composer } from "@/features/composer/composer";
import { useSend } from "@/features/composer/use-send";
import { ConversationProvider, type ConversationContext } from "@/features/conversation/context";
import { foldEvents } from "@/features/conversation/model";
import { Thread } from "@/features/conversation/thread";
import { isArchived, isLive } from "@/features/sessions/model";
import { sessionTitle } from "@/features/sessions/title";
import { errorText } from "@/lib/errors";
import { setStreamStatus } from "@/lib/stream-status";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { ChatLayout, SkeletonThread } from "./chat-layout";

// 过程面板（步骤、改动、轨迹）下一步接上：在那之前对话里不显示每轮的改动卡片，也没有"查看"按钮。
const NO_CHANGES: ConversationContext["changes"] = new Map();
const NO_REVIEW = () => {};
const NO_INTERVENTIONS: readonly InterventionSummary[] = [];

/**
 * 一段对话的页面（docs/web.md §2–§4）：`/` 是主对话，`/chats/$id` 是支线。顶上居中的头像与名字，中间是按 Run 排下来的正文，
 * 下面是输入框。换对话就整页重挂（`key`），状态清掉。
 */
export function SessionPage({ sessionId, main }: { sessionId: string; main: boolean }) {
  return <SessionBody key={sessionId} sessionId={sessionId} main={main} />;
}

function SessionBody({ sessionId, main }: { sessionId: string; main: boolean }) {
  const sessions = useSessions(true);
  const chats = useChats();
  const memories = useMemoryIndex();
  const pendingInterventions = useInterventions();
  const { events, status, error, delta, stream, reload } = useSessionEvents(sessionId);
  const tree = useSessionTree(sessionId);
  const models = useModels();
  const browserApi = desktopBrowser();
  const [browserOpen, setBrowserOpen] = useBrowserOpen();
  const showBrowser = !!browserApi && browserOpen;

  const summary = sessions.data?.find((s) => s.session === sessionId);
  const archived = !!summary && isArchived(summary);
  const view = useMemo(() => (events ? foldEvents(events) : undefined), [events]);
  const { pending, send, retry, stop } = useSend(sessionId, events, view?.runs ?? new Map());
  const context = useMemo(() => contextView(view, tree.data, models.data), [view, tree.data, models.data]);
  const kids = useMemo(() => (sessions.data ?? []).filter((s) => s.kind === "task" && s.home === sessionId && isLive(s)), [sessions.data, sessionId]);

  const chatRows = chats.data?.chats;
  const name = main ? m.chat_name_main() : (chatRows?.find((chat) => chat.id === sessionId)?.title || sessionTitle(summary));
  // 连接状态给图标栏看；名字给浏览器标签
  useEffect(() => {
    setStreamStatus(stream);
    return () => setStreamStatus("idle");
  }, [stream]);
  useEffect(() => {
    document.title = `${name} · ${m.app_name()}`;
  }, [name]);

  // 全部未答复的介入：投进对话的提问按句柄认领自己那条（`ReplyView`）
  const interventions = pendingInterventions.data?.interventions ?? NO_INTERVENTIONS;
  const conversation = useMemo<ConversationContext>(
    () => ({ memories, changes: NO_CHANGES, review: NO_REVIEW, interventions }),
    [memories, interventions],
  );

  // 滚动：对话在底部时跟着长；第一次出内容落到底
  const scroller = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);
  const opened = useRef(false);
  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el || !view) return;
    if (!opened.current || atBottom.current) {
      el.scrollTop = el.scrollHeight;
      opened.current = true;
    }
  }, [view, delta, pending]);

  const ready = status === "ready" && !archived;
  const busy = (view?.runs.size ?? 0) > 0;
  return (
    <ConversationProvider value={conversation}>
      <ChatLayout
        name={name}
        scroller={scroller}
        onScroll={(el) => {
          atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
        }}
        corner={
          browserApi && (
            <IconButton
              name="globe"
              label={showBrowser ? m.browser_close() : m.browser_open()}
              aria-expanded={showBrowser}
              onClick={() => setBrowserOpen(!showBrowser)}
              className={cn("max-[600px]:hidden", showBrowser && "bg-active text-text")}
            />
          )
        }
        aside={showBrowser && browserApi ? <BrowserPane api={browserApi} sessionId={sessionId} onClose={() => setBrowserOpen(false)} /> : undefined}
        composer={
          <Composer
            sessionId={sessionId}
            view={view}
            context={context}
            busy={busy}
            disabled={!ready}
            archived={archived}
            onSend={(text, options) => {
              atBottom.current = true;
              send(text, options);
            }}
            onStop={stop}
          />
        }
      >
        {status === "loading" ? (
          <SkeletonThread />
        ) : status === "error" ? (
          <div className="mx-auto mt-12 max-w-130 text-center text-read leading-6">
            <p className="mb-1.5">{m.session_load_error({ message: errorText(error) })}</p>
            <button type="button" className="mt-2 cursor-pointer font-medium text-text underline underline-offset-3" onClick={reload}>
              {m.retry()}
            </button>
          </div>
        ) : (
          view && <Thread view={view} delta={delta} pending={pending} kids={kids} isHome={main} onRetry={retry} />
        )}
      </ChatLayout>
    </ConversationProvider>
  );
}
