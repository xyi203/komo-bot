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
      { name: "viewport", content: "width=device-width, initial-scale=1" },
      { title: "komo" },
    ],
    links: [{ rel: "stylesheet", href: appCss }],
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
