import { useGatewayHealth } from "@komo/api";
import { BlockError, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { formatDateTime, formatRelative } from "@/lib/format";
import { m } from "@/paraglide/messages.js";
import { CardRow, KState, KeyValues, Panel, SettingsCard } from "../controls";
import { SectionHead } from "../section-head";

/** Electron 壳（C2）给界面的约定：`window.komoDesktop?.connection?.openSettings()` 打开"换 Gateway"的设置。 */
function desktopConnection(): { openSettings: () => void } | undefined {
  return (window as unknown as { komoDesktop?: { connection?: { openSettings?: () => void } } }).komoDesktop?.connection as
    | { openSettings: () => void }
    | undefined;
}

/**
 * Gateway：版本与协议、启动时间、实例、数据目录、遥测。
 * 与原型的偏离：网页由 Gateway 同源托管，换 Gateway 就是换地址，所以不再有"改连的 Gateway"；
 * 这里只显示当前地址（`location.origin`），在 Electron 里多一个按钮去打开壳自己的连接设置。
 */
export function GatewaySection() {
  const health = useGatewayHealth();
  const head = <SectionHead title={m.settings_sec_gateway()} description={m.gateway_desc()} />;
  const desktop = desktopConnection();
  const h = health.data;
  return (
    <>
      {head}
      <SettingsCard>
        <CardRow
          title={m.gateway_connection()}
          desc={
            <>
              <span className="font-mono">{location.origin}</span>
              <span className="mx-1.5 text-faint">·</span>
              {health.isError ? <KState tone="bad">{m.gateway_unreachable()}</KState> : h ? <><KState tone="ok">{m.gateway_connected()}</KState><span className="mx-1.5 text-faint">·</span>{m.gateway_rtt({ ms: h.rtt_ms })}</> : null}
            </>
          }
          control={desktop ? <Button size="sm" onClick={() => desktop.openSettings()}>{m.gateway_change()}</Button> : undefined}
        />
      </SettingsCard>
      {health.isError ? (
        <BlockError message={m.gateway_load_error({ message: errorText(health.error) })} onRetry={() => void health.refetch()} />
      ) : !h ? (
        <SkeletonRows count={3} />
      ) : (
        <Panel>
          <KeyValues
            rows={[
              [m.gateway_version(), <span className="tabular-nums">{m.gateway_version_value({ version: h.version, protocol: h.protocol_version })}</span>],
              [m.gateway_started(), <span className="tabular-nums">{formatDateTime(h.started_at)}</span>],
              [m.gateway_instance(), <span className="font-mono">{h.instance_id}</span>],
              [m.gateway_data_dir(), <span className="font-mono">{h.data_dir}</span>],
              [
                m.gateway_telemetry(),
                h.telemetry.endpoint ? (
                  <>
                    {h.telemetry.endpoint} ·{" "}
                    {h.telemetry.last_error ? (
                      <span className="text-negative">{m.gateway_telemetry_error({ message: h.telemetry.last_error })}</span>
                    ) : h.telemetry.last_ok_at ? (
                      m.gateway_telemetry_ok({ ago: formatRelative(h.telemetry.last_ok_at) })
                    ) : (
                      m.gateway_telemetry_none()
                    )}
                  </>
                ) : (
                  <span className="text-muted">{m.gateway_telemetry_unset()}</span>
                ),
              ],
            ]}
          />
        </Panel>
      )}
    </>
  );
}
