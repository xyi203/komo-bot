/** 对话页的地址：`/`（主对话）、`/chats/new`（还没建会话的新支线）、`/chats/$id`（支线）。`/sessions/$id` 只是跳转，不算。 */
export const isChatPath = (pathname: string) => pathname === "/" || pathname === "/chats" || pathname.startsWith("/chats/");

/** 当前路径对应哪段对话：主对话是 `"main"`，新支线是 `"new"`，支线是它的会话 id；不在对话页是 `null`。 */
export function chatKeyOf(pathname: string): string | null {
  if (pathname === "/") return "main";
  if (!pathname.startsWith("/chats/")) return null;
  const rest = decodeURIComponent(pathname.slice("/chats/".length).replace(/\/$/, ""));
  return rest || null;
}
