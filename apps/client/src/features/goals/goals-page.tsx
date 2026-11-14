import { actMatter, keys, useChats, useHomeSession, useMatterList, uuid } from "@komo/api";
import type { ChatRow, Matter, MatterSchedule } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useMemo, useState } from "react";
import { Icon } from "@/components/icon";
import { BlockError, EmptyFrame, Page, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from "@/components/ui/dropdown-menu";
import { presetDraft } from "@/features/composer/draft";
import { errorText } from "@/lib/errors";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { GoalNewDialog } from "./goal-new";
import {
  dayDiff,
  dueOf,
  formatInstantDay,
  goalSub,
  isOngoing,
  isTracked,
  nextRunText,
  scheduleOn,
  scheduleSummary,
  serverText,
  statusLabel,
} from "./model";
import { ItemBody, itemClass, listClass, Pill, rowClass, SectionTitle } from "./parts";

/** 追踪中的排序：超期的在前，再按截止日期（没有的排后面），最后按建的先后。 */
function trackedOrder(today: string | undefined) {
  return (a: Matter, b: Matter) => {
    const da = dueOf(a, today);
    const db = dueOf(b, today);
    return (
      Number(db?.bad ?? 0) - Number(da?.bad ?? 0) ||
      Number(!a.due) - Number(!b.due) ||
      (a.due && b.due ? a.due.localeCompare(b.due) : 0) ||
      a.created_at.localeCompare(b.created_at)
    );
  };
}

/** 页头下的一句话：超期 > 在等你回话 > 数目 > 空。 */
function finding(tracked: Matter[], asking: number, today: string | undefined): string {
  const late = tracked.filter((x) => dueOf(x, today)?.bad);
  if (late.length === 1 && late[0].due && today) return m.matter_finding_late_one({ title: late[0].title, days: -dayDiff(late[0].due, today) });
  if (late.length) return m.matter_finding_late_many({ count: late.length, title: late[0].title });
  if (asking) return m.goal_finding_asking({ count: asking });
  if (tracked.length) return m.goal_finding_counts({ count: tracked.length });
  return m.goal_finding_none();
}

const ROW = "grid grid-cols-[20px_minmax(0,1fr)_auto] items-start gap-x-3 max-[600px]:grid-cols-[20px_minmax(0,1fr)]";

/**
 * 目标页（docs/web.md §12）：一节「追踪中」，右上角「…」里可以展开「不再追踪」（搁置、完成、放弃）；点一个进它的详情页。
 * 新建走弹窗，也可以让 Komo 在主对话里帮着定。
 */
export function GoalsPage() {
  const query = useMatterList();
  const chats = useChats().data?.chats;
  const home = useHomeSession();
  const navigate = useNavigate();
  const [creating, setCreating] = useState(false);
  const [showRest, setShowRest] = useState(false);

  const matters = useMemo(() => query.data?.matters ?? [], [query.data]);
  const today = query.data?.today;
  const schedules = useMemo(() => new Map((query.data?.schedules ?? []).map((s): [string, MatterSchedule] => [s.job.id, s])), [query.data]);
  // 绑定了支线的目标：从对话列的行上反查（`ChatRow.goal`）
  const bound = useMemo(() => new Map((chats ?? []).flatMap((chat): [string, ChatRow][] => (chat.goal ? [[chat.goal.id, chat]] : []))), [chats]);
  const tracked = useMemo(() => matters.filter(isTracked).sort(trackedOrder(today)), [matters, today]);
  const rest = useMemo(() => matters.filter((x) => !isTracked(x)).sort((a, b) => (b.closed_at ?? b.updated_at).localeCompare(a.closed_at ?? a.updated_at)), [matters]);
  const asking = tracked.filter((x) => bound.get(x.id)?.waiting).length;
  const ready = query.isSuccess;

  const draft = () => {
    if (home.data) presetDraft(home.data.session, m.goal_draft());
    void navigate({ to: "/" });
  };

  const actions = (
    <div className="flex items-center gap-1.5">
      <Button onClick={() => setCreating(true)}>
        <Icon name="plus" className="size-4" />
        {m.matter_new_button()}
      </Button>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <button
            type="button"
            aria-label={m.goal_more()}
            title={m.goal_more()}
            className="grid size-8 cursor-pointer place-items-center rounded-control text-muted transition-colors hover:bg-hover-on-white hover:text-text data-[state=open]:bg-hover-on-white data-[state=open]:text-text"
          >
            <Icon name="more" />
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" size="compact">
          <DropdownMenuItem size="compact" onSelect={() => setShowRest((on) => !on)}>
            {showRest ? m.goal_hide_rest() : m.goal_show_rest()}
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );

  return (
    <Page title={m.nav_goals()} finding={ready ? finding(tracked, asking, today) : query.isError ? m.goal_finding_error() : undefined} actions={actions}>
      <div className="max-w-190">
        {query.isError ? (
          <BlockError message={m.goal_list_error({ message: errorText(query.error) })} onRetry={() => void query.refetch()} />
        ) : !ready ? (
          <SkeletonRows />
        ) : (
          <>
            <section aria-label={m.goal_tracking()}>
              <SectionTitle count={tracked.length}>{m.goal_tracking()}</SectionTitle>
              {tracked.length ? (
                <ul className={listClass}>
                  {tracked.map((matter) => (
                    <Row key={matter.id} matter={matter} today={today} schedule={schedules.get(matter.id)} chat={bound.get(matter.id)} />
                  ))}
                </ul>
              ) : (
                <EmptyFrame className="mt-2">{m.goal_empty()}</EmptyFrame>
              )}
            </section>
            {showRest && (
              <section aria-label={m.goal_untracked()} className="mt-9">
                <SectionTitle count={rest.length}>{m.goal_untracked()}</SectionTitle>
                {rest.length ? (
                  <ul className={listClass}>
                    {rest.map((matter) => (
                      <Row key={matter.id} matter={matter} today={today} schedule={schedules.get(matter.id)} chat={bound.get(matter.id)} />
                    ))}
                  </ul>
                ) : (
                  <EmptyFrame className="mt-2">{m.goal_untracked_empty()}</EmptyFrame>
                )}
              </section>
            )}
            <section aria-label={m.goal_start()} className="mt-9">
              <SectionTitle>{m.goal_start()}</SectionTitle>
              <button type="button" onClick={draft} className={cn(itemClass, "mt-1 cursor-pointer")}>
                <ItemBody icon={<Icon name="chat" />} title={m.goal_start_chat()} desc={m.goal_start_chat_hint()} end={<Icon name="right" className="size-3.5" />} />
              </button>
            </section>
          </>
        )}
      </div>
      <GoalNewDialog open={creating} onOpenChange={setCreating} />
    </Page>
  );
}

function Row({ matter, today, schedule, chat }: { matter: Matter; today: string | undefined; schedule: MatterSchedule | undefined; chat: ChatRow | undefined }) {
  const tracked = isTracked(matter);
  const sub = goalSub(matter);
  // 右边一小段：有定时是节奏与下一次，否则是截止；不再追踪的只写状态
  const due = dueOf(matter, today);
  const next = schedule && tracked ? nextRunText(schedule) : null;
  const meta = !tracked
    ? { text: "", sub: matter.closed_at ? m.matter_closed_on({ date: formatInstantDay(matter.closed_at) }) : "", bad: false }
    : schedule
      ? { text: scheduleSummary(schedule), sub: !scheduleOn(schedule) ? m.cron_paused() : next ? m.goal_schedule_next({ when: next }) : "", bad: false }
      : due
        ? { text: due.text, sub: due.sub, bad: due.bad }
        : null;
  return (
    <li className={cn(rowClass, ROW)}>
      <Mark matter={matter} />
      <Link to="/goals/$id" params={{ id: matter.id }} className="flex min-w-0 flex-col gap-1 text-text no-underline">
        <span className="flex flex-wrap items-center gap-x-2 gap-y-1 leading-5.5">
          <span className="[overflow-wrap:anywhere]">{matter.title}</span>
          {chat && (
            <span title={m.goal_bound({ title: chat.title || m.session_untitled() })} className="inline-grid text-faint">
              <Icon name="flag" className="size-3.5" />
              <span className="sr-only">{m.goal_bound({ title: chat.title || m.session_untitled() })}</span>
            </span>
          )}
          {chat?.waiting && <Pill tone="wait">{m.goal_asking()}</Pill>}
          {!tracked && <Pill tone="off">{statusLabel(matter.state)}</Pill>}
        </span>
        {sub && <span className="line-clamp-2 text-small text-muted [overflow-wrap:anywhere]">{sub}</span>}
      </Link>
      {meta && (meta.text || meta.sub) && (
        <span className="flex flex-col items-end leading-5.5 tabular-nums max-[600px]:col-start-2 max-[600px]:flex-row max-[600px]:items-baseline max-[600px]:gap-2 max-[600px]:text-small max-[600px]:leading-4.5 max-[600px]:text-muted">
          {meta.text}
          {meta.sub && <small className={cn("mt-1 text-small leading-4.5 max-[600px]:mt-0", meta.bad ? "text-negative" : "text-muted")}>{meta.sub}</small>}
        </span>
      )}
    </li>
  );
}

/** 行首的标记：没有终点的是循环箭头；有终点、追踪中的是勾选框（点一下完成）；不再追踪的是静态的小标。 */
function Mark({ matter }: { matter: Matter }) {
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);
  if (isOngoing(matter))
    return (
      <span title={m.goal_ongoing()} className="mt-0.5 grid size-5 place-items-center text-faint">
        <Icon name="loop" className="size-4.5" />
        <span className="sr-only">{m.goal_ongoing()}</span>
      </span>
    );
  if (!isTracked(matter))
    return <span aria-hidden="true" className="mt-0.5 grid size-5 place-items-center text-faint">{matter.state === "done" ? <Icon name="check" /> : <span className="size-2 rounded-full bg-control" />}</span>;
  const complete = async () => {
    setBusy(true);
    try {
      await actMatter(matter.id, "done", { kind: matter.kind, expected_revision: matter.revision, request_key: `web:matter:${matter.id}:done:${uuid()}` });
      await Promise.all([queryClient.invalidateQueries({ queryKey: keys.matters }), queryClient.invalidateQueries({ queryKey: keys.memories })]);
    } catch (error) {
      showToast(m.matter_error_edit({ message: serverText(error) }));
      setBusy(false);
    }
  };
  return (
    <button
      type="button"
      disabled={busy}
      aria-label={m.goal_complete({ title: matter.title })}
      title={m.matter_act_done()}
      onClick={() => void complete()}
      className="mt-0.5 grid size-5 cursor-pointer place-items-center rounded-[6px] text-transparent shadow-[inset_0_0_0_1.5px_var(--color-control)] transition-colors hover:text-muted hover:shadow-[inset_0_0_0_1.5px_var(--color-muted)] disabled:cursor-progress"
    >
      <Icon name="check" className="size-3.5 stroke-2" />
    </button>
  );
}
