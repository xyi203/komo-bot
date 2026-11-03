// 协议里还没有生成类型的几个线上形状。能从 kernel 的 `ts` feature 生成的都在 `@komo/protocol`；
// 这里只放没法生成的（来自 pi 树投影的 JSON），用到哪写到哪。

/** `GET /v1/sessions/{id}/tree` 里的一条 entry（pi v3 树的投影，字段按用到的写）。 */
export interface TreeEntry {
  id: string;
  parentId: string | null;
  timestamp: string;
  type: string;
  customType?: string;
  /** `komo.system` 等自定义条目的负载（压缩条目的 `summary` / `head` 也在这里，可能多包一层 `data`）。 */
  data?: unknown;
  /** `type: "message"` 的消息本体（pi 的 message：role + content）。 */
  message?: TreeMessage;
  /** `type: "context_edit"`：被替换模型视图的目标条目与替换后的内容。 */
  targetId?: string;
  replacement?: Partial<TreeMessage>;
}

/** 树上一条消息：`content` 是字符串或块数组（文本、toolCall…）。 */
export interface TreeMessage {
  role: string;
  content: string | { type: string; text?: string; name?: string; arguments?: unknown }[];
}

export interface SessionTree {
  entries: TreeEntry[];
  leafId: string | null;
}
