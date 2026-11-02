// 拿一段真实会话给演示环境造对话数据：假装是 Responses API（POST /v1/responses），
// 按最后一条用户输入认出是哪一轮、按它后面的 function_call_output 条数挑出那一轮的
// providerBlocks（推理摘要、文字、工具调用），逐字流出去。
// 工具仍由演示 Gateway 真的执行，所以只回放只读的那几轮：导出文件先截到删改之前。
// 用法：komo session export ID -o s.jsonl
//       REPLAY_TITLE=标题 node apps/web/scripts/replay-model.mjs s.jsonl [--port 3999]
// 演示 Gateway 的模型指向 http://127.0.0.1:3999/v1（api_backend = "responses"），再照原话发消息。
import http from "node:http";
import { readFileSync } from "node:fs";

const file = process.argv[2];
const port = Number(process.argv[process.argv.indexOf("--port") + 1]) || 3999;
const lines = readFileSync(file, "utf8").trim().split("\n").map((l) => JSON.parse(l));
// 每条用户输入开一个 turn；turn 里每一轮按"这句话之后已有几条工具结果"来认
const turns = [];
let title = process.env.REPLAY_TITLE || "";
for (const l of lines) {
  if (l.type !== "message") continue;
  const m = l.message;
  if (m.role === "user" && typeof m.content === "string" && !m.content.startsWith("[")) turns.push({ text: m.content, results: 0, rounds: [] });
  const t = turns.at(-1);
  if (!t) continue;
  if (m.role === "toolResult") t.results++;
  if (m.role === "assistant" && m.providerBlocks) t.rounds.push({ after: t.results, blocks: m.providerBlocks, usage: m.usage || {} });
}
console.error(`载入 ${turns.length} 个 turn：${turns.map((t) => t.rounds.length).join(" / ")} 轮`);
const textOf = (item) => (typeof item.content === "string" ? item.content : (item.content || []).map((c) => c.text || "").join(""));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const chunks = (s, n = 4) => Array.from({ length: Math.ceil(s.length / n) }, (_, i) => s.slice(i * n, i * n + n));

async function stream(res, blocks, usage, model) {
  const send = (type, data) => res.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);
  const id = `resp_replay_${Date.now()}`;
  send("response.created", { response: { id, model, status: "in_progress" } });
  await sleep(400);
  for (const [i, item] of blocks.entries()) {
    const base = { output_index: i };
    if (item.type === "reasoning") {
      send("response.output_item.added", { ...base, item: { ...item, summary: [] } });
      for (const s of item.summary || []) for (const c of chunks(s.text, 6)) { send("response.reasoning_summary_text.delta", { ...base, delta: c }); await sleep(25); }
    } else if (item.type === "message") {
      send("response.output_item.added", { ...base, item: { ...item, content: [] } });
      const text = item.content.map((c) => c.text || "").join("");
      for (const c of chunks(text)) { send("response.output_text.delta", { ...base, delta: c }); await sleep(18); }
      send("response.output_text.done", { ...base, text });
    } else if (item.type === "function_call") {
      send("response.output_item.added", { ...base, item: { ...item, arguments: "" } });
      for (const c of chunks(item.arguments, 12)) { send("response.function_call_arguments.delta", { ...base, delta: c }); await sleep(12); }
      send("response.function_call_arguments.done", { ...base, arguments: item.arguments });
    }
    send("response.output_item.done", { ...base, item });
  }
  send("response.completed", {
    response: {
      id, model, status: "completed", output: blocks,
      usage: {
        input_tokens: usage.input ?? 0, output_tokens: usage.output ?? 0,
        input_tokens_details: { cached_tokens: usage.cacheRead ?? 0 },
        output_tokens_details: { reasoning_tokens: usage.reasoning ?? 0 },
      },
    },
  });
  res.end();
}

http.createServer((req, res) => {
  let body = "";
  req.on("data", (c) => (body += c)).on("end", async () => {
    const b = body ? JSON.parse(body) : {};
    const input = Array.isArray(b.input) ? b.input : [];
    // 最后一条用户输入认出是哪个 turn，再数它后面的工具结果
    let at = -1, turn = null;
    for (let i = input.length - 1; i >= 0 && !turn; i--) {
      if (input[i].role !== "user") continue;
      const text = textOf(input[i]);
      const hit = turns.find((t) => text.includes(t.text));
      if (hit) { turn = hit; at = i; }
    }
    const done = input.slice(at + 1).filter((x) => x.type === "function_call_output").length;
    const tools = (b.tools || []).length;
    console.error(`${req.method} ${req.url} model=${b.model} tools=${tools} input=${input.length} outputs=${done}`);
    if (req.url !== "/v1/responses") { res.writeHead(404).end(); return; }
    res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
    // 不带工具的请求是旁路调用（起标题之类），回一句标题
    if (!tools) {
      const blocks = [{ type: "message", role: "assistant", status: "completed", content: [{ type: "output_text", text: title || turns[0]?.text.slice(0, 20) || "回放", annotations: [] }] }];
      return stream(res, blocks, { input: 300, output: 12 }, b.model);
    }
    if (!turn) { console.error("  认不出是哪句话，回 400"); return res.end(); }
    const round = turn.rounds.find((r) => r.after === done) || turn.rounds.at(-1);
    await stream(res, round.blocks, round.usage, b.model);
  });
}).listen(port, "127.0.0.1", () => console.error(`回放模型：http://127.0.0.1:${port}/v1/responses`));
