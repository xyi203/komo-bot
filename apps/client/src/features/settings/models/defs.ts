import type { ConfigModelsResponse, ProviderEntry } from "@komo/protocol";
import { m } from "@/paraglide/messages.js";

// 「模型」一节的静态知识：预设提供商、协议、类型判断（docs/web.md §15）。
// provider 就是一组。类型（对话 / 嵌入）由协议看出来：嵌入组的协议是向量那两种，组里挂的都是向量模型。

export type Kind = "completion" | "embedding";

export const EMBED_BACKENDS = ["ollama_embeddings", "embeddings"] as const;
export const COMPLETION_BACKENDS = ["responses", "chat_completions", "anthropic_messages"] as const;

export const isEmbedBackend = (backend: string | undefined): boolean => !!backend && (EMBED_BACKENDS as readonly string[]).includes(backend);
export const kindOf = (provider: { api_backend?: string } | undefined): Kind => (isEmbedBackend(provider?.api_backend) ? "embedding" : "completion");

/** 协议的显示名。前三个是协议本身的名字，不翻；两个向量协议带了"嵌入"二字，要翻。 */
export function backendName(backend: string | undefined): string {
  switch (backend) {
    case "responses":
      return "OpenAI Responses";
    case "chat_completions":
      return "OpenAI Chat Completions";
    case "anthropic_messages":
      return "Anthropic Messages";
    case "embeddings":
      return m.models_backend_embeddings();
    case "ollama_embeddings":
      return m.models_backend_ollama_embeddings();
    default:
      return backend ?? m.models_backend_unset();
  }
}

export interface Preset {
  key: string;
  label: string;
  id: string;
  base_url: string;
  api_backend: string;
  env_key: string;
}

// 品牌名与地址都是机器的名字，不翻。
export const PRESETS: Preset[] = [
  { key: "openai", label: "OpenAI", id: "openai", base_url: "https://api.openai.com/v1", api_backend: "responses", env_key: "OPENAI_API_KEY" },
  { key: "anthropic", label: "Anthropic", id: "anthropic", base_url: "https://api.anthropic.com/v1", api_backend: "anthropic_messages", env_key: "ANTHROPIC_API_KEY" },
  { key: "openrouter", label: "OpenRouter", id: "openrouter", base_url: "https://openrouter.ai/api/v1", api_backend: "chat_completions", env_key: "OPENROUTER_API_KEY" },
  { key: "gemini", label: "Google Gemini", id: "gemini", base_url: "https://generativelanguage.googleapis.com/v1beta/openai", api_backend: "chat_completions", env_key: "GEMINI_API_KEY" },
];

export const isCustomProvider = (baseUrl: string | undefined) => !PRESETS.some((preset) => preset.base_url === baseUrl);

/** 模型 id → alias：把不合格的字符换成 `-`。 */
export const aliasOf = (model: string) =>
  model
    .trim()
    .replace(/[^A-Za-z0-9._-]+/g, "-")
    .replace(/^-+|-+$/g, "");

/** 目录里一行的草稿：数字与列表都先当字符串，保存时再解析。 */
export interface Row {
  alias?: string;
  model: string;
  name: string;
  dimensions?: string;
  context_window?: string;
  effort?: string;
  efforts?: string;
  open?: boolean;
}

export interface Draft {
  tab: "preset" | "custom";
  preset: string | null;
  v: { pid: string; name: string; base_url: string; api_backend: string; api_key: string; env_key: string; timeout_secs: string };
  rows: Row[];
  fetched: { models: string[] } | { error: string } | null;
  filter: string;
  advOpen?: boolean;
  fetching: boolean;
}

/** 打开表单时把 provider 与它挂着的同类模型抄进草稿。 */
export function providerDraft(config: ConfigModelsResponse, provider: ProviderEntry | null): Draft {
  const kind = kindOf(provider ?? undefined);
  const rows: Row[] = !provider
    ? [{ model: "", name: "" }]
    : kind === "embedding"
      ? (config.embeddings ?? [])
          .filter((e) => e.provider === provider.id)
          .map((e) => ({ alias: e.alias, model: e.model, name: e.name ?? "", dimensions: e.dimensions != null ? String(e.dimensions) : "" }))
      : config.models
          .filter((model) => model.provider === provider.id)
          .map((model) => ({
            alias: model.alias,
            model: model.model,
            name: model.name ?? "",
            context_window: model.context_window != null ? String(model.context_window) : "",
            effort: model.effort ?? "",
            efforts: (model.efforts ?? []).join(", "),
          }));
  return {
    tab: "preset",
    preset: null,
    rows,
    fetched: null,
    filter: "",
    fetching: false,
    v: {
      pid: provider?.id ?? "",
      name: provider?.name ?? "",
      base_url: provider?.base_url ?? "",
      api_backend: provider?.api_backend ?? "chat_completions",
      api_key: "",
      env_key: provider?.env_key ?? "",
      timeout_secs: provider?.timeout_secs != null ? String(provider.timeout_secs) : "",
    },
  };
}

export const num = (text: string | undefined): number | undefined => (String(text ?? "").trim() ? Number(text) : undefined);
export const list = (text: string | undefined): string[] | undefined => {
  const items = (text ?? "").split(/[,，\s]+/).filter(Boolean);
  return items.length ? items : undefined;
};
