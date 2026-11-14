import type {
  MemoryAddRequest,
  MemoryAddResponse,
  MemoryClassifyRequest,
  MemoryDetail,
  MemoryIndexStatus,
  MemoryItem,
  MemoryListResponse,
  MemoryRevisionRequest,
} from "@komo/protocol";
import { useQuery } from "@tanstack/react-query";
import { useMemo } from "react";
import { get, post } from "./http";
import { keys } from "./keys";
import { uuid } from "./uuid";

export type { MemoryScope } from "@komo/protocol";
/** 对话、右栏与 ⌘K 里的一条记忆：就是线上的 `MemoryItem`。 */
export type MemoryBrief = MemoryItem;
export type MemoryListBrief = MemoryListResponse;

const enc = encodeURIComponent;

// 读
/** 默认清单（不带归档的）：对话与右栏要记忆正文。 */
export const fetchMemoryBriefs = () => get<MemoryListResponse>("/v1/memories");
/** 记忆页的清单：带归档的，最多 500 条。 */
export const fetchMemoriesAll = () => get<MemoryListResponse>("/v1/memories", { include_archived: true, limit: 500 });
/** 记忆检索（⌘K）：混合检索，没开记忆时 Gateway 回 `config_invalid`。 */
export const searchMemories = (query: string, limit: number, signal?: AbortSignal) =>
  get<MemoryListResponse>("/v1/memories", { query, limit }, signal);
/** 记忆页的检索：带归档的，结果保持检索的排序，`degraded` 如实给出。 */
export const findMemories = (query: string, signal?: AbortSignal) =>
  get<MemoryListResponse>("/v1/memories", { query, include_archived: true, limit: 30 }, signal);
/** 等你过目的：候选状态的记忆（`state=candidate`），最多 100 条。 */
export const fetchPendingMemories = () => get<MemoryListResponse>("/v1/memories", { state: "candidate", limit: 100 });
export const fetchMemoryCoverage = () => get<MemoryIndexStatus>("/v1/memory-index");

// 写：都带预期 revision 与请求键 `web:{动作}:{id}:{revision}`；回的是写完的那一条。
export type MemoryVerb = "confirm" | "forget" | "restore";
export const memoryVerb = (verb: MemoryVerb, memory: MemoryItem) =>
  post<MemoryDetail>(`/v1/memories/${enc(memory.id)}/${verb}`, {
    expected_revision: memory.revision,
    request_key: `web:${verb}:${memory.id}:${memory.revision}`,
  } satisfies MemoryRevisionRequest);
export const classifyMemory = (memory: MemoryItem, body: Pick<MemoryClassifyRequest, "application" | "applies_when">) =>
  post<MemoryDetail>(`/v1/memories/${enc(memory.id)}/classify`, {
    ...body,
    expected_revision: memory.revision,
    request_key: `web:classify:${memory.id}:${memory.revision}:${body.application}`,
  } satisfies MemoryClassifyRequest);
export const addMemory = (body: Omit<MemoryAddRequest, "request_key">) =>
  post<MemoryAddResponse>("/v1/memories", { ...body, request_key: `web:add:${uuid()}` } satisfies MemoryAddRequest);

// hooks
/** 记忆 id → 内容与用法：对话里"带上了哪些记忆"要显示正文。取不到就当没有（对话照常）。 */
export function useMemoryIndex(): Map<string, MemoryBrief> {
  const query = useQuery({ queryKey: keys.memories, queryFn: fetchMemoryBriefs, retry: false });
  return useMemo(() => new Map((query.data?.memories ?? []).map((memory) => [memory.id, memory])), [query.data]);
}

/** 记忆检索（⌘K）：关键词非空才查，结果按关键词缓存。 */
export function useMemorySearch(query: string, limit: number, enabled: boolean) {
  return useQuery({
    queryKey: keys.memorySearch(query, limit),
    queryFn: ({ signal }) => searchMemories(query, limit, signal),
    select: (data) => data.memories,
    enabled: enabled && query !== "",
    retry: false,
    staleTime: 30_000,
  });
}

/** 记忆页的全部清单（带归档的）。 */
export function useMemoriesAll() {
  return useQuery({ queryKey: keys.memoriesAll, queryFn: fetchMemoriesAll, retry: false });
}

/** 记忆页的检索：关键词非空才查。 */
export function useMemoryFind(query: string) {
  return useQuery({
    queryKey: keys.memoryFind(query),
    queryFn: ({ signal }) => findMemories(query, signal),
    enabled: query !== "",
    retry: false,
    staleTime: 15_000,
  });
}

/** 向量索引状态（取不到不挡页面）。 */
export function useMemoryCoverage() {
  return useQuery({ queryKey: keys.memoryIndexStatus, queryFn: fetchMemoryCoverage, retry: false });
}

/**
 * 等你过目的记忆：候选且你还没确认（确认过的候选不算：服务端的 `confirm` 只改确认状态，见 `docs/memory.md` 附录 C.3.1）。
 * 图标栏记忆图标上的点用它；一分钟取一次，键在 `["memories"]` 前缀下，任何记忆写操作之后随其余记忆查询一起重取。
 */
export function usePendingMemories() {
  return useQuery({
    queryKey: keys.memoriesPending,
    queryFn: fetchPendingMemories,
    select: (data) => data.memories.filter((memory) => memory.confirmation !== "user_confirmed"),
    retry: false,
    staleTime: 30_000,
    refetchInterval: 60_000,
  });
}
