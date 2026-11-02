import type { ErrorBody } from "@komo/protocol";
import { ApiError, fallbackBody } from "./errors";

/**
 * 同源相对路径的 fetch 封装：`/v1/*` 由 Gateway 托管，cookie 自动带（鉴权见 docs/clients.md D6），
 * 令牌不经过这里。失败一律抛 [`ApiError`]。
 */
export async function request<T>(
  method: string,
  path: string,
  options: { body?: unknown; signal?: AbortSignal; query?: Record<string, string | number | boolean | undefined> } = {},
): Promise<T> {
  const url = withQuery(path, options.query);
  let response: Response;
  try {
    response = await fetch(url, {
      method,
      credentials: "same-origin",
      headers: {
        accept: "application/json",
        ...(options.body === undefined ? {} : { "content-type": "application/json" }),
      },
      body: options.body === undefined ? undefined : JSON.stringify(options.body),
      signal: options.signal,
    });
  } catch (error) {
    if (error instanceof DOMException && error.name === "AbortError") throw error;
    throw new ApiError(0, fallbackBody(error instanceof Error ? error.message : String(error)));
  }
  const text = await response.text();
  if (!response.ok) throw new ApiError(response.status, parseErrorBody(text, response));
  return (text ? JSON.parse(text) : undefined) as T;
}

export const get = <T>(path: string, query?: Record<string, string | number | boolean | undefined>, signal?: AbortSignal) =>
  request<T>("GET", path, { query, signal });

export const post = <T>(path: string, body?: unknown, signal?: AbortSignal) =>
  request<T>("POST", path, { body: body ?? {}, signal });

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
