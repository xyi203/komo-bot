import { memoryVerb, useMemoriesAll, useMemoryCoverage, useMemoryFind } from "@komo/api";
import type { MemoryItem } from "@komo/protocol";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useMemo, useRef, useState } from "react";
import { Icon } from "@/components/icon";
import { BlockError, DetailCard, EmptyFrame, Page, PageBody, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { columnLabelClass, listClass, rowClass, rowOnClass, SectionTitle } from "@/features/matters/parts";
import { errorText } from "@/lib/errors";
import { formatNumber, formatRelative } from "@/lib/format";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { MemoryChart } from "./chart";
import { AddForm, MemoryDetail } from "./detail";
import { MemoryMote } from "./mote";
import { bucketOf, finding, inBucket, listRows, moteKind, RUNGS, scopeKey, scopeName, scopesIn, stateName, usageName, type Bucket, type Rung } from "./model";
import { getView, patchView, useMemoryView } from "./view-state";
import { useCommitMemory } from "./writes";

const rungLabel = (r: Rung) => ({ always: m.memory_rung_always, relevant: m.memory_rung_relevant, paused: m.memory_rung_paused, out: m.memory_rung_out })[r]();
const rungSub = (r: Rung) => ({ always: m.memory_rung_always_sub, relevant: m.memory_rung_relevant_sub, paused: m.memory_rung_paused_sub, out: m.memory_rung_out_sub })[r]();
const bucketLabel = (b: Bucket) => (b === "live" ? m.memory_list_live() : rungLabel(b));
const noneText = (b: Bucket) =>
  ({ live: m.memory_none_live, always: m.memory_none_always, relevant: m.memory_none_relevant, paused: m.memory_none_paused, out: m.memory_none_out })[b]();

function useDebounced<T>(value: T, ms: number): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const id = setTimeout(() => setSettled(value), ms);
    return () => clearTimeout(id);
  }, [value, ms]);
  return settled;
}

const scroller = () => document.querySelector("main");

/** 一行的网格：光点 | 正文两行 | 带上 / 用上 | 最近带上 | 动作；≤600 变两行行，次数与时间挪到第二行。 */
const ROW_GRID =
  "grid grid-cols-[16px_minmax(0,1fr)_76px_76px_92px] gap-x-3 max-[600px]:grid-cols-[16px_minmax(0,1fr)_auto] max-[600px]:gap-y-1.5";

/**
 * 记忆页（docs/web.md §11）：komo 现在把哪些关于我的事带进对话，哪些等我过目。
 * 上面是"攒下的记忆"图（图例兼筛选），下面是列表。点一条进它的详情页（不论窗口宽窄），左上角返回，列表回到原来的
 * 滚动位置；选中的 id 在地址里（`/memory/$id`）。
 */
export function MemoryPage({ selected }: { selected: string | null }) {
  const navigate = useNavigate();
  const view = useMemoryView();
  const query = useMemoriesAll();
  const coverage = useMemoryCoverage().data;
  const commit = useCommitMemory();
  const debounced = useDebounced(view.query.trim(), 250);
  const find = useMemoryFind(debounced);
  const listRef = useRef<HTMLElement>(null);

  const ready = query.isSuccess;
  const all = useMemo(() => query.data?.memories ?? [], [query.data]);
  const byId = useMemo(() => new Map([...(find.data?.memories ?? []), ...all].map((x) => [x.id, x])), [all, find.data]);
  const searching = debounced !== "";
  const degraded = find.isError ? errorText(find.error) : find.data?.degraded ? (find.data.degraded_reason ?? m.memory_index_unknown()) : "";

  const rows = useMemo(() => {
    if (searching) {
      // 检索结果保持检索的排序；还没回来时给加载条
      const hits = find.isFetching ? undefined : find.isError ? [] : find.data?.memories;
      return hits?.filter((x) => !view.scope || scopeKey(x.scope) === view.scope);
    }
    return listRows(all, view.bucket, view.scope);
  }, [searching, find.isFetching, find.isError, find.data, all, view.bucket, view.scope]);
  const scopes = useMemo(() => scopesIn(all), [all]);
  const current = selected ? byId.get(selected) : undefined;

  const select = (id: string) => {
    patchView({ listScroll: scroller()?.scrollTop ?? 0 });
    scroller()?.scrollTo({ top: 0 });
    patchView({ adding: false });
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

  // 从地址进来的那条不在当前筛选里：换到它所在的档
  useEffect(() => {
    if (current && ready && !searching && !inBucket(current, view.bucket)) patchView({ bucket: bucketOf(current) });
    // 只在选中的那条或清单变了才对一次，不拦着用户之后点别的档
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected, ready]);
  // 选中的那条不在了（清单变了）：回到没选
  useEffect(() => {
    if (ready && selected && !byId.has(selected) && !find.isFetching) void navigate({ to: "/memory", replace: true });
  }, [ready, selected, byId, find.isFetching, navigate]);

  const confirmFromRow = async (item: MemoryItem) => {
    try {
      commit((await memoryVerb("confirm", item)).memory);
    } catch (error) {
      showToast(m.memory_fail_confirm({ message: errorText(error) }));
      select(item.id);
    }
  };
  const restoreFromRow = async (item: MemoryItem) => {
    try {
      commit((await memoryVerb("restore", item)).memory);
    } catch (error) {
      showToast(m.memory_fail_restore({ message: errorText(error) }));
      select(item.id);
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
            patchView({ bucket: "live", query: "", adding: false });
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

  const title = searching ? m.memory_list_search({ query: debounced }) : bucketLabel(view.bucket);
  const finding_ = ready ? finding(all) : query.isError ? m.memory_finding_error() : undefined;
  return (
    <Page
      title={m.nav_memory()}
      finding={finding_}
      actions={
        <Button
          onClick={() => {
            patchView({ listScroll: scroller()?.scrollTop ?? 0 });
            patchView({ adding: true });
            if (selected) void navigate({ to: "/memory" });
          }}
        >
          <Icon name="plus" className="size-3.5" />
          {m.memory_add()}
        </Button>
      }
    >
      <Ladder
        all={all}
        ready={ready}
        bucket={view.bucket}
        searching={searching}
        selected={selected}
        onRung={(key) => {
          patchView({ bucket: view.bucket === key ? "live" : key, query: "" });
          clearSelection();
        }}
        onPick={(id) => {
          const hit = byId.get(id);
          // 从图上点的那一条可能不在当前这一档：换到它所在的档，列表里也看得到它
          if (hit && (searching || !inBucket(hit, view.bucket))) patchView({ bucket: bucketOf(hit), query: "" });
          select(id);
        }}
      />
      <PageBody columns={"grid-cols-[minmax(0,1fr)]"}>
        <section ref={listRef} aria-label={title} className="min-w-0">
          <div className="flex min-h-9 items-center gap-3 max-[600px]:flex-wrap">
            <SectionTitle count={rows ? m.memory_list_count({ count: formatNumber(rows.length) }) : undefined} className="min-w-0 flex-1">
              {title}
            </SectionTitle>
            <div className="flex min-w-0 gap-2 max-[600px]:w-full">
              <label className="flex h-8 w-50 items-center gap-2 rounded-control bg-surface px-2.5 text-faint shadow-ring-control focus-within:shadow-[0_0_0_2px_var(--color-focus)] max-[600px]:w-auto max-[600px]:min-w-0 max-[600px]:flex-1">
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
              <label className="min-w-0 max-w-40 max-[600px]:w-[42%] max-[600px]:max-w-none">
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
            </div>
          </div>
          {query.isError ? (
            <BlockError message={m.memory_load_error({ message: errorText(query.error) })} onRetry={() => void query.refetch()} />
          ) : !ready || !rows ? (
            <SkeletonRows />
          ) : rows.length ? (
            <>
              {/* 列只有 76px，英文列名会折成两行：随内容长高、底对齐，不能定高（定高时第二行压在第一条记忆上） */}
              <div aria-hidden="true" className={cn(ROW_GRID, "mt-1 min-h-8 items-end pb-1 max-[600px]:hidden")}>
                <span />
                <span className={columnLabelClass}>{m.memory_col_content()}</span>
                <span className={cn(columnLabelClass, "justify-self-end text-right")}>{m.memory_col_used()}</span>
                <span className={cn(columnLabelClass, "justify-self-end text-right")}>{m.memory_col_last()}</span>
                <span />
              </div>
              <ul className={listClass}>
                {rows.map((item) => (
                  <Row key={item.id} item={item} on={item.id === selected} onSelect={select} onConfirm={confirmFromRow} onRestore={restoreFromRow} />
                ))}
              </ul>
            </>
          ) : (
            <EmptyFrame className="mt-2">{searching ? m.memory_none_search() : noneText(view.bucket)}</EmptyFrame>
          )}
          <p className="mt-3 text-small text-muted empty:hidden">
            {degraded
              ? m.memory_index_degraded({ reason: degraded })
              : coverage && coverage.state !== "unconfigured"
                ? m.memory_index_line({ model: coverage.space?.model ?? "", indexed: formatNumber(coverage.indexed), total: formatNumber(coverage.total) }) +
                  (coverage.state === "building" ? m.memory_index_building() : coverage.state === "failed" ? m.memory_index_failed() : "")
                : ""}
          </p>
        </section>
      </PageBody>
    </Page>
  );
}

function Ladder({
  all,
  ready,
  bucket,
  searching,
  selected,
  onRung,
  onPick,
}: {
  all: MemoryItem[];
  ready: boolean;
  bucket: Bucket;
  searching: boolean;
  selected: string | null;
  onRung: (key: Rung) => void;
  onPick: (id: string) => void;
}) {
  return (
    <section aria-label={m.memory_chart_title()} className="mb-9">
      <div className="mb-3 flex items-start gap-4 max-[600px]:flex-col max-[600px]:gap-2">
        <div className="min-w-0 flex-1">
          <SectionTitle>{m.memory_chart_title()}</SectionTitle>
          <p className="mt-0.5 text-small text-muted">{m.memory_chart_note()}</p>
        </div>
        <div role="tablist" aria-label={m.memory_rungs_label()} className="flex flex-wrap justify-end gap-1 max-[600px]:justify-start">
          {RUNGS.map(({ key, test }) => {
            const n = all.filter(test).length;
            const on = !searching && (bucket === key || (bucket === "live" && (key === "always" || key === "relevant")));
            return (
              <button
                key={key}
                type="button"
                role="tab"
                aria-selected={on}
                disabled={!ready}
                title={rungSub(key)}
                onClick={() => onRung(key)}
                className={cn(
                  "inline-flex h-7.5 cursor-pointer items-center gap-1.5 rounded-control px-2.5 text-label text-muted transition-colors hover:bg-hover-on-white hover:text-text max-[600px]:px-2",
                  on && "bg-active font-medium text-text",
                )}
              >
                <MemoryMote kind={({ always: "full", relevant: "ring", paused: "pale", out: "off" } as const)[key]} />
                {rungLabel(key)}
                <span className={cn("tabular-nums", n ? "text-text" : "text-muted")}>{ready ? n : "—"}</span>
              </button>
            );
          })}
        </div>
      </div>
      {ready ? <MemoryChart items={all} bucket={bucket} searching={searching} selected={selected} onPick={onPick} /> : <div aria-hidden="true" className="h-24 rounded-xs bg-active" />}
    </section>
  );
}

function Row({
  item,
  on,
  onSelect,
  onConfirm,
  onRestore,
}: {
  item: MemoryItem;
  on: boolean;
  onSelect: (id: string) => void;
  onConfirm: (item: MemoryItem) => Promise<void>;
  onRestore: (item: MemoryItem) => Promise<void>;
}) {
  const [busy, setBusy] = useState(false);
  const sub = [usageName(item), item.application === "convention" && item.applies_when ? item.applies_when : null, scopeName(item.scope), item.state !== "active" ? stateName(item.state) : null].filter(
    (x): x is string => !!x,
  );
  const act = async (fn: (item: MemoryItem) => Promise<void>) => {
    setBusy(true);
    await fn(item);
    setBusy(false);
  };
  return (
    <li
      className={cn(ROW_GRID, "items-start", rowClass, on && rowOnClass)}
    >
      <MemoryMote kind={moteKind(item)} className="mt-1.75 justify-self-center" />
      <button
        type="button"
        aria-current={on ? "true" : undefined}
        onClick={() => onSelect(item.id)}
        className="flex min-w-0 cursor-pointer flex-col gap-1 text-left text-text"
      >
        <span className="leading-5.5 [overflow-wrap:anywhere]">{item.content}</span>
        <span className="text-small text-muted">
          {sub.map((part, i) => (
            <span key={i}>
              {i > 0 && <span className="mx-1.5 text-faint">·</span>}
              {part}
            </span>
          ))}
        </span>
      </button>
      <span className="flex flex-col items-end leading-5.5 tabular-nums max-[600px]:col-start-2 max-[600px]:row-start-2 max-[600px]:flex-row max-[600px]:items-baseline max-[600px]:gap-2.5 max-[600px]:justify-self-start max-[600px]:text-small max-[600px]:leading-4.5 max-[600px]:text-muted">
        <span className="hidden max-[600px]:inline">{m.memory_row_brought()}</span>
        {formatNumber(item.usage.count)}
        <small className="mt-1 text-small leading-4.5 text-muted max-[600px]:mt-0">{m.memory_row_adopted({ count: formatNumber(item.usage.adopted_count) })}</small>
      </span>
      <span
        className={cn(
          "justify-self-end text-label leading-5.5 text-muted tabular-nums max-[600px]:col-start-2 max-[600px]:row-start-2 max-[600px]:text-small max-[600px]:leading-4.5",
          !item.usage.last_used_at && "max-[600px]:hidden",
        )}
      >
        {item.usage.last_used_at ? formatRelative(item.usage.last_used_at) : "—"}
      </span>
      <span className="flex min-h-5.5 items-center justify-self-end text-label max-[600px]:col-start-3 max-[600px]:row-start-1">
        {item.state === "forgotten" ? (
          <button type="button" disabled={busy} onClick={() => void act(onRestore)} className="cursor-pointer font-medium text-text underline underline-offset-3 disabled:cursor-progress disabled:text-muted">
            {m.memory_restore()}
          </button>
        ) : item.state !== "active" && item.state !== "candidate" && item.state !== "contested" ? (
          <span className="text-muted">—</span>
        ) : item.confirmation === "user_confirmed" ? (
          <span className="inline-flex items-center gap-1 text-muted">
            <Icon name="check" className="size-3.5" />
            {m.memory_confirmed()}
          </span>
        ) : (
          <Button size="sm" disabled={busy} title={m.memory_confirm_title({ revision: item.revision })} onClick={() => void act(onConfirm)}>
            {m.memory_confirm()}
          </Button>
        )}
      </span>
    </li>
  );
}
