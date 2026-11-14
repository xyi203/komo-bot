import type { AuthMeResponse, AuthRevokeResponse, AuthSessionsResponse } from "@komo/protocol";
import { useQuery, type QueryClient } from "@tanstack/react-query";
import { del, get, post } from "./http";
import { keys } from "./keys";

// 网页登录（docs/redesign.md §2.9、docs/web.md §19）：`/auth/*` 是 Gateway 的路径，不是 `/v1/**`。
// 登录本身（`GET /auth/login`）是整页跳转，不是 fetch，所以这里没有它。

/** 当前登录状态；永远 200（没登录也是 200，`authenticated: false`）。 */
export const fetchAuthMe = () => get<AuthMeResponse>("/auth/me");

/** 退出登录：吊销当前会话并清 cookie。 */
export const logout = () => post<Record<string, never>>("/auth/logout");

/** 已登录的设备与网页会话。 */
export const fetchAuthSessions = () => get<AuthSessionsResponse>("/v1/auth/sessions");

/** 吊销一条；吊销的是当前这条就等于退出登录。 */
export const revokeAuthSession = (id: string) => del<AuthRevokeResponse>(`/v1/auth/sessions/${encodeURIComponent(id)}`);

/**
 * 启动时取一次，之后只在登录失效（401）或登录 / 退出后变：`staleTime: Infinity`。
 * 取不到（连不上、旧 Gateway 没有这个接口）是 error 态，应用照常进入，由各页自己的「连不上」提示说话。
 */
export function useAuthMe() {
  return useQuery({ queryKey: keys.authMe, queryFn: fetchAuthMe, retry: false, staleTime: Infinity });
}

/** 设置「关于」里的设备清单：每次打开都重取。 */
export function useAuthSessions() {
  return useQuery({ queryKey: keys.authSessions, queryFn: fetchAuthSessions, retry: false, staleTime: 0 });
}

/**
 * 登录失效或刚退出：就地把 `/auth/me` 缓存标成未登录（守卫立刻换成登录页），再作废它取真实状态
 * （是否配了 OIDC 等）。已经是未登录就不再动，免得并发的 401 反复刷新。
 */
export function markSignedOut(queryClient: QueryClient): void {
  const current = queryClient.getQueryData<AuthMeResponse>(keys.authMe);
  if (current && !current.authenticated) return;
  queryClient.setQueryData<AuthMeResponse>(keys.authMe, { authenticated: false, oidc_configured: current?.oidc_configured ?? true });
  void queryClient.invalidateQueries({ queryKey: keys.authMe });
}
