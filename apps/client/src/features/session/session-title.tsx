import type { SessionSummary } from "@komo/protocol";
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { useSessionActions } from "@/features/sessions/actions";
import { sessionTitle } from "@/features/sessions/title";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";

const text = "text-[15px] leading-5 font-medium text-text";

/**
 * 顶栏的会话标题，点一下就地改名（与侧栏菜单的"重命名"同一个动作，`POST /v1/sessions/{id}/name`）：
 * 框随字宽伸缩，Enter 或失焦存，Esc 放弃；空名或没变不发请求。会话还没读到时只显示应用名。
 */
export function SessionTitle({ session, fallback }: { session: SessionSummary | undefined; fallback: string }) {
  const [editing, setEditing] = useState(false);
  if (!session) return <h1 className={cn("min-w-0 truncate", text)}>{fallback}</h1>;
  const title = sessionTitle(session);
  return (
    <h1 className={cn("-ml-1.5 flex min-w-0", text)}>
      {editing ? (
        <TitleInput session={session} onDone={() => setEditing(false)} />
      ) : (
        <button
          type="button"
          title={m.session_rename_hint({ title })}
          onClick={() => setEditing(true)}
          className="h-7 min-w-0 cursor-pointer truncate rounded-control px-1.5 text-left transition-colors hover:bg-hover-on-white"
        >
          {title}
        </button>
      )}
    </h1>
  );
}

function TitleInput({ session, onDone }: { session: SessionSummary; onDone: () => void }) {
  const actions = useSessionActions();
  const input = useRef<HTMLInputElement>(null);
  const finished = useRef(false);
  const [value, setValue] = useState(session.title ?? "");
  useEffect(() => {
    input.current?.focus();
    input.current?.select();
  }, []);
  const finish = (save: boolean) => {
    if (finished.current) return;
    finished.current = true;
    if (save) void actions.rename(session, value);
    onDone();
  };
  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter" && !event.nativeEvent.isComposing) {
      event.preventDefault();
      finish(true);
    } else if (event.key === "Escape") {
      event.preventDefault();
      finish(false);
    }
  };
  const placeholder = sessionTitle(session);
  // 一个看不见的同字副本撑出宽度，输入框叠在它上面：框随字宽伸缩，最宽到顶栏让出的地方为止
  return (
    <span className="inline-grid h-7 min-w-24 max-w-full rounded-control bg-surface shadow-[0_0_0_2px_var(--color-focus)]">
      <span aria-hidden="true" className="invisible col-start-1 row-start-1 overflow-hidden px-1.5 leading-7 whitespace-pre">
        {value || placeholder}
      </span>
      <input
        ref={input}
        value={value}
        placeholder={placeholder}
        aria-label={m.session_rename_label()}
        maxLength={120}
        // 输入框自带约 20 个字的固有宽度，会把网格列撑开；宽度交给上面的副本
        size={1}
        onChange={(event) => setValue(event.target.value)}
        onKeyDown={onKeyDown}
        onBlur={() => finish(true)}
        className="col-start-1 row-start-1 w-full min-w-0 bg-transparent px-1.5 outline-none placeholder:text-faint"
      />
    </span>
  );
}
