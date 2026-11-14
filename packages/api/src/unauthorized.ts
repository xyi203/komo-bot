// 401 的出口：`request`（任何 `/v1/**` 回 401）与 SSE（重连前核对）都在这里通知，
// 由应用启动时注册的处理函数让登录状态失效。独立成一个文件，免得 http.ts 与 auth.ts 互相引用。
type Handler = () => void;

let handler: Handler | undefined;

/** 应用启动时注册一次；之后任何地方发现登录失效就会调到它。 */
export function setUnauthorizedHandler(next: Handler | undefined): void {
  handler = next;
}

export function notifyUnauthorized(): void {
  handler?.();
}
