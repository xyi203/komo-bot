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
