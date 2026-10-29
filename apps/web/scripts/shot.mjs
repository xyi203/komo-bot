// 用 DevTools 协议按真实视口截图（headless 的窗口有最小宽度，--window-size 截不准手机）。
// 用法：node apps/web/scripts/shot.mjs <url> <out.png> <width>x<height> [要先执行的 JS] [等待毫秒]
import { spawn } from "node:child_process";
import { writeFile, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const [url, out, size = "1440x900", script = "", wait = "2500"] = process.argv.slice(2);
const [width, height] = size.split("x").map(Number);
const chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9300 + Math.floor(Math.random() * 500);
const profile = await mkdtemp(join(tmpdir(), "komo-shot-"));
const proc = spawn(chrome, ["--headless=new", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, "--hide-scrollbars", "about:blank"], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let target;
for (let i = 0; i < 50 && !target; i++) {
  await sleep(100);
  try { target = (await (await fetch(`http://127.0.0.1:${port}/json`)).json()).find((t) => t.type === "page"); } catch {}
}
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((r) => ws.addEventListener("open", r));
let id = 0;
const pending = new Map();
ws.addEventListener("message", (m) => { const d = JSON.parse(m.data); pending.get(d.id)?.(d); });
const send = (method, params = {}) => new Promise((r) => { pending.set(++id, r); ws.send(JSON.stringify({ id, method, params })); });

await send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 2, mobile: width < 700 });
await send("Page.enable");
await send("Page.navigate", { url });
await sleep(Number(wait));
if (script) {
  const r = await send("Runtime.evaluate", { expression: script, awaitPromise: true });
  if (r.result?.exceptionDetails) console.error(r.result.exceptionDetails.exception?.description || r.result.exceptionDetails.text);
  await sleep(600);
}
const shot = await send("Page.captureScreenshot", { format: "png" });
await writeFile(out, Buffer.from(shot.result.data, "base64"));
ws.close();
proc.kill();
console.log(out);
