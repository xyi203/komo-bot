import { putSetting, rebuildMemoryIndex, useConfigSettings, useMemoryIndexStatus } from "@komo/api";
import type { RebuildIndexResponse } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { BlockError, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { formatNumber } from "@/lib/format";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages.js";
import { CardRow, FormError, Notice, Segmented, SettingsCard, Switch, TextButton } from "../controls";
import { EditNoticeView } from "../rich";
import { SectionHead } from "../section-head";
import { useConfigEdit } from "../use-config-edit";
import { PushRow } from "./push-row";

const NAMES = { "zh-CN": () => m.language_zh_cn(), en: () => m.language_en() } as const;

// 通用里的开关：键由 Gateway 给（GET /v1/config/settings），名字与说明是界面自己的文字
const TOGGLES = [
  { key: "memory.enabled", label: () => m.config_memory_enabled(), desc: () => m.config_memory_enabled_desc() },
  { key: "compaction.enabled", label: () => m.config_compaction(), desc: () => m.config_compaction_desc() },
] as const;

const stateText = (state: string) =>
  state === "ready" ? m.memindex_state_ready() : state === "building" ? m.memindex_state_building() : state === "failed" ? m.memindex_state_failed() : state === "unconfigured" ? m.memindex_state_unconfigured() : state;

/**
 * 通用：语言（只存在这台设备）、记忆与自动压缩两个开关（一改就写）、记忆索引（状态与重建）。
 * 工具结果上限、各种时限、检索方式不在界面里（原型已精简，仍可在配置文件里改）。
 */
export function GeneralSection() {
  const { locale, locales, select } = useLocale();
  const settings = useConfigSettings();
  const edit = useConfigEdit();

  return (
    <>
      <SectionHead title={m.settings_sec_general()} />
      <SettingsCard>
        <CardRow
          title={m.language_label()}
          desc={m.general_language_desc()}
          control={<Segmented label={m.language_label()} value={locale} onChange={select} options={locales.map((code) => ({ value: code, label: NAMES[code]() }))} />}
        />
      </SettingsCard>
      <PushRow />
      <EditNoticeView notice={edit.notice} />
      {settings.isError ? (
        <BlockError message={m.config_load_error({ message: errorText(settings.error) })} onRetry={() => void settings.refetch()} />
      ) : !settings.data ? (
        <SkeletonRows count={2} className="h-16" />
      ) : (
        TOGGLES.map(({ key, label, desc }) => {
          const setting = settings.data.settings.find((x) => x.key === key);
          if (!setting) return null;
          return (
            <SettingsCard key={key}>
              <CardRow
                inline
                title={label()}
                desc={desc()}
                control={
                  <Switch
                    checked={setting.effective === true}
                    label={label()}
                    disabled={edit.busy !== ""}
                    onChange={(next) => void edit.run(key, (version) => putSetting(key, version, next), m.config_done_set({ label: label() }))}
                  />
                }
              />
              {edit.failure?.key === key && <FormError>{edit.failure.text}</FormError>}
            </SettingsCard>
          );
        })
      )}
      <IndexRow />
    </>
  );
}

/** 记忆索引：向量模型、状态与覆盖，一个重建入口；索引不可用时检索按关键词降级。 */
function IndexRow() {
  const status = useMemoryIndexStatus();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<RebuildIndexResponse | { error: string } | null>(null);

  const rebuild = async () => {
    setBusy(true);
    try {
      setResult(await rebuildMemoryIndex());
      await queryClient.invalidateQueries({ queryKey: ["config", "memory-index"] });
    } catch (error) {
      setResult({ error: errorText(error) });
    }
    setBusy(false);
  };

  const ix = status.data;
  const errors = ix?.errors ?? [];
  const desc = status.isError ? (
    <span className="text-negative">{m.memindex_load_error({ message: errorText(status.error) })}</span>
  ) : !ix ? (
    " "
  ) : (
    <>
      {ix.space ? `${ix.space.model} · ` : ""}
      {stateText(ix.state)}
      {ix.state !== "unconfigured" && <> · {m.memindex_covered({ indexed: formatNumber(Number(ix.indexed)), total: formatNumber(Number(ix.total)) })}</>}
      {errors.length > 0 && <span className="text-negative"> · {errors.join("; ")}</span>}
    </>
  );
  return (
    <SettingsCard>
      <CardRow
        title={m.settings_index_title()}
        desc={desc}
        control={
          <>
            {status.isError && (
              <TextButton onClick={() => void status.refetch()}>{m.retry()}</TextButton>
            )}
            <Button size="sm" disabled={busy} onClick={() => void rebuild()}>
              {busy ? m.memindex_rebuilding() : m.memindex_rebuild()}
            </Button>
          </>
        }
      />
      {result && (
        <Notice>
          {"error" in result ? (
            <p className="text-negative">{m.memindex_rebuild_failed({ message: result.error })}</p>
          ) : (
            <p>{result.accepted ? m.memindex_rebuild_accepted({ generation: result.generation }) : m.memindex_rebuild_running()}</p>
          )}
        </Notice>
      )}
    </SettingsCard>
  );
}
