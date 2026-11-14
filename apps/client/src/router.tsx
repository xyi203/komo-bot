import { markSignedOut, setUnauthorizedHandler } from "@komo/api";
import { QueryClient } from "@tanstack/react-query";
import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  // 401 重试没有意义（要重新登录），其余失败也不自动重试：错误直接显示，重试由界面的按钮触发。
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false } },
  });
  // 任何 `/v1/**` 回 401（或 SSE 重连前发现登录失效）：守卫换成登录页。
  setUnauthorizedHandler(() => markSignedOut(queryClient));
  return createRouter({
    routeTree,
    context: { queryClient },
    scrollRestoration: true,
    defaultPreload: "intent",
  });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof getRouter>;
  }
}
