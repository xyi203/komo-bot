// 浏览器推送的接口形状（手写，不在 generated/ 里）：服务端稍后照这个实现，`docs/web.md` 的「手机与推送」一节有说明。

/** `GET /v1/push/key`：服务端的 VAPID 公钥（base64url，未压缩的 P-256 点）。 */
export interface PushKeyResponse {
  public_key: string;
}

/** `POST /v1/push/subscriptions` 的请求体：浏览器 `PushSubscription.toJSON()` 的 endpoint 与两把 key。 */
export interface PushSubscribeRequest {
  endpoint: string;
  keys: { p256dh: string; auth: string };
  user_agent?: string;
}

export interface PushSubscribeResponse {
  id: string;
}

/** `DELETE /v1/push/subscriptions` 的请求体；成功回 204。 */
export interface PushUnsubscribeRequest {
  endpoint: string;
}

/** service worker 收到的推送负载（JSON）。`url` 是站内路径（`/chats/<id>`、`/`），点通知时跳到它。 */
export interface PushPayload {
  title: string;
  body: string;
  url: string;
  tag?: string;
}
