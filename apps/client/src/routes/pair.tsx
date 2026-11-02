import { redeemPairCode } from "@komo/api";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { m } from "@/paraglide/messages.js";

export const Route = createFileRoute("/pair")({ component: Pair });

// 配对码只能用一次，而 StrictMode 下 effect 会跑两遍：按码缓存同一个请求。
const attempts = new Map<string, Promise<unknown>>();

type Status = { kind: "working" } | { kind: "failed"; message: string };

/** `komo web` 打开的 `/pair#code=…`：把一次性码 POST 给 Gateway 换会话 cookie，然后进首页。 */
function Pair() {
  const navigate = useNavigate();
  const [status, setStatus] = useState<Status>({ kind: "working" });

  useEffect(() => {
    const code = new URLSearchParams(window.location.hash.slice(1)).get("code");
    // 码在 fragment 里不进服务器日志；读到就从地址栏抹掉，免得留在历史记录里。
    history.replaceState(null, "", window.location.pathname);
    if (!code) {
      setStatus({ kind: "failed", message: m.pair_missing_code() });
      return;
    }
    let request = attempts.get(code);
    if (!request) {
      request = redeemPairCode(code);
      attempts.set(code, request);
    }
    request.then(
      () => void navigate({ to: "/", replace: true }),
      (error: unknown) => setStatus({ kind: "failed", message: errorText(error) }),
    );
  }, [navigate]);

  return (
    <div className="mx-auto max-w-md px-6 py-24">
      <h1 className="text-dialog-title font-semibold">{m.pair_title()}</h1>
      {status.kind === "working" ? (
        <p className="mt-2 text-read text-muted">{m.pair_working()}</p>
      ) : (
        <div className="mt-2 space-y-2">
          <p className="text-read text-negative">{m.pair_failed({ message: status.message })}</p>
          <p className="text-body text-muted">{m.pair_hint()}</p>
          <Button onClick={() => window.location.reload()}>{m.retry()}</Button>
        </div>
      )}
    </div>
  );
}
