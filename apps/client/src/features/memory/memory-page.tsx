import { memoryVerb, useHomeSession, useMemoriesAll, useMemoryCoverage, useMemoryFind, useProviderMemories, useSessions } from "@komo/api";
import type { MemoryItem, ProviderMemory, SessionSummary } from "@komo/protocol";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Icon } from "@/components/icon";
import { BlockError, DetailCard, EmptyFrame, Page, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from "@/components/ui/dropdown-menu";
import { IconButton } from "@/components/ui/icon-button";
import { presetDraft } from "@/features/composer/draft";
import { listClass, rowClass } from "@/features/goals/parts";
import { sessionTitle } from "@/features/sessions/title";
import { errorText } from "@/lib/errors";
import { formatNumber, formatRelative } from "@/lib/format";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { MemoryChart } from "./chart";
import { AddForm, MemoryDetail } from "./detail";
import { MemoryMote } from "./mote";
import { groupOf, groupRows, lead, moteKind, scopeKey, scopeName, scopesIn, stateName, usageName } from "./model";
import { getView, patchView, useMemoryView } from "./view-state";
import { useCommitMemory } from "./writes";

/** "相关才带"默认列出几条，其余收在"显示其余 N 条"后面。 */
const CLUES_SHOWN = 8;

function useDebounced<T>(value: T, ms: number): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const id = setTimeout(() => setSettled(value), ms);
    return () => clearTimeout(id);
  }, [value, ms]);
  return settled;
}

const scroller = () => document.querySelector("main");

/**
 * 记忆页（docs/web.md §11，docs/redesign.md §1.5）：一句概况、攒下的记忆图，下面分组——等你过目（记住 / 不用记）、
 * 暂停中、每次都带、相关才带、不再带（收起），装了外部提供方时再加它只读的一组，最后"记一条"去对话里说。
 * 搜索与作用域筛选还在，检索时列表换成命中的几条（外部那组按正文筛，不受作用域筛选影响）。
 * 点一条进它的详情页（不论窗口宽窄），左上角返回，列表回到原来的滚动位置；选中的 id 在地址里（`/memory/$id`）。
 */
export function MemoryPage({ selected }: { selected: string | null }) {
  const navigate = useNavigate();
  const view = useMemoryView();
  const query = useMemoriesAll();
  const coverage = useMemoryCoverage().data;
  const sessions = useSessions(true).data;
  const home = useHomeSession();
  const commit = useCommitMemory();
  const debounced = useDebounced(view.query.trim(), 250);
  const find = useMemoryFind(debounced);

  const ready = query.isSuccess;
  const all = useMemo(() => query.data?.memories ?? [], [query.data]);
  const byId = useMemo(() => new Map([...(find.data?.memories ?? []), ...all].map((x) => [x.id, x])), [all, find.data]);
  const searching = debounced !== "";
  const degraded = find.isError ? errorText(find.error) : find.data?.degraded ? (find.data.degraded_reason ?? m.memory_index_unknown()) : "";
  const scopes = useMemo(() => scopesIn(all), [all]);
  const groups = useMemo(() => groupRows(all, view.scope), [all, view.scope]);
  const current = selected ? byId.get(selected) : undefined;
  const hits = useMemo(() => {
    if (!searching) return undefined;
    // 检索结果保持检索的排序；还没回来时给加载条
    if (find.isFetching) return undefined;
    return find.isError ? [] : find.data?.memories.filter((x) => !view.scope || scopeKey(x.scope) === view.scope);
  }, [searching, find.isFetching, find.isError, find.data, view.scope]);
  const sessionById = useMemo(() => new Map((sessions ?? []).map((s): [string, SessionSummary] => [s.session, s])), [sessions]);

  const select = (id: string) => {
    patchView({ listScroll: scroller()?.scrollTop ?? 0, adding: false });
    scroller()?.scrollTo({ top: 0 });
    void navigate({ to: "/memory/$id", params: { id } });
  };
  const back = () => {
    patchView({ adding: false });
    void navigate({ to: "/memory" });
    requestAnimationFrame(() => requestAnimationFrame(() => scroller()?.scrollTo({ top: getView().listScroll })));
  };
  const clearSelection = () => {
    patchView({ adding: false });
    if (selected) void navigate({ to: "/memory" });
  };
  const startAdding = () => {
    patchView({ listScroll: scroller()?.scrollTop ?? 0, adding: true });
    if (selected) void navigate({ to: "/memory" });
  };
  // 记一条走对话：去主对话，输入框预填一句话（框里已有没发的字就不覆盖）
  const addInChat = () => {
    if (home.data) presetDraft(home.data.session, m.memory_draft());
    void navigate({ to: "/" });
  };

  // 选中的那条不在了（清单变了）：回到没选
  useEffect(() => {
    if (ready && selected && !byId.has(selected) && !find.isFetching) void navigate({ to: "/memory", replace: true });
  }, [ready, selected, byId, find.isFetching, navigate]);

  const act = async (verb: "confirm" | "forget" | "restore", item: MemoryItem) => {
    try {
      commit((await memoryVerb(verb, item)).memory);
    } catch (error) {
      showToast(({ confirm: m.memory_fail_confirm, forget: m.memory_fail_forget, restore: m.memory_fail_restore })[verb]({ message: errorText(error) }));
    }
  };

  const detail = (
    <DetailCard label={m.memory_detail_label()} compact>
      {view.adding ? (
        <AddForm
          scopes={scopesIn(all, true)}
          compact
          onClose={back}
          onAdded={(item) => {
            patchView({ query: "", adding: false });
            void navigate({ to: "/memory/$id", params: { id: item.id } });
          }}
        />
      ) : current ? (
        <MemoryDetail key={current.id} item={current} all={byId} compact onBack={back} />
      ) : !ready ? (
        query.isError ? null : (
          <div className="p-4">
            <SkeletonRows count={4} className="h-9" />
          </div>
        )
      ) : (
        <EmptyFrame className="m-4">{m.memory_pick()}</EmptyFrame>
      )}
    </DetailCard>
  );

  // 进了详情：页头与列表都收起，只给详情
  if (selected || view.adding) {
    return (
      <Page title={m.nav_memory()} bare>
        {detail}
      </Page>
    );
  }

  const row = (item: MemoryItem) => (
    <Row
      key={item.id}
      item={item}
      source={groupOf(item) === "pending" ? sourceText(item, sessionById) : undefined}
      onSelect={select}
      onAct={act}
    />
  );
  const total = all.length;
  const hasAny = Object.values(groups).some((list) => list.length > 0);
  return (
    <Page
      title={m.nav_memory()}
      finding={ready ? lead(all) : query.isError ? m.memory_finding_error() : undefined}
      actions={
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <IconButton name="more" label={m.memory_more()} />
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end" size="compact">
            <DropdownMenuItem size="compact" onSelect={startAdding}>
              {m.memory_add_manual()}
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      }
    >
      <section aria-label={m.memory_chart_title()} className="mb-6">
        {ready ? (
          <MemoryChart items={all} selected={selected} onPick={select} />
        ) : (
          <div aria-hidden="true" className="h-24 rounded-xs bg-active" />
        )}
        {ready && total > 0 && <p className="mt-1 text-small text-muted">{m.memory_chart_note()}</p>}
      </section>

      <div className="flex min-w-0 gap-2 max-[600px]:flex-wrap">
        <label className="flex h-8 min-w-0 flex-1 items-center gap-2 rounded-control bg-surface px-2.5 text-faint shadow-ring-control focus-within:shadow-[0_0_0_2px_var(--color-focus)]">
          <Icon name="search" className="size-4" />
          <span className="sr-only">{m.memory_search_label()}</span>
          <input
            type="search"
            value={view.query}
            onChange={(ev) => {
              patchView({ query: ev.target.value });
              clearSelection();
            }}
            placeholder={m.memory_search_label()}
            autoComplete="off"
            className="min-w-0 flex-1 border-0 bg-transparent text-body text-text outline-0 placeholder:text-faint"
          />
        </label>
        {scopes.length > 1 && (
          <label className="min-w-0 max-w-44 max-[600px]:w-full max-[600px]:max-w-none">
            <span className="sr-only">{m.memory_scope_label()}</span>
            <select
              value={view.scope}
              onChange={(ev) => {
                patchView({ scope: ev.target.value });
                clearSelection();
              }}
              className="h-8 w-full min-w-0 cursor-pointer rounded-control bg-surface px-2 text-body text-text shadow-ring-control"
            >
              <option value="">{m.memory_scope_all()}</option>
              {scopes.map(([key, name]) => (
                <option key={key} value={key}>
                  {name}
                </option>
              ))}
            </select>
          </label>
        )}
      </div>

      {query.isError ? (
        <BlockError message={m.memory_load_error({ message: errorText(query.error) })} onRetry={() => void query.refetch()} />
      ) : !ready ? (
        <div className="mt-6">
          <SkeletonRows />
        </div>
      ) : searching ? (
        <>
          <GroupLabel tone="plain" count={hits ? formatNumber(hits.length) : undefined}>
            {m.memory_list_search({ query: debounced })}
          </GroupLabel>
          {!hits ? <SkeletonRows count={3} /> : hits.length ? <ul className={listClass}>{hits.map(row)}</ul> : <EmptyFrame>{m.memory_none_search()}</EmptyFrame>}
        </>
      ) : !hasAny ? (
        <EmptyFrame className="mt-6">{total ? m.memory_none_scope() : m.memory_chart_empty()}</EmptyFrame>
      ) : (
        <>
          {groups.pending.length > 0 && (
            <GroupSection tone="wait" label={m.memory_group_pending()} sub={m.memory_group_pending_sub()} count={groups.pending.length}>
              {groups.pending.map(row)}
            </GroupSection>
          )}
          {groups.paused.length > 0 && (
            <GroupSection tone="alert" label={m.memory_group_paused()} sub={m.memory_group_paused_sub()} count={groups.paused.length}>
              {groups.paused.map(row)}
            </GroupSection>
          )}
          <GroupSection tone="accent" label={m.memory_rung_always()} sub={m.memory_group_always_sub()} count={groups.always.length} empty={m.memory_none_always()}>
            {groups.always.map(row)}
          </GroupSection>
          <GroupSection tone="ring" label={m.memory_rung_relevant()} sub={m.memory_group_relevant_sub()} count={groups.relevant.length} empty={m.memory_none_relevant()}>
            {(view.allClues ? groups.relevant : groups.relevant.slice(0, CLUES_SHOWN)).map(row)}
            {groups.relevant.length > CLUES_SHOWN && (
              <li className="py-2">
                <button type="button" onClick={() => patchView({ allClues: !view.allClues })} className="cursor-pointer text-label text-muted hover:text-text">
                  {view.allClues ? m.memory_show_fewer() : m.memory_show_rest({ count: groups.relevant.length - CLUES_SHOWN })}
                </button>
              </li>
            )}
          </GroupSection>
          {groups.out.length > 0 && (
            <section className="mt-8">
              <button
                type="button"
                aria-expanded={view.showOut}
                onClick={() => patchView({ showOut: !view.showOut })}
                className="flex cursor-pointer items-center gap-2 text-read font-semibold text-muted hover:text-text"
              >
                <Icon name="right" className={cn("size-3.5 transition-transform", view.showOut && "rotate-90")} />
                {m.memory_rung_out()}
                <span className="text-body font-normal tabular-nums">{formatNumber(groups.out.length)}</span>
              </button>
              {view.showOut && <ul className={cn(listClass, "mt-1")}>{groups.out.map(row)}</ul>}
            </section>
          )}
        </>
      )}

      {ready && !query.isError && <ProviderSection query={debounced} />}

      {ready && !query.isError && (
        <section className="mt-10">
          <h2 className="m-0 mb-1 text-section-title font-semibold">{m.memory_tell_title()}</h2>
          <button
            type="button"
            onClick={addInChat}
            className={cn(rowClass, "flex w-full cursor-pointer items-center gap-3 text-left text-text")}
          >
            <Icon name="leaf" className="size-4 text-muted" />
            <span className="min-w-0 flex-1 leading-5.5">{m.memory_add()}</span>
            <Icon name="right" className="size-3.5 text-faint" />
          </button>
        </section>
      )}

      <p className="mt-6 text-small text-muted empty:hidden">
        {degraded
          ? m.memory_index_degraded({ reason: degraded })
          : coverage && coverage.state !== "unconfigured"
            ? m.memory_index_line({ model: coverage.space?.model ?? "", indexed: formatNumber(coverage.indexed), total: formatNumber(coverage.total) }) +
              (coverage.state === "building" ? m.memory_index_building() : coverage.state === "failed" ? m.memory_index_failed() : "")
            : ""}
      </p>
    </Page>
  );
}

/** Hindsight 的分类名；不认得的原样给。 */
const providerKinds: Record<string, () => string> = {
  world: m.memory_provider_kind_world,
  experience: m.memory_provider_kind_experience,
  observation: m.memory_provider_kind_observation,
};

/**
 * 外部记忆提供方（docs/memory.md 附录 G.9）存着的条目：只读一组，没有确认、遗忘这些动作（要改去提供方那边）。
 * 主用时它的条目就是"相关才带"的线索（空心环），影子模式只记录对比、不进对话（灰点）。没装提供方整组不出现；
 * 问不通只在这一组里说，不影响上面内置的几组。
 */
function ProviderSection({ query }: { query: string }) {
  const list = useProviderMemories(query);
  const first = list.data?.pages[0];
  if (!first?.provider) return null;
  const items = list.data?.pages.flatMap((page) => page.items) ?? [];
  const name = first.provider.charAt(0).toUpperCase() + first.provider.slice(1);
  return (
    <section>
      <GroupLabel tone="plain" count={first.error ? undefined : formatNumber(first.total)} sub={first.primary ? m.memory_provider_primary() : m.memory_provider_shadow()}>
        {name}
      </GroupLabel>
      {first.error ? (
        <BlockError message={m.memory_provider_error({ provider: name, message: first.error })} onRetry={() => void list.refetch()} />
      ) : items.length ? (
        <ul className={listClass}>
          {items.map((item) => (
            <ProviderRow key={item.id} item={item} live={first.primary} />
          ))}
          {list.hasNextPage && (
            <li className="py-2">
              <button
                type="button"
                disabled={list.isFetchingNextPage}
                onClick={() => void list.fetchNextPage()}
                className="cursor-pointer text-label text-muted hover:text-text disabled:cursor-progress"
              >
                {m.memory_show_rest({ count: first.total - items.length })}
              </button>
            </li>
          )}
        </ul>
      ) : (
        <EmptyFrame>{query ? m.memory_none_search() : m.memory_provider_none()}</EmptyFrame>
      )}
    </section>
  );
}

/** 外部的一条：光点 | 正文与一行灰字（分类、发生时间、非个人的作用域）。不能点。 */
function ProviderRow({ item, live }: { item: ProviderMemory; live: boolean }) {
  const sub = [
    item.kind ? (providerKinds[item.kind]?.() ?? item.kind) : null,
    item.at ? formatRelative(item.at) : null,
    scopeName(item.scope) === m.memory_scope_personal() ? null : scopeName(item.scope),
  ].filter((x): x is string => !!x);
  return (
    <li className={cn(rowClass, "grid grid-cols-[16px_minmax(0,1fr)] items-start gap-x-3")}>
      <MemoryMote kind={live ? "ring" : "off"} className="mt-2 justify-self-center" />
      <div className="flex min-w-0 flex-col gap-0.5">
        <span className="leading-6 [overflow-wrap:anywhere]">{item.text}</span>
        {sub.length > 0 && (
          <span className="text-small leading-4.5 text-muted">
            {sub.map((part, i) => (
              <span key={i}>
                {i > 0 && <span className="mx-1.5 text-faint">·</span>}
                {part}
              </span>
            ))}
          </span>
        )}
      </div>
    </li>
  );
}

/** 等你过目那条的出处：最早一条对话证据所在的对话与时间；没有对话证据只写时间。 */
function sourceText(item: MemoryItem, sessions: Map<string, SessionSummary>): string {
  const first = (item.evidence ?? []).find((e) => e.reference.kind === "event");
  const when = formatRelative(first?.observed_at ?? item.created_at);
  const ref = first?.reference;
  if (!ref || ref.kind !== "event") return m.memory_from_when({ when });
  const session = sessions.get(ref.session);
  return session ? m.memory_from_chat({ when, chat: session.kind === "main" ? m.memory_from_main() : sessionTitle(session) }) : m.memory_from_when({ when });
}

type Tone = "wait" | "alert" | "accent" | "ring" | "plain";

/** 一组的标题：圆点（颜色对应这一档的在场程度）、名字、条数；右边灰字一句说明。 */
function GroupLabel({ tone, count, sub, children }: { tone: Tone; count?: ReactNode; sub?: string; children: ReactNode }) {
  return (
    <h2 className="mt-8 mb-1 flex min-w-0 items-baseline gap-2.5 text-read font-semibold">
      {tone !== "plain" && (
        <i
          aria-hidden="true"
          className={cn(
            "size-2.5 shrink-0 translate-y-px self-center rounded-full",
            tone === "wait" && "bg-wait-dot shadow-[0_0_0_4px_var(--color-pill-wait-bg)]",
            tone === "alert" && "border-[1.5px] border-negative",
            tone === "accent" && "bg-accent shadow-[0_0_0_4px_var(--color-accent-pale)]",
            tone === "ring" && "border-2 border-accent",
          )}
        />
      )}
      <span className={cn(tone === "wait" && "text-pill-wait-ink", tone === "accent" && "text-accent")}>{children}</span>
      {count !== undefined && <small className="text-body font-normal text-muted tabular-nums">{count}</small>}
      {sub && <small className="ml-1 truncate text-small font-normal text-faint max-[600px]:hidden">{sub}</small>}
    </h2>
  );
}

function GroupSection({ tone, label, sub, count, empty, children }: { tone: Tone; label: string; sub?: string; count: number; empty?: string; children: ReactNode }) {
  return (
    <section>
      <GroupLabel tone={tone} count={formatNumber(count)} sub={sub}>
        {label}
      </GroupLabel>
      {count ? <ul className={listClass}>{children}</ul> : empty ? <EmptyFrame>{empty}</EmptyFrame> : null}
    </section>
  );
}

/**
 * 一行：光点 | 正文（全文，不截断）与下面一行灰字（使用方式、条件、作用域、非生效的状态；等你过目的写出处）| 右边的动作。
 * 等你过目的右边是"记住 / 不用记"（确认这一版 / 遗忘）；遗忘过的是"恢复"；其余写带上次数，点正文进详情。
 */
function Row({
  item,
  source,
  onSelect,
  onAct,
}: {
  item: MemoryItem;
  source?: string;
  onSelect: (id: string) => void;
  onAct: (verb: "confirm" | "forget" | "restore", item: MemoryItem) => Promise<void>;
}) {
  const [busy, setBusy] = useState(false);
  const run = async (verb: "confirm" | "forget" | "restore") => {
    setBusy(true);
    await onAct(verb, item);
    setBusy(false);
  };
  const pending = !!source;
  const sub = [
    source ?? usageName(item),
    item.application === "convention" && item.applies_when ? item.applies_when : null,
    scopeName(item.scope) === m.memory_scope_personal() ? null : scopeName(item.scope),
    item.state !== "active" && !pending ? stateName(item.state) : null,
  ].filter((x): x is string => !!x);
  return (
    <li className={cn(rowClass, "grid grid-cols-[16px_minmax(0,1fr)_auto] items-start gap-x-3 max-[600px]:grid-cols-[16px_minmax(0,1fr)]")}>
      <MemoryMote kind={moteKind(item)} className="mt-2 justify-self-center" />
      <button type="button" onClick={() => onSelect(item.id)} className="flex min-w-0 cursor-pointer flex-col gap-0.5 text-left text-text">
        <span className="leading-6 [overflow-wrap:anywhere]">{item.content}</span>
        <span className="text-small leading-4.5 text-muted">
          {sub.map((part, i) => (
            <span key={i}>
              {i > 0 && <span className="mx-1.5 text-faint">·</span>}
              {part}
            </span>
          ))}
        </span>
      </button>
      <span className="flex min-h-6 items-center justify-self-end gap-2 text-label max-[600px]:col-start-2 max-[600px]:mt-2 max-[600px]:justify-self-start">
        {pending ? (
          <>
            <Button size="sm" variant="primary" disabled={busy} title={m.memory_confirm_title({ revision: item.revision })} onClick={() => void run("confirm")}>
              {m.memory_keep()}
            </Button>
            <Button size="sm" disabled={busy} onClick={() => void run("forget")}>
              {m.memory_drop()}
            </Button>
          </>
        ) : item.state === "forgotten" ? (
          <button type="button" disabled={busy} onClick={() => void run("restore")} className="cursor-pointer font-medium text-text underline underline-offset-3 disabled:cursor-progress disabled:text-muted">
            {m.memory_restore()}
          </button>
        ) : item.usage.count > 0 ? (
          <span className="text-muted tabular-nums">{m.memory_row_used({ count: formatNumber(item.usage.count) })}</span>
        ) : null}
      </span>
    </li>
  );
}

