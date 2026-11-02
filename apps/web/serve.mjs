// 原型的本地服务：同源提供静态文件，把 /v1/* 与 /healthz 转给 Gateway，并在服务端带上令牌。
// 默认连本机：令牌每次请求现读 runtime/gateway.json（Gateway 重启后会换）。设置里可以改连另一台
// （/__web/connection），地址与令牌只记在这个进程的内存里，重启就回到本机。令牌从不发给浏览器。
// 用法：node apps/web/serve.mjs [--port 5178]，KOMO_HOME 默认 ~/.komo。
import http from "node:http";
import https from "node:https";
import { readFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";
import { homedir } from "node:os";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL(".", import.meta.url));
const komoHome = process.env.KOMO_HOME || join(homedir(), ".komo");
const port = Number(process.argv[process.argv.indexOf("--port") + 1]) || 5178;
const types = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".png": "image/png", ".svg": "image/svg+xml" };

let remote = null; // { url, token }：设置里改连的那台；null = 本机

async function discovery() {
  return JSON.parse(await readFile(join(komoHome, "runtime", "gateway.json"), "utf8"));
}
const upstreamOf = async () => (remote ? { base_url: remote.url, token: remote.token } : discovery());
const client = (url) => (url.protocol === "https:" ? https : http);

function proxy(req, res) {
  upstreamOf()
    .then(({ base_url, token }) => {
      const target = new URL(req.url, base_url);
      const headers = { ...req.headers, host: target.host, authorization: `Bearer ${token}` };
      delete headers.origin; delete headers.referer; delete headers.cookie; // 本机页面的东西不带去远端
      const upstream = client(target).request(target, { method: req.method, headers }, (up) => {
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

// 试连：/healthz 看版本，再用令牌读一个要鉴权的接口，两样都过才算连得上
function probe(base, token) {
  const get = (path, auth) => new Promise((resolve, reject) => {
    const url = new URL(path, base);
    const r = client(url).get(url, { headers: auth ? { authorization: `Bearer ${token}` } : {}, timeout: 8000 }, (up) => {
      let body = "";
      up.on("data", (c) => (body += c)).on("end", () => resolve({ status: up.statusCode, body }));
    });
    r.on("timeout", () => r.destroy(new Error("8 秒没回应"))).on("error", reject);
  });
  const t0 = Date.now();
  return get("/healthz", false).then((h) => {
    if (h.status !== 200) throw new Error(`/healthz 回了 ${h.status}，这个地址不像 komo Gateway`);
    const health = JSON.parse(h.body);
    return get("/v1/interventions", true).then((a) => {
      if (a.status === 401 || a.status === 403) throw new Error("令牌不对：Gateway 拒绝了");
      if (a.status !== 200) throw new Error(`鉴权接口回了 ${a.status}`);
      return { version: health.version, protocol_version: health.protocol_version, ms: Date.now() - t0 };
    });
  });
}

const json = (res, status, body) => { res.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" }); res.end(JSON.stringify(body)); };
const readBody = (req) => new Promise((resolve) => { let b = ""; req.on("data", (c) => (b += c)).on("end", () => resolve(b)); });

async function connection(req, res) {
  // 只认本机地址打开的页面：挡 DNS 重绑定（Host）与别的网站跨站改连接（Origin）
  const host = req.headers.host || "";
  if (!/^(127\.0\.0\.1|localhost)(:\d+)?$/.test(host)) return json(res, 403, { error: { message: "只接受本机地址" } });
  if (req.method !== "GET" && req.headers.origin !== `http://${host}`) return json(res, 403, { error: { message: "跨站请求" } });
  if (req.method === "GET") {
    const local = await discovery().catch(() => null);
    return json(res, 200, remote ? { mode: "remote", url: remote.url } : { mode: "local", url: local?.base_url ?? null });
  }
  if (req.method === "DELETE") { remote = null; return json(res, 200, { mode: "local" }); }
  if (req.method !== "PUT") return json(res, 405, { error: { message: "不支持" } });
  let url, token;
  try {
    const b = JSON.parse(await readBody(req));
    url = new URL(String(b.url || "").trim());
    token = String(b.token || "").trim();
  } catch { return json(res, 400, { error: { message: "地址不是合法的 URL" } }); }
  if (!/^https?:$/.test(url.protocol)) return json(res, 400, { error: { message: "地址要以 http:// 或 https:// 开头" } });
  if (!token) return json(res, 400, { error: { message: "要填令牌" } });
  // 指回自己会一圈圈转发下去（而且试连会借本机令牌通过）
  if (/^(127\.0\.0\.1|localhost|\[::1\])$/.test(url.hostname) && Number(url.port || 80) === port) return json(res, 400, { error: { message: "这是网页原型自己的地址，填 Gateway 的地址" } });
  const base = url.origin;
  try {
    const info = await probe(base, token);
    remote = { url: base, token };
    return json(res, 200, { mode: "remote", url: base, ...info });
  } catch (err) {
    return json(res, 502, { error: { message: `连不上 ${base}：${err.message}` } });
  }
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
    if (req.url === "/__web/connection") connection(req, res);
    else if (req.url.startsWith("/v1/") || req.url === "/healthz") proxy(req, res);
    else serveStatic(req, res);
  })
  .listen(port, "127.0.0.1", () => console.log(`komo web 原型：http://127.0.0.1:${port}`));
