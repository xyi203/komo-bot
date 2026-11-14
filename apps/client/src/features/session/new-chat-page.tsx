import { createSession, keys, submitRun, useModels, uuid } from "@komo/api";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useMemo, useRef, useState } from "react";
import { Composer } from "@/features/composer/composer";
import { contextView } from "@/features/composer/context-usage";
import type { SendOptions } from "@/features/composer/use-send";
import { foldEvents } from "@/features/conversation/model";
import { Thread, type PendingSend } from "@/features/conversation/thread";
import { errorText } from "@/lib/errors";
import { m } from "@/paraglide/messages.js";
import { ChatLayout } from "./chat-layout";

const EMPTY = foldEvents([]);
const NO_RUNS: never[] = [];
// 输入框的草稿与附件按"会话"存；新支线还没有会话，用一个固定的键（真会话 id 不会是这个）
const DRAFT_KEY = "new";

interface Job {
  key: string;
  body: string;
  options: SendOptions;
}

/**
 * 新支线对话（`/chats/new`）：一个空的对话页，还没有会话。发第一句话时才 `POST /v1/sessions` 建会话、再提交这句话，
 * 然后换到 `/chats/$id`；标题由服务端从第一句话取。建会话或提交失败时留在这里，气泡标红给重试（同一请求键，不会建出两个会话）。
 */
export function NewChatPage() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const models = useModels();
  const [pending, setPending] = useState<PendingSend[]>([]);
  const jobs = useRef(new Map<string, Job>());
  const context = useMemo(() => contextView(undefined, undefined, models.data), [models.data]);
  useEffect(() => {
    document.title = `${m.nav_new_session()} · ${m.app_name()}`;
  }, []);

  const run = async (id: string) => {
    const job = jobs.current.get(id);
    if (!job) return;
    const patch = (change: Partial<PendingSend>) => setPending((all) => all.map((p) => (p.id === id ? { ...p, ...change } : p)));
    patch({ state: "sending", error: undefined });
    try {
      const created = await createSession({ request_key: `web:new:${job.key}` });
      await submitRun(created.session, {
        request_key: `web:${created.session}:${job.key}`,
        text: job.body,
        ...(job.options.model ? { model: job.options.model } : {}),
        ...(job.options.effort ? { effort: job.options.effort } : {}),
      });
      void queryClient.invalidateQueries({ queryKey: keys.chats });
      void queryClient.invalidateQueries({ queryKey: keys.sessions.all });
      void navigate({ to: "/chats/$id", params: { id: created.session }, replace: true });
    } catch (error) {
      patch({ state: "failed", error: errorText(error) });
    }
  };
  const send = (text: string, options: SendOptions) => {
    if (!text.trim()) return;
    const id = uuid();
    jobs.current.clear();
    jobs.current.set(id, { key: id, body: text, options });
    setPending([{ id, text, state: "sending" }]);
    void run(id);
  };

  return (
    <ChatLayout
      name={m.nav_new_session()}
      composer={
        <Composer
          sessionId={DRAFT_KEY}
          view={undefined}
          context={context}
          busy={false}
          disabled={pending.some((p) => p.state === "sending")}
          archived={false}
          onSend={send}
          onStop={() => {}}
        />
      }
    >
      <Thread view={EMPTY} delta={null} pending={pending} kids={NO_RUNS} isHome={false} onRetry={(id) => void run(id)} />
    </ChatLayout>
  );
}
