import { reloadConfig, useConfigCheck, useConfigEnv, useGatewayHealth } from "@komo/api";
import type { ConfigReloadResponse } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { BlockError, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { formatDateTime } from "@/lib/format";
import { m } from "@/paraglide/messages.js";
import { CardRow, FormError, KState, KeyValues, Panel, Pill, SettingsCard, TextInput } from "../controls";
import { editErrorText } from "../use-config-edit";
import { withCode } from "../rich";
import { SectionHead } from "../section-head";
import { DesktopConnectionRow, desktopConnection } from "./desktop-connection";
import { EnvSection } from "./env-section";

/**
 * 关于：Gateway 地址与连接状态、版本与启动时间、配置检查、环境变量入口。
 * 账号与登录状态在登录接上之后放这里（docs/redesign.md §2.9）。
 * 点"环境变量"的"管理"，这一节整块换成环境变量视图，左上角返回。
 */
export function AboutSection() {
  const [view, setView] = useState<"about" | "env">("about");
  if (view === "env") return <EnvSection onBack={() => setView("about")} />;
  return (
    <>
      <SectionHead title={m.settings_sec_about()} />
      <GatewayRows />
      <ConfigCheckRow />
      <EnvRow onManage={() => setView("env")} />
    </>
  );
}

function GatewayRows() {
  const health = useGatewayHealth();
  const h = health.data;
  const desktop = desktopConnection();
  const status = health.isError ? (
    <KState tone="bad">{m.gateway_unreachable()}</KState>
  ) : h ? (
    <>
      <KState tone="ok">{m.gateway_connected()}</KState>
      <span className="mx-1.5 text-faint">·</span>
      {m.gateway_rtt({ ms: h.rtt_ms })}
    </>
  ) : null;
  return (
    <>
      {desktop ? <DesktopConnectionRow api={desktop} status={status} /> : <AddressRow status={status} />}
      {health.isError ? (
        <BlockError message={m.gateway_load_error({ message: errorText(health.error) })} onRetry={() => void health.refetch()} />
      ) : !h ? (
        <SkeletonRows count={2} className="h-10" />
      ) : (
        <Panel>
          <KeyValues
            rows={[
              [m.gateway_version(), <span className="tabular-nums">{m.gateway_version_value({ version: h.version, protocol: h.protocol_version })}</span>],
              [m.gateway_started(), <span className="tabular-nums">{formatDateTime(h.started_at)}</span>],
            ]}
          />
        </Panel>
      )}
    </>
  );
}

/**
 * 浏览器里的 Gateway 地址：网页由 Gateway 同源托管，"改地址"就是整页跳到另一台的网页（那边自己的登录与会话照旧）。
 * 先用 `no-cors` 探一下 `/healthz` 通不通（跨域读不到内容，只能知道连不连得上），连不上就留在原地并说明，仍连着原来的。
 */
function AddressRow({ status }: { status: React.ReactNode }) {
  const [text, setText] = useState(location.origin);
  const [state, setState] = useState<"idle" | "connecting" | "bad">("idle");
  const [error, setError] = useState("");

  const connect = async (event: FormEvent) => {
    event.preventDefault();
    let origin: string;
    try {
      const raw = text.trim();
      const url = new URL(/^[a-z][a-z\d+.-]*:\/\//i.test(raw) ? raw : `https://${raw}`);
      if (url.protocol !== "http:" && url.protocol !== "https:") throw new Error("protocol");
      origin = url.origin;
    } catch {
      setState("bad");
      setError(m.about_address_invalid());
      return;
    }
    if (origin === location.origin) {
      setState("idle");
      setError("");
      setText(origin);
      return;
    }
    setState("connecting");
    setError("");
    try {
      await fetch(`${origin}/healthz`, { mode: "no-cors", cache: "no-store", signal: AbortSignal.timeout(5000) });
      location.assign(`${origin}/`);
    } catch {
      setState("bad");
      setError(m.about_address_unreachable());
    }
  };

  return (
    <SettingsCard>
      <form onSubmit={(event) => void connect(event)}>
        <CardRow
          title={m.conn_url()}
          desc={state === "connecting" ? m.about_address_connecting() : status}
          control={
            <>
              <TextInput
                aria-label={m.conn_url()}
                mono
                value={text}
                spellCheck={false}
                onChange={(event) => {
                  setText(event.target.value);
                  if (state === "bad") setState("idle");
                }}
                className="w-60 max-[600px]:w-auto max-[600px]:flex-1"
              />
              <Button type="submit" size="sm" disabled={state === "connecting" || !text.trim()}>
                {m.about_address_connect()}
              </Button>
            </>
          }
        />
        {state === "bad" && error && <FormError>{error}</FormError>}
      </form>
    </SettingsCard>
  );
}

/** 配置检查：几条提醒、装载时间、文件之后改过没；重新加载与重新检查。 */
function ConfigCheckRow() {
  const check = useConfigCheck();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState<"" | "reload" | "check">("");
  const [reload, setReload] = useState<ConfigReloadResponse | { error: string } | null>(null);
  const data = check.data;

  const doReload = async () => {
    setBusy("reload");
    try {
      setReload(await reloadConfig());
      void queryClient.invalidateQueries({ queryKey: ["config"] });
    } catch (error) {
      setReload({ error: editErrorText(error) });
    }
    setBusy("");
  };
  const doCheck = async () => {
    setBusy("check");
    await check.refetch();
    setBusy("");
  };

  if (check.isError) {
    return (
      <SettingsCard>
        <FormError className="mt-0">{m.config_check_error({ message: errorText(check.error) })}</FormError>
      </SettingsCard>
    );
  }
  if (!data) return <SkeletonRows count={1} />;
  const issues = data.issues;
  const stale = data.sources.some((s) => new Date(s.mtime).getTime() > new Date(data.loaded_at).getTime());
  return (
    <SettingsCard>
      <CardRow
        title={issues.length ? m.config_check_issues({ count: issues.length }) : m.config_check_ok()}
        desc={
          <>
            {m.config_loaded_at({ time: formatDateTime(data.loaded_at) })}
            {stale && <span className="text-negative"> {m.config_stale()}</span>}
          </>
        }
        control={
          <>
            <Button size="sm" disabled={busy !== ""} onClick={() => void doReload()}>
              {busy === "reload" ? m.config_reloading() : m.config_reload()}
            </Button>
            <Button size="sm" disabled={busy !== ""} onClick={() => void doCheck()}>
              {m.config_recheck()}
            </Button>
          </>
        }
      />
      {issues.length > 0 && (
        <ul className="mt-3 flex flex-col gap-2">
          {issues.map((issue, i) => (
            <li key={i} className="flex flex-col gap-1 border-t border-rule pt-2 text-label leading-5">
              <span className="flex flex-wrap items-center gap-2">
                <Pill tone={issue.severity === "error" ? "bad" : "wait"}>{issue.severity === "error" ? m.config_issue_error() : m.config_issue_warning()}</Pill>
                <span className="font-mono text-small">{issue.key}</span>
              </span>
              <span className="[overflow-wrap:anywhere]">{issue.message}</span>
            </li>
          ))}
        </ul>
      )}
      {reload && (
        <div className="mt-3 rounded-control bg-raised px-3 py-2.5 text-label leading-5 shadow-ring">
          {"error" in reload ? (
            <p className="text-negative">{m.config_reload_failed({ message: reload.error })}</p>
          ) : (
            <>
              <p>{reload.changed.length ? m.config_reload_changed({ count: reload.changed.length, keys: reload.changed.join(", ") }) : m.config_reload_unchanged()}</p>
              {reload.start_only.length > 0 && <p className="mt-1.5 text-negative">{withCode(m.config_reload_start_only({ keys: reload.start_only.join(", ") }))}</p>}
            </>
          )}
        </div>
      )}
    </SettingsCard>
  );
}

/** 环境变量的入口：API 密钥与其他凭证；值只能写入。有缺值的变量时提示。 */
function EnvRow({ onManage }: { onManage: () => void }) {
  const env = useConfigEnv();
  const vars = env.data?.vars ?? [];
  const missing = vars.filter((v) => !v.set).length;
  const desc = env.isError ? errorText(env.error) : !env.data ? " " : missing ? m.about_env_count_missing({ count: vars.length, missing }) : m.about_env_count({ count: vars.length });
  return (
    <SettingsCard>
      <CardRow
        title={m.settings_sec_env()}
        desc={desc}
        control={
          <Button size="sm" onClick={onManage}>
            {m.about_env_manage()}
          </Button>
        }
      />
    </SettingsCard>
  );
}
