import { paraglideVitePlugin } from "@inlang/paraglide-js";
import tailwindcss from "@tailwindcss/vite";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import viteReact from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import { komoGatewayProxy } from "./vite-gateway-proxy.ts";

export default defineConfig({
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  // 构建时 TanStack 预渲染会起 `vite preview`（host 默认 localhost）再用 fetch 连它；
  // localhost 解析成 ::1 和 127.0.0.1 两个地址时两边可能各挑一个，导致 ECONNREFUSED。钉死 IPv4 回环。
  preview: { host: "127.0.0.1" },
  plugins: [
    komoGatewayProxy(),
    paraglideVitePlugin({
      project: "./project.inlang",
      outdir: "./src/paraglide",
      // 偏好存 localStorage；没选过跟系统语言（`custom-system`，见 src/lib/i18n.ts），再退到基准语言。
      strategy: ["localStorage", "custom-system", "baseLocale"],
    }),
    // SPA 模式：构建时只预渲染一个壳（`_shell.html`），产物全是静态文件，没有 Node 服务端。
    tanstackStart({ spa: { enabled: true } }),
    tailwindcss(),
    viteReact(),
  ],
});
