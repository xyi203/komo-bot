import { useState } from "react";
import { CopyButton } from "@/components/copy-button";
import { Icon } from "@/components/icon";
import { Markdown } from "@/components/markdown";
import { Spinner } from "@/components/spinner";
import { ChangesCard } from "@/features/changes/changes-card";
import { formatStamp } from "@/lib/format";
import { useNow } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { AttachmentList, splitAttachments } from "./attachments";
import { useConversation } from "./context";
import { formatDuration } from "./duration";
import type { Turn } from "./model";
import { segments, type Segment } from "./process";
import { StepsFold } from "./steps";
import type { Delta } from "@komo/api";
import type { MemoryUse } from "./events";

export function UserBubble({ text, at, standalone = false }: { text: string; at?: string; standalone?: boolean }) {
  const { attachments, text: words } = splitAttachments(text);
  return (
    <div className={cn("flex flex-col items-end gap-1", standalone && "mt-6 mb-2")}>
      <AttachmentList attachments={attachments} />
      {words.trim() && (
        <div className="max-w-[80%] rounded-[16px] bg-raised px-3.5 py-2 text-read leading-6 break-words whitespace-pre-wrap max-[600px]:max-w-[88%]">{words}</div>
      )}
      <div className="mt-0.5 flex items-center gap-1.5 text-small text-muted">
        <span className="tabular-nums">{at ? formatStamp(at) : ""}</span>
        {/* 复制的是用户写的话；只有附件时复制整段（含存放路径） */}
        <CopyButton text={words.trim() ? words : text} />
      </div>
    </div>
  );
}

function recallParts(uses: MemoryUse[], memories: ReturnType<typeof useConversation>["memories"]) {
  const mems = uses.map((u) => memories.get(u.memory));
  const conventions = mems.filter((x) => x?.application === "convention").length;
  return { mems, conventions, references: mems.length - conventions };
}

function recallSummary(uses: MemoryUse[], memories: ReturnType<typeof useConversation>["memories"]): string {
  const { conventions, references } = recallParts(uses, memories);
  const parts = [conventions && m.recall_conventions({ count: conventions }), references && m.recall_references({ count: references })].filter(Boolean);
  return m.recall_brought({ parts: parts.join(m.recall_separator()) });
}

const mote = (convention: boolean) => (
  <span aria-hidden="true" className={cn("inline-block size-2 shrink-0 rounded-full", convention ? "bg-accent" : "border-[1.5px] border-accent")} />
);

function RecallFold({ uses }: { uses: MemoryUse[] }) {
  const { memories } = useConversation();
  const [open, setOpen] = useState(false);
  const { mems } = recallParts(uses, memories);
  return (
    <div className="mb-2">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className="inline-flex cursor-pointer items-center gap-1 py-0.5 text-left text-body leading-6 text-accent hover:text-text"
      >
        <span className="mr-1">{mote(true)}</span>
        {recallSummary(uses, memories)}
        <Icon name="right" className={cn("size-3.5 transition-transform", open && "rotate-90")} />
      </button>
      {open && (
        <div className="mt-1.5 mb-1 border-l border-border py-1">
          {uses.map((use, i) => {
            const memory = mems[i];
            return (
              <div key={use.memory} className="grid grid-cols-[10px_minmax(0,1fr)] gap-2 py-1 pl-3.5">
                <span className="mt-[7px]">{mote(memory?.application === "convention")}</span>
                <span>{memory ? memory.content : <span className="text-muted">{m.recall_missing({ id: use.memory })}</span>}</span>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

function ProcessRow({ part, live }: { part: Segment; live: boolean }) {
  if (part.kind === "steps") return <StepsFold seg={part} live={live} />;
  if (part.kind === "recall") return <RecallFold uses={part.uses} />;
  if (part.kind === "steer") {
    return (
      <div className="mb-2 text-body leading-[22px] text-text">
        <span className="mr-2 inline-block rounded-xs bg-raised px-1.5 text-small text-muted">{m.steer_tag()}</span>
        {part.text}
      </div>
    );
  }
  return <Markdown text={part.text} className="my-3 first:mt-0" />;
}

/**
 * 一轮（一条 Run）：用户的话在上，抬头"已完成，用时 …"点它收起 / 展开这一轮的过程；回复与插话始终可见，
 * 收起只收思考、调用与召回。只有回复、没有过程的一轮抬头不给开合。结束的轮默认收起，跑着的默认展开。
 */
export function TurnView({ turn, delta, last }: { turn: Turn; delta: Delta | null; last: boolean }) {
  const { changes, memories, review } = useConversation();
  const live = turn.status === "running" || turn.status === "queued";
  const now = useNow(live);
  const [userOpen, setUserOpen] = useState<boolean | null>(null);
  const parts = segments(turn.process);
  const foldable = parts.some((p) => p.kind === "steps" || p.kind === "recall");
  const open = !foldable || (userOpen ?? live);
  const spent = formatDuration((turn.end ? new Date(turn.end).getTime() : now) - new Date(turn.start).getTime());

  const headText = live ? (
    <>
      <Spinner small />
      {m.turn_running({ time: spent })}
    </>
  ) : turn.status === "failed" ? (
    <>
      <span className="text-negative">{m.turn_failed()}</span>
      {m.turn_spent_suffix({ time: spent })}
    </>
  ) : turn.status === "cancelled" ? (
    m.turn_cancelled({ time: spent })
  ) : (
    m.turn_done({ time: spent })
  );
  const lit = turn.recall.length > 0 && (
    <span className="ml-auto inline-flex items-center gap-1.5 text-label text-accent">
      {mote(true)}
      {recallSummary(turn.recall, memories)}
    </span>
  );
  const headClass = "my-4 flex w-full items-center gap-1.5 border-b border-border pb-2.5 text-left text-read leading-[22px] text-muted tabular-nums";

  const shown = open ? parts : parts.filter((p) => p.kind === "text" || p.kind === "steer");
  const lastIndex = shown.length - 1;
  const changesOfRun = changes.get(turn.run);
  const card = changesOfRun && !live ? <ChangesCard changes={changesOfRun} onReview={review} /> : null;

  return (
    <section className="group/turn mb-7">
      {turn.users.map((u, i) => (
        <UserBubble key={i} text={u.text} at={u.at} />
      ))}
      {foldable || live ? (
        <button
          type="button"
          aria-expanded={open}
          disabled={!foldable}
          onClick={() => setUserOpen(!open)}
          className={cn(headClass, foldable ? "cursor-pointer hover:text-text" : "cursor-default")}
        >
          {headText}
          {foldable && <Icon name={open ? "chev-up" : "down"} className="size-3.5" />}
          {lit}
        </button>
      ) : (
        <p className={headClass}>
          {headText}
          {lit}
        </p>
      )}
      {shown.length > 0 && (
        <div className="-mt-1">
          {shown.map((part, i) => (
            <ProcessRow key={part.id} part={part} live={live && part.kind === "steps" && i === lastIndex && !turn.answer && !delta} />
          ))}
        </div>
      )}
      {turn.answer ? (
        <>
          <Markdown text={turn.answer.text} className={shown.length > 0 ? "mt-3" : undefined} />
          {card}
          <div
            className={cn(
              "mt-2 -ml-1.5 flex items-center gap-1.5 text-small text-muted",
              !last && "[@media(hover:hover)]:opacity-0 [@media(hover:hover)]:transition-opacity group-hover/turn:opacity-100 focus-within:opacity-100",
            )}
          >
            <CopyButton text={turn.answer.text} label={m.copy_answer()} />
            <span className="tabular-nums">{formatStamp(turn.answer.at)}</span>
          </div>
        </>
      ) : (
        <>
          {live && delta && <Markdown text={delta.text} className={cn("streaming", shown.length > 0 && "mt-3")} />}
          {card}
        </>
      )}
      {turn.status === "failed" && <p className="mb-3.5 text-label text-negative">{m.turn_failed_note({ reason: turn.reason ?? m.turn_no_reason() })}</p>}
      {turn.status === "cancelled" && <p className="mb-3.5 text-label text-muted">{m.turn_cancelled_note()}</p>}
    </section>
  );
}
