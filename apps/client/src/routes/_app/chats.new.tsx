import { createFileRoute } from "@tanstack/react-router";
import { NewChatPage } from "@/features/session/new-chat-page";

// 新支线：空的对话页，发第一句话时才建会话（比 `/chats/$id` 具体，路由先匹配到它）。
export const Route = createFileRoute("/_app/chats/new")({ component: NewChatPage });
