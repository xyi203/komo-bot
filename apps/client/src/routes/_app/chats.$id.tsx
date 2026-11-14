import { createFileRoute } from "@tanstack/react-router";
import { SessionPage } from "@/features/session/session-page";

// 支线对话：用户直接开的会话。
export const Route = createFileRoute("/_app/chats/$id")({ component: Page });

function Page() {
  const { id } = Route.useParams();
  return <SessionPage sessionId={id} main={false} />;
}
