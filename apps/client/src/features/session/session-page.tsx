import { useChats, useInterventions, useMemoryIndex, useModels, useSessionChanges, useSessionEvents, useSessions, useSessionTree } from "@komo/api";
import type { InterventionSummary, RunChanges } from "@komo/protocol";
import { useRouterState } from "@tanstack/react-router";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { ChatMenu } from "@/features/chats/chat-menu";
import { IconButton } from "@/components/ui/icon-button";
import { BrowserPane, desktopBrowser, useBrowserOpen } from "@/features/browser/browser-pane";
import { contextView } from "@/features/composer/context-usage";
import { Composer } from "@/features/composer/composer";
import { useSend } from "@/features/composer/use-send";
import { ConversationProvider, type ConversationContext, type ProcessTab } from "@/features/conversation/context";
import { foldEvents } from "@/features/conversation/model";
import { Thread } from "@/features/conversation/thread";
import { ProcessPanel } from "@/features/process/process-panel";
import { isArchived, isLive } from "@/features/sessions/model";
import { sessionTitle } from "@/features/sessions/title";
import { useTraceUi } from "@/features/trace/state";
import { errorText } from "@/lib/errors";
import { setStreamStatus } from "@/lib/stream-status";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { ChatLayout, SkeletonThread } from "./chat-layout";

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
  const changesQuery = useSessionChanges(sessionId);
  const trace = useTraceUi();
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

  // 过程面板：开着、看哪一轮、哪个标签。换对话整页重挂，状态跟着清掉
  const [panel, setPanel] = useState<{ run: string; tab: ProcessTab } | null>(null);
  const changes = useMemo(() => new Map((changesQuery.data?.runs ?? []).map((r): [string, RunChanges] => [r.run, r])), [changesQuery.data]);
  const [openFiles, setOpenFiles] = useState<Record<string, boolean>>({});
  const [reviewAt, setReviewAt] = useState<string | null>(null);
  const openProcess = useCallback((run: string, tab: ProcessTab = "steps") => setPanel({ run, tab }), []);
  const inspect = useCallback(
    (resultEvent: string) => {
      setPanel((prev) => prev && { ...prev, tab: "trace" });
      trace.setRange(null);
      trace.select(resultEvent, true);
      requestAnimationFrame(() => requestAnimationFrame(() => document.getElementById(`tr-${resultEvent}`)?.scrollIntoView({ block: "center" })));
    },
    [trace],
  );
  const review = useCallback(
    (run: string, index: number) => {
      setOpenFiles((previous) => ({ ...previous, [`${run}:${index}`]: true }));
      setReviewAt(`cg-${run}-${index}`);
      setPanel({ run, tab: "changes" });
    },
    [],
  );

  // 全部未答复的介入：投进对话的提问按句柄认领自己那条（`ReplyView`）
  const interventions = pendingInterventions.data?.interventions ?? NO_INTERVENTIONS;
  const conversation = useMemo<ConversationContext>(
    () => ({ memories, changes, processRun: panel?.run ?? null, openProcess, inspect, review, interventions }),
    [memories, changes, panel?.run, openProcess, inspect, review, interventions],
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

  // 正在换页（比如点了对话列的加号，新页面的代码还在加载）时旧输入框立刻不再收话：否则这时敲的字会发进这段对话
  const navigating = useRouterState({ select: (state) => state.status === "pending" });
  const ready = status === "ready" && !archived && !navigating;
  // 「正在输入」只给真在跑的 Run；停住等你答复的（`waiting`）由投进对话的提问和对话列的黄点表示
  const busy = [...(view?.runs.values() ?? [])].some((run) => !run.waiting);
  const closePanel = () => setPanel(null);
  return (
    <ConversationProvider value={conversation}>
      <ChatLayout
        name={name}
        scroller={scroller}
        onScroll={(el) => {
          atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
        }}
        corner={
          <>
            {browserApi && (
              <IconButton
                name="globe"
                label={showBrowser ? m.browser_close() : m.browser_open()}
                aria-expanded={showBrowser}
                onClick={() => setBrowserOpen(!showBrowser)}
                className={cn("max-[600px]:hidden", showBrowser && "bg-active text-text")}
              />
            )}
            {!main && !archived && <ChatMenu id={sessionId} title={name} current className="size-8" />}
          </>
        }
        aside={showBrowser && browserApi ? <BrowserPane api={browserApi} sessionId={sessionId} onClose={() => setBrowserOpen(false)} /> : undefined}
        panel={
          panel && view && events ? (
            <ProcessPanel
              view={view}
              events={events}
              tree={tree.data}
              memories={memories}
              run={panel.run}
              tab={panel.tab}
              changes={changes}
              changesUi={{
                open: openFiles,
                toggle: (key, current) => setOpenFiles((previous) => ({ ...previous, [key]: !current })),
                reviewAt,
                clearReviewAt: () => setReviewAt(null),
              }}
              trace={trace}
              kids={kids}
              delta={!!delta}
              onTab={(tab) => setPanel({ ...panel, tab })}
              onRun={(run) => setPanel({ ...panel, run })}
              onClose={closePanel}
            />
          ) : undefined
        }
        composer={
          <Composer
            sessionId={sessionId}
            main={main}
            context={context}
            disabled={!ready}
            archived={archived}
            onSend={(text, options) => {
              atBottom.current = true;
              send(text, options);
            }}
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
          view && <Thread view={view} delta={delta} pending={pending} kids={kids} isHome={main} busy={busy} onStop={stop} onRetry={retry} />
        )}
      </ChatLayout>
    </ConversationProvider>
  );
}
