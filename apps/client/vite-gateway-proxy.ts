// 开发时把 `/v1/*`、`/healthz` 与 `/auth/me`（网页启动时问登录状态，带令牌回 `credential: local`）转给本机 Gateway，并在服务端带上令牌（原型 `serve.mjs` 的做法，已随原型删除）：
// 令牌每次请求现读 `$KOMO_HOME/runtime/gateway.json`（Gateway 重启后会换），从不发给浏览器，
// 所以 `pnpm dev` 不需要登录。生产里由 Gateway 自己托管网页，走 cookie（docs/clients.md D6）。
import { readFile } from "node:fs/promises";
import http from "node:http";
import { homedir } from "node:os";
import { join } from "node:path";
import type { Plugin } from "vite";

const komoHome = process.env.KOMO_HOME || join(homedir(), ".komo");
const discoveryPath = join(komoHome, "runtime", "gateway.json");

export function komoGatewayProxy(): Plugin {
  return {
    name: "komo-gateway-proxy",
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        const url = req.url ?? "";
        if (!url.startsWith("/v1/") && url !== "/healthz" && url !== "/auth/me") return next();
        void forward(req, res);
      });
    },
  };
}

async function forward(req: http.IncomingMessage, res: http.ServerResponse) {
  let target: URL;
  let token: string;
  try {
    const found = JSON.parse(await readFile(discoveryPath, "utf8")) as { base_url: string; token?: string };
    target = new URL(req.url ?? "/", found.base_url);
    token = found.token ?? "";
  } catch (error) {
    res.writeHead(502, { "content-type": "application/json" });
    res.end(
      JSON.stringify({
        error: { code: "internal", message: `读不到 ${discoveryPath}：${(error as NodeJS.ErrnoException).code ?? error}（Gateway 没在跑？）` },
      }),
    );
    return;
  }
  // Host 换成 Gateway 自己的（它只认回环与监听地址）；本页的 cookie / Origin 不带过去，鉴权只靠令牌。
  const headers = { ...req.headers, host: target.host, authorization: `Bearer ${token}` };
  delete headers.origin;
  delete headers.referer;
  delete headers.cookie;
  const upstream = http.request(target, { method: req.method, headers }, (up) => {
    res.writeHead(up.statusCode ?? 502, up.headers);
    up.pipe(res);
  });
  upstream.on("error", (error) => {
    if (res.headersSent) return res.destroy();
    res.writeHead(502, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: { code: "internal", message: error.message } }));
  });
  res.on("close", () => upstream.destroy());
  req.pipe(upstream);
}
