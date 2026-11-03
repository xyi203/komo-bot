import { deleteEnv, putEnv, useConfigEnv } from "@komo/api";
import type { EnvVarEntry } from "@komo/protocol";
import { useRef, useState } from "react";
import { Icon } from "@/components/icon";
import { BlockError, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { IconButton } from "@/components/ui/icon-button";
import { errorText } from "@/lib/errors";
import { formatList } from "@/lib/format";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { AddCard, FormError, KState, TextButton, TextInput } from "../controls";
import { EditNoticeView } from "../rich";
import { SectionHead } from "../section-head";
import { useConfigEdit } from "../use-config-edit";

/**
 * 环境变量（docs/web.md §15）：API key 与其他凭证。**值只能写入，任何地方都不显示**：
 * Gateway 的 `GET /v1/config/env` 本来就不带值，输入框都是密码框。
 */
export function EnvSection() {
  const env = useConfigEnv();
  const edit = useConfigEdit();
  // 正在改值的变量名 / 正在确认删除的变量名 / 添加表单是否展开
  const [editing, setEditing] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);

  const head = <SectionHead title={m.settings_sec_env()} description={m.env_desc()} />;
  if (env.isError) {
    return (
      <>
        {head}
        <BlockError message={m.env_load_error({ message: errorText(env.error) })} onRetry={() => void env.refetch()} />
      </>
    );
  }
  if (!env.data) {
    return (
      <>
        {head}
        <SkeletonRows count={4} />
      </>
    );
  }
  const vars = env.data.vars;
  const disabled = edit.busy !== "";

  const saveOne = async (name: string, value: string) => {
    const ok = await edit.run(`env:${name}`, (version) => putEnv(version, [{ name, value }]), m.env_done_set({ name }));
    if (ok) setEditing(null);
  };
  const saveMany = async (items: { name: string; value: string }[]) => {
    const ok = await edit.run(
      "env:new",
      (version) => putEnv(version, items),
      items.length === 1 ? m.env_done_set({ name: items[0].name }) : m.env_done_set_many({ count: items.length }),
    );
    if (ok) setAdding(false);
  };
  const remove = async (name: string) => {
    // 失败原因（还有谁在用它）是 Gateway 给的：写在这一节顶上的回执里，收起确认
    await edit.run(`env-del:${name}`, (version) => deleteEnv(version, name), m.env_done_delete({ name }), { failAsNotice: true });
    setConfirming(null);
  };

  return (
    <>
      {head}
      <EditNoticeView notice={edit.notice} />
      {vars.length > 0 && (
        <ul className="mb-3 overflow-hidden rounded-card bg-surface shadow-lift">
          {vars.map((variable) => (
            <EnvRow
              key={variable.name}
              variable={variable}
              editing={editing === variable.name}
              confirming={confirming === variable.name}
              busy={edit.busy}
              failure={edit.failure}
              disabled={disabled}
              onEdit={() => {
                setEditing(variable.name);
                setConfirming(null);
                edit.setFailure(null);
              }}
              onCancelEdit={() => setEditing(null)}
              onSave={(value) => void saveOne(variable.name, value)}
              onAskDelete={() => {
                setConfirming(variable.name);
                setEditing(null);
              }}
              onCancelDelete={() => setConfirming(null)}
              onDelete={() => void remove(variable.name)}
            />
          ))}
        </ul>
      )}
      {adding ? (
        <AddForm busy={edit.busy === "env:new"} failure={edit.failure?.key === "env:new" ? edit.failure.text : ""} onCancel={() => setAdding(false)} onSave={(items) => void saveMany(items)} />
      ) : (
        <AddCard
          onClick={() => {
            setAdding(true);
            setConfirming(null);
            edit.setFailure(null);
          }}
        >
          <Icon name="plus" className="size-3.5" />
          {m.env_add()}
        </AddCard>
      )}
    </>
  );
}

function EnvRow({
  variable,
  editing,
  confirming,
  busy,
  failure,
  disabled,
  onEdit,
  onCancelEdit,
  onSave,
  onAskDelete,
  onCancelDelete,
  onDelete,
}: {
  variable: EnvVarEntry;
  editing: boolean;
  confirming: boolean;
  busy: string;
  failure: { key: string; text: string } | null;
  disabled: boolean;
  onEdit: () => void;
  onCancelEdit: () => void;
  onSave: (value: string) => void;
  onAskDelete: () => void;
  onCancelDelete: () => void;
  onDelete: () => void;
}) {
  // 没人用时 Gateway 省掉这个字段
  const usedBy = variable.used_by ?? [];
  const [value, setValue] = useState("");
  const mine = failure?.key === `env:${variable.name}`;
  return (
    <li className="border-t border-rule px-4 py-2.5 first:border-t-0 max-[600px]:px-3">
      <div className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-3">
        <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
          <span className="font-mono font-medium [overflow-wrap:anywhere]">{variable.name}</span>
          {variable.set ? <KState tone="ok">{m.env_state_set()}</KState> : <KState tone="warn">{variable.in_file ? m.env_state_empty() : m.env_state_missing()}</KState>}
          {usedBy.length > 0 && <span className="text-small text-muted">{m.env_used_by({ names: formatList(usedBy) })}</span>}
        </div>
        {!editing && (
          <span className="flex items-center gap-2">
            <TextButton quiet disabled={disabled} onClick={onEdit}>
              {variable.in_file ? m.env_change() : m.env_set()}
            </TextButton>
            {variable.in_file && (
              <IconButton name="trash" size="compact" label={m.env_delete_label({ name: variable.name })} disabled={disabled} onClick={onAskDelete} className="hover:text-negative" />
            )}
          </span>
        )}
      </div>
      {editing && (
        <form
          className="mt-2.5 flex flex-wrap items-center gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            onSave(value);
          }}
        >
          <TextInput
            autoFocus
            type="password"
            autoComplete="new-password"
            required
            aria-label={variable.name}
            className="min-w-40 flex-1 tabular-nums"
            placeholder={variable.set ? m.env_value_replace() : m.env_value()}
            value={value}
            onChange={(event) => setValue(event.target.value)}
          />
          <Button size="sm" variant="primary" type="submit" disabled={disabled}>
            {busy === `env:${variable.name}` ? m.controls_saving() : m.controls_save()}
          </Button>
          <Button size="sm" onClick={onCancelEdit}>
            {m.cancel()}
          </Button>
          {mine && <FormError className="mt-0 basis-full">{failure?.text}</FormError>}
        </form>
      )}
      {confirming && (
        <div className="mt-2.5 rounded-control p-3 text-label shadow-[0_0_0_1px_var(--color-negative-ring)]">
          <p>{usedBy.length ? m.env_delete_in_use({ names: formatList(usedBy) }) : m.env_delete_ask()}</p>
          <div className="mt-2.5 flex flex-wrap gap-2">
            <Button variant="danger" size="sm" disabled={disabled} onClick={onDelete}>
              {m.env_delete_sure({ name: variable.name })}
            </Button>
            <Button size="sm" onClick={onCancelDelete}>
              {m.cancel()}
            </Button>
          </div>
        </div>
      )}
    </li>
  );
}

interface Pair {
  id: number;
  name: string;
  value: string;
}

/** 把整段 `.env` 拆成一行一个变量：`#` 开头的跳过，去掉 `export` 与值两边的引号。 */
export function parseEnvText(text: string): { name: string; value: string }[] {
  return text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line && !line.startsWith("#") && line.includes("="))
    .map((line) => {
      const [name, ...rest] = line.replace(/^export\s+/, "").split("=");
      return { name: name.trim(), value: rest.join("=").trim().replace(/^(['"])(.*)\1$/, "$2") };
    });
}

function AddForm({ busy, failure, onCancel, onSave }: { busy: boolean; failure: string; onCancel: () => void; onSave: (items: { name: string; value: string }[]) => void }) {
  const next = useRef(1);
  const [pairs, setPairs] = useState<Pair[]>([{ id: 0, name: "", value: "" }]);
  const patch = (id: number, change: Partial<Pair>) => setPairs((current) => current.map((p) => (p.id === id ? { ...p, ...change } : p)));
  const blank = () => ({ id: next.current++, name: "", value: "" });

  const paste = (id: number, text: string): boolean => {
    if (!/[=\n]/.test(text)) return false;
    const rows = parseEnvText(text);
    if (!rows.length) return false;
    setPairs((current) => {
      const at = current.findIndex((p) => p.id === id);
      const target = current[at];
      const added = rows.map((row) => ({ id: next.current++, ...row }));
      // 粘进的是空行就替换它，否则插在它后面
      return [...current.slice(0, at), ...added, ...current.slice(at + (target.name || target.value ? 0 : 1))];
    });
    return true;
  };

  return (
    <form
      className="rounded-card bg-surface p-4 shadow-lift max-[600px]:p-3"
      onSubmit={(event) => {
        event.preventDefault();
        const items = pairs.map((p) => ({ name: p.name.trim(), value: p.value })).filter((p) => p.name || p.value);
        if (items.length) onSave(items);
      }}
    >
      {pairs.map((pair) => (
        <div key={pair.id} className="mb-3 grid grid-cols-[auto_minmax(0,1fr)_44px] overflow-hidden rounded-inner shadow-ring">
          <label htmlFor={`env-n-${pair.id}`} className="grid min-w-16 place-items-center-safe px-3 text-label text-muted">
            {m.env_name()}
          </label>
          <input
            id={`env-n-${pair.id}`}
            className="h-11 min-w-0 bg-transparent px-3.5 font-mono text-label outline-none placeholder:text-faint focus-visible:outline-offset-[-2px]"
            value={pair.name}
            placeholder="MY_API_KEY"
            autoComplete="off"
            spellCheck={false}
            onChange={(event) => patch(pair.id, { name: event.target.value })}
            onPaste={(event) => {
              if (paste(pair.id, event.clipboardData.getData("text"))) event.preventDefault();
            }}
          />
          <IconButton
            name="trash"
            label={m.env_row_remove()}
            className="self-center justify-self-center hover:text-negative"
            onClick={() => setPairs((current) => (current.length > 1 ? current.filter((p) => p.id !== pair.id) : [blank()]))}
          />
          <label htmlFor={`env-v-${pair.id}`} className="grid min-w-16 place-items-center-safe border-t border-border px-3 text-label text-muted">
            {m.env_value_label()}
          </label>
          <input
            id={`env-v-${pair.id}`}
            type="password"
            autoComplete="new-password"
            className={cn("col-span-2 h-11 min-w-0 border-t border-border bg-transparent px-3.5 tabular-nums outline-none placeholder:text-faint focus-visible:outline-offset-[-2px]")}
            value={pair.value}
            placeholder={m.env_value()}
            onChange={(event) => patch(pair.id, { value: event.target.value })}
          />
        </div>
      ))}
      <div className="flex flex-wrap items-center gap-2.5 text-label text-muted">
        <Button size="sm" onClick={() => setPairs((current) => [...current, blank()])}>
          <Icon name="plus" className="size-3.5" />
          {m.env_row_add()}
        </Button>
        <span>{m.env_paste_hint()}</span>
      </div>
      {failure && <FormError>{failure}</FormError>}
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onCancel}>{m.cancel()}</Button>
        <Button variant="primary" type="submit" disabled={busy}>
          {busy ? m.controls_saving() : m.controls_save()}
        </Button>
      </div>
    </form>
  );
}
