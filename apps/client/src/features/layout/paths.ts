/** 对话页的地址：`/`（主对话）、`/chats/new`（还没建会话的新支线）、`/chats/$id`（支线）。`/sessions/$id` 只是跳转，不算。 */
export const isChatPath = (pathname: string) => pathname === "/" || pathname === "/chats" || pathname.startsWith("/chats/");

/** 窄屏上的对话列表那一屏：`/chats`（宽屏上这个地址会跳回 `/`）。 */
export const isChatListPath = (pathname: string) => pathname.replace(/\/$/, "") === "/chats";

/** 窄屏上不画底部标签栏的页：对话页（输入框贴底）、目标与记忆的详情页（带返回）。 */
export function hidesTabBar(pathname: string): boolean {
  if (isChatPath(pathname)) return !isChatListPath(pathname);
  return /^\/(goals|memory)\/[^/]+/.test(pathname);
}

/** 当前路径对应哪段对话：主对话是 `"main"`，新支线是 `"new"`，支线是它的会话 id；不在对话页是 `null`。 */
export function chatKeyOf(pathname: string): string | null {
  if (pathname === "/") return "main";
  if (!pathname.startsWith("/chats/")) return null;
  const rest = decodeURIComponent(pathname.slice("/chats/".length).replace(/\/$/, ""));
  return rest || null;
}
