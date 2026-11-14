import { useNavigate } from "@tanstack/react-router";

/**
 * ⌘K 弹窗用的两个动作（弹窗下一步重写，这个文件随之去掉）：打开一个会话、新对话。
 * 新对话不再预先建会话：进一个空的新对话页，发第一句话时才建（`features/session/new-chat-page.tsx`）。
 */
export function useSessionActions() {
  const navigate = useNavigate();
  return {
    // 旧地址 `/sessions/$id` 保留成跳转：主会话落到 `/`，其余落到 `/chats/$id`
    open: (session: string) => void navigate({ to: "/sessions/$sessionId", params: { sessionId: session } }),
    create: () => void navigate({ to: "/chats/new" }),
  };
}
