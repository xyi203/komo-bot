import { addMemory, classifyMemory, memoryVerb, useMatters, useSessions, type MemoryVerb } from "@komo/api";
import type { Evidence, MemoryItem, MemoryKind } from "@komo/protocol";
import { Link } from "@tanstack/react-router";
import { useState, type FormEvent, type ReactNode } from "react";
import { Icon } from "@/components/icon";
import { Button } from "@/components/ui/button";
import { IconButton } from "@/components/ui/icon-button";
import { Kv, SubHeading } from "@/features/goals/parts";
import { sessionTitle } from "@/features/sessions/title";
import { errorText } from "@/lib/errors";
import { formatRelative } from "@/lib/format";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { MemoryMote } from "./mote";
import { formatDay, isLive, kindName, provenanceName, scopeFromKey, scopeName, stateName, usageName } from "./model";
import { useCommitMemory } from "./writes";

const control = "block w-full rounded-control bg-surface px-2.5 text-body text-text shadow-ring-control focus:shadow-[0_0_0_2px_var(--color-focus)] focus:outline-0";
const label = "mt-3.5 mb-1.5 block text-label font-medium";
const link = "inline-flex items-center gap-0.5 text-text no-underline hover:underline hover:underline-offset-3";

const Muted = ({ children }: { children: ReactNode }) => <span className="text-muted">{children}</span>;

function StatePill({ item }: { item: MemoryItem }) {
  return (
    <span
      className={cn(
        "ml-auto inline-flex h-5 items-center rounded-full px-1.75 text-small font-medium whitespace-nowrap",
        item.state === "contested" ? "bg-pill-bad-bg text-pill-bad-ink" : item.state === "active" ? "bg-pill-bg text-pill-ink" : "bg-pill-bg text-muted",
      )}
    >
      {stateName(item.state)}
    </span>
  );
}

function EvidenceRow({ evidence }: { evidence: Evidence }) {
  const sessions = useSessions(true).data;
  const ref = evidence.reference;
  const when = `${formatDay(evidence.observed_at)} ${new Date(evidence.observed_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hourCycle: "h23" })}`;
  if (ref.kind === "event") {
    const session = sessions?.find((s) => s.session === ref.session);
    return (
      <li className="flex flex-col gap-0.5 border-b border-rule py-2 text-label last:border-b-0">
        <span className="text-muted">
          {provenanceName(evidence.provenance)}
          <span className="mx-1.5 text-faint">·</span>
          <span className="tabular-nums">{when}</span>
        </span>
        {session ? (
          <Link to="/sessions/$sessionId" params={{ sessionId: ref.session }} className={link}>
            {sessionTitle(session)}
            <Icon name="right" className="size-3" />
          </Link>
        ) : (
          <Muted>{m.memory_evidence_session_gone()}</Muted>
        )}
      </li>
    );
  }
  return (
    <li className="flex flex-col gap-0.5 border-b border-rule py-2 text-label last:border-b-0">
      <span className="text-muted">
        {m.memory_evidence_memos()}
        <span className="mx-1.5 text-faint">·</span>
        <span className="tabular-nums">{when}</span>
      </span>
      <Muted>
        {ref.instance} / {ref.record_id}
      </Muted>
    </li>
  );
}

/** 详情顶行：窄屏左上角是返回。 */
function Top({ compact, onBack, children }: { compact: boolean; onBack: () => void; children: ReactNode }) {
  return (
    <div className="flex min-h-7 items-center gap-2">
      {compact && <IconButton name="left" label={m.memory_back()} onClick={onBack} className="-ml-2" />}
      {children}
    </div>
  );
}

/** 详情（docs/web.md §11）：正文、条件、动作、来历键值井、取代链、证据。 */
export function MemoryDetail({
  item,
  all,
  compact,
  onBack,
}: {
  item: MemoryItem;
  all: Map<string, MemoryItem>;
  compact: boolean;
  onBack: () => void;
}) {
  const commit = useCommitMemory();
  const matters = useMatters(true).data;
  const [busy, setBusy] = useState("");
  const [fail, setFail] = useState("");
  const [confirmForget, setConfirmForget] = useState(false);
  const [classify, setClassify] = useState(false);
  const [when, setWhen] = useState(item.applies_when ?? "");

  const run = async (act: string, failText: (message: string) => string, call: () => Promise<{ memory: MemoryItem }>) => {
    setBusy(act);
    setFail("");
    try {
      commit((await call()).memory);
      setConfirmForget(false);
      setClassify(false);
    } catch (error) {
      setFail(failText(errorText(error)));
    }
    setBusy("");
  };
  const verb = (v: MemoryVerb) =>
    void run(v, (message) => ({ confirm: m.memory_fail_confirm, forget: m.memory_fail_forget, restore: m.memory_fail_restore })[v]({ message }), () => memoryVerb(v, item));
  const classifyTo = (application: "convention" | "reference", appliesWhen?: string) =>
    void run("classify", (message) => m.memory_fail_classify({ message }), () => classifyMemory(item, { application, applies_when: appliesWhen }));

  const live = isLive(item);
  const unconfirmed = item.confirmation !== "user_confirmed";
  const matter = item.belongs_to ? matters?.find((x) => x.id === item.belongs_to) : undefined;
  const toConvention = item.application !== "convention";
  // 经验与事实不能改成约定（§9.2 只给偏好）
  const canClassify = item.state === "active" && !classify && !(toConvention && item.kind !== "preference");
  const evidence = item.evidence ?? [];
  const link2 = (ref: { memory: string } | undefined, text: string) => {
    if (!ref) return null;
    const target = all.get(ref.memory);
    return (
      <>
        {text}{" "}
        <Link to="/memory/$id" params={{ id: ref.memory }} className={link}>
          {target ? `${target.content.slice(0, 24)}…` : m.memory_chain_other()}
        </Link>
      </>
    );
  };
  const chain = [link2(item.supersedes, m.memory_chain_supersedes()), link2(item.superseded_by, m.memory_chain_superseded_by())].filter(Boolean);

  const source = provenanceName(item.provenance);
  const kv: [string, ReactNode][] = [
    [m.memory_kv_scope(), scopeName(item.scope)],
    [m.memory_kv_kind(), kindName(item.kind)],
    [m.memory_kv_source(), item.provenance === "user_statement" && unconfirmed ? m.memory_kv_source_auto({ source }) : source],
    [m.memory_kv_version(), m.memory_kv_revision({ revision: item.revision, status: unconfirmed ? m.memory_unconfirmed() : m.memory_confirmed() })],
    [m.memory_kv_brought(), item.usage.count ? m.memory_kv_times_last({ count: item.usage.count, when: item.usage.last_used_at ? formatRelative(item.usage.last_used_at) : "—" }) : m.memory_kv_times({ count: 0 })],
    [
      m.memory_kv_adopted(),
      item.usage.adopted_count && item.usage.last_adopted_at
        ? m.memory_kv_times_last({ count: item.usage.adopted_count, when: formatRelative(item.usage.last_adopted_at) })
        : m.memory_kv_times({ count: item.usage.adopted_count }),
    ],
    [
      m.memory_kv_noted(),
      <>
        {formatDay(item.created_at)}
        <span className="mx-1.5 text-faint">·</span>
        <span className="font-mono text-small">{item.extraction.model}</span>
        {item.extraction.prompt_version && (
          <>
            <span className="mx-1.5 text-faint">·</span>
            <span className="font-mono text-small">{item.extraction.prompt_version}</span>
          </>
        )}
      </>,
    ],
    [
      m.memory_kv_belongs(),
      matter ? (
        <>
          <Link to="/goals/$id" params={{ id: matter.id }} className={link}>
            {matter.title}
          </Link>
          {matter.closed_at && <Muted>{m.memory_matter_closed()}</Muted>}
        </>
      ) : item.belongs_to ? (
        <Muted>{m.memory_matter_gone()}</Muted>
      ) : (
        <Muted>—</Muted>
      ),
    ],
  ];
  if (item.valid_until) kv.push([m.memory_kv_until(), formatDay(item.valid_until)]);

  return (
    <div className={cn(compact ? "pt-1" : "px-4.5 pt-4 pb-4.5")}>
      <Top compact={compact} onBack={onBack}>
        <span className="inline-flex items-center gap-2 font-medium">
          <MemoryMote kind={item} />
          {usageName(item)}
        </span>
        <StatePill item={item} />
      </Top>
      <p className="mt-3 text-read leading-6 [overflow-wrap:anywhere]">{item.content}</p>
      {item.applies_when && (
        <p className="mt-2 text-label leading-5">
          <span className="mr-2 text-muted">{m.memory_applies_when()}</span>
          {item.applies_when}
        </p>
      )}
      {fail && <p className="mt-3 text-label text-negative">{fail}</p>}
      {live ? (
        <>
          <div className="mt-5 flex flex-wrap gap-2 empty:hidden">
            {unconfirmed && (
              <Button variant="primary" disabled={!!busy} onClick={() => verb("confirm")}>
                {m.memory_act_confirm({ revision: item.revision })}
              </Button>
            )}
            {canClassify && (
              <Button disabled={!!busy} onClick={() => (toConvention ? setClassify(true) : classifyTo("reference"))}>
                {toConvention ? m.memory_act_to_convention() : m.memory_act_to_reference()}
              </Button>
            )}
          </div>
          {classify && (
            <form
              className="mt-3"
              onSubmit={(ev: FormEvent) => {
                ev.preventDefault();
                classifyTo("convention", when.trim());
              }}
            >
              <label htmlFor="c-when" className="mb-1.5 block text-label font-medium">
                {m.memory_when_label()}
              </label>
              <input id="c-when" required value={when} onChange={(ev) => setWhen(ev.target.value)} placeholder={m.memory_when_placeholder()} className={cn(control, "h-9")} />
              <div className="mt-4 flex flex-wrap gap-2">
                <Button type="submit" variant="primary" disabled={!!busy}>
                  {m.memory_act_to_convention()}
                </Button>
                <Button onClick={() => setClassify(false)}>{m.cancel()}</Button>
              </div>
            </form>
          )}
          {confirmForget ? (
            <div className="mt-3 rounded-control p-3 text-label shadow-[0_0_0_1px_var(--color-negative-ring)]">
              <p>{m.memory_forget_note()}</p>
              <div className="mt-2.5 flex flex-wrap gap-2">
                <Button variant="danger" disabled={!!busy} onClick={() => verb("forget")}>
                  {m.memory_act_forget({ revision: item.revision })}
                </Button>
                <Button onClick={() => setConfirmForget(false)}>{m.cancel()}</Button>
              </div>
            </div>
          ) : (
            <button type="button" onClick={() => setConfirmForget(true)} className="mt-3 cursor-pointer text-label text-negative underline underline-offset-3">
              {m.memory_act_forget_ask()}
            </button>
          )}
        </>
      ) : item.state === "forgotten" ? (
        <div className="mt-4 flex flex-wrap gap-2">
          <Button variant="primary" disabled={!!busy} onClick={() => verb("restore")}>
            {m.memory_restore()}
          </Button>
        </div>
      ) : (
        <p className="mt-2.5 text-label leading-5 text-muted">{item.state === "archived" ? m.memory_hint_archived() : m.memory_hint_superseded()}</p>
      )}
      <Kv rows={kv} className="mt-5" />
      {chain.length > 0 && (
        <p className="mt-3 text-label leading-5 text-muted">
          {chain.map((node, i) => (
            <span key={i} className="block">
              {node}
            </span>
          ))}
        </p>
      )}
      <SubHeading count={evidence.length}>{m.memory_evidence()}</SubHeading>
      {evidence.length ? (
        <ul>
          {evidence.map((e, i) => (
            <EvidenceRow key={i} evidence={e} />
          ))}
        </ul>
      ) : (
        <p className="mt-2.5 text-label leading-5 text-muted">{m.memory_evidence_none()}</p>
      )}
    </div>
  );
}

/** 记一条：在详情位置打开的表单。这里记的不带会话，按 §9.3 记成已确认。 */
export function AddForm({
  scopes,
  compact,
  onClose,
  onAdded,
}: {
  scopes: [string, string][];
  compact: boolean;
  onClose: () => void;
  onAdded: (item: MemoryItem) => void;
}) {
  const commit = useCommitMemory();
  const [content, setContent] = useState("");
  const [kind, setKind] = useState<MemoryKind>("preference");
  const [scope, setScope] = useState("personal");
  const [application, setApplication] = useState<"reference" | "convention">("reference");
  const [when, setWhen] = useState("");
  const [busy, setBusy] = useState(false);
  const [fail, setFail] = useState("");

  const submit = async (ev: FormEvent) => {
    ev.preventDefault();
    setBusy(true);
    setFail("");
    try {
      const { memory } = await addMemory({
        content: content.trim(),
        kind,
        scope: scopeFromKey(scope),
        application,
        applies_when: application === "convention" ? when.trim() : undefined,
      });
      commit(memory);
      onAdded(memory);
    } catch (error) {
      setFail(m.memory_fail_add({ message: errorText(error) }));
    }
    setBusy(false);
  };

  return (
    <form onSubmit={(ev) => void submit(ev)} className={cn(compact ? "max-w-160 pt-1" : "px-4.5 pt-4 pb-4.5")}>
      <Top compact={compact} onBack={onClose}>
        <h2 className="m-0 flex-1 text-section-title font-semibold">{m.memory_add_title()}</h2>
        {!compact && <IconButton name="x" label={m.close()} onClick={onClose} className="-mr-2" />}
      </Top>
      <p className="mt-2.5 text-label leading-5 text-muted">{m.memory_add_hint()}</p>
      <label htmlFor="f-content" className={label}>
        {m.memory_add_content()}
      </label>
      <textarea
        id="f-content"
        autoFocus
        rows={3}
        maxLength={300}
        required
        value={content}
        onChange={(ev) => setContent(ev.target.value)}
        placeholder={m.memory_add_content_ph()}
        className={cn(control, "resize-y py-2")}
      />
      <div className="grid grid-cols-2 gap-3 max-[600px]:grid-cols-1">
        <div>
          <label htmlFor="f-kind" className={label}>
            {m.memory_add_kind()}
          </label>
          <select id="f-kind" value={kind} onChange={(ev) => setKind(ev.target.value as MemoryKind)} className={cn(control, "h-9 cursor-pointer")}>
            {(["preference", "fact", "experience"] as const).map((k) => (
              <option key={k} value={k}>
                {kindName(k)}
              </option>
            ))}
          </select>
        </div>
        <div>
          <label htmlFor="f-scope" className={label}>
            {m.memory_add_scope()}
          </label>
          <select id="f-scope" value={scope} onChange={(ev) => setScope(ev.target.value)} className={cn(control, "h-9 cursor-pointer")}>
            {scopes.map(([key, name]) => (
              <option key={key} value={key}>
                {name}
              </option>
            ))}
          </select>
        </div>
      </div>
      <fieldset className="m-0 mt-3.5 border-0 p-0">
        <legend className="mb-1.5 p-0 text-label font-medium">{m.memory_add_application()}</legend>
        {(
          [
            ["reference", m.memory_add_reference()],
            ["convention", m.memory_add_convention()],
          ] as const
        ).map(([value, text]) => (
          <label key={value} className="flex min-h-7.5 cursor-pointer items-center gap-2 text-label">
            <input type="radio" name="application" value={value} checked={application === value} onChange={() => setApplication(value)} className="m-0 accent-primary" />
            {text}
          </label>
        ))}
      </fieldset>
      {application === "convention" && (
        <div>
          <label htmlFor="f-when" className={label}>
            {m.memory_when_label()}
          </label>
          <input id="f-when" required value={when} onChange={(ev) => setWhen(ev.target.value)} placeholder={m.memory_when_placeholder()} className={cn(control, "h-9")} />
        </div>
      )}
      {fail && <p className="mt-3 text-label text-negative">{fail}</p>}
      <div className="mt-4 flex flex-wrap gap-2">
        <Button type="submit" variant="primary" disabled={busy}>
          {busy ? m.memory_add_busy() : m.memory_add_submit()}
        </Button>
        <Button onClick={onClose}>{m.cancel()}</Button>
      </div>
    </form>
  );
}
