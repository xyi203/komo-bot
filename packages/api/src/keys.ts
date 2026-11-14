/**
 * Query key 按资源组织：同一个资源的 key 共用前缀，失效时按前缀一次清掉。
 *
 * 会话列表（`sessions`）与单个会话的资源（`session(id)`）分在两个前缀下：SSE 帧一到就让列表失效，
 * 不能把整段事件历史也一起重取。
 */
export const keys = {
  health: ["health"] as const,
  home: ["home-session"] as const,
  chats: ["chats"] as const,
  models: ["models"] as const,
  memories: ["memories", "list"] as const,
  memoriesAll: ["memories", "list", "all"] as const,
  memoryIndexStatus: ["memories", "index-status"] as const,
  interventions: ["interventions"] as const,
  matters: ["matters"] as const,
  mattersAll: ["matters", "all"] as const,
  monitors: ["monitors"] as const,
  monitorsAll: ["monitors", "all"] as const,
  sessionSearch: (query: string, limit: number) => ["session-search", query, limit] as const,
  memoryFind: (query: string) => ["memories", "find", query] as const,
  memorySearch: (query: string, limit: number) => ["memories", "search", query, limit] as const,
  sessions: {
    all: ["sessions"] as const,
    list: (all = false) => ["sessions", "list", { all }] as const,
    detail: (id: string) => ["sessions", "detail", id] as const,
  },
  session: (id: string) => ({
    events: ["session", id, "events"] as const,
    tree: ["session", id, "tree"] as const,
    changes: ["session", id, "changes"] as const,
  }),
  runs: {
    diff: (run: string, index: number) => ["runs", run, "diff", index] as const,
  },
};
