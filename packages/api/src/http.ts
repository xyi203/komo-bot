import type { ErrorBody } from "@komo/protocol";
import { ApiError, fallbackBody } from "./errors";
import { notifyUnauthorized } from "./unauthorized";

/**
 * 同源相对路径的 fetch 封装：`/v1/*` 由 Gateway 托管，cookie 自动带（鉴权见 docs/clients.md D6），
 * 令牌不经过这里。失败一律抛 [`ApiError`]。
 */
export async function request<T>(
  method: string,
  path: string,
  options: {
    body?: unknown;
    /** 原样发出去的字节（附件上传）；给了它就不发 JSON `body`。 */
    raw?: Blob;
    signal?: AbortSignal;
    query?: Record<string, string | number | boolean | undefined>;
  } = {},
): Promise<T> {
  const url = withQuery(path, options.query);
  let response: Response;
  try {
    response = await fetch(url, {
      method,
      credentials: "same-origin",
      headers: {
        accept: "application/json",
        ...(options.raw ? { "content-type": options.raw.type || "application/octet-stream" } : options.body === undefined ? {} : { "content-type": "application/json" }),
      },
      body: options.raw ?? (options.body === undefined ? undefined : JSON.stringify(options.body)),
      signal: options.signal,
    });
  } catch (error) {
    if (error instanceof DOMException && error.name === "AbortError") throw error;
    throw new ApiError(0, fallbackBody(error instanceof Error ? error.message : String(error)));
  }
  const text = await response.text();
  if (!response.ok) {
    // `/v1/**` 回 401：登录失效，让应用回到登录页（而不是当成「连不上」）。`/auth/*` 的 401 不算。
    if (response.status === 401 && path.startsWith("/v1/")) notifyUnauthorized();
    throw new ApiError(response.status, parseErrorBody(text, response));
  }
  return (text ? JSON.parse(text) : undefined) as T;
}

export const get = <T>(path: string, query?: Record<string, string | number | boolean | undefined>, signal?: AbortSignal) =>
  request<T>("GET", path, { query, signal });

export const post = <T>(path: string, body?: unknown, signal?: AbortSignal) =>
  request<T>("POST", path, { body: body ?? {}, signal });

export const del = <T>(path: string, signal?: AbortSignal) => request<T>("DELETE", path, { signal });

function withQuery(path: string, query?: Record<string, string | number | boolean | undefined>): string {
  if (!query) return path;
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) {
    if (value !== undefined) params.set(key, String(value));
  }
  const text = params.toString();
  return text ? `${path}?${text}` : path;
}

function parseErrorBody(text: string, response: Response): ErrorBody {
  try {
    const parsed = JSON.parse(text) as Partial<ErrorBody>;
    if (parsed?.error?.code && typeof parsed.error.message === "string") return parsed as ErrorBody;
  } catch {
    // 不是 JSON：下面补一个。
  }
  return fallbackBody(`${response.status} ${response.statusText}`.trim() || "request failed");
}
