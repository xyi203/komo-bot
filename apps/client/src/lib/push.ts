import { fetchPushKey, subscribePush, unsubscribePush } from "@komo/api";
import { m } from "@/paraglide/messages.js";
import { serviceWorkerUsable } from "./pwa";

/** 设置里「通知」一行的状态。 */
export type PushState =
  /** 这个浏览器 / 环境不支持（不是安全上下文、没有 PushManager、桌面壳）。 */
  | "unsupported"
  /** iOS 且没装到主屏幕：只有装到主屏幕的网页才能收推送。 */
  | "ios-install"
  /** 浏览器里已经拒绝了通知权限：只能去浏览器 / 系统设置里改。 */
  | "denied"
  | "off"
  | "on";

export function isIos(): boolean {
  // iPadOS 13+ 的 Safari 自称 Mac，靠触点数分辨
  return /iPad|iPhone|iPod/.test(navigator.userAgent) || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
}

export function isStandalone(): boolean {
  return window.matchMedia("(display-mode: standalone)").matches || (navigator as Navigator & { standalone?: boolean }).standalone === true;
}

const pushSupported = () => serviceWorkerUsable() && "PushManager" in window && "Notification" in window;

async function currentSubscription(): Promise<PushSubscription | null> {
  const registration = await navigator.serviceWorker.getRegistration("/");
  return registration ? registration.pushManager.getSubscription() : null;
}

export async function readPushState(): Promise<PushState> {
  if (typeof window === "undefined") return "unsupported";
  if (isIos() && !isStandalone() && !window.komoDesktop) return "ios-install";
  if (!pushSupported()) return "unsupported";
  if (Notification.permission === "denied") return "denied";
  const subscription = await currentSubscription();
  return subscription && Notification.permission === "granted" ? "on" : "off";
}

/** base64url → `applicationServerKey` 要的字节。 */
function keyBytes(base64url: string): Uint8Array<ArrayBuffer> {
  const padded = base64url.replace(/-/g, "+").replace(/_/g, "/").padEnd(Math.ceil(base64url.length / 4) * 4, "=");
  const raw = atob(padded);
  const bytes = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
  return bytes;
}

/** 开启：请求权限 → 取服务端公钥 → 订阅 → 把订阅交给服务端。返回新状态（用户在权限弹窗里拒绝时是 `denied`）。 */
export async function enablePush(): Promise<PushState> {
  const permission = await Notification.requestPermission();
  if (permission !== "granted") return permission === "denied" ? "denied" : "off";
  const registration = await navigator.serviceWorker.ready;
  const { public_key } = await fetchPushKey();
  const existing = await registration.pushManager.getSubscription();
  const subscription = existing ?? (await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: keyBytes(public_key) }));
  const json = subscription.toJSON();
  const p256dh = json.keys?.p256dh;
  const auth = json.keys?.auth;
  if (!json.endpoint || !p256dh || !auth) {
    await subscription.unsubscribe();
    throw new Error(m.push_incomplete());
  }
  try {
    await subscribePush({ endpoint: json.endpoint, keys: { p256dh, auth }, user_agent: navigator.userAgent });
  } catch (error) {
    // 服务端没记下就别留着浏览器这头的订阅，否则界面显示已开启却收不到
    if (!existing) await subscription.unsubscribe();
    throw error;
  }
  return "on";
}

/** 关闭：先告诉服务端，再退订；服务端那步失败就保持开启并抛错，让用户重试。 */
export async function disablePush(): Promise<PushState> {
  const subscription = await currentSubscription();
  if (subscription) {
    await unsubscribePush({ endpoint: subscription.endpoint });
    await subscription.unsubscribe();
  }
  return "off";
}
