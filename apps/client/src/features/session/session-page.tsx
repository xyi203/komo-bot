import { useHomeSession, useMemoryIndex, useModels, useSessionChanges, useSessionEvents, useSessions, useSessionTree } from "@komo/api";
import type { RunChanges } from "@komo/protocol";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { IconButton } from "@/components/ui/icon-button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip } from "@/components/ui/tooltip";
import { ChangesPage } from "@/features/changes/changes-page";
import { Composer } from "@/features/composer/composer";
import { useSend } from "@/features/composer/use-send";
import { ConversationProvider } from "@/features/conversation/context";
import { foldEvents } from "@/features/conversation/model";
import { Thread } from "@/features/conversation/thread";
import { NavToggle } from "@/features/layout/nav-toggle";
import { PanelShell, usePanel } from "@/features/layout/panel";
import { contextView } from "@/features/panel/context-usage";
import { PanelContent } from "@/features/panel/panel-content";
import { useSessionActions } from "@/features/sessions/actions";
import { isArchived, isLive } from "@/features/sessions/model";
import { sessionTitle } from "@/features/sessions/title";
import { SessionTitle } from "./session-title";
import { useTraceUi } from "@/features/trace/state";
import { TraceView } from "@/features/trace/trace-view";
import { errorText } from "@/lib/errors";
import { setStreamStatus } from "@/lib/stream-status";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";

type Tab = "chat" | "trace" | "changes";

// 切到改动页的快捷键：⇧⌘D（Windows / Linux 上 Ctrl+Shift+D）
const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);
const CHANGES_SHORTCUT = isMac ? "⇧⌘D" : "Ctrl+Shift+D";

/**
 * 一个会话的页面（docs/web.md §2–§4）：顶栏（标题、对话 / 轨迹、右上角改动 / 分叉 / 右栏开关），中间是当前标签页，
 * 下面是发送框，右边是右栏。标签页的选择跨会话保留（只在这一层记）；其余状态换会话就清掉（`key`）。
 */
export function SessionPage({ sessionId }: { sessionId: string }) {
  const [tab, setTab] = useState<Tab>("chat");
  return <SessionBody key={sessionId} sessionId={sessionId} tab={tab} setTab={setTab} />;
}

function SessionBody({ sessionId, tab, setTab }: { sessionId: string; tab: Tab; setTab: (tab: Tab) => void }) {
  const sessions = useSessions(true);
  const home = useHomeSession();
  const memories = useMemoryIndex();
  const { events, status, error, delta, stream, reload } = useSessionEvents(sessionId);
  const tree = useSessionTree(sessionId);
  const models = useModels();
  const changesQuery = useSessionChanges(sessionId);
  const actions = useSessionActions();
  const panel = usePanel();
  const trace = useTraceUi();

  const summary = sessions.data?.find((s) => s.session === sessionId);
  const archived = !!summary && isArchived(summary);
  const view = useMemo(() => (events ? foldEvents(events) : undefined), [events]);
  const { pending, send, retry, stop } = useSend(sessionId, events, view?.runs ?? new Map());
  // 输入框的上下文环与右栏"上下文"是同一个数
  const context = useMemo(() => contextView(view, tree.data, models.data), [view, tree.data, models.data]);
  const kids = useMemo(() => (sessions.data ?? []).filter((s) => s.kind === "task" && s.home === sessionId && isLive(s)), [sessions.data, sessionId]);

  // 连接状态给侧栏看；标题给浏览器标签
  useEffect(() => {
    setStreamStatus(stream);
    return () => setStreamStatus("idle");
  }, [stream]);
  const title = sessionTitle(summary);
  useEffect(() => {
    document.title = `${summary ? title : m.app_name()} · ${m.app_name()}`;
  }, [summary, title]);

  // 每轮改动
  const changes = useMemo(() => new Map((changesQuery.data?.runs ?? []).map((r): [string, RunChanges] => [r.run, r])), [changesQuery.data]);
  const turnNumbers = useMemo(() => {
    const numbers = new Map<string, number>();
    let n = 0;
    for (const item of view?.items ?? []) if (item.kind === "turn") numbers.set(item.run, ++n);
    return numbers;
  }, [view]);
  const changeRuns = useMemo(
    () => [...changes.values()].sort((a, b) => (turnNumbers.get(b.run) ?? 0) - (turnNumbers.get(a.run) ?? 0)),
    [changes, turnNumbers],
  );
  const changedFiles = useMemo(() => new Set(changeRuns.flatMap((c) => c.files.map((f) => f.path))).size, [changeRuns]);
  const [openFiles, setOpenFiles] = useState<Record<string, boolean>>({});
  const [reviewAt, setReviewAt] = useState<string | null>(null);

  const inspect = useCallback(
    (resultEvent: string) => {
      setTab("trace");
      trace.setRange(null);
      trace.select(resultEvent, true);
      requestAnimationFrame(() => requestAnimationFrame(() => document.getElementById(`tr-${resultEvent}`)?.scrollIntoView({ block: "center" })));
    },
    [setTab, trace],
  );
  const review = useCallback(
    (run: string, index: number) => {
      setOpenFiles((previous) => ({ ...previous, [`${run}:${index}`]: true }));
      trace.select(null);
      setReviewAt(`cg-${run}-${index}`);
      setTab("changes");
    },
    [setTab, trace],
  );
  const conversation = useMemo(() => ({ memories, changes, inspect, review }), [memories, changes, inspect, review]);

  // 滚动：对话在底部时跟着长；切回对话落到底；改动页从顶开始
  const scroller = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);
  const opened = useRef(false);
  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el || tab !== "chat" || !view) return;
    if (!opened.current || atBottom.current) {
      el.scrollTop = el.scrollHeight;
      opened.current = true;
    }
  }, [view, delta, pending, tab]);
  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    if (tab === "chat") {
      el.scrollTop = el.scrollHeight;
      atBottom.current = true;
    } else if (tab === "changes") {
      requestAnimationFrame(() => requestAnimationFrame(() => (el.scrollTop = 0)));
    }
  }, [tab]);

  // ⇧⌘D：在改动页和对话之间来回（与右上角改动按钮同一个动作）
  const toggleChanges = useCallback(() => setTab(tab === "changes" ? "chat" : "changes"), [setTab, tab]);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.shiftKey && !event.altKey && !event.repeat && event.key.toLowerCase() === "d") {
        event.preventDefault();
        toggleChanges();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [toggleChanges]);

  const ready = status === "ready" && !archived;
  const busy = (view?.runs.size ?? 0) > 0;
  const body = (content: React.ReactNode) =>
    status === "loading" ? (
      <SkeletonThread />
    ) : status === "error" ? (
      <div className="mx-auto mt-12 max-w-130 text-center text-read leading-6">
        <p className="mb-1.5">{m.session_load_error({ message: errorText(error) })}</p>
        <button type="button" className="mt-2 cursor-pointer font-medium text-text underline underline-offset-3" onClick={reload}>
          {m.retry()}
        </button>
      </div>
    ) : (
      content
    );

  return (
    <ConversationProvider value={conversation}>
      <Tabs value={tab} onValueChange={(value) => setTab(value as Tab)} className="flex h-full min-h-0 flex-col overflow-hidden">
        <header className="flex h-12 shrink-0 items-center gap-2 pr-3 pl-5 max-[900px]:pl-2">
          <NavToggle />
          <SessionTitle session={summary} fallback={m.app_name()} />
          <TabsList aria-label={m.topbar_tabs()} className="ml-2 shrink-0 gap-0.5 max-[600px]:ml-0 max-[600px]:gap-0">
            <Trigger value="chat">{m.tab_chat()}</Trigger>
            <Trigger value="trace">{m.tab_trace()}</Trigger>
          </TabsList>
          <div className="ml-auto flex gap-1">
            <Tooltip
              label={
                <>
                  {m.tab_changes()}
                  {changedFiles > 0 && <span className="ml-1.5 text-white/60 tabular-nums">{changedFiles}</span>}
                </>
              }
              shortcut={CHANGES_SHORTCUT}
            >
              <IconButton
                name="diff"
                label={changedFiles > 0 ? `${m.tab_changes()} ${changedFiles}` : m.tab_changes()}
                title={undefined}
                aria-pressed={tab === "changes"}
                aria-keyshortcuts={isMac ? "Shift+Meta+D" : "Control+Shift+D"}
                onClick={toggleChanges}
                className={cn("max-[600px]:size-11", tab === "changes" && "bg-active text-text")}
              />
            </Tooltip>
            <IconButton name="branch" label={m.topbar_fork()} onClick={() => void actions.fork(sessionId)} className="max-[600px]:size-11" />
            <IconButton
              name="details"
              label={panel.open ? m.topbar_panel_close() : m.topbar_panel_open()}
              aria-expanded={panel.open}
              onClick={panel.toggle}
              className={cn("max-[600px]:size-11", panel.open && "bg-active text-text")}
            />
          </div>
        </header>
        <div className={cn("grid min-h-0 flex-1", panel.docked && panel.open ? "grid-cols-[minmax(0,1fr)_336px]" : "grid-cols-[minmax(0,1fr)]")}>
          <div className="flex min-h-0 min-w-0 flex-col">
            <div
              ref={scroller}
              onScroll={(event) => {
                const el = event.currentTarget;
                atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
              }}
              className="min-h-0 flex-1 overflow-auto px-8 pt-2 pb-6 max-[600px]:px-4 max-[600px]:pt-1 max-[600px]:pb-4"
            >
              <TabsContent value="chat" className="mx-auto max-w-180 outline-none">
                {body(
                  view && (
                    <Thread
                      view={view}
                      delta={delta}
                      pending={pending}
                      kids={kids}
                      isHome={sessionId === home.data?.session}
                      onRetry={retry}
                    />
                  ),
                )}
              </TabsContent>
              <TabsContent value="trace" className="outline-none">
                {body(events && view && <TraceView events={events} liveRuns={view.runs} tree={tree.data} memories={memories} ui={trace} />)}
              </TabsContent>
              <TabsContent value="changes" className="outline-none">
                {body(
                  <ChangesPage
                    runs={changeRuns}
                    turnNumbers={turnNumbers}
                    ui={{
                      open: openFiles,
                      toggle: (key, current) => setOpenFiles((previous) => ({ ...previous, [key]: !current })),
                      reviewAt,
                      clearReviewAt: () => setReviewAt(null),
                    }}
                  />,
                )}
              </TabsContent>
            </div>
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
          </div>
          <PanelShell panel={panel}>
            <PanelContent
              view={view}
              viewLoading={status === "loading"}
              context={context}
              treeStatus={tree.isError ? "error" : tree.data ? "ready" : "loading"}
              memories={memories}
              sessions={sessions.data ?? []}
              sessionId={sessionId}
            />
          </PanelShell>
        </div>
      </Tabs>
    </ConversationProvider>
  );
}

function Trigger({ value, children }: { value: Tab; children: React.ReactNode }) {
  return (
    <TabsTrigger
      value={value}
      className="h-7.5 rounded-control px-2.5 text-label text-muted hover:bg-hover-on-white hover:text-text data-[state=active]:bg-active data-[state=active]:font-medium data-[state=active]:text-text max-[600px]:px-1.5"
    >
      {children}
    </TabsTrigger>
  );
}

/** 加载：最终布局里放平的灰条，不做微光。 */
function SkeletonThread() {
  const bar = (width: string, extra = "") => <span className={`mb-3 block h-3.5 rounded-xs bg-active ${extra}`} style={{ width }} />;
  return (
    <div className="mx-auto max-w-180 pt-6">
      <div className="mb-5 flex justify-end">
        <span className="block h-10 rounded-[16px] bg-active" style={{ width: "46%" }} />
      </div>
      {bar("92%")}
      {bar("78%")}
      {bar("40%")}
      {bar("30%", "mt-4.5")}
      {bar("88%")}
      {bar("64%")}
    </div>
  );
}
