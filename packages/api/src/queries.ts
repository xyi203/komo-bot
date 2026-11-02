import type { HealthResponse, PairRequest, SessionListResponse, SseFrame } from "@komo/protocol";
import { useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { get, post } from "./http";
import { keys } from "./keys";
import { subscribeSession, type StreamState } from "./sse";

export const fetchHealth = () => get<HealthResponse>("/healthz");
export const fetchSessions = (all = false) =>
  get<SessionListResponse>("/v1/sessions", { all: all || undefined });

/** 配对码换会话 cookie（`Set-Cookie` 由浏览器收下，响应体为空对象）。 */
export const redeemPairCode = (code: string) => post<Record<string, never>>("/v1/web/session", { code } satisfies PairRequest);

export function useHealth() {
  return useQuery({ queryKey: keys.health, queryFn: fetchHealth, retry: false });
}

export function useSessions(all = false) {
  return useQuery({
    queryKey: keys.sessions.list(all),
    queryFn: () => fetchSessions(all),
    select: (data) => data.sessions,
  });
}

/**
 * SSE 帧到达时让对应的 key 失效：原始事件和 Run 状态变化都可能改标题、状态、最近更新时间，
 * 所以会话列表重新取；打字机帧（`assistant_delta`）只是界面提示，不碰缓存。
 */
export function invalidateOnFrame(queryClient: QueryClient, frame: SseFrame): void {
  if (frame.event === "event" || frame.event === "run_status") {
    void queryClient.invalidateQueries({ queryKey: keys.sessions.all });
  }
}

const MAX_FRAMES = 500;

/** 订阅一个会话的帧流：断线按游标续上，到达的帧累积在 `frames`（最多 500 帧），并让缓存失效。 */
export function useSessionStream(session: string | undefined) {
  const queryClient = useQueryClient();
  const [frames, setFrames] = useState<SseFrame[]>([]);
  const [state, setState] = useState<StreamState>("connecting");
  const client = useRef(queryClient);
  client.current = queryClient;

  useEffect(() => {
    setFrames([]);
    if (!session) return;
    return subscribeSession(session, {
      onState: setState,
      onFrame: (frame) => {
        setFrames((previous) => [...previous.slice(-(MAX_FRAMES - 1)), frame]);
        invalidateOnFrame(client.current, frame);
      },
    });
  }, [session]);

  return { frames, state };
}
