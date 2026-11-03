import { keys } from "@komo/api";
import type { MemoryItem, MemoryListResponse } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";

/**
 * 写操作回来的那一条记忆：先写进清单缓存（列表与详情立刻是新的），再让所有记忆查询（检索、右栏、⌘K）重取。
 */
export function useCommitMemory() {
  const queryClient = useQueryClient();
  return (item: MemoryItem) => {
    queryClient.setQueryData<MemoryListResponse>(keys.memoriesAll, (previous) => {
      if (!previous) return previous;
      const has = previous.memories.some((x) => x.id === item.id);
      return { ...previous, memories: has ? previous.memories.map((x) => (x.id === item.id ? item : x)) : [item, ...previous.memories] };
    });
    void queryClient.invalidateQueries({ queryKey: ["memories"] });
  };
}
