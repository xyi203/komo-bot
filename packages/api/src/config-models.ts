// 设置「模型」一节的写：提供商（连同整份模型目录）、单个模型、向量模型、默认模型、取提供商的可用模型。
// 读与三份视图的缓存在 `config.ts`；这里每个写都返回 `ConfigEditResponse`，调用方用 `applyConfigEdit` 写回缓存。
import type {
  ConfigEditResponse,
  DefaultModelRequest,
  EmbeddingUpsertRequest,
  ModelUpsertRequest,
  ProviderModelsRequest,
  ProviderModelsResponse,
  ProviderUpsertRequest,
} from "@komo/protocol";
import { post, request } from "./http";

const enc = encodeURIComponent;

type WithoutVersion<T extends { version: string }> = Omit<T, "version">;

export const putProvider = (id: string, version: string, body: WithoutVersion<ProviderUpsertRequest>) =>
  request<ConfigEditResponse>("PUT", `/v1/config/model-providers/${enc(id)}`, { body: { version, ...body } satisfies ProviderUpsertRequest });
export const deleteProvider = (id: string, version: string) =>
  request<ConfigEditResponse>("DELETE", `/v1/config/model-providers/${enc(id)}`, { body: { version } });

export const putModel = (alias: string, version: string, body: WithoutVersion<ModelUpsertRequest>) =>
  request<ConfigEditResponse>("PUT", `/v1/config/models/${enc(alias)}`, { body: { version, ...body } satisfies ModelUpsertRequest });
export const deleteModel = (alias: string, version: string) =>
  request<ConfigEditResponse>("DELETE", `/v1/config/models/${enc(alias)}`, { body: { version } });

export const putEmbeddingModel = (alias: string, version: string, body: WithoutVersion<EmbeddingUpsertRequest>) =>
  request<ConfigEditResponse>("PUT", `/v1/config/embedding-models/${enc(alias)}`, {
    body: { version, ...body } satisfies EmbeddingUpsertRequest,
  });
export const deleteEmbeddingModel = (alias: string, version: string) =>
  request<ConfigEditResponse>("DELETE", `/v1/config/embedding-models/${enc(alias)}`, { body: { version } });

export const putDefaultModel = (version: string, alias: string) =>
  request<ConfigEditResponse>("PUT", "/v1/config/default-model", { body: { version, alias } satisfies DefaultModelRequest });

/** 向提供商取可用模型 id（会访问它的 `/models`）；用表单里还没保存的地址、协议与密钥。 */
export const fetchProviderModels = (body: ProviderModelsRequest) => post<ProviderModelsResponse>("/v1/config/provider-models", body);

/** 提供商写入体（`version` 由调用方带）。 */
export type ProviderRequestBody = WithoutVersion<ProviderUpsertRequest>;
