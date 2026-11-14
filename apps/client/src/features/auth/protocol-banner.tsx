import { useHealth } from "@komo/api";
import { EXPECTED_PROTOCOL_VERSION } from "@komo/protocol";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages.js";

/**
 * 网页期望的协议版本与 `/healthz` 给的不一致：顶上一条不能关的提示（不阻断使用）。
 * 服务端没给这个字段（旧 Gateway）或还没取到时不提示；数字或字符串都按文本比较。
 */
export function ProtocolBanner() {
  const version = (useHealth().data as { protocol_version?: number | string | null } | undefined)?.protocol_version;
  if (version === undefined || version === null || String(version) === String(EXPECTED_PROTOCOL_VERSION)) return null;
  return (
    <div role="alert" className="fixed inset-x-0 top-0 z-50 flex items-center justify-center gap-3 bg-pill-wait-bg px-4 py-1.5 text-label text-pill-wait-ink shadow-[0_1px_0_rgb(0_0_0/0.08)]">
      <span className="min-w-0 [overflow-wrap:anywhere]">{m.protocol_mismatch()}</span>
      <Button size="sm" onClick={() => location.reload()}>
        {m.protocol_refresh()}
      </Button>
    </div>
  );
}
