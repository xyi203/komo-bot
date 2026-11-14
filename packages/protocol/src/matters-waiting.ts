// 目标列表接口带的「在等你」（手写；形状待与服务端核对）：另一个子代理在服务端加，
// 暂定为 `GET /v1/matters` 响应里多一个 `waiting`，键是目标 id，值是问话所在的对话与介入句柄。
// 没有这个字段（旧 Gateway、或服务端形状不同）时网页回落到从 `GET /v1/chats` 的行上反查。
import type { MatterListResponse } from "./generated/index";

/** 目标在等你的那一句：所在的对话（会话 id）与介入句柄。 */
export interface MatterWaiting {
  chat: string;
  handle: string;
}

/** `GET /v1/matters` 的响应加上暂定的 `waiting`。 */
export type MatterListWithWaiting = MatterListResponse & {
  waiting?: Record<string, MatterWaiting>;
};
