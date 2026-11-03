import type { SessionSummary } from "@komo/protocol";
import { m } from "@/paraglide/messages.js";

/** 会话的显示名：没起名的主会话叫"主会话"，其余"新会话"。 */
export function sessionTitle(session: Pick<SessionSummary, "title" | "kind"> | undefined): string {
  if (session?.title) return session.title;
  return session?.kind === "main" ? m.session_kind_main() : m.session_untitled();
}
