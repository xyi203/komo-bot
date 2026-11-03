import { putSetting, reloadConfig, useConfigCheck, useConfigModels, useConfigSettings } from "@komo/api";
import type { ConfigReloadResponse, ConfigSetting } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { BlockError, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { formatDateTime } from "@/lib/format";
import { m } from "@/paraglide/messages.js";
import { CardRow, FormError, NativeSelect, NumberInput, Pill, SettingsCard, Switch, TextButton } from "../controls";
import { editErrorText, useConfigEdit } from "../use-config-edit";
import { EditNoticeView, withCode } from "../rich";
import { SectionHead } from "../section-head";

// 常用设置的说法：键与种类、选项由 Gateway 给（GET /v1/config/settings），名字与说明是界面自己的文字。
const COPY: Record<string, { label: () => string; desc: () => string; unit?: () => string }> = {
  default_agent: { label: () => m.config_default_agent(), desc: () => m.config_default_agent_desc() },
  "memory.enabled": { label: () => m.config_memory_enabled(), desc: () => m.config_memory_enabled_desc() },
  "memory.model": { label: () => m.config_memory_model(), desc: () => m.config_memory_model_desc() },
  "memory.embedding": { label: () => m.config_memory_embedding(), desc: () => m.config_memory_embedding_desc() },
  "memory.retrieval.mode": { label: () => m.config_retrieval_mode(), desc: () => m.config_retrieval_mode_desc() },
  "compaction.enabled": { label: () => m.config_compaction(), desc: () => m.config_compaction_desc() },
  "execution.model_result_bytes": { label: () => m.config_result_bytes(), desc: () => m.config_result_bytes_desc(), unit: () => m.config_unit_bytes() },
  "execution.call_timeout_secs": { label: () => m.config_call_timeout(), desc: () => m.config_call_timeout_desc(), unit: () => m.config_unit_secs() },
  "execution.confirm_timeout_mins": { label: () => m.config_confirm_timeout(), desc: () => m.config_confirm_timeout_desc(), unit: () => m.config_unit_mins() },
};

const CHOICES: Record<string, () => string> = {
  hybrid: () => m.config_mode_hybrid(),
  keyword: () => m.config_mode_keyword(),
  vector: () => m.config_mode_vector(),
};

// 记忆用的向量模型换了，热重载装不上（向量后端只在启动后探测装上，docs/komo_bot.md §3）
const EMBED_RESTART_KEY = "memory.embedding";

export function ConfigSection() {
  const settings = useConfigSettings();
  const models = useConfigModels().data;
  const edit = useConfigEdit();
  const queryClient = useQueryClient();

  const labelOf = (key: string) => COPY[key]?.label() ?? key;
  const choiceText = (key: string, value: string) => {
    if (key === "memory.model") return models?.models.find((x) => x.alias === value)?.name || value;
    if (key === "memory.embedding") {
      const found = models?.embeddings?.find((x) => x.alias === value);
      return found ? `${found.model} (${value})` : value;
    }
    return CHOICES[value]?.() ?? value;
  };
  // 没写时有别的意思的两项留一个头选项：记忆模型跟着默认模型走，向量模型不设就只按关键词检索
  const unsetChoice = (setting: ConfigSetting): string | undefined => {
    if (setting.key === "memory.model") return m.config_memory_model_follow({ model: choiceText("memory.model", String(setting.effective ?? "")) });
    if (setting.key === "memory.embedding") return m.config_memory_embedding_none();
    return undefined;
  };

  const send = (key: string, value: unknown) =>
    edit.run(
      key,
      (version) => putSetting(key, version, value),
      value === null ? m.config_done_default({ label: labelOf(key) }) : m.config_done_set({ label: labelOf(key) }),
      { note: key === EMBED_RESTART_KEY && value !== null ? m.config_embed_restart() : undefined },
    );

  const head = <SectionHead title={m.settings_sec_config()} description={m.config_desc()} />;
  if (settings.isError) {
    return (
      <>
        {head}
        <BlockError message={m.config_load_error({ message: errorText(settings.error) })} onRetry={() => void settings.refetch()} />
      </>
    );
  }
  return (
    <>
      {head}
      <EditNoticeView notice={edit.notice} />
      <StatusCard
        onReloaded={() => void queryClient.invalidateQueries({ queryKey: ["config"] })}
      />
      {!settings.data ? (
        <SkeletonRows count={4} />
      ) : (
        settings.data.settings.map((setting) => {
          const copy = COPY[setting.key];
          const unsetText = unsetChoice(setting);
          const custom = setting.value != null && setting.key !== "default_agent" && !(setting.kind === "choice" && unsetText);
          const failed = edit.failure?.key === setting.key;
          const disabled = edit.busy !== "";
          return (
            <SettingsCard key={setting.key}>
              <CardRow
                inline={setting.kind === "bool"}
                title={labelOf(setting.key)}
                desc={
                  <>
                    {copy?.desc()}
                    {custom && (
                      <>
                        {" "}
                        <TextButton quiet disabled={disabled} onClick={() => void send(setting.key, null)}>
                          {m.config_restore_default()}
                        </TextButton>
                      </>
                    )}
                  </>
                }
                control={
                  setting.kind === "bool" ? (
                    <Switch
                      checked={setting.effective === true}
                      label={labelOf(setting.key)}
                      disabled={disabled}
                      onChange={(next) => void send(setting.key, next)}
                    />
                  ) : setting.kind === "choice" ? (
                    <NativeSelect
                      aria-label={labelOf(setting.key)}
                      disabled={disabled}
                      // 没写时选中生效的那一项；有头选项的两项，没写就选头选项
                      value={String(setting.value ?? (unsetText ? "" : (setting.effective ?? "")))}
                      onChange={(event) => void send(setting.key, event.target.value === "" ? null : event.target.value)}
                    >
                      {unsetText && <option value="">{unsetText}</option>}
                      {(setting.options ?? []).map((option) => (
                        <option key={option} value={option}>
                          {choiceText(setting.key, option)}
                        </option>
                      ))}
                    </NativeSelect>
                  ) : (
                    <NumberField setting={setting} unit={copy?.unit?.()} label={labelOf(setting.key)} disabled={disabled} onSave={(value) => send(setting.key, value)} />
                  )
                }
              />
              {failed && <FormError>{edit.failure?.text}</FormError>}
            </SettingsCard>
          );
        })
      )}
    </>
  );
}

function NumberField({
  setting,
  unit,
  label,
  disabled,
  onSave,
}: {
  setting: ConfigSetting;
  unit?: string;
  label: string;
  disabled: boolean;
  onSave: (value: number | null) => void;
}) {
  const saved = setting.value == null ? "" : String(setting.value);
  const [draft, setDraft] = useState(saved);
  // 保存成功后服务端值变了：输入框跟着
  const [seen, setSeen] = useState(saved);
  if (seen !== saved) {
    setSeen(saved);
    setDraft(saved);
  }
  return (
    <form
      className="flex items-center gap-2 max-[600px]:flex-1"
      onSubmit={(event) => {
        event.preventDefault();
        onSave(draft === "" ? null : Number(draft));
      }}
    >
      <NumberInput
        aria-label={label}
        min={setting.min ?? undefined}
        max={setting.max ?? undefined}
        value={draft}
        placeholder={setting.effective == null ? "" : String(setting.effective)}
        onChange={(event) => setDraft(event.target.value)}
      />
      <span className="min-w-7 text-label text-muted">{unit}</span>
      <Button size="sm" type="submit" disabled={disabled}>
        {m.controls_save()}
      </Button>
    </form>
  );
}

/** 最上面一张：配置检查（几条提醒、装载时间、文件之后改过没）、重新加载、重新检查。 */
function StatusCard({ onReloaded }: { onReloaded: () => void }) {
  const check = useConfigCheck();
  const [busy, setBusy] = useState<"" | "reload" | "check">("");
  const [reload, setReload] = useState<ConfigReloadResponse | { error: string } | null>(null);
  const data = check.data;

  const doReload = async () => {
    setBusy("reload");
    try {
      setReload(await reloadConfig());
      onReloaded();
    } catch (error) {
      setReload({ error: editErrorText(error) });
    }
    setBusy("");
  };
  const doCheck = async () => {
    setBusy("check");
    await check.refetch();
    setBusy("");
  };

  if (check.isError) {
    return (
      <SettingsCard>
        <FormError className="mt-0">{m.config_check_error({ message: errorText(check.error) })}</FormError>
      </SettingsCard>
    );
  }
  if (!data) return <SkeletonRows count={1} />;
  const issues = data.issues;
  const stale = data.sources.some((s) => new Date(s.mtime).getTime() > new Date(data.loaded_at).getTime());
  return (
    <SettingsCard>
      <CardRow
        title={issues.length ? m.config_check_issues({ count: issues.length }) : m.config_check_ok()}
        desc={
          <>
            {m.config_loaded_at({ time: formatDateTime(data.loaded_at) })}
            {stale && <span className="text-negative"> {m.config_stale()}</span>}
          </>
        }
        control={
          <>
            <Button size="sm" disabled={busy !== ""} onClick={() => void doReload()}>
              {busy === "reload" ? m.config_reloading() : m.config_reload()}
            </Button>
            <Button size="sm" disabled={busy !== ""} onClick={() => void doCheck()}>
              {m.config_recheck()}
            </Button>
          </>
        }
      />
      {issues.length > 0 && (
        <ul className="mt-3 flex flex-col gap-2">
          {issues.map((issue, i) => (
            <li key={i} className="flex flex-col gap-1 border-t border-rule pt-2 text-label leading-5">
              <span className="flex flex-wrap items-center gap-2">
                <Pill tone={issue.severity === "error" ? "bad" : "wait"}>{issue.severity === "error" ? m.config_issue_error() : m.config_issue_warning()}</Pill>
                <span className="font-mono text-small">{issue.key}</span>
              </span>
              <span className="[overflow-wrap:anywhere]">{issue.message}</span>
            </li>
          ))}
        </ul>
      )}
      {reload && (
        <div className="mt-3 rounded-control bg-raised px-3 py-2.5 text-label leading-5 shadow-ring">
          {"error" in reload ? (
            <p className="text-negative">{m.config_reload_failed({ message: reload.error })}</p>
          ) : (
            <>
              <p>{reload.changed.length ? m.config_reload_changed({ count: reload.changed.length, keys: reload.changed.join(", ") }) : m.config_reload_unchanged()}</p>
              {reload.start_only.length > 0 && <p className="mt-1.5 text-negative">{withCode(m.config_reload_start_only({ keys: reload.start_only.join(", ") }))}</p>}
            </>
          )}
        </div>
      )}
    </SettingsCard>
  );
}
