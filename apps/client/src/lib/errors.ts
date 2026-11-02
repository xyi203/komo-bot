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
    case "internal":
      return error.status === 500 ? m.error_internal() : error.message;
    default:
      return error.message;
  }
}
