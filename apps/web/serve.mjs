// 原型的本地服务：同源提供静态文件，把 /v1/* 与 /healthz 转给 Gateway，并在服务端带上令牌。
// 令牌每次请求现读 runtime/gateway.json（Gateway 重启后会换），从不发给浏览器。
// 用法：node apps/web/serve.mjs [--port 5178]，KOMO_HOME 默认 ~/.komo。
import http from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";
import { homedir } from "node:os";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL(".", import.meta.url));
const komoHome = process.env.KOMO_HOME || join(homedir(), ".komo");
const port = Number(process.argv[process.argv.indexOf("--port") + 1]) || 5178;
const types = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".png": "image/png", ".svg": "image/svg+xml" };

async function discovery() {
  return JSON.parse(await readFile(join(komoHome, "runtime", "gateway.json"), "utf8"));
}

function proxy(req, res) {
  discovery()
    .then(({ base_url, token }) => {
      const target = new URL(req.url, base_url);
      const headers = { ...req.headers, host: target.host, authorization: `Bearer ${token}` };
      const upstream = http.request(target, { method: req.method, headers }, (up) => {
        res.writeHead(up.statusCode, up.headers);
        up.pipe(res);
      });
      upstream.on("error", (err) => {
        if (res.headersSent) return res.destroy();
        res.writeHead(502, { "content-type": "application/json" });
        res.end(JSON.stringify({ error: { code: "gateway_unreachable", message: err.message } }));
      });
      res.on("close", () => upstream.destroy());
      req.pipe(upstream);
    })
    .catch((err) => {
      res.writeHead(502, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { code: "gateway_not_running", message: `读不到 ${join(komoHome, "runtime", "gateway.json")}：${err.code || err.message}` } }));
    });
}

async function serveStatic(req, res) {
  const path = normalize(decodeURIComponent(new URL(req.url, "http://x").pathname)).replace(/^\/+/, "");
  const file = join(root, path === "." || !path ? "index.html" : path);
  if (!file.startsWith(root) || path.split("/").some((seg) => seg.startsWith(".") && seg !== ".")) {
    res.writeHead(404).end();
    return;
  }
  try {
    const body = await readFile(file);
    res.writeHead(200, { "content-type": types[extname(file)] || "application/octet-stream", "cache-control": "no-store" });
    res.end(body);
  } catch {
    res.writeHead(404).end();
  }
}

http
  .createServer((req, res) => {
    if (req.url.startsWith("/v1/") || req.url === "/healthz") proxy(req, res);
    else serveStatic(req, res);
  })
  .listen(port, "127.0.0.1", () => console.log(`komo web 原型：http://127.0.0.1:${port}`));
