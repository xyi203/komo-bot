import type { ReactNode } from "react";
import { Icon } from "@/components/icon";
import { IconButton } from "@/components/ui/icon-button";
import { Markdown } from "@/components/markdown";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { formatElapsed, formatFullTime, formatMs, formatNumber } from "@/lib/format";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import type { ModelRow, SysRow, ThinkRow, ToolRow, Tokens, Trace, TraceRow, UserRow, CtxRow } from "./model";
import { KindIcon, kindLabel } from "./record-list";
import type { TraceUi } from "./state";

const statusName = (status: string) =>
  status === "completed" ? m.trace_status_completed()
  : status === "failed" ? m.trace_failed()
  : status === "uncertain" ? m.trace_uncertain()
  : status === "running" ? m.trace_status_running()
  : status;

const ms = (iso: string) => Date.parse(iso);

function Kv({ rows }: { rows: (false | undefined | null | [string, ReactNode])[] }) {
  return (
    <dl className="mt-2.5 grid grid-cols-[minmax(56px,max-content)_minmax(0,1fr)] gap-x-3 rounded-control bg-raised px-3 py-2.5 text-label leading-5 [.peek_&]:mt-0">
      {rows.filter((r): r is [string, ReactNode] => !!r).map(([k, v], i) => (
        <div key={i} className="contents">
          <dt className="py-1 text-muted">{k}</dt>
          <dd className="min-w-0 py-1 break-words">{v}</dd>
        </div>
      ))}
    </dl>
  );
}

const Pre = ({ text, out = false }: { text: string; out?: boolean }) => (
  <pre className={cn("m-0 overflow-auto rounded-control bg-raised px-3 py-2.5 font-mono text-small leading-5 break-words whitespace-pre-wrap", out && "whitespace-pre text-muted")}>
    {text}
  </pre>
);

const Note = ({ children }: { children: ReactNode }) => <p className="mb-3.5 text-label text-muted">{children}</p>;
const H5 = ({ children }: { children: ReactNode }) => <h5 className="mt-3 mb-1.5 text-small font-medium text-muted">{children}</h5>;
const Tok = ({ n }: { n: number }) => <span className="tabular-nums">{m.trace_tok({ count: formatNumber(n) })}</span>;

function Link({ row, label, onJump }: { row: string | undefined; label: string; onJump: (id: string) => void }) {
  if (!row) return null;
  return (
    <button
      type="button"
      onClick={() => onJump(row)}
      className="mr-2 inline-flex cursor-pointer items-center gap-0.5 text-text underline decoration-faint underline-offset-3 hover:decoration-current"
    >
      {label}
      <Icon name="right" className="size-3 text-faint" />
    </button>
  );
}

interface TabDef {
  id: string;
  title: string;
  body: ReactNode;
}

/**
 * 详情：顶上是真的标签页；"概述"把各页的开头摆在一起，段标题点了切到那一页；
 * 工具是概述 / 参数 / 结果 / Schema / 计时，助手是概述 / 思考 / 预览 / 原始内容 / 用量 / 计时，系统是差异 / 系统提示词 / 工具。
 */
export function TraceDetail({ row, trace, ui }: { row: TraceRow; trace: Trace; ui: TraceUi }) {
  const jump = (id: string) => {
    ui.select(id, true);
    requestAnimationFrame(() => requestAnimationFrame(() => document.getElementById(`tr-${id}`)?.scrollIntoView({ block: "nearest", behavior: "smooth" })));
  };
  const tabs = buildTabs(row, trace, ui.setDetailTab, jump);
  const tab = tabs.some((t) => t.id === ui.detailTab) ? ui.detailTab : tabs[0]!.id;
  const where = row.n ? (row.kind === "tool" || row.kind === "model" || row.kind === "think") && row.step ? m.trace_where_step({ n: row.n, step: row.step }) : m.trace_turn({ n: row.n }) : m.trace_where_start();
  return (
    <Tabs value={tab} onValueChange={ui.setDetailTab} className="flex min-h-0 flex-1 flex-col">
      <div className="flex shrink-0 items-center gap-2 pt-2.5 pr-2 pb-1.5 pl-3.5">
        <KindIcon row={row} />
        <span className="text-label font-medium">{kindLabel(row.kind)}</span>
        <span className="flex-1 text-small text-faint">{where}</span>
        <IconButton name="x" label={m.trace_detail_close()} size="sm" onClick={() => ui.select(null)} />
      </div>
      <TabsList aria-label={m.trace_detail_label()} className="shrink-0 gap-3.5 overflow-x-auto px-3.5 shadow-[inset_0_-1px_0_var(--color-rule)]">
        {tabs.map((t) => (
          <TabsTrigger
            key={t.id}
            value={t.id}
            className="h-8.5 shrink-0 text-label text-muted shadow-[inset_0_-2px_0_transparent] hover:text-text data-[state=active]:font-medium data-[state=active]:text-text data-[state=active]:shadow-[inset_0_-2px_0_var(--color-text)]"
          >
            {t.title}
          </TabsTrigger>
        ))}
      </TabsList>
      {tabs.map((t) => (
        <TabsContent key={t.id} value={t.id} className="min-h-0 flex-1 overflow-auto px-3.5 pt-1 pb-4 outline-none">
          {t.body}
        </TabsContent>
      ))}
    </Tabs>
  );
}

function buildTabs(row: TraceRow, trace: Trace, goTab: (tab: string) => void, jump: (id: string) => void): TabDef[] {
  const Section = ({ tab, title, children }: { tab: string; title: string; children: ReactNode }) => (
    <section className="pt-3">
      <h4 className="mb-1.5">
        <button type="button" onClick={() => goTab(tab)} className="inline-flex cursor-pointer items-center gap-0.5 text-label font-medium text-muted hover:text-text">
          {title}
          <Icon name="right" className="size-3" />
        </button>
      </h4>
      <div className="peek max-h-[180px] overflow-auto">{children}</div>
    </section>
  );
  const timing = (from: string | undefined, to: string | undefined, live?: boolean) => (
    <Kv
      rows={[
        [m.trace_start_time(), <span key="a" className="tabular-nums">{from ? formatFullTime(from) : m.trace_unavailable()}</span>],
        [m.trace_duration(), <span key="b" className="tabular-nums">{live && from ? formatElapsed(from) : from && to ? formatMs(ms(to) - ms(from)) : m.trace_unavailable()}</span>],
        [m.trace_timing_source(), live ? m.trace_timing_live() : m.trace_timing_stamp()],
      ]}
    />
  );
  const usage = (u: Tokens, title: string) => (
    <>
      <H5>{title}</H5>
      <Kv
        rows={[
          [m.trace_usage_input(), <Tok key="i" n={(u.input ?? 0) + (u.cache ?? 0)} />],
          u.cache != null && [m.trace_usage_cache(), <Tok key="c" n={u.cache} />],
          [m.trace_usage_output(), <Tok key="o" n={u.output ?? 0} />],
          u.reasoning != null && [m.trace_usage_reasoning(), <Tok key="r" n={u.reasoning} />],
        ]}
      />
    </>
  );

  switch (row.kind) {
    case "tool": {
      const t = row as ToolRow;
      const def = trace.schema.get(t.name);
      const fn = def?.function ?? def ?? {};
      const schema = def ? (
        <>
          <p className="mt-2 mb-0.5 font-mono font-medium">{t.name}</p>
          <p className="mb-2 text-label leading-5 whitespace-pre-wrap text-muted">{fn.description ?? ""}</p>
        </>
      ) : (
        <Note>{m.trace_schema_unavailable()}</Note>
      );
      const args = JSON.stringify(t.args, null, 2);
      const result = t.preview ? <Pre text={t.preview} out /> : <Note>{t.live ? m.trace_executing() : m.trace_no_result()}</Note>;
      const status = t.live ? (
        <span className="inline-flex items-center gap-1.5 text-small text-muted">
          <span className="size-[7px] rounded-full bg-accent" />
          {m.trace_status_running()}
        </span>
      ) : t.bad ? (
        <span className="text-negative">{statusName(t.status)}</span>
      ) : (
        m.trace_status_done()
      );
      return [
        {
          id: "overview",
          title: m.trace_tab_overview(),
          body: (
            <>
              <Kv rows={[[m.trace_level(), <Link key="p" row={t.parent} label={m.trace_assistant_message()} onJump={jump} />], [m.trace_status(), status]]} />
              <Section tab="args" title={m.trace_tab_args()}><Pre text={args} /></Section>
              <Section tab="result" title={m.trace_tab_result()}>{result}</Section>
              <Section tab="schema" title={m.trace_tab_schema()}>{schema}</Section>
              <Section tab="timing" title={m.trace_tab_timing()}>{timing(t.at, t.end, t.live)}</Section>
            </>
          ),
        },
        { id: "args", title: m.trace_tab_args(), body: <Pre text={args} /> },
        { id: "result", title: m.trace_tab_result(), body: result },
        {
          id: "schema",
          title: m.trace_tab_schema(),
          body: (
            <>
              {schema}
              {fn.parameters != null && (
                <>
                  <H5>{m.trace_tab_args()}</H5>
                  <Pre text={JSON.stringify(fn.parameters, null, 2)} />
                </>
              )}
            </>
          ),
        },
        { id: "timing", title: m.trace_tab_timing(), body: timing(t.at, t.end, t.live) },
      ];
    }
    case "model": {
      const r = row as ModelRow;
      const body = r.plain ? <Note>{m.trace_no_content()}</Note> : <Markdown text={r.body} className="compact" />;
      const calls = r.calls.length ? r.calls.map((c, i) => <Link key={i} row={c.row} label={c.name} onJump={jump} />) : m.trace_none();
      return [
        {
          id: "overview",
          title: m.trace_tab_overview(),
          body: (
            <>
              <Kv rows={[[m.trace_status(), m.trace_status_done()], [m.trace_tool_calls(), calls]]} />
              {r.thought && <Section tab="think" title={m.trace_tab_think()}><Markdown text={r.thought} className="compact text-muted" /></Section>}
              <Section tab="preview" title={m.trace_tab_preview()}>{body}</Section>
              <Section tab="usage" title={m.trace_tab_usage()}>
                <Kv rows={[[m.trace_usage_input(), <Tok key="i" n={(r.tokens.input ?? 0) + (r.tokens.cache ?? 0)} />], [m.trace_usage_output(), <Tok key="o" n={r.tokens.output ?? 0} />]]} />
              </Section>
              <Section tab="timing" title={m.trace_tab_timing()}>{timing(r.at, r.end)}</Section>
            </>
          ),
        },
        ...(r.thought ? [{ id: "think", title: m.trace_tab_think(), body: <Markdown text={r.thought} className="compact" /> }] : []),
        { id: "preview", title: m.trace_tab_preview(), body },
        { id: "raw", title: m.trace_tab_raw(), body: r.plain ? <Note>{m.trace_no_content()}</Note> : <Pre text={r.body} /> },
        { id: "usage", title: m.trace_tab_usage(), body: <>{usage(r.tokens, m.trace_usage_this_request())}{usage(r.cum, m.trace_usage_session())}</> },
        { id: "timing", title: m.trace_tab_timing(), body: timing(r.at, r.end) },
      ];
    }
    case "sys": {
      const r = row as SysRow;
      const prompt = Object.keys(r.sections).length ? (
        Object.entries(r.sections).map(([k, v]) => (
          <div key={k}>
            <H5>{k}</H5>
            <Pre text={v} />
          </div>
        ))
      ) : (
        <Note>{m.trace_no_system_prompt()}</Note>
      );
      const tools = r.tools.length ? (
        r.tools.map((d, i) => {
          const fn = d.function ?? d;
          return (
            <div key={i}>
              <p className="mt-2 mb-0.5 font-mono font-medium">{fn.name}</p>
              <p className="mb-2 text-label leading-5 whitespace-pre-wrap text-muted">{fn.description ?? ""}</p>
            </div>
          );
        })
      ) : (
        <Note>{m.trace_no_tools()}</Note>
      );
      const diffBody = r.first
        ? null
        : r.changed.length || r.added.length || r.removed.length ? (
            <>
              {r.changed.map(([k, v]) => (
                <div key={k}>
                  <H5>{k}</H5>
                  {v == null ? <Note>{m.trace_section_removed()}</Note> : <Pre text={v} />}
                </div>
              ))}
              {r.added.length > 0 && <p className="mb-2 text-label leading-5 text-muted">{m.trace_tools_added({ names: r.added.join(", ") })}</p>}
              {r.removed.length > 0 && <p className="mb-2 text-label leading-5 text-muted">{m.trace_tools_removed({ names: r.removed.join(", ") })}</p>}
            </>
          ) : (
            <Note>{m.trace_no_change()}</Note>
          );
      return [
        ...(diffBody ? [{ id: "diff", title: m.trace_tab_diff(), body: diffBody }] : []),
        { id: "prompt", title: m.trace_tab_prompt(), body: prompt },
        { id: "tools", title: m.trace_tab_tools(), body: tools },
      ];
    }
    default: {
      const r = row as UserRow | CtxRow | ThinkRow;
      const parent = r.kind === "think" ? r.parent : undefined;
      const body = <Markdown text={r.text} className="compact" />;
      return [
        {
          id: "overview",
          title: m.trace_tab_overview(),
          body: (
            <>
              {parent && <Kv rows={[[m.trace_level(), <Link key="p" row={parent} label={m.trace_assistant_message()} onJump={jump} />]]} />}
              <Section tab="preview" title={m.trace_tab_preview()}>{body}</Section>
              {r.at && <Section tab="timing" title={m.trace_tab_timing()}>{timing(r.at, r.end)}</Section>}
            </>
          ),
        },
        { id: "preview", title: m.trace_tab_preview(), body },
        { id: "raw", title: m.trace_tab_raw(), body: <Pre text={r.text} /> },
        ...(r.at ? [{ id: "timing", title: m.trace_tab_timing(), body: timing(r.at, r.end) }] : []),
      ];
    }
  }
}
