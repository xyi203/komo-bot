import type { PushKeyResponse, PushSubscribeRequest, PushSubscribeResponse, PushUnsubscribeRequest } from "@komo/protocol";
import { get, post, request } from "./http";

// 浏览器推送（网页侧）：订阅信息存在服务端，由服务端发推送。接口形状见 @komo/protocol 的 push.ts。
export const fetchPushKey = () => get<PushKeyResponse>("/v1/push/key");
export const subscribePush = (body: PushSubscribeRequest) => post<PushSubscribeResponse>("/v1/push/subscriptions", body);
export const unsubscribePush = (body: PushUnsubscribeRequest) => request<void>("DELETE", "/v1/push/subscriptions", { body });
