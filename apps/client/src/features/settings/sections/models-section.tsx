import {
  configKeys,
  deleteEmbeddingModel,
  deleteProvider,
  putDefaultModel,
  putEmbeddingModel,
  putProvider,
  putSetting,
  useConfigModels,
  useConfigSettings,
} from "@komo/api";
import type { ConfigSetting, EmbeddingModelEntry, ProviderEntry } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import { Icon } from "@/components/icon";
import { BlockError, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { m } from "@/paraglide/messages.js";
import { AddCard, CardRow, FormError, Hint, KState, NativeSelect, Notice, SettingsCard, TextButton, Tag } from "../controls";
import { EmbeddingForm, type EmbeddingSubmit } from "../models/embedding-form";
import { isCustomProvider, kindOf } from "../models/defs";
import { ProviderForm, type ProviderSubmit } from "../models/provider-form";
import { SectionHead } from "../section-head";
import { useConfigEdit } from "../use-config-edit";

type Editing = { kind: "provider"; id: string } | { kind: "new-provider" } | { kind: "embed"; id: string };

/**
 * 设置·模型（docs/web.md §15）：上面四个选择（默认、主对话、记忆、向量模型），下面是提供商列表；
 * 点"编辑 / 添加"右侧整块换成提供商编辑视图（类型、密钥、自定义设置与整份模型目录，底部删除）。
 * 每次写都带读到的 `version`，成功写一句回执并刷新输入框下的模型菜单；出错显示服务端的原话。
 */
export function ModelsSection() {
  const query = useConfigModels();
  const settings = useConfigSettings();
  const queryClient = useQueryClient();
  const edit = useConfigEdit();
  const [editing, setEditing] = useState<Editing | null>(null);
  const [confirm, setConfirm] = useState(false);

  const config = query.data;
  if (query.isError) {
    return (
      <>
        <SectionHead title={m.settings_sec_models()} />
        <BlockError message={m.models_load_error({ message: errorText(query.error) })} onRetry={() => void query.refetch()} />
      </>
    );
  }
  if (!config) {
    return (
      <>
        <SectionHead title={m.settings_sec_models()} />
        <SkeletonRows count={6} />
      </>
    );
  }

  const completions = config.models;
  const embeds = config.embeddings ?? [];
  const busy = edit.busy !== "";
  const idle = () => {
    setEditing(null);
    setConfirm(false);
  };
  // 成功才收起表单；失败留着，原因写在表单（或确认框）下
  const send = async (key: string, run: Parameters<typeof edit.run>[1], done: string, note?: string) => {
    if (await edit.run(key, run, done, { note })) idle();
  };

  const submitProvider = ({ was, id, rename, body }: ProviderSubmit) =>
    send(
      "form",
      (version) => putProvider(id, version, body),
      rename ? m.models_done_provider_renamed({ from: was, to: rename }) : was ? m.models_done_provider_saved({ id }) : m.models_done_provider_added({ id }),
    );
  const submitEmbed = ({ was, alias, rename, body, changed }: EmbeddingSubmit) =>
    send(
      "form",
      (version) => putEmbeddingModel(alias, version, { rename, ...body }),
      rename ? m.models_done_embed_renamed({ from: was, to: rename }) : m.models_done_embed_saved({ alias }),
      alias === config.memory_embedding && changed ? m.models_embed_restart() : undefined,
    );

  const formFailure = edit.failure?.key === "form" ? edit.failure.text : undefined;
  const confirmFailure = edit.failure?.key === "delete" ? edit.failure.text : undefined;
  const reread = () => {
    edit.setNotice(null);
    idle();
    void queryClient.invalidateQueries({ queryKey: configKeys.models });
    void queryClient.invalidateQueries({ queryKey: configKeys.settings });
    void queryClient.invalidateQueries({ queryKey: configKeys.env });
  };
  const notice = edit.notice && (
    <Notice bad={edit.notice.bad} className="mt-0 mb-3">
      <p>{edit.notice.text}</p>
      {edit.notice.reread && (
        <TextButton className="mt-1.5" onClick={reread}>
          {m.models_reread()}
        </TextButton>
      )}
    </Notice>
  );

  // ---- 编辑视图：整块换页 ----
  if (editing) {
    const back = (
      <TextButton quiet onClick={() => { edit.setFailure(null); idle(); }} className="mt-4 -mb-1 inline-flex items-center gap-1">
        <Icon name="left" />
        {m.settings_sec_models()}
      </TextButton>
    );
    if (editing.kind === "new-provider") {
      return (
        <>
          {back}
          <SectionHead title={m.models_add_provider()} />
          {notice}
          <ProviderForm config={config} provider={null} busy={busy} failure={formFailure} onSubmit={(s) => void submitProvider(s)} onCancel={idle} />
        </>
      );
    }
    if (editing.kind === "provider") {
      const provider = config.providers.find((p) => p.id === editing.id);
      if (provider) {
        const count = completions.filter((model) => model.provider === provider.id).length + embeds.filter((e) => e.provider === provider.id).length;
        return (
          <>
            {back}
            <SectionHead title={provider.name || provider.id} description={provider.name ? provider.id : undefined} />
            {notice}
            <ProviderForm key={provider.id} config={config} provider={provider} busy={busy} failure={formFailure} onSubmit={(s) => void submitProvider(s)} onCancel={idle} />
            <DeleteArea
              open={confirm}
              onOpen={() => { edit.setFailure(null); setConfirm(true); }}
              text={count ? m.models_del_provider_used({ count }) : m.models_del_provider({ id: provider.id })}
              id={provider.id}
              busy={busy}
              failure={confirmFailure}
              onSure={() => void send("delete", (version) => deleteProvider(provider.id, version), m.models_done_provider_deleted({ id: provider.id }))}
              onCancel={() => setConfirm(false)}
            />
          </>
        );
      }
    }
    if (editing.kind === "embed") {
      const entry = embeds.find((e) => e.alias === editing.id);
      if (entry) {
        const inUse = entry.alias === config.memory_embedding;
        return (
          <>
            {back}
            <SectionHead title={entry.name || entry.alias} />
            {notice}
            <EmbeddingForm key={entry.alias} entry={entry} busy={busy} failure={formFailure} onSubmit={(s) => void submitEmbed(s)} onCancel={idle} />
            <DeleteArea
              open={confirm}
              onOpen={() => { edit.setFailure(null); setConfirm(true); }}
              text={inUse ? m.models_del_embed_in_use() : m.models_del_embed({ id: entry.alias })}
              id={entry.alias}
              busy={busy}
              failure={confirmFailure}
              onSure={() => void send("delete", (version) => deleteEmbeddingModel(entry.alias, version), m.models_done_embed_deleted({ id: entry.alias }))}
              onCancel={() => setConfirm(false)}
            />
          </>
        );
      }
    }
    // 编辑的那一项已经不在了（别处删了）：落回下面的列表
  }

  // ---- 列表视图 ----
  const settingOf = (key: string) => settings.data?.settings.find((x) => x.key === key);
  const modelName = (alias: string) => completions.find((x) => x.alias === alias)?.name || alias;
  const embedName = (alias: string) => {
    const found = embeds.find((x) => x.alias === alias);
    return found ? `${found.name || found.model} (${alias})` : alias;
  };
  const defaultName = config.default ? modelName(config.default) : "";

  const choiceRow = (key: string, title: string, desc: string, unsetText: string | undefined, label: (alias: string) => string) => {
    const setting: ConfigSetting | undefined = settingOf(key);
    return (
      <SettingsCard>
        <CardRow
          title={title}
          desc={desc}
          control={
            setting ? (
              <NativeSelect
                aria-label={title}
                disabled={busy}
                value={String(setting.value ?? "")}
                onChange={(event) => {
                  const value = event.target.value === "" ? null : event.target.value;
                  void send(key, (version) => putSetting(key, version, value), value === null ? m.config_done_default({ label: title }) : m.config_done_set({ label: title }), key === "memory.embedding" && value !== null ? m.config_embed_restart() : undefined);
                }}
              >
                {unsetText && <option value="">{unsetText}</option>}
                {(setting.options ?? []).map((option) => (
                  <option key={option} value={option}>
                    {label(option)}
                  </option>
                ))}
              </NativeSelect>
            ) : settings.isError ? undefined : (
              <span className="block h-9 w-55 rounded-control bg-active" aria-hidden="true" />
            )
          }
        />
        {!setting && settings.isError && <FormError>{m.config_load_error({ message: errorText(settings.error) })}</FormError>}
        {edit.failure?.key === key && <FormError>{edit.failure.text}</FormError>}
      </SettingsCard>
    );
  };

  const providerRow = (provider: ProviderEntry) => {
    const count = completions.filter((model) => model.provider === provider.id).length + embeds.filter((e) => e.provider === provider.id).length;
    const ready = provider.key_set || !!provider.auth;
    return (
      <Card
        key={provider.id}
        title={
          <>
            <span className="text-read">{provider.name || provider.id}</span>
            {kindOf(provider) === "embedding" && <Tag className="ml-0">{m.models_tag_embedding()}</Tag>}
            {isCustomProvider(provider.base_url) && <Tag className="ml-0">{m.models_tag_custom()}</Tag>}
          </>
        }
        desc={
          <>
            <KState tone={ready ? "ok" : "warn"}>{providerDotTitle(provider)}</KState>
            <span className="mx-1.5 text-faint">·</span>
            {m.models_model_count({ count })}
          </>
        }
        actions={
          <Button size="sm" onClick={() => { edit.setFailure(null); setConfirm(false); setEditing({ kind: "provider", id: provider.id }); }}>
            {ready ? m.models_edit() : m.models_fill_key()}
          </Button>
        }
      />
    );
  };

  // 没挂 provider 的向量模型：各占一行（端点写在模型项里）
  const looseEmbedRow = (entry: EmbeddingModelEntry) => (
    <Card
      key={`embed:${entry.alias}`}
      title={
        <>
          <span className="text-read">{entry.name || entry.alias}</span>
          <Tag className="ml-0">{m.models_tag_embedding()}</Tag>
          <Tag className="ml-0">{m.models_tag_custom()}</Tag>
          {entry.alias === config.memory_embedding && <Tag className="ml-0">{m.models_in_use()}</Tag>}
        </>
      }
      desc={
        <KState tone={entry.key_set ? "ok" : "warn"}>
          {entry.key_set ? m.models_key_set({ name: entry.env_key ?? m.models_key_generic() }) : m.models_key_missing({ name: entry.env_key ?? "" })}
        </KState>
      }
      actions={
        <Button size="sm" onClick={() => { edit.setFailure(null); setConfirm(false); setEditing({ kind: "embed", id: entry.alias }); }}>
          {m.models_edit()}
        </Button>
      }
    />
  );

  return (
    <>
      <SectionHead title={m.settings_sec_models()} description={m.models_desc()} />
      {notice}
      <SettingsCard>
        <CardRow
          title={m.models_default()}
          desc={m.models_default_desc()}
          control={
            completions.length > 0 ? (
              <NativeSelect
                aria-label={m.models_default()}
                disabled={busy}
                value={config.default ?? ""}
                onChange={(e) => {
                  const alias = e.target.value;
                  void send("default", (version) => putDefaultModel(version, alias), m.models_done_default({ alias }));
                }}
              >
                {completions.map((model) => (
                  <option key={model.alias} value={model.alias}>
                    {model.name || model.alias}
                  </option>
                ))}
              </NativeSelect>
            ) : undefined
          }
        />
        {completions.length === 0 && <Hint>{m.models_no_completion()}</Hint>}
        {edit.failure?.key === "default" && <FormError>{edit.failure.text}</FormError>}
      </SettingsCard>
      {choiceRow("models.main", m.models_main(), m.models_main_desc(), m.models_follow_default({ model: defaultName }), modelName)}
      {choiceRow("memory.model", m.config_memory_model(), m.config_memory_model_desc(), m.config_memory_model_follow({ model: modelName(String(settingOf("memory.model")?.effective ?? config.default ?? "")) }), modelName)}
      {choiceRow("memory.embedding", m.config_memory_embedding(), m.config_memory_embedding_desc(), m.config_memory_embedding_none(), embedName)}

      <h4 className="mt-7 mb-1 text-label font-semibold">{m.models_providers()}</h4>
      {embeds.length === 0 && <Hint className="mt-1 mb-3">{m.models_no_embedding()}</Hint>}
      {config.providers.map(providerRow)}
      {embeds.filter((e) => !e.provider).map(looseEmbedRow)}
      {completions
        .filter((model) => model.standalone)
        .map((model) => (
          <Card
            key={`chat:${model.alias}`}
            title={
              <>
                <span className="text-read">{model.name || model.alias}</span>
                <Tag className="ml-0">{m.models_tag_custom()}</Tag>
              </>
            }
            actions={<span className="text-small text-muted">{m.models_standalone_only()}</span>}
          />
        ))}
      <AddCard
        onClick={() => {
          edit.setFailure(null);
          setConfirm(false);
          setEditing({ kind: "new-provider" });
        }}
      >
        <Icon name="plus" className="size-3.5" />
        {m.models_add_provider()}
      </AddCard>
    </>
  );
}

function providerDotTitle(provider: ProviderEntry): string {
  if (provider.auth) return m.models_key_login({ auth: provider.auth });
  if (provider.key_set) return m.models_key_set({ name: provider.env_key ?? "" });
  return provider.env_key ? m.models_key_missing({ name: provider.env_key }) : m.models_key_unnamed();
}

/** 提供商的一行，和其余设置行同一套尺寸：上一行名称与标签，下一行凭证状态与模型数，操作在右。 */
function Card({ title, desc, actions }: { title: ReactNode; desc?: ReactNode; actions?: ReactNode }) {
  return (
    <section className="border-b border-rule py-4 max-[600px]:py-3.5">
      <header className="flex items-center gap-6 max-[600px]:gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex min-h-5.5 flex-wrap items-center gap-x-2 gap-y-1 text-read leading-5.5">{title}</div>
          {desc && <p className="mt-1 text-label leading-5 text-muted">{desc}</p>}
        </div>
        {actions && <span className="flex shrink-0 items-center gap-3">{actions}</span>}
      </header>
    </section>
  );
}

/** 编辑视图底部的删除：先点一下展开确认，被引用的由 Gateway 拒绝，原话写在确认框里。 */
function DeleteArea({
  open,
  onOpen,
  text,
  id,
  busy,
  failure,
  onSure,
  onCancel,
}: {
  open: boolean;
  onOpen: () => void;
  text: string;
  id: string;
  busy: boolean;
  failure?: string;
  onSure: () => void;
  onCancel: () => void;
}) {
  return (
    <div className="mt-8 border-t border-rule pt-4">
      {open ? (
        <div className="rounded-control p-3 text-label shadow-[0_0_0_1px_var(--color-negative-ring)]">
          <p>{text}</p>
          {failure && <p className="mt-2 text-negative">{failure}</p>}
          <div className="mt-2.5 flex flex-wrap gap-2">
            <Button variant="danger" disabled={busy} onClick={onSure}>
              {m.models_del_sure({ id })}
            </Button>
            <Button onClick={onCancel}>{m.cancel()}</Button>
          </div>
        </div>
      ) : (
        <Button variant="danger" onClick={onOpen}>
          {m.models_delete()}
        </Button>
      )}
    </div>
  );
}
