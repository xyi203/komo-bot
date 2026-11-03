import { answerIntervention, keys, useInterventions, useSessions } from "@komo/api";
import type { InterventionAnswerResponse, InterventionSummary, InterventionVerdict, SessionSummary } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { Fragment, useEffect, useMemo, useState, type ReactNode } from "react";
import { Icon } from "@/components/icon";
import { BlockError, DetailCard, EmptyFrame, Page, PageBody, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { IconButton } from "@/components/ui/icon-button";
import { sessionTitle } from "@/features/sessions/title";
import { errorText } from "@/lib/errors";
import { formatRelative } from "@/lib/format";
import { useMediaQuery } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";

const kindLabel = (kind: string) =>
  kind === "confirm" ? m.inbox_kind_confirm() : kind === "verify" ? m.inbox_kind_verify() : kind === "blocked" ? m.inbox_kind_blocked() : kind;

/** 答案按钮的字：照抄 Gateway 给的 `verdicts`，不认识的原样写。 */
function verdictLabel(verdict: InterventionVerdict): string {
  switch (verdict) {
    case "approve":
      return m.inbox_verdict_approve();
    case "reject":
      return m.inbox_verdict_reject();
    case "satisfied":
      return m.inbox_verdict_satisfied();
    case "not_performed":
      return m.inbox_verdict_not_performed();
    case "resolve":
      return m.inbox_verdict_resolve();
    case "abandon":
      return m.inbox_verdict_abandon();
    default:
      return verdict;
  }
}

// 已答复的回执（Gateway 回的那句话）按句柄记在这一次页面访问里：列表重取后这条就不在清单里了，详情还要写出结果。
const answered = new Map<string, InterventionAnswerResponse>();

/**
 * 待处理（docs/web.md §14）：无人值守的 Run 停下来等人的地方。列表在左，详情在右（宽屏）；选中的句柄在地址里
 * （`/inbox/$handle`），窄屏（≤1180）详情盖住列表，左上角返回。
 */
export function InboxPage({ selected }: { selected: string | null }) {
  const query = useInterventions();
  const sessions = useSessions(true).data;
  const navigate = useNavigate();
  const compact = useMediaQuery("(max-width: 1180px)");
  const items = useMemo(() => query.data?.interventions ?? [], [query.data]);
  const ready = query.isSuccess;
  const current = items.find((x) => x.handle === selected);
  const receipt = selected ? answered.get(selected) : undefined;

  // 宽屏没选就选第一条（换地址不留历史）
  useEffect(() => {
    if (!compact && ready && !selected && items.length) void navigate({ to: "/inbox/$id", params: { id: items[0].handle }, replace: true });
  }, [compact, ready, selected, items, navigate]);

  const sessionOf = (id: string) => sessions?.find((s) => s.session === id);
  const select = (handle: string) => void navigate({ to: "/inbox/$id", params: { id: handle } });
  const back = () => void navigate({ to: "/inbox" });

  const showDetail = !!selected || (!compact && items.length > 0);
  const detail = (
    <DetailCard label={m.inbox_detail()} compact={compact}>
      {!ready ? (
        query.isError ? null : (
          <div className="p-4">
            <SkeletonRows count={3} className="h-9" />
          </div>
        )
      ) : receipt ? (
        <div className="px-4.5 pt-4 pb-4.5">
          <DetailTop label={m.inbox_answered()} compact={compact} onBack={back} />
          <p className="mt-3 text-read leading-6">{receipt.note}</p>
        </div>
      ) : current ? (
        <Detail key={current.handle} item={current} session={sessionOf(current.session)} compact={compact} onBack={back} />
      ) : (
        <EmptyFrame className="m-4">{items.length ? m.inbox_pick() : m.inbox_none_to_answer()}</EmptyFrame>
      )}
    </DetailCard>
  );

  // 窄屏选中了一条：只给详情（页头与列表都收起）
  if (compact && selected) return (
      <Page title={m.nav_inbox()} bare>
        {detail}
      </Page>
    );

  const n = items.length;
  const finding = ready ? (n ? m.inbox_finding({ count: n }) : m.inbox_finding_none()) : query.isError ? m.inbox_finding_error() : undefined;
  return (
    <Page title={m.nav_inbox()} finding={finding}>
      <PageBody columns={compact || !showDetail ? "grid-cols-[minmax(0,1fr)]" : "grid-cols-[minmax(0,760px)_360px]"}>
        <section aria-label={m.inbox_list()} className="min-w-0">
          {query.isError ? (
            <BlockError message={m.inbox_load_error({ message: errorText(query.error) })} onRetry={() => void query.refetch()} />
          ) : !ready ? (
            <SkeletonRows />
          ) : n === 0 ? (
            <Explainer />
          ) : (
            <ul className="relative z-0">
              {items.map((item) => (
                <Row key={item.handle} item={item} session={sessionOf(item.session)} on={item.handle === selected} onSelect={select} />
              ))}
            </ul>
          )}
        </section>
        {showDetail && !compact && detail}
      </PageBody>
    </Page>
  );
}

function Row({
  item,
  session,
  on,
  onSelect,
}: {
  item: InterventionSummary;
  session: SessionSummary | undefined;
  on: boolean;
  onSelect: (handle: string) => void;
}) {
  return (
    <li
      className={cn(
        "relative grid grid-cols-[16px_minmax(0,1fr)] gap-x-3 border-b border-rule py-3 before:absolute before:inset-x-[-8px] before:inset-y-px before:-z-10 before:rounded-control before:transition-colors hover:before:bg-hover-on-white",
        on && "before:bg-active hover:before:bg-active",
      )}
    >
      <span
        aria-hidden="true"
        className={cn("mt-1.75 size-2 justify-self-center rounded-full", item.kind === "confirm" ? "border-[1.5px] border-pill-wait-ink" : "bg-negative")}
      />
      <button
        type="button"
        aria-current={on ? "true" : undefined}
        onClick={() => onSelect(item.handle)}
        className="flex min-w-0 cursor-pointer flex-col gap-1 text-left text-text"
      >
        <span className="leading-5.5">{item.question}</span>
        <span className="text-small text-muted">
          {kindLabel(item.kind)}
          <Sep />
          {session ? sessionTitle(session) : m.inbox_session_gone()}
          <Sep />
          <span className="tabular-nums">{formatRelative(item.created_at)}</span>
        </span>
      </button>
    </li>
  );
}

const Sep = () => <span className="mx-1.5 text-faint">·</span>;

/** 一句话里的标识符（`home_chat`、命令）按代码样式写。 */
function withCode(text: string): ReactNode {
  return text.split(/(home_chat|komo intervention list)/).map((part, i) => (i % 2 ? <code key={i} className="font-mono text-[0.92em]">{part}</code> : <Fragment key={i}>{part}</Fragment>));
}

/** 没有待处理时：说明哪三种情况会停在这里。 */
function Explainer() {
  const terms: [string, string][] = [
    [m.inbox_kind_confirm(), m.inbox_explain_confirm()],
    [m.inbox_kind_verify_short(), m.inbox_explain_verify()],
    [m.inbox_kind_blocked(), m.inbox_explain_blocked()],
  ];
  return (
    <>
      <EmptyFrame>{m.inbox_empty()}</EmptyFrame>
      <h2 className="mt-6 text-section-title font-semibold">{m.inbox_explain_title()}</h2>
      <dl className="mt-2 grid max-w-190 grid-cols-[max-content_minmax(0,1fr)] gap-x-6 text-body leading-5.5 max-[600px]:gap-x-4">
        {terms.map(([term, text]) => (
          <Fragment key={term}>
            <dt className="border-b border-rule py-2 text-muted">{term}</dt>
            <dd className="m-0 border-b border-rule py-2">{text}</dd>
          </Fragment>
        ))}
      </dl>
      <p className="mt-2.5 text-label leading-5 text-muted">{withCode(m.inbox_explain_hint())}</p>
    </>
  );
}

function DetailTop({ label, compact, onBack }: { label: string; compact: boolean; onBack: () => void }) {
  return (
    <div className="flex min-h-7 items-center gap-2">
      {compact && <IconButton name="left" label={m.inbox_back()} onClick={onBack} className="-ml-2" />}
      <span className="font-medium">{label}</span>
    </div>
  );
}

function Detail({ item, session, compact, onBack }: { item: InterventionSummary; session: SessionSummary | undefined; compact: boolean; onBack: () => void }) {
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState<InterventionVerdict | null>(null);
  const [fail, setFail] = useState("");
  const [confirming, setConfirming] = useState(false);

  const answer = async (verdict: InterventionVerdict) => {
    setBusy(verdict);
    setFail("");
    try {
      const response = await answerIntervention(item.handle, { verdict, request_key: `web:answer:${item.handle}:${verdict}` });
      answered.set(item.handle, response);
      setConfirming(false);
      await queryClient.refetchQueries({ queryKey: keys.interventions });
    } catch (error) {
      setFail(m.inbox_answer_error({ message: errorText(error) }));
    }
    setBusy(null);
  };

  return (
    <div className={cn(compact ? "pt-1" : "px-4.5 pt-4 pb-4.5")}>
      <DetailTop label={kindLabel(item.kind)} compact={compact} onBack={onBack} />
      <p className="mt-3 text-read leading-6">{item.question}</p>
      {fail && <p className="mt-3 text-label text-negative">{fail}</p>}
      <div className="mt-4 flex flex-wrap gap-2">
        {item.verdicts.map((verdict, i) =>
          confirming && verdict === "abandon" ? null : (
            <Button
              key={verdict}
              variant={i === 0 ? "primary" : verdict === "abandon" ? "danger" : "default"}
              disabled={busy !== null}
              // "到此为止"不可撤回，先要二次确认；其余答案一点就发
              onClick={() => (verdict === "abandon" ? setConfirming(true) : void answer(verdict))}
            >
              {verdictLabel(verdict)}
            </Button>
          ),
        )}
      </div>
      {confirming && (
        <div className="mt-3 rounded-control p-3 text-label shadow-[0_0_0_1px_var(--color-negative-ring)]">
          <p>{m.inbox_abandon_ask()}</p>
          <div className="mt-2.5 flex flex-wrap gap-2">
            <Button variant="danger" disabled={busy !== null} onClick={() => void answer("abandon")}>
              {m.inbox_abandon_sure()}
            </Button>
            <Button onClick={() => setConfirming(false)}>{m.cancel()}</Button>
          </div>
        </div>
      )}
      <dl className="mt-4.5 grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 rounded-control bg-raised px-3 py-2.5 text-label leading-5">
        <dt className="py-1 text-muted">{m.inbox_kv_session()}</dt>
        <dd className="m-0 min-w-0 py-1 [overflow-wrap:anywhere]">
          {session ? (
            <Link to="/sessions/$sessionId" params={{ sessionId: item.session }} className="inline-flex items-center gap-0.5 text-text no-underline hover:underline hover:underline-offset-3">
              {sessionTitle(session)}
              <Icon name="right" className="size-3" />
            </Link>
          ) : (
            <span className="text-muted">{item.session}</span>
          )}
        </dd>
        <dt className="py-1 text-muted">{m.inbox_kv_stopped()}</dt>
        <dd className="m-0 py-1 tabular-nums">{formatRelative(item.created_at)}</dd>
        <dt className="py-1 text-muted">{m.inbox_kv_handle()}</dt>
        <dd className="m-0 py-1 font-mono text-small [overflow-wrap:anywhere]">{item.handle}</dd>
      </dl>
      <p className="mt-2.5 text-label leading-5 text-muted">{m.inbox_hint()}</p>
    </div>
  );
}
