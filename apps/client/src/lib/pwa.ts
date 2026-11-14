import { useRouter } from "@tanstack/react-router";
import { useEffect } from "react";

/**
 * 能不能用 service worker：浏览器支持、安全上下文（https 或 localhost），并且是普通网页。
 * 桌面壳（`app://komo`）里不注册：那个协议没有开 service worker，桌面端的通知由壳自己管。
 */
export function serviceWorkerUsable(): boolean {
  return (
    typeof window !== "undefined" &&
    "serviceWorker" in navigator &&
    window.isSecureContext &&
    !window.komoDesktop &&
    (location.protocol === "https:" || location.protocol === "http:")
  );
}

/**
 * 应用启动时注册 `/sw.js`（只收推送与点通知，没有离线缓存，见 public/sw.js），并接 service worker 发来的
 * 「跳到某个站内路径」：点通知时已开着的窗口由它聚焦后让这里用路由跳，不整页刷新。
 * 注册失败只记一条控制台日志：没有 service worker 只是收不到推送，网页照常用。
 */
export function useServiceWorker() {
  const router = useRouter();
  useEffect(() => {
    if (!serviceWorkerUsable()) return;
    const onMessage = (event: MessageEvent) => {
      const data = event.data as { type?: string; url?: unknown } | null;
      if (data?.type === "komo:navigate" && typeof data.url === "string" && data.url.startsWith("/")) router.history.push(data.url);
    };
    navigator.serviceWorker.addEventListener("message", onMessage);
    navigator.serviceWorker.register("/sw.js", { scope: "/" }).catch((error: unknown) => console.warn("service worker 注册失败", error));
    return () => navigator.serviceWorker.removeEventListener("message", onMessage);
  }, [router]);
}
