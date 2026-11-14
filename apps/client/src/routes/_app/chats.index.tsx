import { Navigate, createFileRoute } from "@tanstack/react-router";
import { useNarrow } from "@/lib/hooks";

// `/chats` 只在窄屏上有意义：外壳（app-shell）直接把对话列表铺满一屏，这个路由本身不画东西。
// 宽屏上对话列表常驻左边，这个地址没有内容，跳回主对话。
export const Route = createFileRoute("/_app/chats/")({ component: ChatsIndex });

function ChatsIndex() {
  return useNarrow() ? null : <Navigate to="/" replace />;
}
