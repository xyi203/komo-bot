// 设置（docs/web.md §15）用到的读写：配置检查、配置 / 环境变量 / 模型三份视图、记忆索引、MCP。
//
// 三份视图（`models` / `settings` / `env`）共用一个 `version`：任何一次写都带读到的 version，文件在别处被改过 Gateway 回 409。
// 写的回应（`ConfigEditResponse`）带回新的三份视图，`applyConfigEdit` 把它们写回各自的缓存。
import type {
  ConfigCheckResponse,
  ConfigEditResponse,
  ConfigEnvResponse,
  ConfigModelsResponse,
  ConfigReloadResponse,
  ConfigSettingsResponse,
  EnvAssignment,
  HealthResponse,
  McpReconnectResponse,
  McpServersResponse,
  MemoryIndexStatus,
  RebuildIndexResponse,
} from "@komo/protocol";
import { useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { get, post, request } from "./http";
import { keys } from "./keys";

const enc = encodeURIComponent;

export const configKeys = {
  check: ["config", "check"] as const,
  models: ["config", "models"] as const,
  settings: ["config", "settings"] as const,
  env: ["config", "env"] as const,
  memoryIndex: ["config", "memory-index"] as const,
  mcp: ["config", "mcp"] as const,
};

// 读
export const fetchConfigCheck = () => get<ConfigCheckResponse>("/v1/config/check");
export const fetchConfigModels = () => get<ConfigModelsResponse>("/v1/config/models");
export const fetchConfigSettings = () => get<ConfigSettingsResponse>("/v1/config/settings");
export const fetchConfigEnv = () => get<ConfigEnvResponse>("/v1/config/env");
export const fetchMemoryIndexStatus = () => get<MemoryIndexStatus>("/v1/memory-index");
export const fetchMcp = () => get<McpServersResponse>("/v1/mcp");

// 写
export const reloadConfig = () => post<ConfigReloadResponse>("/v1/config/reload");
/** 常用设置：`value` 为 `null` = 删掉那一行（回到默认）。 */
export const putSetting = (key: string, version: string, value: unknown) =>
  request<ConfigEditResponse>("PUT", `/v1/config/settings/${enc(key)}`, { body: { version, value } });
export const putEnv = (version: string, vars: EnvAssignment[]) =>
  request<ConfigEditResponse>("PUT", "/v1/config/env", { body: { version, vars } });
export const deleteEnv = (version: string, name: string) =>
  request<ConfigEditResponse>("DELETE", `/v1/config/env/${enc(name)}`, { body: { version } });
export const rebuildMemoryIndex = () => post<RebuildIndexResponse>("/v1/memory-index/rebuild");
export const reconnectMcp = (servers: string[] = []) => post<McpReconnectResponse>("/v1/mcp/reconnect", { servers });
export const disconnectMcp = (servers: string[]) => post<McpReconnectResponse>("/v1/mcp/disconnect", { servers });

/** 写完把回应里新的三份视图写回缓存，并让配置检查与 `/v1/models`（输入框下的模型菜单）重读。 */
export function applyConfigEdit(queryClient: QueryClient, response: ConfigEditResponse) {
  queryClient.setQueryData(configKeys.models, response.models);
  queryClient.setQueryData(configKeys.settings, response.settings);
  queryClient.setQueryData(configKeys.env, response.env);
  void queryClient.invalidateQueries({ queryKey: configKeys.check });
  void queryClient.invalidateQueries({ queryKey: configKeys.memoryIndex });
  void queryClient.invalidateQueries({ queryKey: keys.models });
}

/** 三份视图共用的 version：哪份读到了用哪份。 */
export function configVersion(queryClient: QueryClient): string | undefined {
  return (
    queryClient.getQueryData<ConfigSettingsResponse>(configKeys.settings)?.version ??
    queryClient.getQueryData<ConfigModelsResponse>(configKeys.models)?.version ??
    queryClient.getQueryData<ConfigEnvResponse>(configKeys.env)?.version
  );
}

/** 每次打开设置都重读一遍（配置可能在文件里改过）：`staleTime: 0` 且挂载时重取。 */
const fresh = { staleTime: 0, refetchOnMount: "always", retry: false } as const;

export function useConfigCheck() {
  return useQuery({ queryKey: configKeys.check, queryFn: fetchConfigCheck, ...fresh });
}
export function useConfigModels() {
  return useQuery({ queryKey: configKeys.models, queryFn: fetchConfigModels, ...fresh });
}
export function useConfigSettings() {
  return useQuery({ queryKey: configKeys.settings, queryFn: fetchConfigSettings, ...fresh });
}
export function useConfigEnv() {
  return useQuery({ queryKey: configKeys.env, queryFn: fetchConfigEnv, ...fresh });
}
export function useMemoryIndexStatus() {
  return useQuery({ queryKey: configKeys.memoryIndex, queryFn: fetchMemoryIndexStatus, ...fresh });
}
export function useMcp() {
  return useQuery({ queryKey: configKeys.mcp, queryFn: fetchMcp, ...fresh });
}

/** Gateway 的 `/healthz`，附上这一圈请求的往返时间（浏览器 → Gateway）。 */
export function useGatewayHealth() {
  return useQuery({
    queryKey: ["config", "health"] as const,
    queryFn: async (): Promise<HealthResponse & { rtt_ms: number }> => {
      const t0 = performance.now();
      const health = await get<HealthResponse>("/healthz");
      return { ...health, rtt_ms: Math.round(performance.now() - t0) };
    },
    ...fresh,
  });
}

export function useRefreshConfig() {
  const queryClient = useQueryClient();
  return () => queryClient.invalidateQueries({ queryKey: ["config"] });
}
