import { useModels } from "@komo/api";
import { useLayoutEffect, useRef, useState, type ClipboardEvent, type DragEvent, type FormEvent, type KeyboardEvent } from "react";
import { Icon } from "@/components/icon";
import { ImageZoom } from "@/components/image-zoom";
import { Spinner } from "@/components/spinner";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { pctOf, pctText, type ContextView } from "./context-usage";
import { formatTokens } from "@/lib/format";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { useDraftText } from "./draft";
import { effortLabel, modelLabel } from "./effort";
import { useAttachments, type Draft } from "./use-attachments";
import type { SendOptions } from "./use-send";

// 模型与 effort 的选择在会话之间保留（原型里它们是页面上唯一的两个 <select>，换会话不重置），刷新后回到默认。
let chosen: { model: string | null; effort: string } = { model: null, effort: "" };

/** 上下文占用到这个比例（%）就在输入框旁出小提示。 */
const CONTEXT_WARN = 80;

/**
 * 输入框（docs/web.md §5）：加号、一行文字、发送，别的都收进加号菜单（docs/redesign.md P15）。
 * 支线对话的加号是一个菜单：添加附件、模型、effort、上下文占用；主对话的加号就是添加附件（只有这一项，不开菜单）。
 * 上下文占用到 80% 时输入框旁出一个小提示。图片与文件也可以直接粘贴或拖进来：先传给 Gateway，发送时把每个附件的那行说明拼在文字前面。
 * 有 Run 在跑时发送就是插话；停止在对话末尾的"正在输入"旁。没发出去的文字和附件按会话留着（`draft.ts`、`use-attachments.ts`）。
 */
export function Composer({
  sessionId,
  main,
  context,
  disabled,
  archived,
  onSend,
}: {
  sessionId: string;
  /** 主对话：加号只有添加附件，模型不在这里选（去设置改默认模型）。 */
  main: boolean;
  context: ContextView;
  disabled: boolean;
  archived: boolean;
  onSend: (text: string, options: SendOptions) => void;
}) {
  const [text, setText] = useDraftText(sessionId);
  const area = useRef<HTMLTextAreaElement>(null);
  const picker = useRef<HTMLInputElement>(null);
  const { drafts, add, remove, clear } = useAttachments(sessionId);
  // 带着草稿挂上来时按内容撑开高度（平时在 onChange 里撑）
  useLayoutEffect(() => {
    const el = area.current;
    if (!el || !el.value) return;
    el.style.height = "auto";
    el.style.height = `${el.scrollHeight}px`;
  }, []);
  const models = useModels().data;
  const [pick, setPick] = useState(chosen);

  const defaultModel = models?.find((x) => x.default) ?? models?.[0];
  const current = models?.find((x) => x.id === (pick.model ?? defaultModel?.id)) ?? defaultModel;
  const efforts = current?.efforts ?? [];
  const effort = efforts.includes(pick.effort) ? pick.effort : "";
  const warn = context.measured && context.win > 0 && pctOf(context.total, context.win) >= CONTEXT_WARN;
  const choose = (next: typeof chosen) => {
    chosen = next;
    setPick(next);
  };

  const submit = (event?: FormEvent) => {
    event?.preventDefault();
    if (disabled || (!text.trim() && drafts.length === 0)) return;
    if (drafts.some((d) => d.state === "uploading")) return showToast(m.composer_attach_wait());
    if (drafts.some((d) => d.state === "failed")) return showToast(m.composer_attach_blocked());
    // 说明在前、用户的话在后
    const body = [...drafts.map((d) => d.note ?? ""), text.trim() ? text : ""].filter(Boolean).join("\n");
    onSend(body, main ? {} : {
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
    <div className="shrink-0 px-8 pb-3 max-[600px]:px-3 narrow:pb-[max(0.5rem,env(safe-area-inset-bottom))]">
      <form
        onSubmit={submit}
        onDragOver={(event) => event.dataTransfer.types.includes("Files") && event.preventDefault()}
        onDrop={onDrop}
        className={cn(
          "mx-auto max-w-180 rounded-[16px] p-1.5 shadow-[0_0_0_1px_rgb(0_0_0/0.2),0_2px_8px_rgb(0_0_0/0.05)] transition-shadow focus-within:shadow-[0_0_0_1px_var(--color-muted),0_2px_8px_rgb(0_0_0/0.05)]",
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
          {main ? (
            <button
              type="button"
              aria-label={m.composer_attach()}
              title={m.composer_attach()}
              disabled={disabled}
              onClick={() => picker.current?.click()}
              className={plusClass}
            >
              <Icon name="plus" />
            </button>
          ) : (
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <button type="button" aria-label={m.composer_menu()} title={m.composer_menu()} disabled={disabled} className={plusClass}>
                  <Icon name="plus" />
                </button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="start" side="top" sideOffset={8}>
                <DropdownMenuItem onSelect={() => picker.current?.click()}>
                  <Icon name="file" className="size-4 text-muted" />
                  {m.composer_attach()}
                </DropdownMenuItem>
                {models && models.length > 0 && (
                  <>
                    <DropdownMenuSeparator />
                    <DropdownMenuLabel>{m.composer_model()}</DropdownMenuLabel>
                    <DropdownMenuRadioGroup value={current?.id ?? ""} onValueChange={(id) => choose({ model: id, effort: "" })}>
                      {models.map((model) => (
                        <DropdownMenuRadioItem key={model.id} value={model.id}>
                          <span className="truncate">{modelLabel(model.name || model.id)}</span>
                        </DropdownMenuRadioItem>
                      ))}
                    </DropdownMenuRadioGroup>
                    {efforts.length > 0 && (
                      <>
                        <DropdownMenuLabel>{m.composer_effort()}</DropdownMenuLabel>
                        <DropdownMenuRadioGroup value={effort} onValueChange={(value) => choose({ ...pick, effort: value })}>
                          <DropdownMenuRadioItem value="">{m.effort_default()}</DropdownMenuRadioItem>
                          {efforts.map((value) => (
                            <DropdownMenuRadioItem key={value} value={value}>
                              {effortLabel(value)}
                            </DropdownMenuRadioItem>
                          ))}
                        </DropdownMenuRadioGroup>
                      </>
                    )}
                  </>
                )}
                <DropdownMenuSeparator />
                <div className="flex items-center gap-2 px-2.5 py-1.5 text-label text-muted">
                  <ContextRing context={context} />
                  <span className="min-w-0 truncate tabular-nums">{ringLabel(context)}</span>
                </div>
              </DropdownMenuContent>
            </DropdownMenu>
          )}
          <label className="sr-only" htmlFor="composer-input">
            {m.composer_label()}
          </label>
          <textarea
            id="composer-input"
            ref={area}
            rows={1}
            value={text}
            disabled={disabled}
            placeholder={archived ? m.composer_archived() : undefined}
            onChange={(event) => {
              setText(event.target.value);
              event.target.style.height = "auto";
              event.target.style.height = `${event.target.scrollHeight}px`;
            }}
            onKeyDown={onKeyDown}
            onPaste={onPaste}
            className="max-h-60 min-h-8 min-w-0 flex-1 resize-none bg-transparent px-1 py-1.5 text-read leading-5 text-text outline-none placeholder:text-faint max-[600px]:min-h-11 max-[600px]:py-3 narrow:text-[16px]"
          />
          {warn && (
            <span role="img" aria-label={ringLabel(context)} title={ringLabel(context)} className="flex h-8 shrink-0 items-center gap-1.5 text-small text-accent tabular-nums">
              <ContextRing context={context} />
              {m.composer_ctx_hint({ pct: pctText(context.total, context.win) })}
            </span>
          )}
          <button
            type="submit"
            aria-label={m.composer_send()}
            title={m.composer_send()}
            disabled={disabled}
            className={cn(
              "relative grid size-8 shrink-0 cursor-pointer place-items-center rounded-full bg-primary text-white transition-[transform,opacity] hover:opacity-90 active:scale-95 max-[600px]:m-1.5 narrow:after:absolute narrow:after:-inset-1.5",
              disabled && "cursor-not-allowed opacity-35",
            )}
          >
            <Icon name="up" />
          </button>
        </div>
      </form>
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

const plusClass =
  "relative grid size-8 shrink-0 cursor-pointer place-items-center rounded-full text-text transition-colors hover:bg-hover-on-white disabled:cursor-not-allowed disabled:opacity-35 max-[600px]:m-1.5 narrow:after:absolute narrow:after:-inset-1.5";

/** 上下文占用的一句话：`上下文 12%（24k / 200k）`；还没请求过模型时写明。 */
function ringLabel({ total, win, measured }: ContextView): string {
  const used = measured ? total : 0;
  return used ? m.composer_ring({ pct: win ? pctText(used, win) : "—", used: formatTokens(used), win: win ? formatTokens(win) : "?" }) : m.composer_ring_none();
}

/** 上下文环：上一次请求的 input_tokens ÷ 模型窗口。 */
function ContextRing({ context }: { context: ContextView }) {
  const { total, win, measured } = context;
  const used = measured ? total : 0;
  const pct = win && used ? Math.min(100, pctOf(used, win)) : 0;
  const r = 6;
  const len = 2 * Math.PI * r;
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" className="size-4 shrink-0 -rotate-90 fill-none [&_circle]:stroke-[2.2]">
      <circle cx="8" cy="8" r={r} className="stroke-active" />
      <circle cx="8" cy="8" r={r} strokeLinecap="round" className="stroke-accent" strokeDasharray={`${(len * pct) / 100} ${len}`} />
    </svg>
  );
}
