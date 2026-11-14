import { logout, markSignedOut, revokeAuthSession, useAuthMe, useAuthSessions, keys } from "@komo/api";
import type { AuthSessionView } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { formatRelative } from "@/lib/format";
import { m } from "@/paraglide/messages.js";
import { CardRow, FormError, Pill, SettingsCard } from "../controls";
import { DesktopSignOut, desktopConnection } from "./desktop-connection";

/** 关于里的「账号」：当前账号与退出登录。本机令牌（`pnpm dev` 的代理、命令行）没有账号，写「本机访问」。 */
export function AccountRow() {
  const me = useAuthMe().data;
  const queryClient = useQueryClient();
  const desktop = desktopConnection();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  if (!me?.authenticated) return null;

  const web = me.credential === "web";
  const signOut = async () => {
    setBusy(true);
    setError("");
    try {
      await logout();
      markSignedOut(queryClient);
    } catch (failure) {
      setError(errorText(failure));
      setBusy(false);
    }
  };
  const name = me.email ?? me.subject;
  return (
    <SettingsCard>
      <CardRow
        title={m.account_title()}
        desc={me.credential === "local" ? m.account_local() : (name ?? m.account_unknown())}
        control={
          desktop ? (
            <DesktopSignOut api={desktop} />
          ) : web ? (
            <Button size="sm" disabled={busy} onClick={() => void signOut()}>
              {m.account_logout()}
            </Button>
          ) : null
        }
      />
      {error && <FormError>{error}</FormError>}
    </SettingsCard>
  );
}

/** 关于里的「已登录的设备」：网页会话与桌面端设备令牌，每行可吊销；吊销当前这条等于退出登录，要再点一次确认。 */
export function DevicesRow() {
  const sessions = useAuthSessions();
  const queryClient = useQueryClient();
  const desktop = desktopConnection();
  const [confirming, setConfirming] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState("");

  // 取不到（旧 Gateway 没有这个接口、连不上）：整块不画，免得在关于里多一条红字
  if (sessions.isError || !sessions.data) return null;

  const revoke = async (item: AuthSessionView) => {
    setBusy(item.id);
    setError("");
    try {
      if (item.current) {
        // 当前这条：桌面壳走壳的退出（吊销令牌并回连接设置页），浏览器走 `POST /auth/logout`（吊销并清 cookie）
        if (desktop) {
          const result = await desktop.signOut();
          if (result.warning) setError(result.warning);
        } else {
          await logout();
          markSignedOut(queryClient);
        }
      } else {
        await revokeAuthSession(item.id);
        await queryClient.invalidateQueries({ queryKey: keys.authSessions });
      }
      setConfirming(null);
    } catch (failure) {
      setError(errorText(failure));
    }
    setBusy(null);
  };

  const list = sessions.data.sessions;
  return (
    <SettingsCard>
      <CardRow title={m.devices_title()} desc={list.length ? m.devices_hint() : m.devices_empty()} />
      {list.length > 0 && (
        <ul className="mt-3">
          {list.map((item) => (
            <li key={item.id} className="flex items-center gap-3 border-t border-rule py-2.5 first:border-t-0 max-[600px]:flex-wrap">
              <div className="min-w-0 flex-1">
                <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-read leading-5.5">
                  <span className="[overflow-wrap:anywhere]">{item.label || m.devices_unnamed()}</span>
                  <Pill>{item.kind === "web" ? m.devices_kind_web() : m.devices_kind_device()}</Pill>
                  {item.current && <Pill tone="wait">{m.devices_current()}</Pill>}
                </p>
                <p className="mt-0.5 text-label leading-5 text-muted">{m.devices_last_used({ when: formatRelative(item.last_used_at) })}</p>
              </div>
              {confirming === item.id ? (
                <div className="flex items-center gap-2">
                  <Button variant="danger" size="sm" disabled={busy === item.id} onClick={() => void revoke(item)}>
                    {item.current ? m.devices_revoke_current_confirm() : m.devices_revoke_confirm()}
                  </Button>
                  <Button size="sm" disabled={busy === item.id} onClick={() => setConfirming(null)}>
                    {m.cancel()}
                  </Button>
                </div>
              ) : (
                <Button
                  size="sm"
                  disabled={busy !== null}
                  onClick={() => (item.current ? setConfirming(item.id) : void revoke(item))}
                >
                  {m.devices_revoke()}
                </Button>
              )}
            </li>
          ))}
        </ul>
      )}
      {confirming && sessions.data.sessions.some((x) => x.id === confirming && x.current) && (
        <p className="mt-1 text-label leading-5 text-muted">{m.devices_revoke_current_hint()}</p>
      )}
      {error && <FormError>{error}</FormError>}
    </SettingsCard>
  );
}
