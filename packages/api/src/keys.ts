/** Query key 按资源组织：同一个资源的 key 共用前缀，失效时按前缀一次清掉。 */
export const keys = {
  health: ["health"] as const,
  sessions: {
    all: ["sessions"] as const,
    list: (all = false) => ["sessions", "list", { all }] as const,
    detail: (id: string) => ["sessions", "detail", id] as const,
  },
};
