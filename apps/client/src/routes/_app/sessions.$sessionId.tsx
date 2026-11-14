import { useHomeSession } from "@komo/api";
import { Navigate, createFileRoute } from "@tanstack/react-router";
import { errorText } from "@/lib/errors";
import { m } from "@/paraglide/messages.js";

// 旧地址保留成跳转（存量链接、记忆 / 事项 / 定时页里的会话链接）：主会话落到 `/`，其余落到 `/chats/$id`。
export const Route = createFileRoute("/_app/sessions/$sessionId")({ component: Redirect });

function Redirect() {
  const { sessionId } = Route.useParams();
  const home = useHomeSession();
  if (home.data) {
    return home.data.session === sessionId ? <Navigate to="/" replace /> : <Navigate to="/chats/$id" params={{ id: sessionId }} replace />;
  }
  return (
    <div className="grid h-full place-items-center px-6 text-read text-faint">
      {home.isError ? m.home_error({ message: errorText(home.error) }) : m.sessions_loading()}
    </div>
  );
}
