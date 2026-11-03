import { useHomeSession } from "@komo/api";
import { Navigate, createFileRoute } from "@tanstack/react-router";
import { errorText } from "@/lib/errors";
import { m } from "@/paraglide/messages.js";

// 根路径落在主会话（原型启动时也是打开主会话）。
export const Route = createFileRoute("/_app/")({ component: Home });

function Home() {
  const home = useHomeSession();
  if (home.data) return <Navigate to="/sessions/$sessionId" params={{ sessionId: home.data.session }} replace />;
  return (
    <div className="grid h-full place-items-center px-6 text-read text-faint">
      {home.isError ? m.home_error({ message: errorText(home.error) }) : m.sessions_loading()}
    </div>
  );
}
