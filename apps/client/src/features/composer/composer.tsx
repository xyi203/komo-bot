import { useModels } from "@komo/api";
import { useRef, useState, type ClipboardEvent, type DragEvent, type FormEvent, type KeyboardEvent } from "react";
import { Icon } from "@/components/icon";
import { ImageZoom } from "@/components/image-zoom";
import { Spinner } from "@/components/spinner";
import type { View } from "@/features/conversation/model";
import { pctOf, pctText, type ContextView } from "@/features/panel/context-usage";
import { formatElapsed, formatTokens } from "@/lib/format";
import { useNow } from "@/lib/hooks";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { effortLabel, modelLabel } from "./effort";
import { useAttachments, type Draft } from "./use-attachments";
import type { SendOptions } from "./use-send";

// 模型与 effort 的选择在会话之间保留（原型里它们是页面上唯一的两个 <select>，换会话不重置），刷新后回到默认。
let chosen: { model: string | null; effort: string } = { model: null, effort: "" };

/**
 * 输入框（docs/web.md §5）：加号（选图片或文件）、一行文字、发送；会话有 Run 在跑且输入为空时发送键变停止，输入了文字则是插话。
 * 图片与文件也可以直接粘贴或拖进来：先传给 Gateway，发送时把每个附件的那行说明拼在文字前面（附件不进消息结构）。
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
  const picker = useRef<HTMLInputElement>(null);
  const { drafts, add, remove, clear } = useAttachments();
  const models = useModels().data;
  const [pick, setPick] = useState(chosen);
  const showStop = busy && !text.trim() && drafts.length === 0;

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
    if (disabled || (!text.trim() && drafts.length === 0)) return;
    if (drafts.some((d) => d.state === "uploading")) return showToast(m.composer_attach_wait());
    if (drafts.some((d) => d.state === "failed")) return showToast(m.composer_attach_blocked());
    // 说明在前、用户的话在后（与聊天渠道一样：附言是 `/pending` 也不会被当成命令）
    const body = [...drafts.map((d) => d.note ?? ""), text.trim() ? text : ""].filter(Boolean).join("\n");
    onSend(body, {
      // 选的就是默认模型时不带 model，让 Gateway 用当前快照的主模型
      ...(current && current.id !== defaultModel?.id ? { model: current.id } : {}),
      ...(effort ? { effort } : {}),
    });
    setText("");
    clear();
    if (area.current) area.current.style.height = "auto";
  };
  // 剪贴板里有文件（截图、从访达复制的文件）就当附件收下；只有文字照常粘贴
  const onPaste = (event: ClipboardEvent<HTMLTextAreaElement>) => {
    const files = [...event.clipboardData.files];
    if (!files.length || disabled) return;
    event.preventDefault();
    add(files);
  };
  const onDrop = (event: DragEvent<HTMLFormElement>) => {
    const files = [...event.dataTransfer.files];
    if (!files.length) return;
    event.preventDefault();
    if (!disabled) add(files);
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
        onDragOver={(event) => event.dataTransfer.types.includes("Files") && event.preventDefault()}
        onDrop={onDrop}
        className={cn(
          "mx-auto max-w-180 rounded-[16px] p-1.5 shadow-[0_0_0_1px_rgb(34_40_28/0.2),0_2px_8px_rgb(34_40_28/0.05)] transition-shadow focus-within:shadow-[0_0_0_1px_var(--color-muted),0_2px_8px_rgb(34_40_28/0.05)]",
          disabled ? "bg-raised" : "bg-surface",
        )}
      >
        {drafts.length > 0 && <DraftList drafts={drafts} onRemove={remove} />}
        <div className="flex items-end gap-1">
          <input
            ref={picker}
            type="file"
            multiple
            hidden
            onChange={(event) => {
              add([...(event.target.files ?? [])]);
              event.target.value = "";
            }}
          />
          <button
            type="button"
            aria-label={m.composer_attach()}
            title={m.composer_attach()}
            disabled={disabled}
            onClick={() => picker.current?.click()}
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
            onPaste={onPaste}
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
        </div>
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

/** 框里待发的附件：图片是方形缩略图（点开看大图），其他文件是带文件名的标签；传着的半透明，传失败的标红（悬停看原因）。 */
function DraftList({ drafts, onRemove }: { drafts: readonly Draft[]; onRemove: (id: string) => void }) {
  return (
    <ul className="flex flex-wrap gap-2 px-1.5 pt-1.5 pb-2">
      {drafts.map((d) => {
        const hint =
          d.state === "failed" ? m.composer_attach_failed({ name: d.name, message: d.error ?? "" }) : d.state === "uploading" ? m.composer_attach_uploading({ name: d.name }) : d.name;
        return (
          <li
            key={d.id}
            title={hint}
            className={cn(
              "group/draft relative rounded-inner bg-raised",
              d.state === "uploading" && "opacity-60",
              d.state === "failed" && "outline-2 outline-negative -outline-offset-2",
            )}
          >
            {d.preview ? (
              <ImageZoom src={d.preview} name={d.name} className="rounded-inner">
                <img src={d.preview} alt={d.name} className="size-16 rounded-inner object-cover" />
              </ImageZoom>
            ) : (
              <span className="flex h-16 max-w-48 items-center gap-1.5 px-3 text-label text-text">
                <Icon name="file" className="size-3.5 shrink-0 text-muted" />
                <span className="truncate">{d.name}</span>
              </span>
            )}
            {d.state === "uploading" && (
              <span className="pointer-events-none absolute inset-0 grid place-items-center">
                <Spinner small />
              </span>
            )}
            <button
              type="button"
              aria-label={m.composer_attach_remove({ name: d.name })}
              onClick={() => onRemove(d.id)}
              className="absolute -top-1.5 -right-1.5 grid size-5 cursor-pointer place-items-center rounded-full bg-primary text-white shadow-ring [@media(hover:hover)]:opacity-0 [@media(hover:hover)]:transition-opacity group-hover/draft:opacity-100 focus-visible:opacity-100"
            >
              <Icon name="x" className="size-3" />
            </button>
          </li>
        );
      })}
    </ul>
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
