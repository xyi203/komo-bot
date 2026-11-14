import { actMatter, bindMatterChat, isApiError, keys, unbindMatterChat, useHomeSession, useMatter, uuid } from "@komo/api";
import type { Matter, MatterActionRequest, MatterActivity, MatterDetail, MonitorSummary } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState, type FormEvent, type ReactNode } from "react";
import { Icon } from "@/components/icon";
import { BlockError, EmptyFrame, Page, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { presetDraft } from "@/features/composer/draft";
import { MemoryMote } from "@/features/memory/mote";
import { errorText } from "@/lib/errors";
import { formatRelative } from "@/lib/format";
import { m } from "@/paraglide/messages.js";
import { dueOf, formatInstantDay, formatWhen, isClosed, isOngoing, isTracked, serverText, statusLabel, taskLabel } from "./model";
import { ArrowLink, FormError, Hint, Input, ItemBody, itemClass, Kv, Label, linkClass, listClass, Notice, Pill, Section, Textarea, TextButton } from "./parts";
import { ScheduleRow } from "./schedule";

// 每个状态能做什么（docs/goals.md §4 的表；「不再追踪」对有终点的目标是搁置 `park`，对没有终点的是 `retire`，docs/redesign.md §1.4）。
type Verb = "done" | "next" | "wait" | "park" | "edit" | "drop" | "resume" | "reopen" | "retire" | "unlink";
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
  const retrack: Action = { verb: matter.state === "parked" ? "resume" : "reopen", label: m.goal_act_retrack(), primary: true };
  if (isClosed(matter)) return [retrack];
  const edit: Action = { verb: "edit", label: isOngoing(matter) ? m.matter_act_edit_area() : m.matter_act_edit(), form: true, text: true };
  if (isOngoing(matter)) return [edit, { verb: "retire", label: m.goal_act_untrack(), text: true }];
  const drop: Action = { verb: "drop", label: m.matter_act_drop(), text: true };
  const untrack: Action = { verb: "park", label: m.goal_act_untrack(), text: true };
  if (matter.state === "active")
    return [
      { verb: "done", label: m.matter_act_done(), primary: true },
      { verb: "next", label: matter.next ? m.matter_act_next_edit() : m.matter_act_next_write(), form: true },
      { verb: "wait", label: m.matter_act_wait(), form: true },
      edit,
      untrack,
      drop,
    ];
  if (matter.state === "waiting")
    return [
      { verb: "resume", label: m.matter_act_resume_wait(), form: true, primary: true },
      { verb: "wait", label: m.matter_act_wait_edit(), form: true },
      { verb: "done", label: m.matter_act_done() },
      edit,
      untrack,
      drop,
    ];
  // 搁置着的目标
  return [retrack, { verb: "done", label: m.matter_act_done() }, edit, drop];
}

interface Receipt {
  closure: MatterDetail["closure"];
  stillLinked: number;
}

const Back = () => (
  <Link to="/goals" className="-ml-1.5 inline-flex h-7 items-center gap-1 rounded-control px-1.5 text-label text-muted no-underline hover:bg-hover-on-white hover:text-text">
    <Icon name="left" className="size-3.5" />
    {m.nav_goals()}
  </Link>
);

/** 目标详情页（docs/web.md §12）：`/goals/$id`。 */
export function GoalDetailPage({ id }: { id: string }) {
  const query = useMatter(id);
  const gone = query.isError && isApiError(query.error) && query.error.code === "not_found";
  return (
    <Page title={m.nav_goals()} bare>
      <article className="max-w-160">
        <Back />
        {query.data ? (
          <GoalDetail key={query.data.matter.id} detail={query.data} />
        ) : gone ? (
          <EmptyFrame className="mt-4">{m.goal_gone()}</EmptyFrame>
        ) : query.isError ? (
          <BlockError message={m.goal_load_error({ message: errorText(query.error) })} onRetry={() => void query.refetch()} />
        ) : (
          <div className="pt-4">
            <SkeletonRows count={4} className="h-9" />
          </div>
        )}
      </article>
    </Page>
  );
}

function GoalDetail({ detail }: { detail: MatterDetail }) {
  const { matter, schedule, overview, today } = detail;
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const home = useHomeSession();
  const [form, setForm] = useState<Verb | null>(null);
  const [editingDescription, setEditingDescription] = useState(false);
  const [busy, setBusy] = useState<Verb | "chat" | null>(null);
  const [fail, setFail] = useState("");
  const [receipt, setReceipt] = useState<Receipt | null>(null);

  const closed = isClosed(matter);
  const tracked = isTracked(matter);
  const ongoing = isOngoing(matter);
  const due = dueOf(matter, today);
  const chat = overview?.chat;
  const monitors = overview?.monitors ?? [];
  const activity = overview?.activity ?? [];
  const memories = overview?.memories ?? [];

  const refresh = () => Promise.all([queryClient.invalidateQueries({ queryKey: keys.matters }), queryClient.invalidateQueries({ queryKey: keys.chats })]);

  /** 发一个动作；成功回 true。结案 / 重开会改记忆（归档、恢复），一起重取。 */
  const run = async (verb: Verb, fields: MatterActionRequest = {}): Promise<boolean> => {
    setBusy(verb);
    setFail("");
    try {
      const response = await actMatter(matter.id, verb, {
        kind: matter.kind,
        expected_revision: matter.revision,
        request_key: `web:matter:${matter.id}:${verb}:${uuid()}`,
        ...fields,
      });
      const stillLinked = response.still_linked?.length ?? 0;
      setReceipt(response.closure || stillLinked ? { closure: response.closure, stillLinked } : null);
      setForm(null);
      await Promise.all([refresh(), queryClient.invalidateQueries({ queryKey: keys.memories })]);
      setBusy(null);
      return true;
    } catch (error) {
      setFail(m.matter_error_edit({ message: serverText(error) }));
      setBusy(null);
      return false;
    }
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
      case "edit": {
        const fields: MatterActionRequest = { title: v("title") };
        if (!ongoing) {
          if (v("due")) fields.due = v("due");
          else if (matter.due) fields.clear_due = true;
        } else fields.standard = v("standard");
        return void run("edit", fields);
      }
    }
  };

  const openChat = () => {
    if (chat) return void navigate({ to: "/chats/$id", params: { id: chat.session } });
    // 没绑定：去主对话，输入框里预填一句话
    if (home.data) presetDraft(home.data.session, m.goal_chat_draft({ title: matter.title }));
    void navigate({ to: "/" });
  };

  const bind = async () => {
    setBusy("chat");
    setFail("");
    try {
      const response = await bindMatterChat(matter.id);
      await refresh();
      if (response.chat) void navigate({ to: "/chats/$id", params: { id: response.chat.session } });
    } catch (error) {
      setFail(m.goal_chat_error({ message: serverText(error) }));
    }
    setBusy(null);
  };

  const unbind = async () => {
    setBusy("chat");
    setFail("");
    try {
      await unbindMatterChat(matter.id);
      await refresh();
    } catch (error) {
      setFail(m.goal_chat_error({ message: serverText(error) }));
    }
    setBusy(null);
  };

  const actions = actionsOf(matter);
  const kv: ([string, ReactNode] | false)[] = ongoing
    ? [matter.standard ? [m.matter_field_standard(), matter.standard] : false]
    : [
        matter.state === "waiting" && !!matter.waiting_for && [m.matter_field_waiting(), matter.waiting_for],
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
      ];

  return (
    <>
      <h1 className="mt-3 text-page-title font-semibold tracking-tight [overflow-wrap:anywhere]">{matter.title}</h1>
      <p className="mt-1.5 flex flex-wrap items-center gap-x-2.5 gap-y-1 text-label text-muted">
        <Pill tone={matter.state === "waiting" ? "wait" : tracked ? undefined : "off"}>{tracked ? m.goal_tracking() : statusLabel(matter.state)}</Pill>
        {ongoing && m.goal_ongoing()}
      </p>

      {editingDescription ? (
        <DescriptionForm
          initial={matter.description ?? ""}
          busy={busy === "edit"}
          onCancel={() => setEditingDescription(false)}
          onSave={async (text) => {
            if (await run("edit", { description: text })) setEditingDescription(false);
          }}
        />
      ) : (
        <div className="mt-4">
          {matter.description ? <p className="m-0 text-read leading-6 whitespace-pre-wrap [overflow-wrap:anywhere]">{matter.description}</p> : <p className="m-0 text-label text-muted">{m.goal_description_empty()}</p>}
          <TextButton tone="quiet" onClick={() => setEditingDescription(true)} className="mt-1.5">
            {m.goal_description_edit()}
          </TextButton>
        </div>
      )}

      <Kv rows={kv} />
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
        {!closed && (
          <Button disabled={busy !== null} onClick={openChat}>
            {m.goal_chat_talk()}
          </Button>
        )}
      </div>
      {actions.some((a) => a.text) && (
        <div className="mt-3 flex flex-wrap gap-x-4 gap-y-1">
          {actions
            .filter((a) => a.text)
            .map((a) => (
              <TextButton
                key={a.verb}
                tone={a.verb === "drop" ? "danger" : "quiet"}
                disabled={busy !== null}
                onClick={() => (a.form ? (setForm(form === a.verb ? null : a.verb), setFail("")) : void run(a.verb))}
              >
                {a.label}
              </TextButton>
            ))}
        </div>
      )}
      {form && <ActionForm key={form} verb={form} matter={matter} busy={busy !== null} onSubmit={submit} onCancel={() => (setForm(null), setFail(""))} />}

      <Section
        title={m.goal_chat()}
        action={
          chat && (
            <TextButton tone="quiet" disabled={busy !== null} onClick={() => void unbind()}>
              {m.goal_chat_unbind()}
            </TextButton>
          )
        }
      >
        {chat ? (
          <div className={listClass}>
            <Link to="/chats/$id" params={{ id: chat.session }} className={itemClass}>
              <ItemBody icon={<Icon name="chat" />} title={chat.title || m.session_untitled()} end={<Icon name="right" className="size-3.5" />} desc={m.goal_chat_bound_hint()} />
            </Link>
          </div>
        ) : (
          <button type="button" disabled={closed || busy !== null} onClick={() => void bind()} className={`${itemClass} cursor-pointer disabled:cursor-default`}>
            <ItemBody
              icon={<Icon name="plus" />}
              title={m.goal_chat_new()}
              end={!closed && <Icon name="right" className="size-3.5" />}
              desc={closed ? m.goal_chat_closed() : m.goal_chat_new_hint()}
            />
          </button>
        )}
      </Section>

      <Section title={m.goal_schedule()}>
        <ScheduleRow matter={matter} schedule={schedule} />
      </Section>

      {monitors.length > 0 && (
        <Section title={m.goal_monitors()} count={monitors.length}>
          <div className={listClass}>
            {monitors.map((monitor) => (
              <MonitorRow key={`${monitor.session ?? ""}:${monitor.handle}`} matter={matter} monitor={monitor} busy={busy !== null} onUnlink={(trigger) => void run("unlink", { trigger })} />
            ))}
          </div>
        </Section>
      )}

      <Section title={m.goal_activity()} count={activity.length || undefined}>
        {activity.length ? (
          <div className={listClass}>
            {activity.map((item) => (
              <ActivityRow key={item.task.session} item={item} />
            ))}
          </div>
        ) : (
          <Hint>{m.goal_activity_empty()}</Hint>
        )}
      </Section>

      <Section title={m.matter_memories()} count={memories.length || undefined}>
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
          <Hint>{m.goal_memories_empty()}</Hint>
        )}
      </Section>

      <Kv
        className="mt-9"
        rows={[
          [m.matter_kv_created(), <span className="tabular-nums">{formatInstantDay(matter.created_at)}</span>],
          [m.matter_kv_updated(), <span className="tabular-nums">{formatRelative(matter.updated_at)}</span>],
          matter.closed_at ? [m.matter_kv_closed(), <span className="tabular-nums">{formatInstantDay(matter.closed_at)}</span>] : null,
        ]}
      />
    </>
  );
}

function DescriptionForm({ initial, busy, onSave, onCancel }: { initial: string; busy: boolean; onSave: (text: string) => void; onCancel: () => void }) {
  const [text, setText] = useState(initial);
  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        onSave(text.trim());
      }}
      className="mt-4"
    >
      <Label htmlFor="gd-desc">{m.goal_description()}</Label>
      <Textarea id="gd-desc" autoFocus maxLength={8000} value={text} onChange={(event) => setText(event.target.value)} placeholder={m.goal_new_description_hint()} className="min-h-28" />
      <Hint className="mt-1.5">{m.goal_description_hint()}</Hint>
      <div className="mt-3 flex flex-wrap gap-2">
        <Button type="submit" variant="primary" disabled={busy || text.trim() === initial.trim()}>
          {busy ? m.matter_busy_edit() : m.matter_save()}
        </Button>
        <Button onClick={onCancel}>{m.cancel()}</Button>
      </div>
    </form>
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
  busy,
  onSubmit,
  onCancel,
}: {
  verb: Verb;
  matter: Matter;
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
        <Label htmlFor="mf-title">{isOngoing(matter) ? m.matter_form_title_area() : m.matter_form_title_goal()}</Label>
        <Input id="mf-title" name="title" defaultValue={matter.title} maxLength={80} required />
        {isOngoing(matter) ? (
          <>
            <Label htmlFor="mf-standard">{m.matter_form_standard()}</Label>
            <Input id="mf-standard" name="standard" defaultValue={matter.standard ?? ""} maxLength={300} placeholder={m.matter_form_standard_hint()} />
          </>
        ) : (
          <>
            <Label htmlFor="mf-due">{m.matter_form_due()}</Label>
            <Input id="mf-due" name="due" type="date" defaultValue={matter.due ?? ""} />
            <Hint>{m.matter_form_due_hint()}</Hint>
          </>
        )}
        {buttons(m.matter_save())}
      </>,
    );
  return null;
}

function MonitorRow({
  matter,
  monitor,
  busy,
  onUnlink,
}: {
  matter: Matter;
  monitor: MonitorSummary;
  busy: boolean;
  onUnlink: (trigger: NonNullable<MatterActionRequest["trigger"]>) => void;
}) {
  const ended = monitor.state === "ended";
  const trigger = (matter.triggers ?? []).find(
    (t) => t.kind === "monitor" && t.session === monitor.session && t.monitor === monitor.handle.replace(/^#/, ""),
  );
  const title = monitor.title || monitor.command;
  return (
    <div className={itemClass}>
      <ItemBody
        icon={<Icon name="eye" />}
        title={
          monitor.session ? (
            <Link to="/sessions/$sessionId" params={{ sessionId: monitor.session }} className={linkClass}>
              <ArrowLink>{title}</ArrowLink>
            </Link>
          ) : (
            title
          )
        }
        end={
          <>
            {ended ? m.goal_monitor_ended() : m.goal_monitor_running()}
            {trigger && !isClosed(matter) && (
              <TextButton tone="quiet" disabled={busy} onClick={() => onUnlink(trigger)} className="ml-2">
                {m.matter_unlink()}
              </TextButton>
            )}
          </>
        }
        desc={ended && monitor.reason ? monitor.reason : undefined}
      />
    </div>
  );
}

/** 活动里一件事的标记：在跑橙点、在等你琥珀圈、卡住 / 没做完红点、做完一个勾、其余灰点。 */
function ActivityMark({ item }: { item: MatterActivity }) {
  const { task, waiting } = item;
  if (waiting) return <span aria-hidden="true" className="size-2.5 rounded-full border-[1.5px] border-wait-dot" />;
  if (task.state === "running") return <span aria-hidden="true" className="size-2.5 animate-breathe rounded-full bg-accent" />;
  if (task.outcome === "blocked" || task.outcome === "not_completed") return <span aria-hidden="true" className="size-2.5 rounded-full bg-negative" />;
  if (task.outcome === "done") return <Icon name="check" className="size-4" />;
  return <span aria-hidden="true" className="size-2 rounded-full bg-control" />;
}

/** 活动的一行：点进对应会话看过程；在等你的去问话所在的那段对话。 */
function ActivityRow({ item }: { item: MatterActivity }) {
  const { task, waiting } = item;
  const target = waiting?.chat ?? task.session;
  return (
    <Link to="/sessions/$sessionId" params={{ sessionId: target }} className={itemClass}>
      <ItemBody
        icon={<ActivityMark item={item} />}
        title={
          <>
            {task.title || m.session_untitled()}
            {waiting && <Pill tone="wait">{m.goal_task_waiting()}</Pill>}
          </>
        }
        end={<span className="tabular-nums">{formatWhen(task.started_at)}</span>}
        desc={taskLabel(task, waiting !== undefined)}
      />
    </Link>
  );
}
