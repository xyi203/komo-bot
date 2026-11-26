import { keys, restoreSession, useArchivedChats, useChats, useInterventions, useMemoryIndex, useModels, useSessionChanges, useSessionEvents, useSessions, useSessionTree } from "@komo/api";
import type { InterventionSummary, RunChanges } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { Link, useRouterState } from "@tanstack/react-router";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { ChatMenu } from "@/features/chats/chat-menu";
import { Icon } from "@/components/icon";
import { Button } from "@/components/ui/button";
import { IconButton } from "@/components/ui/icon-button";
import { BrowserPane, desktopBrowser, useBrowserOpen } from "@/features/browser/browser-pane";
import { contextView } from "@/features/composer/context-usage";
import { Composer } from "@/features/composer/composer";
import { useSend } from "@/features/composer/use-send";
import { ConversationProvider, type ConversationContext, type ProcessTab } from "@/features/conversation/context";
import { foldEvents, type Turn } from "@/features/conversation/model";
import { Thread } from "@/features/conversation/thread";
import { PanelLauncher, type PanelEntry } from "@/features/process/panel-launcher";
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
  const models = useModels();
  const changesQuery = useSessionChanges(sessionId);
  const trace = useTraceUi();
  const browserApi = desktopBrowser();
  const [browserOpen, setBrowserOpen] = useBrowserOpen();
  const showBrowser = !!browserApi && browserOpen;

  const summary = sessions.data?.find((s) => s.session === sessionId);
  // 会话列表是分页的，归档的那段未必在已取的页里：不在对话列里的支线再对一下已归档列表
  const chatRows = chats.data?.chats;
  const inChats = !!chatRows?.some((chat) => chat.id === sessionId);
  const archivedChats = useArchivedChats(!main && !!chatRows && !inChats);
  const archivedRow = archivedChats.data?.chats.find((chat) => chat.id === sessionId);
  const archived = (!!summary && isArchived(summary)) || !!archivedRow;
  const queryClient = useQueryClient();
  const [restoring, setRestoring] = useState(false);
  const restore = async () => {
    setRestoring(true);
    try {
      await restoreSession(sessionId);
    } finally {
      // `keys.chats` 是前缀：对话列与已归档一起重取
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: keys.chats }),
        queryClient.invalidateQueries({ queryKey: keys.sessions.all }),
      ]);
      setRestoring(false);
    }
  };
  const view = useMemo(() => (events ? foldEvents(events) : undefined), [events]);
  const { pending, send, retry, stop } = useSend(sessionId, events, view?.runs ?? new Map());
  const kids = useMemo(() => (sessions.data ?? []).filter((s) => s.kind === "task" && s.home === sessionId && isLive(s)), [sessions.data, sessionId]);

  const goal = main ? undefined : chatRows?.find((chat) => chat.id === sessionId)?.goal;
  const name = main ? m.chat_name_main() : (chatRows?.find((chat) => chat.id === sessionId)?.title || archivedRow?.title || sessionTitle(summary));
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
  // 面板入口（右上角侧栏开关）：右栏还没选面板时显示；选了就换成那个面板
  const [launcher, setLauncher] = useState(false);
  // 会话树很大，首屏不取：过程面板的系统提示要它；上下文占用只在还没有实报用量时拿它估算
  const tree = useSessionTree(sessionId, !!panel || (!!view && !view.usage));
  const context = useMemo(() => contextView(view, tree.data, models.data), [view, tree.data, models.data]);
  const changes = useMemo(() => new Map((changesQuery.data?.runs ?? []).map((r): [string, RunChanges] => [r.run, r])), [changesQuery.data]);
  const [openFiles, setOpenFiles] = useState<Record<string, boolean>>({});
  const [reviewAt, setReviewAt] = useState<string | null>(null);
  const openProcess = useCallback((run: string, tab: ProcessTab = "steps") => {
    setLauncher(false);
    setPanel({ run, tab });
  }, []);
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
      setLauncher(false);
      setPanel({ run, tab: "changes" });
    },
    [],
  );

  // 入口里的几项：过程与轨迹定位到最近一轮，改动定位到最近改过文件的那一轮；浏览器只在桌面壳里有
  const closeLauncher = useCallback(() => setLauncher(false), []);
  const entries = useMemo<PanelEntry[]>(() => {
    const turns = (view?.items ?? []).filter((item): item is Turn => item.kind === "turn");
    const last = turns.at(-1)?.run;
    const changed = [...turns].reverse().find((t) => (changes.get(t.run)?.total ?? 0) > 0)?.run;
    const list: PanelEntry[] = [
      { key: "p", icon: "details", label: m.proc_title(), hint: last ? m.panel_process_hint() : m.panel_none_yet(), disabled: !last, onOpen: () => last && openProcess(last, "steps") },
      {
        key: "c",
        icon: "diff",
        label: m.proc_tab_changes(),
        hint: changed ? m.panel_changes_hint() : last ? m.panel_no_changes() : m.panel_none_yet(),
        disabled: !changed,
        onOpen: () => changed && openProcess(changed, "changes"),
      },
      { key: "t", icon: "clock", label: m.proc_tab_trace(), hint: last ? m.panel_trace_hint() : m.panel_none_yet(), disabled: !last, onOpen: () => last && openProcess(last, "trace") },
    ];
    if (browserApi)
      list.push({
        key: "b",
        icon: "globe",
        label: m.browser_title(),
        hint: showBrowser ? m.browser_close() : m.panel_browser_hint(),
        active: showBrowser,
        onOpen: () => {
          setBrowserOpen(!showBrowser);
          setLauncher(false);
        },
      });
    return list;
  }, [view, changes, browserApi, showBrowser, setBrowserOpen, openProcess]);

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
  const sideOpen = !!panel || launcher;
  const toggleSide = () => {
    if (sideOpen) {
      setPanel(null);
      setLauncher(false);
    } else setLauncher(true);
  };
  return (
    <ConversationProvider value={conversation}>
      <ChatLayout
        name={name}
        title={!main ? <ChatMenu id={sessionId} title={name} current archived={archived} asTitle /> : undefined}
        scroller={scroller}
        onScroll={(el) => {
          atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
        }}
        corner={
          <>
            {goal && (
              <Link
                to="/goals/$id"
                params={{ id: goal.id }}
                title={m.chats_goal({ title: goal.title })}
                className="inline-flex h-8 max-w-60 items-center gap-1.5 rounded-full bg-surface pr-3 pl-2.5 text-label text-muted no-underline shadow-[0_0_0_1px_rgb(0_0_0/0.1)] transition-colors hover:bg-hover-on-white hover:text-text max-[600px]:max-w-32"
              >
                <Icon name="flag" className="size-3.5" />
                <span className="truncate">{goal.title}</span>
              </Link>
            )}
            {!main && archived && (
              <Button type="button" disabled={restoring} onClick={() => void restore()}>
                <Icon name="reload" className="size-4" />
                {m.chat_restore()}
              </Button>
            )}
            <IconButton
              name="panel-right"
              label={sideOpen ? m.panel_close() : m.panel_open()}
              aria-expanded={sideOpen}
              onClick={toggleSide}
              className={cn(sideOpen && "bg-active text-text")}
            />
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
          ) : launcher ? (
            <PanelLauncher entries={entries} onClose={closeLauncher} />
          ) : undefined
        }
        composer={
          <Composer
            sessionId={sessionId}
            main={main}
            context={context}
            disabled={!ready}
            archived={archived}
            busy={busy}
            onStop={stop}
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
          view && <Thread view={view} delta={delta} pending={pending} kids={kids} isHome={main} busy={busy} onRetry={retry} />
        )}
      </ChatLayout>
    </ConversationProvider>
  );
}
