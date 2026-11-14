import { answerIntervention, keys, uuid } from "@komo/api";
import type { InterventionSummary, InterventionVerdict } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Markdown } from "@/components/markdown";
import { errorText } from "@/lib/errors";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { useConversation } from "./context";
import { ReplyTools } from "./tools";

/**
 * 投进对话的一句话（`line.reply`，docs/redesign.md §2.1）：一条普通的 Komo 消息，按时间顺序排在轮之间。
 * 停住的运行投进来的提问带介入句柄：那条介入还没答复（还在 `GET /v1/interventions` 里）时，下面给答复按钮。
 */
export function ReplyView({ text, always, intervention }: { text: string; always: boolean; intervention?: string }) {
  const { interventions } = useConversation();
  const open = intervention ? interventions.filter((item) => item.handle === intervention) : [];
  return (
    <section className="group/msg mb-5">
      <Markdown text={text} className="mt-4" />
      {/* 投进来的话没有"过程"，也不挂记忆：只有复制 */}
      <ReplyTools text={text} always={always} />
      {open.length > 0 && <AnswerButtons interventions={open} />}
    </section>
  );
}

const LABEL: Record<InterventionVerdict, () => string> = {
  approve: () => m.verdict_approve(),
  reject: () => m.verdict_reject(),
  abandon: () => m.verdict_abandon(),
  resolve: () => m.verdict_resolve(),
  satisfied: () => m.verdict_satisfied(),
  not_performed: () => m.verdict_not_performed(),
};

/**
 * 停住的运行的答复：按 Gateway 给的 `verdicts` 一排小按钮（它此刻允许答什么就给什么，界面不自己推），点了
 * `POST /v1/interventions/{handle}/answer`。已答复的下一次取清单就没了，按钮随之消失。
 * 事件里的那句话不带介入的句柄（`line.reply` 只有 text / origin / task），所以按钮挂在最后一句投进来的话下面；
 * 同时有几条在等时每条前面写出它的问题，免得点错。
 */
function AnswerButtons({ interventions }: { interventions: readonly InterventionSummary[] }) {
  return (
    <div className="mt-3 flex flex-col gap-3">
      {interventions.map((item) => (
        <AnswerRow key={item.handle} item={item} named={interventions.length > 1} />
      ))}
    </div>
  );
}

function AnswerRow({ item, named }: { item: InterventionSummary; named: boolean }) {
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [sure, setSure] = useState(false);
  const answer = async (verdict: InterventionVerdict) => {
    // "放弃"不可撤销（这条 Run 取消后不再继续）：点一次先变成确认
    if (verdict === "abandon" && !sure) {
      setSure(true);
      return;
    }
    setBusy(true);
    try {
      const done = await answerIntervention(item.handle, { verdict, request_key: `web:intervention:${item.handle}:${uuid()}` });
      if (done.note) showToast(done.note);
    } catch (error) {
      showToast(m.verdict_error({ message: errorText(error) }));
    } finally {
      setBusy(false);
      setSure(false);
      void queryClient.invalidateQueries({ queryKey: keys.interventions });
      void queryClient.invalidateQueries({ queryKey: keys.chats });
    }
  };
  return (
    <div>
      {named && <p className="mb-1.5 text-label text-muted">{item.question}</p>}
      <div className="flex flex-wrap gap-2">
        {item.verdicts.map((verdict, i) => (
          <button
            key={verdict}
            type="button"
            disabled={busy}
            onClick={() => void answer(verdict)}
            className={cn(
              "h-7 cursor-pointer rounded-full px-3 text-label transition-colors disabled:cursor-not-allowed disabled:opacity-50",
              i === 0 ? "bg-primary text-white hover:bg-primary-hover" : "shadow-ring hover:bg-hover-on-white",
              verdict === "abandon" && sure && "text-negative shadow-[0_0_0_1px_var(--color-negative-ring)]",
            )}
          >
            {verdict === "abandon" && sure ? m.verdict_abandon_sure() : LABEL[verdict]()}
          </button>
        ))}
      </div>
    </div>
  );
}
