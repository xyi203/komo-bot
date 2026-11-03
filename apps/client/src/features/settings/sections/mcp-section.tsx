import { disconnectMcp, isApiError, reconnectMcp, useMcp } from "@komo/api";
import type { McpServerView } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import { Icon } from "@/components/icon";
import { BlockError, SkeletonRows } from "@/components/page";
import { Spinner } from "@/components/spinner";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { m } from "@/paraglide/messages.js";
import { CardRow, FormError, Hint, KState, Panel, Pill, SettingsCard, Tag, TextButton } from "../controls";
import { withCode } from "../rich";
import { SectionHead } from "../section-head";

const exposureText = (exposure: string) =>
  exposure === "codemode" ? m.mcp_exposure_codemode() : exposure === "direct" ? m.mcp_exposure_direct() : exposure === "hidden" ? m.mcp_exposure_hidden() : exposure;

function StateLine({ server }: { server: McpServerView }) {
  switch (server.state) {
    case "connected":
      return <KState tone="ok">{m.mcp_state_connected()}</KState>;
    case "connecting":
      return (
        <span className="inline-flex items-center gap-1.5 text-small text-muted">
          <Spinner small />
          {m.mcp_state_connecting()}
        </span>
      );
    case "needs_auth":
      return <KState tone="warn">{m.mcp_state_needs_auth()}</KState>;
    case "disconnected":
      return <KState>{m.mcp_state_disconnected()}</KState>;
    default:
      return <KState tone="bad">{m.mcp_state_failed()}</KState>;
  }
}

const Way = ({ server }: { server: McpServerView }) => (
  <span className="inline-flex items-center gap-1.5">
    {server.transport === "http" ? m.mcp_way_remote() : m.mcp_way_local()}
    {server.oauth && <Tag className="ml-0">OAuth</Tag>}
  </span>
);

const Sep = () => <span className="mx-1.5 text-faint">·</span>;

/**
 * MCP：读 `GET /v1/mcp`（不等握手、不重连）。一张服务器表，点一行进详情；增删改服务器仍在配置文件里。
 * 选中的服务器只记在这一节里：换节、关弹窗都回到表。
 */
export function McpSection() {
  const mcp = useMcp();
  const queryClient = useQueryClient();
  const [selected, setSelected] = useState<string | null>(null);
  const [busy, setBusy] = useState<"" | "reconnect" | "disconnect">("");
  const [failure, setFailure] = useState("");

  const refresh = () => queryClient.invalidateQueries({ queryKey: ["config", "mcp"] });
  const act = async (kind: "reconnect" | "disconnect", servers: string[]) => {
    setBusy(kind);
    setFailure("");
    try {
      if (kind === "reconnect") await reconnectMcp(servers);
      else await disconnectMcp(servers);
      await refresh();
    } catch (error) {
      setFailure(kind === "reconnect" ? m.mcp_reconnect_failed({ message: errorText(error) }) : m.mcp_disconnect_failed({ message: errorText(error) }));
    }
    setBusy("");
  };

  const servers = mcp.data?.servers ?? [];
  const current = selected ? servers.find((s) => s.name === selected) : undefined;
  const reconnectAll =
    servers.length > 0 && !current ? (
      <Button size="sm" disabled={busy !== ""} onClick={() => void act("reconnect", [])}>
        {busy === "reconnect" ? m.mcp_reconnecting() : m.mcp_reconnect_all()}
      </Button>
    ) : undefined;
  const head = <SectionHead title={m.settings_sec_mcp()} description={m.mcp_desc()} right={reconnectAll} />;

  // 404 = 在跑的 Gateway 早于 GET /v1/mcp：说清楚，别只甩一个状态码
  if (mcp.isError && isApiError(mcp.error) && mcp.error.status === 404) {
    return (
      <>
        {head}
        <Panel>
          <p className="text-label text-muted">{m.mcp_old_gateway()}</p>
        </Panel>
      </>
    );
  }
  if (mcp.isError) {
    return (
      <>
        {head}
        <BlockError message={m.mcp_load_error({ message: errorText(mcp.error) })} onRetry={() => void mcp.refetch()} />
      </>
    );
  }
  if (!mcp.data) {
    return (
      <>
        {head}
        <SkeletonRows count={3} />
      </>
    );
  }
  if (current) return <>{head}<Detail server={current} busy={busy} failure={failure} onBack={() => { setSelected(null); setFailure(""); }} onAct={act} /></>;
  return (
    <>
      {head}
      {failure && <FormError className="mt-0 mb-3">{failure}</FormError>}
      {servers.length === 0 ? (
        <Panel>
          <p className="text-label text-muted">{m.mcp_none()}</p>
        </Panel>
      ) : (
        <div className="-mx-3">
          <div aria-hidden="true" className="grid h-9 grid-cols-[minmax(0,1fr)_144px_56px_104px_16px] items-center gap-x-4 border-b border-border px-3 text-small text-muted max-[600px]:hidden">
            <span>{m.mcp_col_server()}</span>
            <span>{m.mcp_col_way()}</span>
            <span>{m.mcp_col_tools()}</span>
            <span>{m.mcp_col_state()}</span>
            <span />
          </div>
          {servers.map((server) => (
            <button
              key={server.name}
              type="button"
              onClick={() => setSelected(server.name)}
              className="grid min-h-13 w-full cursor-pointer grid-cols-[minmax(0,1fr)_144px_56px_104px_16px] items-center gap-x-4 border-0 border-t border-rule bg-transparent px-3 text-left text-text transition-colors first:border-t-0 hover:bg-hover-on-white max-[600px]:grid-cols-[minmax(0,1fr)_auto_16px] max-[600px]:gap-y-0.5 max-[600px]:py-2.5"
            >
              <span className="truncate font-mono max-[600px]:col-start-1 max-[600px]:row-start-1">{server.name}</span>
              <span className="text-label text-muted max-[600px]:col-start-1 max-[600px]:row-start-2">
                <Way server={server} />
              </span>
              <span className="text-label text-muted tabular-nums max-[600px]:hidden">{server.state === "connected" ? server.tools.length : "—"}</span>
              <span className="max-[600px]:col-start-2 max-[600px]:row-span-2 max-[600px]:row-start-1">
                <StateLine server={server} />
              </span>
              <Icon name="right" className="text-faint max-[600px]:col-start-3 max-[600px]:row-span-2 max-[600px]:row-start-1" />
            </button>
          ))}
        </div>
      )}
    </>
  );
}

function Detail({
  server,
  busy,
  failure,
  onBack,
  onAct,
}: {
  server: McpServerView;
  busy: "" | "reconnect" | "disconnect";
  failure: string;
  onBack: () => void;
  onAct: (kind: "reconnect" | "disconnect", servers: string[]) => Promise<void>;
}) {
  const tools = server.tools ?? [];
  const exposureTag = (exposure: string): ReactNode => (exposure === server.exposure ? null : <Tag className="ml-0">{exposureText(exposure)}</Tag>);
  return (
    <>
      <div className="mb-2">
        <TextButton quiet onClick={onBack} className="inline-flex items-center gap-1">
          <Icon name="left" />
          {m.mcp_back()}
        </TextButton>
      </div>
      <SettingsCard>
        <CardRow
          mono
          title={server.name}
          desc={
            <span className="inline-flex flex-wrap items-center">
              <StateLine server={server} />
              {server.state === "connected" && (
                <>
                  <Sep />
                  {m.mcp_tool_count({ count: tools.length })}
                </>
              )}
              <Sep />
              <Way server={server} />
              <Sep />
              {m.mcp_default_exposure({ way: exposureText(server.exposure) })}
            </span>
          }
          control={
            <>
              {server.state !== "disconnected" && (
                <Button size="sm" disabled={busy !== ""} onClick={() => void onAct("disconnect", [server.name])}>
                  {busy === "disconnect" ? m.mcp_disconnecting() : m.mcp_disconnect()}
                </Button>
              )}
              <Button size="sm" disabled={busy !== ""} onClick={() => void onAct("reconnect", [server.name])}>
                {busy === "reconnect" ? m.mcp_reconnecting() : server.state === "disconnected" ? m.mcp_connect() : m.mcp_reconnect()}
              </Button>
            </>
          }
        />
        {server.description && <p className="mt-3 text-body leading-5.5">{server.description}</p>}
        {server.state === "disconnected" && <Hint>{m.mcp_disconnected_hint()}</Hint>}
        {server.state === "needs_auth" ? <Hint>{withCode(m.mcp_needs_auth_hint({ command: `komo auth mcp login ${server.name}` }))}</Hint> : server.detail ? <Hint bad>{server.detail}</Hint> : null}
        {failure && <FormError>{failure}</FormError>}
      </SettingsCard>
      {tools.length ? (
        <ul>
          {tools.map((tool) => (
            <li key={tool.name} className="border-t border-rule py-2.5 first:border-t-0">
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
                <span className="font-mono font-medium [overflow-wrap:anywhere]">{tool.name}</span>
                {exposureTag(tool.exposure)}
                {tool.outward && <Pill tone="wait">{m.mcp_tool_outward()}</Pill>}
                {tool.read_only && <Pill>{m.mcp_tool_read_only()}</Pill>}
                {tool.description && <span className="text-small text-muted">{tool.description}</span>}
              </div>
            </li>
          ))}
        </ul>
      ) : (
        <p className="text-label text-muted">{server.state === "connected" ? m.mcp_no_tools() : m.mcp_tools_after_connect()}</p>
      )}
    </>
  );
}
