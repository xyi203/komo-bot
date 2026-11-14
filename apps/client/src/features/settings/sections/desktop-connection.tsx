import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages.js";
import { CardRow, FormError, SettingsCard } from "../controls";

type DesktopConnection = NonNullable<NonNullable<Window["komoDesktop"]>["connection"]>;
type Full = DesktopConnection & Required<Pick<DesktopConnection, "get" | "signOut">>;

/** 桌面壳给了 `get` 与 `signOut` 才算完整（老版本的壳只有 openSettings）。 */
export function desktopConnection(): Full | undefined {
  const connection = typeof window === "undefined" ? undefined : window.komoDesktop?.connection;
  return connection?.get && connection.signOut ? (connection as Full) : undefined;
}

/** IPC 抛回来的错误带一段 "Error invoking remote method '…': Error: " 前缀，给人看时去掉。 */
function ipcError(error: unknown): string {
  return String((error as Error)?.message ?? error).replace(/^Error invoking remote method '[^']+': (?:Error: )?/, "");
}

/** 桌面端的"连接"一行：当前地址与状态，右边一个按钮去桌面壳的连接设置（本机 / 远程、登录都在那里做）。 */
export function DesktopConnectionRow({ api, status }: { api: Full; status: React.ReactNode }) {
  const [url, setUrl] = useState("");
  useEffect(() => {
    void api.get().then((info) => setUrl(info.baseUrl ?? info.remote?.url ?? ""));
  }, [api]);
  return (
    <SettingsCard>
      <CardRow
        title={m.gateway_connection()}
        desc={
          <>
            <span className="font-mono">{url || location.origin}</span>
            <span className="mx-1.5 text-faint">·</span>
            {status}
          </>
        }
        control={
          <Button size="sm" onClick={() => void api.openSettings()}>
            {m.desktop_open_connection()}
          </Button>
        }
      />
    </SettingsCard>
  );
}

/** 桌面壳里的退出：吊销设备令牌并回到壳的连接设置页（整页离开 SPA），所以成功时这里什么也不用做。 */
export function DesktopSignOut({ api }: { api: Full }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const signOut = async () => {
    setBusy(true);
    setError("");
    try {
      const result = await api.signOut();
      if (result.warning) setError(result.warning);
    } catch (failure) {
      setError(ipcError(failure));
    }
    setBusy(false);
  };
  return (
    <div className="flex flex-col items-end gap-1.5">
      <Button size="sm" disabled={busy} onClick={() => void signOut()}>
        {m.account_logout()}
      </Button>
      {error && <FormError className="mt-0">{error}</FormError>}
    </div>
  );
}
