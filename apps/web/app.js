// komo Web 客户端原型：数据全部来自 Gateway（经 serve.mjs 同源代理）。
// 会话正文 = 事件流 fold：先按页补读 GET /v1/sessions/{id}/events，再从 next 接 SSE 直播。
const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
const icon = (name, cls = "i") => `<svg class="${cls}" aria-hidden="true"><use href="#i-${name}"/></svg>`;

/* ---------- API ---------- */
class ApiError extends Error {
  constructor(status, message) { super(message); this.status = status; }
}
async function api(path, options = {}) {
  let res;
  try {
    res = await fetch(path, { ...options, headers: { accept: "application/json", "content-type": "application/json", ...options.headers } });
  } catch (err) {
    throw new ApiError(0, "连不上原型服务");
  }
  const body = await res.json().catch(() => null);
  if (!res.ok) throw new ApiError(res.status, body?.error?.message || body?.message || `HTTP ${res.status}`);
  return body;
}

/* ---------- 状态 ---------- */
const S = {
  boot: "loading",        // loading | ready | error
  bootError: "",
  lastGood: null,
  health: null,
  sessions: [],
  details: new Map(),     // session -> detail（只取任务会话，看它在不在跑）
  homeId: null,
  models: [],
  matters: [],
  memories: new Map(),
  current: null,
  view: null,
  showMore: new Set(),    // 展开了"显示全部"的分组
  group: localStorage.getItem("komo.group") || "tree",                // tree | workdir | flat
  sort: localStorage.getItem("komo.sort") || "updated",               // updated | created
  closed: new Set(JSON.parse(localStorage.getItem("komo.closed") || "[]")), // 收起的分组
  query: "",
  filter: localStorage.getItem("komo.filter") || "active",            // active | archived | all
  navOpen: new Set(JSON.parse(localStorage.getItem("komo.navOpen") || "[]")), // 展开了子任务的父会话
  open: new Set(),        // 展开着的折叠（按稳定 id）
  stream: "idle",         // idle | live | retrying
};
const newView = (id) => ({
  id, status: "loading", error: "", items: [], turns: new Map(), calls: new Map(), runs: new Map(), stats: { output: 0, input: 0, cache: 0, steps: 0 },
  recall: null, usage: null, model: null, delta: null, next: 0, pending: [], tree: null,
});

/* ---------- 格式 ---------- */
function ago(iso, now = Date.now()) {
  const d = new Date(iso), s = Math.max(0, Math.round((now - d) / 1000));
  if (s < 60) return "刚刚";
  if (s < 3600) return `${Math.floor(s / 60)} 分钟前`;
  const today = new Date(); today.setHours(0, 0, 0, 0);
  if (d >= today) return d.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false });
  if (d >= today - 86400000) return "昨天";
  return `${d.getMonth() + 1}月${d.getDate()}日`;
}
function elapsed(fromIso, to = Date.now()) {
  const s = Math.max(0, Math.round((to - new Date(fromIso)) / 1000));
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), r = s % 60;
  return h ? `${h}h ${m}m` : m ? `${m}m ${r}s` : `${r}s`;
}
const seconds = (ms) => (ms == null ? "" : ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(1)} s`);
const kTokens = (n) => (n < 1000 ? `${n}` : n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 100000 ? `${Math.round(n / 1000)}k` : `${(n / 1000).toFixed(1)}k`);
const clock = (d) => d.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false });
const titleOf = (s) => s?.title || (s?.kind === "main" ? "主会话" : "未命名会话");

/* 回复正文：代码块、标题、列表、表格、行内代码、粗体、链接，够看就行 */
function inline(s) {
  return esc(s)
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noreferrer">$1</a>');
}
function markdown(src) {
  const out = [], lines = String(src).replace(/\r/g, "").split("\n");
  for (let i = 0; i < lines.length; ) {
    const line = lines[i];
    if (/^```/.test(line)) {
      const buf = [];
      for (i++; i < lines.length && !/^```/.test(lines[i]); i++) buf.push(lines[i]);
      i++;
      out.push(`<pre><code>${esc(buf.join("\n"))}</code></pre>`);
    } else if (/^\s*\|.*\|\s*$/.test(line) && /^\s*\|[\s:|-]+\|\s*$/.test(lines[i + 1] || "")) {
      const cells = (l) => l.trim().replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
      const head = cells(line); i += 2;
      const rows = [];
      for (; i < lines.length && /^\s*\|/.test(lines[i]); i++) rows.push(cells(lines[i]));
      out.push(`<div class="table-wrap"><table><thead><tr>${head.map((c) => `<th>${inline(c)}</th>`).join("")}</tr></thead><tbody>${rows.map((r) => `<tr>${r.map((c) => `<td>${inline(c)}</td>`).join("")}</tr>`).join("")}</tbody></table></div>`);
    } else if (/^\s*([-*]|\d+\.)\s+/.test(line)) {
      const ordered = /^\s*\d+\./.test(line), items = [];
      for (; i < lines.length && /^\s*([-*]|\d+\.)\s+/.test(lines[i]); i++) items.push(lines[i].replace(/^\s*([-*]|\d+\.)\s+/, ""));
      out.push(`<${ordered ? "ol" : "ul"}>${items.map((t) => `<li>${inline(t)}</li>`).join("")}</${ordered ? "ol" : "ul"}>`);
    } else if (/^#{1,6}\s/.test(line)) {
      out.push(`<p class="md-h">${inline(line.replace(/^#+\s/, ""))}</p>`); i++;
    } else if (!line.trim()) {
      i++;
    } else {
      const buf = [];
      for (; i < lines.length && lines[i].trim() && !/^(```|#{1,6}\s|\s*([-*]|\d+\.)\s|\s*\|)/.test(lines[i]); i++) buf.push(lines[i]);
      out.push(`<p>${buf.map(inline).join("<br>")}</p>`);
    }
  }
  return out.join("");
}

/* ---------- 事件 fold ---------- */
const toolIcon = { bash: "term", codemode: "code", read: "eye", rg: "search", edit: "code", write: "code", task: "branch", monitor: "eye" };
function callLabel(name, args = {}) {
  const first = (v) => String(v ?? "").split("\n").find((l) => l.trim()) || "";
  switch (name) {
    case "bash": return first(args.command);
    case "read": case "write": case "edit": return args.path || "";
    case "rg": return `${args.pattern ?? ""}${args.path ? `  ${args.path}` : ""}`;
    case "task": return args.title || (args.task_id ? `#${args.task_id}` : first(args.prompt));
    case "codemode": return first(args.code || args.script);
    default: {
      const v = Object.values(args).find((x) => typeof x === "string");
      return first(v);
    }
  }
}
// 一轮 = 一条 Run：用户的话在上，过程（召回、过程中的说明、工具调用、插话）收进"用时"那一行，最终回答在下
function turnOf(v, e) {
  if (!e.run_id) return null;
  let t = v.turns.get(e.run_id);
  if (!t) {
    t = { kind: "turn", id: `t-${e.run_id}`, run: e.run_id, users: [], process: [], answer: null, start: e.at, end: null, status: "queued", recall: [] };
    v.turns.set(e.run_id, t);
    v.items.push(t);
  }
  return t;
}
function apply(v, e) {
  const d = e.data || {};
  const t = turnOf(v, e);
  switch (e.type) {
    case "message.user": {
      if (t && t.status === "running" && t.users.length) t.process.push({ kind: "steer", id: e.event_id, text: d.text });
      else (t ? t.users : v.items).push(t ? { text: d.text, at: e.at } : { kind: "user", id: e.event_id, text: d.text, at: e.at });
      const i = v.pending.findIndex((p) => p.text === d.text);
      if (i >= 0) v.pending.splice(i, 1);
      break;
    }
    case "memory.injected":
      v.recall = d.uses || [];
      if (t && v.recall.length) { t.recall = v.recall; t.process.unshift({ kind: "recall", id: e.event_id, uses: v.recall }); }
      break;
    case "message.assistant": {
      v.delta = null;
      v.stats.output += d.output_tokens || 0;
      v.stats.cache += d.cache_read_tokens || 0;
      v.stats.input += d.input_tokens || 0;
      if (d.input_tokens) v.usage = d.input_tokens;
      if (!t) break;
      // 上一段"回答"后面又来了一轮：它其实是过程中的说明，挪回过程里
      if (t.answer) { t.process.push({ kind: "text", id: t.answer.id, text: t.answer.text }); t.answer = null; }
      const calls = d.tool_calls || [];
      if (d.text && d.text.trim()) {
        if (calls.length) t.process.push({ kind: "text", id: e.event_id, text: d.text });
        else t.answer = { id: e.event_id, text: d.text, at: e.at };
      }
      for (const tc of calls) {
        let group = t.process[t.process.length - 1];
        if (!group || group.kind !== "tools") t.process.push((group = { kind: "tools", id: `g-${tc.call_id}`, calls: [] }));
        const call = { id: tc.call_id, name: tc.name, label: callLabel(tc.name, tc.arguments), status: "planned" };
        group.calls.push(call);
        v.calls.set(tc.call_id, call);
        v.stats.steps++;
      }
      break;
    }
    case "tool.started": {
      const c = v.calls.get(d.call_id);
      if (c) { c.status = "running"; c.startedAt = e.at; }
      break;
    }
    case "tool.result": {
      const c = v.calls.get(d.call_id);
      if (c) { c.status = d.status; c.ms = d.elapsed_ms; c.preview = d.preview; }
      break;
    }
    case "run.started":
      v.runs.set(e.run_id, { since: e.at, waiting: false });
      if (t) { t.status = "running"; t.start = e.at; }
      break;
    case "run.waiting": { const r = v.runs.get(e.run_id); if (r) r.waiting = true; break; }
    case "run.absorbed": {
      // 插话：这条排队的输入被正在跑的那一轮并了进去，话挪到宿主那一轮的过程里
      const host = v.turns.get(d.into);
      if (t && host) {
        for (const u of t.users) host.process.push({ kind: "steer", id: `${t.id}-${u.at}`, text: u.text });
        v.items.splice(v.items.indexOf(t), 1);
        v.turns.delete(t.run);
      }
      break;
    }
    case "run.completed": case "run.abandoned":
      v.runs.delete(e.run_id); v.delta = null;
      if (t) { t.status = "done"; t.end = e.at; }
      break;
    case "run.failed":
      v.runs.delete(e.run_id); v.delta = null;
      if (t) { t.status = "failed"; t.end = e.at; t.reason = d.reason || "没有给出原因"; }
      break;
    case "run.cancelled":
      v.runs.delete(e.run_id); v.delta = null;
      if (t) { t.status = "cancelled"; t.end = e.at; }
      break;
    case "context.compacted":
      v.items.push({ kind: "note", id: e.event_id, text: "上下文在这里压缩过一次" });
      break;
    case "config.changed":
      if (d.model) v.model = d.model;
      break;
  }
}

/* ---------- 加载 ---------- */
async function boot() {
  S.boot = "loading"; render();
  try {
    const [health, sessions, home, models, matters, memories] = await Promise.all([
      api("/healthz"), api("/v1/sessions?all=1"), api("/v1/home-session"), api("/v1/models"),
      api("/v1/matters"), api("/v1/memories"),
    ]);
    Object.assign(S, {
      health, sessions: sessions.sessions, homeId: home.session, models: models.models, matters: matters.matters,
      boot: "ready", lastGood: new Date(),
      memories: new Map(memories.memories.map((m) => [m.id, m])),
    });
    fillModels();
    render();
    loadTaskDetails();
    openSession(routeSession() || S.homeId);
  } catch (err) {
    S.boot = "error"; S.bootError = err.message; render();
  }
}
async function loadTaskDetails() {
  const tasks = S.sessions.filter((s) => s.kind === "task");
  const details = await Promise.allSettled(tasks.map((t) => api(`/v1/sessions/${t.session}`)));
  details.forEach((r, i) => r.status === "fulfilled" && S.details.set(tasks[i].session, r.value));
  render();
}
let source = null;
async function openSession(id) {
  if (source) { source.close(); source = null; }
  S.current = id;
  S.view = newView(id);
  const parent = S.sessions.find((x) => x.session === id)?.home;
  if (parent) { S.navOpen.add(parent); saveNav(); }
  if (location.hash !== `#/s/${id}`) history.replaceState(null, "", `#/s/${id}`);
  render();
  const v = S.view;
  try {
    let from = 0;
    for (;;) {
      const page = await api(`/v1/sessions/${id}/events?from=${from}&limit=500`);
      if (S.view !== v) return;
      page.events.forEach((e) => apply(v, e));
      from = page.next;
      if (!page.more) break;
    }
    v.next = from; v.status = "ready"; S.lastGood = new Date();
    loadTree(v);
    render({ stick: true });
    live(v);
  } catch (err) {
    if (S.view !== v) return;
    v.status = "error"; v.error = err.message; render();
  }
}
function live(v) {
  source = new EventSource(`/v1/sessions/${v.id}/events?from=${v.next}`);
  const frame = (fn) => (msg) => { if (S.view === v) { fn(JSON.parse(msg.data)); S.stream = "live"; render({ stick: true }); } };
  source.addEventListener("event", frame((f) => { apply(v, f.data); v.next = f.id; refreshSide(f.data); }));
  source.addEventListener("assistant_delta", frame((f) => {
    v.delta = v.delta && v.delta.run === f.data.run && v.delta.round === f.data.round ? { ...v.delta, text: v.delta.text + f.data.text } : { ...f.data };
  }));
  source.addEventListener("heartbeat", () => { S.stream = "live"; });
  source.onopen = () => { S.stream = "live"; render(); };
  source.onerror = () => { S.stream = "retrying"; render(); };
}
async function loadTree(v) {
  try { v.tree = await api(`/v1/sessions/${v.id}/tree`); } catch { v.tree = null; }
  if (S.view === v) render();
}
// 事件影响到侧栏和右栏的，按需重取那一块
let sideTimer = null;
function refreshSide(e) {
  if (!/^(run\.|memory\.)/.test(e.type)) return;
  if (/^run\.(completed|failed|cancelled)$/.test(e.type) && S.view) loadTree(S.view);
  clearTimeout(sideTimer);
  sideTimer = setTimeout(async () => {
    const [sessions, matters] = await Promise.allSettled([api("/v1/sessions?all=1"), api("/v1/matters")]);
    if (sessions.status === "fulfilled") S.sessions = sessions.value.sessions;
    if (matters.status === "fulfilled") S.matters = matters.value.matters;
    loadTaskDetails();
  }, 400);
}

/* ---------- 发送、停止、新会话 ---------- */
async function send(text) {
  const v = S.view;
  if (!v || !text.trim()) return;
  const pending = { id: crypto.randomUUID(), text, state: "sending" };
  v.pending.push(pending); render({ stick: true, force: true });
  const busy = v.runs.size > 0;
  const body = { request_key: `web:${v.id}:${pending.id}`, text, interject: busy };
  const model = $("model").value, effort = $("effort").value;
  const def = S.models.find((m) => m.default);
  if (model && model !== def?.id) body.model = model;
  if (effort) body.effort = effort;
  try {
    await api(`/v1/sessions/${v.id}/runs`, { method: "POST", body: JSON.stringify(body) });
    pending.state = "sent";
  } catch (err) {
    pending.state = "failed"; pending.error = err.message; pending.body = body;
  }
  render();
}
async function retry(id) {
  const v = S.view, p = v.pending.find((x) => x.id === id);
  if (!p) return;
  p.state = "sending"; render();
  try {
    await api(`/v1/sessions/${v.id}/runs`, { method: "POST", body: JSON.stringify(p.body) });
    p.state = "sent";
  } catch (err) { p.state = "failed"; p.error = err.message; }
  render();
}
async function cancelRuns() {
  const v = S.view;
  await Promise.allSettled([...v.runs.keys()].map((run) => api(`/v1/runs/${run}/cancel`, { method: "POST", body: "{}" })));
}
async function forkSession() {
  try {
    const r = await api(`/v1/sessions/${S.current}/fork`, { method: "POST", body: "{}" });
    const list = await api("/v1/sessions?all=1");
    S.sessions = list.sessions;
    openSession(r.session);
  } catch (err) { alert(`没能分叉：${err.message}`); }
}
async function newSession(workdir) {
  try {
    const body = { request_key: `web:new:${crypto.randomUUID()}` };
    if (workdir) body.workdir = workdir;
    const s = await api("/v1/sessions", { method: "POST", body: JSON.stringify(body) });
    S.sessions.unshift(s);
    openSession(s.session);
  } catch (err) {
    alert(`没能新建会话：${err.message}`);
  }
}

/* ---------- 渲染：侧栏 ---------- */
const running = (id) => (S.details.get(id)?.unfinished || []).length > 0;
const saveNav = () => localStorage.setItem("komo.navOpen", JSON.stringify([...S.navOpen]));
const archived = (s) => s.state === "deleted" || s.state === "closing";
const passes = (s) => S.filter === "all" || (S.filter === "archived" ? archived(s) : !archived(s));
function sessionBtn(s, child = false, flat = false) {
  const live = running(s.session) ? `<span class="dot" role="img" aria-label="运行中"></span>` : "";
  const tag = archived(s) && S.filter !== "archived" ? `<span class="tag">已归档</span>` : flat && s.kind === "task" ? `<span class="tag">子任务</span>` : "";
  return `<li><button class="session${child ? " child" : ""}${archived(s) ? " is-archived" : ""}" data-session="${s.session}" ${s.session === S.current ? 'aria-current="true"' : ""} title="${esc(titleOf(s))}">
    <span class="name">${esc(titleOf(s))}</span>${tag}${live}<span class="when num">${ago(s.updated_at)}</span></button></li>`;
}
const kidsOf = (s) => S.sessions.filter((k) => k.kind === "task" && k.home === s.session && passes(k)).sort((a, b) => new Date(b.created_at) - new Date(a.created_at));
// 有子任务的会话：名字后面一个 ▸。收起时下面一行"N 个子任务"，点它或 ▸ 展开；展开后直接列出全部子任务
function withChildren(s) {
  const kids = kidsOf(s);
  if (!kids.length) return sessionBtn(s);
  const open = S.navOpen.has(s.session);
  const live = kids.filter((k) => running(k.session)).length;
  const own = running(s.session) ? `<span class="dot" role="img" aria-label="运行中"></span>` : "";
  const row = `<li><div class="session parent${archived(s) ? " is-archived" : ""}" data-session="${s.session}" ${s.session === S.current ? 'aria-current="true"' : ""}>
      <button class="sname" title="${esc(titleOf(s))}">${esc(titleOf(s))}</button>
      <button class="chev" data-kids="${s.session}" aria-expanded="${open}" aria-label="${open ? "收起" : "展开"} ${kids.length} 个子任务">${icon("right")}</button>
      ${own}<span class="when num">${ago(s.updated_at)}</span></div></li>`;
  if (open) return row + kids.map((k) => sessionBtn(k, true)).join("");
  return row + `<li><button class="kids-toggle" data-kids="${s.session}" aria-expanded="false">${kids.length} 个子任务${icon("right")}${live ? `<span class="kids-live"><span class="dot" aria-hidden="true"></span>${live} 个在跑</span>` : ""}</button></li>`;
}
function skeletonRows(n, cls = "sk-row") {
  return Array.from({ length: n }, (_, i) => `<div class="${cls}"><span class="skeleton" style="width:${[72, 56, 64, 48, 60][i % 5]}%"></span></div>`).join("");
}
// 侧栏会话：分组（会话树 / 工作目录 / 单列表）× 排序（最近更新 / 最近创建）× 筛选（隐藏已归档 / 全部 / 仅已归档），外加按标题搜索
const DEFAULT_DIR = "默认目录";
const dirOf = (s) => s.workdir || "";
const dirLabel = (dir) => (dir ? dir.replace(/\/+$/, "").split("/").pop() || dir : DEFAULT_DIR);
const sortKey = (s) => new Date(S.sort === "created" ? s.created_at : s.updated_at);
const bySort = (a, b) => sortKey(b) - sortKey(a);
const saveClosed = () => localStorage.setItem("komo.closed", JSON.stringify([...S.closed]));
const PAGE_ROWS = { tree: 12, workdir: 5, flat: 20 };
function groupBlock(key, label, sessions, { title = "", add = null, nest = true } = {}) {
  const closed = S.closed.has(key), more = S.showMore.has(key), cap = PAGE_ROWS[S.group];
  const shown = more ? sessions : sessions.slice(0, cap);
  const none = { active: "没有活跃的会话。", archived: "没有已归档的会话。", all: "还没有会话。" }[S.filter];
  const rows = sessions.length ? shown.map((x) => (nest ? withChildren(x) : sessionBtn(x, false, true))).join("") : `<li class="empty-line pad">${S.query ? "没有标题匹配的会话。" : none}</li>`;
  return `<section class="group">
    <div class="group-head"><button class="group-toggle" aria-expanded="${!closed}" data-group="${esc(key)}" title="${esc(title)}">${esc(label)} <span class="count num">${sessions.length}</span>${icon("down")}</button>
      ${add != null ? `<button class="icon-btn" data-new-in="${esc(add)}" aria-label="在 ${esc(label)} 新建会话" title="在 ${esc(title || label)} 新建会话">${icon("plus")}</button>` : ""}</div>
    ${closed ? "" : `<ul>${rows}${sessions.length > cap ? `<li><button class="text-btn more" data-show-more="${esc(key)}">${more ? "收起" : `显示全部 ${sessions.length} 个`}</button></li>` : ""}</ul>`}
  </section>`;
}
function renderGroups() {
  if (S.boot === "loading") return ($("groups").innerHTML = `<div class="group">${skeletonRows(8)}</div>`);
  if (S.boot === "error") {
    return ($("groups").innerHTML = `<div class="block-error"><p>会话列表没取到。</p><button class="text-btn" data-retry-boot>重试</button></div>`);
  }
  const visible = S.sessions.filter(passes);
  // 搜索时不分组、不嵌套：直接列出标题匹配的
  if (S.query) {
    const q = S.query.toLowerCase();
    return ($("groups").innerHTML = groupBlock("search", "搜索结果", visible.filter((x) => titleOf(x).toLowerCase().includes(q)).sort(bySort), { nest: false }));
  }
  if (S.group === "flat") return ($("groups").innerHTML = groupBlock("flat", { active: "会话", archived: "已归档", all: "全部会话" }[S.filter], visible.sort(bySort), { nest: false }));
  const top = (x) => x.kind !== "task" || !S.sessions.some((p) => p.session === x.home && passes(p));
  const main = visible.find((x) => x.session === S.homeId);
  const rest = visible.filter((x) => x.session !== S.homeId && top(x)).sort(bySort);
  const head = main ? `<section class="group"><ul>${withChildren(main)}</ul></section>` : "";
  if (S.group === "tree") {
    return ($("groups").innerHTML = head + groupBlock("tree", { active: "最近", archived: "已归档", all: "全部会话" }[S.filter], rest));
  }
  // 按工作目录：每个目录一组，按组里最近的那个会话排；组头的 + 在这个目录新建会话
  const dirs = new Map();
  for (const x of rest) { const d = dirOf(x); if (!dirs.has(d)) dirs.set(d, []); dirs.get(d).push(x); }
  const order = [...dirs.entries()].sort((a, b) => sortKey(b[1][0]) - sortKey(a[1][0]));
  $("groups").innerHTML = head + (order.length
    ? order.map(([d, list]) => groupBlock(`dir:${d}`, dirLabel(d), list, { title: d || "~/.komo/workspaces", add: d })).join("")
    : groupBlock("dir:", DEFAULT_DIR, []));
}
const MENU = [
  ["分组方式", "group", [["tree", "按会话树", "branch"], ["workdir", "按工作目录", "folder"], ["flat", "单列表", "list"]]],
  ["排序方式", "sort", [["updated", "最近更新", "clock"], ["created", "最近创建", "calendar"]]],
  ["筛选会话", "filter", [["active", "隐藏已归档", "eye-off"], ["all", "全部对话（显示已归档）", "chat"], ["archived", "仅显示已归档", "archive"]]],
];
function renderMenu() {
  $("view-menu").innerHTML = MENU.map(([title, key, items], i) => `${i ? `<div class="menu-sep" role="separator"></div>` : ""}<div class="menu-title" role="presentation">${title}</div>
    ${items.map(([value, label, ic]) => `<button class="menu-item" role="menuitemradio" aria-checked="${S[key] === value}" data-set="${key}" data-value="${value}">${icon(ic)}<span>${label}</span>${S[key] === value ? icon("check", "i check") : ""}</button>`).join("")}`).join("");
}
function setMenu(open) {
  $("view-menu").hidden = !open;
  $("view-menu-btn").setAttribute("aria-expanded", open);
  if (open) { renderMenu(); $("view-menu").querySelector('[aria-checked="true"]')?.focus(); }
}
function setSearch(open) {
  $("sec-search").hidden = !open;
  $("search-toggle").setAttribute("aria-expanded", open);
  if (open) $("search-input").focus();
  else if (S.query) { S.query = ""; $("search-input").value = ""; render(); }
}
function renderConn() {
  const ok = S.boot === "ready" && S.stream !== "retrying";
  $("conn-dot").className = `dot${ok ? "" : S.boot === "loading" ? " idle" : " bad"}`;
  $("conn-dot").setAttribute("aria-label", ok ? "Gateway 已连接" : "Gateway 未连接");
  $("conn-text").textContent =
    S.boot === "loading" ? "正在连接 Gateway" : S.boot === "error" ? "连不上 Gateway" : S.stream === "retrying" ? "事件流断了，正在重连" : `Gateway ${S.health?.version ?? ""} · 已连接`;
}

/* ---------- 渲染：对话 ---------- */
function toolsFold(item) {
  const total = item.calls.reduce((a, c) => a + (c.ms || 0), 0);
  const failed = item.calls.filter((c) => c.status === "failed" || c.status === "uncertain").length;
  const runningNow = item.calls.some((c) => c.status === "running" || c.status === "planned");
  const open = S.open.has(item.id);
  const rows = item.calls
    .map((c) => {
      const out = (c.preview || "").split("\n").slice(1, 13).join("\n");
      const tail = c.status === "running" || c.status === "planned" ? `<span class="spinner sm" aria-label="运行中"></span>` : c.status === "failed" ? `<span class="bad">失败</span>` : c.status === "uncertain" ? `<span class="bad">结果不明</span>` : `<span class="num">${seconds(c.ms)}</span>`;
      return `<div class="call">${icon(toolIcon[c.name] || "term")}<span class="label"><span class="tname">${esc(c.name)}</span>${esc(c.label)}</span><span class="ms">${tail}</span></div>
        ${out.trim() ? `<div class="call-out">${esc(out)}</div>` : ""}`;
    })
    .join("");
  const head = `${runningNow ? "正在运行" : "运行了"} ${item.calls.length} 次调用${total ? ` · <span class="num">${seconds(total)}</span>` : ""}${failed ? ` · <span class="bad">${failed} 次没成</span>` : ""}`;
  return `<div class="fold"><button class="fold-head" aria-expanded="${open}" data-fold="${item.id}">${head}${icon("right")}</button>
    <div class="fold-body" ${open ? "" : "hidden"}>${rows}</div></div>`;
}
function useCounts(uses) {
  const mems = uses.map((u) => S.memories.get(u.memory));
  const conv = mems.filter((m) => m?.application === "convention").length;
  return { mems, conv, clue: mems.length - conv };
}
function recallFold(item) {
  const { mems, conv, clue } = useCounts(item.uses);
  const open = S.open.has(item.id);
  const rows = item.uses
    .map((u, i) => {
      const m = mems[i];
      return `<div class="recall-row"><span class="mote ${m?.application === "convention" ? "full" : "ring"}" aria-hidden="true"></span><span>${m ? esc(m.content) : `<span class="muted">${esc(u.memory)}（库里已经没有这条）</span>`}</span></div>`;
    })
    .join("");
  const parts = [conv && `${conv} 条约定`, clue && `${clue} 条线索`].filter(Boolean).join("、");
  return `<div class="fold"><button class="fold-head lit" aria-expanded="${open}" data-fold="${item.id}"><span class="mote full" aria-hidden="true"></span>带上 ${parts}${icon("right")}</button>
    <div class="fold-body" ${open ? "" : "hidden"}>${rows}</div></div>`;
}
// 这段对话派出去、还在跑的任务会话
function kidLines(v) {
  return S.sessions.filter((s) => s.kind === "task" && s.home === v.id && running(s.session))
    .map((k) => `<button class="live-line link" data-session="${k.session}"><span class="spinner" aria-hidden="true"></span><span class="label">${esc(titleOf(k))}</span><span class="num">${elapsed(k.created_at)}</span>${icon("right")}</button>`)
    .join("");
}
const stamp = (iso) => { const d = new Date(iso); const today = new Date(); return d.toDateString() === today.toDateString() ? clock(d) : `${d.getMonth() + 1}/${d.getDate()} ${clock(d)}`; };
function userBubble(text, at) {
  return `<div class="msg-user"><div class="bubble">${esc(text)}</div>
    <div class="msg-meta"><span class="num">${at ? stamp(at) : ""}</span><button class="icon-btn sm" data-copy="${esc(text)}" aria-label="复制">${icon("copy")}</button></div></div>`;
}
function duration(ms) {
  const s = Math.max(0, Math.round(ms / 1000)), m = Math.floor(s / 60), h = Math.floor(m / 60);
  return h ? `${h}h ${m % 60}m` : m ? `${m}m ${s % 60}s` : `${s}s`;
}
function processRow(p) {
  if (p.kind === "tools") return toolsFold(p);
  if (p.kind === "recall") return recallFold(p);
  if (p.kind === "steer") return `<div class="steer"><span class="steer-tag">插话</span>${esc(p.text)}</div>`;
  if (p.kind === "text") return `<div class="msg-agent proc">${markdown(p.text)}</div>`;
  return "";
}
function turnView(t, v) {
  const live = t.status === "running" || t.status === "queued";
  const spent = duration((t.end ? new Date(t.end) : new Date()) - new Date(t.start));
  const steps = t.process.reduce((a, p) => a + (p.kind === "tools" ? p.calls.length : 0), 0);
  const failedCalls = t.process.reduce((a, p) => a + (p.kind === "tools" ? p.calls.filter((c) => c.status === "failed" || c.status === "uncertain").length : 0), 0);
  // 跑着的那一轮默认展开，看得到正在做什么；结束了的默认收起，只留结论
  const open = S.open.has(t.id) ? true : S.open.has(`${t.id}:closed`) ? false : live;
  const running = live ? [...v.calls.values()].reverse().find((c) => c.status === "running") : null;
  const headText = live
    ? `<span class="spinner sm" aria-hidden="true"></span>${running ? `${esc(running.name)} ${esc(running.label)}` : v.delta ? "正在回复" : "正在想"} · <span class="num">${spent}</span>`
    : `${t.status === "done" ? "完成，" : ""}用时 <span class="num">${spent}</span>${steps ? ` · <span class="num">${steps}</span> 步` : ""}${failedCalls ? ` · <span class="bad">${failedCalls} 步没成</span>` : ""}`;
  const lit = t.recall.length ? `<span class="head-lit"><span class="mote full" aria-hidden="true"></span>${recallShort(t.recall)}</span>` : "";
  const hasProcess = t.process.length > 0 || live;
  const head = hasProcess
    ? `<button class="turn-head" aria-expanded="${open}" data-turn="${t.id}">${headText}${icon("down", "i chev")}${lit}</button>`
    : "";
  const answer = t.answer
    ? `<div class="msg-agent">${markdown(t.answer.text)}</div>
       <div class="msg-meta left"><button class="icon-btn sm" data-copy="${esc(t.answer.text)}" aria-label="复制回答">${icon("copy")}</button><span class="num">${stamp(t.answer.at)}</span></div>`
    : live && v.delta ? `<div class="msg-agent streaming">${markdown(v.delta.text)}</div>` : "";
  const tail = t.status === "failed" ? `<p class="note bad">这一轮失败了：${esc(t.reason)}</p>` : t.status === "cancelled" ? `<p class="note">这一轮已取消</p>` : "";
  return `<section class="turn">
    ${t.users.map((u) => userBubble(u.text, u.at)).join("")}
    ${head}
    ${hasProcess && open ? `<div class="turn-process">${t.process.map(processRow).join("") || `<p class="note">还没有步骤。</p>`}</div>` : ""}
    ${answer}${tail}
  </section>`;
}
function recallShort(uses) {
  const { conv, clue } = useCounts(uses);
  return `带上 ${[conv && `${conv} 条约定`, clue && `${clue} 条线索`].filter(Boolean).join("、")}`;
}
function renderThread() {
  const v = S.view;
  const host = $("transcript");
  if (S.boot === "error") {
    host.innerHTML = `<div class="state-box"><p>连不上 Gateway：${esc(S.bootError)}。${S.lastGood ? `上次正常是 ${clock(S.lastGood)}。` : ""}</p><p class="muted">确认 Gateway 在跑（<code>komo gateway status</code>），再重试。</p><button class="text-btn" data-retry-boot>重试</button></div>`;
    return;
  }
  if (!v || v.status === "loading") {
    host.innerHTML = `<div class="sk-thread">
      <div class="sk-user"><span class="skeleton" style="width:46%;height:40px;border-radius:16px"></span></div>
      ${["92%", "78%", "40%"].map((w) => `<span class="skeleton line" style="width:${w}"></span>`).join("")}
      <span class="skeleton line" style="width:30%;margin-top:18px"></span>
      ${["88%", "64%"].map((w) => `<span class="skeleton line" style="width:${w}"></span>`).join("")}</div>`;
    return;
  }
  if (v.status === "error") {
    host.innerHTML = `<div class="state-box"><p>这段对话没取到：${esc(v.error)}。</p><button class="text-btn" data-retry-session>重试</button></div>`;
    return;
  }
  const body = v.items
    .map((it) => {
      if (it.kind === "turn") return turnView(it, v);
      if (it.kind === "user") return userBubble(it.text, it.at);
      if (it.kind === "note") return `<p class="note${it.bad ? " bad" : ""}">${esc(it.text)}</p>`;
      return "";
    })
    .join("");
  const delta = "";
  const pending = v.pending
    .map((p) => `<div class="msg-user"><div class="bubble${p.state === "failed" ? " failed" : " pending"}">${esc(p.text)}</div>
      <span class="meta">${p.state === "failed" ? `没发出去：${esc(p.error)} · <button class="text-btn" data-retry-send="${p.id}">重试</button>` : "发送中"}</span></div>`)
    .join("");
  const empty = !v.items.length && !pending && !v.runs.size
    ? `<div class="state-box empty"><p>${v.id === S.homeId ? "主会话里还没有消息。" : "这段对话还没开始。"}</p><p class="muted">在下面说第一句话。</p></div>`
    : "";
  host.innerHTML = empty + body + pending + kidLines(v);
}

/* ---------- 渲染：输入框与标题 ---------- */
function fillModels() {
  const def = S.models.find((m) => m.default) || S.models[0];
  $("model").innerHTML = S.models.map((m) => `<option value="${m.id}" ${m === def ? "selected" : ""}>${esc(m.name.replace(/（.*）/, ""))}</option>`).join("");
  fillEfforts();
}
function fillEfforts() {
  const m = S.models.find((x) => x.id === $("model").value);
  const names = { none: "不想", minimal: "最低", low: "低", medium: "中", high: "高", xhigh: "很高", max: "最高" };
  $("effort").innerHTML = `<option value="">默认</option>` + (m?.efforts || []).map((e) => `<option value="${e}">${names[e] || e}</option>`).join("");
}
function windowOf(v) {
  const m = S.models.find((x) => x.model === v?.model || x.id === v?.model) || S.models.find((x) => x.default);
  return m?.context_window || 0;
}
function renderComposer() {
  const v = S.view;
  const busy = !!v && v.runs.size > 0;
  const curS = S.sessions.find((x) => x.session === S.current);
  $("input").placeholder = curS && archived(curS) ? "这段对话已归档，只能查看" : busy ? "↵ 插话，并进正在跑的这一轮" : "↵ 发送，⇧↵ 换行";
  $("stop-run").hidden = !busy || !!$("input").value.trim();
  $("send").hidden = !$("stop-run").hidden;
  const cur = S.sessions.find((x) => x.session === S.current);
  const readOnly = !!cur && archived(cur);
  const ready = S.boot === "ready" && !!v && v.status === "ready" && !readOnly;
  $("input").disabled = !ready;
  $("send").disabled = !ready;
  $("composer").classList.toggle("off", !ready);
  $("model").hidden = $("effort").hidden = !S.models.length;
  const win = windowOf(v), used = v?.usage || 0;
  const pct = win && used ? Math.min(100, (used / win) * 100) : 0;
  // 环：上下文用了多少，和右栏"上下文"同一个数
  const r = 6, len = 2 * Math.PI * r;
  $("ring").innerHTML = `<svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="${r}" class="track"/><circle cx="8" cy="8" r="${r}" class="fill" stroke-dasharray="${(len * pct) / 100} ${len}"/></svg><span class="num">${used ? pctText(used, win) : "—"}</span>`;
  $("ring").title = used ? `上下文 ${pctText(used, win)}（${kTokens(used)} / ${kTokens(win)}）` : "这段对话还没有请求过模型";
  $("ring").setAttribute("aria-label", $("ring").title);
  const st = v?.stats, turns = v ? [...v.turns.values()].length : 0;
  const hit = st && st.input ? Math.round((st.cache / st.input) * 100) : null;
  $("under-left").innerHTML = !v || !turns ? "" : busy
    ? `<span class="spinner sm" aria-hidden="true"></span> 正在跑 · <span class="num">${elapsed([...v.runs.values()][0].since)}</span>`
    : `<span class="num">${turns}</span> 轮 <span class="num">${st.steps}</span> 步<span class="sep">·</span><span class="num">${kTokens(st.input + st.output)}</span> tok${hit != null ? `<span class="sep">·</span>缓存命中 <span class="num">${hit}%</span>` : ""}`;
  const s = S.sessions.find((x) => x.session === S.current);
  $("title").textContent = S.boot === "ready" && s ? titleOf(s) : "komo";
  document.title = `${$("title").textContent} · komo`;
}


/* ---------- 上下文构成 ----------
   原料来自会话树（GET /v1/sessions/{id}/tree）：沿 leafId 取当前分支，压缩过就只算保留下来的部分；
   系统提示的各段与工具定义取路径上最新的 komo.system；衰减过的工具结果按 context_edit 的替换视图算。
   各项按字节 / 4 估（komo 自己的窗口保护也这样估），再按上一次请求实报的 input_tokens 等比校准，
   所以合计是实测、各项比例是估算。 */
const enc = new TextEncoder();
const bytes = (x) => enc.encode(typeof x === "string" ? x : JSON.stringify(x ?? "")).length;
const CONTEXT_PARTS = [
  ["messages", "对话消息", "#4f6b38"],
  ["results", "工具结果", "#7f9a63"],
  ["tools", "工具定义", "#b3c59c"],
  ["memory", "记忆", "#3f4637"],
  ["skills", "Skills 清单", "#8d9384"],
  ["project", "项目规范", "#aab0a1"],
  ["system", "系统提示", "#c9cdc1"],
  ["summary", "压缩摘要", "#dfe2d9"],
];
function contextParts(tree) {
  if (!tree?.entries?.length) return null;
  const byId = new Map(tree.entries.map((e) => [e.id, e]));
  const path = [];
  for (let id = tree.leafId; id && byId.has(id); id = byId.get(id).parentId) path.push(byId.get(id));
  path.reverse();
  let start = 0, summary = 0;
  path.forEach((e, i) => {
    if (e.customType !== "komo.compaction") return;
    const d = e.data?.data || e.data || {};
    summary = bytes(d.summary || d.text || "");
    const head = path.findIndex((x) => x.id === String(d.head ?? ""));
    start = head >= 0 ? head : i + 1;
  });
  // komo.system 是增量：分段为 null 表示删掉，工具按 toolsAdded / toolsRemoved 增减，沿路径叠起来才是当前这份
  const sec = {}, toolMap = new Map();
  for (const e of path) {
    if (e.customType !== "komo.system") continue;
    const m = e.data?.model?.[0] || {};
    for (const [k, val] of Object.entries(m.sections || {})) val == null ? delete sec[k] : (sec[k] = val);
    for (const t of m.toolsAdded || []) toolMap.set(t.name || t.function?.name, t);
    for (const t of m.toolsRemoved || []) toolMap.delete(t.name || t.function?.name);
  }
  const tools = [...toolMap.values()];
  const edits = new Map(path.filter((e) => e.type === "context_edit" && e.replacement).map((e) => [e.targetId, e.replacement]));
  const raw = { messages: 0, results: 0, tools: bytes(tools), memory: bytes(sec.memory || ""), skills: bytes(sec.skills || ""), project: bytes(sec.project || ""), system: 0, summary };
  for (const [k, v] of Object.entries(sec)) if (!["memory", "skills", "project"].includes(k)) raw.system += bytes(v);
  for (const e of path.slice(start)) {
    if (e.type !== "message") continue;
    const m = edits.has(e.id) ? { ...e.message, ...edits.get(e.id) } : e.message;
    const blocks = typeof m.content === "string" ? [{ type: "text", text: m.content }] : m.content || [];
    const size = blocks.reduce((a, b) => a + (b.type === "text" ? bytes(b.text) : b.type === "toolCall" ? bytes(b.name) + bytes(b.arguments) : 0), 0);
    if (m.role === "toolResult") raw.results += size;
    else if (m.role === "user" && blocks[0]?.text?.startsWith("[已知记忆]")) raw.memory += size;
    else raw.messages += size;
  }
  const est = Object.fromEntries(Object.entries(raw).map(([k, b]) => [k, Math.round(b / 4)]));
  return { est, tools: tools.map((t) => ({ name: t.name || t.function?.name || "?", tokens: Math.round(bytes(t) / 4) })) };
}
function contextView(v) {
  const win = windowOf(v);
  const parts = contextParts(v?.tree);
  const sum = parts ? Object.values(parts.est).reduce((a, b) => a + b, 0) : 0;
  const total = v?.usage || sum;
  const scale = parts && v?.usage && sum ? v.usage / sum : 1;
  const rows = parts ? CONTEXT_PARTS.map(([k, label, color]) => ({ k, label, color, tokens: Math.round(parts.est[k] * scale) })).filter((r) => r.tokens > 0) : [];
  return { win, total, measured: !!v?.usage, rows, tools: parts?.tools || [], scale };
}
const pctOf = (n, win) => (win ? (n / win) * 100 : 0);
const pctText = (n, win) => { const p = pctOf(n, win); return p >= 10 ? `${p.toFixed(0)}%` : p >= 0.1 ? `${p.toFixed(1)}%` : "<0.1%"; };
function stackBar(c, mini = false) {
  if (!c.win) return "";
  const segs = c.rows.map((r) => `<span style="width:${pctOf(r.tokens, c.win)}%;background:${r.color}"></span>`).join("");
  return `<span class="stack${mini ? " mini" : ""}" aria-hidden="true">${c.rows.length ? segs : `<span style="width:${pctOf(c.total, c.win)}%;background:var(--accent)"></span>`}</span>`;
}
function contextCard(v) {
  const c = contextView(v);
  if (!c.measured) return { value: "—", body: empty("这段对话还没有请求过模型。") };
  const pct = c.win ? pctText(c.total, c.win) : "—";
  const free = Math.max(0, c.win - c.total);
  const rows = c.rows.map((r) => `<div class="ctx-row"><span class="sw" style="background:${r.color}"></span><span class="lbl">${r.label}</span><span class="num v">${kTokens(r.tokens)}</span><span class="num p">${pctText(r.tokens, c.win)}</span></div>`).join("");
  const toolsOpen = S.open.has("ctx:tools");
  const toolList = c.tools.length
    ? `<button class="ctx-more" aria-expanded="${toolsOpen}" data-fold-ctx="ctx:tools">${icon("right")}<span>工具定义</span><span class="num">${c.tools.length}</span></button>
       ${toolsOpen ? `<div class="ctx-sub">${c.tools.sort((a, b) => b.tokens - a.tokens).map((t) => `<div class="ctx-row sub"><span class="lbl">${esc(t.name)}</span><span class="num v">${kTokens(Math.round(t.tokens * c.scale))}</span></div>`).join("")}</div>` : ""}`
    : "";
  const body = `
    <div class="ctx-top"><span><b class="num">${pct}</b> 已用</span><span class="num muted">${kTokens(c.total)} / ${c.win ? kTokens(c.win) : "?"}</span></div>
    ${stackBar(c)}
    ${rows || (v?.tree === null ? empty("会话树没取到，分不出构成。") : empty("正在读会话树。"))}
    ${c.win ? `<div class="ctx-row"><span class="sw free"></span><span class="lbl">空闲</span><span class="num v">${kTokens(free)}</span><span class="num p">${pctText(free, c.win)}</span></div>` : ""}
    ${toolList}
    <p class="ctx-note">${c.measured ? "合计是上一次请求实报的 token；各项按字节估算后按合计校准。" : "还没有实报的用量，各项都是按字节估算。"}</p>`;
  return { value: `${pct}${stackBar(c, true)}`, body };
}

/* ---------- 渲染：右栏 ---------- */
function card(id, title, value, body, openByDefault = true) {
  const expanded = S.open.has(`card:${id}`) ? !openByDefault : openByDefault;
  return `<section class="card"><button class="card-head" aria-expanded="${expanded}" aria-controls="${id}" data-card="${id}">
      <span class="t">${title}</span>${icon("down", "i chev")}<span class="aside-val num">${value}</span></button>
    <div class="card-body" id="${id}">${body}</div></section>`;
}
const empty = (text) => `<p class="empty-line">${text}</p>`;
function renderPanel() {
  if (S.boot !== "ready") {
    $("panel-body").innerHTML = ["在做的事", "带上的记忆", "在后台"].map((t, i) => card(`sk${i}`, t, "", S.boot === "error" ? empty("没取到。") : skeletonRows(2, "sk-row in-card"))).join("");
    return;
  }
  const v = S.view;
  const stateName = { active: "进行中", waiting: "等待中", parked: "搁置", managed: "在管" };
  const matters = S.matters.length
    ? S.matters.map((m) => `<div class="row">${icon(m.kind === "area" ? "clock" : "target")}<div class="main-t"><div class="line1"><span class="what">${esc(m.title)}</span><span class="pill${m.state === "waiting" ? " wait" : ""}">${esc(stateName[m.state] || m.state)}</span></div>
        ${m.next || m.waiting_on ? `<div class="line2 one">${esc(m.waiting_on ? `在等 ${m.waiting_on}` : `下一步 ${m.next}`)}</div>` : ""}</div></div>`).join("")
    : empty("没有进行中的目标或在管的领域。");

  const uses = v?.recall || [];
  const activeCount = [...S.memories.values()].filter((m) => m.state === "active").length;
  const memory = (uses.length
    ? uses.map((u) => {
        const m = S.memories.get(u.memory);
        return `<div class="row"><span class="mote ${m?.application === "convention" ? "full" : "ring"}" aria-hidden="true"></span><div class="main-t">${m ? esc(m.content) : esc(u.memory)}
          <div class="line2">${m ? `${m.application === "convention" ? "约定" : "线索"} · 用过 ${m.usage?.count ?? 0} 次 · v${u.revision}` : "库里已经没有这条"}</div></div></div>`;
      }).join("")
    : empty(v?.status === "loading" ? "正在读这段对话。" : "最近这一轮没有带上记忆。")) + `<a class="card-foot" href="#">全部 ${activeCount} 条生效的记忆</a>`;

  const ctx = contextCard(v);

  // 只列这段对话派出去的任务会话：在跑的在前，结束的给出用了多久
  const kids = S.sessions.filter((s) => s.kind === "task" && s.home === S.current).sort((a, b) => running(b.session) - running(a.session) || new Date(b.created_at) - new Date(a.created_at));
  const runningKids = kids.filter((k) => running(k.session));
  const bg = kids.length
    ? kids.map((k) => {
        const live = running(k.session);
        return `<div class="row">${live ? `<span class="spinner sm row-spin" aria-label="运行中"></span>` : icon("term")}<div class="main-t"><button class="row-link" data-session="${k.session}">${esc(titleOf(k))}</button>
          <div class="line2 num">${live ? `已跑 ${elapsed(k.created_at)}` : `已结束 · ${ago(k.updated_at)}`}</div></div></div>`;
      }).join("")
    : empty("这段对话没有派出后台任务。");

  const h = S.health;
  const conn = `<div class="row">${icon("plug")}<div class="main-t">Gateway ${esc(h.version)}<div class="line2 num">启动于 ${new Date(h.started_at).toLocaleString("zh-CN", { hour12: false })}</div></div></div>
    <div class="row">${icon("plug")}<div class="main-t">遥测<div class="line2">${h.telemetry?.last_error ? `出错：${esc(h.telemetry.last_error)}` : h.telemetry?.last_ok_at ? `正常，最近一次 ${ago(h.telemetry.last_ok_at)}` : "未配置"}</div></div></div>
    <p class="empty-line">渠道和 MCP 的连接状态，Gateway 还没有接口可查。</p>`;

  $("panel-body").innerHTML =
    card("c-matters", "在做的事", S.matters.length, matters) +
    card("c-memory", "带上的记忆", uses.length, memory) +
    card("c-context", "上下文", ctx.value, ctx.body, false) +
    card("c-bg", "在后台", runningKids.length ? `${runningKids.length} 个在跑` : kids.length, bg) +
    card("c-conn", "连接", h.telemetry?.last_error ? "有问题" : "正常", conn, false);
}

/* ---------- 总渲染 ---------- */
let frame = 0;
function render({ stick = false, force = false } = {}) {
  cancelAnimationFrame(frame);
  frame = requestAnimationFrame(() => {
    const sc = $("scroll");
    const atBottom = sc.scrollHeight - sc.scrollTop - sc.clientHeight < 80;
    renderGroups(); renderConn(); renderThread(); renderComposer(); renderPanel();
    if ((stick && atBottom) || force) { sc.scrollTop = sc.scrollHeight; }
    if (S.view && S.view.status === "ready" && !S.view.scrolled) { sc.scrollTop = sc.scrollHeight; S.view.scrolled = true; }
  });
}
setInterval(() => { if (S.view?.runs.size || S.sessions.some((s) => running(s.session))) render(); }, 1000);

/* ---------- 交互 ---------- */
const app = $("app");
const wide = () => innerWidth > 1180;
function syncPanelButton() {
  const open = wide() ? !app.classList.contains("panel-closed") : app.classList.contains("panel-open");
  $("toggle-panel").setAttribute("aria-expanded", open);
  $("toggle-panel").setAttribute("aria-label", open ? "收起右栏" : "展开右栏");
}
const routeSession = () => (location.hash.match(/^#\/s\/([\w-]+)/) || [])[1];
document.addEventListener("click", (ev) => {
  const t = ev.target;
  const fold = t.closest("[data-fold]");
  if (fold) { const id = fold.dataset.fold; S.open.has(id) ? S.open.delete(id) : S.open.add(id); render(); return; }
  const turnBtn = t.closest("[data-turn]");
  if (turnBtn) {
    const id = turnBtn.dataset.turn, open = turnBtn.getAttribute("aria-expanded") === "true";
    S.open.delete(id); S.open.delete(`${id}:closed`);
    S.open.add(open ? `${id}:closed` : id);
    render(); return;
  }
  const copy = t.closest("[data-copy]");
  if (copy) { navigator.clipboard?.writeText(copy.dataset.copy); copy.classList.add("done"); setTimeout(() => copy.classList.remove("done"), 1200); return; }
  if (t.closest("#fork")) { forkSession(); return; }
  const ctxBtn = t.closest("[data-fold-ctx]");
  if (ctxBtn) { const id = ctxBtn.dataset.foldCtx; S.open.has(id) ? S.open.delete(id) : S.open.add(id); render(); return; }
  const cardBtn = t.closest("[data-card]");
  if (cardBtn) { const id = `card:${cardBtn.dataset.card}`; S.open.has(id) ? S.open.delete(id) : S.open.add(id); render(); return; }
  const group = t.closest("[data-group]");
  if (group) { const k = group.dataset.group; S.closed.has(k) ? S.closed.delete(k) : S.closed.add(k); saveClosed(); render(); return; }
  const more = t.closest("[data-show-more]");
  if (more) { const k = more.dataset.showMore; S.showMore.has(k) ? S.showMore.delete(k) : S.showMore.add(k); render(); return; }
  const newIn = t.closest("[data-new-in]");
  if (newIn) { newSession(newIn.dataset.newIn || undefined); return; }
  const set = t.closest("[data-set]");
  if (set) {
    S[set.dataset.set] = set.dataset.value; S.showMore.clear();
    localStorage.setItem(`komo.${set.dataset.set}`, set.dataset.value);
    setMenu(false); $("view-menu-btn").focus(); render(); return;
  }
  if (t.closest("#view-menu-btn")) { setMenu($("view-menu").hidden); return; }
  if (t.closest("#search-toggle")) { setSearch($("sec-search").hidden); return; }
  if (t.closest("#nav-search")) { ev.preventDefault(); setSearch(true); return; }
  const kt = t.closest("[data-kids]");
  if (kt) { const id = kt.dataset.kids; S.navOpen.has(id) ? S.navOpen.delete(id) : S.navOpen.add(id); saveNav(); render(); return; }
  const sess = t.closest("[data-session]");
  if (sess) { openSession(sess.dataset.session); app.classList.remove("nav-open", "panel-open"); syncPanelButton(); return; }

  if (t.closest("[data-retry-boot]")) boot();
  if (t.closest("[data-retry-session]")) openSession(S.current);
  const rs = t.closest("[data-retry-send]");
  if (rs) retry(rs.dataset.retrySend);
  if (t.closest("#new-session")) newSession();
  if (t.closest("#stop-run")) cancelRuns();
  if (t.closest("#toggle-panel")) { app.classList.toggle(wide() ? "panel-closed" : "panel-open"); syncPanelButton(); }
  if (t.closest("#open-nav")) app.classList.add("nav-open");
  if (t.closest("#close-nav, #close-panel, #scrim")) { app.classList.remove("nav-open", "panel-open"); syncPanelButton(); }
});
// 点在菜单和它的按钮之外就关菜单（先于其他点击处理）
document.addEventListener("click", (ev) => { if (!$("view-menu").hidden && !ev.target.closest("#view-menu, #view-menu-btn")) setMenu(false); }, true);
document.addEventListener("keydown", (ev) => {
  if (ev.key === "Escape") {
    if (!$("view-menu").hidden) { setMenu(false); $("view-menu-btn").focus(); return; }
    if (document.activeElement === $("search-input")) { setSearch(false); return; }
    app.classList.remove("nav-open", "panel-open"); syncPanelButton();
  }
  if ((ev.metaKey || ev.ctrlKey) && ev.key.toLowerCase() === "k") { ev.preventDefault(); if (innerWidth <= 900) app.classList.add("nav-open"); setSearch(true); }
  if (!$("view-menu").hidden && (ev.key === "ArrowDown" || ev.key === "ArrowUp")) {
    ev.preventDefault();
    const items = [...$("view-menu").querySelectorAll(".menu-item")], i = items.indexOf(document.activeElement);
    items[(i + (ev.key === "ArrowDown" ? 1 : -1) + items.length) % items.length].focus();
  }
});
$("composer").addEventListener("submit", (ev) => {
  ev.preventDefault();
  const text = $("input").value;
  if (!text.trim()) return;
  $("input").value = ""; $("input").style.height = "auto";
  send(text);
});
$("input").addEventListener("keydown", (ev) => {
  if (ev.key === "Enter" && !ev.shiftKey && !ev.isComposing) { ev.preventDefault(); $("composer").requestSubmit(); }
});
$("input").addEventListener("input", (ev) => { ev.target.style.height = "auto"; ev.target.style.height = ev.target.scrollHeight + "px"; renderComposer(); });
$("model").addEventListener("change", fillEfforts);
$("search-input").addEventListener("input", (ev) => { S.query = ev.target.value.trim(); render(); });
addEventListener("resize", syncPanelButton);
addEventListener("hashchange", () => { const id = routeSession(); if (id && id !== S.current) openSession(id); });

syncPanelButton();
boot();
