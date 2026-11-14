import { useHomeSession } from "@komo/api";
import { createFileRoute } from "@tanstack/react-router";
import { errorText } from "@/lib/errors";
import { SessionPage } from "@/features/session/session-page";
import { m } from "@/paraglide/messages.js";

// 根路径就是主对话（默认 Agent 的主会话，没有就建）。
export const Route = createFileRoute("/_app/")({ component: Home });

function Home() {
  const home = useHomeSession();
  if (home.data) return <SessionPage sessionId={home.data.session} main />;
  return (
    <div className="grid h-full place-items-center px-6 text-read text-faint">
      {home.isError ? m.home_error({ message: errorText(home.error) }) : m.sessions_loading()}
    </div>
  );
}
