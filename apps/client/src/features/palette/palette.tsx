import { useCronJobs, useMatters, useMemorySearch, useSessions, type MemoryBrief, type MemoryScope } from "@komo/api";
import type { CronJob, Matter, SessionSummary } from "@komo/protocol";
import * as Dialog from "@radix-ui/react-dialog";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { Icon, type IconName } from "@/components/icon";
import { IconButton } from "@/components/ui/icon-button";
import { useSessionActions } from "@/features/sessions/actions";
import { dirName, dirOf, isArchived } from "@/features/sessions/model";
import { sessionTitle } from "@/features/sessions/title";
import { scheduleText } from "@/lib/cron-text";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { closePalette, usePaletteOpen } from "./store";

type Kind = "all" | "session" | "matter" | "memory" | "cron";
const KINDS: readonly Kind[] = ["all", "session", "matter", "memory", "cron"];
const kindLabel = (kind: Kind) =>
  ({ all: m.palette_type_all, session: m.palette_type_session, matter: m.palette_type_matter, memory: m.palette_type_memory, cron: m.palette_type_cron })[kind]();

interface Item {
  key: string;
  icon: IconName;
  title: string;
  meta: string;
  run: () => void;
}
interface Section {
  title: string;
  items: Item[];
}

/** ⌘K 搜索弹窗（docs/web.md §3）。内容只在打开时挂着：关掉再开，输入与类型都回到初始。 */
export function Palette() {
  const open = usePaletteOpen();
  return (
    <Dialog.Root open={open} onOpenChange={(next) => !next && closePalette()}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-30 bg-[rgb(20_24_16/0.32)]" />
        <Dialog.Content
          aria-describedby={undefined}
          className="fixed top-[12vh] left-1/2 z-35 flex max-h-[min(640px,calc(100svh-96px))] w-[min(760px,calc(100vw-32px))] -translate-x-1/2 flex-col overflow-hidden rounded-dialog bg-surface text-text shadow-[0_0_0_1px_rgb(34_40_28/0.12),0_24px_48px_rgb(20_24_16/0.18)] outline-none max-[600px]:top-4 max-[600px]:max-h-[calc(100svh-32px)]"
        >
          <Dialog.Title className="sr-only">{m.palette_title()}</Dialog.Title>
          <PaletteBody />
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

const has = (text: string | undefined, q: string) => (text ?? "").toLowerCase().includes(q.toLowerCase());

/** 输入停下 `ms` 毫秒后才跟上（记忆检索走 Gateway，不逐键发）。 */
function useDebounced<T>(value: T, ms: number): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const id = setTimeout(() => setSettled(value), ms);
    return () => clearTimeout(id);
  }, [value, ms]);
  return settled;
}

function scopeName(scope: MemoryScope | undefined): string {
  if (!scope || scope.kind === "personal") return m.memory_scope_personal();
  if (scope.kind === "project") return scope.project_id ?? "";
  if (scope.kind === "environment") return m.memory_scope_environment({ id: scope.instance_id ?? "" });
  return m.memory_scope_agent({ id: scope.agent_id ?? "" });
}

function PaletteBody() {
  const navigate = useNavigate();
  const actions = useSessionActions();
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const [text, setText] = useState("");
  const [kind, setKind] = useState<Kind>("all");
  const [sel, setSel] = useState(0);
  const q = text.trim();

  // 会话已经在手里；事项与定时打开时取一次；记忆按输入去 Gateway 检索（停 200 ms 再发，"全部"取 5 条、"记忆"取 20 条）
  const sessions = useSessions(true).data;
  const matters = useMatters(true).data;
  const cron = useCronJobs().data;
  const settled = useDebounced(q, 200);
  const memoryKind = kind === "all" || kind === "memory";
  const memorySearch = useMemorySearch(settled, kind === "memory" ? 20 : 5, memoryKind);
  const memoryDone = settled === q && (memorySearch.isSuccess || memorySearch.isError);
  const memoryWaiting = q !== "" && memoryKind && !memoryDone;

  const sections = useMemo<Section[]>(() => {
    const all = kind === "all";
    const cap = all ? 5 : 30;
    const want = (k: Kind) => all || kind === k;
    const out: Section[] = [];
    const add = (title: string, items: Item[]) => {
      if (items.length) out.push({ title, items });
    };
    const sessionItem = (s: SessionSummary): Item => ({
      key: `s:${s.session}`,
      icon: "chat",
      title: sessionTitle(s),
      meta: isArchived(s) ? m.palette_meta_archived() : s.kind === "task" ? m.palette_meta_task() : dirOf(s) ? dirName(dirOf(s)) : "",
      run: () => actions.open(s.session),
    });
    const matterItem = (x: Matter): Item => ({
      key: `m:${x.id}`,
      icon: x.kind === "area" ? "clock" : "target",
      title: x.title,
      meta: x.kind === "area" ? m.palette_meta_area() : m.palette_meta_goal(),
      run: () => void navigate({ to: "/matters/$id", params: { id: x.id } }),
    });
    const cronItem = (j: CronJob): Item => ({
      key: `c:${j.id}`,
      icon: "clock",
      title: j.name,
      meta: scheduleText(j.trigger),
      run: () => void navigate({ to: "/cron/$id", params: { id: j.id } }),
    });
    const memoryItem = (x: MemoryBrief): Item => ({
      key: `y:${x.id}`,
      icon: "leaf",
      title: x.content,
      meta: scopeName(x.scope),
      run: () => void navigate({ to: "/memory/$id", params: { id: x.id } }),
    });
    const byRecent = [...(sessions ?? [])].sort((a, b) => new Date(b.updated_at).getTime() - new Date(a.updated_at).getTime());
    if (!q) {
      if (want("session")) add(all ? m.palette_sec_recent() : m.palette_sec_session(), byRecent.filter((s) => !isArchived(s)).slice(0, all ? 8 : 30).map(sessionItem));
      if (want("matter") && !all) add(m.palette_sec_matter(), (matters ?? []).filter((x) => !x.closed_at).map(matterItem));
      if (want("cron") && !all) add(m.palette_sec_cron(), (cron ?? []).map(cronItem));
      if (want("session")) add(m.palette_sec_action(), [{ key: "a:new", icon: "plus", title: m.nav_new_session(), meta: "", run: () => void actions.create() }]);
      return out;
    }
    if (want("session")) add(m.palette_sec_session(), byRecent.filter((s) => has(sessionTitle(s), q)).slice(0, cap).map(sessionItem));
    if (want("matter")) add(m.palette_sec_matter(), (matters ?? []).filter((x) => has(x.title, q)).slice(0, cap).map(matterItem));
    if (want("memory") && memoryDone) add(m.palette_sec_memory(), (memorySearch.data ?? []).slice(0, cap).map(memoryItem));
    if (want("cron")) add(m.palette_sec_cron(), (cron ?? []).filter((j) => has(j.name, q) || has(j.prompt, q)).slice(0, cap).map(cronItem));
    return out;
  }, [kind, q, sessions, matters, cron, memorySearch.data, memoryDone, actions, navigate]);
  const flat = useMemo(() => sections.flatMap((s) => s.items), [sections]);
  const active = Math.min(sel, Math.max(0, flat.length - 1));

  useEffect(() => {
    list.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: "nearest" });
  }, [active, sections]);

  const openAt = (i: number) => {
    const item = flat[i];
    if (!item) return;
    closePalette();
    item.run();
  };
  const changeKind = (next: Kind) => {
    setKind(next);
    setSel(0);
    input.current?.focus();
  };
  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    const n = flat.length;
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (n) setSel((active + (event.key === "ArrowDown" ? 1 : -1) + n) % n);
    } else if (event.key === "Enter" && !event.nativeEvent.isComposing) {
      event.preventDefault();
      openAt(active);
    }
    // 光标在输入框两头时左右键换类型，别的时候照常挪光标
    const i = KINDS.indexOf(kind);
    const el = event.currentTarget;
    if (event.key === "ArrowLeft" && el.selectionStart === 0 && el.selectionEnd === 0 && i > 0) {
      event.preventDefault();
      changeKind(KINDS[i - 1]);
    }
    if (event.key === "ArrowRight" && el.selectionStart === el.value.length && el.selectionEnd === el.value.length && i < KINDS.length - 1) {
      event.preventDefault();
      changeKind(KINDS[i + 1]);
    }
  };

  const emptyText =
    kind === "memory" && !q ? m.palette_empty_memory_hint() : memoryWaiting && !flat.length ? m.palette_searching() : q ? m.palette_empty_none() : m.palette_empty_blank();
  let n = 0;
  return (
    <>
      <div className="flex items-center gap-2.5 py-3.5 pr-3.5 pl-5.5 text-faint">
        <Icon name="search" className="shrink-0" />
        <input
          ref={input}
          autoFocus
          type="search"
          value={text}
          role="combobox"
          aria-expanded="true"
          aria-controls="palette-results"
          aria-activedescendant={flat.length ? `palette-opt-${active}` : undefined}
          aria-label={m.palette_title()}
          placeholder={m.palette_placeholder()}
          autoComplete="off"
          onChange={(event) => {
            setText(event.target.value);
            setSel(0);
          }}
          onKeyDown={onKeyDown}
          className="h-8 min-w-0 flex-1 bg-transparent text-section-title text-text outline-none placeholder:text-faint [&::-webkit-search-cancel-button]:hidden"
        />
        <IconButton name="x" label={m.palette_close()} onClick={closePalette} />
      </div>
      <div role="tablist" aria-label={m.palette_types()} className="flex gap-1 overflow-x-auto border-b border-border px-4.5 pb-3">
        {KINDS.map((k) => (
          <button
            key={k}
            type="button"
            role="tab"
            aria-selected={k === kind}
            tabIndex={-1}
            onClick={() => changeKind(k)}
            className={cn(
              "h-7.5 shrink-0 cursor-pointer rounded-control px-2.5 text-label text-muted hover:text-text",
              k === kind && "bg-active font-medium text-text",
            )}
          >
            {kindLabel(k)}
          </button>
        ))}
      </div>
      <div ref={list} id="palette-results" role="listbox" aria-label={m.palette_results()} className="min-h-30 flex-1 overflow-y-auto overscroll-contain px-2.5 pt-1 pb-2.5">
        {sections.length ? (
          <>
            {sections.map((section) => (
              <div key={section.title} role="presentation">
                <div className="px-3 pt-3.5 pb-1.5 text-small text-muted">{section.title}</div>
                {section.items.map((item) => {
                  const i = n++;
                  const on = i === active;
                  return (
                    <button
                      key={item.key}
                      id={`palette-opt-${i}`}
                      type="button"
                      role="option"
                      aria-selected={on}
                      tabIndex={-1}
                      onClick={() => openAt(i)}
                      className={cn("group flex h-10 w-full cursor-pointer items-center gap-3 rounded-inner px-3 text-left hover:bg-hover-on-white", on && "bg-active hover:bg-active")}
                    >
                      <Icon name={item.icon} className="size-4 shrink-0 text-muted" />
                      <span className="min-w-0 flex-1 truncate">{item.title}</span>
                      {item.meta && <span className="max-w-[40%] shrink-0 truncate text-small text-muted">{item.meta}</span>}
                      <span aria-hidden="true" className={cn("w-4 shrink-0 text-muted", on ? "visible" : "invisible")}>
                        ↵
                      </span>
                    </button>
                  );
                })}
              </div>
            ))}
            {memoryWaiting && <p className="px-3 py-5 text-label text-muted">{m.palette_searching_memory()}</p>}
          </>
        ) : (
          <p className="px-3 py-5 text-label text-muted">{emptyText}</p>
        )}
      </div>
      <div className="flex flex-wrap gap-x-5 gap-y-1.5 border-t border-border px-5.5 py-3 text-small text-muted max-[600px]:hidden">
        <Hint label={m.palette_key_close()} keys={["Esc"]} />
        <Hint label={m.palette_key_type()} keys={["←", "→"]} />
        <Hint label={m.palette_key_select()} keys={["↑", "↓"]} />
        <Hint label={m.palette_key_open()} keys={["↵"]} />
      </div>
    </>
  );
}

function Hint({ label, keys }: { label: string; keys: string[] }) {
  return (
    <span>
      {label}
      {keys.map((key, i) => (
        <kbd
          key={key}
          className={cn("inline-grid h-5 min-w-5 place-items-center rounded-[5px] px-1.25 font-sans text-[11px] shadow-ring", i === 0 ? "ml-1.5" : "ml-0.75")}
        >
          {key}
        </kbd>
      ))}
    </span>
  );
}
