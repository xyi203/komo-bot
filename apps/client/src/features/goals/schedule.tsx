import { actMatter, scheduleMatter, keys, useMatterRuns, uuid } from "@komo/api";
import type { Matter, MatterRun, MatterSchedule, NotifyPolicy, ScheduleSpec } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState, type FormEvent } from "react";
import { Icon } from "@/components/icon";
import { BlockError, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { IconButton } from "@/components/ui/icon-button";
import { cn } from "@/lib/utils";
import { errorText } from "@/lib/errors";
import { m } from "@/paraglide/messages.js";
import { firingLabel, formatWhen, isClosed, lastRunText, nextRunText, scheduleOn, scheduleSummary, serverText } from "./model";
import { FormError, Hint, Input, itemClass, ItemBody, Label, Notice, Pill, Select, SubHeading, rowClass, TextButton, Textarea } from "./parts";

// 目标详情里的「定时」：一行摘要（点开弹层）+ 弹层（改配置、立即跑一次、去掉定时、运行历史）。定时是目标的一项配置
// （docs/redesign.md P8），没有单独的页面；写都走目标的动词 `schedule` / `unschedule` / `run`。

/** 改了定时之后重取目标详情、清单与运行历史（都在 `keys.matters` 前缀下）。 */
function useRefreshGoals() {
  const queryClient = useQueryClient();
  return () => queryClient.invalidateQueries({ queryKey: keys.matters });
}

/** 开关：暂停 / 恢复定时。 */
function Switch({ on, busy, label, onToggle }: { on: boolean; busy: boolean; label: string; onToggle: () => void }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label}
      title={label}
      disabled={busy}
      onClick={onToggle}
      className={cn(
        "relative mt-0.5 h-5 w-9 shrink-0 cursor-pointer rounded-full transition-colors disabled:cursor-progress disabled:opacity-60",
        on ? "bg-primary" : "bg-control",
      )}
    >
      <span className={cn("absolute top-0.5 left-0.5 size-4 rounded-full bg-white shadow-sm transition-transform", on && "translate-x-4")} />
    </button>
  );
}

/** 详情里的定时一行：没设置是「加上定时」，设置了是节奏、开关、上次与下一次；点开弹层。 */
export function ScheduleRow({ matter, schedule }: { matter: Matter; schedule: MatterSchedule | undefined }) {
  const refresh = useRefreshGoals();
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [fail, setFail] = useState("");
  const closed = isClosed(matter);

  const dialog = <ScheduleDialog matter={matter} schedule={schedule} open={open} onOpenChange={setOpen} />;
  if (!schedule) {
    return (
      <>
        <button type="button" disabled={closed} onClick={() => setOpen(true)} className={cn(itemClass, "cursor-pointer disabled:cursor-default")}>
          <ItemBody
            icon={<Icon name="plus" />}
            title={<span>{m.goal_schedule_add()}</span>}
            end={!closed && <Icon name="right" className="size-3.5" />}
            desc={closed ? m.goal_schedule_closed() : m.goal_schedule_add_hint()}
          />
        </button>
        {dialog}
      </>
    );
  }
  const { job } = schedule;
  const on = scheduleOn(schedule);
  const next = nextRunText(schedule);
  const toggle = async () => {
    setBusy(true);
    setFail("");
    try {
      await scheduleMatter(matter.id, { enabled: !on });
      await refresh();
    } catch (error) {
      setFail(m.matter_error_edit({ message: serverText(error) }));
    }
    setBusy(false);
  };
  return (
    <>
      <div className={cn(rowClass, "grid grid-cols-[20px_minmax(0,1fr)_auto] items-start gap-x-3")}>
        <span className="mt-0.5 grid size-5 place-items-center text-faint">
          <Icon name="clock" />
        </span>
        <button type="button" onClick={() => setOpen(true)} className="col-start-2 min-w-0 cursor-pointer text-left text-text">
          <span className="flex flex-wrap items-center gap-x-2 leading-5.5">
            {scheduleSummary(schedule)}
            {job.status === "paused" && <Pill tone="off">{m.cron_paused()}</Pill>}
            {job.status === "done" && <Pill tone="off">{m.cron_finished()}</Pill>}
          </span>
          <span className="mt-0.5 block text-small leading-4.5 text-muted">
            {lastRunText(schedule)}
            {next && ` · ${m.goal_schedule_next({ when: next })}`}
          </span>
          {job.last_error && <span className="mt-0.5 block text-small leading-4.5 text-negative [overflow-wrap:anywhere]">{job.last_error}</span>}
        </button>
        {job.status !== "done" && !closed ? <Switch on={on} busy={busy} label={on ? m.goal_schedule_pause() : m.goal_schedule_resume()} onToggle={() => void toggle()} /> : <span />}
        {fail && <span className="col-start-2 col-end-4 mt-1 text-label text-negative">{fail}</span>}
      </div>
      {dialog}
    </>
  );
}

// ---- 配置表单 ----

type Mode = "minutes" | "hours" | "daily" | "custom";
interface Freq {
  mode: Mode;
  n: string;
  time: string;
  expr: string;
}

const pad = (n: number) => String(n).padStart(2, "0");
const browserZone = () => Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";

/** 现有定时怎么回填到表单：认得出的三种写法回填成间隔 / 每天，其余原样放进表达式框；一次性的写成 `@at 本机时间`。 */
function freqOf(schedule: MatterSchedule | undefined): Freq {
  const blank: Freq = { mode: "daily", n: "30", time: "09:00", expr: "" };
  const trigger = schedule?.job.trigger;
  if (!trigger) return blank;
  if (trigger.kind === "at") {
    const at = new Date(trigger.at);
    const text = `@at ${at.getFullYear()}-${pad(at.getMonth() + 1)}-${pad(at.getDate())} ${pad(at.getHours())}:${pad(at.getMinutes())}`;
    return { ...blank, mode: "custom", expr: text };
  }
  const expr = trigger.expr.trim();
  let hit = /^\*\/(\d+) \* \* \* \*$/.exec(expr);
  if (hit) return { ...blank, mode: "minutes", n: hit[1], expr };
  hit = /^0 \*\/(\d+) \* \* \*$/.exec(expr);
  if (hit) return { ...blank, mode: "hours", n: hit[1], expr };
  hit = /^(\d{1,2}) (\d{1,2}) \* \* \*$/.exec(expr);
  if (hit) return { ...blank, mode: "daily", time: `${pad(Number(hit[2]))}:${pad(Number(hit[1]))}`, expr };
  return { ...blank, mode: "custom", expr };
}

/** 表单现在写的是哪条表达式；写得不成样（空、越界）是 null。 */
function exprOf(f: Freq): string | null {
  const n = Number(f.n);
  switch (f.mode) {
    case "minutes":
      return Number.isInteger(n) && n >= 1 && n <= 59 ? `*/${n} * * * *` : null;
    case "hours":
      return Number.isInteger(n) && n >= 1 && n <= 23 ? `0 */${n} * * *` : null;
    case "daily": {
      const hit = /^(\d{1,2}):(\d{2})$/.exec(f.time);
      return hit ? `${Number(hit[2])} ${Number(hit[1])} * * *` : null;
    }
    case "custom":
      return f.expr.trim() || null;
  }
}

function timeZones(): string[] {
  try {
    return Intl.supportedValuesOf("timeZone");
  } catch {
    return [];
  }
}

function ScheduleForm({ matter, schedule, onDone }: { matter: Matter; schedule: MatterSchedule | undefined; onDone: () => void }) {
  const refresh = useRefreshGoals();
  const job = schedule?.job;
  const initial = freqOf(schedule);
  const initialExpr = exprOf(initial) ?? "";
  const initialZone = job?.trigger.kind === "cron" ? job.trigger.tz : browserZone();
  const [freq, setFreq] = useState<Freq>(initial);
  const [zone, setZone] = useState(initialZone);
  const [prompt, setPrompt] = useState(job?.prompt ?? "");
  const [notify, setNotify] = useState<NotifyPolicy>(job?.notify ?? "always");
  const [busy, setBusy] = useState(false);
  const [fail, setFail] = useState("");

  const expr = exprOf(freq);
  const set = (patch: Partial<Freq>) => setFreq((prev) => ({ ...prev, ...patch }));

  // 改的时候只发改过的字段（后端「给出的字段才改」）；新建要给全
  const spec: ScheduleSpec = {};
  if (expr !== null && expr !== initialExpr) spec.schedule = expr;
  const zoneText = zone.trim();
  if (zoneText && (zoneText !== initialZone || (spec.schedule !== undefined && job?.trigger.kind === "at"))) spec.timezone = zoneText;
  if (!job?.command && prompt.trim() && prompt.trim() !== (job?.prompt ?? "")) spec.prompt = prompt.trim();
  if (notify !== (job?.notify ?? "always")) spec.notify = notify;
  const changed = Object.keys(spec).length > 0;
  const ready = schedule ? changed && expr !== null && !!zoneText : expr !== null && !!zoneText;

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!ready || busy) return;
    setBusy(true);
    setFail("");
    try {
      await scheduleMatter(matter.id, schedule ? spec : { ...spec, schedule: expr ?? undefined, timezone: zoneText, notify });
      await refresh();
      if (!schedule) onDone();
    } catch (error) {
      setFail(m.matter_error_edit({ message: serverText(error) }));
    }
    setBusy(false);
  };

  return (
    <form onSubmit={(event) => void submit(event)} className="mt-1">
      <Label htmlFor="sc-mode">{m.goal_schedule_when()}</Label>
      <Select id="sc-mode" value={freq.mode} onChange={(event) => set({ mode: event.target.value as Mode })}>
        <option value="daily">{m.goal_freq_daily()}</option>
        <option value="hours">{m.goal_freq_hours()}</option>
        <option value="minutes">{m.goal_freq_minutes()}</option>
        <option value="custom">{m.goal_freq_custom()}</option>
      </Select>
      {freq.mode === "daily" && <Input aria-label={m.goal_freq_time()} type="time" required value={freq.time} onChange={(event) => set({ time: event.target.value })} className="mt-2" />}
      {(freq.mode === "minutes" || freq.mode === "hours") && (
        <div className="mt-2 flex items-center gap-2 text-label">
          {m.goal_freq_every()}
          <Input
            aria-label={m.goal_freq_every()}
            type="number"
            min={1}
            max={freq.mode === "minutes" ? 59 : 23}
            required
            value={freq.n}
            onChange={(event) => set({ n: event.target.value })}
            className="w-20"
          />
          {freq.mode === "minutes" ? m.goal_freq_unit_minutes() : m.goal_freq_unit_hours()}
        </div>
      )}
      {freq.mode === "custom" && (
        <>
          <Input aria-label={m.goal_freq_custom()} required value={freq.expr} onChange={(event) => set({ expr: event.target.value })} className="mt-2 font-mono text-label" placeholder="30 9 * * 1-5" />
          <Hint className="mt-1.5">{m.goal_freq_custom_hint()}</Hint>
        </>
      )}

      <Label htmlFor="sc-zone">{m.goal_schedule_zone()}</Label>
      <Input id="sc-zone" list="sc-zones" required value={zone} onChange={(event) => setZone(event.target.value)} placeholder="Asia/Shanghai" />
      <datalist id="sc-zones">
        {timeZones().map((tz) => (
          <option key={tz} value={tz} />
        ))}
      </datalist>

      {job?.command ? (
        <>
          <Label>{m.goal_schedule_command()}</Label>
          <p className="rounded-control bg-raised px-3 py-2 font-mono text-label leading-5 [overflow-wrap:anywhere]">{job.command}</p>
        </>
      ) : (
        <>
          <Label htmlFor="sc-prompt">{m.goal_schedule_prompt()}</Label>
          <Textarea id="sc-prompt" value={prompt} onChange={(event) => setPrompt(event.target.value)} placeholder={m.goal_schedule_prompt_hint()} />
        </>
      )}

      <Label htmlFor="sc-notify">{m.goal_schedule_notify()}</Label>
      <Select id="sc-notify" value={notify} onChange={(event) => setNotify(event.target.value as NotifyPolicy)}>
        <option value="always">{m.goal_notify_always()}</option>
        <option value="on_error">{m.goal_notify_on_error()}</option>
        <option value="never">{m.goal_notify_never()}</option>
      </Select>

      {fail && <FormError>{fail}</FormError>}
      <div className="mt-4">
        <Button type="submit" variant="primary" disabled={!ready || busy}>
          {busy ? m.goal_schedule_saving() : schedule ? m.matter_save() : m.goal_schedule_create()}
        </Button>
      </div>
    </form>
  );
}

// ---- 运行历史 ----

function RunDot({ run }: { run: MatterRun }) {
  const waiting = run.waiting !== undefined || run.status === "waiting";
  return (
    <span
      aria-hidden="true"
      className={cn(
        "mt-2 size-2 shrink-0 rounded-full",
        waiting
          ? "border-[1.5px] border-wait-dot"
          : run.status === "error"
            ? "bg-negative"
            : run.status === "queued"
              ? "animate-breathe bg-accent"
              : run.status === "skipped"
                ? "border-[1.5px] border-dashed border-faint"
                : "bg-faint",
      )}
    />
  );
}

function RunRow({ run }: { run: MatterRun }) {
  const label = run.waiting ? m.goal_task_waiting() : firingLabel(run.status);
  const body = (
    <>
      <RunDot run={run} />
      <span className="min-w-0">
        <span className="flex flex-wrap items-center gap-x-2 leading-6">
          {label}
          {run.waiting && <Pill tone="wait">{m.goal_task_waiting()}</Pill>}
        </span>
        {run.error && <span className={cn("block text-small leading-4.5 [overflow-wrap:anywhere]", run.status === "error" ? "text-negative" : "text-muted")}>{run.error}</span>}
      </span>
      <span className="flex items-center gap-1 text-small leading-6 text-faint tabular-nums">
        {formatWhen(run.fired_at ?? run.scheduled_at)}
        {run.session && <Icon name="right" className="size-3" />}
      </span>
    </>
  );
  const cls = cn(rowClass, "grid grid-cols-[8px_minmax(0,1fr)_auto] items-start gap-x-3 py-2 text-label text-text no-underline");
  return run.session ? (
    <Link to="/sessions/$sessionId" params={{ sessionId: run.session }} className={cls}>
      {body}
    </Link>
  ) : (
    <div className={cls}>{body}</div>
  );
}

/** 运行历史：新的在前，一页 20 条，底下「显示更多」。每条点进对应的会话看过程。 */
function RunHistory({ id }: { id: string }) {
  const runs = useMatterRuns(id, true);
  const rows = runs.data?.pages.flatMap((page) => page.runs) ?? [];
  return (
    <>
      <SubHeading count={runs.data ? rows.length : undefined}>{m.goal_runs()}</SubHeading>
      {runs.isError && !runs.data ? (
        <BlockError message={m.goal_runs_error({ message: errorText(runs.error) })} onRetry={() => void runs.refetch()} />
      ) : !runs.data ? (
        <SkeletonRows count={3} className="h-9" />
      ) : rows.length ? (
        <div className="relative z-0 divide-y divide-rule">
          {rows.map((run) => (
            <RunRow key={run.scheduled_at} run={run} />
          ))}
        </div>
      ) : (
        <Hint>{m.cron_never_ran()}</Hint>
      )}
      {runs.hasNextPage && (
        <TextButton tone="quiet" disabled={runs.isFetchingNextPage} onClick={() => void runs.fetchNextPage()} className="mt-2">
          {runs.isFetchingNextPage ? m.goal_runs_loading() : m.goal_runs_more()}
        </TextButton>
      )}
    </>
  );
}

// ---- 弹层 ----

export function ScheduleDialog({
  matter,
  schedule,
  open,
  onOpenChange,
}: {
  matter: Matter;
  schedule: MatterSchedule | undefined;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent aria-describedby={undefined} className="flex max-h-[min(780px,calc(100svh-48px))] w-[min(520px,calc(100vw-32px))] flex-col">
        <ScheduleBody matter={matter} schedule={schedule} onClose={() => onOpenChange(false)} />
      </DialogContent>
    </Dialog>
  );
}

function ScheduleBody({ matter, schedule, onClose }: { matter: Matter; schedule: MatterSchedule | undefined; onClose: () => void }) {
  const refresh = useRefreshGoals();
  const [busy, setBusy] = useState<"toggle" | "run" | "remove" | null>(null);
  const [fail, setFail] = useState("");
  const [fired, setFired] = useState<string | null>(null);
  const [sure, setSure] = useState(false);
  const closed = isClosed(matter);

  const act = async (kind: "toggle" | "run" | "remove") => {
    setBusy(kind);
    setFail("");
    try {
      if (kind === "toggle" && schedule) await scheduleMatter(matter.id, { enabled: !scheduleOn(schedule) });
      else if (kind === "run") setFired((await actMatter(matter.id, "run", { request_key: `web:matter-run:${uuid()}` })).fired?.session ?? null);
      else if (kind === "remove") {
        await actMatter(matter.id, "unschedule", {});
        onClose();
      }
      await refresh();
    } catch (error) {
      setFail(m.matter_error_edit({ message: serverText(error) }));
    }
    setBusy(null);
    setSure(false);
  };

  const on = schedule ? scheduleOn(schedule) : false;
  const next = schedule ? nextRunText(schedule) : null;
  return (
    <div className="min-h-0 overflow-y-auto px-5 pt-4 pb-5">
      <div className="flex min-h-8 items-center gap-2">
        <DialogTitle className="m-0 flex-1 text-section-title font-semibold">{m.goal_schedule_title()}</DialogTitle>
        <DialogClose asChild>
          <IconButton name="x" label={m.close()} className="-mr-2" />
        </DialogClose>
      </div>
      {schedule && (
        <p className="mt-0.5 text-label text-muted">
          {scheduleSummary(schedule)}
          {schedule.job.status === "paused" && ` · ${m.cron_paused()}`}
          {next && ` · ${m.goal_schedule_next({ when: next })}`}
        </p>
      )}
      <ScheduleForm matter={matter} schedule={schedule} onDone={onClose} />
      {schedule && (
        <>
          <div className="mt-5 flex flex-wrap items-center gap-2 border-t border-rule pt-4">
            {schedule.job.status !== "done" && !closed && (
              <Button disabled={busy !== null} onClick={() => void act("toggle")}>
                {on ? m.goal_schedule_pause() : m.goal_schedule_resume()}
              </Button>
            )}
            {!closed && (
              <Button disabled={busy !== null} onClick={() => void act("run")}>
                {busy === "run" ? m.cron_run_busy() : m.cron_run_now()}
              </Button>
            )}
            <span className="ml-auto">
              <TextButton tone="danger" disabled={busy !== null} onClick={() => (sure ? void act("remove") : setSure(true))}>
                {sure ? m.goal_schedule_remove_sure() : m.goal_schedule_remove()}
              </TextButton>
            </span>
          </div>
          {fail && <FormError>{fail}</FormError>}
          {fired !== null && (
            <Notice>
              {m.goal_run_started()}{" "}
              <Link to="/sessions/$sessionId" params={{ sessionId: fired }} onClick={onClose} className="font-medium text-text underline underline-offset-3">
                {m.goal_run_session()}
              </Link>
            </Notice>
          )}
          <RunHistory id={matter.id} />
        </>
      )}
    </div>
  );
}
