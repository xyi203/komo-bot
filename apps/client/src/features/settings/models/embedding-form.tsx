import type { EmbeddingModelEntry } from "@komo/protocol";
import { useId, useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages.js";
import { Field, FormError, Hint, NativeSelect, TextInput } from "../controls";
import { backendName, EMBED_BACKENDS, isEmbedBackend, num } from "./defs";

export interface EmbeddingSubmit {
  was: string;
  alias: string;
  rename?: string;
  body: {
    model: string;
    api_backend?: string;
    base_url?: string;
    env_key?: string;
    dimensions?: number;
    api_key?: string;
  };
  /** 换了向量空间（模型、协议、维数、端点）。 */
  changed: boolean;
}

/**
 * 没挂提供商的向量模型的表单：端点、key 写在模型项里，名称就是它的 alias（能改）。界面不再新建这种，只编辑已有的；
 * 挂在 provider 下的向量模型跟着 provider 的模型目录一起改。
 */
export function EmbeddingForm({
  entry,
  busy,
  failure,
  onSubmit,
  onCancel,
}: {
  entry: EmbeddingModelEntry;
  busy: boolean;
  failure?: string;
  onSubmit: (submit: EmbeddingSubmit) => void;
  onCancel: () => void;
}) {
  const uid = useId();
  const [alias, setAlias] = useState(entry.alias);
  const [model, setModel] = useState(entry.model);
  const [backend, setBackend] = useState(isEmbedBackend(entry.api_backend) ? (entry.api_backend as string) : "ollama_embeddings");
  const [dimensions, setDimensions] = useState(entry.dimensions != null ? String(entry.dimensions) : "");
  const [baseUrl, setBaseUrl] = useState(entry.base_url ?? "");
  const [envKey, setEnvKey] = useState(entry.env_key ?? "");
  const [apiKey, setApiKey] = useState("");

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const next = alias.trim();
    const rename = next && next !== entry.alias ? next : undefined;
    const body = {
      model: model.trim(),
      api_backend: backend || undefined,
      base_url: baseUrl.trim() || undefined,
      env_key: envKey.trim() || undefined,
      dimensions: num(dimensions),
      api_key: apiKey.trim() || undefined,
    };
    // 只改名不换空间（指纹不含 alias），不用提示重启重建
    const changed =
      body.model !== entry.model ||
      (!!body.api_backend && body.api_backend !== entry.api_backend) ||
      (body.dimensions ?? null) !== (entry.dimensions ?? null) ||
      baseUrl.trim() !== (entry.base_url ?? "");
    onSubmit({ was: entry.alias, alias: entry.alias, rename, body, changed });
  };

  return (
    <form onSubmit={submit} className="pt-1">
      <div className="grid grid-cols-2 gap-3 max-[600px]:grid-cols-1">
        <Field label={m.models_row_alias()} htmlFor={`${uid}-alias`} className="mt-3.5">
          <TextInput id={`${uid}-alias`} required pattern="[A-Za-z0-9._\-]+" maxLength={64} value={alias} onChange={(e) => setAlias(e.target.value)} placeholder={m.models_embed_alias_ph()} />
        </Field>
        <Field label={m.models_embed_model()} htmlFor={`${uid}-model`} className="mt-3.5">
          <TextInput id={`${uid}-model`} mono required value={model} onChange={(e) => setModel(e.target.value)} placeholder="qwen3-embedding:0.6b" />
        </Field>
      </div>
      <div className="mt-3 grid grid-cols-2 gap-3 max-[600px]:grid-cols-1">
        <Field label={m.models_embed_backend()} htmlFor={`${uid}-backend`}>
          <NativeSelect id={`${uid}-backend`} value={backend} onChange={(e) => setBackend(e.target.value)} className="min-w-0">
            {EMBED_BACKENDS.map((value) => (
              <option key={value} value={value}>
                {backendName(value)}
              </option>
            ))}
          </NativeSelect>
        </Field>
        <Field label={m.models_dimensions()} htmlFor={`${uid}-dim`}>
          <TextInput id={`${uid}-dim`} type="number" min={1} value={dimensions} onChange={(e) => setDimensions(e.target.value)} placeholder={m.models_embed_dimensions_ph()} />
        </Field>
      </div>
      <div className="mt-3 grid grid-cols-2 gap-3 max-[600px]:grid-cols-1">
        <Field label={m.models_base_url()} htmlFor={`${uid}-url`}>
          <TextInput id={`${uid}-url`} value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} placeholder="http://127.0.0.1:11434" />
        </Field>
        <Field label={m.models_api_key()} htmlFor={`${uid}-key`}>
          <TextInput
            id={`${uid}-key`}
            type="password"
            autoComplete="new-password"
            value={apiKey}
            onChange={(e) => setApiKey(e.target.value)}
            placeholder={entry.key_set ? m.models_embed_key_set() : m.models_embed_key_unset()}
          />
        </Field>
      </div>
      <div className="mt-3 grid grid-cols-2 gap-3 max-[600px]:grid-cols-1">
        <Field label={m.models_env_key()} htmlFor={`${uid}-env`}>
          <TextInput id={`${uid}-env`} mono value={envKey} onChange={(e) => setEnvKey(e.target.value)} placeholder={m.models_embed_env_key_ph()} />
        </Field>
      </div>
      <Hint>{m.models_embed_key_hint()}</Hint>
      {failure && <FormError>{failure}</FormError>}
      <div className="mt-4 flex flex-wrap gap-2">
        <Button type="submit" variant="primary" disabled={busy}>
          {busy ? m.models_saving() : m.models_save()}
        </Button>
        <Button onClick={onCancel}>{m.cancel()}</Button>
      </div>
    </form>
  );
}
