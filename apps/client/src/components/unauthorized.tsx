import { m } from "@/paraglide/messages.js";

/** 没有 cookie 或 cookie 失效（Gateway 重启后会话 cookie 全部失效）：说清楚怎么重新配对。 */
export function Unauthorized() {
  return (
    <div className="mx-auto max-w-md px-6 py-24">
      <h1 className="text-dialog-title font-semibold">{m.unauthorized_title()}</h1>
      <p className="mt-2 text-read text-muted">{m.unauthorized_body()}</p>
    </div>
  );
}
