import { isApiError, useChats, useHealth } from "@komo/api";
import { useStreamStatus } from "@/lib/stream-status";

/**
 * 和 Gateway 的连接出了什么问题：`down` 是请求连不上（健康检查或对话列取不到，没配对的 401 不算），
 * `retrying` 是当前对话的事件流断了、正在自己重连；都没有就是 `null`。
 * 图标栏设置按钮上的点与对话页输入框上的提示共用这一份。
 */
export function useConnection(): "down" | "retrying" | null {
  const health = useHealth();
  const chats = useChats();
  const stream = useStreamStatus();
  const unauthorized = chats.isError && isApiError(chats.error) && chats.error.unauthorized;
  if (health.isError || (chats.isError && !unauthorized)) return "down";
  return stream === "reconnecting" ? "retrying" : null;
}
