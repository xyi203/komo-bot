import { cancelRun, submitRun, uuid } from "@komo/api";
import type { SubmitRunRequest, WireEvent } from "@komo/protocol";
import { useCallback, useEffect, useRef, useState } from "react";
import { errorText } from "@/lib/errors";
import type { PendingSend } from "@/features/conversation/thread";

const countUser = (events: readonly WireEvent[] | undefined, text: string) =>
  (events ?? []).filter((e) => e.type === "message.user" && (e.data as { text?: string } | undefined)?.text === text).length;

/** 这次发送选的模型与 effort（不给就用 Gateway 的默认）。 */
export interface SendOptions {
  model?: string;
  effort?: string;
}

/**
 * 发送、重试与停止。发送先显示一个淡色的待发气泡（`pending`），事件流里那句话出现后它就撤掉；失败标红并给重试，
 * 同一请求键重发（Gateway 按键去重，重试沿用第一次的模型与 effort）。有 Run 在跑时发送是插话（`interject`）。
 */
export function useSend(sessionId: string, events: readonly WireEvent[] | undefined, runs: ReadonlyMap<string, unknown>) {
  const [pending, setPending] = useState<(PendingSend & { baseline: number })[]>([]);
  const bodies = useRef(new Map<string, SubmitRunRequest>());
  const latestEvents = useRef(events);
  latestEvents.current = events;

  // 话在事件流里出现了（比发送时多了一条同样的）：待发气泡撤掉
  useEffect(() => {
    setPending((previous) => {
      const next = previous.filter((p) => countUser(events, p.text) <= p.baseline);
      return next.length === previous.length ? previous : next;
    });
  }, [events]);

  const submit = useCallback(
    async (id: string, body: SubmitRunRequest) => {
      const patch = (change: Partial<PendingSend>) => setPending((all) => all.map((p) => (p.id === id ? { ...p, ...change } : p)));
      try {
        await submitRun(sessionId, body);
        patch({ state: "sent" });
      } catch (error) {
        patch({ state: "failed", error: errorText(error) });
      }
    },
    [sessionId],
  );

  const busy = runs.size > 0;
  const send = useCallback(
    (text: string, options: SendOptions = {}) => {
      if (!text.trim()) return;
      const id = uuid();
      const body: SubmitRunRequest = {
        request_key: `web:${sessionId}:${id}`,
        text,
        ...(options.model ? { model: options.model } : {}),
        ...(options.effort ? { effort: options.effort } : {}),
        ...(busy ? { interject: true } : {}),
      };
      bodies.current.set(id, body);
      setPending((all) => [...all, { id, text, state: "sending", baseline: countUser(latestEvents.current, text) }]);
      void submit(id, body);
    },
    [busy, sessionId, submit],
  );

  const retry = useCallback(
    (id: string) => {
      const body = bodies.current.get(id);
      if (!body) return;
      setPending((all) => all.map((p) => (p.id === id ? { ...p, state: "sending" } : p)));
      void submit(id, body);
    },
    [submit],
  );

  const stop = useCallback(() => {
    void Promise.allSettled([...runs.keys()].map((run) => cancelRun(run)));
  }, [runs]);

  return { pending, send, retry, stop };
}
