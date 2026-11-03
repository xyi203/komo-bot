import { isApiError } from "@komo/api";
import { m } from "@/paraglide/messages.js";

/**
 * 界面上显示的错误文字：按 `code` 映射成译文；不认识的 `code` 回落到 Gateway 原样给的 `message`
 * （那句话是机器返回的，不翻译，docs/clients.md D9）。
 */
export function errorText(error: unknown): string {
  if (!isApiError(error)) return error instanceof Error ? error.message : String(error);
  if (error.status === 0) return m.error_unreachable();
  switch (error.code) {
    case "unauthorized":
      return m.error_unauthorized();
    case "denied":
      return m.error_denied();
    case "not_found":
      return m.error_not_found();
    case "request_key_conflict":
      return m.error_request_key_conflict();
    case "version_conflict":
      return m.error_version_conflict();
    case "invalid_request":
      return m.error_invalid_request();
    case "conflict":
      // 409 带着具体原因（还有引用没处置、这个状态下做不了）：原因本身是 Gateway 给的，不翻译
      return error.message || m.error_conflict();
    case "config_invalid":
      return m.error_config_invalid();
    case "vector_unavailable":
      return m.error_vector_unavailable();
    case "corrupt":
      return m.error_corrupt();
    case "internal":
      return error.status === 500 ? m.error_internal() : error.message;
    default:
      return error.message;
  }
}
