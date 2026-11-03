import { useCronList, useMatterList, useOwnedMemories, useSessions } from "@komo/api";
import type { Matter, MemoryItem } from "@komo/protocol";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useMemo, useState } from "react";
import { Icon } from "@/components/icon";
import { BlockError, DetailCard, EmptyFrame, Page, PageBody, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { MemoryMote } from "@/features/memory/mote";
import { errorText } from "@/lib/errors";
import { useMediaQuery } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { dayDiff, dueOf, formatInstantDay, isClosed, statusLabel } from "./dates";
import { MatterDetailView } from "./matter-detail";
import { NewMatterForm } from "./matter-new";
import { Pill, TextButton } from "./parts";

/** 一页里的事项分组：目标、领域、搁置、已结案（`docs/web.md` §12）。 */
function groupMatters(items: Matter[], today: string | undefined) {
  const order = (a: Matter, b: Matter) => {
    const da = dueOf(a, today);
    const db = dueOf(b, today);
    return (
      Number(db?.bad ?? 0) - Number(da?.bad ?? 0) ||
      Number(!a.due) - Number(!b.due) ||
      (a.due && b.due ? a.due.localeCompare(b.due) : 0) ||
      a.created_at.localeCompare(b.created_at)
    );
  };
  const open = items.filter((x) => !isClosed(x));
  return {
    open,
    goals: open.filter((x) => x.kind === "goal" && x.state !== "parked").sort(order),
    areas: open.filter((x) => x.kind === "area").sort((a, b) => a.created_at.localeCompare(b.created_at)),
    parked: open.filter((x) => x.state === "parked").sort(order),
    closed: items.filter(isClosed).sort((a, b) => (b.closed_at ?? "").localeCompare(a.closed_at ?? "")),
  };
}

/** 发现一句，按轻重取一条：超期 > 在等 > 数目 > 空。 */
function finding(open: Matter[], today: string | undefined): string {
  const goals = open.filter((x) => x.kind === "goal" && x.state !== "parked");
  const areas = open.filter((x) => x.kind === "area");
  const late = goals.filter((x) => dueOf(x, today)?.bad);
  const waiting = goals.filter((x) => x.state === "waiting");
  if (late.length === 1 && late[0].due && today) return m.matter_finding_late_one({ title: late[0].title, days: -dayDiff(late[0].due, today) });
  if (late.length) return m.matter_finding_late_many({ count: late.length, title: late[0].title });
  if (waiting.length) {
    const what = new Intl.ListFormat(undefined, { type: "conjunction", style: "narrow" }).format(waiting.map((x) => x.waiting_for ?? ""));
    return m.matter_finding_waiting({ count: waiting.length, what });
  }
  if (goals.length || areas.length) return m.matter_finding_counts({ goals: goals.length, areas: areas.length });
  return m.matter_finding_none();
}

const COLUMNS = "grid-cols-[16px_minmax(0,1fr)_104px_84px_76px] max-[600px]:grid-cols-[16px_minmax(0,1fr)_auto]";

/**
 * 事项页（docs/web.md §12）：目标与领域，列表在左、详情在右；选中的 id 在地址里（`/matters/$id`），
 * 窄屏（≤1180）详情盖住列表。"新建"表单占详情的位置，不进地址。
 */
export function MattersPage({ selected }: { selected: string | null }) {
  const query = useMatterList();
  const memories = useOwnedMemories().data;
  const cron = useCronList().data;
  const sessions = useSessions(true).data;
  const navigate = useNavigate();
  const compact = useMediaQuery("(max-width: 1180px)");
  const [creating, setCreating] = useState(false);
  const [showClosed, setShowClosed] = useState(false);

  const items = useMemo(() => query.data?.matters ?? [], [query.data]);
  const today = query.data?.today;
  const ready = query.isSuccess;
  const groups = useMemo(() => groupMatters(items, today), [items, today]);
  const current = items.find((x) => x.id === selected);

  const ownedBy = useMemo(() => {
    const byMatter = new Map<string, MemoryItem[]>();
    for (const memory of memories ?? []) {
      if (!memory.belongs_to) continue;
      byMatter.set(memory.belongs_to, [...(byMatter.get(memory.belongs_to) ?? []), memory]);
    }
    return byMatter;
  }, [memories]);

  // 宽屏没选就选第一件在做的（换地址不留历史）
  useEffect(() => {
    if (compact || creating || !ready || selected) return;
    const first = groups.open[0] ?? items[0];
    if (first) void navigate({ to: "/matters/$id", params: { id: first.id }, replace: true });
  }, [compact, creating, ready, selected, groups.open, items, navigate]);

  const select = (id: string) => {
    setCreating(false);
    void navigate({ to: "/matters/$id", params: { id } });
  };
  const back = () => {
    if (creating) setCreating(false);
    else void navigate({ to: "/matters" });
  };

  const showDetail = creating || !!selected || (!compact && items.length > 0);
  const detail = (
    <DetailCard label={m.matter_detail()} compact={compact}>
      {creating ? (
        <NewMatterForm
          compact={compact}
          onBack={back}
          onClose={() => setCreating(false)}
          onCreated={(id) => {
            setCreating(false);
            void navigate({ to: "/matters/$id", params: { id } });
          }}
        />
      ) : !ready ? (
        query.isError ? null : (
          <div className="p-4">
            <SkeletonRows count={3} className="h-9" />
          </div>
        )
      ) : current ? (
        <MatterDetailView
          key={current.id}
          matter={current}
          today={today}
          memories={ownedBy.get(current.id) ?? []}
          jobs={cron?.jobs ?? []}
          sessions={sessions ?? []}
          compact={compact}
          onBack={back}
        />
      ) : (
        <EmptyFrame className="m-4">{items.length ? m.matter_pick() : m.matter_pick_none()}</EmptyFrame>
      )}
    </DetailCard>
  );

  // 窄屏选中了一件（或在新建）：只给详情
  if (compact && (selected || creating)) {
    return (
      <Page title={m.nav_matters()} bare>
        {detail}
      </Page>
    );
  }

  const newButton = (
    <Button onClick={() => setCreating(true)}>
      <Icon name="plus" className="size-4" />
      {m.matter_new_button()}
    </Button>
  );
  return (
    <Page
      title={m.nav_matters()}
      finding={ready ? finding(groups.open, today) : query.isError ? m.matter_finding_error() : undefined}
      actions={newButton}
    >
      <PageBody columns={compact || !showDetail ? "grid-cols-[minmax(0,1fr)]" : "grid-cols-[minmax(0,1fr)_360px]"}>
        <section aria-label={m.matter_list()} className="min-w-0">
          {query.isError ? (
            <BlockError message={m.matter_load_error({ message: errorText(query.error) })} onRetry={() => void query.refetch()} />
          ) : !ready ? (
            <SkeletonRows />
          ) : (
            <>
              <div aria-hidden="true" className={cn("grid h-8 items-center gap-x-3 border-b border-border text-small text-muted max-[600px]:hidden", COLUMNS)}>
                <span />
                <span>{m.matter_col_matter()}</span>
                <span className="text-right">{m.matter_col_due()}</span>
                <span className="text-right">{m.matter_col_memory()}</span>
                <span />
              </div>
              <Group title={m.matter_group_goals()} items={groups.goals} empty={m.matter_empty_goals()} {...{ today, selected, ownedBy, select }} />
              <Group title={m.matter_group_areas()} items={groups.areas} empty={m.matter_empty_areas()} {...{ today, selected, ownedBy, select }} />
              {groups.parked.length > 0 && <Group title={m.matter_group_parked()} items={groups.parked} {...{ today, selected, ownedBy, select }} />}
              {groups.closed.length > 0 && (
                <Group
                  title={m.matter_group_closed()}
                  items={groups.closed}
                  cap={5}
                  showAll={showClosed}
                  onToggle={() => setShowClosed((on) => !on)}
                  {...{ today, selected, ownedBy, select }}
                />
              )}
            </>
          )}
        </section>
        {showDetail && !compact && detail}
      </PageBody>
    </Page>
  );
}

function Group({
  title,
  items,
  empty,
  cap = 0,
  showAll,
  onToggle,
  today,
  selected,
  ownedBy,
  select,
}: {
  title: string;
  items: Matter[];
  empty?: string;
  cap?: number;
  showAll?: boolean;
  onToggle?: () => void;
  today: string | undefined;
  selected: string | null;
  ownedBy: Map<string, MemoryItem[]>;
  select: (id: string) => void;
}) {
  const shown = cap && !showAll ? items.slice(0, cap) : items;
  return (
    <div className="[&+&]:mt-5">
      <h2 className="mt-4 border-b border-border pb-2 text-label font-semibold">
        {title}
        <span className="ml-1 font-normal text-muted tabular-nums">{items.length}</span>
      </h2>
      {items.length ? (
        <>
          <ul className="relative z-0">
            {shown.map((matter) => (
              <Row key={matter.id} matter={matter} today={today} on={matter.id === selected} owned={ownedBy.get(matter.id) ?? []} onSelect={select} />
            ))}
          </ul>
          {cap > 0 && items.length > cap && onToggle && (
            <TextButton tone="quiet" onClick={onToggle} className="mt-2">
              {showAll ? m.matter_show_less() : m.matter_show_all({ count: items.length })}
            </TextButton>
          )}
        </>
      ) : (
        <EmptyFrame className="mt-2.5">{empty}</EmptyFrame>
      )}
    </div>
  );
}

function Row({
  matter,
  today,
  on,
  owned,
  onSelect,
}: {
  matter: Matter;
  today: string | undefined;
  on: boolean;
  owned: MemoryItem[];
  onSelect: (id: string) => void;
}) {
  const closed = isClosed(matter);
  const due = dueOf(matter, today);
  const sub =
    matter.kind === "area"
      ? matter.standard
        ? m.matter_sub_standard({ what: matter.standard })
        : m.matter_sub_area()
      : matter.state === "waiting"
        ? m.matter_sub_waiting({ what: matter.waiting_for ?? "" })
        : matter.next
          ? m.matter_sub_next({ what: matter.next })
          : closed
            ? ""
            : m.matter_sub_no_next();
  const closedOn = closed && matter.closed_at ? m.matter_closed_on({ date: formatInstantDay(matter.closed_at) }) : "";
  const pill = closed || matter.state === "parked" ? <Pill tone="off">{statusLabel(matter.state)}</Pill> : matter.state === "waiting" ? <Pill tone="wait">{statusLabel(matter.state)}</Pill> : null;
  return (
    <li
      className={cn(
        "relative grid items-start gap-x-3 gap-y-1.5 border-b border-rule py-3 before:absolute before:inset-x-[-8px] before:inset-y-px before:-z-10 before:rounded-control before:transition-colors hover:before:bg-hover-on-white",
        COLUMNS,
        on && "before:bg-active hover:before:bg-active",
      )}
    >
      <Mark matter={matter} />
      <button
        type="button"
        aria-current={on ? "true" : undefined}
        onClick={() => onSelect(matter.id)}
        className="flex min-w-0 cursor-pointer flex-col gap-1 text-left text-text"
      >
        <span className="leading-5.5">{matter.title}</span>
        {sub && <span className="text-small text-muted">{sub}</span>}
      </button>
      <span className="flex flex-col items-end leading-5.5 tabular-nums max-[600px]:col-start-2 max-[600px]:row-start-2 max-[600px]:flex-row max-[600px]:items-baseline max-[600px]:gap-2 max-[600px]:justify-self-start max-[600px]:text-small max-[600px]:leading-4.5 max-[600px]:text-muted">
        {due ? due.text : <span className="text-muted">—</span>}
        {(due?.sub || closedOn) && (
          <small className={cn("mt-1 text-small leading-4.5 max-[600px]:mt-0", due?.bad ? "text-negative" : "text-muted")}>{due?.sub || closedOn}</small>
        )}
      </span>
      <span className="flex items-center justify-end gap-1.5 text-label leading-5.5 max-[600px]:col-start-2 max-[600px]:row-start-2 max-[600px]:justify-self-end">
        {owned.length ? (
          <>
            <span className="flex flex-wrap gap-[3px]" aria-hidden="true">
              {owned.slice(0, 6).map((memory) => (
                <MemoryMote key={memory.id} kind={memory} />
              ))}
            </span>
            <span className="tabular-nums" title={m.matter_memories_count({ count: owned.length })}>
              {owned.length}
            </span>
          </>
        ) : (
          <span className="text-muted">—</span>
        )}
      </span>
      <span className="justify-self-end max-[600px]:col-start-3 max-[600px]:row-start-1">{pill}</span>
    </li>
  );
}

/** 状态点：目标墨点、领域墨方块、在等琥珀圈、搁置虚线圈、结案灰点。 */
export function Mark({ matter }: { matter: Matter }) {
  const closed = isClosed(matter);
  return (
    <span
      aria-hidden="true"
      className={cn(
        "mt-1.75 justify-self-center",
        closed
          ? "size-1.75 rounded-full bg-control"
          : matter.kind === "area"
            ? "size-2 rounded-[2px] bg-text"
            : matter.state === "waiting"
              ? "size-2 rounded-full border-[1.5px] border-pill-wait-ink"
              : matter.state === "parked"
                ? "size-2 rounded-full border-[1.5px] border-dashed border-faint"
                : "size-2 rounded-full bg-text",
      )}
    />
  );
}
