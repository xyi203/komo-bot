import { fetchProviderModels, type ProviderRequestBody } from "@komo/api";
import type { ConfigModelsResponse, ProviderEntry } from "@komo/protocol";
import { useId, useState, type FormEvent } from "react";
import { Icon } from "@/components/icon";
import { Button } from "@/components/ui/button";
import { IconButton } from "@/components/ui/icon-button";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { Field, FormError, Hint, NativeSelect, Segmented, TextButton, TextInput } from "../controls";
import { editErrorText } from "../use-config-edit";
import {
  aliasOf,
  backendName,
  COMPLETION_BACKENDS,
  EMBED_BACKENDS,
  isCustomProvider,
  isEmbedBackend,
  list,
  num,
  PRESETS,
  providerDraft,
  type Preset,
  type Draft,
  type Kind,
  type Row,
} from "./defs";

export interface ProviderSubmit {
  /** 编辑时是原来的 id；新建时是空串。 */
  was: string;
  id: string;
  rename?: string;
  kind: Kind;
  body: ProviderRequestBody;
}

/**
 * provider 表单（新建与编辑共用）：新建时先选"第三方模型提供商"（预设填好 ID、地址、协议）或"自定义模型 API"；
 * 字段平铺；下面是这个 provider 的整份模型目录，保存时和 provider 一起写（目录外原来挂着的同类模型删掉）。
 */
export function ProviderForm({
  config,
  provider,
  busy,
  failure,
  onSubmit,
  onCancel,
}: {
  config: ConfigModelsResponse;
  provider: ProviderEntry | null;
  busy: boolean;
  failure?: string;
  onSubmit: (submit: ProviderSubmit) => void;
  onCancel: () => void;
}) {
  const uid = useId();
  const isNew = !provider;
  const [draft, setDraft] = useState<Draft>(() => providerDraft(config, provider));
  const [localError, setLocalError] = useState("");
  const { v } = draft;
  const kind: Kind = isEmbedBackend(v.api_backend) ? "embedding" : "completion";
  const used =
    !!provider && (config.models.some((model) => model.provider === provider.id) || (config.embeddings ?? []).some((e) => e.provider === provider.id));
  const patch = (next: Partial<Draft>) => setDraft((d) => ({ ...d, ...next }));
  const patchV = (next: Partial<Draft["v"]>) => setDraft((d) => ({ ...d, v: { ...d.v, ...next } }));
  const patchRow = (i: number, next: Partial<Row>) => setDraft((d) => ({ ...d, rows: d.rows.map((row, k) => (k === i ? { ...row, ...next } : row)) }));

  const pickPreset = (preset: Preset) =>
    setDraft((d) => ({
      ...d,
      preset: preset.key,
      fetched: null,
      v: { ...d.v, pid: preset.id, name: preset.label, base_url: preset.base_url, api_backend: preset.api_backend, env_key: preset.env_key },
    }));

  const tabs = isNew && (
    <>
      <Segmented
        label={m.models_tab_label()}
        value={draft.tab}
        onChange={(tab) => {
          // 从预设切到自定义：预设填进去的值清掉，从空表开始
          if (tab !== draft.tab) setDraft({ ...providerDraft(config, null), tab });
        }}
        options={[
          { value: "preset", label: m.models_tab_preset() },
          { value: "custom", label: m.models_tab_custom() },
        ]}
      />
      <Hint className="mt-2.5">{draft.tab === "preset" ? m.models_tab_preset_hint() : m.models_tab_custom_hint()}</Hint>
    </>
  );

  if (isNew && draft.tab === "preset" && !draft.preset) {
    return (
      <div className="pt-3.5">
        {tabs}
        <PresetGrid selected={draft.preset} onPick={pickPreset} />
        <div className="mt-4">
          <Button onClick={onCancel}>{m.cancel()}</Button>
        </div>
      </div>
    );
  }

  // 换类型：协议换成那一类里最可能的一个（11434 端口是 Ollama），地址、密钥、目录里的模型 id 都留着。
  // 预设的同一家常要对话、嵌入两个提供商：切到嵌入时 ID 与名称跟着加后缀，免得和对话那个撞上。
  const switchKind = (to: Kind) => {
    if (to === kind) return;
    const preset = PRESETS.find((p) => p.key === draft.preset);
    const next: Partial<Draft["v"]> = {
      api_backend: to === "embedding" ? (/:11434\b/.test(v.base_url) ? "ollama_embeddings" : "embeddings") : (preset?.api_backend ?? "chat_completions"),
    };
    if (preset && to === "embedding" && v.pid === preset.id) Object.assign(next, { pid: `${preset.id}-embed`, name: m.models_embed_name({ label: preset.label }) });
    if (preset && to === "completion" && v.pid === `${preset.id}-embed`) Object.assign(next, { pid: preset.id, name: preset.label });
    patchV(next);
  };

  const fetchModels = async () => {
    patch({ fetching: true });
    try {
      const response = await fetchProviderModels({
        provider: provider?.id,
        base_url: v.base_url || undefined,
        api_backend: v.api_backend || undefined,
        env_key: v.env_key || undefined,
        api_key: v.api_key || undefined,
      });
      patch({ fetching: false, fetched: { models: response.models } });
    } catch (error) {
      patch({ fetching: false, fetched: { error: editErrorText(error) } });
    }
  };

  const pick = (id: string) =>
    setDraft((d) => {
      const at = d.rows.findIndex((row) => row.model.trim() === id);
      if (at >= 0) return { ...d, rows: d.rows.filter((_, k) => k !== at) };
      const blank = d.rows.findIndex((row) => !row.model.trim());
      return blank >= 0 ? { ...d, rows: d.rows.map((row, k) => (k === blank ? { ...row, model: id } : row)) } : { ...d, rows: [...d.rows, { model: id, name: "" }] };
    });

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const id = provider?.id ?? v.pid.trim();
    if (isNew && config.providers.some((p) => p.id === id)) {
      setLocalError(m.models_dup_provider({ id }));
      return;
    }
    setLocalError("");
    const rename = provider && v.pid.trim() !== provider.id ? v.pid.trim() : undefined;
    const models = draft.rows
      .filter((row) => row.model.trim())
      .map((row) => ({
        alias: (row.alias ?? "").trim() || aliasOf(row.model),
        model: row.model.trim(),
        name: row.name.trim() || undefined,
        ...(kind === "embedding"
          ? { dimensions: num(row.dimensions) }
          : { context_window: num(row.context_window), effort: (row.effort ?? "").trim() || undefined, efforts: list(row.efforts) }),
      }));
    onSubmit({
      was: provider?.id ?? "",
      id,
      rename,
      kind,
      body: {
        rename,
        name: v.name.trim() || undefined,
        api_backend: v.api_backend,
        base_url: v.base_url.trim() || undefined,
        env_key: v.env_key.trim() || undefined,
        auth: provider?.auth || undefined,
        timeout_secs: num(v.timeout_secs),
        api_key: v.api_key.trim() || undefined,
        models,
      },
    });
  };

  const fetched = draft.fetched;
  const have = new Set(draft.rows.map((row) => row.model.trim()));
  const filter = draft.filter.toLowerCase();
  // 不是预设的地址（自定义接口）就把"自定义设置"默认展开；手动开合过就按人点的
  const custom = isNew ? draft.tab === "custom" : isCustomProvider(v.base_url);
  const advOpen = draft.advOpen ?? custom;
  const showTitle = !(isNew && draft.tab === "custom");
  const backends: readonly string[] = kind === "embedding" ? EMBED_BACKENDS : COMPLETION_BACKENDS;

  return (
    <form onSubmit={submit} className="pt-0.5">
      {isNew && <div className="pt-3.5">{tabs}</div>}
      {isNew && draft.tab === "preset" && <PresetGrid selected={draft.preset} onPick={pickPreset} />}
      {showTitle && (
        <div className="mt-3.5 flex items-baseline gap-2.5">
          <span className="text-read font-semibold">{v.name || v.pid}</span>
          {v.name && v.pid && <span className="text-label text-muted">{v.pid}</span>}
        </div>
      )}

      <div className="mt-3.5 flex flex-col gap-1.5">
        <span className="text-label font-medium">{m.models_kind()}</span>
        <div>
          <Segmented
            label={m.models_kind()}
            value={kind}
            disabled={used}
            onChange={switchKind}
            options={[
              { value: "completion", label: m.models_kind_completion() },
              { value: "embedding", label: m.models_kind_embedding() },
            ]}
          />
        </div>
        {used && <p className="text-small text-muted">{m.models_kind_locked()}</p>}
      </div>

      <div className="mt-3.5">
        {provider?.auth ? (
          <Hint className="mt-0">{m.models_key_login({ auth: provider.auth })}</Hint>
        ) : (
          <Field label={m.models_api_key()} htmlFor={`${uid}-key`} hint={m.models_api_key_hint()}>
            <TextInput
              id={`${uid}-key`}
              type="password"
              autoComplete="new-password"
              value={v.api_key}
              onChange={(e) => patchV({ api_key: e.target.value })}
              placeholder={provider?.key_set ? m.models_api_key_keep() : m.models_api_key_enter()}
            />
          </Field>
        )}
      </div>

      <details
        open={advOpen}
        onToggle={(e) => {
          // 渲染时的 open 与人点的不一致才记：避免 React 自己改 open 触发的 toggle 绕一圈
          const open = e.currentTarget.open;
          if (open !== advOpen) patch({ advOpen: open });
        }}
        className="mt-4 border-t border-rule pt-3 text-label"
      >
        <summary className="cursor-pointer font-medium text-muted">{m.models_advanced()}</summary>
        <div className="mt-2 grid gap-3.5">
          <Field
            label={m.models_provider_id()}
            htmlFor={`${uid}-id`}
            hint={isNew ? m.models_provider_id_hint_new() : m.models_provider_id_hint_edit()}
          >
            <TextInput
              id={`${uid}-id`}
              required
              pattern="[A-Za-z][A-Za-z0-9._\-]*"
              maxLength={64}
              value={v.pid}
              onChange={(e) => patchV({ pid: e.target.value })}
              placeholder="acme-gateway"
            />
          </Field>
          <Field label={m.models_display_name()} htmlFor={`${uid}-name`}>
            <TextInput id={`${uid}-name`} maxLength={64} value={v.name} onChange={(e) => patchV({ name: e.target.value })} placeholder={m.models_display_name()} />
          </Field>
          <Field label={m.models_base_url()} htmlFor={`${uid}-url`}>
            <TextInput id={`${uid}-url`} value={v.base_url} onChange={(e) => patchV({ base_url: e.target.value })} placeholder="https://gateway.example/v1" />
          </Field>
          <Field label={m.models_backend()} htmlFor={`${uid}-backend`}>
            {/* 协议下拉只列这个类型的；类型在面板顶上单独切（挂着模型时两样都锁住） */}
            <NativeSelect
              id={`${uid}-backend`}
              disabled={used}
              value={v.api_backend}
              onChange={(e) => patchV({ api_backend: e.target.value })}
              className="w-1/2 min-w-55 max-[600px]:w-full"
            >
              {backends.map((backend) => (
                <option key={backend} value={backend}>
                  {backendName(backend)}
                </option>
              ))}
            </NativeSelect>
          </Field>
          <div className="grid grid-cols-2 gap-3 max-[600px]:grid-cols-1">
            <Field label={m.models_env_key()} htmlFor={`${uid}-env`}>
              <TextInput id={`${uid}-env`} mono value={v.env_key} onChange={(e) => patchV({ env_key: e.target.value })} placeholder={m.models_env_key_ph()} />
            </Field>
            <Field label={m.models_timeout()} htmlFor={`${uid}-timeout`}>
              <TextInput
                id={`${uid}-timeout`}
                type="number"
                min={1}
                value={v.timeout_secs}
                onChange={(e) => patchV({ timeout_secs: e.target.value })}
                placeholder={m.models_timeout_ph()}
              />
            </Field>
          </div>
        </div>
      </details>

      <div className="mt-4 flex items-center justify-between gap-3 border-t border-rule pt-3.5">
        <h5 className="m-0 flex items-baseline gap-2 text-label font-medium">
          {m.models_catalog()}
          <span className="text-small font-normal text-muted">{kind === "embedding" ? m.models_catalog_embedding() : m.models_catalog_completion()}</span>
        </h5>
        <TextButton quiet disabled={draft.fetching} onClick={() => void fetchModels()}>
          {draft.fetching ? m.models_fetching() : m.models_fetch()}
        </TextButton>
      </div>

      {fetched && "error" in fetched && <Hint bad>{fetched.error}</Hint>}
      {fetched && "models" in fetched && (
        <div className="mt-2.5 grid gap-2 rounded-inner bg-ground p-2.5">
          <TextInput
            value={draft.filter}
            onChange={(e) => patch({ filter: e.target.value })}
            placeholder={m.models_filter_placeholder({ count: fetched.models.length })}
            aria-label={m.models_filter_label()}
          />
          <div className="flex max-h-45 flex-wrap gap-1.5 overflow-y-auto">
            {fetched.models.filter((id) => !filter || id.toLowerCase().includes(filter)).slice(0, 200).map((id) => (
              <button
                key={id}
                type="button"
                aria-pressed={have.has(id)}
                onClick={() => pick(id)}
                className={cn(
                  "inline-flex h-7 cursor-pointer items-center gap-1 rounded-full px-2.5 font-mono text-small shadow-ring",
                  have.has(id) ? "bg-active" : "bg-surface",
                )}
              >
                {have.has(id) && <Icon name="check" className="size-3" />}
                {id}
              </button>
            ))}
            {!fetched.models.some((id) => !filter || id.toLowerCase().includes(filter)) && (
              <span className="text-small text-muted">{m.models_no_match()}</span>
            )}
          </div>
        </div>
      )}

      <div className="mt-2.5 grid gap-2">
        {draft.rows.length === 0 && <p className="text-small text-muted">{m.models_catalog_empty()}</p>}
        {draft.rows.map((row, i) => (
          <div key={i} className="rounded-inner bg-surface p-2 shadow-ring">
            <div className="grid grid-cols-[minmax(0,1.4fr)_minmax(0,1fr)_32px_32px] items-center gap-2">
              <TextInput mono value={row.model} onChange={(e) => patchRow(i, { model: e.target.value })} placeholder={m.models_model_id()} aria-label={m.models_model_id()} />
              <TextInput value={row.name} onChange={(e) => patchRow(i, { name: e.target.value })} placeholder={m.models_display_name()} aria-label={m.models_display_name()} />
              <IconButton
                name="right"
                label={row.open ? m.models_row_collapse() : m.models_row_expand()}
                aria-expanded={!!row.open}
                onClick={() => patchRow(i, { open: !row.open })}
                className={cn("[&_svg]:transition-transform", row.open && "[&_svg]:rotate-90")}
              />
              <IconButton name="trash" label={m.models_row_delete()} onClick={() => patch({ rows: draft.rows.filter((_, k) => k !== i) })} />
            </div>
            {row.open && (
              <div className="mt-2 grid grid-cols-[repeat(auto-fit,minmax(150px,1fr))] gap-x-2 gap-y-2 px-0.5 pb-1">
                <RowField label={m.models_row_alias()} id={`${uid}-a-${i}`}>
                  <TextInput id={`${uid}-a-${i}`} mono value={row.alias ?? ""} onChange={(e) => patchRow(i, { alias: e.target.value })} placeholder={aliasOf(row.model) || m.models_alias_auto()} />
                </RowField>
                {kind === "embedding" ? (
                  <RowField label={m.models_dimensions()} id={`${uid}-d-${i}`}>
                    <TextInput id={`${uid}-d-${i}`} type="number" min={1} value={row.dimensions ?? ""} onChange={(e) => patchRow(i, { dimensions: e.target.value })} placeholder={m.models_dimensions_ph()} />
                  </RowField>
                ) : (
                  <>
                    <RowField label={m.models_window()} id={`${uid}-w-${i}`}>
                      <TextInput id={`${uid}-w-${i}`} type="number" min={1} value={row.context_window ?? ""} onChange={(e) => patchRow(i, { context_window: e.target.value })} placeholder={m.models_window_ph()} />
                    </RowField>
                    <RowField label={m.models_effort()} id={`${uid}-e-${i}`}>
                      <TextInput id={`${uid}-e-${i}`} value={row.effort ?? ""} onChange={(e) => patchRow(i, { effort: e.target.value })} placeholder={m.models_effort_ph()} />
                    </RowField>
                    <RowField label={m.models_efforts()} id={`${uid}-es-${i}`}>
                      <TextInput id={`${uid}-es-${i}`} value={row.efforts ?? ""} onChange={(e) => patchRow(i, { efforts: e.target.value })} placeholder="low, medium, high" />
                    </RowField>
                  </>
                )}
              </div>
            )}
          </div>
        ))}
      </div>
      <Button size="sm" className="mt-2.5" onClick={() => patch({ rows: [...draft.rows, { model: "", name: "" }] })}>
        <Icon name="plus" className="size-3.5" />
        {m.models_add_model()}
      </Button>

      {(localError || failure) && <FormError>{localError || failure}</FormError>}
      <div className="mt-4 flex flex-wrap gap-2">
        <Button type="submit" variant="primary" disabled={busy}>
          {busy ? m.models_saving() : isNew ? m.models_add() : m.models_save()}
        </Button>
        <Button onClick={onCancel}>{m.cancel()}</Button>
      </div>
    </form>
  );
}

/** 预设卡：点一张填好 ID、显示名称、地址、协议，只剩密钥和模型。 */
function PresetGrid({ selected, onPick }: { selected: string | null; onPick: (preset: Preset) => void }) {
  return (
    <div className="mt-3 grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-2.5">
      {PRESETS.map((preset) => (
        <button
          key={preset.key}
          type="button"
          aria-pressed={selected === preset.key}
          onClick={() => onPick(preset)}
          className={cn(
            "flex cursor-pointer flex-col gap-0.5 rounded-inner bg-surface px-3.5 py-3 text-left shadow-ring hover:shadow-ring-control",
            selected === preset.key && "shadow-[0_0_0_2px_var(--color-primary)] hover:shadow-[0_0_0_2px_var(--color-primary)]",
          )}
        >
          <b className="font-semibold">{preset.label}</b>
          <span className="text-small text-muted">{backendName(preset.api_backend)}</span>
        </button>
      ))}
    </div>
  );
}

function RowField({ label, id, children }: { label: string; id: string; children: React.ReactNode }) {
  return (
    <div className="flex min-w-0 flex-col gap-1">
      <label htmlFor={id} className="text-small text-muted">
        {label}
      </label>
      {children}
    </div>
  );
}
