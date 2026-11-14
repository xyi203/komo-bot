import { useChats, useMatters, useSessionSearch } from "@komo/api";
import * as Dialog from "@radix-ui/react-dialog";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { Icon, type IconName } from "@/components/icon";
import { formatChatTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { firstLine } from "@/features/goals/model";
import { closePalette, usePaletteOpen } from "./store";

/** 没输入时列最近的几段对话。 */
const RECENT = 8;
/** 输入后每组最多几条。 */
const CAP = 8;

interface Item {
  key: string;
  icon: IconName;
  title: string;
  /** 第二行：最后一句话 / 命中的片段 / 目标的下一步。 */
  detail: string;
  /** 右侧：时间。 */
  stamp: string;
  run: () => void;
}
interface Section {
  title: string;
  items: Item[];
}

/**
 * ⌘K 搜索弹窗（docs/redesign.md P4，docs/web.md §3）：浮在当前页上（窄屏铺满全屏）。没输入时列最近的对话；输入后分「对话」「目标」两组，
 * 回车或点击进入。内容只在打开时挂着：关掉再开，输入回到初始。记忆不进搜索结果（待定）。
 */
export function Palette() {
  const open = usePaletteOpen();
  return (
    <Dialog.Root open={open} onOpenChange={(next) => !next && closePalette()}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-30 bg-[rgb(0_0_0/0.08)]" />
        <Dialog.Content
          aria-describedby={undefined}
          className="fixed top-[18vh] left-1/2 z-35 flex max-h-[min(560px,calc(100svh-96px))] w-[min(560px,calc(100vw-32px))] -translate-x-1/2 flex-col overflow-hidden rounded-[20px] bg-surface p-2 text-text shadow-[0_0_0_1px_rgb(0_0_0/0.08),0_16px_48px_rgb(0_0_0/0.16)] outline-none max-[900px]:top-[var(--app-top,0px)] max-[900px]:left-0 max-[900px]:h-[var(--app-height,100dvh)] max-[900px]:max-h-none max-[900px]:w-full max-[900px]:translate-x-0 max-[900px]:rounded-none max-[900px]:pb-[env(safe-area-inset-bottom)] max-[900px]:shadow-none"
        >
          <Dialog.Title className="sr-only">{m.palette_title()}</Dialog.Title>
          <PaletteBody />
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

const has = (text: string | undefined, q: string) => (text ?? "").toLowerCase().includes(q.toLowerCase());

/** 输入停下 `ms` 毫秒后才跟上（对话检索走 Gateway，不逐键发）。 */
function useDebounced<T>(value: T, ms: number): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const id = setTimeout(() => setSettled(value), ms);
    return () => clearTimeout(id);
  }, [value, ms]);
  return settled;
}

function PaletteBody() {
  const navigate = useNavigate();
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const [text, setText] = useState("");
  const [sel, setSel] = useState(0);
  const q = text.trim();

  const chats = useChats().data?.chats;
  const matters = useMatters(true).data;
  const settled = useDebounced(q, 200);
  const search = useSessionSearch(settled, CAP);
  const searchDone = settled === q && (search.isSuccess || search.isError);
  const waiting = q !== "" && !searchDone;

  const sections = useMemo<Section[]>(() => {
    const chatTitle = (main: boolean, title: string) => (main ? m.chats_main() : title || m.session_untitled());
    const openChat = (main: boolean, id: string) => () => void (main ? navigate({ to: "/" }) : navigate({ to: "/chats/$id", params: { id } }));
    if (!q) {
      const items = (chats ?? []).slice(0, RECENT).map(
        (c): Item => ({
          key: `c:${c.id}`,
          icon: "chat",
          title: chatTitle(c.main, c.title),
          detail: c.lastMessage || m.chats_no_message(),
          stamp: formatChatTime(c.lastMessageAt),
          run: openChat(c.main, c.id),
        }),
      );
      return items.length ? [{ title: m.palette_sec_recent(), items }] : [];
    }
    const mainId = chats?.find((c) => c.main)?.id;
    const chatItems = (search.data ?? []).map(
      (hit): Item => ({
        key: `c:${hit.session}`,
        icon: "chat",
        title: chatTitle(hit.kind === "main" || hit.session === mainId, hit.title),
        detail: hit.snippets[0] ?? "",
        stamp: formatChatTime(hit.matched_at),
        run: openChat(hit.kind === "main" || hit.session === mainId, hit.session),
      }),
    );
    const goalItems = (matters ?? [])
      .filter((x) => has(x.title, q))
      .slice(0, CAP)
      .map(
        (x): Item => ({
          key: `g:${x.id}`,
          icon: "target",
          title: x.title,
          detail: firstLine(x.description) || (x.next ?? x.standard ?? ""),
          stamp: "",
          run: () => void navigate({ to: "/goals/$id", params: { id: x.id } }),
        }),
      );
    return [
      { title: m.palette_sec_chats(), items: chatItems },
      { title: m.palette_sec_goals(), items: goalItems },
    ].filter((section) => section.items.length);
  }, [q, chats, matters, search.data, navigate]);
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
  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    const n = flat.length;
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (n) setSel((active + (event.key === "ArrowDown" ? 1 : -1) + n) % n);
    } else if (event.key === "Enter" && !event.nativeEvent.isComposing) {
      event.preventDefault();
      openAt(active);
    }
  };

  const emptyText =
    waiting && !flat.length
      ? m.palette_searching()
      : q
        ? search.isError && searchDone
          ? m.palette_error()
          : m.palette_empty_none({ query: q })
        : m.palette_empty_blank();
  let n = 0;
  return (
    <>
      <div className="flex items-center gap-2.5 border-b border-rule px-3 pt-2.5 pb-3.5 text-faint">
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
          className="h-7 min-w-0 flex-1 bg-transparent text-section-title text-text outline-none placeholder:text-faint [&::-webkit-search-cancel-button]:hidden"
        />
        {/* 窄屏铺满全屏，没有遮罩可点，补一个取消 */}
        <button type="button" onClick={closePalette} className="-mr-1 hidden h-11 shrink-0 cursor-pointer px-2 text-body text-text narrow:block">
          {m.cancel()}
        </button>
      </div>
      <div ref={list} id="palette-results" role="listbox" aria-label={m.palette_results()} className="min-h-0 flex-1 overflow-y-auto overscroll-contain">
        {sections.length ? (
          sections.map((section) => (
            <div key={section.title} role="presentation">
              <h3 className="mx-3 mt-2.5 mb-1 text-small font-normal text-faint">{section.title}</h3>
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
                    onMouseMove={() => i !== active && setSel(i)}
                    className={cn("grid w-full cursor-pointer grid-cols-[20px_minmax(0,1fr)_auto] items-center gap-x-3 rounded-[12px] px-3 py-2 text-left", on && "bg-ground")}
                  >
                    <Icon name={item.icon} className="row-span-2 size-4 text-muted" />
                    <span className="truncate">{item.title}</span>
                    <span className="row-span-2 col-start-3 text-small text-faint tabular-nums">{item.stamp}</span>
                    {item.detail && <span className="col-start-2 truncate text-small leading-4.5 text-faint">{item.detail}</span>}
                  </button>
                );
              })}
            </div>
          ))
        ) : (
          <p className="px-3 py-5 text-center text-label text-faint">{emptyText}</p>
        )}
      </div>
    </>
  );
}
