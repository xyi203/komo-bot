import { actMatter, keys } from "@komo/api";
import type { CronJob, Evidence, Matter, MatterActionRequest, MatterDetail, MatterTrigger, MemoryItem, SessionSummary } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState, type FormEvent, type ReactNode } from "react";
import { MemoryMote } from "@/features/memory/mote";
import { sessionTitle } from "@/features/sessions/title";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { formatDateTime, formatRelative } from "@/lib/format";
import { m } from "@/paraglide/messages.js";
import { dueOf, formatInstantDay, isClosed, statusLabel } from "./dates";
import { ArrowLink, DetailTop, FormError, Hint, Input, Kv, Label, linkClass, Notice, Pill, Select, SubHeading, TextButton } from "./parts";

// 每个状态能做什么（docs/goals.md §4 的表）。
type Verb = "done" | "next" | "wait" | "park" | "edit" | "link" | "drop" | "resume" | "reopen" | "retire" | "unlink";
interface Action {
  verb: Verb;
  label: string;
  /** 要先填字的动作：展开一张小表单，不直接发。 */
  form?: boolean;
  primary?: boolean;
  /** 次要的文字动作。 */
  text?: boolean;
}

function actionsOf(matter: Matter): Action[] {
  if (isClosed(matter)) return [{ verb: "reopen", label: m.matter_act_reopen(), primary: true }];
  const edit: Action = { verb: "edit", label: m.matter_act_edit(), form: true, text: true };
  const link: Action = { verb: "link", label: m.matter_act_link(), form: true, text: true };
  const drop: Action = { verb: "drop", label: m.matter_act_drop(), text: true };
  if (matter.kind === "area")
    return [{ ...edit, label: m.matter_act_edit_area(), text: false }, { ...link, text: false }, { verb: "retire", label: m.matter_act_retire(), text: true }];
  if (matter.state === "active")
    return [
      { verb: "done", label: m.matter_act_done(), primary: true },
      { verb: "next", label: matter.next ? m.matter_act_next_edit() : m.matter_act_next_write(), form: true },
      { verb: "wait", label: m.matter_act_wait(), form: true },
      { verb: "park", label: m.matter_act_park() },
      edit,
      link,
      drop,
    ];
  if (matter.state === "waiting")
    return [
      { verb: "resume", label: m.matter_act_resume_wait(), form: true, primary: true },
      { verb: "wait", label: m.matter_act_wait_edit(), form: true },
      { verb: "done", label: m.matter_act_done() },
      { verb: "park", label: m.matter_act_park() },
      edit,
      link,
      drop,
    ];
  return [{ verb: "resume", label: m.matter_act_resume(), form: true, primary: true }, { verb: "done", label: m.matter_act_done() }, edit, drop];
}

interface Receipt {
  closure: MatterDetail["closure"];
  stillLinked: number;
}

/** 事项详情（docs/web.md §12）：字段、按状态给的动作、触发器、为它记的记忆、版本、用户原话。 */
export function MatterDetailView({
  matter,
  today,
  memories,
  jobs,
  sessions,
  compact,
  onBack,
}: {
  matter: Matter;
  today: string | undefined;
  memories: MemoryItem[];
  jobs: CronJob[];
  sessions: SessionSummary[];
  compact: boolean;
  onBack: () => void;
}) {
  const queryClient = useQueryClient();
  const [form, setForm] = useState<Verb | null>(null);
  const [busy, setBusy] = useState<Verb | null>(null);
  const [fail, setFail] = useState("");
  const [receipt, setReceipt] = useState<Receipt | null>(null);

  const closed = isClosed(matter);
  const due = dueOf(matter, today);
  const triggers = matter.triggers ?? [];
  const evidence = matter.evidence ?? [];

  const run = async (verb: Verb, fields: MatterActionRequest = {}) => {
    setBusy(verb);
    setFail("");
    try {
      const response = await actMatter(matter.id, verb, {
        kind: matter.kind,
        expected_revision: matter.revision,
        request_key: `web:matter:${matter.id}:${matter.revision}:${verb}`,
        ...fields,
      });
      const stillLinked = response.still_linked?.length ?? 0;
      setReceipt(response.closure || stillLinked ? { closure: response.closure, stillLinked } : null);
      setForm(null);
      // 结案 / 重开会改记忆（归档、恢复），一起重取
      await Promise.all([queryClient.invalidateQueries({ queryKey: keys.matters }), queryClient.invalidateQueries({ queryKey: keys.memories })]);
    } catch (error) {
      setFail(m.matter_error_edit({ message: errorText(error) }));
    }
    setBusy(null);
  };

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const data = new FormData(event.currentTarget);
    const v = (key: string) => String(data.get(key) ?? "").trim();
    switch (form) {
      case "next":
        return void run("next", { next: v("next") });
      case "wait":
        return void run("wait", { waiting_for: v("waiting_for") });
      case "resume":
        return void run("resume", v("next") ? { next: v("next") } : {});
      case "link":
        return void run("link", { trigger: { kind: "cron", job: v("job") } });
      case "edit": {
        const fields: MatterActionRequest = { title: v("title") };
        if (matter.kind === "goal") {
          if (v("due")) fields.due = v("due");
          else if (matter.due) fields.clear_due = true;
        } else fields.standard = v("standard");
        return void run("edit", fields);
      }
    }
  };

  const actions = actionsOf(matter);
  const pillTone = matter.state === "waiting" ? "wait" : closed || matter.state === "parked" ? "off" : undefined;
  return (
    <div className={compact ? "pt-1" : "px-4.5 pt-4 pb-4.5"}>
      <DetailTop
        label={matter.kind === "goal" ? m.matter_kind_goal() : m.matter_kind_area()}
        pill={<Pill tone={pillTone}>{statusLabel(matter.state)}</Pill>}
        compact={compact}
        onBack={onBack}
      />
      <p className="mt-3 text-read leading-6">{matter.title}</p>
      <Kv
        rows={
          matter.kind === "goal"
            ? [
                matter.state === "waiting" ? ([m.matter_field_waiting(), matter.waiting_for] as const) : false,
                [m.matter_field_next(), matter.next ?? <span className="text-muted">—</span>],
                [
                  m.matter_field_due(),
                  due ? (
                    <>
                      {due.text}
                      {due.sub && (
                        <>
                          {" · "}
                          <span className={due.bad ? "text-negative" : undefined}>{due.sub}</span>
                        </>
                      )}
                    </>
                  ) : (
                    <span className="text-muted">{m.matter_not_set()}</span>
                  ),
                ],
              ]
            : [[m.matter_field_standard(), matter.standard ?? <span className="text-muted">{m.matter_not_written()}</span>]]
        }
      />
      {fail && <FormError>{fail}</FormError>}
      {receipt && <ReceiptView receipt={receipt} />}
      <div className="mt-5 flex flex-wrap gap-2">
        {actions
          .filter((a) => !a.text)
          .map((a) => (
            <Button
              key={a.verb}
              variant={a.primary ? "primary" : "default"}
              disabled={busy !== null}
              onClick={() => (a.form ? (setForm(form === a.verb ? null : a.verb), setFail("")) : void run(a.verb))}
            >
              {a.label}
            </Button>
          ))}
      </div>
      {actions.some((a) => a.text) && (
        <div className="mt-3 flex flex-wrap gap-x-4 gap-y-1">
          {actions
            .filter((a) => a.text)
            .map((a) => (
              <TextButton
                key={a.verb}
                tone={a.verb === "drop" || a.verb === "retire" ? "danger" : "quiet"}
                disabled={busy !== null}
                onClick={() => (a.form ? (setForm(form === a.verb ? null : a.verb), setFail("")) : void run(a.verb))}
              >
                {a.label}
              </TextButton>
            ))}
        </div>
      )}
      {form && <ActionForm key={form} verb={form} matter={matter} jobs={jobs} busy={busy !== null} onSubmit={submit} onCancel={() => (setForm(null), setFail(""))} />}

      <SubHeading count={triggers.length}>{m.matter_triggers()}</SubHeading>
      {triggers.length ? (
        <ul>
          {triggers.map((trigger, i) => (
            <TriggerRow key={i} trigger={trigger} jobs={jobs} sessions={sessions} canUnlink={!closed} busy={busy !== null} onUnlink={() => void run("unlink", { trigger })} />
          ))}
        </ul>
      ) : (
        <Hint>{m.matter_triggers_empty()}</Hint>
      )}

      <SubHeading count={memories.length}>{m.matter_memories()}</SubHeading>
      {memories.length ? (
        <ul>
          {memories.map((memory) => (
            <li key={memory.id} className="flex items-start gap-2.5 border-b border-rule py-2 text-label last:border-b-0">
              <MemoryMote kind={memory} className="mt-1.5" />
              <Link to="/memory/$id" params={{ id: memory.id }} className={`${linkClass} leading-5`}>
                {memory.content}
              </Link>
            </li>
          ))}
        </ul>
      ) : (
        <Hint>{m.matter_memories_empty()}</Hint>
      )}

      <Kv
        rows={[
          [m.matter_kv_created(), <span className="tabular-nums">{formatInstantDay(matter.created_at)}</span>],
          [m.matter_kv_updated(), <span className="tabular-nums">{formatRelative(matter.updated_at)}</span>],
          matter.closed_at ? [m.matter_kv_closed(), <span className="tabular-nums">{formatInstantDay(matter.closed_at)}</span>] : null,
          [m.matter_kv_revision(), m.matter_revision({ n: matter.revision })],
        ]}
      />

      <SubHeading count={evidence.length}>{m.matter_evidence()}</SubHeading>
      {evidence.length ? (
        <ul>
          {evidence.map((item, i) => (
            <EvidenceRow key={i} evidence={item} sessions={sessions} />
          ))}
        </ul>
      ) : (
        <Hint>{m.matter_evidence_empty()}</Hint>
      )}
    </div>
  );
}

/** 结案 / 重开的回执：归档了几条线索、恢复了几条、还挂着几个触发器、哪些约定还归属它。 */
function ReceiptView({ receipt }: { receipt: Receipt }) {
  const lines: string[] = [];
  if (receipt.closure?.archived) lines.push(m.matter_closure_archived({ count: receipt.closure.archived }));
  if (receipt.closure?.restored) lines.push(m.matter_closure_restored({ count: receipt.closure.restored }));
  if (receipt.stillLinked) lines.push(m.matter_closure_linked({ count: receipt.stillLinked }));
  const conventions = receipt.closure?.conventions ?? [];
  if (!lines.length && !conventions.length) return null;
  return (
    <Notice>
      {lines.map((line) => (
        <p key={line} className="[&+*]:mt-1.5">
          {line}
        </p>
      ))}
      {conventions.length > 0 && (
        <>
          <p className={lines.length ? "mt-1.5" : undefined}>{m.matter_closure_conventions()}</p>
          <ul className="mt-1.5 list-disc pl-4.5">
            {conventions.map((c) => (
              <li key={c.id}>
                <Link to="/memory/$id" params={{ id: c.id }} className={linkClass}>
                  {c.content}
                </Link>
              </li>
            ))}
          </ul>
        </>
      )}
    </Notice>
  );
}

function ActionForm({
  verb,
  matter,
  jobs,
  busy,
  onSubmit,
  onCancel,
}: {
  verb: Verb;
  matter: Matter;
  jobs: CronJob[];
  busy: boolean;
  onSubmit: (event: FormEvent<HTMLFormElement>) => void;
  onCancel: () => void;
}) {
  const buttons = (label: string) => (
    <div className="mt-4 flex flex-wrap gap-2">
      <Button type="submit" variant="primary" disabled={busy}>
        {busy ? m.matter_busy_edit() : label}
      </Button>
      <Button onClick={onCancel}>{m.cancel()}</Button>
    </div>
  );
  const wrap = (children: ReactNode) => (
    <form onSubmit={onSubmit} className="mt-4 border-t border-rule pt-1">
      {children}
    </form>
  );
  if (verb === "next")
    return wrap(
      <>
        <Label htmlFor="mf-next">{m.matter_form_next()}</Label>
        <Input id="mf-next" name="next" defaultValue={matter.next ?? ""} maxLength={300} placeholder={m.matter_form_next_hint()} />
        {buttons(m.matter_save())}
      </>,
    );
  if (verb === "wait")
    return wrap(
      <>
        <Label htmlFor="mf-wait">{m.matter_form_waiting()}</Label>
        <Input id="mf-wait" name="waiting_for" defaultValue={matter.waiting_for ?? ""} maxLength={300} required />
        {buttons(m.matter_submit_wait())}
      </>,
    );
  if (verb === "resume")
    return wrap(
      <>
        <Label htmlFor="mf-resume">{m.matter_form_resume_next()}</Label>
        <Input id="mf-resume" name="next" defaultValue={matter.next ?? ""} maxLength={300} />
        {buttons(m.matter_submit_resume())}
      </>,
    );
  if (verb === "edit")
    return wrap(
      <>
        <Label htmlFor="mf-title">{matter.kind === "goal" ? m.matter_form_title_goal() : m.matter_form_title_area()}</Label>
        <Input id="mf-title" name="title" defaultValue={matter.title} maxLength={80} required />
        {matter.kind === "goal" ? (
          <>
            <Label htmlFor="mf-due">{m.matter_form_due()}</Label>
            <Input id="mf-due" name="due" type="date" defaultValue={matter.due ?? ""} />
            <Hint>{m.matter_form_due_hint()}</Hint>
          </>
        ) : (
          <>
            <Label htmlFor="mf-standard">{m.matter_form_standard()}</Label>
            <Input id="mf-standard" name="standard" defaultValue={matter.standard ?? ""} maxLength={300} placeholder={m.matter_form_standard_hint()} />
          </>
        )}
        {buttons(m.matter_save())}
      </>,
    );
  // link：只列还没挂着的定时任务
  const linked = new Set(matter.triggers?.flatMap((t) => (t.kind === "cron" ? [t.job] : [])));
  const free = jobs.filter((job) => !linked.has(job.id));
  if (!free.length)
    return (
      <Hint className="mt-4">
        {m.matter_no_jobs_before()}
        <Link to="/cron" className="font-medium text-text underline underline-offset-3">
          {m.nav_cron()}
        </Link>
        {m.matter_no_jobs_after()}
      </Hint>
    );
  return wrap(
    <>
      <Label htmlFor="mf-job">{m.matter_form_job()}</Label>
      <Select id="mf-job" name="job">
        {free.map((job) => (
          <option key={job.id} value={job.id}>
            {job.name}
          </option>
        ))}
      </Select>
      {buttons(m.matter_submit_link())}
    </>,
  );
}

function TriggerRow({
  trigger,
  jobs,
  sessions,
  canUnlink,
  busy,
  onUnlink,
}: {
  trigger: MatterTrigger;
  jobs: CronJob[];
  sessions: SessionSummary[];
  canUnlink: boolean;
  busy: boolean;
  onUnlink: () => void;
}) {
  const unlink = canUnlink && (
    <TextButton tone="quiet" disabled={busy} onClick={onUnlink} className="ml-auto">
      {m.matter_unlink()}
    </TextButton>
  );
  if (trigger.kind === "cron") {
    const job = jobs.find((x) => x.id === trigger.job);
    return (
      <li className="flex flex-col gap-0.5 border-b border-rule py-2 text-label last:border-b-0">
        <span className="text-muted">{m.matter_trigger_cron()}</span>
        <span className="flex min-w-0 items-center gap-2.5">
          {job ? (
            <Link to="/cron/$id" params={{ id: job.id }} className={linkClass}>
              <ArrowLink>{job.name}</ArrowLink>
            </Link>
          ) : (
            <span className="text-muted">{m.matter_trigger_cron_gone()}</span>
          )}
          {unlink}
        </span>
      </li>
    );
  }
  const session = sessions.find((x) => x.session === trigger.session);
  return (
    <li className="flex flex-col gap-0.5 border-b border-rule py-2 text-label last:border-b-0">
      <span className="text-muted">
        monitor <span className="tabular-nums">#{trigger.monitor}</span>
      </span>
      <span className="flex min-w-0 items-center gap-2.5">
        {session ? (
          <Link to="/sessions/$sessionId" params={{ sessionId: trigger.session }} className={linkClass}>
            <ArrowLink>{sessionTitle(session)}</ArrowLink>
          </Link>
        ) : (
          <span className="text-muted">{m.matter_trigger_session_gone()}</span>
        )}
        {unlink}
      </span>
    </li>
  );
}

const provenanceLabel = (p: Evidence["provenance"]) =>
  ({ user_statement: m.matter_provenance_user_statement, tool_observation: m.matter_provenance_tool_observation, model_inference: m.matter_provenance_model_inference })[p]();

function EvidenceRow({ evidence, sessions }: { evidence: Evidence; sessions: SessionSummary[] }) {
  const ref = evidence.reference;
  const when = formatDateTime(evidence.observed_at);
  if (ref.kind === "event") {
    const session = sessions.find((x) => x.session === ref.session);
    return (
      <li className="flex flex-col gap-0.5 border-b border-rule py-2 text-label last:border-b-0">
        <span className="text-muted">
          {provenanceLabel(evidence.provenance)}
          <span className="mx-1.5 text-faint">·</span>
          <span className="tabular-nums">{when}</span>
        </span>
        {session ? (
          <Link to="/sessions/$sessionId" params={{ sessionId: ref.session }} className={linkClass}>
            <ArrowLink>{sessionTitle(session)}</ArrowLink>
          </Link>
        ) : (
          <span className="text-muted">{m.matter_trigger_session_gone()}</span>
        )}
      </li>
    );
  }
  return (
    <li className="flex flex-col gap-0.5 border-b border-rule py-2 text-label last:border-b-0">
      <span className="text-muted">
        {m.matter_evidence_memos()}
        <span className="mx-1.5 text-faint">·</span>
        <span className="tabular-nums">{when}</span>
      </span>
      <span className="text-muted">
        {ref.instance} / {ref.record_id}
      </span>
    </li>
  );
}
