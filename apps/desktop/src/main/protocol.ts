// app:// 协议：同一个来源 `app://komo` 下既是静态文件（SPA 产物、壳自带的提示页），也是 Gateway 的反向代理。
//   /v1/*、/healthz、/auth/me → 转给当前 Gateway，加设备令牌与额外请求头（渲染进程看不到令牌，也不需要 CORS）；
//                       Gateway 回 401 就清本地登录并回到登录界面。其余 /auth/*（登录、回调、令牌）只由主进程自己调，不转发
//   /_desktop/*       → dist/pages（连接设置、版本不一致提示页）
//   其他              → SPA 产物目录；无扩展名回落 `_shell.html`（客户端路由）
import { protocol } from "electron";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import path from "node:path";
import type { ConnectionManager } from "./connection";
import { clientRoot, pagesRoot } from "./paths";
import { errorMessage } from "./upstream";

export const APP_ORIGIN = "app://komo";

/** 必须在 app ready 之前调用：把 app:// 注册成"标准、安全、可流式"的协议。 */
export function registerSchemes() {
  protocol.registerSchemesAsPrivileged([
    { scheme: "app", privileges: { standard: true, secure: true, supportFetchAPI: true, stream: true } },
  ]);
}

const MIME: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".map": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".webp": "image/webp",
  ".gif": "image/gif",
  ".ico": "image/x-icon",
  ".woff": "font/woff",
  ".woff2": "font/woff2",
  ".txt": "text/plain; charset=utf-8",
  ".wasm": "application/wasm",
  ".webmanifest": "application/manifest+json",
};

const proxied = (pathname: string) => pathname === "/healthz" || pathname === "/auth/me" || pathname.startsWith("/v1/");

export function installHandler(connection: ConnectionManager) {
  protocol.handle("app", async (request) => {
    const url = new URL(request.url);
    if (url.host !== "komo") return plain(404, "not found");
    if (proxied(url.pathname)) return forward(request, url, connection);
    if (url.pathname.startsWith("/_desktop/")) {
      return serveFile(pagesRoot(), url.pathname.slice("/_desktop".length), { shellFallback: false, pageCsp: true });
    }
    return serveFile(clientRoot(), url.pathname, { shellFallback: true, pageCsp: false });
  });
}

// ---- 静态文件 ----

async function serveFile(
  root: string,
  pathname: string,
  options: { shellFallback: boolean; pageCsp: boolean },
): Promise<Response> {
  let decoded: string;
  try {
    decoded = decodeURIComponent(pathname);
  } catch {
    return plain(400, "bad path");
  }
  let file = path.join(root, decoded);
  // 目录穿越：解析后必须仍在根目录之内。
  if (file !== root && !file.startsWith(root + path.sep)) return plain(403, "forbidden");

  const hasExtension = path.extname(decoded) !== "";
  if (options.shellFallback && (!hasExtension || decoded.endsWith("/"))) file = path.join(root, "_shell.html");
  try {
    const body = await readFile(file);
    const ext = path.extname(file).toLowerCase();
    const headers: Record<string, string> = { "content-type": MIME[ext] ?? "application/octet-stream" };
    if (ext === ".html") {
      headers["content-security-policy"] = csp(options.pageCsp ? [] : inlineScriptHashes(body.toString("utf8")));
      headers["cache-control"] = "no-cache";
    }
    return new Response(new Uint8Array(body), { headers });
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return plain(404, options.shellFallback ? `${path.basename(file)} 不存在（先 pnpm build，或设置 KOMO_DESKTOP_CLIENT_DIR）` : "not found");
    }
    return plain(500, errorMessage(error));
  }
}

/**
 * 壳 html 里 TanStack Start 内联了几段启动脚本，不能一刀切禁内联：按内容算 sha256 放进 script-src，
 * 页面里多出来的、没算过的脚本照样被挡。样式允许内联（React 的 style 属性）。
 */
function inlineScriptHashes(html: string): string[] {
  const hashes: string[] = [];
  for (const match of html.matchAll(/<script(?![^>]*\ssrc=)[^>]*>([\s\S]*?)<\/script>/g)) {
    if (match[1]) hashes.push(`'sha256-${createHash("sha256").update(match[1]).digest("base64")}'`);
  }
  return hashes;
}

function csp(scriptHashes: string[]): string {
  return [
    "default-src 'self'",
    `script-src 'self' ${scriptHashes.join(" ")}`.trim(),
    "style-src 'self' 'unsafe-inline'",
    "img-src 'self' data: blob:",
    "font-src 'self' data:",
    "connect-src 'self'",
    "object-src 'none'",
    "base-uri 'none'",
    "form-action 'none'",
    "frame-ancestors 'none'",
  ].join("; ");
}

// ---- 反向代理 ----

/** 不从渲染进程原样带给 Gateway 的请求头：鉴权与来源由主进程管，其余是 HTTP 栈自己的事。 */
const DROP_REQUEST = new Set([
  "host", "origin", "referer", "cookie", "authorization", "connection", "content-length", "keep-alive",
  "transfer-encoding", "upgrade", "te", "trailer", "proxy-authorization", "sec-fetch-site", "sec-fetch-mode",
  "sec-fetch-dest", "sec-fetch-user",
]);
/** fetch 已经解过码、重新分块的响应头，原样带回去会让渲染进程对不上长度与编码。 */
const DROP_RESPONSE = new Set(["content-encoding", "content-length", "transfer-encoding", "connection", "keep-alive", "set-cookie"]);

async function forward(request: Request, url: URL, connection: ConnectionManager): Promise<Response> {
  const method = request.method.toUpperCase();
  const body = method === "GET" || method === "HEAD" ? undefined : await request.arrayBuffer();
  const wantsStream = (request.headers.get("accept") ?? "").includes("text/event-stream");

  const target = connection.targetSync();
  if (!target) return jsonError(401, "还没有登录 Gateway，请在连接设置里登录", "unauthorized");
  const headers = new Headers();
  for (const [name, value] of request.headers) if (!DROP_REQUEST.has(name.toLowerCase())) headers.set(name, value);
  // 额外请求头与 Authorization 来自主进程，覆盖渲染进程发来的同名头。
  for (const [name, value] of Object.entries(target.headers)) headers.set(name, value);
  // 事件流不要压缩：中间层对压缩流的缓冲会让事件迟到。
  if (wantsStream) headers.set("accept-encoding", "identity");

  // 渲染进程取消（EventSource.close、页面跳走）时，上游连接跟着断。
  const controller = new AbortController();
  request.signal.addEventListener("abort", () => controller.abort(), { once: true });
  try {
    const upstream = await fetch(`${target.base}${url.pathname}${url.search}`, {
      method,
      headers,
      body,
      redirect: "manual",
      signal: controller.signal,
    });
    // 令牌失效：清本地登录，主进程会把窗口带回登录界面。响应照常交给界面（它马上会被整页换掉）。
    if (upstream.status === 401) connection.handleUnauthorized(target);
    return relay(upstream, controller);
  } catch (error) {
    return jsonError(502, `转发到 Gateway 失败：${errorMessage(error)}`);
  }
}

function relay(upstream: Response, controller: AbortController): Response {
  const headers = new Headers();
  for (const [name, value] of upstream.headers) if (!DROP_RESPONSE.has(name.toLowerCase())) headers.set(name, value);
  if (upstream.status >= 300 && upstream.status < 400) {
    // 上游把我们重定向了（访问代理要登录）：让界面看到一个可读的错误，而不是跟着跳到外站。
    void upstream.body?.cancel();
    return jsonError(502, `Gateway 前面的代理重定向到了 ${upstream.headers.get("location") ?? "别处"}，检查连接设置里的额外请求头`);
  }
  if (!upstream.body || upstream.status === 204 || upstream.status === 304) {
    return new Response(null, { status: upstream.status, headers });
  }
  // 以流原样转发；渲染进程取消读取（cancel）时断开上游。
  const reader = upstream.body.getReader();
  const stream = new ReadableStream<Uint8Array>({
    async pull(sink) {
      try {
        const { done, value } = await reader.read();
        if (done) sink.close();
        else sink.enqueue(value);
      } catch (error) {
        sink.error(error);
      }
    },
    cancel() {
      controller.abort();
    },
  });
  return new Response(stream, { status: upstream.status, headers });
}

const plain = (status: number, text: string) =>
  new Response(text, { status, headers: { "content-type": "text/plain; charset=utf-8" } });

/** 与 Gateway 的 ErrorBody 同形（{ error: { code, message } }），界面按同一条路径展示。 */
const jsonError = (status: number, message: string, code = "internal") =>
  new Response(JSON.stringify({ error: { code, message } }), {
    status,
    headers: { "content-type": "application/json; charset=utf-8" },
  });
