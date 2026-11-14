// Komo 的 service worker：只做两件事——收推送显示通知，点通知时回到网页。
// 刻意不做离线缓存、不拦截 fetch：网页壳每次都向 Gateway 问最新的，不会被旧版本缓住。
// 负载（JSON）：{ title, body, url, tag? }，url 是站内路径（/chats/<id>、/）。

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("push", (event) => {
  let payload = {};
  try {
    payload = event.data ? event.data.json() : {};
  } catch {
    payload = { body: event.data ? event.data.text() : "" };
  }
  const title = payload.title || "Komo";
  const options = {
    body: payload.body || "",
    icon: "/icon-1024.png",
    data: { url: typeof payload.url === "string" && payload.url.startsWith("/") ? payload.url : "/" },
  };
  // 同一个 tag 的通知互相替换（例如同一段对话的多次提醒只留最新一条）
  if (payload.tag) options.tag = payload.tag;
  event.waitUntil(self.registration.showNotification(title, options));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const target = new URL((event.notification.data && event.notification.data.url) || "/", self.location.origin).href;
  event.waitUntil(
    (async () => {
      const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      for (const client of windows) {
        if (new URL(client.url).origin !== self.location.origin) continue;
        // 已开的窗口：聚焦后让网页自己用路由跳（不整页刷新）；聚焦失败就退回开新窗口
        try {
          await client.focus();
          client.postMessage({ type: "komo:navigate", url: new URL(target).pathname + new URL(target).search });
          return;
        } catch {
          break;
        }
      }
      await self.clients.openWindow(target);
    })(),
  );
});
