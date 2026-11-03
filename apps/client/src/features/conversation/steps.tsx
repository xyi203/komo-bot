import { useState, type ReactNode } from "react";
import { CopyButton } from "@/components/copy-button";
import { Icon, type IconName } from "@/components/icon";
import { Markdown } from "@/components/markdown";
import { Spinner } from "@/components/spinner";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import type { Call } from "./model";
import { callBad, callBusy, callDetail, categoryOf, groupSummary, thinkPreview, type Category, type Segment, type Step } from "./process";
import { useConversation } from "./context";

const CATEGORY_ICON: Record<Category, IconName> = {
  thinking: "think",
  read: "eye",
  search: "search",
  write: "edit",
  edit: "edit",
  commands: "term",
  code: "code",
  subagents: "branch",
  tools: "plug",
};

// 类别名（进行中 / 已完成 / 接在"并"或"and"后面 / 三类并列里接在前一项后面的写法）。
const LIVE: Record<Category, () => string> = {
  thinking: () => m.cat_thinking_live(),
  read: () => m.cat_read_live(),
  search: () => m.cat_search_live(),
  write: () => m.cat_write_live(),
  edit: () => m.cat_edit_live(),
  commands: () => m.cat_commands_live(),
  code: () => m.cat_code_live(),
  subagents: () => m.cat_subagents_live(),
  tools: () => m.cat_tools_live(),
};
const DONE: Record<Category, [lead: () => string, then: () => string, and: () => string]> = {
  thinking: [() => m.cat_thinking_done(), () => m.cat_thinking_done(), () => m.cat_thinking_done()],
  read: [() => m.cat_read_done(), () => m.cat_read_done_then(), () => m.cat_read_done_and()],
  search: [() => m.cat_search_done(), () => m.cat_search_done_then(), () => m.cat_search_done_and()],
  write: [() => m.cat_write_done(), () => m.cat_write_done_then(), () => m.cat_write_done_and()],
  edit: [() => m.cat_edit_done(), () => m.cat_edit_done_then(), () => m.cat_edit_done_and()],
  commands: [() => m.cat_commands_done(), () => m.cat_commands_done_then(), () => m.cat_commands_done_and()],
  code: [() => m.cat_code_done(), () => m.cat_code_done_then(), () => m.cat_code_done_and()],
  subagents: [() => m.cat_subagents_done(), () => m.cat_subagents_done_then(), () => m.cat_subagents_done_and()],
  tools: [() => m.cat_tools_done(), () => m.cat_tools_done_then(), () => m.cat_tools_done_and()],
};

const TOOL_TITLE: Record<string, () => string> = {
  bash: () => m.tool_bash(),
  read: () => m.tool_read(),
  rg: () => m.tool_rg(),
  write: () => m.tool_write(),
  edit: () => m.tool_edit(),
  codemode: () => m.tool_codemode(),
  task: () => m.tool_task(),
};

/** 组标题的文字：结束的按类别次数降序取前三（两类用"并"连、三类用逗号、多于三类末尾加"等"），在跑的写当前在跑的那一类。 */
function groupTitle(seg: Extract<Segment, { kind: "steps" }>, live: boolean) {
  const summary = groupSummary(seg, live);
  if (live) return { text: LIVE[summary.categories[0]!](), detail: summary.detail, icon: summary.icon };
  const [a, b, c] = summary.categories.map((cat) => DONE[cat]);
  const text =
    !b ? a![0]()
    : !c ? m.cat_join_two({ a: a![0](), b: b[2]() })
    : summary.more ? m.cat_join_many({ a: a![0](), b: b[1](), c: c[1]() })
    : m.cat_join_three({ a: a![0](), b: b[1](), c: c[1]() });
  return { text, detail: undefined, icon: summary.icon };
}

/** 过程组：组头是类别图标加活动摘要，悬停或展开时图标换成箭头；默认收起，展开后限高自己滚。 */
export function StepsFold({ seg, live }: { seg: Extract<Segment, { kind: "steps" }>; live: boolean }) {
  const [open, setOpen] = useState(false);
  const failed = seg.steps.filter((x) => x.kind === "call" && callBad(x.call)).length;
  const title = groupTitle(seg, live);
  return (
    <div className="my-1">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className="group/head flex min-h-7 w-full cursor-pointer items-center gap-2 text-left text-body text-faint hover:text-text focus-visible:text-text"
      >
        <span className="group-hover/head:hidden group-focus-visible/head:hidden group-aria-expanded/head:hidden">
          <Icon name={CATEGORY_ICON[title.icon]} />
        </span>
        <Icon name={open ? "chev-up" : "down"} className="hidden group-hover/head:block group-focus-visible/head:block group-aria-expanded/head:block" />
        <span className={cn("min-w-0 truncate", live && "text-muted")}>
          {title.text}
          {title.detail && <span className="text-faint"> · {title.detail}</span>}
        </span>
        {failed > 0 && <span className="text-negative">· {m.steps_failed({ count: failed })}</span>}
      </button>
      {open && (
        <div className="mt-2 grid max-h-[min(400px,50vh)] grid-cols-[minmax(0,1fr)] gap-1 overflow-auto pl-6">
          {seg.steps.map((step) => (
            <StepRow key={step.id} step={step} />
          ))}
        </div>
      )}
    </div>
  );
}

const stepHead =
  "flex min-h-7 w-full cursor-pointer items-center gap-2 text-left text-body text-faint hover:text-text";

function StepRow({ step }: { step: Step }) {
  const [open, setOpen] = useState(false);
  if (step.kind === "think") {
    return (
      <div>
        <button type="button" aria-expanded={open} onClick={() => setOpen((v) => !v)} className={stepHead}>
          <Icon name="think" />
          <Verb>{m.step_think()}</Verb>
          <span className="min-w-0 flex-1 truncate text-faint">{thinkPreview(step.text)}</span>
        </button>
        {open && <Markdown text={step.text} className="compact mb-1 ml-6 text-muted" />}
      </div>
    );
  }
  const call = step.call;
  const busy = callBusy(call);
  return (
    <div>
      <button type="button" aria-expanded={open} onClick={() => setOpen((v) => !v)} className={stepHead}>
        <Icon name={CATEGORY_ICON[categoryOf(call.name)]} />
        <Verb>{(TOOL_TITLE[call.name] ?? (() => m.tool_generic()))()}</Verb>
        <span className="min-w-0 flex-1 truncate text-faint">{callDetail(call)}</span>
        <span className="shrink-0 text-small">
          {busy ? <Spinner small /> : callBad(call) ? <span className="text-negative">{call.status === "failed" ? m.call_failed() : m.call_uncertain()}</span> : null}
        </span>
      </button>
      {open && <CallCard call={call} />}
    </div>
  );
}

function Verb({ children }: { children: ReactNode }) {
  return <span className="shrink-0 after:ml-2 after:text-faint after:content-['·']">{children}</span>;
}

/** 展开的一次调用：命令（可复制）在上，输出在下；"查看"跳到轨迹里这一条的详情。 */
function CallCard({ call }: { call: Call }) {
  const { inspect } = useConversation();
  const args = call.args;
  const command =
    call.name === "bash" ? String(args.command ?? "")
    : call.name === "codemode" ? String(args.code ?? args.script ?? "")
    : JSON.stringify(args, null, 2);
  const lines = (call.preview ?? "").split("\n");
  const meta = [lines.shift()?.replace(/^\[|\]$/g, "")];
  if (/^exit=/.test(lines[0] ?? "")) meta.push(lines.shift());
  const output = lines.join("\n").replace(/^\n+/, "").trimEnd();
  const busy = callBusy(call);
  const pad = "px-3.5 pl-7";
  return (
    <>
      <div className="mt-0.5 mb-1.5 ml-6 overflow-hidden rounded-control bg-raised max-[600px]:ml-0">
        <div className="grid grid-cols-[6px_auto_minmax(0,1fr)_26px] items-start gap-2.5 py-2 pr-1.5 pl-3">
          <span
            aria-hidden="true"
            className={cn("mt-[7px] size-1.5 rounded-full bg-control", busy && "animate-breathe bg-accent", callBad(call) && "bg-negative")}
          />
          <span className="font-mono text-small leading-5 text-faint">{call.name}</span>
          <pre className="m-0 max-h-40 overflow-auto font-mono text-small leading-5 break-words whitespace-pre-wrap text-text">{command}</pre>
          <CopyButton text={command} label={m.call_copy_command()} />
        </div>
        {output ? (
          <pre className={cn("m-0 max-h-70 overflow-auto bg-surface py-2.5 font-mono text-small leading-5 whitespace-pre text-muted shadow-[inset_0_1px_0_var(--color-rule)]", pad)}>
            {output}
          </pre>
        ) : (
          busy && <p className={cn("m-0 py-1.5 pb-2 text-small leading-[18px] text-faint", pad)}>{m.call_no_output()}</p>
        )}
        {call.preview && (
          <p className={cn("m-0 py-1.5 pb-2 text-small leading-[18px] text-faint shadow-[inset_0_1px_0_var(--color-rule)]", pad)}>
            {meta.filter(Boolean).join(" · ")}
          </p>
        )}
      </div>
      {call.resultEvent && (
        <button
          type="button"
          onClick={() => inspect(call.resultEvent!)}
          className="mb-1.5 ml-6 inline-flex h-6.5 cursor-pointer items-center gap-1 rounded-full px-2.5 text-small text-muted shadow-ring hover:bg-hover-on-white hover:text-text max-[600px]:ml-0"
        >
          <Icon name="code" className="size-[13px]" />
          {m.call_inspect()}
        </button>
      )}
    </>
  );
}
