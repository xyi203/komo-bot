import {
  configKeys,
  deleteEmbeddingModel,
  deleteProvider,
  putDefaultModel,
  putEmbeddingModel,
  putProvider,
  useConfigModels,
} from "@komo/api";
import type { EmbeddingModelEntry, ProviderEntry } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import { Icon } from "@/components/icon";
import { BlockError, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { m } from "@/paraglide/messages.js";
import { AddCard, Hint, KState, NativeSelect, Notice, TextButton, Tag } from "../controls";
import { EmbeddingForm, type EmbeddingSubmit } from "../models/embedding-form";
import { isCustomProvider, kindOf } from "../models/defs";
import { ProviderForm, type ProviderSubmit } from "../models/provider-form";
import { SectionHead } from "../section-head";
import { useConfigEdit } from "../use-config-edit";

type Editing = { kind: "provider"; id: string } | { kind: "new-provider" } | { kind: "embed"; id: string };
type Confirm = { kind: "provider" | "embed"; id: string };

/**
 * 设置·模型（docs/web.md §15）：一个提供商一行（显示名称、嵌入 / 自定义标签、凭证状态点），编辑时在卡里展开灰底面板，
 * 面板里是类型、密钥、自定义设置与整份模型目录；每次写都带读到的 `version`，成功写一句回执并刷新输入框下的模型菜单。
 */
export function ModelsSection() {
  const query = useConfigModels();
  const queryClient = useQueryClient();
  const edit = useConfigEdit();
  const [editing, setEditing] = useState<Editing | null>(null);
  const [confirm, setConfirm] = useState<Confirm | null>(null);

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
    setConfirm(null);
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

  const defaultPick = completions.length > 0 && (
    <label className="flex shrink-0 items-center gap-2 text-label text-muted max-[600px]:self-stretch">
      <span>{m.models_default()}</span>
      <NativeSelect
        disabled={busy}
        value={config.default ?? ""}
        onChange={(e) => {
          const alias = e.target.value;
          void send("default", (version) => putDefaultModel(version, alias), m.models_done_default({ alias }));
        }}
        className="min-w-50 text-text max-[600px]:min-w-0 max-[600px]:flex-1"
      >
        {completions.map((model) => (
          <option key={model.alias} value={model.alias}>
            {model.name || model.alias}
          </option>
        ))}
      </NativeSelect>
    </label>
  );

  const formFailure = edit.failure?.key === "form" ? edit.failure.text : undefined;
  const confirmFailure = edit.failure?.key === "delete" ? edit.failure.text : undefined;

  const providerRow = (provider: ProviderEntry) => {
    const open = editing?.kind === "provider" && editing.id === provider.id;
    const count = completions.filter((model) => model.provider === provider.id).length + embeds.filter((e) => e.provider === provider.id).length;
    const asking = confirm?.kind === "provider" && confirm.id === provider.id;
    return (
      <Card
        key={provider.id}
        open={open}
        title={
          <>
            <span className="text-read">{provider.name || provider.id}</span>
            {kindOf(provider) === "embedding" && <Tag className="ml-0">{m.models_tag_embedding()}</Tag>}
            {isCustomProvider(provider.base_url) && <Tag className="ml-0">{m.models_tag_custom()}</Tag>}
          </>
        }
        desc={<KState tone={provider.key_set ? "ok" : "warn"}>{providerDotTitle(provider)}</KState>}
        actions={
          <>
            <Button size="sm" onClick={() => { edit.setFailure(null); setConfirm(null); setEditing({ kind: "provider", id: provider.id }); }}>
              {m.models_edit()}
            </Button>
            <TextButton quiet className="text-negative hover:text-negative" onClick={() => { edit.setFailure(null); setEditing(null); setConfirm({ kind: "provider", id: provider.id }); }}>
              {m.models_delete()}
            </TextButton>
          </>
        }
      >
        {open ? (
          <Panel>
            <ProviderForm config={config} provider={provider} busy={busy} failure={formFailure} onSubmit={(s) => void submitProvider(s)} onCancel={idle} />
          </Panel>
        ) : (
          asking && (
            <DeleteAsk
              text={count ? m.models_del_provider_used({ count }) : m.models_del_provider({ id: provider.id })}
              id={provider.id}
              busy={busy}
              failure={confirmFailure}
              onSure={() => void send("delete", (version) => deleteProvider(provider.id, version), m.models_done_provider_deleted({ id: provider.id }))}
              onCancel={idle}
            />
          )
        )}
      </Card>
    );
  };

  // 没挂 provider 的向量模型：各占一行（端点写在模型项里）
  const looseEmbedRow = (entry: EmbeddingModelEntry) => {
    const open = editing?.kind === "embed" && editing.id === entry.alias;
    const inUse = entry.alias === config.memory_embedding;
    const asking = confirm?.kind === "embed" && confirm.id === entry.alias;
    return (
      <Card
        key={`embed:${entry.alias}`}
        open={open}
        title={
          <>
            <span className="text-read">{entry.name || entry.alias}</span>
            <Tag className="ml-0">{m.models_tag_embedding()}</Tag>
            <Tag className="ml-0">{m.models_tag_custom()}</Tag>
            {inUse && <Tag className="ml-0">{m.models_in_use()}</Tag>}
          </>
        }
        desc={
          <KState tone={entry.key_set ? "ok" : "warn"}>
            {entry.key_set ? m.models_key_set({ name: entry.env_key ?? m.models_key_generic() }) : m.models_key_missing({ name: entry.env_key ?? "" })}
          </KState>
        }
        actions={
          <>
            <Button size="sm" onClick={() => { edit.setFailure(null); setConfirm(null); setEditing({ kind: "embed", id: entry.alias }); }}>
              {m.models_edit()}
            </Button>
            <TextButton quiet className="text-negative hover:text-negative" onClick={() => { edit.setFailure(null); setEditing(null); setConfirm({ kind: "embed", id: entry.alias }); }}>
              {m.models_delete()}
            </TextButton>
          </>
        }
      >
        {open ? (
          <Panel>
            <EmbeddingForm entry={entry} busy={busy} failure={formFailure} onSubmit={(s) => void submitEmbed(s)} onCancel={idle} />
          </Panel>
        ) : (
          asking && (
            <DeleteAsk
              text={inUse ? m.models_del_embed_in_use() : m.models_del_embed({ id: entry.alias })}
              id={entry.alias}
              busy={busy}
              failure={confirmFailure}
              onSure={() => void send("delete", (version) => deleteEmbeddingModel(entry.alias, version), m.models_done_embed_deleted({ id: entry.alias }))}
              onCancel={idle}
            />
          )
        )}
      </Card>
    );
  };

  return (
    <>
      <SectionHead title={m.settings_sec_models()} description={m.models_desc()} right={defaultPick || undefined} />
      {edit.notice && (
        <Notice bad={edit.notice.bad} className="mt-0 mb-3">
          <p>{edit.notice.text}</p>
          {edit.notice.reread && (
            <TextButton
              className="mt-1.5"
              onClick={() => {
                edit.setNotice(null);
                idle();
                void queryClient.invalidateQueries({ queryKey: configKeys.models });
                void queryClient.invalidateQueries({ queryKey: configKeys.settings });
                void queryClient.invalidateQueries({ queryKey: configKeys.env });
              }}
            >
              {m.models_reread()}
            </TextButton>
          )}
        </Notice>
      )}
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
      {editing?.kind === "new-provider" ? (
        <Card open title={<span className="text-read">{m.models_add_provider()}</span>}>
          <Panel>
            <ProviderForm config={config} provider={null} busy={busy} failure={formFailure} onSubmit={(s) => void submitProvider(s)} onCancel={idle} />
          </Panel>
        </Card>
      ) : (
        <AddCard
          onClick={() => {
            edit.setFailure(null);
            setConfirm(null);
            setEditing({ kind: "new-provider" });
          }}
        >
          <Icon name="plus" className="size-3.5" />
          {m.models_add_provider()}
        </AddCard>
      )}
    </>
  );
}

function providerDotTitle(provider: ProviderEntry): string {
  if (provider.auth) return m.models_key_login({ auth: provider.auth });
  if (provider.key_set) return m.models_key_set({ name: provider.env_key ?? "" });
  return provider.env_key ? m.models_key_missing({ name: provider.env_key }) : m.models_key_unnamed();
}

/**
 * 一个提供商的一行，和其余设置行（`SettingsCard` + `CardRow`）同一套尺寸：上一行名称与标签，下一行凭证状态（`KState`），
 * 操作在右、对着两行的中线；展开编辑时操作收起。行间细线。
 */
function Card({ open, title, desc, actions, children }: { open?: boolean; title: ReactNode; desc?: ReactNode; actions?: ReactNode; children?: ReactNode }) {
  return (
    <section className="border-b border-rule py-4 max-[600px]:py-3.5">
      <header className="flex items-center gap-6 max-[600px]:gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex min-h-5.5 flex-wrap items-center gap-x-2 gap-y-1 text-read leading-5.5">{title}</div>
          {desc && <p className="mt-1 text-label leading-5 text-muted">{desc}</p>}
        </div>
        {!open && actions && <span className="flex shrink-0 items-center gap-3">{actions}</span>}
      </header>
      {children}
    </section>
  );
}

/** 展开的编辑 / 添加表单：不再放在灰底的井里，直接落在这一行下面（输入框自己有细环）。 */
function Panel({ children }: { children: ReactNode }) {
  return <div className="pb-1">{children}</div>;
}

function DeleteAsk({ text, id, busy, failure, onSure, onCancel }: { text: string; id: string; busy: boolean; failure?: string; onSure: () => void; onCancel: () => void }) {
  return (
    <div className="mt-3 rounded-control p-3 text-label shadow-[0_0_0_1px_var(--color-negative-ring)]">
      <p>{text}</p>
      {failure && <p className="mt-2 text-negative">{failure}</p>}
      <div className="mt-2.5 flex flex-wrap gap-2">
        <Button variant="danger" disabled={busy} onClick={onSure}>
          {m.models_del_sure({ id })}
        </Button>
        <Button onClick={onCancel}>{m.cancel()}</Button>
      </div>
    </div>
  );
}
