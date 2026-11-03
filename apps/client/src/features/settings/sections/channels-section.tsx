import { isApiError, probeChannel, useChannels } from "@komo/api";
import type { ChannelView } from "@komo/protocol";
import { useState } from "react";
import { BlockError, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { formatElapsed } from "@/lib/format";
import { m } from "@/paraglide/messages.js";
import { CardRow, Hint, KState, Panel, SettingsCard, Tag } from "../controls";
import { withCode } from "../rich";
import { SectionHead } from "../section-head";

// 渠道的显示名是平台自己的名字，不随界面语言变（机器名）。
const CHANNEL_NAME: Record<string, string> = { feishu: "Feishu", telegram: "Telegram", wechat: "WeChat" };

const Sep = () => <span className="mx-1.5 text-faint">·</span>;

// Gateway 只看得见插件进程这一层；平台长连接掉了由插件自己往 home 告警
function State({ channel }: { channel: ChannelView }) {
  if (!channel.enabled) return <span className="text-muted">{m.channels_state_off()}</span>;
  if (channel.state === "running") {
    return <KState tone="ok">{channel.since ? m.channels_state_running_for({ time: formatElapsed(channel.since) }) : m.channels_state_running()}</KState>;
  }
  if (channel.state === "retrying") return <KState tone="warn">{m.channels_state_retrying({ count: channel.restarts })}</KState>;
  if (channel.state === "failed") return <KState tone="bad">{m.channels_state_failed()}</KState>;
  return <KState tone="warn">{m.channels_state_idle()}</KState>;
}

/** 渠道：每个渠道是一个插件进程；身份名单仍只在配置文件里改（docs/komo_bot.md §11.2）。 */
export function ChannelsSection() {
  const channels = useChannels();
  const [probing, setProbing] = useState("");
  const [results, setResults] = useState<Record<string, { ok: boolean; message: string }>>({});

  const probe = async (id: string) => {
    // 探测另起一个插件进程问平台，可能要几秒；结果记在那个渠道的卡片上
    setProbing(id);
    try {
      const response = await probeChannel(id);
      setResults((current) => ({ ...current, [id]: response }));
    } catch (error) {
      setResults((current) => ({ ...current, [id]: { ok: false, message: errorText(error) } }));
    }
    setProbing("");
  };

  const refresh = (
    <Button size="sm" disabled={channels.isFetching} onClick={() => void channels.refetch()}>
      {m.channels_refresh()}
    </Button>
  );
  const head = <SectionHead title={m.settings_sec_channels()} description={m.channels_desc()} right={refresh} />;
  // 404 = 在跑的 Gateway 早于 GET /v1/channels
  if (channels.isError && isApiError(channels.error) && channels.error.status === 404) {
    return (
      <>
        {head}
        <Panel>
          <p className="text-label text-muted">{withCode(m.channels_old_gateway())}</p>
        </Panel>
      </>
    );
  }
  if (channels.isError) {
    return (
      <>
        {head}
        <BlockError message={m.channels_load_error({ message: errorText(channels.error) })} onRetry={() => void channels.refetch()} />
      </>
    );
  }
  if (!channels.data) {
    return (
      <>
        {head}
        <SkeletonRows count={3} />
      </>
    );
  }
  return (
    <>
      {head}
      {channels.data.channels.map((channel) => {
        const result = results[channel.channel];
        return (
          <SettingsCard key={channel.channel}>
            <CardRow
              title={
                <>
                  {CHANNEL_NAME[channel.channel] ?? channel.channel}
                  {channel.custom_plugin && <Tag>{m.channels_custom_plugin()}</Tag>}
                </>
              }
              desc={
                <span>
                  <State channel={channel} />
                  {channel.enabled && (
                    <>
                      <Sep />
                      {channel.allow_tenant ? m.channels_allow_tenant({ count: channel.allow_from }) : m.channels_allow({ count: channel.allow_from })}
                      <Sep />
                      {channel.home_chat ? (
                        <>
                          {m.channels_home_to()} <span className="font-mono">{channel.home_chat}</span>
                        </>
                      ) : (
                        m.channels_home_none()
                      )}
                    </>
                  )}
                </span>
              }
              control={
                channel.enabled ? (
                  <Button size="sm" disabled={probing === channel.channel} onClick={() => void probe(channel.channel)}>
                    {probing === channel.channel ? m.channels_probing() : m.channels_probe()}
                  </Button>
                ) : undefined
              }
            />
            {channel.outbound_only && <Hint>{withCode(m.channels_outbound_only())}</Hint>}
            {channel.detail && channel.state !== "running" && <Hint bad>{channel.detail}</Hint>}
            {result && (
              <Hint bad={!result.ok}>
                {result.ok ? m.channels_probe_ok() : m.channels_probe_failed()} {result.message}
              </Hint>
            )}
          </SettingsCard>
        );
      })}
    </>
  );
}
