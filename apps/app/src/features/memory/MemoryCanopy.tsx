import { useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ChevronDownIcon, MoonStarIcon, SearchIcon, XIcon } from "lucide-react";

import { qk } from "@/shared/api/query-keys";
import { useConnection } from "@/shared/api/use-connection";
import { POLL } from "@/shared/config";
import { fmtAgo } from "@/shared/lib/format";
import { cn } from "@/shared/lib/utils";
import type { Memory } from "@/shared/types";
import { Button } from "@/shared/ui/button";
import { EmptyState } from "@/shared/ui/empty-state";
import { ErrorLine } from "@/shared/ui/error-line";
import { IconButton } from "@/shared/ui/icon-button";
import { Input } from "@/shared/ui/input";
import { Loading } from "@/shared/ui/loading";
import { actOnMemory, applyDream, fetchDream, fetchMemories } from "./api";
import {
  ACTION_LABELS,
  TIERS,
  TIER_ORDER,
  actionsFor,
  confidenceLabel,
  kindLabel,
  tierOf,
  type Tier,
} from "./light";

/** Tiers that are out of the prompt. Folded away by default: at a few hundred
 *  memories the shade is most of the library, and scrolling past it to reach the
 *  four things that need a decision is the whole failure of a flat list. */
const SHADE: Tier[] = ["archived", "rejected"];

export function MemoryCanopy() {
  const { connected } = useConnection();
  const qc = useQueryClient();
  const [search, setSearch] = useState("");
  const [kind, setKind] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [shadeOpen, setShadeOpen] = useState(false);
  // Rows that just changed, so they can hold an afterglow where they land.
  const [glowing, setGlowing] = useState<string[]>([]);
  const glowTimers = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());

  const memories = useQuery({
    queryKey: qk.memories(""),
    queryFn: () => fetchMemories(""),
    refetchInterval: POLL.dashboard,
    enabled: connected,
  });

  const dream = useQuery({
    queryKey: qk.dream,
    queryFn: fetchDream,
    enabled: connected,
  });

  const markGlowing = (id: string) => {
    setGlowing((current) => (current.includes(id) ? current : [...current, id]));
    clearTimeout(glowTimers.current.get(id));
    glowTimers.current.set(
      id,
      setTimeout(() => {
        setGlowing((current) => current.filter((item) => item !== id));
        glowTimers.current.delete(id);
      }, 2400),
    );
  };

  const act = useMutation({
    mutationFn: ({ id, action }: { id: string; action: string }) => actOnMemory(id, action),
    onSuccess: (_data, { id }) => markGlowing(id),
    onSettled: () => qc.invalidateQueries({ queryKey: ["memories"] }),
  });

  const consolidate = useMutation({
    mutationFn: applyDream,
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: ["memories"] });
      void qc.invalidateQueries({ queryKey: qk.dream });
    },
  });

  const all = useMemo(() => memories.data ?? [], [memories.data]);

  const kinds = useMemo(() => [...new Set(all.map((memory) => memory.kind))].sort(), [all]);

  const visible = useMemo(() => {
    const needle = search.trim().toLowerCase();
    return all.filter((memory) => {
      if (kind && memory.kind !== kind) return false;
      if (!needle) return true;
      return memory.content.toLowerCase().includes(needle);
    });
  }, [all, search, kind]);

  const grouped = useMemo(() => {
    const buckets = new Map<Tier, Memory[]>(TIER_ORDER.map((tier) => [tier, []]));
    for (const memory of visible) buckets.get(tierOf(memory))!.push(memory);
    // Inside a tier, the most recently touched sits highest — recall time when a
    // memory has one, otherwise when it was last written.
    for (const rows of buckets.values()) {
      rows.sort((a, b) => (b.last_used_at ?? b.updated_at) - (a.last_used_at ?? a.updated_at));
    }
    return buckets;
  }, [visible]);

  const litCount = all.filter((memory) => tierOf(memory) === "active").length;
  const everRecalled = all.some((memory) => memory.recall_count > 0);
  const selected = all.find((memory) => memory.id === selectedId) ?? null;

  // Candidates first — they are the only rows that are *waiting on the operator*.
  // Everything else is ordered by how much light it stands in.
  const order: Tier[] = ["candidate", "active"];

  return (
    <div className="flex min-h-0 min-w-0 flex-1">
      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <header className="komorebi-dapple shrink-0 border-b border-border px-6 pt-6 pb-4">
          <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:gap-4">
            {/* The page opens with what komo knows, not with the word "记忆" —
                the shell header already carries the label. */}
            <div className="min-w-0 flex-1">
              <h1 className="max-w-2xl text-lg leading-relaxed font-medium tracking-tight text-pretty">
                {all.length === 0 ? (
                  "komo 还没有记住任何事。"
                ) : (
                  <>
                    komo 记住了 <Count>{all.length}</Count> 件事，
                    <Count>{litCount}</Count> 件能被回忆检索到 。
                  </>
                )}
              </h1>
              <p className="mt-2 text-xs text-muted-foreground">
                此处管理按需回忆的记忆。每轮携带的常驻记忆在 MEMORY.md 中编辑。
              </p>
            </div>
            <DreamButton
              candidateCount={dream.data?.candidate_count ?? 0}
              promoteCount={dream.data?.promote.length ?? 0}
              archiveCount={dream.data?.archive.length ?? 0}
              pending={consolidate.isPending}
              onApply={() => consolidate.mutate()}
            />
          </div>

          <div className="mt-4 flex flex-wrap items-center gap-2">
            <div className="relative min-w-52 flex-1">
              <SearchIcon
                className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground"
                aria-hidden="true"
              />
              <Input
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder="搜索记忆内容…"
                className="h-8 pl-8 text-sm"
              />
            </div>
            <div className="flex flex-wrap items-center gap-1">
              <FilterChip active={!kind} onClick={() => setKind("")}>
                全部类型
              </FilterChip>
              {kinds.map((value) => (
                <FilterChip key={value} active={kind === value} onClick={() => setKind(value)}>
                  {kindLabel(value)}
                </FilterChip>
              ))}
            </div>
          </div>
        </header>

        <div className="min-h-0 flex-1 overflow-y-auto px-6 py-5">
          {!connected ? (
            <EmptyState>未连接到 gateway。</EmptyState>
          ) : memories.isPending ? (
            <Loading>正在读取记忆…</Loading>
          ) : memories.error ? (
            <ErrorLine error={memories.error} />
          ) : all.length === 0 ? (
            <EmptyState>还没有记忆。和 komo 多聊几次，它会开始记住要紧的事。</EmptyState>
          ) : visible.length === 0 ? (
            <EmptyState>没有符合条件的记忆。</EmptyState>
          ) : (
            <div className="mx-auto flex max-w-3xl flex-col gap-7">
              {order.map((tier) => {
                const rows = grouped.get(tier)!;
                if (rows.length === 0) return null;
                return (
                  <TierSection key={tier} tier={tier} count={rows.length}>
                    {rows.map((memory) => (
                      <MemoryRow
                        key={memory.id}
                        memory={memory}
                        glowing={glowing.includes(memory.id)}
                        selected={memory.id === selectedId}
                        busy={act.isPending}
                        onSelect={() => setSelectedId(memory.id === selectedId ? null : memory.id)}
                        onAct={(action) => act.mutate({ id: memory.id, action })}
                      />
                    ))}
                  </TierSection>
                );
              })}

              <ShadeLayer
                open={shadeOpen}
                onToggle={() => setShadeOpen((value) => !value)}
                rows={SHADE.flatMap((tier) => grouped.get(tier)!)}
                glowing={glowing}
                selectedId={selectedId}
                busy={act.isPending}
                onSelect={(id) => setSelectedId(id === selectedId ? null : id)}
                onAct={(id, action) => act.mutate({ id, action })}
              />

              {!everRecalled && all.length > 0 && (
                <p className="border-t border-border pt-4 text-xs leading-relaxed text-muted-foreground">
                  还没有任何一条记忆被回忆命中过（回忆次数全部为 0）。
                  夜间巩固依据的正是回忆信号，所以在这之前，候选不会自动晋升——
                  需要你在这里手动决定。
                </p>
              )}
            </div>
          )}

          {act.error && (
            <div className="mx-auto mt-4 max-w-3xl">
              <ErrorLine error={act.error} />
            </div>
          )}
        </div>
      </div>

      {selected && <Inspector memory={selected} onClose={() => setSelectedId(null)} />}
    </div>
  );
}

/** A number inside the opening sentence, weighted so the counts read first. */
function Count({ children }: { children: React.ReactNode }) {
  return <span className="font-semibold tabular-nums text-primary">{children}</span>;
}

function FilterChip({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "h-7 rounded-full border px-2.5 text-xs transition-colors duration-(--duration-quick) ease-(--ease-komo)",
        active
          ? "border-primary/40 bg-primary/10 text-foreground"
          : "border-border text-muted-foreground hover:bg-muted hover:text-foreground",
      )}
    >
      {children}
    </button>
  );
}

function TierSection({
  tier,
  count,
  children,
}: {
  tier: Tier;
  count: number;
  children: React.ReactNode;
}) {
  const style = TIERS[tier];
  return (
    <section>
      <div className="mb-2 flex items-baseline gap-2">
        <h2 className="text-sm font-medium">{style.label}</h2>
        <span className="text-xs tabular-nums text-muted-foreground">{count}</span>
        <span className="text-xs text-muted-foreground">· {style.meaning}</span>
      </div>
      <div className="flex flex-col gap-1.5">{children}</div>
    </section>
  );
}

function MemoryRow({
  memory,
  glowing,
  selected,
  busy,
  onSelect,
  onAct,
}: {
  memory: Memory;
  glowing: boolean;
  selected: boolean;
  busy: boolean;
  onSelect: () => void;
  onAct: (action: string) => void;
}) {
  const tier = tierOf(memory);
  const style = TIERS[tier];
  const actions = actionsFor(tier);

  return (
    <div
      className={cn(
        "group rounded-lg border px-3 py-2.5 transition-colors duration-(--duration-base) ease-(--ease-komo)",
        style.row,
        selected && "ring-1 ring-ring/45",
        glowing && "komorebi-afterglow",
      )}
    >
      <div className="flex items-start gap-2.5">
        <span
          aria-hidden="true"
          className={cn("mt-1.5 size-1.5 shrink-0 rounded-full", style.mote)}
        />
        <button
          type="button"
          onClick={onSelect}
          className="min-w-0 flex-1 text-left text-sm leading-relaxed break-words"
        >
          {memory.content}
        </button>
      </div>

      <div className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 pl-4 text-xs text-muted-foreground">
        <span>{kindLabel(memory.kind)}</span>
        <span aria-hidden="true">·</span>
        <span>{confidenceLabel(memory.confidence)}</span>
        <span aria-hidden="true">·</span>
        <span>
          {memory.last_used_at
            ? `${fmtAgo(memory.last_used_at)}被回忆 · 共 ${memory.recall_count} 次`
            : `${fmtAgo(memory.created_at)}记住 · 未被回忆过`}
        </span>
        <span className="flex-1" />
        {/* Candidates are the rows the page exists to resolve, so their verbs
            stay visible; everywhere else the actions are secondary and wait for
            a pointer rather than crowding the row. */}
        <span
          className={cn(
            "flex shrink-0 gap-1 transition-opacity duration-(--duration-quick)",
            tier !== "candidate" && "opacity-0 group-hover:opacity-100 focus-within:opacity-100",
          )}
        >
          {actions.map((action) => (
            <Button
              key={action}
              size="xs"
              variant={action === "reject" ? "ghost" : "secondary"}
              disabled={busy}
              onClick={() => onAct(action)}
              className={action === "reject" ? "text-muted-foreground hover:text-destructive" : ""}
            >
              {ACTION_LABELS[action]}
            </Button>
          ))}
        </span>
      </div>
    </div>
  );
}

function ShadeLayer({
  open,
  onToggle,
  rows,
  glowing,
  selectedId,
  busy,
  onSelect,
  onAct,
}: {
  open: boolean;
  onToggle: () => void;
  rows: Memory[];
  glowing: string[];
  selectedId: string | null;
  busy: boolean;
  onSelect: (id: string) => void;
  onAct: (id: string, action: string) => void;
}) {
  if (rows.length === 0) return null;
  return (
    <section>
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={open}
        className="flex w-full items-baseline gap-2 border-t border-border pt-4 text-left"
      >
        <ChevronDownIcon
          className={cn(
            "size-3.5 self-center text-muted-foreground transition-transform duration-(--duration-base) ease-(--ease-komo)",
            !open && "-rotate-90",
          )}
          aria-hidden="true"
        />
        <h2 className="text-sm font-medium text-muted-foreground">{TIERS.archived.label}</h2>
        <span className="text-xs tabular-nums text-muted-foreground">{rows.length}</span>
        <span className="text-xs text-muted-foreground">· {TIERS.archived.meaning}</span>
      </button>
      {open && (
        <div className="mt-2 flex flex-col gap-1.5">
          {rows.map((memory) => (
            <MemoryRow
              key={memory.id}
              memory={memory}
              glowing={glowing.includes(memory.id)}
              selected={memory.id === selectedId}
              busy={busy}
              onSelect={() => onSelect(memory.id)}
              onAct={(action) => onAct(memory.id, action)}
            />
          ))}
        </div>
      )}
    </section>
  );
}

function DreamButton({
  candidateCount,
  promoteCount,
  archiveCount,
  pending,
  onApply,
}: {
  candidateCount: number;
  promoteCount: number;
  archiveCount: number;
  pending: boolean;
  onApply: () => void;
}) {
  const nothingToDo = promoteCount === 0 && archiveCount === 0;
  return (
    <div className="shrink-0 rounded-lg border border-border bg-card/70 px-3 py-2 text-xs sm:max-w-60">
      <div className="flex items-center gap-1.5 font-medium">
        <MoonStarIcon className="size-3.5 text-muted-foreground" aria-hidden="true" />
        夜间巩固
      </div>
      <p className="mt-1 leading-relaxed text-muted-foreground">
        {nothingToDo
          ? `${candidateCount} 个候选都还不够条件，今晚不会有变化。`
          : `今晚会晋升 ${promoteCount} 条、归档 ${archiveCount} 条。`}
      </p>
      {!nothingToDo && (
        <Button size="xs" variant="secondary" className="mt-2" disabled={pending} onClick={onApply}>
          {pending ? "正在应用…" : "现在就应用"}
        </Button>
      )}
    </div>
  );
}

function Inspector({ memory, onClose }: { memory: Memory; onClose: () => void }) {
  const tier = tierOf(memory);
  const rows: [string, string][] = [
    ["在场", `${TIERS[tier].label} — ${TIERS[tier].meaning}`],
    ["类型", kindLabel(memory.kind)],
    ["来源", confidenceLabel(memory.confidence)],
    ["重要度", String(memory.importance)],
    ["回忆次数", memory.recall_count > 0 ? `${memory.recall_count} 次` : "从未被回忆"],
    ["上次回忆", memory.last_used_at ? fmtAgo(memory.last_used_at) : "—"],
    ["记住于", fmtAgo(memory.created_at)],
    ["最后变更", fmtAgo(memory.updated_at)],
  ];

  return (
    <>
      {/* Below lg there is no room for a rail, so the same panel slides over the
          canopy instead of disappearing — selecting a row has to lead somewhere
          on a narrow screen too. */}
      <button
        type="button"
        aria-label="关闭详情"
        onClick={onClose}
        className="fixed inset-0 z-20 bg-foreground/20 lg:hidden"
      />
      <aside className="fixed inset-y-0 right-0 z-30 flex w-80 max-w-[85vw] shrink-0 flex-col border-l border-border bg-sidebar shadow-xl lg:static lg:z-auto lg:max-w-none lg:shadow-none">
        <div className="flex h-12 shrink-0 items-center gap-2 border-b border-border px-4">
          <h2 className="flex-1 text-sm font-medium">记忆详情</h2>
          <IconButton title="关闭" onClick={onClose}>
            <XIcon className="size-3.5" />
          </IconButton>
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4">
          <p className="text-sm leading-relaxed break-words">{memory.content}</p>
          <dl className="mt-4 flex flex-col gap-2 border-t border-border pt-4 text-xs">
            {rows.map(([label, value]) => (
              <div key={label} className="flex gap-3">
                <dt className="w-16 shrink-0 text-muted-foreground">{label}</dt>
                <dd className="min-w-0 flex-1 break-words">{value}</dd>
              </div>
            ))}
            <div className="flex gap-3">
              <dt className="w-16 shrink-0 text-muted-foreground">来自会话</dt>
              <dd className="min-w-0 flex-1 font-mono text-[11px] break-all">{memory.source}</dd>
            </div>
          </dl>
        </div>
      </aside>
    </>
  );
}
