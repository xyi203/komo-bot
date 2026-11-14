import type { QueryClient } from "@tanstack/react-query";
import { QueryClientProvider } from "@tanstack/react-query";
import { HeadContent, Outlet, Scripts, createRootRouteWithContext } from "@tanstack/react-router";
import type { ReactNode } from "react";
import { LocaleProvider } from "@/lib/i18n";
import appCss from "@/styles.css?url";

export const Route = createRootRouteWithContext<{ queryClient: QueryClient }>()({
  head: () => ({
    meta: [
      { charSet: "utf-8" },
      // viewport-fit=cover：env(safe-area-inset-*) 才有值（iPhone 的主屏幕横条）；
      // interactive-widget=resizes-content：Android Chrome 的软键盘缩小布局视口，输入框不被遮（iOS 靠 app-shell 的 visualViewport 定高）
      { name: "viewport", content: "width=device-width, initial-scale=1, viewport-fit=cover, interactive-widget=resizes-content" },
      { name: "theme-color", content: "#f4f4f5" },
      // 添加到 iOS 主屏幕后以独立窗口打开；状态栏用 default（内容在状态栏下面，不用处理顶部安全区）
      { name: "apple-mobile-web-app-capable", content: "yes" },
      { name: "mobile-web-app-capable", content: "yes" },
      { name: "apple-mobile-web-app-status-bar-style", content: "default" },
      { name: "apple-mobile-web-app-title", content: "Komo" },
      { title: "komo" },
    ],
    links: [
      { rel: "stylesheet", href: appCss },
      { rel: "manifest", href: "/manifest.webmanifest" },
      { rel: "icon", href: "/icon.svg", type: "image/svg+xml" },
      // iOS 只认 PNG 的 apple-touch-icon；这是临时用的桌面图标（带透明边），换正式图标见 docs/web.md
      { rel: "apple-touch-icon", href: "/icon-1024.png" },
    ],
  }),
  shellComponent: RootDocument,
  component: RootComponent,
});

// macOS 桌面壳去掉了系统标题栏（apps/desktop 的 window.ts）：在 <html> 上标出平台，styles.css 据此留出红绿灯的位置、
// 把顶上一条当标题栏拖窗口。预渲染时没有 window，网页里没有 komoDesktop，都不标。
if (typeof window !== "undefined" && window.komoDesktop?.platform) document.documentElement.dataset.desktop = window.komoDesktop.platform;

// 壳（构建时预渲染的 `_shell.html`）里没有任何带语言的文字：语言是浏览器里的事。
function RootDocument({ children }: { children: ReactNode }) {
  return (
    <html suppressHydrationWarning>
      <head>
        <HeadContent />
      </head>
      <body>
        {children}
        <Scripts />
      </body>
    </html>
  );
}

function RootComponent() {
  const { queryClient } = Route.useRouteContext();
  return (
    <QueryClientProvider client={queryClient}>
      <LocaleProvider>
        <Outlet />
      </LocaleProvider>
    </QueryClientProvider>
  );
}
