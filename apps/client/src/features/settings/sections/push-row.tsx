import { useEffect, useState } from "react";
import { errorText } from "@/lib/errors";
import { disablePush, enablePush, readPushState, type PushState } from "@/lib/push";
import { m } from "@/paraglide/messages.js";
import { CardRow, FormError, SettingsCard, Switch } from "../controls";

/**
 * 通用里的「通知」一行：这台设备要不要收 Komo 找你的系统通知（浏览器推送）。
 * 状态由浏览器说了算（权限、订阅），所以每次挂载都重读；不支持、没装到 iOS 主屏幕、被浏览器拒绝时不给开关，只写原因。
 */
export function PushRow() {
  const [state, setState] = useState<PushState | "loading">("loading");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    let live = true;
    readPushState().then(
      (next) => live && setState(next),
      () => live && setState("unsupported"),
    );
    return () => {
      live = false;
    };
  }, []);

  const toggle = async (on: boolean) => {
    setBusy(true);
    setError("");
    try {
      setState(on ? await enablePush() : await disablePush());
    } catch (failure) {
      setError(errorText(failure));
      // 失败后以浏览器里的真实状态为准
      setState(await readPushState().catch(() => "unsupported" as const));
    }
    setBusy(false);
  };

  const desc =
    state === "ios-install"
      ? m.push_ios_install()
      : state === "denied"
        ? m.push_denied()
        : state === "unsupported"
          ? m.push_unsupported()
          : m.push_desc();
  const toggleable = state === "on" || state === "off";
  return (
    <SettingsCard>
      <CardRow
        inline
        title={m.push_title()}
        desc={desc}
        control={toggleable ? <Switch checked={state === "on"} label={m.push_title()} disabled={busy} onChange={(next) => void toggle(next)} /> : undefined}
      />
      {error && <FormError>{m.push_failed({ message: error })}</FormError>}
    </SettingsCard>
  );
}
