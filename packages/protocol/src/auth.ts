// 登录相关的线上形状（手写，不在 generated/ 里）：照 `crates/komo-kernel/src/protocol/http.rs` 的
// `AuthMeResponse` / `AuthSessionView` / `AuthSessionsResponse` / `AuthRevokeResponse` 抄的。
// 重新生成绑定（`KOMO_UPDATE_TS=1 cargo test -p komo-kernel --features ts`）后 generated/ 里会出现同名类型，
// 那时删掉本文件与 index.ts 里对应的一行即可（index.ts 里是具名导出，同名时本文件的优先，不会冲突）。

/** 当前请求用的凭据：浏览器会话 cookie / 桌面端设备令牌 / 本机令牌。 */
export type AuthCredentialKind = "web" | "device" | "local";

/** 登录状态的种类：网页会话 / 设备令牌。 */
export type AuthSessionKind = "web" | "device";

/** `GET /auth/me`（不要登录，永远 200）。 */
export interface AuthMeResponse {
  authenticated: boolean;
  /** 这台 Gateway 配了 `[auth.oidc]` 没有；`false` 时网页没有登录可走。 */
  oidc_configured: boolean;
  credential?: AuthCredentialKind;
  /** OIDC 的 `sub`（本机令牌没有）。 */
  subject?: string;
  email?: string;
  /** 当前这条登录状态的 id（本机令牌没有）；与 `GET /v1/auth/sessions` 里的 `id` 对得上。 */
  session_id?: string;
}

/** 一条登录状态（网页会话或设备令牌）。 */
export interface AuthSessionView {
  id: string;
  kind: AuthSessionKind;
  /** 网页是浏览器 User-Agent 的摘要，设备是客户端报的名字。 */
  label: string;
  subject: string;
  email?: string;
  created_at: string;
  last_used_at: string;
  /** 设备令牌不过期（`null`）。 */
  expires_at?: string | null;
  /** 是不是发这次请求用的这一条。 */
  current: boolean;
}

/** `GET /v1/auth/sessions`。 */
export interface AuthSessionsResponse {
  sessions: AuthSessionView[];
}

/** `DELETE /v1/auth/sessions/{id}`。 */
export interface AuthRevokeResponse {
  revoked: boolean;
}
