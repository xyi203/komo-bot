import type { ErrorBody, ErrorCode } from "@komo/protocol";

/**
 * 统一的失败：Gateway 的 `ErrorBody`，或者读不出 `ErrorBody` 时（代理报 502、网络断了）按同样的形状补一个。
 * `message` 是 Gateway 原样给的文字，不翻译；界面按 `code` 映射译文，不认识的 `code` 再回落到它。
 */
export class ApiError extends Error {
  readonly status: number;
  readonly code: ErrorCode;
  readonly keys: string[];

  constructor(status: number, body: ErrorBody) {
    super(body.error.message);
    this.name = "ApiError";
    this.status = status;
    this.code = body.error.code;
    // 线上 `keys` 为空时整个字段不出现（见 komo-kernel 的 protocol/ts.rs）。
    this.keys = body.error.keys ?? [];
  }

  /** 没带凭据或凭据失效（浏览器该重新 `komo web` 配对）。 */
  get unauthorized(): boolean {
    return this.code === "unauthorized";
  }
}

export function isApiError(error: unknown): error is ApiError {
  return error instanceof ApiError;
}

/** 读不出 `ErrorBody` 的响应（或根本没有响应）补成同样的形状。 */
export function fallbackBody(message: string): ErrorBody {
  return { error: { code: "internal", message, keys: [] } };
}
