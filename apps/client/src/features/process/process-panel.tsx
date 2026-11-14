import type { MemoryBrief, SessionTree } from "@komo/api";
import type { RunChanges, SessionSummary, WireEvent } from "@komo/protocol";
import { Link } from "@tanstack/react-router";
import { Icon } from "@/components/icon";
import { Markdown } from "@/components/markdown";
import { Spinner } from "@/components/spinner";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { ChangesPage, type ChangesUi } from "@/features/changes/changes-page";
import { splitAttachments } from "@/features/conversation/attachments";
import type { ProcessTab } from "@/features/conversation/context";
import { formatDuration } from "@/features/conversation/duration";
import type { Turn, View } from "@/features/conversation/model";
import { segments } from "@/features/conversation/process";
import { RecallFold } from "@/features/conversation/recall";
import { StepsFold } from "@/features/conversation/steps";
import { sessionTitle } from "@/features/sessions/title";
import type { TraceUi } from "@/features/trace/state";
import { TraceView } from "@/features/trace/trace-view";
import { formatElapsed } from "@/lib/format";
import { useNow } from "@/lib/hooks";
import { m } from "@/paraglide/messages.js";

/**
 * 过程面板（docs/redesign.md P5，docs/web.md §4）：对话右侧滑出，宽 380（≤1180 浮在对话上面）。
 * 标题栏：当前看的是第几轮（上一轮 / 下一轮来回切）与关闭；三个标签：步骤（这一轮的思考、工具调用、召回、插话，
 * 这段对话派出的后台任务）、改动（这一轮的文件对比）、轨迹（整段对话的轨迹，定位到这一轮）。
 */
export function ProcessPanel({
  view,
  events,
  tree,
  memories,
  run,
  tab,
  changes,
  changesUi,
  trace,
  kids,
  delta,
  onTab,
  onRun,
  onClose,
}: {
  view: View;
  events: readonly WireEvent[];
  tree: SessionTree | undefined;
  memories: ReadonlyMap<string, MemoryBrief>;
  run: string;
  tab: ProcessTab;
  changes: ReadonlyMap<string, RunChanges>;
  changesUi: ChangesUi;
  trace: TraceUi;
  kids: readonly SessionSummary[];
  delta: boolean;
  onTab: (tab: ProcessTab) => void;
  onRun: (run: string) => void;
  onClose: () => void;
}) {
  const turns = view.items.filter((item): item is Turn => item.kind === "turn");
  const index = turns.findIndex((t) => t.run === run);
  const turn = turns[index];
  const live = !!turn && (turn.status === "running" || turn.status === "queued");
  const now = useNow(live);
  const numbers = new Map(turns.map((t, i) => [t.run, i + 1]));
  const mine = changes.get(run);

  const ask = turn?.users[0] ? splitAttachments(turn.users[0].text).text.trim() : "";
  const parts = turn ? segments(turn.process) : [];
  const steps = parts.reduce((sum, p) => sum + (p.kind === "steps" ? p.steps.filter((x) => x.kind === "call").length : 0), 0);
  const spent = turn ? formatDuration((turn.end ? new Date(turn.end).getTime() : now) - new Date(turn.start).getTime()) : "";

  const nav = "grid size-7 cursor-pointer place-items-center rounded-control text-muted transition-colors hover:bg-hover-on-white hover:text-text disabled:cursor-default disabled:opacity-35 disabled:hover:bg-transparent";
  return (
    <aside
      aria-label={m.proc_title()}
      className="flex min-h-0 flex-col border-l border-border bg-surface max-[1180px]:fixed max-[1180px]:inset-y-0 max-[1180px]:right-0 max-[1180px]:z-20 max-[1180px]:w-[min(380px,100%)] max-[1180px]:shadow-[-16px_0_48px_rgb(0_0_0/0.12)]"
    >
      <header className="flex h-14 shrink-0 items-center gap-1 pr-3 pl-5">
        <h2 className="text-section-title font-semibold">{m.proc_title()}</h2>
        {turns.length > 1 && (
          <div className="ml-2 flex items-center text-label text-muted tabular-nums">
            <button type="button" aria-label={m.proc_prev()} title={m.proc_prev()} disabled={index <= 0} onClick={() => onRun(turns[index - 1]!.run)} className={nav}>
              <Icon name="arrow-left" className="size-3.5" />
            </button>
            <span>{m.proc_turn_of({ n: index + 1, total: turns.length })}</span>
            <button type="button" aria-label={m.proc_next()} title={m.proc_next()} disabled={index < 0 || index >= turns.length - 1} onClick={() => onRun(turns[index + 1]!.run)} className={nav}>
              <Icon name="arrow-right" className="size-3.5" />
            </button>
          </div>
        )}
        <button type="button" aria-label={m.proc_close()} title={m.proc_close()} onClick={onClose} className={`${nav} ml-auto`}>
          <Icon name="x" />
        </button>
      </header>
      <Tabs value={tab} onValueChange={(value) => onTab(value as ProcessTab)} className="flex min-h-0 flex-1 flex-col">
        <div className="min-h-0 flex-1 overflow-y-auto px-5 pb-6">
          {ask && <p className="line-clamp-3 rounded-[12px] bg-raised px-3 py-2 text-label leading-5 break-words whitespace-pre-wrap">{ask}</p>}
          {turn && (
            <p className="mt-2 flex items-center gap-1.5 text-small text-faint tabular-nums">
              {live && <Spinner small />}
              {m.proc_meta({ time: spent, steps })}
              {turn.recall.length > 0 && <> · {m.reply_memories({ count: turn.recall.length })}</>}
            </p>
          )}
          <TabsList aria-label={m.proc_title()} className="mt-3 mb-3 gap-0.5 rounded-control bg-ground p-0.5">
            {(["steps", "changes", "trace"] as const).map((key) => (
              <TabsTrigger
                key={key}
                value={key}
                className="h-7 rounded-sm px-3 text-label text-muted hover:text-text data-[state=active]:bg-surface data-[state=active]:text-text data-[state=active]:shadow-ring"
              >
                {key === "steps" ? m.proc_tab_steps() : key === "changes" ? m.proc_tab_changes() : m.proc_tab_trace()}
              </TabsTrigger>
            ))}
          </TabsList>
          <TabsContent value="steps" className="outline-none">
            {turn ? <StepsTab turn={turn} parts={parts} streaming={delta} kids={kids} /> : null}
          </TabsContent>
          <TabsContent value="changes" className="outline-none">
            <ChangesPage runs={mine ? [mine] : []} turnNumbers={numbers} ui={changesUi} />
          </TabsContent>
          <TabsContent value="trace" className="outline-none">
            <TraceView events={events} liveRuns={view.runs} tree={tree} memories={memories} ui={trace} focusRun={run} compact />
          </TabsContent>
        </div>
      </Tabs>
    </aside>
  );
}

/** 步骤：这一轮的过程按组排下来（沿用对话里原来的分组与展开），底下是这段对话派出、还在跑的后台任务。 */
function StepsTab({ turn, parts, streaming, kids }: { turn: Turn; parts: ReturnType<typeof segments>; streaming: boolean; kids: readonly SessionSummary[] }) {
  const live = turn.status === "running" || turn.status === "queued";
  const now = useNow(kids.length > 0);
  const lastIndex = parts.length - 1;
  return (
    <>
      {parts.length === 0 && <p className="text-label text-muted">{m.proc_empty_steps()}</p>}
      {parts.map((part, i) => {
        if (part.kind === "steps") return <StepsFold key={part.id} seg={part} live={live && i === lastIndex && !turn.answer && !streaming} defaultOpen />;
        if (part.kind === "recall") return <RecallFold key={part.id} uses={part.uses} />;
        if (part.kind === "steer") {
          return (
            <div key={part.id} className="mb-2 text-body leading-[22px] text-text">
              <span className="mr-2 inline-block rounded-xs bg-raised px-1.5 text-small text-muted">{m.steer_tag()}</span>
              {part.text}
            </div>
          );
        }
        return <Markdown key={part.id} text={part.text} className="compact my-3 first:mt-0" />;
      })}
      {kids.length > 0 && (
        <>
          <h3 className="mt-5 mb-1 text-small text-faint">{m.proc_kids()}</h3>
          {kids.map((kid) => (
            <Link
              key={kid.session}
              to="/sessions/$sessionId"
              params={{ sessionId: kid.session }}
              className="group/kid flex min-h-8 items-center gap-2.5 text-body text-muted no-underline"
            >
              <Spinner small />
              <span className="min-w-0 truncate text-text group-hover/kid:underline group-hover/kid:underline-offset-3">{sessionTitle(kid)}</span>
              <span className="shrink-0 tabular-nums">{formatElapsed(kid.created_at, now)}</span>
              <Icon name="right" className="ml-auto size-3.5 shrink-0" />
            </Link>
          ))}
        </>
      )}
    </>
  );
}
