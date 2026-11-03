import { useModels } from "@komo/api";
import { useRef, useState, type FormEvent, type KeyboardEvent } from "react";
import { Icon } from "@/components/icon";
import { Spinner } from "@/components/spinner";
import type { View } from "@/features/conversation/model";
import { pctOf, pctText, type ContextView } from "@/features/panel/context-usage";
import { formatElapsed, formatTokens } from "@/lib/format";
import { useNow } from "@/lib/hooks";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { effortLabel, modelLabel } from "./effort";
import type { SendOptions } from "./use-send";

// 模型与 effort 的选择在会话之间保留（原型里它们是页面上唯一的两个 <select>，换会话不重置），刷新后回到默认。
let chosen: { model: string | null; effort: string } = { model: null, effort: "" };

/**
 * 输入框（docs/web.md §5）：加号（附件，未接）、一行文字、发送；会话有 Run 在跑且输入为空时发送键变停止，输入了文字则是插话。
 * 框下：左边用量行（轮数、步数、tok、缓存命中；跑着时是计时），右边模型、effort 与上下文环。
 */
export function Composer({
  view,
  context,
  busy,
  disabled,
  archived,
  onSend,
  onStop,
}: {
  view: View | undefined;
  context: ContextView;
  busy: boolean;
  disabled: boolean;
  archived: boolean;
  onSend: (text: string, options: SendOptions) => void;
  onStop: () => void;
}) {
  const [text, setText] = useState("");
  const area = useRef<HTMLTextAreaElement>(null);
  const models = useModels().data;
  const [pick, setPick] = useState(chosen);
  const showStop = busy && !text.trim();

  const defaultModel = models?.find((x) => x.default) ?? models?.[0];
  const current = models?.find((x) => x.id === (pick.model ?? defaultModel?.id)) ?? defaultModel;
  const efforts = current?.efforts ?? [];
  const effort = efforts.includes(pick.effort) ? pick.effort : "";
  const choose = (next: typeof chosen) => {
    chosen = next;
    setPick(next);
  };

  const submit = (event?: FormEvent) => {
    event?.preventDefault();
    if (disabled || !text.trim()) return;
    onSend(text, {
      // 选的就是默认模型时不带 model，让 Gateway 用当前快照的主模型
      ...(current && current.id !== defaultModel?.id ? { model: current.id } : {}),
      ...(effort ? { effort } : {}),
    });
    setText("");
    if (area.current) area.current.style.height = "auto";
  };
  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
      event.preventDefault();
      submit();
    }
  };

  return (
    <div className="shrink-0 px-8 pb-3 max-[600px]:px-3 max-[600px]:pb-2">
      <form
        onSubmit={submit}
        className={cn(
          "mx-auto flex max-w-180 items-end gap-1 rounded-[16px] p-1.5 shadow-[0_0_0_1px_rgb(34_40_28/0.2),0_2px_8px_rgb(34_40_28/0.05)] transition-shadow focus-within:shadow-[0_0_0_1px_var(--color-muted),0_2px_8px_rgb(34_40_28/0.05)]",
          disabled ? "bg-raised" : "bg-surface",
        )}
      >
        {/* 附件还没有 Gateway 接口（SubmitRunRequest 只有 text）：先放加号占位，点了说明还不支持 */}
        <button
          type="button"
          aria-label={m.composer_attach()}
          title={m.composer_attach()}
          disabled={disabled}
          onClick={() => showToast(m.composer_attach_unsupported())}
          className="grid size-8 shrink-0 cursor-pointer place-items-center rounded-full text-text transition-colors hover:bg-hover-on-white disabled:cursor-not-allowed disabled:opacity-35 max-[600px]:m-1.5"
        >
          <Icon name="plus" />
        </button>
        <label className="sr-only" htmlFor="composer-input">
          {m.composer_label()}
        </label>
        <textarea
          id="composer-input"
          ref={area}
          rows={1}
          value={text}
          disabled={disabled}
          placeholder={archived ? m.composer_archived() : busy ? m.composer_interject() : m.composer_send_hint()}
          onChange={(event) => {
            setText(event.target.value);
            event.target.style.height = "auto";
            event.target.style.height = `${event.target.scrollHeight}px`;
          }}
          onKeyDown={onKeyDown}
          className="max-h-60 min-h-8 min-w-0 flex-1 resize-none bg-transparent px-1 py-1.5 text-read leading-5 text-text outline-none placeholder:text-faint max-[600px]:min-h-11 max-[600px]:py-3"
        />
        {showStop ? (
          <button
            type="button"
            aria-label={m.composer_stop()}
            title={m.composer_stop()}
            onClick={onStop}
            className="grid size-8 shrink-0 cursor-pointer place-items-center rounded-full bg-primary text-white transition-[transform,opacity] hover:opacity-90 active:scale-95 max-[600px]:m-1.5"
          >
            <Icon name="stop" solid />
          </button>
        ) : (
          <button
            type="submit"
            aria-label={m.composer_send()}
            title={m.composer_send()}
            disabled={disabled}
            className={cn(
              "grid size-8 shrink-0 cursor-pointer place-items-center rounded-full bg-primary text-white transition-[transform,opacity] hover:opacity-90 active:scale-95 max-[600px]:m-1.5",
              disabled && "cursor-not-allowed opacity-35",
            )}
          >
            <Icon name="up" />
          </button>
        )}
      </form>
      <div className="mx-auto mt-1 flex h-7 max-w-180 items-center gap-2 px-2 text-label text-muted max-[600px]:justify-end">
        <UsageLine view={view} busy={busy} />
        {models && models.length > 0 && (
          <>
            <select
              aria-label={m.composer_model()}
              value={current?.id ?? ""}
              onChange={(event) => choose({ model: event.target.value, effort: "" })}
              className={cn(pickClass, "ml-auto text-text max-[600px]:ml-0")}
            >
              {models.map((model) => (
                <option key={model.id} value={model.id}>
                  {modelLabel(model.name || model.id)}
                </option>
              ))}
            </select>
            <select
              aria-label={m.composer_effort()}
              value={effort}
              onChange={(event) => choose({ ...pick, effort: event.target.value })}
              className={cn(pickClass, "-ml-1")}
            >
              <option value="">{m.effort_default()}</option>
              {efforts.map((value) => (
                <option key={value} value={value}>
                  {effortLabel(value)}
                </option>
              ))}
            </select>
          </>
        )}
        <ContextRing context={context} className={cn(!(models && models.length > 0) && "ml-auto")} />
      </div>
    </div>
  );
}

const pickClass =
  "h-6 min-w-0 max-w-40 cursor-pointer truncate rounded-sm border-0 bg-transparent px-1 text-label hover:bg-hover-on-white max-[600px]:max-w-32";

/** 左边的用量行：跑着时是转圈加计时；否则"N 轮 M 步 · 4.4M tok · 缓存命中 95%"（从 `message.assistant` 的用量累加）。 */
function UsageLine({ view, busy }: { view: View | undefined; busy: boolean }) {
  const turns = view?.turns.size ?? 0;
  const running = busy ? [...(view?.runs.values() ?? [])][0] : undefined;
  const now = useNow(!!running);
  if (!view || !turns) return <span className="max-[600px]:hidden" />;
  const { stats } = view;
  const hit = stats.input ? Math.round((stats.cache / stats.input) * 100) : null;
  return (
    <span className="inline-flex min-w-0 items-center gap-1 overflow-hidden whitespace-nowrap tabular-nums max-[600px]:hidden">
      {running ? (
        <>
          <Spinner small />
          {m.composer_running({ time: formatElapsed(running.since, now) })}
        </>
      ) : (
        <>
          {m.composer_turns({ count: turns })} {m.composer_steps({ count: stats.steps })}
          <Sep />
          {m.composer_tokens({ tokens: formatTokens(stats.input + stats.output) })}
          {hit != null && (
            <>
              <Sep />
              {m.composer_cache({ pct: hit })}
            </>
          )}
        </>
      )}
    </span>
  );
}

const Sep = () => <span className="mx-1.5 text-faint">·</span>;

/** 上下文环：上一次请求的 input_tokens ÷ 模型窗口，与右栏"上下文"同一个数。 */
function ContextRing({ context, className }: { context: ContextView; className?: string }) {
  const { total, win, measured } = context;
  const used = measured ? total : 0;
  const pct = win && used ? Math.min(100, pctOf(used, win)) : 0;
  const r = 6;
  const len = 2 * Math.PI * r;
  const label = used ? m.composer_ring({ pct: win ? pctText(used, win) : "—", used: formatTokens(used), win: win ? formatTokens(win) : "?" }) : m.composer_ring_none();
  return (
    <span role="img" aria-label={label} title={label} className={cn("inline-flex items-center gap-1.25 tabular-nums", className)}>
      <svg viewBox="0 0 16 16" aria-hidden="true" className="size-4 -rotate-90 fill-none [&_circle]:stroke-[2.2]">
        <circle cx="8" cy="8" r={r} className="stroke-active" />
        <circle cx="8" cy="8" r={r} strokeLinecap="round" className="stroke-accent" strokeDasharray={`${(len * pct) / 100} ${len}`} />
      </svg>
      <span>{used ? (win ? pctText(used, win) : "—") : "—"}</span>
    </span>
  );
}
