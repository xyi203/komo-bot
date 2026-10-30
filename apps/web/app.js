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
  page: "session",        // session | memory
  mem: { status: "idle", error: "", bucket: "live", scope: "", query: "", hits: null, degraded: "", selected: null, index: null, mode: null, busy: "", fail: "", confirmForget: false, classify: false },
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
    const [health, sessions, home, models, matters, memories, inbox] = await Promise.all([
      api("/healthz"), api("/v1/sessions?all=1"), api("/v1/home-session"), api("/v1/models"),
      api("/v1/matters"), api("/v1/memories"), api("/v1/interventions").catch(() => null),
    ]);
    S.inbox = inbox ? inbox.interventions : null;
    Object.assign(S, {
      health, sessions: sessions.sessions, homeId: home.session, models: models.models, matters: matters.matters,
      boot: "ready", lastGood: new Date(),
      memories: new Map(memories.memories.map((m) => [m.id, m])),
    });
    fillModels();
    render();
    loadTaskDetails();
    const sec = routeSettings(), r = routePage();
    if (r) openPage(r.page, r.id);
    else openSession(routeSession() || S.homeId);
    if (sec) openSettings(sec);
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
  S.page = "session";
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
    const [sessions, matters, inbox] = await Promise.allSettled([api("/v1/sessions?all=1"), api("/v1/matters"), api("/v1/interventions")]);
    if (sessions.status === "fulfilled") S.sessions = sessions.value.sessions;
    if (matters.status === "fulfilled") S.matters = matters.value.matters;
    if (inbox.status === "fulfilled") S.inbox = inbox.value.interventions;
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
  return `<li><button class="session${child ? " child" : ""}${archived(s) ? " is-archived" : ""}" data-session="${s.session}" ${S.page === "session" && s.session === S.current ? 'aria-current="true"' : ""} title="${esc(titleOf(s))}">
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
  const row = `<li><div class="session parent${archived(s) ? " is-archived" : ""}" data-session="${s.session}" ${S.page === "session" && s.session === S.current ? 'aria-current="true"' : ""}>
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
  const stateName = { active: "进行中", waiting: "在等", parked: "搁置" };
  const matters = S.matters.length
    ? S.matters.map((m) => `<div class="row">${icon(m.kind === "area" ? "clock" : "target")}<div class="main-t"><div class="line1"><span class="what">${esc(m.title)}</span>${m.kind === "goal" ? `<span class="pill${m.state === "waiting" ? " wait" : ""}">${esc(stateName[m.state] || m.state)}</span>` : ""}</div>
        ${m.next || m.waiting_for || m.standard ? `<div class="line2 one">${esc(m.waiting_for ? `在等 ${m.waiting_for}` : m.next ? `下一步 ${m.next}` : `标准 ${m.standard}`)}</div>` : ""}</div></div>`).join("")
    : empty("没有进行中的目标或在管的领域。");
  const mattersFoot = `<a class="card-foot" href="#/matters">全部事项</a>`;

  const uses = v?.recall || [];
  const activeCount = [...S.memories.values()].filter((m) => m.state === "active").length;
  const memory = (uses.length
    ? uses.map((u) => {
        const m = S.memories.get(u.memory);
        return `<div class="row"><span class="mote ${m?.application === "convention" ? "full" : "ring"}" aria-hidden="true"></span><div class="main-t">${m ? esc(m.content) : esc(u.memory)}
          <div class="line2">${m ? `${m.application === "convention" ? "约定" : "线索"} · 用过 ${m.usage?.count ?? 0} 次 · v${u.revision}` : "库里已经没有这条"}</div></div></div>`;
      }).join("")
    : empty(v?.status === "loading" ? "正在读这段对话。" : "最近这一轮没有带上记忆。")) + `<a class="card-foot" href="#/memory">全部 ${activeCount} 条生效的记忆</a>`;

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
    card("c-matters", "在做的事", S.matters.length, matters + mattersFoot) +
    card("c-memory", "带上的记忆", uses.length, memory) +
    card("c-context", "上下文", ctx.value, ctx.body, false) +
    card("c-bg", "在后台", runningKids.length ? `${runningKids.length} 个在跑` : kids.length, bg) +
    card("c-conn", "连接", h.telemetry?.last_error ? "有问题" : "正常", conn, false);
}

/* ---------- 记忆页 ----------
   一屏回答：komo 现在把哪些关于我的事带进对话，哪些等我过目。
   上面是"在场光阶"：每条记忆一个光点，按离 prompt 的远近分四档，兼作筛选；下面列表 + 详情。
   列表是库存（GET /v1/memories，带归档的），检索走同一接口带 query（混合检索，降级如实写）。 */
const RUNGS = [
  ["always", "每次都带", "约定", (m) => m.state === "active" && m.application === "convention"],
  ["relevant", "相关才带", "线索", (m) => m.state === "active" && m.application !== "convention"],
  ["paused", "暂停", "候选与有争议", (m) => m.state === "candidate" || m.state === "contested"],
  ["out", "不再带", "已不生效", (m) => ["forgotten", "superseded", "archived"].includes(m.state)],
];
const rungOf = (m) => RUNGS.find((r) => r[3](m))?.[0];
const inBucket = (m, b) => (b === "live" ? m.state === "active" : rungOf(m) === b);
const STATE_NAME = { active: "生效", candidate: "候选", contested: "有争议", superseded: "被取代", forgotten: "已遗忘", archived: "已归档" };
const KIND_NAME = { preference: "偏好", fact: "事实", experience: "经验" };
const PROV_NAME = { user_statement: "用户原话", tool_observation: "工具观察", model_inference: "模型推断" };
const useName = (m) => (m.application === "convention" ? "约定" : m.application === "reference" ? "线索" : "线索（未分类）");
const scopeKey = (s) => (s.kind === "personal" ? "personal" : `${s.kind}:${s.project_id || s.instance_id || s.agent_id || ""}`);
const scopeName = (s) => (s.kind === "personal" ? "个人" : s.kind === "project" ? s.project_id : s.kind === "environment" ? `环境 ${s.instance_id}` : `Agent ${s.agent_id}`);
const moteClass = (m) => (m.state === "active" ? (m.application === "convention" ? "full" : "ring") : m.state === "contested" ? "alert" : m.state === "candidate" ? "pale" : "off");
const day = (iso) => { const d = new Date(iso); return `${d.getMonth() + 1}月${d.getDate()}日`; };
const memList = () => [...S.memories.values()];

async function openMemory(id = null) {
  if (source) { source.close(); source = null; }
  S.page = "memory";
  const M = S.mem;
  M.selected = id; M.confirmForget = false; M.classify = false; M.fail = "";
  if (id && S.memories.get(id)) M.bucket = inBucket(S.memories.get(id), "live") ? "live" : rungOf(S.memories.get(id));
  setMemHash();
  render();
  M.status = "loading"; render();
  try {
    const [list, index, all] = await Promise.all([api("/v1/memories?include_archived=true&limit=500"), api("/v1/memory-index").catch(() => null), api("/v1/matters?all=true").catch(() => null)]);
    if (all) S.allMatters = all.matters;
    S.memories = new Map(list.memories.map((m) => [m.id, m]));
    M.index = index; M.status = "ready"; S.lastGood = new Date();
    if (M.selected && !S.memories.has(M.selected)) M.selected = null;
    if (M.selected) M.bucket = inBucket(S.memories.get(M.selected), "live") ? "live" : rungOf(S.memories.get(M.selected));
  } catch (err) {
    M.status = "error"; M.error = err.message;
  }
  render();
}
function setMemHash() {
  const want = S.mem.selected ? `#/memory/${S.mem.selected}` : "#/memory";
  if (location.hash !== want) history.replaceState(null, "", want);
}
function memRows() {
  const M = S.mem;
  let rows = M.hits ?? memList().filter((m) => inBucket(m, M.bucket));
  if (M.scope) rows = rows.filter((m) => scopeKey(m.scope) === M.scope);
  if (M.hits) return rows; // 检索结果保持检索的排序
  // 在起作用的：约定在前，再按带上次数；不再带的：最近变动的在前
  return rows.sort((a, b) =>
    M.bucket === "out" ? new Date(b.updated_at) - new Date(a.updated_at)
      : (b.application === "convention") - (a.application === "convention") || (b.usage?.count || 0) - (a.usage?.count || 0));
}
function memFinding() {
  const all = memList();
  const contested = all.filter((m) => m.state === "contested").length;
  const candidate = all.filter((m) => m.state === "candidate").length;
  const live = all.filter((m) => m.state === "active");
  const unseen = live.filter((m) => m.confirmation !== "user_confirmed").length;
  if (contested) return `${contested} 条有冲突，暂停带上，等你定哪条算数。`;
  if (candidate) return `${candidate} 条候选还没进召回，等你看。`;
  if (!live.length) return "现在没有生效的记忆：对话空闲后，komo 会从里面整理。";
  if (!unseen) return `在起作用的 ${live.length} 条你都确认过。`;
  return unseen === live.length ? `在起作用的 ${live.length} 条都还没过目。` : `在起作用的 ${live.length} 条里，${unseen} 条还没过目。`;
}
/* 攒下的记忆：横轴是记下的日子（跨度超过 60 天按周并），一天一格，格里一个点是一条记忆。
   颜色是在场程度（同光阶），点的面积按带进对话的次数（开方）；格内从左下往上排，在起作用的在下面。
   图例就是筛选：选中一档时其余点淡下去，列表跟着换。 */
const VIZ_CELL = 22;
function memChart(all, M, width) {
  if (!all.length) return `<p class="empty-frame">还没有记忆：对话空闲一会儿后，komo 会从里面整理出偏好、事实和经验。</p>`;
  const today = ymd(new Date());
  const first = all.reduce((a, m) => (ymd(new Date(m.created_at)) < a ? ymd(new Date(m.created_at)) : a), today);
  const spanDays = dayDiff(today, first) + 1, bin = spanDays > 60 ? 7 : 1;
  const nCols = Math.max(1, Math.ceil(spanDays / bin));
  const padL = 4, padR = 4, axisH = 26, W = Math.max(280, width);
  const colW = (W - padL - padR) / nCols;
  const per = Math.max(1, Math.min(4, Math.floor((colW - 6) / VIZ_CELL))); // 每格最多四列，往上堆成柱
  const maxUse = Math.max(1, ...all.map((m) => m.usage?.count || 0));
  const rank = { always: 0, relevant: 1, paused: 2, out: 3 };
  const cols = Array.from({ length: nCols }, () => []);
  for (const m of all) cols[Math.min(nCols - 1, Math.floor(dayDiff(ymd(new Date(m.created_at)), first) / bin))].push(m);
  for (const c of cols) c.sort((a, b) => rank[rungOf(a)] - rank[rungOf(b)] || (b.usage?.count || 0) - (a.usage?.count || 0));
  const rows = Math.max(1, ...cols.map((c) => Math.ceil(c.length / per)));
  const H = 26 + rows * VIZ_CELL + axisH, base = H - axisH;
  const lit = (m) => !M.hits && (M.bucket === "live" ? m.state === "active" : rungOf(m) === M.bucket);
  const dots = cols.map((c, ci) => {
    const x0 = padL + ci * colW + (colW - Math.min(c.length, per) * VIZ_CELL) / 2;
    return c.map((m, i) => {
      const cx = x0 + (i % per) * VIZ_CELL + VIZ_CELL / 2, cy = base - Math.floor(i / per) * VIZ_CELL - VIZ_CELL / 2 - 2;
      const r = 3.5 + 5.5 * Math.sqrt((m.usage?.count || 0) / maxUse);
      const k = moteClass(m);
      return `<circle class="vz ${k}${lit(m) ? "" : " dim"}${m.id === M.selected ? " sel" : ""}" cx="${cx.toFixed(1)}" cy="${cy.toFixed(1)}" r="${r.toFixed(1)}" data-mem="${m.id}"><title>${esc(m.content)}（${STATE_NAME[m.state]}，带上 ${m.usage?.count || 0} 次）</title></circle>`;
    }).join("");
  }).join("");
  const tops = cols.map((c, ci) => (c.length ? `<text class="tally" x="${(padL + (ci + 0.5) * colW).toFixed(1)}" y="${(base - Math.ceil(c.length / per) * VIZ_CELL - 8).toFixed(1)}" text-anchor="middle">+${c.length}</text>` : "")).join("");
  const step = Math.ceil(nCols / Math.max(2, Math.floor(W / 84)));
  const labels = cols.map((_, ci) => {
    if (ci % step && ci !== nCols - 1) return "";
    const d = new Date(`${first}T00:00:00`); d.setDate(d.getDate() + ci * bin);
    const text = ci === nCols - 1 && bin === 1 ? "今天" : `${d.getMonth() + 1}月${d.getDate()}日${bin > 1 ? "那周" : ""}`;
    return `<text x="${(padL + (ci + 0.5) * colW).toFixed(1)}" y="${H - 8}" text-anchor="middle">${text}</text>`;
  }).join("");
  const seams = cols.map((_, ci) => (ci ? `<line x1="${(padL + ci * colW).toFixed(1)}" x2="${(padL + ci * colW).toFixed(1)}" y1="8" y2="${base}" class="seam"/>` : "")).join("");
  return `<svg class="viz" viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" role="img" aria-label="${all.length} 条记忆按记下的日子排开">
    ${seams}<line x1="0" x2="${W}" y1="${base}" y2="${base}" class="axis"/>${dots}${tops}${labels}</svg>`;
}
function renderLadder() {
  const M = S.mem, all = memList(), el = $("mem-ladder");
  const loading = M.status !== "ready";
  const tabs = RUNGS.map(([key, label, sub, test]) => {
    const n = all.filter(test).length;
    const on = !M.hits && (M.bucket === key || (M.bucket === "live" && (key === "always" || key === "relevant")));
    const sw = { always: "full", relevant: "ring", paused: "pale", out: "off" }[key];
    return `<button class="rung" role="tab" aria-selected="${on}" data-rung="${key}" title="${sub}" ${loading ? "disabled" : ""}>
      <span class="mote ${sw}" aria-hidden="true"></span><span class="rung-label">${label}</span><span class="rung-val num${n ? "" : " zero"}">${loading ? "—" : n}</span></button>`;
  }).join("");
  const width = Math.round((el.clientWidth || 1000) - 2 * 20);
  el.innerHTML = `<div class="viz-head"><div><h2 class="viz-title">攒下的记忆</h2><p class="viz-note">一个点一条记忆，按记下的日子排；点越大，带进对话的次数越多。</p></div>
      <div class="rungs" role="tablist" aria-label="按离对话的远近筛选">${tabs}</div></div>
    ${loading ? `<div class="viz-sk"><span class="skeleton" style="width:100%;height:96px"></span></div>` : memChart(all, M, width)}`;
}
function rowAction(m) {
  if (m.state === "forgotten") return `<button class="text-btn" data-mem-act="restore" data-id="${m.id}">恢复</button>`;
  if (m.state !== "active" && m.state !== "candidate" && m.state !== "contested") return `<span class="muted">—</span>`;
  if (m.confirmation === "user_confirmed") return `<span class="confirmed">${icon("check")}已确认</span>`;
  return `<button class="btn ghost sm" data-mem-act="confirm" data-id="${m.id}" title="确认第 ${m.revision} 版的文字">确认</button>`;
}
function renderMemRows() {
  const M = S.mem;
  if (M.status === "error") {
    $("mem-rows").innerHTML = `<div class="block-error"><p>记忆没取到：${esc(M.error)}。</p><button class="text-btn" data-mem-reload>重试</button></div>`;
    return;
  }
  if (M.status !== "ready") { $("mem-rows").innerHTML = skeletonRows(5, "sk-row mrow-sk"); return; }
  const rows = memRows();
  const head = `<div class="mhead" aria-hidden="true"><span></span><span>内容</span><span class="r">带上 / 用上</span><span class="r">最近带上</span><span></span></div>`;
  const none = M.hits ? "没有检索到相关的记忆。" : { live: "现在没有生效的记忆。", always: "还没有约定：你说「以后都…」「默认…」这类话时，komo 会记成约定。", relevant: "没有生效的线索。", paused: "没有候选或有争议的记忆。", out: "没有遗忘、被取代或归档的记忆。" }[M.bucket];
  $("mem-rows").innerHTML = rows.length
    ? head + `<ul class="mrows">${rows.map((m) => {
        const sub = [useName(m), m.application === "convention" && m.applies_when ? m.applies_when : null, scopeName(m.scope), m.state !== "active" ? STATE_NAME[m.state] : null].filter(Boolean);
        return `<li class="mrow${m.id === M.selected ? " on" : ""}">
          <span class="mote ${moteClass(m)}" aria-hidden="true"></span>
          <button class="mrow-main" data-mem="${m.id}" ${m.id === M.selected ? 'aria-current="true"' : ""}><span class="mtext">${esc(m.content)}</span><span class="msub">${sub.map(esc).join('<span class="sep">·</span>')}</span></button>
          <span class="mnum num r">${m.usage?.count || 0}<small>${m.usage?.adopted_count ? `用上 ${m.usage.adopted_count}` : "用上 0"}</small></span>
          <span class="mwhen num r">${m.usage?.last_used_at ? ago(m.usage.last_used_at) : `<span class="muted">—</span>`}</span>
          <span class="mact">${rowAction(m)}</span></li>`;
      }).join("")}</ul>`
    : `<p class="empty-frame">${none}</p>`;
}
function evidenceRow(e) {
  const r = e.reference || {};
  const when = `${day(e.observed_at)} ${clock(new Date(e.observed_at))}`;
  if (r.kind === "event") {
    const s = S.sessions.find((x) => x.session === r.session);
    return `<li class="ev"><span class="ev-what">${esc(PROV_NAME[e.provenance] || e.provenance)}<span class="sep">·</span><span class="num">${when}</span></span>
      ${s ? `<a class="ev-link" href="#/s/${r.session}">${esc(titleOf(s))}${icon("right")}</a>` : `<span class="muted">会话已不在列表里</span>`}</li>`;
  }
  return `<li class="ev"><span class="ev-what">Memos 记录<span class="sep">·</span><span class="num">${when}</span></span><span class="muted">${esc(r.instance)} / ${esc(r.record_id)}</span></li>`;
}
function addForm() {
  const scopes = new Map([["personal", "个人"]]);
  for (const m of memList()) scopes.set(scopeKey(m.scope), scopeName(m.scope));
  return `<form class="mform" id="mem-form">
    <div class="detail-top"><h2 class="detail-h">记一条</h2><button type="button" class="icon-btn" data-mem-close aria-label="关闭">${icon("x")}</button></div>
    <p class="hint">你在这里记的算你亲口说的，直接记成已确认。</p>
    <label class="lbl" for="f-content">内容</label>
    <textarea id="f-content" name="content" rows="3" maxlength="300" required placeholder="一句话，单一的一件事"></textarea>
    <div class="two">
      <div><label class="lbl" for="f-kind">种类</label><select class="select" id="f-kind" name="kind"><option value="preference">偏好</option><option value="fact">事实</option><option value="experience">经验</option></select></div>
      <div><label class="lbl" for="f-scope">作用域</label><select class="select" id="f-scope" name="scope">${[...scopes].map(([k, n]) => `<option value="${esc(k)}">${esc(n)}</option>`).join("")}</select></div>
    </div>
    <fieldset class="seg"><legend class="lbl">怎么用</legend>
      <label><input type="radio" name="application" value="reference" checked>线索：查找与判断时参考</label>
      <label><input type="radio" name="application" value="convention">约定：条件成立就照做</label>
    </fieldset>
    <div id="f-when-wrap" hidden><label class="lbl" for="f-when">什么时候适用</label><input class="input" id="f-when" name="applies_when" placeholder="例：创建发布工单、需要选 Platform 时"></div>
    ${S.mem.fail ? `<p class="form-err">${esc(S.mem.fail)}</p>` : ""}
    <div class="actions"><button class="btn primary" type="submit" ${S.mem.busy ? "disabled" : ""}>${S.mem.busy ? "正在记" : "记下"}</button><button class="btn" type="button" data-mem-close>取消</button></div>
  </form>`;
}
function renderMemDetail() {
  const M = S.mem, el = $("mem-detail");
  if (M.selected === "new") { el.innerHTML = addForm(); return; }
  const m = M.selected && S.memories.get(M.selected);
  if (M.status !== "ready" && !m) { el.innerHTML = M.status === "error" ? "" : `<div class="detail-pad">${skeletonRows(4, "sk-row in-card")}</div>`; return; }
  if (!m) { el.innerHTML = `<p class="empty-frame">选一条，看它从哪来、带上过几次。</p>`; return; }
  const live = m.state === "active" || m.state === "candidate" || m.state === "contested";
  const unconfirmed = m.confirmation !== "user_confirmed";
  const matter = m.belongs_to && (S.allMatters || S.matters).find((x) => x.id === m.belongs_to);
  const kv = [
    ["作用域", esc(scopeName(m.scope))],
    ["种类", esc(KIND_NAME[m.kind] || m.kind)],
    ["来源", `${esc(PROV_NAME[m.provenance] || m.provenance)}${m.provenance === "user_statement" && unconfirmed ? "，自动整理" : ""}`],
    ["版本", `第 <span class="num">${m.revision}</span> 版 · ${unconfirmed ? "未确认" : "已确认"}`],
    ["带上", m.usage?.count ? `<span class="num">${m.usage.count}</span> 次 · 最近 <span class="num">${ago(m.usage.last_used_at)}</span>` : `<span class="num">0</span> 次`],
    ["用上", `<span class="num">${m.usage?.adopted_count || 0}</span> 次${m.usage?.last_adopted_at ? ` · 最近 <span class="num">${ago(m.usage.last_adopted_at)}</span>` : ""}`],
    ["记下", `<span class="num">${day(m.created_at)}</span> · <span class="mono">${esc(m.extraction?.model || "")}</span>${m.extraction?.prompt_version ? ` · <span class="mono">${esc(m.extraction.prompt_version)}</span>` : ""}`],
    ["归属", matter ? `<a class="ev-link" href="#/matters/${matter.id}">${esc(matter.title)}</a>${matter.closed_at ? `<span class="muted">（已结案）</span>` : ""}` : m.belongs_to ? `<span class="muted">事项已不在</span>` : `<span class="muted">—</span>`],
  ];
  if (m.valid_until) kv.push(["有效到", `<span class="num">${day(m.valid_until)}</span>`]);
  const link = (ref, verb) => ref ? `${verb} <a class="ev-link" href="#/memory/${ref.memory}">${S.memories.has(ref.memory) ? esc(S.memories.get(ref.memory).content.slice(0, 24)) + "…" : "另一条"}</a>` : "";
  const chain = [link(m.supersedes, "取代了"), link(m.superseded_by, "被这条取代：")].filter(Boolean).join("<br>");
  const ev = m.evidence || [];
  const classifyTo = m.application === "convention" ? "reference" : "convention";
  const classify = M.classify
    ? `<form class="mform inline" id="classify-form">
        <label class="lbl" for="c-when">什么时候适用</label>
        <input class="input" id="c-when" name="applies_when" required value="${esc(m.applies_when || "")}" placeholder="例：创建发布工单、需要选 Platform 时">
        <div class="actions"><button class="btn primary" type="submit" ${M.busy ? "disabled" : ""}>改成约定</button><button class="btn" type="button" data-mem-act="classify-cancel">取消</button></div></form>`
    : "";
  const forget = M.confirmForget
    ? `<div class="danger"><p>遗忘后这条不再带进对话，同一来源也不会再被整理出来。原对话不删。</p>
        <div class="actions"><button class="btn danger-btn" data-mem-act="forget" data-id="${m.id}" ${M.busy ? "disabled" : ""}>遗忘第 ${m.revision} 版</button><button class="btn" data-mem-act="forget-cancel">取消</button></div></div>`
    : "";
  const acts = live
    ? `<div class="actions">
        ${unconfirmed ? `<button class="btn primary" data-mem-act="confirm" data-id="${m.id}" ${M.busy ? "disabled" : ""}>确认第 ${m.revision} 版</button>` : ""}
        ${m.state === "active" && !M.classify ? (classifyTo === "convention" && m.kind !== "preference" ? "" : `<button class="btn" data-mem-act="${classifyTo === "convention" ? "classify-open" : "classify-ref"}" data-id="${m.id}" ${M.busy ? "disabled" : ""}>改成${classifyTo === "convention" ? "约定" : "线索"}</button>`) : ""}
      </div>
      ${classify}
      ${M.confirmForget ? forget : `<button class="text-btn danger-link" data-mem-act="forget-ask">遗忘这条</button>`}`
    : m.state === "forgotten"
      ? `<div class="actions"><button class="btn primary" data-mem-act="restore" data-id="${m.id}" ${M.busy ? "disabled" : ""}>恢复</button></div>`
      : `<p class="hint">${m.state === "archived" ? "所属的事结案后归档；重开那件事时恢复。" : "被新的一条取代，不再带上。"}</p>`;
  el.innerHTML = `<div class="detail-pad">
    <div class="detail-top"><button class="icon-btn back" data-mem-close aria-label="回到列表">${icon("left")}</button>
      <span class="detail-kind"><span class="mote ${moteClass(m)}" aria-hidden="true"></span>${esc(useName(m))}</span>
      <span class="pill${m.state === "contested" ? " bad" : m.state === "active" ? "" : " off"}">${STATE_NAME[m.state] || m.state}</span></div>
    <p class="detail-text">${esc(m.content)}</p>
    ${m.applies_when ? `<p class="detail-when"><span class="muted">适用于</span>${esc(m.applies_when)}</p>` : ""}
    ${M.fail ? `<p class="form-err">${esc(M.fail)}</p>` : ""}
    ${acts}
    <dl class="kv">${kv.map(([k, v]) => `<dt>${k}</dt><dd>${v}</dd>`).join("")}</dl>
    ${chain ? `<p class="hint chain">${chain}</p>` : ""}
    <h3 class="sub-h">证据 <span class="num muted">${ev.length}</span></h3>
    ${ev.length ? `<ul class="evs">${ev.map(evidenceRow).join("")}</ul>` : `<p class="hint">没有事件证据：这条是你在终端或这里亲手记的。</p>`}
  </div>`;
}
function renderMemory() {
  const M = S.mem;
  $("mem-finding").textContent = M.status === "ready" ? memFinding() : M.status === "error" ? "记忆没取到。" : " ";
  renderLadder();
  const n = M.status === "ready" ? memRows().length : null;
  const title = M.hits ? `检索「${M.query}」` : { live: "在起作用", always: "每次都带", relevant: "相关才带", paused: "暂停", out: "不再带" }[M.bucket];
  $("mem-list-title").innerHTML = `${esc(title)}${n == null ? "" : ` <span class="muted">${n} 条</span>`}`;
  const scopes = new Map();
  for (const m of memList()) scopes.set(scopeKey(m.scope), scopeName(m.scope));
  $("mem-scope").innerHTML = `<option value="">全部作用域</option>` + [...scopes].sort((a, b) => (a[0] === "personal" ? -1 : b[0] === "personal" ? 1 : a[1].localeCompare(b[1]))).map(([k, nme]) => `<option value="${esc(k)}" ${k === M.scope ? "selected" : ""}>${esc(nme)}</option>`).join("");
  renderMemRows();
  // 详情：宽屏常驻右侧，默认选中列表第一条；窄屏是推进去的一层
  if (wide() && M.status === "ready" && !M.selected && memRows().length) M.selected = memRows()[0].id;
  renderMemDetail();
  app.classList.toggle("detail-open", !!M.selected);
  const ix = M.index;
  $("mem-index").innerHTML = M.degraded
    ? `检索降级了：${esc(M.degraded)}`
    : ix ? `检索用关键词 + 向量 · ${esc(ix.space?.model || "")} · 已索引 <span class="num">${ix.indexed} / ${ix.total}</span> 条生效的${ix.state !== "ready" ? ` · 索引${ix.state === "building" ? "正在建" : "不可用"}` : ""}` : "";
}
async function memAct(act, id) {
  const M = S.mem, m = S.memories.get(id);
  if (!m) return;
  const path = { confirm: "confirm", forget: "forget", restore: "restore" }[act];
  M.busy = act; M.fail = ""; render();
  try {
    const r = await api(`/v1/memories/${id}/${path}`, { method: "POST", body: JSON.stringify({ expected_revision: m.revision, request_key: `web:${act}:${id}:${m.revision}` }) });
    S.memories.set(id, r.memory);
    M.confirmForget = false;
  } catch (err) { M.fail = `没${{ confirm: "确认", forget: "遗忘", restore: "恢复" }[act]}成：${err.message}`; }
  M.busy = ""; render();
}
async function memClassify(id, application, appliesWhen) {
  const M = S.mem, m = S.memories.get(id);
  M.busy = "classify"; M.fail = ""; render();
  const body = { expected_revision: m.revision, application, request_key: `web:classify:${id}:${m.revision}:${application}` };
  if (appliesWhen) body.applies_when = appliesWhen;
  try { const r = await api(`/v1/memories/${id}/classify`, { method: "POST", body: JSON.stringify(body) }); S.memories.set(id, r.memory); M.classify = false; }
  catch (err) { M.fail = `没改成：${err.message}`; }
  M.busy = ""; render();
}
async function memAdd(form) {
  const M = S.mem, f = new FormData(form);
  const key = f.get("scope"), [kind, ...rest] = key.split(":");
  const scope = kind === "personal" ? { kind } : { kind, [{ project: "project_id", environment: "instance_id", agent: "agent_id" }[kind]]: rest.join(":") };
  const body = { content: f.get("content").trim(), kind: f.get("kind"), scope, application: f.get("application"), request_key: `web:add:${crypto.randomUUID()}` };
  if (body.application === "convention") body.applies_when = String(f.get("applies_when") || "").trim();
  M.busy = "add"; M.fail = ""; render();
  try {
    const r = await api("/v1/memories", { method: "POST", body: JSON.stringify(body) });
    S.memories.set(r.memory.id, r.memory);
    M.bucket = "live"; M.hits = null; M.selected = r.memory.id; setMemHash();
  } catch (err) { M.fail = `没记下：${err.message}`; }
  M.busy = ""; render();
}
let memSearchTimer = null;
function memSearch(q) {
  const M = S.mem;
  M.query = q;
  clearTimeout(memSearchTimer);
  if (!q) { M.hits = null; M.degraded = ""; render(); return; }
  memSearchTimer = setTimeout(async () => {
    try {
      const r = await api(`/v1/memories?query=${encodeURIComponent(q)}&include_archived=true&limit=30`);
      if (M.query !== q) return;
      for (const m of r.memories) S.memories.set(m.id, m);
      M.hits = r.memories; M.degraded = r.degraded ? r.degraded_reason || "原因没有给出" : "";
    } catch (err) { M.hits = []; M.degraded = err.message; }
    render();
  }, 250);
}

/* ---------- 页面框架：记忆之外的四页共用 ----------
   每页一个状态块：st（idle | loading | ready | error）、selected（选中的 id，"new" = 新建表单）、
   form（详情里展开的那张小表单）、busy / fail（写操作进行中 / 失败原因）、notice（写完之后的一句回执）。 */
const PAGES = { memory: "mem", matters: "pg-matters", cron: "pg-cron", inbox: "pg-inbox" };
const PAGE_NAME = { memory: "记忆", matters: "事项", cron: "定时", inbox: "待处理" };
const routePage = () => { const m = location.hash.match(/^#\/(memory|matters|cron|inbox)(?:\/([\w-]+))?/); return m ? { page: m[1], id: m[2] || null } : null; };
// 设置是弹窗，平时不占地址；#/settings[/节] 只当入口用，打开后地址换回底下那一页
const routeSettings = () => { const m = location.hash.match(/^#\/settings(?:\/(\w+))?/); return m ? m[1] || "models" : null; };
const pageState = () => ({ st: "idle", error: "", selected: null, form: null, busy: "", fail: "", notice: null, confirm: null });
S.pg = { matters: { ...pageState(), items: [], today: null, showClosed: false }, cron: { ...pageState(), jobs: [], status: new Map() }, inbox: { ...pageState(), items: [] }, settings: { ...pageState(), data: {}, result: {}, edit: null } };
const LOADERS = {
  async matters(P) {
    const [list, mems, cron] = await Promise.all([api("/v1/matters?all=true"), api("/v1/memories?include_archived=true&limit=500").catch(() => null), api("/v1/cron").catch(() => null)]);
    P.items = list.matters; P.today = list.today; S.allMatters = list.matters;
    if (mems) S.memories = new Map(mems.memories.map((m) => [m.id, m]));
    if (cron) { S.pg.cron.jobs = cron.jobs; S.pg.cron.status = new Map(cron.status.map((x) => [x.job, x])); }
  },
  async cron(P) {
    const [list, all] = await Promise.all([api("/v1/cron"), api("/v1/matters?all=true").catch(() => null)]);
    P.jobs = list.jobs; P.status = new Map(list.status.map((x) => [x.job, x]));
    if (all) S.allMatters = all.matters;
  },
  async inbox(P) { P.items = (await api("/v1/interventions")).interventions; S.inbox = P.items; },
  async settings(P) {
    const [health, config, index, cfg, settings, env, mcp] = await Promise.allSettled([api("/healthz"), api("/v1/config/check"), api("/v1/memory-index"), api("/v1/config/models"), api("/v1/config/settings"), api("/v1/config/env"), api("/v1/mcp")]);
    const v = (r) => (r.status === "fulfilled" ? r.value : { error: r.reason?.message || "没取到" });
    P.data = { health: v(health), config: v(config), index: v(index), cfg: v(cfg), settings: v(settings), env: v(env), mcp: v(mcp) };
  },
};
async function openPage(page, id = null) {
  if (page === "memory") return openMemory(id);
  if (source) { source.close(); source = null; }
  S.page = page;
  const P = S.pg[page];
  P.selected = id; P.form = null; P.fail = ""; P.notice = null; P.confirm = null;
  setPageHash();
  P.st = "loading"; render();
  await reloadPage(page);
}
async function reloadPage(page) {
  const P = S.pg[page];
  try { await LOADERS[page](P); P.st = "ready"; S.lastGood = new Date(); }
  catch (err) { P.st = "error"; P.error = err.message; }
  render();
}
function setPageHash() {
  if (S.page === "memory") return setMemHash();
  const id = S.pg[S.page]?.selected;
  const want = id && id !== "new" ? `#/${S.page}/${id}` : `#/${S.page}`;
  if (location.hash !== want) history.replaceState(null, "", want);
}
function selectIn(page, id) {
  const P = S.pg[page];
  if (!wide()) { P.listScroll = $(PAGES[page]).scrollTop; $(PAGES[page]).scrollTop = 0; }
  P.selected = id; P.form = null; P.fail = ""; P.notice = null; P.confirm = null;
  setPageHash(); render();
}
function closeDetail(page) {
  const P = S.pg[page];
  P.selected = null; P.form = null; P.fail = ""; P.notice = null; P.confirm = null;
  setPageHash(); render();
  if (!wide()) requestAnimationFrame(() => requestAnimationFrame(() => { $(PAGES[page]).scrollTop = P.listScroll || 0; }));
}
// 列表块与详情块的公共外壳：出错留在块里，加载是放平的灰条
function listShell(P, body) {
  if (P.st === "error") return `<div class="block-error"><p>没取到：${esc(P.error)}。</p><button class="text-btn" data-pg-reload>重试</button></div>`;
  if (P.st !== "ready") return skeletonRows(5, "sk-row mrow-sk");
  return body();
}
function detailTop(label, pill, pillClass = "") {
  return `<div class="detail-top"><button class="icon-btn back" data-pg-close aria-label="回到列表">${icon("left")}</button>
    <span class="detail-kind">${label}</span>${pill ? `<span class="pill${pillClass}">${pill}</span>` : ""}</div>`;
}
const failLine = (P) => (P.fail ? `<p class="form-err">${esc(P.fail)}</p>` : "");
const kvBlock = (rows) => `<dl class="kv">${rows.filter(Boolean).map(([k, v]) => `<dt>${k}</dt><dd>${v}</dd>`).join("")}</dl>`;
const btn = (label, attrs, cls = "") => `<button class="btn${cls ? ` ${cls}` : ""}" ${attrs}>${label}</button>`;
function finishDetail(page, html) {
  const P = S.pg[page];
  $(`${page}-detail`).innerHTML = html;
  app.classList.toggle("detail-open", !!P.selected);
}
function renderNavCounts() {
  const n = S.inbox?.length || 0;
  $("inbox-count").hidden = !n;
  $("inbox-count").textContent = n;
}
const ymd = (d) => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
const dayDiff = (a, b) => Math.round((Date.parse(`${a}T00:00:00Z`) - Date.parse(`${b}T00:00:00Z`)) / 86400000);
const ymdShort = (s) => { const [, m, d] = s.split("-").map(Number); return `${m}月${d}日`; };

/* ---------- 事项：目标与领域 ----------
   一屏回答：我现在在做什么、在等什么、哪件超期了。目标按超期 → 截止近 → 新建先后排；
   每行右边是为它记的记忆，光点与记忆页同一套。 */
const MATTER_STATE = { active: "在做", waiting: "在等", parked: "搁置", done: "完成", dropped: "放弃", retired: "不再管" };
const closedState = (m) => ["done", "dropped", "retired"].includes(m.state);
function dueOf(m, today) {
  if (!m.due) return null;
  if (closedState(m) || !today) return { text: ymdShort(m.due), sub: "", bad: false };
  const d = dayDiff(m.due, today);
  return { text: ymdShort(m.due), sub: d < 0 ? `超期 ${-d} 天` : d === 0 ? "今天到期" : d === 1 ? "明天到期" : `还有 ${d} 天`, bad: d < 0 };
}
const memoriesOf = (id) => memList().filter((x) => x.belongs_to === id);
function matterFinding(P) {
  const open = P.items.filter((m) => !closedState(m));
  const goals = open.filter((m) => m.kind === "goal" && m.state !== "parked"), areas = open.filter((m) => m.kind === "area");
  const late = goals.filter((m) => dueOf(m, P.today)?.bad);
  const waiting = goals.filter((m) => m.state === "waiting");
  if (late.length) return late.length === 1 ? `「${late[0].title}」超期 ${-dayDiff(late[0].due, P.today)} 天。` : `${late.length} 件超期了，最早的是「${late[0].title}」。`;
  if (waiting.length) return `${waiting.length} 件在等别人：${waiting.map((m) => m.waiting_for).join("、")}。`;
  if (goals.length || areas.length) return `${goals.length} 个目标在做，${areas.length} 个领域在管。`;
  return "现在没有在做的事。在对话里说「开始做…」「这件事我来管」，komo 会记下。";
}
function matterRow(m, P) {
  const due = dueOf(m, P.today);
  const sub = m.kind === "area" ? (m.standard ? `标准：${m.standard}` : "领域") : m.state === "waiting" ? `在等：${m.waiting_for}` : m.next ? `下一步：${m.next}` : closedState(m) ? "" : "还没有下一步";
  const mems = memoriesOf(m.id);
  const motes = mems.slice(0, 6).map((x) => `<i class="mote ${moteClass(x)}"></i>`).join("");
  const mark = closedState(m) ? "off" : m.kind === "area" ? "area" : m.state === "waiting" ? "wait" : m.state === "parked" ? "pale" : "on";
  const pill = closedState(m) ? `<span class="pill off">${MATTER_STATE[m.state]}</span>` : m.state === "waiting" ? `<span class="pill wait">在等</span>` : m.state === "parked" ? `<span class="pill off">搁置</span>` : "";
  return `<li class="mrow mrow-matter${m.id === P.selected ? " on" : ""}">
    <span class="smark ${mark}" aria-hidden="true"></span>
    <button class="mrow-main" data-pg-sel="matters" data-id="${m.id}" ${m.id === P.selected ? 'aria-current="true"' : ""}><span class="mtext">${esc(m.title)}</span>${sub ? `<span class="msub">${esc(sub)}</span>` : ""}</button>
    <span class="mnum num r">${due ? `${due.text}<small class="${due.bad ? "bad" : ""}">${due.sub || (closedState(m) && m.closed_at ? `结案 ${day(m.closed_at)}` : "")}</small>` : closedState(m) && m.closed_at ? `<span class="muted">—</span><small>结案 ${day(m.closed_at)}</small>` : `<span class="muted">—</span>`}</span>
    <span class="mmotes r">${mems.length ? `<span class="motes" aria-hidden="true">${motes}</span><span class="num">${mems.length}</span>` : `<span class="muted">—</span>`}</span>
    <span class="mact">${pill}</span></li>`;
}
function matterGroup(title, items, P, emptyText, cap = 0) {
  const shown = cap && !P.showClosed ? items.slice(0, cap) : items;
  const more = cap && items.length > cap ? `<button class="text-btn more-btn" data-pg-act="toggle-closed">${P.showClosed ? "收起" : `显示全部 ${items.length} 件`}</button>` : "";
  return `<div class="grp"><h2 class="grp-h">${title} <span class="muted num">${items.length}</span></h2>
    ${items.length ? `<ul class="mrows">${shown.map((m) => matterRow(m, P)).join("")}</ul>${more}` : `<p class="empty-frame">${emptyText}</p>`}</div>`;
}
function renderMatters() {
  const P = S.pg.matters;
  $("matters-finding").textContent = P.st === "ready" ? matterFinding(P) : P.st === "error" ? "事项没取到。" : " ";
  const order = (a, b) => {
    const da = dueOf(a, P.today), db = dueOf(b, P.today);
    return (db?.bad || 0) - (da?.bad || 0) || (a.due ? 0 : 1) - (b.due ? 0 : 1) || (a.due && b.due ? a.due.localeCompare(b.due) : 0) || a.created_at.localeCompare(b.created_at);
  };
  $("matters-list").innerHTML = listShell(P, () => {
    const open = P.items.filter((m) => !closedState(m));
    const goals = open.filter((m) => m.kind === "goal" && m.state !== "parked").sort(order);
    const areas = open.filter((m) => m.kind === "area").sort((a, b) => a.created_at.localeCompare(b.created_at));
    const parked = open.filter((m) => m.state === "parked").sort(order);
    const closed = P.items.filter(closedState).sort((a, b) => (b.closed_at || "").localeCompare(a.closed_at || ""));
    const head = `<div class="mhead mhead-matter" aria-hidden="true"><span></span><span>事项</span><span class="r">截止</span><span class="r">记忆</span><span></span></div>`;
    return head +
      matterGroup("目标", goals, P, "没有在做的目标：有结果、会结束的事记成目标，可以带截止日期。") +
      matterGroup("领域", areas, P, "没有在管的领域：没有终点、要一直维持的事（每周告警轮换）记成领域。") +
      (parked.length ? matterGroup("搁置", parked, P, "") : "") +
      (closed.length ? matterGroup("已结案", closed, P, "", 5) : "");
  });
  if (wide() && P.st === "ready" && !P.selected) {
    const first = P.items.find((m) => !closedState(m)) || P.items[0];
    if (first) { P.selected = first.id; setPageHash(); return renderMatters(); }
  }
  finishDetail("matters", matterDetail(P));
}
// 每个状态能做什么（docs/goals.md §4 的表）：[动词, 文案, 是否要表单, 是否主按钮]
function matterActions(m) {
  if (closedState(m)) return [["reopen", "重开", false, true]];
  if (m.kind === "area") return [["edit", "改名称或标准", true], ["link", "挂定时任务", true], ["retire", "不再管", false, false, "text"]];
  if (m.state === "active") return [["done", "完成", false, true], ["next", m.next ? "改下一步" : "写下一步", true], ["wait", "转为在等", true], ["park", "搁置"], ["edit", "改标题或截止", true, false, "text"], ["link", "挂定时任务", true, false, "text"], ["drop", "放弃", false, false, "text"]];
  if (m.state === "waiting") return [["resume", "等到了，继续", true, true], ["wait", "改在等什么", true], ["done", "完成"], ["park", "搁置"], ["edit", "改标题或截止", true, false, "text"], ["link", "挂定时任务", true, false, "text"], ["drop", "放弃", false, false, "text"]];
  return [["resume", "继续", true, true], ["done", "完成"], ["edit", "改标题或截止", true, false, "text"], ["drop", "放弃", false, false, "text"]];
}
function matterForm(m, P) {
  const f = P.form, busy = P.busy ? "disabled" : "";
  const submit = (label) => `<div class="actions">${btn(P.busy ? "正在改" : label, `type="submit" ${busy}`, "primary")}${btn("取消", 'type="button" data-pg-act="form-cancel"')}</div>`;
  const input = (name, label, value = "", extra = "") => `<label class="lbl" for="mf-${name}">${label}</label><input class="input" id="mf-${name}" name="${name}" value="${esc(value)}" maxlength="300" ${extra}>`;
  if (f === "next") return `<form class="mform inline" id="matter-form" data-verb="next">${input("next", "下一步", m.next || "", 'placeholder="一句话；留空就是清掉"')}${submit("保存")}</form>`;
  if (f === "wait") return `<form class="mform inline" id="matter-form" data-verb="wait">${input("waiting_for", "在等谁、等什么", m.waiting_for || "", "required")}${submit("转为在等")}</form>`;
  if (f === "resume") return `<form class="mform inline" id="matter-form" data-verb="resume">${input("next", "接下来做什么（可不填）", m.next || "")}${submit("继续")}</form>`;
  if (f === "edit") {
    return `<form class="mform inline" id="matter-form" data-verb="edit">${input("title", m.kind === "goal" ? "标题" : "名称", m.title, 'required maxlength="80"')}
      ${m.kind === "goal"
        ? `<label class="lbl" for="mf-due">截止日期</label><input class="input" type="date" id="mf-due" name="due" value="${m.due || ""}"><p class="hint">清空日期就是不再设截止。</p>`
        : input("standard", "要维持的标准", m.standard || "", 'placeholder="留空就是清掉"')}${submit("保存")}</form>`;
  }
  if (f === "link") {
    const linked = new Set((m.triggers || []).filter((t) => t.kind === "cron").map((t) => t.job));
    const jobs = S.pg.cron.jobs.filter((j) => !linked.has(j.id));
    if (!jobs.length) return `<p class="hint">没有可挂的定时任务。先去<a class="ev-link" href="#/cron">定时</a>建一个。</p>`;
    return `<form class="mform inline" id="matter-form" data-verb="link"><label class="lbl" for="mf-job">定时任务</label>
      <select class="select" id="mf-job" name="job">${jobs.map((j) => `<option value="${j.id}">${esc(j.name)}</option>`).join("")}</select>${submit("挂上")}</form>`;
  }
  return "";
}
function triggerRow(t, m) {
  const unlink = closedState(m) ? "" : `<button class="text-btn quiet" data-pg-act="unlink" data-trigger='${esc(JSON.stringify(t))}'>摘下</button>`;
  if (t.kind === "cron") {
    const j = S.pg.cron.jobs.find((x) => x.id === t.job);
    return `<li class="ev"><span class="ev-what">定时任务</span><span class="ev-line">${j ? `<a class="ev-link" href="#/cron/${j.id}">${esc(j.name)}${icon("right")}</a>` : `<span class="muted">已删除的任务</span>`}${unlink}</span></li>`;
  }
  const s = S.sessions.find((x) => x.session === t.session);
  return `<li class="ev"><span class="ev-what">monitor <span class="num">#${esc(t.monitor)}</span></span><span class="ev-line">${s ? `<a class="ev-link" href="#/s/${t.session}">${esc(titleOf(s))}${icon("right")}</a>` : `<span class="muted">会话已不在列表里</span>`}${unlink}</span></li>`;
}
function closureNotice(P) {
  const n = P.notice;
  if (!n) return "";
  const parts = [];
  if (n.closure?.archived) parts.push(`归档了 ${n.closure.archived} 条为它记的线索。`);
  if (n.closure?.restored) parts.push(`恢复了 ${n.closure.restored} 条归档的线索。`);
  if (n.still_linked?.length) parts.push(`还挂着 ${n.still_linked.length} 个触发器，没删；不用了就在下面摘下。`);
  const conv = n.closure?.conventions || [];
  if (!parts.length && !conv.length) return "";
  return `<div class="notice">${parts.map((p) => `<p>${p}</p>`).join("")}
    ${conv.length ? `<p>这些约定还归属它，照常带进对话；去记忆页改归属或遗忘：</p><ul>${conv.map((c) => `<li><a class="ev-link" href="#/memory/${c.id}">${esc(c.content)}</a></li>`).join("")}</ul>` : ""}</div>`;
}
function matterNewForm(P) {
  return `<form class="mform" id="matter-new">
    <div class="detail-top"><h2 class="detail-h">新建事项</h2><button type="button" class="icon-btn" data-pg-close aria-label="关闭">${icon("x")}</button></div>
    <fieldset class="seg"><legend class="lbl">是什么</legend>
      <label><input type="radio" name="kind" value="goal" checked>目标：有结果、会结束</label>
      <label><input type="radio" name="kind" value="area">领域：没有终点、要一直维持</label></fieldset>
    <label class="lbl" for="mn-title">一句话</label><input class="input" id="mn-title" name="title" required maxlength="80" placeholder="例：这周把 owl 的发布观察做完">
    <div data-for="goal"><label class="lbl" for="mn-next">下一步（可不填）</label><input class="input" id="mn-next" name="next" maxlength="300">
      <label class="lbl" for="mn-due">截止日期（可不填）</label><input class="input" type="date" id="mn-due" name="due"></div>
    <div data-for="area" hidden><label class="lbl" for="mn-standard">要维持的标准（可不填）</label><input class="input" id="mn-standard" name="standard" maxlength="300" placeholder="例：每周一前完成告警值班轮换"></div>
    ${failLine(P)}
    <div class="actions">${btn(P.busy ? "正在建" : "建好", `type="submit" ${P.busy ? "disabled" : ""}`, "primary")}${btn("取消", 'type="button" data-pg-close')}</div></form>`;
}
function matterDetail(P) {
  if (P.selected === "new") return matterNewForm(P);
  const m = P.items.find((x) => x.id === P.selected);
  if (P.st !== "ready") return P.st === "error" ? "" : `<div class="detail-pad">${skeletonRows(4, "sk-row in-card")}</div>`;
  if (!m) return `<p class="empty-frame">${P.items.length ? "选一件，看它的下一步、触发器和为它记的记忆。" : "新建的事项会出现在这里。"}</p>`;
  const due = dueOf(m, P.today);
  const fields = m.kind === "goal"
    ? [m.state === "waiting" ? ["在等", esc(m.waiting_for)] : null, ["下一步", m.next ? esc(m.next) : `<span class="muted">—</span>`], ["截止", due ? `${due.text}${due.sub ? ` · <span class="${due.bad ? "bad" : ""}">${due.sub}</span>` : ""}` : `<span class="muted">没设</span>`]]
    : [["标准", m.standard ? esc(m.standard) : `<span class="muted">没写</span>`]];
  const acts = matterActions(m);
  const buttons = acts.filter((a) => a[4] !== "text").map(([verb, label, form, primary]) => btn(label, `data-pg-act="${form ? "form" : "verb"}" data-verb="${verb}" ${P.busy ? "disabled" : ""}`, primary ? "primary" : "")).join("");
  const texts = acts.filter((a) => a[4] === "text").map(([verb, label, form]) => `<button class="text-btn quiet${verb === "drop" || verb === "retire" ? " danger-link" : ""}" data-pg-act="${form ? "form" : "verb"}" data-verb="${verb}">${label}</button>`).join("");
  const mems = memoriesOf(m.id);
  const ev = m.evidence || [];
  return `<div class="detail-pad">
    ${detailTop(m.kind === "goal" ? "目标" : "领域", MATTER_STATE[m.state], m.state === "waiting" ? " wait" : closedState(m) || m.state === "parked" ? " off" : "")}
    <p class="detail-text">${esc(m.title)}</p>
    ${kvBlock(fields).replace('class="kv"', 'class="kv plain"')}
    ${failLine(P)}${closureNotice(P)}
    <div class="actions">${buttons}</div>
    ${texts ? `<div class="text-acts">${texts}</div>` : ""}
    ${matterForm(m, P)}
    <h3 class="sub-h">触发器 <span class="num muted">${(m.triggers || []).length}</span></h3>
    ${(m.triggers || []).length ? `<ul class="evs">${m.triggers.map((t) => triggerRow(t, m)).join("")}</ul>` : `<p class="hint">没挂。在等别人时 komo 常会挂一个 monitor 盯着；定期要做的事可以挂定时任务。</p>`}
    <h3 class="sub-h">为它记的记忆 <span class="num muted">${mems.length}</span></h3>
    ${mems.length ? `<ul class="evs">${mems.map((x) => `<li class="ev ev-mem"><span class="mote ${moteClass(x)}" aria-hidden="true"></span><a class="ev-link plain" href="#/memory/${x.id}">${esc(x.content)}</a></li>`).join("")}</ul>` : `<p class="hint">还没有。提取时 komo 会把相关的新观察挂到进行中的事项下。</p>`}
    ${kvBlock([["新建", `<span class="num">${day(m.created_at)}</span>`], ["更新", `<span class="num">${ago(m.updated_at)}</span>`], m.closed_at ? ["结案", `<span class="num">${day(m.closed_at)}</span>`] : null, ["版本", `第 <span class="num">${m.revision}</span> 版`]])}
    <h3 class="sub-h">用户原话 <span class="num muted">${ev.length}</span></h3>
    ${ev.length ? `<ul class="evs">${ev.map(evidenceRow).join("")}</ul>` : `<p class="hint">没有：这件事是在终端或这里直接建的。</p>`}
  </div>`;
}
async function matterVerb(verb, fields = {}) {
  const P = S.pg.matters, m = P.items.find((x) => x.id === P.selected);
  if (!m) return;
  P.busy = verb; P.fail = ""; render();
  const body = { kind: m.kind, expected_revision: m.revision, request_key: `web:matter:${m.id}:${m.revision}:${verb}`, ...fields };
  try {
    const r = await api(`/v1/matters/${m.id}/${verb}`, { method: "POST", body: JSON.stringify(body) });
    P.items = P.items.map((x) => (x.id === m.id ? r.matter : x));
    P.notice = r.closure || r.still_linked ? { closure: r.closure, still_linked: r.still_linked } : null;
    P.form = null;
    if (r.closure?.archived || r.closure?.restored) LOADERS.matters(P).then(render);
    api("/v1/matters").then((x) => { S.matters = x.matters; }).catch(() => {});
  } catch (err) { P.fail = `没改成：${err.message}`; }
  P.busy = ""; render();
}
async function matterCreate(form) {
  const P = S.pg.matters, f = new FormData(form), kind = f.get("kind");
  const body = { kind, title: f.get("title").trim(), request_key: `web:matter-add:${crypto.randomUUID()}` };
  if (kind === "goal") { if (f.get("next").trim()) body.next = f.get("next").trim(); if (f.get("due")) body.due = f.get("due"); }
  else if (f.get("standard").trim()) body.standard = f.get("standard").trim();
  P.busy = "add"; P.fail = ""; render();
  try {
    const r = await api("/v1/matters", { method: "POST", body: JSON.stringify(body) });
    P.items.push(r.matter); P.selected = r.matter.id; setPageHash();
    api("/v1/matters").then((x) => { S.matters = x.matters; }).catch(() => {});
  } catch (err) { P.fail = `没建成：${err.message}`; }
  P.busy = ""; render();
}

/* ---------- 定时 ----------
   一个列表，点开看详情。列表按在跑 → 暂停 → 跑完、再按下一次排；不选时列表独占画布，选了右边出详情。 */
const FIRING = { queued: "排队中", ok: "成功", error: "出错", waiting: "等你处理", skipped: "跳过" };
const FIRING_PILL = { ok: "", error: " bad", waiting: " wait", skipped: " off", queued: " off" };
const NOTIFY = { always: "每次都投", on_error: "只在出错时投", never: "从不投" };
const OVERLAP = { skip: "上次没跑完就跳过这次", allow: "照常再起一个" };
const DOW = ["日", "一", "二", "三", "四", "五", "六"];
const localTz = Intl.DateTimeFormat().resolvedOptions().timeZone;
// 只用来把常见的五字段表达式读成人话；读不懂的照写表达式
function cronParse(expr) {
  const f = String(expr).trim().split(/\s+/);
  if (f.length !== 5) return null;
  const R = [[0, 59], [0, 23], [1, 31], [1, 12], [0, 7]];
  const names = [null, null, null, "jan feb mar apr may jun jul aug sep oct nov dec".split(" "), "sun mon tue wed thu fri sat".split(" ")];
  const val = (x, i) => { const n = names[i]?.indexOf(x.toLowerCase()); return n >= 0 ? n + (i === 3 ? 1 : 0) : Number(x); };
  const sets = [];
  for (let i = 0; i < 5; i++) {
    const set = new Set();
    for (const part of f[i].split(",")) {
      const [range, stepS] = part.split("/"), step = stepS ? Number(stepS) : 1;
      let [lo, hi] = range === "*" ? R[i] : range.split("-").map((x) => val(x, i));
      if (hi === undefined) hi = stepS ? R[i][1] : lo;
      if (![lo, hi, step].every(Number.isFinite) || step < 1) return null;
      for (let v = lo; v <= hi; v += step) set.add(i === 4 && v === 7 ? 0 : v);
    }
    sets.push(set);
  }
  return { sets, raw: f };
}
function scheduleText(job) {
  const t = job.trigger;
  if (t.kind === "at") return `${day(t.at)} ${clock(new Date(t.at))}，只跑一次`;
  const c = cronParse(t.expr);
  if (!c) return t.expr;
  const [mi, ho, dm, mo, dw] = c.raw;
  const single = (x) => /^\d+$/.test(x);
  const hm = single(mi) && single(ho) ? `${ho.padStart(2, "0")}:${mi.padStart(2, "0")}` : null;
  let words = null;
  if (hm && mo === "*") {
    if (dm === "*" && dw === "*") words = `每天 ${hm}`;
    else if (dm === "*" && /^[\d,-]+$/.test(dw)) words = dw === "1-5" ? `工作日 ${hm}` : `每周${[...c.sets[4]].sort().map((d) => DOW[d]).join("、")} ${hm}`;
    else if (single(dm) && dw === "*") words = `每月 ${dm} 日 ${hm}`;
  } else if (single(mi) && ho === "*" && dm === "*" && mo === "*" && dw === "*") words = `每小时第 ${mi} 分`;
  else if (/^\*\/\d+$/.test(mi) && ho === "*" && dm === "*" && mo === "*" && dw === "*") words = `每 ${mi.slice(2)} 分钟`;
  return (words || t.expr) + (t.tz && t.tz !== localTz ? `（${t.tz}）` : "");
}
function whenText(iso) {
  if (!iso) return "—";
  const d = new Date(iso), today = new Date(); today.setHours(0, 0, 0, 0);
  const diff = Math.floor((new Date(d).setHours(0, 0, 0, 0) - today) / 86400000);
  const pre = diff === 0 ? "今天" : diff === 1 ? "明天" : diff > 1 && diff < 7 ? `周${DOW[d.getDay()]}` : diff === -1 ? "昨天" : `${d.getMonth() + 1}月${d.getDate()}日`;
  return `${pre} ${clock(d)}`;
}
const cronLast = (job) => S.pg.cron.status.get(job.id)?.last || null;
const cronNext = (job) => S.pg.cron.status.get(job.id)?.next_run_at || job.next_run_at;
function cronFinding(P) {
  const jobs = P.jobs;
  if (!jobs.length) return "还没有定时任务。在对话里说「每周五下午两点…」，komo 会建一个。";
  const waiting = jobs.filter((j) => cronLast(j)?.status === "waiting");
  if (waiting.length) return `${waiting.length} 个任务停在等你处理：${waiting.map((j) => `「${j.name}」`).join("")}。`;
  const failed = jobs.filter((j) => cronLast(j)?.status === "error" || j.last_error);
  if (failed.length) return `「${failed[0].name}」${failed[0].last_error ? "排不上下一次" : "上一次出错了"}。`;
  const next = jobs.filter((j) => j.status === "active" && cronNext(j)).sort((a, b) => cronNext(a).localeCompare(cronNext(b)))[0];
  return next ? `下一次是「${next.name}」，${whenText(cronNext(next))}。` : "所有任务都暂停了。";
}
function renderCron() {
  const P = S.pg.cron;
  $("cron-finding").textContent = P.st === "ready" ? cronFinding(P) : P.st === "error" ? "定时任务没取到。" : " ";
  $("cron-list").innerHTML = listShell(P, () => {
    if (!P.jobs.length) return `<p class="empty-frame">还没有定时任务。</p>`;
    const order = { active: 0, paused: 1, done: 2 };
    const jobs = [...P.jobs].sort((a, b) => order[a.status] - order[b.status] || (cronNext(a) || "~").localeCompare(cronNext(b) || "~"));
    return `<div class="mhead mhead-cron" aria-hidden="true"><span></span><span>任务</span><span class="r">下一次</span><span class="r">上一次</span></div>
      <ul class="mrows">${jobs.map((j) => {
        const last = cronLast(j);
        const mark = j.status === "active" ? (j.last_error ? "bad" : "on") : "off";
        return `<li class="mrow mrow-cron${j.id === P.selected ? " on" : ""}">
          <span class="smark ${mark}" aria-hidden="true"></span>
          <button class="mrow-main" data-pg-sel="cron" data-id="${j.id}" ${j.id === P.selected ? 'aria-current="true"' : ""}><span class="mtext">${esc(j.name)}</span><span class="msub">${esc(scheduleText(j))}${j.status !== "active" ? `<span class="sep">·</span>${j.status === "paused" ? "已暂停" : "已跑完"}` : ""}</span></button>
          <span class="mwhen num r">${j.status === "active" && cronNext(j) ? whenText(cronNext(j)) : `<span class="muted">—</span>`}</span>
          <span class="mact">${last ? `<span class="pill${FIRING_PILL[last.status] ?? ""}">${FIRING[last.status] || last.status}</span>` : `<span class="muted">还没跑过</span>`}</span></li>`;
      }).join("")}</ul>`;
  });
  finishDetail("cron", cronDetail(P));
  // 没选中就不留详情栏：列表独占画布
  $("cron-detail").hidden = !P.selected;
  $("cron-body").classList.toggle("single", !P.selected);
}
function cronDetail(P) {
  const j = P.jobs.find((x) => x.id === P.selected);
  if (!j) return "";
  const last = cronLast(j), busy = P.busy ? "disabled" : "";
  const lastSession = last?.session && S.sessions.find((x) => x.session === last.session);
  const stateName = { active: "在跑", paused: "已暂停", done: "已跑完" }[j.status];
  const n = P.notice;
  const acts = j.status === "done" ? "" : `<div class="actions">
    ${btn(P.busy === "run" ? "正在触发" : "现在跑一次", `data-pg-act="cron-run" ${busy}`, "primary")}
    ${btn(j.status === "paused" ? "恢复" : "暂停", `data-pg-act="cron-toggle" ${busy}`)}</div>`;
  const del = P.confirm === "delete"
    ? `<div class="danger"><p>删掉之后不再跑；跑过的记录还在各自的会话里。</p>
        <div class="actions">${btn("删掉「" + esc(j.name) + "」", `data-pg-act="cron-delete" ${busy}`, "danger-btn")}${btn("取消", 'data-pg-act="confirm-cancel"')}</div></div>`
    : `<button class="text-btn danger-link" data-pg-act="confirm" data-what="delete">删除这个任务</button>`;
  return `<div class="detail-pad">
    <div class="detail-top"><span class="detail-kind">${j.command != null ? "命令" : "提示词"}</span><span class="pill${j.status === "active" ? "" : " off"}">${stateName}</span>
      <button class="icon-btn close-x" data-pg-close aria-label="关闭详情">${icon("x")}</button></div>
    <p class="detail-text">${esc(j.name)}</p>
    <p class="detail-when"><span class="muted">什么时候</span>${esc(scheduleText(j))}</p>
    ${j.last_error ? `<p class="form-err">排不上下一次：${esc(j.last_error)}</p>` : ""}
    ${failLine(P)}
    ${n?.run ? `<div class="notice"><p>已触发，这一次在<a class="ev-link" href="#/s/${n.run.session}">它的会话${icon("right")}</a>里跑。</p></div>` : ""}
    ${acts}
    <h3 class="sub-h">跑什么</h3>
    ${j.command != null ? `<pre class="code-block">${esc(j.command)}</pre>` : `<p class="detail-body">${esc(j.prompt)}</p>`}
    ${kvBlock([
      j.workdir ? ["目录", `<span class="mono">${esc(j.workdir)}</span>`] : null,
      ["下一次", j.status === "active" && cronNext(j) ? `<span class="num">${whenText(cronNext(j))}</span>` : `<span class="muted">—</span>`],
      ["上一次", last ? `<span class="pill${FIRING_PILL[last.status] ?? ""}">${FIRING[last.status] || last.status}</span> <span class="num">${whenText(last.scheduled_at)}</span>${last.session ? ` · <a class="ev-link" href="#/s/${last.session}">${lastSession ? esc(titleOf(lastSession)) : "那次的会话"}</a>` : ""}${last.error ? `<div class="muted">${esc(last.error)}</div>` : ""}` : `<span class="muted">还没跑过</span>`],
      ["投递", NOTIFY[j.notify] || j.notify],
      ["重叠", OVERLAP[j.overlap] || j.overlap],
    ])}
    ${del}
  </div>`;
}
async function cronAct(act) {
  const P = S.pg.cron, j = P.jobs.find((x) => x.id === P.selected);
  if (!j) return;
  P.busy = act === "cron-run" ? "run" : "act"; P.fail = ""; P.notice = null; render();
  try {
    if (act === "cron-run") {
      const r = await api(`/v1/cron/${j.id}/run`, { method: "POST", body: JSON.stringify({ request_key: `web:cron-run:${crypto.randomUUID()}` }) });
      P.notice = { run: r };
    } else if (act === "cron-toggle") {
      await api(`/v1/cron/${j.id}`, { method: "PATCH", body: JSON.stringify({ status: j.status === "paused" ? "active" : "paused", request_key: `web:cron-status:${j.id}:${crypto.randomUUID()}` }) });
    } else if (act === "cron-delete") {
      await api(`/v1/cron/${j.id}`, { method: "DELETE" });
      P.selected = null; P.confirm = null; setPageHash();
    }
    await LOADERS.cron(P);
  } catch (err) { P.fail = `没成：${err.message}`; }
  P.busy = ""; render();
}

/* ---------- 待处理 ----------
   无人值守的 Run 停下来等人的地方：确认对外调用、核对结果不明的调用、前提没了。
   每一条的答案菜单由 Gateway 给（verdicts），界面照抄，不自己推。 */
const INBOX_KIND = { confirm: "要调对外工具", verify: "结果不明，等你核对", blocked: "前提没了" };
const VERDICT = { approve: "执行", reject: "不执行", satisfied: "已经是那样了，继续", not_performed: "确定没执行，重放", resolve: "处理好了，重新判断", abandon: "到此为止" };
function renderInbox() {
  const P = S.pg.inbox;
  const n = P.items.length;
  $("inbox-finding").textContent = P.st === "ready" ? (n ? `${n} 条在等你答复；不答，那条 Run 就一直停着。` : "没有等你答复的事。") : P.st === "error" ? "待处理没取到。" : " ";
  $("inbox-list").innerHTML = listShell(P, () => {
    if (!n) return `<p class="empty-frame">现在没有停着的 Run。</p>
      <h2 class="grp-h">什么会停在这里</h2>
      <dl class="kv plain inbox-kinds">
        <dt>要调对外工具</dt><dd>定时任务、monitor 或后台任务起的 Run，要调声明为对外的工具（发消息、提工单）时，等你点执行或不执行。</dd>
        <dt>结果不明</dt><dd>一次调用跑到一半 Gateway 重启了，不知道做没做成：你核对后告诉它"已经是那样了"或"确定没执行"。</dd>
        <dt>前提没了</dt><dd>它依赖的文件或会话没了：处理好之后让它重新判断，或到此为止。</dd>
      </dl>
      <p class="hint">没有渠道配 home_chat 时，这些卡投不到聊天里，只在这里和 <code>komo intervention list</code> 看得到。</p>`;
    return `<ul class="mrows">${P.items.map((x) => {
      const s = S.sessions.find((y) => y.session === x.session);
      return `<li class="mrow mrow-inbox${x.handle === P.selected ? " on" : ""}">
        <span class="smark ${x.kind === "confirm" ? "wait" : "bad"}" aria-hidden="true"></span>
        <button class="mrow-main" data-pg-sel="inbox" data-id="${esc(x.handle)}"><span class="mtext">${esc(x.question)}</span>
          <span class="msub">${INBOX_KIND[x.kind] || x.kind}<span class="sep">·</span>${s ? esc(titleOf(s)) : "会话已不在列表里"}<span class="sep">·</span><span class="num">${ago(x.created_at)}</span></span></button>
        <span class="mwhen"></span></li>`;
    }).join("")}</ul>`;
  });
  if (wide() && P.st === "ready" && !P.selected && n) { P.selected = P.items[0].handle; setPageHash(); return renderInbox(); }
  const x = P.items.find((y) => y.handle === P.selected);
  let html;
  if (P.st !== "ready") html = P.st === "error" ? "" : `<div class="detail-pad">${skeletonRows(3, "sk-row in-card")}</div>`;
  else if (P.notice?.answered) html = `<div class="detail-pad">${detailTop("已答复", "")}<p class="detail-text">${esc(P.notice.answered.note)}</p></div>`;
  else if (!x) html = `<p class="empty-frame">${n ? "选一条答复它。" : "没有要答复的。"}</p>`;
  else {
    const s = S.sessions.find((y) => y.session === x.session);
    const verdicts = (x.verdicts || []).map((v, i) => P.confirm === v
      ? ""
      : btn(VERDICT[v] || v, `data-pg-act="answer" data-verdict="${v}" ${P.busy ? "disabled" : ""}`, i === 0 ? "primary" : v === "abandon" ? "danger-btn" : "")).join("");
    const abandonAsk = P.confirm === "abandon" ? `<div class="danger"><p>这条 Run 到此为止，取消后不会再继续。</p><div class="actions">${btn("确定到此为止", 'data-pg-act="answer" data-verdict="abandon" data-sure="1"', "danger-btn")}${btn("取消", 'data-pg-act="confirm-cancel"')}</div></div>` : "";
    html = `<div class="detail-pad">${detailTop(INBOX_KIND[x.kind] || x.kind, "")}
      <p class="detail-text">${esc(x.question)}</p>
      ${failLine(P)}
      <div class="actions">${verdicts}</div>${abandonAsk}
      ${kvBlock([["会话", s ? `<a class="ev-link" href="#/s/${x.session}">${esc(titleOf(s))}${icon("right")}</a>` : `<span class="muted">${esc(x.session)}</span>`], ["停下", `<span class="num">${ago(x.created_at)}</span>`], ["句柄", `<span class="mono">${esc(x.handle)}</span>`]])}
      <p class="hint">答复只对这一次有效，没有"以后都允许"。</p></div>`;
  }
  finishDetail("inbox", html);
  $("inbox-detail").hidden = P.st === "ready" && !n && !P.notice;
  $("inbox-body").classList.toggle("single", $("inbox-detail").hidden);
}
async function inboxAnswer(verdict, sure) {
  const P = S.pg.inbox, x = P.items.find((y) => y.handle === P.selected);
  if (!x) return;
  if (verdict === "abandon" && !sure) { P.confirm = "abandon"; render(); return; }
  P.busy = verdict; P.fail = ""; render();
  try {
    const r = await api(`/v1/interventions/${encodeURIComponent(x.handle)}/answer`, { method: "POST", body: JSON.stringify({ verdict, request_key: `web:answer:${x.handle}:${verdict}` }) });
    P.notice = { answered: r }; P.confirm = null;
    await LOADERS.inbox(P);
  } catch (err) { P.fail = `没答上：${err.message}`; }
  P.busy = ""; render();
}

/* ---------- 设置 ----------
   一个尺寸固定的弹窗：左边一列分节，右边是这一节，内容长了只在右边滚。模型、配置、环境变量三节能直接改：
   Gateway 代写文件、校验通过才落盘、再热重载（docs/komo_bot.md §3「从 Web 改模型配置」「从 Web 改常用设置与环境变量」）；
   其余各节只读。界面不说文件在磁盘上的哪儿。 */
const WINDOW_SOURCE = { config: "配置里写的", provider: "provider 报的", unknown: "不知道" };
const MCP_STATE = { connecting: "连接中", connected: "已连上", needs_auth: "要登录", "needs-auth": "要登录", failed: "连不上" };
const BACKENDS = [["responses", "Responses"], ["chat_completions", "Chat Completions"], ["anthropic_messages", "Anthropic Messages"]];
const SET_SECTIONS = [["models", "模型", "i-leaf"], ["config", "配置", "i-sliders"], ["env", "环境变量", "i-key"], ["index", "记忆索引", "i-search"], ["mcp", "MCP", "i-plug"], ["gateway", "Gateway", "i-term"], ["channels", "渠道", "i-chat"]];
// 常用设置的说法（键与种类、选项由 Gateway 给，GET /v1/config/settings）
const SETTING_COPY = {
  default_agent: ["默认助手", "新会话用哪个助手。"],
  "memory.enabled": ["记忆", "从对话里提取偏好、约定与结论，用得上时带进对话。"],
  "memory.model": ["记忆用的模型", "提取和整理记忆时用哪个模型；不设就跟默认模型。"],
  "memory.embedding": ["记忆用的向量模型", "检索时把文字变成向量。换了要重启 Gateway 才生效。"],
  "memory.retrieval.mode": ["记忆检索方式", "怎么找出和眼下这段对话相关的记忆。"],
  "compaction.enabled": ["自动压缩", "上下文快满时，把早先的对话压成一段摘要。"],
  "execution.model_result_bytes": ["工具结果给模型的上限", "超过就只给开头的预览和完整结果的引用。单位字节。", "字节"],
  "execution.call_timeout_secs": ["工具调用时限", "一次工具调用最长跑多久。", "秒"],
  "execution.confirm_timeout_mins": ["对外操作等确认", "无人值守时，等你批准对外工具的时长；到点不答就不执行。", "分钟"],
};
const CHOICE_TEXT = { hybrid: "混合（关键词 + 向量）", keyword: "只用关键词", vector: "只用向量" };
const errLine = (x) => (x?.error ? `<p class="form-err">没取到：${esc(x.error)}</p>` : "");
let settingsHtml = "";
function renderSettings() {
  const P = S.pg.settings, D = P.data;
  const cfg = D.config, issues = cfg?.issues || [];
  const section = SET_SECTIONS.some(([k]) => k === P.selected) ? P.selected : "models";
  $("settings-finding").textContent = P.st !== "ready" ? " "
    : cfg?.error ? "配置检查没取到。"
    : `Gateway ${D.health?.version || ""} 在跑，${issues.length ? `配置有 ${issues.length} 条提醒。` : "配置检查通过。"}`;
  $("settings-nav").innerHTML = SET_SECTIONS.map(([k, label, ic]) => `<button type="button" data-set-sec="${k}" ${k === section ? 'aria-current="page"' : ""}>${icon(ic.slice(2))}${label}${k === "config" && issues.length ? `<span class="nav-count soft num">${issues.length}</span>` : ""}</button>`).join("");
  if (P.st === "error") { $("settings-body").innerHTML = `<div class="block-error"><p>没取到：${esc(P.error)}。</p><button class="text-btn" data-pg-reload>重试</button></div>`; settingsHtml = ""; return; }
  if (P.st !== "ready") { $("settings-body").innerHTML = skeletonRows(6, "sk-row mrow-sk"); settingsHtml = ""; return; }
  const html = { models: modelsSection, config: configSection, env: envSection, index: indexSection, mcp: mcpSection, gateway: gatewaySection, channels: channelsSection }[section](P);
  // 会话跑着时每秒重画一次：内容没变就不动 DOM，免得表单里正在输的字被冲掉
  if (settingsHtml !== html) { $("settings-body").innerHTML = html; settingsHtml = html; }
}
function openSettings(section = null) {
  const P = S.pg.settings, dlg = $("settings");
  if (section) P.selected = section;
  // 焦点落在弹窗本身：默认会落到第一个按钮（关闭钮），带着焦点环出现
  if (!dlg.open) { P.edit = null; P.confirm = null; P.fail = ""; P.result.edit = null; dlg.showModal(); dlg.focus(); }
  // 每次打开都重读：配置可能在文件里改过
  P.st = P.st === "ready" ? "ready" : "loading"; render();
  reloadPage("settings");
}
function settingsSection(k) {
  const P = S.pg.settings;
  if (P.selected === k) return;
  P.selected = k; P.edit = null; P.confirm = null; P.fail = ""; P.result.edit = null; P.mcpSel = null;
  $("settings-body").scrollTop = 0; render();
}
const secHead = (title, desc, right = "") => `<div class="set-head"><div><h3>${title}</h3><p>${desc}</p></div>${right}</div>`;

// —— 模型：provider 一张卡，卡里是挂在它上面的模型 ——
function keyLine(p) {
  if (p.auth) return `<span class="kstate ok"><i class="kdot"></i>凭证来自 ${esc(p.auth)} 登录</span>`;
  if (!p.env_key) return `<span class="kstate warn"><i class="kdot"></i>没写凭证变量</span>`;
  return p.key_set ? `<span class="kstate ok"><i class="kdot"></i><span class="mono">${esc(p.env_key)}</span> 已设置</span>` : `<span class="kstate warn"><i class="kdot"></i>缺 <span class="mono">${esc(p.env_key)}</span></span>`;
}
function providerForm(P, p) {
  const isNew = !p, busy = P.busy ? "disabled" : "";
  return `<form class="mform cfg-form" id="cfg-provider-form" data-id="${esc(p?.id || "")}">
    ${isNew ? `<label class="lbl" for="pf-id">名字</label><input class="input" id="pf-id" name="pid" required pattern="[A-Za-z0-9._-]+" maxlength="64" placeholder="例：deepseek">
      <p class="hint">模型靠这个名字找到它。</p>` : ""}
    <div class="two"><div><label class="lbl" for="pf-backend">协议</label><select class="select" id="pf-backend" name="api_backend">${BACKENDS.map(([v, n]) => `<option value="${v}" ${v === (p?.api_backend || "responses") ? "selected" : ""}>${n}</option>`).join("")}</select></div>
      <div><label class="lbl" for="pf-url">Base URL</label><input class="input" id="pf-url" name="base_url" value="${esc(p?.base_url || "")}" placeholder="https://api.example.com/v1"></div></div>
    ${p?.auth ? `<p class="hint">凭证来自 ${esc(p.auth)} 登录，不走 API key。</p><input type="hidden" name="auth" value="${esc(p.auth)}">`
      : `<label class="lbl" for="pf-key">API key</label><input class="input" id="pf-key" name="api_key" type="password" autocomplete="new-password" placeholder="${p ? (p.key_set ? "已设置；留空不改" : "还没设置") : "粘贴 key"}">
      <p class="hint">存成环境变量${p?.env_key ? ` <span class="mono">${esc(p.env_key)}</span>` : "，名字默认是 <code>名字大写_API_KEY</code>"}，不进配置。</p>`}
    <details class="adv" ${p?.timeout_secs || (p?.env_key && !isNew) ? "" : ""}><summary>更多设置</summary>
      ${p?.auth ? "" : `<label class="lbl" for="pf-env">凭证变量名</label><input class="input" id="pf-env" name="env_key" value="${esc(p?.env_key || "")}" placeholder="留空：有 key 时用 名字大写_API_KEY">`}
      <label class="lbl" for="pf-timeout">请求时限（秒）</label><input class="input" id="pf-timeout" name="timeout_secs" type="number" min="1" value="${p?.timeout_secs ?? ""}" placeholder="默认 120"></details>
    ${failLine(P)}
    <div class="actions">${btn(P.busy ? "正在保存" : isNew ? "添加" : "保存", `type="submit" ${busy}`, "primary")}${btn("取消", 'type="button" data-cfg="cancel"')}</div></form>`;
}
// 对话模型与向量模型共用一张表，类型决定显示哪组字段（新建时可切换，只切 hidden，不重绘，免得丢掉已填的）。
// 不挂 provider 的只能是向量模型：端点与 key 写在模型项里。
function modelForm(P, m, providerId, kind) {
  const C = P.data.cfg, isNew = !m, busy = P.busy ? "disabled" : "";
  const pid = m ? m.provider || "" : providerId, loose = !pid;
  const type = loose ? "embedding" : m ? kind : P.edit?.type || "completion";
  const parent = C.providers.find((p) => p.id === pid);
  // 视图里继承来的值不回填：留空就接着继承 provider 的，改了 provider 也跟着变
  const own = (k) => (m?.[k] && m[k] !== parent?.[k] ? m[k] : "");
  const backend = EMBED_BACKENDS[m?.api_backend] ? m.api_backend : "ollama_embeddings";
  const only = (t, html) => `<div data-only="${t}" ${type === t ? "" : "hidden"}>${html}</div>`;
  const providerPick = `<div><label class="lbl" for="mf2-provider">Provider</label><select class="select" id="mf2-provider" name="provider">${C.providers.map((p) => `<option value="${esc(p.id)}" ${p.id === pid ? "selected" : ""}>${esc(p.id)}</option>`).join("")}</select></div>`;
  return `<form class="mform cfg-form" id="cfg-model-form" data-alias="${esc(m?.alias || "")}" data-type="${type}">
    ${isNew && !loose ? `<label class="lbl" for="mf2-type">类型</label><select class="select" id="mf2-type" name="type"><option value="completion" ${type === "completion" ? "selected" : ""}>对话</option><option value="embedding" ${type === "embedding" ? "selected" : ""}>向量（记忆检索用）</option></select>` : ""}
    ${isNew && loose ? `<p class="hint">不挂 provider 的只能是向量模型；对话模型先建 provider 再挂上去。</p>` : ""}
    <div class="two">${isNew ? `<div><label class="lbl" for="mf2-alias">名字（alias）</label><input class="input" id="mf2-alias" name="alias" required pattern="[A-Za-z0-9._-]+" maxlength="64" placeholder="例：ds-chat"></div>` : ""}
      <div><label class="lbl" for="mf2-model">上游模型 id</label><input class="input mono" id="mf2-model" name="model" required value="${esc(m?.model || "")}" placeholder="例：deepseek-chat"></div>
      ${isNew || loose ? "" : providerPick}</div>
    ${isNew && !loose ? `<input type="hidden" name="provider" value="${esc(pid)}">` : ""}
    ${only("completion", `<div class="two"><div><label class="lbl" for="mf2-name">显示名</label><input class="input" id="mf2-name" name="name" value="${esc(m?.name || "")}" placeholder="菜单里显示的名字"></div>
        <div><label class="lbl" for="mf2-window">上下文窗口（token）</label><input class="input" id="mf2-window" name="context_window" type="number" min="1" value="${m?.context_window ?? ""}" placeholder="留空：问 provider"></div></div>
      <div class="two"><div><label class="lbl" for="mf2-effort">默认 effort</label><input class="input" id="mf2-effort" name="effort" value="${esc(m?.effort || "")}" placeholder="留空：provider 默认"></div>
        <div><label class="lbl" for="mf2-efforts">可选的 effort 档位</label><input class="input" id="mf2-efforts" name="efforts" value="${esc((m?.efforts || []).join(", "))}" placeholder="low, medium, high"></div></div>`)}
    ${only("embedding", `<div class="two"><div><label class="lbl" for="mf2-backend">向量协议</label><select class="select" id="mf2-backend" name="api_backend">${Object.entries(EMBED_BACKENDS).map(([v, n]) => `<option value="${v}" ${v === backend ? "selected" : ""}>${n}</option>`).join("")}</select></div>
        <div><label class="lbl" for="mf2-dim">维数</label><input class="input" id="mf2-dim" name="dimensions" type="number" min="1" value="${m?.dimensions ?? ""}" placeholder="留空：用模型返回的"></div></div>
      ${loose ? `<div class="two"><div><label class="lbl" for="mf2-url">Base URL</label><input class="input" id="mf2-url" name="base_url" value="${esc(m?.base_url || "")}" placeholder="http://127.0.0.1:11434"></div>
        <div><label class="lbl" for="mf2-key">API key</label><input class="input" id="mf2-key" name="api_key" type="password" autocomplete="new-password" placeholder="${m ? (m.key_set ? "已设置；留空不改" : "还没设置") : "本机端点不验 key 也要填一个值"}"></div></div>
        <p class="hint">key 存成环境变量${m?.env_key ? ` <span class="mono">${esc(m.env_key)}</span>` : "，名字默认是 <code>名字大写_API_KEY</code>"}，不进配置。</p>` : ""}
      <details class="adv"><summary>更多设置</summary><div class="two">
        ${loose ? "" : `<div><label class="lbl" for="mf2-url">Base URL</label><input class="input" id="mf2-url" name="base_url" value="${esc(own("base_url"))}" placeholder="留空：用 ${esc(pid)} 的"></div>`}
        <div><label class="lbl" for="mf2-env">凭证变量名</label><input class="input mono" id="mf2-env" name="env_key" value="${esc(loose ? m?.env_key || "" : own("env_key"))}" placeholder="${loose ? "名字大写_API_KEY" : `留空：用 ${esc(pid)} 的`}"></div></div></details>`)}
    ${failLine(P)}
    <div class="actions">${btn(P.busy ? "正在保存" : isNew ? "添加" : "保存", `type="submit" ${busy}`, "primary")}${btn("取消", 'type="button" data-cfg="cancel"')}</div></form>`;
}
function delAsk(P, kind, id, used) {
  if (!(P.confirm && P.confirm.kind === kind && P.confirm.id === id)) return "";
  return `<div class="danger"><p>${used}</p><div class="actions">${btn(`删掉 ${esc(id)}`, `data-cfg="del-${kind}-sure" data-id="${esc(id)}" ${P.busy ? "disabled" : ""}`, "danger-btn")}${btn("取消", 'data-cfg="cancel"')}</div></div>`;
}
function modelRow(P, m, def) {
  const editing = P.edit?.kind === "model" && P.edit.id === m.alias;
  const menu = (S.models || []).find((x) => x.id === m.alias);
  const win = m.context_window || menu?.context_window;
  return `<li class="pm${editing ? " editing" : ""}">
    <div class="pm-main"><span class="pm-name">${esc(m.name || m.alias)}${m.alias === def ? `<span class="pill">默认</span>` : ""}</span>
      <span class="pm-sub"><span class="mono">${esc(m.alias)}</span><span class="sep">·</span><span class="mono">${esc(m.model)}</span>${win ? `<span class="sep">·</span><span class="num">${kTokens(win)}</span> 窗口` : ""}${m.effort ? `<span class="sep">·</span>effort ${esc(m.effort)}` : ""}</span></div>
    ${m.standalone ? "" : `<span class="pm-act"><button class="text-btn quiet" data-cfg="edit-model" data-id="${esc(m.alias)}">编辑</button><button class="text-btn quiet danger-link" data-cfg="del-model" data-id="${esc(m.alias)}">删除</button></span>`}
    ${editing ? modelForm(P, m, m.provider, "completion") : ""}
    ${delAsk(P, "model", m.alias, `删掉之后菜单里没有它；已经开始的请求用的是开始时那份配置，不受影响。`)}</li>`;
}
function modelsSection(P) {
  const C = P.data.cfg;
  if (!C || C.error) return secHead("模型", "") + errLine(C);
  const def = C.default, used = C.memory_embedding;
  const completions = C.models, embeds = C.embeddings || [];
  const defaultPick = `<label class="def-pick"><span>默认模型</span><select class="select" id="cfg-default" ${P.busy ? "disabled" : ""}>${completions.map((m) => `<option value="${esc(m.alias)}" ${m.alias === def ? "selected" : ""}>${esc(m.name || m.alias)}</option>`).join("")}</select></label>`;
  const adding = (pid) => P.edit?.kind === "new-model" && P.edit.provider === pid;
  const addBtn = (pid, text) => `<button class="text-btn quiet add-model" data-cfg="new-model" data-provider="${esc(pid)}">${icon("plus")}${text}</button>`;
  const cards = C.providers.map((p) => {
    const mine = completions.filter((m) => m.provider === p.id), vecs = embeds.filter((e) => e.provider === p.id);
    const editing = P.edit?.kind === "provider" && P.edit.id === p.id;
    const n = mine.length + vecs.length;
    return `<section class="pcard${editing ? " editing" : ""}">
      <header class="pcard-head"><div class="pcard-title"><span class="pname">${esc(p.id)}</span><span class="tag">${esc((BACKENDS.find(([v]) => v === p.api_backend) || [0, p.api_backend || "协议没写"])[1])}</span>${keyLine(p)}</div>
        ${editing ? "" : `<span class="pact">${btn("编辑", `data-cfg="edit-provider" data-id="${esc(p.id)}"`, "sm")}<button class="text-btn quiet danger-link" data-cfg="del-provider" data-id="${esc(p.id)}">删除</button></span>`}</header>
      ${p.base_url && !editing ? `<p class="pcard-url mono">${esc(p.base_url)}</p>` : ""}
      ${editing ? providerForm(P, p) : ""}
      ${delAsk(P, "provider", p.id, n ? `还有 ${n} 个模型挂在它上面，Gateway 会拒绝；先删掉或改挂别的 provider。` : `删掉 provider ${esc(p.id)}；它的 API key 留在环境变量里。`)}
      <ul class="pms">${mine.map((m) => modelRow(P, m, def)).join("")}${vecs.map((e) => embeddingRow(P, e, used)).join("") || (n ? "" : `<li class="pm empty">还没有模型挂在它上面。</li>`)}</ul>
      ${adding(p.id) ? `<div class="pm-new">${modelForm(P, null, p.id)}</div>` : addBtn(p.id, "添加模型")}
    </section>`;
  }).join("");
  const looseChat = completions.filter((m) => m.standalone), looseVec = embeds.filter((e) => !e.provider);
  const looseCard = looseChat.length || looseVec.length || adding("")
    ? `<section class="pcard${adding("") ? " editing" : ""}"><header class="pcard-head"><div class="pcard-title"><span class="pname">独立模型</span><span class="muted small">端点直接写在模型项里，没挂 provider</span></div></header>
      <ul class="pms">${looseChat.map((m) => modelRow(P, m, def)).join("")}${looseVec.map((e) => embeddingRow(P, e, used)).join("")}</ul>
      ${looseChat.length ? `<p class="hint">这里的对话模型只能在配置文件里改。</p>` : ""}
      ${adding("") ? `<div class="pm-new">${modelForm(P, null, "")}</div>` : addBtn("", "添加向量模型")}</section>` : "";
  const addProvider = P.edit?.kind === "new-provider"
    ? `<section class="pcard editing"><header class="pcard-head"><div class="pcard-title"><span class="pname">添加 provider</span></div></header>${providerForm(P, null)}</section>`
    : `<div class="add-cards"><button class="add-card" data-cfg="new-provider">${icon("plus")}添加 provider</button>${looseCard ? "" : `<button class="add-card" data-cfg="new-model" data-provider="">${icon("plus")}添加独立的向量模型</button>`}</div>`;
  const noVec = embeds.length ? "" : `<p class="hint lead-hint">还没有向量模型：记忆只能按关键词检索。在 provider 里"添加模型"选类型"向量"，或添加一个独立的。</p>`;
  return secHead("模型", "改完马上生效：校验通过才保存，随后热重载。", completions.length ? defaultPick : "") + editNotice(P) + noVec + cards + looseCard + addProvider;
}
const EMBED_BACKENDS = { ollama_embeddings: "Ollama", embeddings: "OpenAI 兼容" };
// 向量模型和对话模型同一种行，名字后面一个"向量"标记；记忆在用的再标一个
function embeddingRow(P, e, used) {
  const editing = P.edit?.kind === "embed" && P.edit.id === e.alias, inUse = e.alias === used;
  const key = e.provider || !e.env_key ? "" : e.key_set ? `<span class="kstate ok"><i class="kdot"></i><span class="mono">${esc(e.env_key)}</span> 已设置</span>` : `<span class="kstate warn"><i class="kdot"></i>缺 <span class="mono">${esc(e.env_key)}</span></span>`;
  const sub = [`<span class="mono">${esc(e.model)}</span>`, esc(EMBED_BACKENDS[e.api_backend] || e.api_backend || "协议没写"), e.dimensions ? `<span class="num">${e.dimensions}</span> 维` : "", !e.provider && e.base_url ? `<span class="mono">${esc(e.base_url)}</span>` : "", key].filter(Boolean);
  return `<li class="pm${editing ? " editing" : ""}"><div class="pm-main"><span class="pm-name">${esc(e.alias)}<span class="tag">向量</span>${inUse ? `<span class="pill">记忆在用</span>` : ""}</span>
    <span class="pm-sub">${sub.join('<span class="sep">·</span>')}</span></div>
    ${editing ? "" : `<span class="pm-act">${inUse ? "" : `<button class="text-btn quiet" data-cfg="use-embed" data-id="${esc(e.alias)}">让记忆用它</button>`}<button class="text-btn quiet" data-cfg="edit-embed" data-id="${esc(e.alias)}">编辑</button><button class="text-btn quiet danger-link" data-cfg="del-embed" data-id="${esc(e.alias)}">删除</button></span>`}
    ${editing ? modelForm(P, e, e.provider || "", "embedding") : ""}
    ${delAsk(P, "embed", e.alias, inUse ? "记忆正用着它，Gateway 会拒绝；先让记忆换一个向量模型。" : `删掉向量模型 ${esc(e.alias)}；它的 key 留在环境变量里。`)}</li>`;
}
// 写完之后的一句回执；409 时给"重新读取"
function editNotice(P) {
  const R = P.result.edit;
  return R ? `<div class="notice${R.bad ? " bad-notice" : ""}"><p>${R.text}</p>${R.reread ? `<button class="text-btn" data-cfg="reread">重新读取</button>` : ""}</div>` : "";
}
// 记忆在用的向量模型换了，热重载装不上（向量后端只在启动后探测装上，docs/komo_bot.md §3）
const EMBED_RESTART = `记忆用的向量模型要重启 Gateway 才换上：在终端跑 <code>komo gateway restart</code>。重启后自动按新模型补建索引，建好之前检索只用关键词；相关度门槛是按旧模型校准的，换了模型可能要重调。`;
async function cfgSend(method, path, body, done, note = "") {
  const P = S.pg.settings;
  P.busy = "cfg"; P.fail = ""; render();
  try {
    // 三份视图共用一个 version：哪份读到了用哪份
    const version = [P.data.cfg, P.data.settings, P.data.env].find((x) => x?.version)?.version;
    const r = await api(path, { method, body: JSON.stringify({ version, ...body }) });
    P.data.cfg = r.models; P.data.settings = r.settings; P.data.env = r.env;
    api("/v1/config/check").then((x) => { P.data.config = x; render(); }).catch(() => {});
    const n = r.reload?.changed?.length || 0;
    P.result.edit = { text: `${done}。${n ? `重载装上了，变了 ${n} 个键` : "重载装上了"}${r.reload?.start_only?.length ? `；其中 ${r.reload.start_only.join("、")} 要重启 Gateway 才生效` : ""}。${note ? ` ${note}` : ""}` };
    P.edit = null; P.confirm = null;
    api("/v1/models").then((x) => { S.models = x.models; fillModels(); render(); }).catch(() => {});
  } catch (err) {
    if (err.status === 409 && /改过了/.test(err.message)) P.result.edit = { text: esc(err.message), bad: true, reread: true };
    else P.fail = err.message;
    if (P.confirm) { P.result.edit = { text: esc(err.message), bad: true }; P.confirm = null; }
  }
  P.busy = ""; render();
}
function cfgAct(el) {
  const P = S.pg.settings, a = el.dataset.cfg, id = el.dataset.id;
  P.fail = "";
  if (a === "cancel") { P.edit = null; P.confirm = null; render(); return; }
  if (a === "reread") { P.result.edit = null; reloadPage("settings"); return; }
  if (a === "new-provider") { P.edit = { kind: "new-provider" }; P.confirm = null; render(); setTimeout(() => $("pf-id")?.focus()); return; }
  if (a === "edit-provider") { P.edit = { kind: "provider", id }; P.confirm = null; render(); return; }
  if (a === "new-model") { P.edit = { kind: "new-model", provider: el.dataset.provider, type: "completion" }; P.confirm = null; render(); setTimeout(() => $("mf2-alias")?.focus()); return; }
  if (a === "edit-model") { P.edit = { kind: "model", id }; P.confirm = null; render(); return; }
  if (a === "edit-embed") { P.edit = { kind: "embed", id }; P.confirm = null; render(); return; }
  // 在模型一节点的：出错写在这一节顶上的回执里（confirm 让 cfgSend 这么做），不落到配置一节的卡片
  if (a === "use-embed") { P.confirm = { kind: "use-embed", id }; return cfgSend("PUT", "/v1/config/settings/memory.embedding", { value: id }, `记忆改用 ${esc(id)}`, EMBED_RESTART); }
  if (a === "del-provider" || a === "del-model" || a === "del-env" || a === "del-embed") { P.confirm = { kind: a.slice(4), id }; P.edit = null; render(); return; }
  if (a === "set-clear") return settingSend(el.dataset.key, null);
  if (a === "set-toggle") return settingSend(el.dataset.key, el.getAttribute("aria-checked") !== "true");
  if (a === "edit-env") { P.edit = { kind: "env", id }; P.confirm = null; render(); setTimeout(() => $("env-one")?.focus()); return; }
  if (a === "new-env") { P.edit = { kind: "env-new" }; P.envRows = [{ name: "", value: "" }]; P.confirm = null; render(); setTimeout(() => document.querySelector("[data-env-name]")?.focus()); return; }
  if (a === "env-row-add") { syncEnvRows(); P.envRows.push({ name: "", value: "" }); render(); setTimeout(() => [...document.querySelectorAll("[data-env-name]")].pop()?.focus()); return; }
  if (a === "env-row-del") { syncEnvRows(); P.envRows.splice(Number(el.dataset.row), 1); if (!P.envRows.length) P.envRows.push({ name: "", value: "" }); render(); return; }
  if (a === "del-env-sure") return cfgSend("DELETE", `/v1/config/env/${encodeURIComponent(id)}`, {}, `删掉了 ${esc(id)}`);
  if (a === "del-provider-sure") return cfgSend("DELETE", `/v1/config/model-providers/${encodeURIComponent(id)}`, {}, `删掉了 provider ${esc(id)}`);
  if (a === "del-embed-sure") return cfgSend("DELETE", `/v1/config/embedding-models/${encodeURIComponent(id)}`, {}, `删掉了向量模型 ${esc(id)}`);
  if (a === "del-model-sure") return cfgSend("DELETE", `/v1/config/models/${encodeURIComponent(id)}`, {}, `删掉了模型 ${esc(id)}`);
}
function cfgSubmit(form) {
  const f = new FormData(form), v = (k) => String(f.get(k) ?? "").trim();
  if (form.id === "cfg-setting-num") return settingSend(form.dataset.key, v("v") === "" ? null : Number(v("v")));
  if (form.id === "cfg-env-one") return cfgSend("PUT", "/v1/config/env", { vars: [{ name: form.dataset.name, value: String(f.get("value") ?? "") }] }, `${esc(form.dataset.name)} 设好了`);
  if (form.id === "cfg-env-form") {
    syncEnvRows();
    const vars = S.pg.settings.envRows.map((r) => ({ name: r.name.trim(), value: r.value })).filter((r) => r.name || r.value);
    if (!vars.length) return;
    return cfgSend("PUT", "/v1/config/env", { vars }, vars.length === 1 ? `${esc(vars[0].name)} 设好了` : `设好了 ${vars.length} 个变量`);
  }
  const num = (k) => (v(k) ? Number(v(k)) : undefined);
  if (form.id === "cfg-provider-form") {
    const id = form.dataset.id || v("pid"); // 字段不能叫 id：会遮住 form.id，提交监听就认不出这张表
    const body = { api_backend: v("api_backend"), base_url: v("base_url") || undefined, env_key: v("env_key") || undefined, auth: v("auth") || undefined, timeout_secs: num("timeout_secs"), api_key: v("api_key") || undefined };
    return cfgSend("PUT", `/v1/config/model-providers/${encodeURIComponent(id)}`, body, form.dataset.id ? `保存了 provider ${esc(id)}` : `添加了 provider ${esc(id)}`);
  }
  if (form.id === "cfg-model-form" && form.dataset.type === "embedding") {
    const alias = form.dataset.alias || v("alias");
    const body = { model: v("model"), api_backend: v("api_backend"), provider: v("provider") || undefined, base_url: v("base_url") || undefined, env_key: v("env_key") || undefined, dimensions: num("dimensions"), api_key: v("api_key") || undefined };
    const inUse = alias === S.pg.settings.data.cfg?.memory_embedding;
    return cfgSend("PUT", `/v1/config/embedding-models/${encodeURIComponent(alias)}`, body, form.dataset.alias ? `保存了向量模型 ${esc(alias)}` : `添加了向量模型 ${esc(alias)}`, inUse ? EMBED_RESTART : "");
  }
  const alias = form.dataset.alias || v("alias");
  const efforts = v("efforts") ? v("efforts").split(/[,，\s]+/).filter(Boolean) : undefined;
  const body = { provider: v("provider"), model: v("model"), name: v("name") || undefined, effort: v("effort") || undefined, efforts, context_window: num("context_window") };
  return cfgSend("PUT", `/v1/config/models/${encodeURIComponent(alias)}`, body, form.dataset.alias ? `保存了模型 ${esc(alias)}` : `添加了模型 ${esc(alias)}`);
}

// —— 配置：一项一张卡，左边说它是什么，右边直接改 ——
function settingSend(key, value) {
  const label = SETTING_COPY[key]?.[0] || key;
  S.pg.settings.failKey = key; // 失败原因写在这一项的卡片里
  return cfgSend("PUT", `/v1/config/settings/${encodeURIComponent(key)}`, { value }, value === null ? `${label}回到了默认` : `${label}改好了`, key === "memory.embedding" ? EMBED_RESTART : "");
}
function choiceText(P, key, v) {
  if (key === "memory.model") { const m = (P.data.cfg?.models || []).find((x) => x.alias === v); return m?.name || v; }
  if (key === "memory.embedding") { const m = (P.data.cfg?.embeddings || []).find((x) => x.alias === v); return m ? `${m.model}（${v}）` : v; }
  return CHOICE_TEXT[v] || v;
}
function settingControl(P, x) {
  const dis = P.busy ? "disabled" : "", [label, , unit] = SETTING_COPY[x.key] || [x.key];
  const eff = x.effective;
  if (x.kind === "bool") {
    return `<button type="button" class="switch" role="switch" aria-checked="${eff === true}" aria-label="${esc(label)}" data-cfg="set-toggle" data-key="${esc(x.key)}" ${dis}><i></i></button>`;
  }
  if (x.kind === "choice") {
    // 没写的项头一个选项是"默认"，选它就删掉那一行；default_agent 没有默认，不给这一项
    const unsetText = x.key === "memory.model" ? `跟默认模型（${esc(choiceText(P, x.key, eff))}）` : x.key === "memory.embedding" ? "不设（只按关键词检索）" : `默认（${esc(choiceText(P, x.key, eff))}）`;
    const first = x.key === "default_agent" ? "" : `<option value="" ${x.value == null ? "selected" : ""}>${unsetText}</option>`;
    return `<select class="select" data-setting="${esc(x.key)}" aria-label="${esc(label)}" ${dis}>${first}${(x.options || []).map((o) => `<option value="${esc(o)}" ${o === x.value ? "selected" : ""}>${esc(choiceText(P, x.key, o))}</option>`).join("")}</select>`;
  }
  return `<form class="snum" id="cfg-setting-num" data-key="${esc(x.key)}"><input class="input num" type="number" name="v" aria-label="${esc(label)}" ${x.min != null ? `min="${x.min}"` : ""} ${x.max != null ? `max="${x.max}"` : ""} value="${x.value ?? ""}" placeholder="${eff ?? ""}"><span class="unit">${unit || ""}</span>${btn("保存", `type="submit" ${dis}`, "sm")}</form>`;
}
function configSection(P) {
  const cfg = P.data.config, X = P.data.settings, R = P.result, busy = (k) => (P.busy === k ? "disabled" : "");
  const issues = cfg?.issues || [];
  const stale = (cfg?.sources || []).some((s) => s.mtime > cfg.loaded_at);
  const reload = R.reload;
  const status = cfg?.error ? `<section class="scard">${errLine(cfg)}</section>` : `<section class="scard scard-status">
    <div class="scard-row"><div class="scard-text"><h4>${issues.length ? `配置有 <span class="num">${issues.length}</span> 条提醒` : "配置检查通过"}</h4>
      <p>生效的配置装于 <span class="num">${day(cfg.loaded_at)} ${clock(new Date(cfg.loaded_at))}</span>${stale ? `，<span class="bad">之后文件改过，还没装上</span>` : ""}。</p></div>
      <div class="scard-ctl">${btn(P.busy === "reload" ? "正在加载" : "重新加载", `data-pg-act="config-reload" ${busy("reload")}`, "sm")}${btn("重新检查", `data-pg-act="config-check" ${busy("check")}`, "sm")}</div></div>
    ${issues.length ? `<ul class="evs">${issues.map((x) => `<li class="ev"><span class="ev-line"><span class="pill${x.severity === "error" ? " bad" : " wait"}">${x.severity === "error" ? "错误" : "提醒"}</span><span class="mono">${esc(x.key)}</span></span><span>${esc(x.message)}</span></li>`).join("")}</ul>` : ""}
    ${reload ? `<div class="notice">${reload.error ? `<p class="bad">没装上：${esc(reload.error)}</p>` : `<p>${reload.changed.length ? `装上了，变了 ${reload.changed.length} 个键：<span class="num">${reload.changed.map(esc).join("、")}</span>` : "装上了，没有键变化"}。</p>
      ${reload.start_only.length ? `<p class="bad">其中 <span class="num">${reload.start_only.map(esc).join("、")}</span> 只在启动时生效，要重启 Gateway（<code>komo gateway restart</code>）。</p>` : ""}`}</div>` : ""}</section>`;
  const rows = !X || X.error ? errLine(X) : X.settings.map((x) => {
    const [label, desc] = SETTING_COPY[x.key] || [x.key, ""];
    const custom = x.value != null && x.key !== "default_agent" && x.kind !== "choice";
    return `<section class="scard"><div class="scard-row${x.kind === "bool" ? " bool" : ""}"><div class="scard-text"><h4>${esc(label)}</h4><p>${desc}${custom ? ` <button class="text-btn quiet" data-cfg="set-clear" data-key="${esc(x.key)}">恢复默认</button>` : ""}</p></div>
      <div class="scard-ctl">${settingControl(P, x)}</div></div>${P.failKey === x.key ? failLine({ fail: P.fail.replace(`\`${x.key}\``, label) }) : ""}</section>`;
  }).join("");
  return secHead("配置", "常用的几项，改完马上生效。其余的在配置文件里改，保存后 Gateway 自己装上。") + editNotice(P) + status + rows;
}

// —— 环境变量：API key 与其他凭证。值只进不出 ——
function syncEnvRows() {
  const P = S.pg.settings;
  document.querySelectorAll("[data-env-name]").forEach((el) => { P.envRows[Number(el.dataset.envName)].name = el.value; });
  document.querySelectorAll("[data-env-value]").forEach((el) => { P.envRows[Number(el.dataset.envValue)].value = el.value; });
}
// 把整段 .env 粘进名字框：一行一个变量，# 开头的跳过，值两边的引号去掉
function envPaste(ev) {
  const el = ev.target.closest?.("[data-env-name]");
  const text = ev.clipboardData?.getData("text") || "";
  if (!el || !/[=\n]/.test(text)) return;
  const rows = text.split(/\r?\n/).map((l) => l.trim()).filter((l) => l && !l.startsWith("#") && l.includes("="))
    .map((l) => { const [k, ...rest] = l.replace(/^export\s+/, "").split("="); return { name: k.trim(), value: rest.join("=").trim().replace(/^(['"])(.*)\1$/, "$2") }; });
  if (!rows.length) return;
  ev.preventDefault();
  const P = S.pg.settings;
  syncEnvRows();
  const at = Number(el.dataset.envName);
  P.envRows.splice(at, P.envRows[at].name || P.envRows[at].value ? 0 : 1, ...rows);
  render();
}
function envSection(P) {
  const E = P.data.env, dis = P.busy ? "disabled" : "";
  if (!E || E.error) return secHead("环境变量", "") + errLine(E);
  const rows = E.vars.map((v) => {
    v.used_by ||= []; // 没人用时 Gateway 省掉这个字段
    const editing = P.edit?.kind === "env" && P.edit.id === v.name;
    const state = v.set ? `<span class="kstate ok"><i class="kdot"></i>已设置</span>` : `<span class="kstate warn"><i class="kdot"></i>${v.in_file ? "是空的" : "缺"}</span>`;
    return `<li class="envrow${editing ? " editing" : ""}">
      <div class="env-main"><span class="env-name mono">${esc(v.name)}</span>${state}${v.used_by.length ? `<span class="env-use">${v.used_by.map(esc).join("、")} 在用</span>` : ""}</div>
      ${editing ? "" : `<span class="pm-act"><button class="text-btn quiet" data-cfg="edit-env" data-id="${esc(v.name)}">${v.in_file ? "改值" : "设置"}</button>${v.in_file ? `<button class="icon-btn danger-icon" data-cfg="del-env" data-id="${esc(v.name)}" aria-label="删掉 ${esc(v.name)}" title="删掉">${icon("trash")}</button>` : ""}</span>`}
      ${editing ? `<form class="env-one" id="cfg-env-one" data-name="${esc(v.name)}"><input class="input num" id="env-one" name="value" type="password" autocomplete="new-password" required placeholder="${v.set ? "新值，会替换现在的" : "值"}">${btn(P.busy ? "正在保存" : "保存", `type="submit" ${dis}`, "sm primary")}${btn("取消", 'type="button" data-cfg="cancel"', "sm")}${failLine(P)}</form>` : ""}
      ${delAsk(P, "env", v.name, v.used_by.length ? `${v.used_by.map(esc).join("、")} 还在用它，Gateway 会拒绝。` : `删掉之后，读它的地方拿不到值。`)}</li>`;
  }).join("");
  const adding = P.edit?.kind === "env-new";
  const form = adding ? `<form class="envform" id="cfg-env-form">
      ${P.envRows.map((r, i) => `<div class="kvpair">
        <label class="kv-k" for="envn-${i}">名字</label><input class="input mono" id="envn-${i}" data-env-name="${i}" value="${esc(r.name)}" placeholder="MY_API_KEY" autocomplete="off" spellcheck="false">
        <button type="button" class="icon-btn danger-icon" data-cfg="env-row-del" data-row="${i}" aria-label="去掉这一行">${icon("trash")}</button>
        <label class="kv-k" for="envv-${i}">值</label><input class="input num" id="envv-${i}" data-env-value="${i}" type="password" value="${esc(r.value)}" placeholder="值" autocomplete="new-password"></div>`).join("")}
      <div class="kv-more">${btn(`${icon("plus")}添加`, 'type="button" data-cfg="env-row-add"', "sm")}<span class="muted">或把 .env 的内容整段粘贴到名字框里。</span></div>
      ${failLine(P)}
      <div class="kv-foot">${btn("取消", 'type="button" data-cfg="cancel"')}${btn(P.busy ? "正在保存" : "保存", `type="submit" ${dis}`, "primary")}</div></form>`
    : `<button class="add-card" data-cfg="new-env">${icon("plus")}添加变量</button>`;
  return secHead("环境变量", "API key 与其他凭证。值只能写入、不再显示；改完马上生效。") + editNotice(P)
    + (E.vars.length ? `<ul class="envlist">${rows}</ul>` : "") + form;
}

function indexSection(P) {
  const ix = P.data.index, R = P.result;
  if (ix.error) return secHead("记忆索引", "") + errLine(ix);
  return secHead("记忆索引", "记忆检索的向量部分；索引不可用时检索按关键词降级。") + `<div class="set-panel">${kvBlock([
      ["向量", `${esc(ix.space?.model || "")} · ${esc(ix.space?.provider || "")}`],
      ["端点", `<span class="mono">${esc(ix.space?.endpoint || "")}</span>`],
      ["状态", `${{ ready: "就绪", building: "正在建", stale: "要重建", unavailable: "不可用" }[ix.state] || esc(ix.state)} · 已索引 <span class="num">${ix.indexed} / ${ix.total}</span> 条生效的`],
      ix.errors?.length ? ["错误", `<span class="bad">${ix.errors.map(esc).join("；")}</span>`] : null,
    ])}
    ${R.rebuild ? `<div class="notice"><p>${R.rebuild.error ? `<span class="bad">没提交：${esc(R.rebuild.error)}</span>` : R.rebuild.accepted ? `已提交重建（代次 <span class="num">${esc(R.rebuild.generation)}</span>），后台跑完前检索按关键词降级。` : "已经有一次重建在跑，没有再起一个。"}</p></div>` : ""}
    <div class="actions">${btn(P.busy === "rebuild" ? "正在提交" : "重建索引", `data-pg-act="index-rebuild" ${P.busy === "rebuild" ? "disabled" : ""}`)}</div></div>`;
}
const MCP_EXPOSURE = { codemode: "脚本里调", direct: "直接给模型", hidden: "不挂" };
function mcpStateLine(x) {
  if (x.state === "connected") return `<span class="kstate ok"><i class="kdot"></i>已连上</span>`;
  if (x.state === "connecting") return `<span class="kstate"><span class="spinner sm" aria-hidden="true"></span>连接中</span>`;
  if (x.state === "needs_auth") return `<span class="kstate warn"><i class="kdot"></i>要登录</span>`;
  return `<span class="kstate bad"><i class="kdot"></i>${MCP_STATE[x.state] || esc(x.state)}</span>`;
}
const mcpWay = (x) => (x.transport === "http" ? "远程" : "本机进程") + (x.oauth ? `<span class="tag">OAuth</span>` : "");
function mcpSection(P) {
  const R = P.data.mcp, busy = P.busy === "mcp";
  // 404 = 在跑的 Gateway 早于 GET /v1/mcp：说清楚，别只甩一个状态码
  if (R?.error && /404/.test(R.error)) return secHead("MCP", "") + `<div class="set-panel"><p class="set-line muted">在跑的 Gateway 还没有服务器列表的接口；装上新版后这里列出每台服务器和它的工具。</p></div>`;
  if (!R || R.error) return secHead("MCP", "") + errLine(R);
  const failed = P.result.mcp?.error ? `<p class="form-err">重连没成：${esc(P.result.mcp.error)}</p>` : "";
  const x = P.mcpSel && R.servers.find((s) => s.name === P.mcpSel);
  if (x) {
    const tools = x.tools || [];
    const count = x.state === "connected" ? `<span class="sep">·</span><span class="num">${tools.length}</span> 个工具` : "";
    const exposure = (t) => (t.exposure === x.exposure ? "" : `<span class="tag">${MCP_EXPOSURE[t.exposure] || esc(t.exposure)}</span>`);
    return `<div class="set-head"><div><button type="button" class="text-btn quiet back-link" data-mcp-back>${icon("left")}全部服务器</button></div></div>
      <section class="scard"><div class="scard-row"><div class="scard-text"><h4 class="mono">${esc(x.name)}</h4><p>${mcpStateLine(x)}${count}<span class="sep">·</span>${mcpWay(x)}<span class="sep">·</span>工具默认${MCP_EXPOSURE[x.exposure] || esc(x.exposure)}</p></div>
        <div class="scard-ctl">${btn(busy ? "正在重连" : "重连", `data-pg-act="mcp-reconnect" data-id="${esc(x.name)}" ${busy ? "disabled" : ""}`, "sm")}</div></div>
        ${x.description ? `<p class="mcp-desc">${esc(x.description)}</p>` : ""}
        ${x.state === "needs_auth" ? `<p class="hint">在终端跑 <code>komo auth mcp login ${esc(x.name)}</code>，登录完下一次调用自动连上。</p>` : x.detail ? `<p class="hint bad">${esc(x.detail)}</p>` : ""}
        ${failed}</section>
      ${tools.length ? `<ul class="envlist">${tools.map((t) => `<li class="envrow"><div class="env-main"><span class="env-name mono">${esc(t.name)}</span>${exposure(t)}${t.outward ? `<span class="pill wait">对外</span>` : ""}${t.read_only ? `<span class="pill">只读</span>` : ""}${t.description ? `<span class="env-use">${esc(t.description)}</span>` : ""}</div></li>`).join("")}</ul>`
        : `<p class="set-line muted">${x.state === "connected" ? "这台服务器没有列出工具。" : "连上之后这里列出它的工具。"}</p>`}`;
  }
  const rows = R.servers.map((s) => `<button type="button" class="mcp-row" data-mcp-sel="${esc(s.name)}"><span class="mcp-name mono">${esc(s.name)}</span><span class="mcp-way">${mcpWay(s)}</span><span class="mcp-tools num">${s.state === "connected" ? (s.tools || []).length : "—"}</span><span class="mcp-st">${mcpStateLine(s)}</span>${icon("right")}</button>`).join("");
  return secHead("MCP", "外部工具服务器。增删改在配置文件里；这里看连没连上、有哪些工具。", R.servers.length ? btn(busy ? "正在重连" : "重连全部", `data-pg-act="mcp-reconnect" ${busy ? "disabled" : ""}`, "sm") : "") + failed
    + (R.servers.length ? `<div class="mcp-table"><div class="mcp-head"><span>服务器</span><span>方式</span><span>工具</span><span>状态</span><span></span></div>${rows}</div>` : `<div class="set-panel"><p class="set-line muted">没有配 MCP 服务器。</p></div>`);
}
function gatewaySection(P) {
  const h = P.data.health;
  return secHead("Gateway", "唯一持有状态的进程；Web、TUI、CLI 都是它的客户端。") + `<div class="set-panel">${h.error ? errLine(h) : kvBlock([
    ["版本", `<span class="num">${esc(h.version)}</span> · 协议 <span class="num">${h.protocol_version}</span>`],
    ["启动于", `<span class="num">${day(h.started_at)} ${clock(new Date(h.started_at))}</span>`],
    ["实例", `<span class="mono">${esc(h.instance_id)}</span>`],
    ["数据目录", `<span class="mono">${esc(h.data_dir)}</span>`],
    ["遥测", h.telemetry?.endpoint ? `${esc(h.telemetry.endpoint)} · ${h.telemetry.last_error ? `<span class="bad">出错：${esc(h.telemetry.last_error)}</span>` : h.telemetry.last_ok_at ? `正常，最近一次 ${ago(h.telemetry.last_ok_at)}` : "还没导出过"}` : `<span class="muted">没配</span>`],
  ])}</div>`;
}
function channelsSection() {
  return secHead("渠道", "飞书、Telegram、微信。谁能说话、哪个会话收主动消息，在配置文件里设；凭证见环境变量一节。") + `<div class="set-panel"><p class="set-line muted">渠道的连接状态 Gateway 还没有接口可查；在终端里用 <code>komo channel list</code> 与 <code>komo channel probe</code>。</p></div>`;
}
async function settingsAct(act, el) {
  const P = S.pg.settings;
  const key = { "config-reload": "reload", "config-check": "check", "index-rebuild": "rebuild", "mcp-reconnect": "mcp" }[act];
  P.busy = key; render();
  try {
    if (act === "config-reload") P.result.reload = await api("/v1/config/reload", { method: "POST", body: "{}" });
    if (act === "index-rebuild") P.result.rebuild = await api("/v1/memory-index/rebuild", { method: "POST", body: JSON.stringify({}) });
    if (act === "mcp-reconnect") P.result.mcp = await api("/v1/mcp/reconnect", { method: "POST", body: JSON.stringify({ servers: el?.dataset.id ? [el.dataset.id] : [] }) });
    await LOADERS.settings(P);
  } catch (err) { P.result[key] = { error: err.message }; }
  P.busy = ""; render();
}
const PAGE_RENDER = { memory: renderMemory, matters: renderMatters, cron: renderCron, inbox: renderInbox };

/* ---------- 总渲染 ---------- */
let frame = 0;
function render({ stick = false, force = false } = {}) {
  cancelAnimationFrame(frame);
  frame = requestAnimationFrame(() => {
    const sc = $("scroll");
    const atBottom = sc.scrollHeight - sc.scrollTop - sc.clientHeight < 80;
    const onPage = S.page !== "session";
    app.classList.toggle("on-page", onPage);
    for (const [k, id] of Object.entries(PAGES)) $(id).hidden = S.page !== k;
    // 当前页挪到 main 最前：读屏与检查工具认第一个 h1，隐藏页的 h1 不该排在它前面
    if (onPage && document.querySelector(".main").firstElementChild !== $(PAGES[S.page])) document.querySelector(".main").prepend($(PAGES[S.page]));
    document.querySelectorAll("[data-nav]").forEach((a) => a.toggleAttribute("aria-current", a.dataset.nav === S.page));
    renderGroups(); renderConn(); renderNavCounts();
    if ($("settings").open) renderSettings();
    if (onPage) { PAGE_RENDER[S.page](); document.title = `${PAGE_NAME[S.page]} · komo`; return; }
    app.classList.remove("detail-open");
    renderThread(); renderComposer(); renderPanel();
    if ((stick && atBottom) || force) { sc.scrollTop = sc.scrollHeight; }
    if (S.view && S.view.status === "ready" && !S.view.scrolled) { sc.scrollTop = sc.scrollHeight; S.view.scrolled = true; }
  });
}
setInterval(() => { if (S.view?.runs.size || S.sessions.some((s) => running(s.session))) render(); }, 1000);
// 待处理的数量不随会话事件流来（在别的页面上没有流）：半分钟取一次
setInterval(() => { if (S.boot === "ready") api("/v1/interventions").then((r) => { S.inbox = r.interventions; renderNavCounts(); }).catch(() => {}); }, 30000);

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

  const pgSel = t.closest("[data-pg-sel]");
  if (pgSel) {
    const page = pgSel.dataset.pgSel;
    if (S.page !== page) location.hash = `#/${page}/${pgSel.dataset.id}`;
    else selectIn(page, pgSel.dataset.id);
    return;
  }
  if (t.closest("#open-settings")) { app.classList.remove("nav-open"); openSettings(); return; }
  if (t.closest("#settings-close")) { $("settings").close(); return; }
  const sec = t.closest("[data-set-sec]");
  if (sec) { settingsSection(sec.dataset.setSec); return; }
  const mcpSel = t.closest("[data-mcp-sel]");
  if (mcpSel) { S.pg.settings.mcpSel = mcpSel.dataset.mcpSel; $("settings-body").scrollTop = 0; render(); return; }
  if (t.closest("[data-mcp-back]")) { S.pg.settings.mcpSel = null; render(); return; }
  if (t.closest("[data-pg-close]")) { closeDetail(S.page); return; }
  if (t.closest("[data-pg-reload]")) { reloadPage(t.closest("#settings") ? "settings" : S.page); return; }
  const addBtn = t.closest("#matters-add");
  if (addBtn) { selectIn(addBtn.id.split("-")[0], "new"); setTimeout(() => $("mn-title")?.focus()); return; }
  const ca = t.closest("[data-cfg]");
  if (ca) { cfgAct(ca); return; }
  const pa = t.closest("[data-pg-act]");
  if (pa) { pageAct(pa); return; }
  const M = S.mem;
  const rung = t.closest("[data-rung]");
  if (rung) {
    const k = rung.dataset.rung;
    // 点在已选的一档上回到"在起作用"；前两档本来就在"在起作用"里亮着，点了只看那一档
    M.bucket = M.bucket === k ? "live" : k;
    M.hits = null; M.query = ""; $("mem-query").value = ""; M.selected = null; M.confirmForget = M.classify = false; M.fail = "";
    setMemHash(); render(); return;
  }
  const memBtn = t.closest("[data-mem]");
  // 窄屏的详情是推进去的一层：进去时记下列表滚到哪，回来时还原
  if (memBtn) {
    // 从图上点的那一条可能不在当前这一档：换到它所在的档，列表里也看得到它
    const hit = S.mem && S.memories.get(memBtn.dataset.mem);
    if (hit && !M.hits && !inBucket(hit, M.bucket)) M.bucket = hit.state === "active" ? "live" : rungOf(hit);
    if (!wide()) { M.listScroll = $("mem").scrollTop; $("mem").scrollTop = 0; } M.selected = memBtn.dataset.mem; M.confirmForget = M.classify = false; M.fail = ""; setMemHash(); render(); return;
  }
  if (t.closest("#mem-add")) { if (!wide()) { M.listScroll = $("mem").scrollTop; $("mem").scrollTop = 0; } M.selected = "new"; M.fail = ""; render(); setTimeout(() => $("f-content")?.focus()); return; }
  if (t.closest("[data-mem-close]")) {
    M.selected = null; M.fail = ""; setMemHash(); render();
    if (!wide()) requestAnimationFrame(() => requestAnimationFrame(() => { $("mem").scrollTop = M.listScroll || 0; }));
    return;
  }
  if (t.closest("[data-mem-reload]")) { openMemory(M.selected); return; }
  const ma = t.closest("[data-mem-act]");
  if (ma) {
    const act = ma.dataset.memAct, id = ma.dataset.id || M.selected;
    if (act === "forget-ask") M.confirmForget = true;
    else if (act === "forget-cancel") M.confirmForget = false;
    else if (act === "classify-open") M.classify = true;
    else if (act === "classify-cancel") M.classify = false;
    else if (act === "classify-ref") { memClassify(id, "reference"); return; }
    else { if (act === "confirm" && !t.closest(".page-detail")) M.selected = id; memAct(act, id); return; }
    render(); return;
  }
  if (t.closest("[data-retry-boot]")) boot();
  if (t.closest("[data-retry-session]")) openSession(S.current);
  const rs = t.closest("[data-retry-send]");
  if (rs) retry(rs.dataset.retrySend);
  if (t.closest("#new-session")) newSession();
  if (t.closest("#stop-run")) cancelRuns();
  if (t.closest("#toggle-panel")) { app.classList.toggle(wide() ? "panel-closed" : "panel-open"); syncPanelButton(); }
  if (t.closest("[data-open-nav]")) app.classList.add("nav-open");
  if (t.closest("[data-nav]")) app.classList.remove("nav-open");
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
addEventListener("resize", () => { syncPanelButton(); if (S.page === "memory") render(); }); // 图按容器宽度画
// 点在弹窗外的遮罩上关；关掉时丢下没提交的表单和删除确认
$("settings").addEventListener("click", (ev) => { if (ev.target === ev.currentTarget) ev.currentTarget.close(); });
$("settings").addEventListener("close", () => { const P = S.pg.settings; P.edit = null; P.confirm = null; P.fail = ""; });
addEventListener("hashchange", (ev) => {
  if (S.boot !== "ready") return;
  const sec = routeSettings();
  if (sec) { history.replaceState(null, "", new URL(ev.oldURL).hash || "#/"); openSettings(sec); return; }
  const r = routePage();
  if (r) {
    if (S.page !== r.page) openPage(r.page, r.id);
    else { const P = r.page === "memory" ? S.mem : S.pg[r.page]; if (P && r.id !== P.selected) { P.selected = r.id; P.fail = ""; P.form = null; render(); } }
    return;
  }
  const id = routeSession();
  if (id && (id !== S.current || S.page !== "session")) openSession(id);
});
function pageAct(el) {
  const P = el.closest("#settings") ? S.pg.settings : S.pg[S.page], a = el.dataset.pgAct;
  if (!P) return;
  if (a === "form") { P.form = P.form === el.dataset.verb ? null : el.dataset.verb; P.fail = ""; render(); return; }
  if (a === "form-cancel") { P.form = null; P.fail = ""; render(); return; }
  if (a === "confirm") { P.confirm = el.dataset.what; render(); return; }
  if (a === "confirm-cancel") { P.confirm = null; render(); return; }
  if (a === "toggle-closed") { P.showClosed = !P.showClosed; render(); return; }
  if (a === "verb") return matterVerb(el.dataset.verb);
  if (a === "unlink") return matterVerb("unlink", { trigger: JSON.parse(el.dataset.trigger) });
  if (a.startsWith("cron-")) return cronAct(a);
  if (a === "answer") return inboxAnswer(el.dataset.verdict, !!el.dataset.sure);
  if (a === "config-check") { P.busy = "check"; render(); return LOADERS.settings(P).then(() => { P.busy = ""; render(); }); }
  return settingsAct(a, el);
}
function matterFormSubmit(form) {
  const m = S.pg.matters.items.find((x) => x.id === S.pg.matters.selected), f = new FormData(form), verb = form.dataset.verb;
  const v = (k) => String(f.get(k) ?? "").trim();
  if (verb === "next") return matterVerb("next", { next: v("next") });
  if (verb === "wait") return matterVerb("wait", { waiting_for: v("waiting_for") });
  if (verb === "resume") return matterVerb("resume", v("next") ? { next: v("next") } : {});
  if (verb === "link") return matterVerb("link", { trigger: { kind: "cron", job: v("job") } });
  const fields = { title: v("title") };
  if (m.kind === "goal") { if (v("due")) fields.due = v("due"); else if (m.due) fields.clear_due = true; }
  else fields.standard = v("standard");
  return matterVerb("edit", fields);
}
document.addEventListener("submit", (ev) => {
  if (ev.target.id === "matter-form") { ev.preventDefault(); matterFormSubmit(ev.target); }
  if (ev.target.id === "matter-new") { ev.preventDefault(); matterCreate(ev.target); }
  if (/^cfg-(provider-form|model-form|setting-num|env-one|env-form)$/.test(ev.target.id)) { ev.preventDefault(); cfgSubmit(ev.target); }
  if (ev.target.id === "mem-form") { ev.preventDefault(); memAdd(ev.target); }
  if (ev.target.id === "classify-form") { ev.preventDefault(); memClassify(S.mem.selected, "convention", new FormData(ev.target).get("applies_when").trim()); }
});
document.addEventListener("change", (ev) => {
  // 新建事项表单里的单选决定显示哪组字段（目标 / 领域）
  if (ev.target.type === "radio" && ev.target.form?.id === "matter-new") {
    ev.target.form.querySelectorAll("[data-for]").forEach((d) => { d.hidden = d.dataset.for !== ev.target.value; });
  }
  if (ev.target.dataset?.setting) { settingSend(ev.target.dataset.setting, ev.target.value === "" ? null : ev.target.value); return; }
  if (ev.target.id === "mf2-type") {
    const f = ev.target.form, t = ev.target.value;
    f.dataset.type = t; f.querySelectorAll("[data-only]").forEach((d) => { d.hidden = d.dataset.only !== t; });
    if (S.pg.settings.edit) S.pg.settings.edit.type = t;
    return;
  }
  if (ev.target.id === "cfg-default") { cfgSend("PUT", "/v1/config/default-model", { alias: ev.target.value }, `默认模型改成了 ${esc(ev.target.value)}`); return; }
  if (ev.target.id === "mem-scope") { S.mem.scope = ev.target.value; S.mem.selected = null; render(); }
  if (ev.target.name === "application" && ev.target.form?.id === "mem-form") {
    const conv = ev.target.value === "convention";
    $("f-when-wrap").hidden = !conv; $("f-when").required = conv;
  }
});
document.addEventListener("paste", envPaste);
$("mem-query").addEventListener("input", (ev) => { S.mem.selected = null; memSearch(ev.target.value.trim()); });

syncPanelButton();
boot();
