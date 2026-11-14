// 设备登录（crates/komo-gateway/src/http/auth.rs）：系统浏览器走完 OIDC，Gateway 303 到
// komo://auth/callback?code&state，主进程拿一次性码 + PKCE verifier 换设备令牌。
// 这里只管协议本身（PKCE、state、换令牌）；存哪、什么时候清是 connection.ts 的事。
import { createHash, randomBytes, timingSafeEqual } from "node:crypto";
import { hostname } from "node:os";
import { errorMessage, request, serverMessage, type Target } from "./upstream";

/** 必须与 Gateway 白名单里的 komo://auth/callback 完全一致（换令牌时也要原样再发一次）。 */
export const REDIRECT_URI = "komo://auth/callback";
/** 从打开浏览器起，这么久没回跳就放弃（与 CLI 的 `komo login` 同为 5 分钟）。 */
export const LOGIN_TTL_MS = 5 * 60 * 1000;

export interface PendingLogin {
  base: string;
  headers: Record<string, string>;
  verifier: string;
  state: string;
  expiresAt: number;
}

export interface DeviceToken {
  token: string;
  sessionId: string;
  subject: string;
  email?: string;
}

/** 起一次登录：内存里记下 verifier 与 state，返回要在系统浏览器里打开的地址。 */
export function beginLogin(base: string, headers: Record<string, string>): { pending: PendingLogin; url: string } {
  const verifier = randomBytes(32).toString("base64url");
  const state = randomBytes(32).toString("base64url");
  const url = new URL(`${base}/auth/device/authorize`);
  url.searchParams.set("code_challenge", createHash("sha256").update(verifier).digest("base64url"));
  url.searchParams.set("code_challenge_method", "S256");
  url.searchParams.set("redirect_uri", REDIRECT_URI);
  url.searchParams.set("state", state);
  // 设备清单里显示的名字（Gateway 截 60 个字符）。
  url.searchParams.set("name", `komo 桌面端 @ ${hostname()}`.slice(0, 60));
  return { pending: { base, headers, verifier, state, expiresAt: Date.now() + LOGIN_TTL_MS }, url: url.toString() };
}

/** 认 `komo://auth/callback?code=…&state=…`；别的形状（包括 komo://session/<id>）返回 undefined。 */
export function parseCallback(raw: string): { code?: string; state?: string } | undefined {
  try {
    const url = new URL(raw);
    if (url.protocol !== "komo:" || url.hostname !== "auth" || url.pathname !== "/callback") return undefined;
    return { code: url.searchParams.get("code") || undefined, state: url.searchParams.get("state") || undefined };
  } catch {
    return undefined;
  }
}

export function sameState(a: string, b: string): boolean {
  const left = Buffer.from(a);
  const right = Buffer.from(b);
  return left.length === right.length && timingSafeEqual(left, right);
}

/** `POST /auth/device/token`：失败时抛带人话的 Error。 */
export async function redeemCode(pending: PendingLogin, code: string): Promise<DeviceToken> {
  const target: Target = { base: pending.base, headers: pending.headers };
  let response: Response;
  try {
    response = await request(target, "/auth/device/token", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ code, code_verifier: pending.verifier, redirect_uri: REDIRECT_URI }),
      timeoutMs: 15_000,
    });
  } catch (error) {
    throw new Error(`没连上 Gateway：${errorMessage(error)}`);
  }
  if (!response.ok) throw new Error(`Gateway 拒绝了授权码：${await serverMessage(response)}（授权码只能用一次、几分钟内有效，请重新登录）`);
  const body = (await response.json().catch(() => undefined)) as
    | { token?: unknown; session_id?: unknown; subject?: unknown; email?: unknown }
    | undefined;
  if (typeof body?.token !== "string" || typeof body.session_id !== "string" || typeof body.subject !== "string" || !body.token) {
    throw new Error("Gateway 的响应不像设备令牌");
  }
  return {
    token: body.token,
    sessionId: body.session_id,
    subject: body.subject,
    ...(typeof body.email === "string" && body.email ? { email: body.email } : {}),
  };
}

/** `DELETE /v1/auth/sessions/{id}`。2xx、404（已经没了）、401（令牌本来就失效）都算吊销成功。 */
export async function revokeSession(target: Target, sessionId: string): Promise<{ ok: true } | { ok: false; message: string }> {
  try {
    const response = await request(target, `/v1/auth/sessions/${encodeURIComponent(sessionId)}`, { method: "DELETE" });
    if (response.ok || response.status === 404 || response.status === 401) {
      void response.body?.cancel();
      return { ok: true };
    }
    return { ok: false, message: await serverMessage(response) };
  } catch (error) {
    return { ok: false, message: errorMessage(error) };
  }
}
