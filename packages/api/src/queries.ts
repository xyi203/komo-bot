import type {
  ChatListResponse,
  FileDiff,
  HealthResponse,
  ModelsResponse,
  PairRequest,
  RunChanges,
  SessionChanges,
  SessionListResponse,
  SessionSummary,
  WireEvent,
} from "@komo/protocol";
import { useInfiniteQuery, useQuery, useQueryClient, type InfiniteData, type QueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import {
  fetchEventPage,
  fetchHome,
  fetchInterventions,
  fetchModels,
  fetchRunChanges,
  fetchRunDiff,
  fetchSessionChanges,
  searchSessions,
  fetchTree,
} from "./actions";
import { get, post } from "./http";
import { keys } from "./keys";
import { subscribeSession, type StreamState } from "./sse";
import type { SessionTree } from "./types";

/** 对话列：主对话 + 支线，每行带最后一句话与是否在等你 / 在跑，另给等你的总数。 */
export const fetchChats = () => get<ChatListResponse>("/v1/chats");
export const fetchHealth = () => get<HealthResponse>("/healthz");
/** 全部会话，一次取完（不带 `limit`）。 */
export const fetchSessions = (all = false) =>
  get<SessionListResponse>("/v1/sessions", { all: all || undefined });

/** 会话列表一页的条数。 */
export const SESSION_PAGE = 50;

/** 会话列表的一页（最新创建的在前）；`cursor` 是上一页的 `next_cursor`。 */
export const fetchSessionPage = (all = false, cursor?: string, limit = SESSION_PAGE) =>
  get<SessionListResponse>("/v1/sessions", { all: all || undefined, limit, cursor });

/** 会话列表缓存的形状（按页）。 */
export type SessionPages = InfiniteData<SessionListResponse, string | undefined>;

/** 配对码换会话 cookie（`Set-Cookie` 由浏览器收下，响应体为空对象）。 */
export const redeemPairCode = (code: string) => post<Record<string, never>>("/v1/web/session", { code } satisfies PairRequest);

export function useHealth() {
  return useQuery({ queryKey: keys.health, queryFn: fetchHealth, retry: false });
}

/** 这个会话有没有没跑完的 Run（排队、在跑或停在等待上）。 */
export function sessionUnfinished(session: SessionSummary): boolean {
  const state = session.current_state;
  return state === "accepted" || state === "queued" || state === "running" || state === "waiting";
}

/** 列表里有会话在跑时每 4 秒重取一次：别的会话（比如派出去的任务）的进展不走当前会话的事件流。 */
const LIVE_POLL_MS = 4000;

/**
 * 会话列表，按页取（每页 {@link SESSION_PAGE} 条，最新创建的在前）；`data` 是已取到的各页拼起来的数组，
 * `hasNextPage` / `fetchNextPage` 接着取下一页。重取时从第一页按新的游标逐页重来，不会漏掉被挤下去的会话。
 */
export function useSessions(all = false) {
  return useInfiniteQuery({
    queryKey: keys.sessions.list(all),
    queryFn: ({ pageParam }) => fetchSessionPage(all, pageParam),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    select: (data) => data.pages.flatMap((page) => page.sessions),
    refetchInterval: (query) =>
      query.state.data?.pages.some((page) => page.sessions.some(sessionUnfinished)) ? LIVE_POLL_MS : false,
  });
}

/** 有对话在跑时的重取间隔：别的对话（比如定时触发、主对话派出的）的进展不走当前会话的事件流。 */
const CHATS_LIVE_POLL_MS = 4000;
const CHATS_IDLE_POLL_MS = 15_000;

/**
 * 对话列（`GET /v1/chats`）。当前打开的会话的事件流有变化时由 `useSessionEvents` 让它失效重取；
 * 别的对话的变化（在等你、在跑）靠轮询：有在跑的 4 秒一次，否则 15 秒。
 */
export function useChats() {
  return useQuery({
    queryKey: keys.chats,
    queryFn: fetchChats,
    refetchInterval: (query) => (query.state.data?.chats.some((chat) => chat.running) ? CHATS_LIVE_POLL_MS : CHATS_IDLE_POLL_MS),
  });
}

/** 默认 Agent 的主会话（没有就建）。 */
export function useHomeSession() {
  return useQuery({ queryKey: keys.home, queryFn: fetchHome, staleTime: Infinity });
}

/** 搜索弹窗的对话检索：关键词非空才查，结果按关键词缓存。 */
export function useSessionSearch(query: string, limit: number) {
  return useQuery({
    queryKey: keys.sessionSearch(query, limit),
    queryFn: ({ signal }) => searchSessions(query, limit, signal),
    select: (data) => data.hits,
    enabled: query !== "",
    retry: false,
    staleTime: 30_000,
  });
}

export function useModels() {
  return useQuery({ queryKey: keys.models, queryFn: fetchModels, select: (data: ModelsResponse) => data.models });
}

/** 还没答复的介入清单（停住的运行）：不随会话事件流来，半分钟取一次；对话页在最后一句投进来的话下面给答复按钮。 */
export function useInterventions() {
  return useQuery({ queryKey: keys.interventions, queryFn: fetchInterventions, refetchInterval: 30_000, retry: false });
}

export function useSessionTree(session: string | undefined) {
  return useQuery({
    queryKey: keys.session(session ?? "").tree,
    queryFn: () => fetchTree(session!),
    enabled: !!session,
    retry: false,
  });
}

/** 这个会话里 Gateway 还记得的各轮改动（旧 Gateway 没有这个接口：当作没有）。 */
export function useSessionChanges(session: string | undefined) {
  return useQuery({
    queryKey: keys.session(session ?? "").changes,
    queryFn: () => fetchSessionChanges(session!),
    enabled: !!session,
    retry: false,
  });
}

/** 一个文件的对比：点开才取，取到不再变（Gateway 重启过就取不到，错误由界面写出来）。 */
export function useRunDiff(run: string, index: number, enabled: boolean) {
  return useQuery<FileDiff>({
    queryKey: keys.runs.diff(run, index),
    queryFn: () => fetchRunDiff(run, index),
    enabled,
    staleTime: Infinity,
    retry: false,
  });
}

/**
 * 旧版的"打字机"提示：同一轮 `(run, round)` 的 `assistant_delta` 拼起来，正式回复一到就丢掉。
 */
export interface Delta {
  run: string;
  round: number;
  text: string;
}

const TERMINAL = /^run\.(completed|failed|cancelled|abandoned)$/;

/** 事件影响到侧栏和右栏的，按需重取那一块（合并成 400 ms 内一次）。 */
function sideRefresher(queryClient: QueryClient) {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const schedule = () => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      for (const key of [keys.sessions.all, keys.chats, keys.matters, keys.interventions, keys.memories]) {
        void queryClient.invalidateQueries({ queryKey: key });
      }
    }, 400);
  };
  return { schedule, stop: () => clearTimeout(timer) };
}

/** 一条 Run 进终态：Gateway 收尾后改动才齐，`GET /v1/runs/{id}/changes` 会等它。 */
async function mergeRunChanges(queryClient: QueryClient, session: string, run: string) {
  let changes: RunChanges;
  try {
    changes = await fetchRunChanges(run);
  } catch {
    return; // 没记过（旧 Gateway、重启过）：没有卡片
  }
  queryClient.setQueryData<SessionChanges>(keys.session(session).changes, (previous) => {
    const runs = (previous?.runs ?? []).filter((r) => r.run !== run);
    return { runs: [...runs, changes] };
  });
}

export type EventsStatus = "loading" | "ready" | "error";

/**
 * 一个会话的正文：先按页补读 `GET /v1/sessions/{id}/events`，再从 `next` 接 SSE 直播；
 * 到达的事件按 seq 去重后写进同一份 Query 缓存，界面从事件 fold 出正文（刷新即重建）。
 */
export function useSessionEvents(session: string) {
  const queryClient = useQueryClient();
  const key = keys.session(session).events;
  const query = useQuery({
    queryKey: key,
    queryFn: async ({ signal }) => {
      const events: WireEvent[] = [];
      let from = 0;
      for (;;) {
        const page = await fetchEventPage(session, from, signal);
        events.push(...page.events);
        from = page.next;
        if (!page.more) return { events, next: from };
      }
    },
    // 离开再回来要重新补读：缓存里的事件在不看它的时候没有人在追加。
    gcTime: 0,
    staleTime: Infinity,
    retry: false,
  });

  const [delta, setDelta] = useState<Delta | null>(null);
  const [stream, setStream] = useState<StreamState>("connecting");
  const ready = query.isSuccess;

  useEffect(() => {
    if (!ready) return;
    // key 在 effect 里按 session 重算：外面那份每次渲染都是新数组，放进依赖会让每次渲染都断开重连
    // （onState 又触发渲染，形成每秒几百次的重连循环）。
    const key = keys.session(session).events;
    const side = sideRefresher(queryClient);
    const next = queryClient.getQueryData<{ next: number }>(key)?.next ?? 0;
    const stop = subscribeSession(session, {
      from: next,
      onState: setStream,
      onFrame: (frame) => {
        if (frame.event === "event") {
          const event = frame.data;
          queryClient.setQueryData<{ events: WireEvent[]; next: number }>(key, (previous) => {
            if (!previous) return previous;
            const last = previous.events[previous.events.length - 1];
            if (last && event.seq <= last.seq) return previous;
            return { events: [...previous.events, event], next: event.seq };
          });
          if (event.type === "message.assistant" || TERMINAL.test(event.type)) setDelta(null);
          if (TERMINAL.test(event.type)) {
            void queryClient.invalidateQueries({ queryKey: keys.session(session).tree });
            if (event.run_id && event.type !== "run.abandoned") void mergeRunChanges(queryClient, session, event.run_id);
          }
          // 对话列的最后一句话与状态：Run 的进展、用户发的话、投进对话的话（`line.reply`）都会改它
          if (/^(run\.|memory\.|message\.user$|line\.reply$)/.test(event.type)) side.schedule();
        } else if (frame.event === "assistant_delta") {
          const piece = frame.data;
          setDelta((current) =>
            current && current.run === piece.run && current.round === piece.round
              ? { ...current, text: current.text + piece.text }
              : { ...piece },
          );
        }
      },
    });
    return () => {
      stop();
      side.stop();
    };
  }, [session, ready, queryClient]);

  // 换会话时上一个会话的打字机残留清掉
  const current = useRef(session);
  useEffect(() => {
    if (current.current !== session) {
      current.current = session;
      setDelta(null);
    }
  }, [session]);

  const status: EventsStatus = query.isError ? "error" : query.isSuccess ? "ready" : "loading";
  return {
    events: query.data?.events,
    status,
    error: query.error,
    delta,
    stream,
    reload: () => void query.refetch(),
  };
}
