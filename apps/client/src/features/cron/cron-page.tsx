import { deleteCron, keys, patchCron, runCron, useCronList, useSessions } from "@komo/api";
import type { CronFiring, CronJob, CronJobStatus, FiringStatus, ManualCronRunResponse, SessionSummary } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useMemo, useState } from "react";
import { BlockError, DetailCard, Page, PageBody, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { DetailTop, FormError, Kv, linkClass, Notice, Pill, SubHeading, TextButton } from "@/features/matters/parts";
import { sessionTitle } from "@/features/sessions/title";
import { errorText } from "@/lib/errors";
import { scheduleText } from "@/lib/cron-text";
import { useMediaQuery } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { formatWhen } from "./when";

const firingLabel = (status: FiringStatus) =>
  ({ queued: m.cron_firing_queued, ok: m.cron_firing_ok, error: m.cron_firing_error, waiting: m.cron_firing_waiting, skipped: m.cron_firing_skipped })[status]();
const firingTone = (status: FiringStatus) => (status === "error" ? "bad" : status === "waiting" ? "wait" : status === "ok" ? undefined : "off");
const notifyLabel = (policy: CronJob["notify"]) => ({ always: m.cron_notify_always, on_error: m.cron_notify_on_error, never: m.cron_notify_never })[policy]();
const overlapLabel = (policy: CronJob["overlap"]) => ({ skip: m.cron_overlap_skip, allow: m.cron_overlap_allow })[policy]();
const stateLabel = (status: CronJob["status"]) => ({ active: m.cron_state_active, paused: m.cron_paused, done: m.cron_finished })[status]();

const COLUMNS = "grid-cols-[16px_minmax(0,1fr)_128px_104px] max-[600px]:grid-cols-[16px_minmax(0,1fr)_auto]";

function FiringPill({ last }: { last: CronFiring | undefined }) {
  return last ? <Pill tone={firingTone(last.status)}>{firingLabel(last.status)}</Pill> : <span className="text-label text-muted">{m.cron_never_ran()}</span>;
}

/**
 * 定时页（docs/web.md §13）：一个列表，点开看详情；不选时列表独占画布。选中的任务 id 在地址里（`/cron/$id`），
 * 窄屏（≤1180）详情盖住列表。新建与修改走对话或 `komo cron`。
 */
export function CronPage({ selected }: { selected: string | null }) {
  const query = useCronList();
  const sessions = useSessions(true).data;
  const navigate = useNavigate();
  const compact = useMediaQuery("(max-width: 1180px)");
  const jobs = useMemo(() => query.data?.jobs ?? [], [query.data]);
  const statuses = useMemo(() => new Map<string, CronJobStatus>((query.data?.status ?? []).map((x) => [x.job, x])), [query.data]);
  const ready = query.isSuccess;
  const current = jobs.find((j) => j.id === selected);
  const lastOf = (job: CronJob) => statuses.get(job.id)?.last;
  const nextOf = (job: CronJob) => statuses.get(job.id)?.next_run_at ?? job.next_run_at;

  const sorted = useMemo(() => {
    const rank = { active: 0, paused: 1, done: 2 };
    const next = (job: CronJob) => statuses.get(job.id)?.next_run_at ?? job.next_run_at ?? "~";
    return [...jobs].sort((a, b) => rank[a.status] - rank[b.status] || next(a).localeCompare(next(b)));
  }, [jobs, statuses]);

  const finding = (): string => {
    if (!jobs.length) return m.cron_finding_none();
    const waiting = jobs.filter((j) => lastOf(j)?.status === "waiting");
    if (waiting.length)
      return m.cron_finding_waiting({ count: waiting.length, names: new Intl.ListFormat(undefined, { type: "conjunction", style: "narrow" }).format(waiting.map((j) => `“${j.name}”`)) });
    const failed = jobs.find((j) => lastOf(j)?.status === "error" || j.last_error);
    if (failed) return failed.last_error ? m.cron_finding_no_slot({ name: failed.name }) : m.cron_finding_failed({ name: failed.name });
    const upcoming = jobs.filter((j) => j.status === "active" && nextOf(j)).sort((a, b) => nextOf(a)!.localeCompare(nextOf(b)!))[0];
    return upcoming ? m.cron_finding_next({ name: upcoming.name, when: formatWhen(nextOf(upcoming)) }) : m.cron_finding_all_paused();
  };

  const select = (id: string) => void navigate({ to: "/cron/$id", params: { id } });
  const close = () => void navigate({ to: "/cron" });

  const detail = current ? (
    <DetailCard label={m.cron_detail()} compact={compact}>
      <CronDetail key={current.id} job={current} status={statuses.get(current.id)} sessions={sessions ?? []} compact={compact} onClose={close} />
    </DetailCard>
  ) : null;

  // 窄屏选中了一个：只给详情
  if (compact && selected && detail) {
    return (
      <Page title={m.nav_cron()} bare>
        {detail}
      </Page>
    );
  }

  return (
    <Page title={m.nav_cron()} finding={ready ? finding() : query.isError ? m.cron_finding_error() : undefined}>
      <PageBody columns={detail && !compact ? "grid-cols-[minmax(0,1fr)_360px]" : "grid-cols-[minmax(0,1fr)]"}>
        <section aria-label={m.cron_list()} className="min-w-0">
          {query.isError ? (
            <BlockError message={m.cron_load_error({ message: errorText(query.error) })} onRetry={() => void query.refetch()} />
          ) : !ready ? (
            <SkeletonRows />
          ) : !jobs.length ? (
            <p className="my-3 rounded-control border border-dashed border-control px-3.5 py-2.5 text-label text-muted">{m.cron_empty()}</p>
          ) : (
            <>
              <div aria-hidden="true" className={cn("grid h-8 items-center gap-x-3 border-b border-border text-small text-muted max-[600px]:hidden", COLUMNS)}>
                <span />
                <span>{m.cron_col_job()}</span>
                <span className="text-right">{m.cron_col_next()}</span>
                <span className="text-right">{m.cron_col_last()}</span>
              </div>
              <ul className="relative z-0">
                {sorted.map((job) => (
                  <li
                    key={job.id}
                    className={cn(
                      "relative grid items-start gap-x-3 gap-y-1.5 border-b border-rule py-3 before:absolute before:inset-x-[-8px] before:inset-y-px before:-z-10 before:rounded-control before:transition-colors hover:before:bg-hover-on-white",
                      COLUMNS,
                      job.id === selected && "before:bg-active hover:before:bg-active",
                    )}
                  >
                    <span
                      aria-hidden="true"
                      className={cn(
                        "mt-1.75 justify-self-center rounded-full",
                        job.status !== "active" ? "size-1.75 bg-control" : job.last_error ? "size-2 bg-negative" : "size-2 bg-text",
                      )}
                    />
                    <button
                      type="button"
                      aria-current={job.id === selected ? "true" : undefined}
                      onClick={() => select(job.id)}
                      className="flex min-w-0 cursor-pointer flex-col gap-1 text-left text-text"
                    >
                      <span className="leading-5.5">{job.name}</span>
                      <span className="text-small text-muted">
                        {scheduleText(job.trigger)}
                        {job.status !== "active" && (
                          <>
                            <span className="mx-1.5 text-faint">·</span>
                            {job.status === "paused" ? m.cron_paused() : m.cron_finished()}
                          </>
                        )}
                      </span>
                    </button>
                    <span className="text-right text-label leading-5.5 text-muted tabular-nums max-[600px]:col-start-2 max-[600px]:row-start-2 max-[600px]:text-left max-[600px]:text-small max-[600px]:leading-4.5">
                      {job.status === "active" && nextOf(job) ? formatWhen(nextOf(job)) : "—"}
                    </span>
                    <span className="justify-self-end max-[600px]:col-start-3 max-[600px]:row-start-1">
                      <FiringPill last={lastOf(job)} />
                    </span>
                  </li>
                ))}
              </ul>
            </>
          )}
        </section>
        {detail && !compact && detail}
      </PageBody>
    </Page>
  );
}

function CronDetail({
  job,
  status,
  sessions,
  compact,
  onClose,
}: {
  job: CronJob;
  status: CronJobStatus | undefined;
  sessions: SessionSummary[];
  compact: boolean;
  onClose: () => void;
}) {
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState<"run" | "act" | null>(null);
  const [fail, setFail] = useState("");
  const [confirming, setConfirming] = useState(false);
  const [ran, setRan] = useState<ManualCronRunResponse | null>(null);

  const last = status?.last;
  const next = status?.next_run_at ?? job.next_run_at;
  const lastSession = last?.session ? sessions.find((x) => x.session === last.session) : undefined;
  const isCommand = job.command != null;

  const act = async (kind: "run" | "toggle" | "delete") => {
    setBusy(kind === "run" ? "run" : "act");
    setFail("");
    setRan(null);
    try {
      if (kind === "run") setRan(await runCron(job.id, `web:cron-run:${crypto.randomUUID()}`));
      else if (kind === "toggle")
        await patchCron(job.id, { status: job.status === "paused" ? "active" : "paused", request_key: `web:cron-status:${job.id}:${crypto.randomUUID()}` });
      else {
        await deleteCron(job.id);
        setConfirming(false);
        onClose();
      }
      await Promise.all([queryClient.invalidateQueries({ queryKey: keys.cron }), queryClient.invalidateQueries({ queryKey: keys.sessions.all })]);
    } catch (error) {
      setFail(m.cron_error_act({ message: errorText(error) }));
    }
    setBusy(null);
  };

  const disabled = busy !== null;
  return (
    <div className={compact ? "pt-1" : "px-4.5 pt-4 pb-4.5"}>
      <DetailTop
        label={isCommand ? m.cron_kind_command() : m.cron_kind_prompt()}
        pill={<Pill tone={job.status === "active" ? undefined : "off"}>{stateLabel(job.status)}</Pill>}
        onClose={onClose}
      />
      <p className="mt-3 text-read leading-6">{job.name}</p>
      <p className="mt-2 text-label leading-5">
        <span className="mr-2 text-muted">{m.cron_when()}</span>
        {scheduleText(job.trigger)}
      </p>
      {job.last_error && <FormError>{m.cron_no_slot({ reason: job.last_error })}</FormError>}
      {fail && <FormError>{fail}</FormError>}
      {ran && (
        <Notice>
          <p>
            {m.cron_triggered_before()}
            <Link to="/sessions/$sessionId" params={{ sessionId: ran.session }} className="font-medium text-text underline underline-offset-3">
              {m.cron_triggered_link()}
            </Link>
            {m.cron_triggered_after()}
          </p>
        </Notice>
      )}
      {job.status !== "done" && (
        <div className="mt-4 flex flex-wrap gap-2">
          <Button variant="primary" disabled={disabled} onClick={() => void act("run")}>
            {busy === "run" ? m.cron_run_busy() : m.cron_run_now()}
          </Button>
          <Button disabled={disabled} onClick={() => void act("toggle")}>
            {job.status === "paused" ? m.cron_resume() : m.cron_pause()}
          </Button>
        </div>
      )}
      <SubHeading>{m.cron_what()}</SubHeading>
      {isCommand ? (
        <pre className="m-0 overflow-x-auto rounded-control bg-raised px-3 py-2.5 font-mono text-[12px] leading-4.5 break-all whitespace-pre-wrap">{job.command}</pre>
      ) : (
        <p className="m-0 text-body leading-5.5 whitespace-pre-wrap">{job.prompt}</p>
      )}
      <Kv
        rows={[
          job.workdir ? [m.cron_kv_workdir(), <span className="font-mono text-small">{job.workdir}</span>] : null,
          [m.cron_kv_next(), job.status === "active" && next ? <span className="tabular-nums">{formatWhen(next)}</span> : <span className="text-muted">—</span>],
          [
            m.cron_kv_last(),
            last ? (
              <>
                <Pill tone={firingTone(last.status)}>{firingLabel(last.status)}</Pill> <span className="tabular-nums">{formatWhen(last.scheduled_at)}</span>
                {last.session && (
                  <>
                    {" · "}
                    <Link to="/sessions/$sessionId" params={{ sessionId: last.session }} className={linkClass}>
                      {lastSession ? sessionTitle(lastSession) : m.cron_last_session()}
                    </Link>
                  </>
                )}
                {last.error && <div className="text-muted">{last.error}</div>}
              </>
            ) : (
              <span className="text-muted">{m.cron_never_ran()}</span>
            ),
          ],
          [m.cron_kv_notify(), notifyLabel(job.notify)],
          [m.cron_kv_overlap(), overlapLabel(job.overlap)],
        ]}
      />
      {confirming ? (
        <div className="mt-3 rounded-control p-3 text-label shadow-[0_0_0_1px_var(--color-negative-ring)]">
          <p>{m.cron_delete_warn()}</p>
          <div className="mt-2.5 flex flex-wrap gap-2">
            <Button variant="danger" disabled={disabled} onClick={() => void act("delete")}>
              {m.cron_delete_confirm({ name: job.name })}
            </Button>
            <Button onClick={() => setConfirming(false)}>{m.cancel()}</Button>
          </div>
        </div>
      ) : (
        <TextButton tone="danger" className="mt-3 underline underline-offset-3" onClick={() => setConfirming(true)}>
          {m.cron_delete()}
        </TextButton>
      )}
    </div>
  );
}
