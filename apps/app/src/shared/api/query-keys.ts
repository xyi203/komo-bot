// Every react-query key in one place, so an invalidation can't miss a cache by
// spelling its key differently.

export const qk = {
  connection: ["connection"] as const,
  sessions: ["sessions"] as const,
  sessionHistory: (session: string) => ["session-history", session] as const,
  status: ["status"] as const,
  workspaces: ["workspaces"] as const,
  models: ["models"] as const,
  memories: (status: string) => ["memories", status] as const,
  dream: ["dream"] as const,
  runs: (limit: number) => ["runs", limit] as const,
  run: (id: string) => ["run", id] as const,
};
