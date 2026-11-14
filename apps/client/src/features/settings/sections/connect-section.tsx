import { configKeys, isApiError, reconnectMcp, setMcpServerEnabled, setMcpToolEnabled, setSkillEnabled, useMcp, useSkills } from "@komo/api";
import type { McpServersResponse, McpServerView, SkillsResponse, SkillView } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import { Icon } from "@/components/icon";
import { BlockError, SkeletonRows } from "@/components/page";
import { Spinner } from "@/components/spinner";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { FormError, Hint, KState, Panel, Pill, Segmented, Switch, Tag } from "../controls";
import { withCode } from "../rich";
import { SectionHead } from "../section-head";

type Tab = "tools" | "skills";

// 一行：名字（带说明）| 类型 | 状态 | 开关；≤600 类型与状态挪到名字下面
const TOOL_GRID = "grid grid-cols-[minmax(0,1fr)_104px_112px_44px] items-center gap-x-4 max-[600px]:grid-cols-[minmax(0,1fr)_44px] max-[600px]:gap-y-1";
const SKILL_GRID = "grid grid-cols-[minmax(0,1fr)_96px_44px] items-center gap-x-4 max-[600px]:grid-cols-[minmax(0,1fr)_44px] max-[600px]:gap-y-1";

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
    case "disabled":
      return <KState>{m.mcp_state_disabled()}</KState>;
    default:
      return <KState tone="bad">{m.mcp_state_failed()}</KState>;
  }
}

/**
 * 连接（docs/web.md §15）：工具（MCP 服务器）与技能两张表，渠道随 P17 去掉了。
 * 工具：一台一行（名字、在哪、类型、状态、整台开关），点名字展开它的工具逐个开关；整台关着时工具开关置灰。
 * 技能：名字、说明、来源、开关，`unavailable` 有值时写出原因。增删服务器与技能目录仍在配置文件里。
 */
export function ConnectSection() {
  const [tab, setTab] = useState<Tab>("tools");
  const tabs = (
    <Segmented
      label={m.settings_sec_connect()}
      value={tab}
      onChange={setTab}
      options={[
        { value: "tools", label: m.connect_tab_tools() },
        { value: "skills", label: m.connect_tab_skills() },
      ]}
    />
  );
  return (
    <>
      <SectionHead title={m.settings_sec_connect()} description={tab === "tools" ? m.mcp_desc() : m.connect_skills_desc()} right={tabs} />
      {tab === "tools" ? <ToolsTable /> : <SkillsTable />}
    </>
  );
}

function ToolsTable() {
  const mcp = useMcp();
  const queryClient = useQueryClient();
  const [open, setOpen] = useState<string | null>(null);
  // 正在写的开关（`服务器` 或 `服务器/工具`）与各自的失败原因
  const [busy, setBusy] = useState("");
  const [failure, setFailure] = useState<{ key: string; text: string } | null>(null);
  const [reconnecting, setReconnecting] = useState("");

  const put = (data: McpServersResponse) => queryClient.setQueryData(configKeys.mcp, data);
  const toggle = async (key: string, write: () => Promise<McpServersResponse>) => {
    setBusy(key);
    setFailure(null);
    try {
      put(await write());
    } catch (error) {
      setFailure({ key, text: m.connect_toggle_failed({ message: errorText(error) }) });
    }
    setBusy("");
  };
  const reconnect = async (names: string[]) => {
    setReconnecting(names[0] ?? "*");
    setFailure(null);
    try {
      await reconnectMcp(names);
      await queryClient.invalidateQueries({ queryKey: configKeys.mcp });
    } catch (error) {
      setFailure({ key: names[0] ?? "*", text: m.mcp_reconnect_failed({ message: errorText(error) }) });
    }
    setReconnecting("");
  };

  // 404 = 在跑的 Gateway 早于 GET /v1/mcp：说清楚，别只甩一个状态码
  if (mcp.isError && isApiError(mcp.error) && mcp.error.status === 404) {
    return (
      <Panel>
        <p className="text-label text-muted">{m.mcp_old_gateway()}</p>
      </Panel>
    );
  }
  if (mcp.isError) return <BlockError message={m.mcp_load_error({ message: errorText(mcp.error) })} onRetry={() => void mcp.refetch()} />;
  if (!mcp.data) return <SkeletonRows count={3} />;
  const servers = mcp.data.servers;
  if (servers.length === 0) {
    return (
      <Panel>
        <p className="text-label text-muted">{m.mcp_none()}</p>
      </Panel>
    );
  }
  const live = servers.some((s) => s.enabled);
  return (
    <>
      {failure?.key === "*" && <FormError className="mt-0 mb-3">{failure.text}</FormError>}
      <div className="-mx-3">
        <div aria-hidden="true" className={cn(TOOL_GRID, "h-9 border-b border-border px-3 text-small text-muted max-[600px]:hidden")}>
          <span>{m.mcp_col_server()}</span>
          <span>{m.mcp_col_way()}</span>
          <span>{m.mcp_col_state()}</span>
          <span />
        </div>
        {servers.map((server) => (
          <ServerRows
            key={server.name}
            server={server}
            open={open === server.name}
            busy={busy}
            reconnecting={reconnecting === server.name}
            failure={failure?.key.split("/")[0] === server.name ? failure : null}
            onOpen={() => setOpen(open === server.name ? null : server.name)}
            onServer={(next) => void toggle(server.name, () => setMcpServerEnabled(server.name, next))}
            onTool={(tool, next) => void toggle(`${server.name}/${tool}`, () => setMcpToolEnabled(server.name, tool, next))}
            onReconnect={() => void reconnect([server.name])}
          />
        ))}
      </div>
      {live && (
        <div className="mt-3 flex justify-end">
          <Button size="sm" disabled={reconnecting !== ""} onClick={() => void reconnect([])}>
            {reconnecting === "*" ? m.mcp_reconnecting() : m.mcp_reconnect_all()}
          </Button>
        </div>
      )}
    </>
  );
}

function ServerRows({
  server,
  open,
  busy,
  reconnecting,
  failure,
  onOpen,
  onServer,
  onTool,
  onReconnect,
}: {
  server: McpServerView;
  open: boolean;
  busy: string;
  reconnecting: boolean;
  failure: { key: string; text: string } | null;
  onOpen: () => void;
  onServer: (next: boolean) => void;
  onTool: (tool: string, next: boolean) => void;
  onReconnect: () => void;
}) {
  const tools = server.tools ?? [];
  const enabledTools = tools.filter((tool) => tool.enabled).length;
  // 整台关着时也能展开看工具（开关置灰）：关着的服务器不连，没有工具列表可看就不给展开
  const expandable = tools.length > 0 || (server.enabled && server.state !== "connected");
  const exposureTag = (exposure: string): ReactNode => (exposure === server.exposure ? null : <Tag className="ml-0">{exposureText(exposure)}</Tag>);
  const sub = !server.enabled
    ? m.mcp_state_disabled()
    : server.state === "needs_auth"
      ? m.connect_login_expired()
      : server.state === "connected"
        ? enabledTools === tools.length
          ? m.mcp_tool_count({ count: tools.length })
          : m.connect_tool_ratio({ on: enabledTools, total: tools.length })
        : null;
  return (
    <div className={cn("border-t border-rule first:border-t-0", !server.enabled && "opacity-70")}>
      <div className={cn(TOOL_GRID, "min-h-14 px-3 py-2")}>
        <button
          type="button"
          aria-expanded={open}
          disabled={!expandable}
          onClick={onOpen}
          className="flex min-w-0 cursor-pointer items-center gap-2.5 border-0 bg-transparent p-0 text-left text-text disabled:cursor-default"
        >
          <Icon name={server.transport === "http" ? "globe" : "term"} className="text-muted" />
          <span className="min-w-0">
            <span className="block truncate font-mono">{server.name}</span>
            <span className="block truncate text-small text-muted">
              {server.endpoint && <span className="font-mono">{server.endpoint}</span>}
              {server.endpoint && sub && <span className="mx-1.5 text-faint">·</span>}
              {sub}
            </span>
          </span>
          {expandable && <Icon name="right" className={cn("ml-auto shrink-0 text-faint transition-transform", open && "rotate-90")} />}
        </button>
        <span className="inline-flex items-center gap-1.5 text-label text-muted max-[600px]:col-start-1 max-[600px]:row-start-2">
          {server.transport === "http" ? m.mcp_way_remote() : m.mcp_way_local()}
          {server.oauth && <Tag className="ml-0">OAuth</Tag>}
          <span className="hidden max-[600px]:inline">
            <span className="mx-1 text-faint">·</span>
            <StateLine server={server} />
          </span>
        </span>
        <span className="max-[600px]:hidden">
          <StateLine server={server} />
        </span>
        <span className="justify-self-end max-[600px]:col-start-2 max-[600px]:row-span-2 max-[600px]:row-start-1">
          <Switch checked={server.enabled} label={server.name} disabled={busy !== ""} onChange={onServer} />
        </span>
      </div>
      {failure?.key === server.name && <FormError className="mt-0 mb-2 px-3">{failure.text}</FormError>}
      {open && expandable && (
        <div className="mb-2 px-3 pl-9">
          {server.description && <p className="text-label leading-5 text-muted">{server.description}</p>}
          {server.state === "needs_auth" ? (
            <Hint>{withCode(m.mcp_needs_auth_hint({ command: `komo auth mcp login ${server.name}` }))}</Hint>
          ) : server.detail ? (
            <Hint bad>{server.detail}</Hint>
          ) : null}
          {server.state === "disconnected" && <Hint>{m.mcp_disconnected_hint()}</Hint>}
          {tools.length ? (
            <ul className="mt-1">
              {tools.map((tool) => (
                <li key={tool.name} className="flex items-center gap-4 border-t border-rule py-2.5 first:border-t-0">
                  <div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                      <span className={cn("font-mono font-medium [overflow-wrap:anywhere]", !tool.enabled && "text-muted")}>{tool.name}</span>
                      {exposureTag(tool.exposure)}
                      {tool.outward && <Pill tone="wait">{m.mcp_tool_outward()}</Pill>}
                      {tool.read_only && <Pill>{m.mcp_tool_read_only()}</Pill>}
                    </div>
                    {tool.description && <p className="mt-0.5 text-small leading-4.5 text-muted">{tool.description}</p>}
                    {failure?.key === `${server.name}/${tool.name}` && <FormError className="mt-1">{failure.text}</FormError>}
                  </div>
                  <Switch checked={tool.enabled} label={tool.name} disabled={busy !== "" || !server.enabled} onChange={(next) => onTool(tool.name, next)} />
                </li>
              ))}
            </ul>
          ) : (
            <p className="mt-1 text-label text-muted">{server.state === "connected" ? m.mcp_no_tools() : m.mcp_tools_after_connect()}</p>
          )}
          {server.enabled && (
            <div className="mt-2 flex justify-end">
              <Button size="sm" disabled={reconnecting} onClick={onReconnect}>
                {reconnecting ? m.mcp_reconnecting() : server.state === "disconnected" ? m.mcp_connect() : m.mcp_reconnect()}
              </Button>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

const sourceText = (source: SkillView["source"]) =>
  source === "komo" ? m.connect_source_komo() : source === "shared" ? m.connect_source_shared() : m.connect_source_configured();

function SkillsTable() {
  const skills = useSkills();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState("");
  const [failure, setFailure] = useState<{ name: string; text: string } | null>(null);

  const toggle = async (name: string, next: boolean) => {
    setBusy(name);
    setFailure(null);
    try {
      queryClient.setQueryData<SkillsResponse>(configKeys.skills, await setSkillEnabled(name, next));
    } catch (error) {
      setFailure({ name, text: m.connect_toggle_failed({ message: errorText(error) }) });
    }
    setBusy("");
  };

  if (skills.isError && isApiError(skills.error) && skills.error.status === 404) {
    return (
      <Panel>
        <p className="text-label text-muted">{m.connect_skills_old_gateway()}</p>
      </Panel>
    );
  }
  if (skills.isError) return <BlockError message={m.connect_skills_load_error({ message: errorText(skills.error) })} onRetry={() => void skills.refetch()} />;
  if (!skills.data) return <SkeletonRows count={3} />;
  const list = skills.data.skills;
  if (list.length === 0) {
    return (
      <Panel>
        <p className="text-label text-muted">{m.connect_skills_none()}</p>
      </Panel>
    );
  }
  return (
    <div className="-mx-3">
      <div aria-hidden="true" className={cn(SKILL_GRID, "h-9 border-b border-border px-3 text-small text-muted max-[600px]:hidden")}>
        <span>{m.connect_tab_skills()}</span>
        <span>{m.connect_col_source()}</span>
        <span />
      </div>
      {list.map((skill) => (
        <div key={skill.name} className={cn("border-t border-rule first:border-t-0", !skill.enabled && "opacity-70")}>
          <div className={cn(SKILL_GRID, "min-h-14 px-3 py-2.5")}>
            <span className="flex min-w-0 items-start gap-2.5">
              <Icon name="file" className="mt-1 text-muted" />
              <span className="min-w-0">
                <span className="block truncate font-mono" title={skill.path}>
                  {skill.name}
                </span>
                {skill.description && <span className="line-clamp-2 text-small leading-4.5 text-muted">{skill.description}</span>}
                {skill.enabled && skill.unavailable && <span className="mt-0.5 block text-small leading-4.5 text-pill-wait-ink">{m.connect_skill_unavailable({ reason: skill.unavailable })}</span>}
              </span>
            </span>
            <span className="text-label text-muted max-[600px]:col-start-1 max-[600px]:row-start-2 max-[600px]:pl-6.5">{sourceText(skill.source)}</span>
            <span className="justify-self-end max-[600px]:col-start-2 max-[600px]:row-span-2 max-[600px]:row-start-1">
              <Switch checked={skill.enabled} label={skill.name} disabled={busy !== ""} onChange={(next) => void toggle(skill.name, next)} />
            </span>
          </div>
          {failure?.name === skill.name && <FormError className="mt-0 mb-2 px-3">{failure.text}</FormError>}
        </div>
      ))}
    </div>
  );
}
