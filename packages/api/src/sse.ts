import type { SseFrame } from "@komo/protocol";

export type StreamState = "connecting" | "open" | "reconnecting";

export interface SubscribeOptions {
  /** 从哪个 seq 之后读起；不给从头。 */
  from?: number;
  onFrame: (frame: SseFrame) => void;
  onState?: (state: StreamState) => void;
}

/** `event:` 行的名字，就是 `SseEvent` 的 serde 标签（心跳是注释行，不会到这里）。 */
const FRAME_EVENTS = ["event", "assistant_delta", "run_status", "nested_calls"] as const;
const RETRY_MIN_MS = 1000;
const RETRY_MAX_MS = 15000;

/**
 * 订阅一个会话的 SSE（`GET /v1/sessions/{id}/events`），返回取消函数。
 *
 * 游标是**原始事件**的 seq（`event` 帧的 `id`）：断开后自己重连并带上 `?from=<游标>`，让 Gateway 补齐
 * 缺的事件（补读与直播按 seq 去重）。不用 `EventSource` 自带的重连——那一份只记最后一个 `id:`，而
 * `run_status` / `assistant_delta` 这类派生帧与源事件同号，拿它当游标可能跳过还没送到的源事件。
 * 所以 `error` 一来就关掉浏览器的自动重连，按退避时间自己重开。
 */
export function subscribeSession(session: string, options: SubscribeOptions): () => void {
  let cursor = options.from ?? 0;
  let source: EventSource | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let delay = RETRY_MIN_MS;
  let stopped = false;
  let everOpened = false;

  const open = () => {
    options.onState?.(everOpened ? "reconnecting" : "connecting");
    const url = `/v1/sessions/${encodeURIComponent(session)}/events?from=${cursor}`;
    const es = new EventSource(url, { withCredentials: true });
    source = es;
    es.onopen = () => {
      everOpened = true;
      delay = RETRY_MIN_MS;
      options.onState?.("open");
    };
    for (const name of FRAME_EVENTS) {
      es.addEventListener(name, (message) => {
        let frame: SseFrame;
        try {
          frame = JSON.parse((message as MessageEvent<string>).data) as SseFrame;
        } catch {
          return; // 读不懂的帧跳过，不让一帧坏数据停掉订阅
        }
        if (frame.event === "event") cursor = Math.max(cursor, frame.id);
        options.onFrame(frame);
      });
    }
    es.onerror = () => {
      es.close();
      if (stopped) return;
      options.onState?.("reconnecting");
      timer = setTimeout(open, delay);
      delay = Math.min(delay * 2, RETRY_MAX_MS);
    };
  };

  open();
  return () => {
    stopped = true;
    if (timer) clearTimeout(timer);
    source?.close();
  };
}
