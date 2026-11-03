// 对 Gateway 发请求的公共部分：额外请求头的校验、远程地址的规范化、HTTP / SSE 探测。
import type { TestResult, TestStep } from "../shared/api";

/** 额外请求头不许覆盖的：鉴权由主进程管，Host/Origin/Cookie 是浏览器与 Gateway 的 Host 校验、cookie 鉴权在看的。 */
const FORBIDDEN = new Set(["authorization", "cookie", "host", "origin"]);
/** 这些由 HTTP 栈自己管，设了只会打坏请求。 */
const FRAMING = new Set(["content-length", "transfer-encoding", "connection", "upgrade", "keep-alive", "te", "trailer"]);
const TOKEN = /^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/;

/** 解析 `Name: value` 逐行文本。不合法或碰了保留名就抛，错误信息直接给人看。 */
export function parseExtraHeaders(text: string): Record<string, string> {
  const headers: Record<string, string> = {};
  for (const [index, raw] of text.split(/\r?\n/).entries()) {
    const line = raw.trim();
    if (!line) continue;
    const colon = line.indexOf(":");
    const name = colon > 0 ? line.slice(0, colon).trim() : "";
    const value = colon > 0 ? line.slice(colon + 1).trim() : "";
    if (!name || !TOKEN.test(name)) throw new Error(`额外请求头第 ${index + 1} 行不是 “名字: 值” 的形式`);
    const lower = name.toLowerCase();
    if (FORBIDDEN.has(lower)) throw new Error(`额外请求头不能设置 ${name}（Authorization、Cookie、Host、Origin 由 App 管）`);
    if (FRAMING.has(lower)) throw new Error(`额外请求头不能设置 ${name}`);
    if (/[\r\n\0]/.test(value)) throw new Error(`额外请求头 ${name} 的值含非法字符`);
    headers[name] = value;
  }
  return headers;
}

/** 规范成 `scheme://host[:port][/prefix]`（无结尾斜杠）。只收 http / https，不收地址里的账号密码。 */
export function normalizeBaseUrl(input: string): { url: string; warning?: string } {
  let parsed: URL;
  try {
    parsed = new URL(input.trim());
  } catch {
    throw new Error("地址不是合法的 URL（例如 https://komo.example.com）");
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") throw new Error("地址只支持 http 或 https");
  if (parsed.username || parsed.password) throw new Error("地址里不要带账号密码；令牌在下面单独填");
  const loopback = ["localhost", "127.0.0.1", "[::1]"].includes(parsed.hostname);
  return {
    url: `${parsed.origin}${parsed.pathname.replace(/\/+$/, "")}`,
    warning:
      parsed.protocol === "http:" && !loopback ? "这是明文 HTTP：令牌会在网络上明文传输，除非前面有 Tailscale 之类的加密链路。" : undefined,
  };
}

export interface Target {
  base: string;
  headers: Record<string, string>;
}

const json = { accept: "application/json" };

/** 给定目标发一次请求，带上额外请求头与令牌。 */
export function request(target: Target, pathAndQuery: string, init: RequestInit & { timeoutMs?: number } = {}) {
  const { timeoutMs = 8000, ...rest } = init;
  return fetch(`${target.base}${pathAndQuery}`, {
    redirect: "manual",
    signal: init.signal ?? AbortSignal.timeout(timeoutMs),
    ...rest,
    headers: { ...json, ...target.headers, ...(rest.headers as Record<string, string> | undefined) },
  });
}

export interface Health {
  instance_id: string;
  version: string;
  protocol_version: number;
}

/** 把"上游给了重定向"这种常见的访问代理症状说成人话。 */
export function describeStatus(response: Response): string {
  if (response.status >= 300 && response.status < 400) {
    return `被重定向到 ${response.headers.get("location") ?? "别处"}（前面的访问代理要求登录？检查额外请求头）`;
  }
  return `HTTP ${response.status}`;
}

export async function fetchHealth(target: Target): Promise<Health> {
  const response = await request(target, "/healthz");
  if (!response.ok) throw new Error(`/healthz 返回 ${describeStatus(response)}`);
  const body = (await response.json()) as Partial<Health>;
  if (typeof body.protocol_version !== "number" || typeof body.instance_id !== "string") {
    throw new Error("/healthz 的响应不像 komo Gateway");
  }
  return body as Health;
}

/** 完整测一遍：HTTP、协议版本、令牌、SSE。`appProtocol` 是 App 打包时的 PROTOCOL_VERSION。 */
export async function probe(target: Target, appProtocol: number): Promise<{ result: TestResult; health?: Health }> {
  const steps: TestStep[] = [];
  const done = (health?: Health) => ({ result: { ok: steps.every((step) => step.ok), steps }, health });

  let health: Health;
  try {
    health = await fetchHealth(target);
    steps.push({ name: "health", ok: true, message: `Gateway ${health.version}（协议 ${health.protocol_version}）` });
  } catch (error) {
    steps.push({ name: "health", ok: false, message: errorMessage(error) });
    return done();
  }

  const protocolOk = health.protocol_version === appProtocol;
  steps.push({
    name: "protocol",
    ok: protocolOk,
    message: protocolOk
      ? `协议版本一致（${appProtocol}）`
      : `协议版本不一致：Gateway ${health.protocol_version}，App ${appProtocol}`,
  });
  if (!protocolOk) return done(health);

  let sessions: Array<{ session: string }>;
  try {
    const response = await request(target, "/v1/sessions");
    if (response.status === 401) throw new Error("令牌被拒绝（401）");
    if (!response.ok) throw new Error(`/v1/sessions 返回 ${describeStatus(response)}`);
    sessions = ((await response.json()) as { sessions?: Array<{ session: string }> }).sessions ?? [];
    steps.push({ name: "auth", ok: true, message: "令牌有效" });
  } catch (error) {
    steps.push({ name: "auth", ok: false, message: errorMessage(error) });
    return done(health);
  }

  const first = sessions[0]?.session;
  if (!first) {
    steps.push({ name: "sse", ok: true, message: "还没有会话，SSE 没法实测（有会话后会自动用上）" });
    return done(health);
  }
  const controller = new AbortController();
  try {
    const response = await request(target, `/v1/sessions/${encodeURIComponent(first)}/events?from=0`, {
      headers: { accept: "text/event-stream", "accept-encoding": "identity" },
      signal: controller.signal,
    });
    const type = response.headers.get("content-type") ?? "";
    if (!response.ok || !type.includes("text/event-stream")) {
      throw new Error(`SSE 端点返回 ${describeStatus(response)} ${type}`.trim());
    }
    // 订阅一建立，Gateway 就先补读再推心跳；读到第一块字节才算"流真的通了"（缓冲型代理会卡在这里）。
    const reader = response.body!.getReader();
    const chunk = await Promise.race([
      reader.read(),
      new Promise<never>((_, reject) => setTimeout(() => reject(new Error("8 秒内没收到任何 SSE 数据（前面的代理缓冲了流？）")), 8000)),
    ]);
    if (chunk.done) throw new Error("SSE 连接立刻被关闭");
    steps.push({ name: "sse", ok: true, message: "SSE 通了" });
  } catch (error) {
    steps.push({ name: "sse", ok: false, message: errorMessage(error) });
  } finally {
    controller.abort();
  }
  return done(health);
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error) {
    const cause = (error as { cause?: { code?: string; message?: string } }).cause;
    return cause?.code ? `${error.message}（${cause.code}）` : error.message;
  }
  return String(error);
}
