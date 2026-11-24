// djs codemode 前置引导脚本（独立编译并固化进进程级 Snapshot，不占用户脚本行号）。
// 宿主预注入了 `__acall`、`__text`、`__console`；每次运行调用 `__initRuntime(toolsList, catalog)`
// 绑定本次能力面里的工具表与发现目录。
(() => {
  class ToolError extends Error {
    constructor(message) {
      super(message);
      this.name = "ToolError";
    }
  }

  function _formatValue(value) {
    if (typeof value === "string") return value;
    if (value === undefined) return "undefined";
    const seen = new Set();
    return JSON.stringify(
      value,
      (_k, v) => {
        if (typeof v === "bigint") return v.toString();
        if (v instanceof Error) return { name: v.name, message: v.message };
        if (v instanceof Set) return Array.from(v);
        if (v instanceof Map) return Object.fromEntries(v);
        if (v && typeof v === "object") {
          if (seen.has(v)) return "[Circular]";
          seen.add(v);
        }
        return v;
      },
      2,
    );
  }

  function text(value) {
    try {
      __text(_formatValue(value));
    } catch (e) {
      throw new ToolError(e && typeof e.message === "string" ? e.message : String(e));
    }
  }

  function _logLine(args) {
    const parts = [];
    for (let i = 0; i < args.length; i++) {
      const v = args[i];
      if (typeof v === "string") parts.push(v);
      else if (v === null) parts.push("null");
      else if (v === undefined) parts.push("undefined");
      else if (typeof v === "object") {
        try {
          parts.push(JSON.stringify(v));
        } catch {
          parts.push(String(v));
        }
      } else {
        parts.push(String(v));
      }
    }
    __console(parts.join(" ") + "\n");
  }

  const console = Object.freeze({
    log(...args) { _logLine(args); },
    info(...args) { _logLine(args); },
    warn(...args) { _logLine(args); },
    error(...args) { _logLine(args); },
    debug(...args) { _logLine(args); },
  });

  // ---------------------------------------------------------------- 工具名相似度与绑定
  function _compact(name) {
    return name.replace(/[_-]/g, "").toLowerCase();
  }

  function _levenshtein(a, b) {
    const row = [];
    for (let j = 0; j <= b.length; j++) row[j] = j;
    for (let i = 0; i < a.length; i++) {
      let prev = row[0];
      row[0] = i + 1;
      for (let j = 0; j < b.length; j++) {
        const cur = row[j + 1];
        row[j + 1] = Math.min(prev + (a[i] === b[j] ? 0 : 1), row[j] + 1, cur + 1);
        prev = cur;
      }
    }
    return row[b.length];
  }

  function _closestTools(toolList, wrong) {
    const wanted = _compact(wrong);
    const ranked = [];
    for (const raw of toolList) {
      const shown = raw.replace(/-/g, "_");
      const have = _compact(shown);
      if (have.includes(wanted) || wanted.includes(have)) {
        ranked.push([0, Math.abs(have.length - wanted.length), shown]);
        continue;
      }
      const dist = _levenshtein(wanted, have);
      if (dist <= Math.ceil(wanted.length / 2)) {
        ranked.push([1, dist, shown]);
      }
    }
    ranked.sort((a, b) => a[0] - b[0] || a[1] - b[1] || (a[2] < b[2] ? -1 : a[2] > b[2] ? 1 : 0));
    return ranked.slice(0, 3).map((x) => "tools." + x[2]);
  }

  function _normalizeArgs(args) {
    if (args === undefined || args === null) return {};
    if (typeof args !== "object" || Array.isArray(args)) {
      throw new ToolError("工具参数必须是对象（如 tools.read({ path: 'a.txt' })）");
    }
    const clean = {};
    for (const [k, v] of Object.entries(args)) {
      if (v !== undefined) clean[k] = v;
    }
    return clean;
  }

  function _bindTool(rawName) {
    async function invoke(args) {
      const clean = _normalizeArgs(args);
      try {
        return await __acall(rawName, clean);
      } catch (e) {
        throw new ToolError(e && typeof e.message === "string" ? e.message : String(e));
      }
    }
    return invoke;
  }

  const _IGNORED_PROPS = new Set([
    "then",
    "toJSON",
    "valueOf",
    "toString",
    "constructor",
    "hasOwnProperty",
  ]);

  // ---------------------------------------------------------------- 工具发现（BM25，含 CJK）
  const DEFAULT_SEARCH_LIMIT = 8;
  const _STOP_WORDS = new Set([
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it",
    "of", "on", "or", "that", "the", "this", "to", "with",
  ]);
  const _CJK_RE = /[\u3400-\u9fff\uf900-\ufaff\u3040-\u30ff\uac00-\ud7af]+/g;

  function _stem(term) {
    if (term.length > 4 && term.endsWith("ies")) return term.slice(0, -3) + "y";
    if (term.length > 4 && /(ches|shes|sses|xes|zes)$/.test(term)) return term.slice(0, -2);
    if (term.length > 3 && term.endsWith("s") && !term.endsWith("ss")) return term.slice(0, -1);
    return term;
  }

  function _tokenize(value) {
    const lowered = value
      .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
      .replace(/([A-Z]+)([A-Z][a-z])/g, "$1 $2")
      .toLowerCase();
    const terms = [];
    for (const part of lowered.split(/[^a-z0-9]+/)) {
      if (part && !_STOP_WORDS.has(part)) terms.push(_stem(part));
    }
    const cjkRuns = lowered.match(_CJK_RE);
    if (cjkRuns) {
      for (const run of cjkRuns) {
        for (let i = 0; i < run.length; i++) terms.push(run[i]);
        for (let i = 0; i + 1 < run.length; i++) terms.push(run.slice(i, i + 2));
      }
    }
    return terms;
  }

  function _schemaText(schema, parts) {
    if (!schema || typeof schema !== "object" || Array.isArray(schema)) return;
    if (typeof schema.description === "string") parts.push(schema.description);
    if (schema.properties && typeof schema.properties === "object") {
      for (const [name, prop] of Object.entries(schema.properties)) {
        parts.push(name);
        _schemaText(prop, parts);
      }
    }
    _schemaText(schema.items, parts);
    for (const key of ["anyOf", "oneOf", "allOf"]) {
      if (Array.isArray(schema[key])) {
        for (const variant of schema[key]) _schemaText(variant, parts);
      }
    }
  }

  function _identifier(name) {
    return name.replace(/[^A-Za-z0-9_]/g, "_");
  }

  let _catalogTools = [];
  let _catalogNamespaces = new Map();

  function _namespaceMatches(ns, query) {
    const qid = _identifier(query);
    for (const candidate of [ns.name, ns.server || ""]) {
      if (candidate && (candidate === query || _identifier(candidate) === qid)) return true;
    }
    return false;
  }

  function _namespaceOf(tool) {
    return tool.namespace ? _catalogNamespaces.get(tool.namespace) : undefined;
  }

  function _toolDocument(tool) {
    const parts = [tool.name, tool.name.replace(/_/g, " "), tool.description || ""];
    _schemaText(tool.parameters, parts);
    const ns = _namespaceOf(tool);
    if (ns) {
      parts.push(ns.name, ns.description || "", ns.instructions || "");
    }
    return parts.filter((p) => p && p.trim()).join(" ");
  }

  function _toolEntry(tool) {
    return { name: _identifier(tool.name), description: tool.description || "" };
  }

  function searchTools(query, limit = DEFAULT_SEARCH_LIMIT, namespace = null) {
    if (typeof query !== "string") throw new ToolError("searchTools() 要一个查询字符串");
    if (!Number.isInteger(limit) || limit <= 0) {
      throw new ToolError("searchTools() 的 limit 要是正整数");
    }
    if (namespace !== null && namespace !== undefined && typeof namespace !== "string") {
      throw new ToolError("searchTools() 的 namespace 要是字符串");
    }
    const pool = _catalogTools.filter((tool) => {
      if (!namespace) return true;
      const ns = _namespaceOf(tool);
      return Boolean(ns && _namespaceMatches(ns, namespace));
    });
    const queryTerms = Array.from(new Set(_tokenize(query)));
    if (!queryTerms.length || !pool.length) return [];

    const counts = pool.map((tool) => {
      const tf = new Map();
      for (const term of _tokenize(_toolDocument(tool))) {
        tf.set(term, (tf.get(term) || 0) + 1);
      }
      return tf;
    });
    const lengths = counts.map((tf) => {
      let sum = 0;
      for (const c of tf.values()) sum += c;
      return sum;
    });
    const average = lengths.reduce((a, b) => a + b, 0) / pool.length || 1;
    const idf = new Map();
    for (const term of queryTerms) {
      let freq = 0;
      for (const tf of counts) if (tf.has(term)) freq++;
      idf.set(term, Math.log(1 + (pool.length - freq + 0.5) / (freq + 0.5)));
    }
    const k1 = 1.2;
    const b = 0.75;
    const scored = [];
    for (let i = 0; i < pool.length; i++) {
      let score = 0;
      for (const term of queryTerms) {
        const count = counts[i].get(term);
        if (!count) continue;
        const norm = k1 * (1 - b + (b * lengths[i]) / average);
        score += (idf.get(term) * (count * (k1 + 1))) / (count + norm);
      }
      if (score > 0) scored.push([score, i]);
    }
    scored.sort((a, b) => b[0] - a[0] || a[1] - b[1]);
    return scored.slice(0, limit).map(([, idx]) => _toolEntry(pool[idx]));
  }

  function describeTool(name) {
    if (typeof name !== "string") throw new ToolError("describeTool() 要一个工具名");
    for (const tool of _catalogTools) {
      if (tool.name === name || _identifier(tool.name) === name) {
        const described = {
          name: _identifier(tool.name),
          description: tool.description || "",
          parameters: tool.parameters || {},
        };
        if (tool.namespace) described.namespace = tool.namespace;
        return described;
      }
    }
    return null;
  }

  function describeNamespace(name) {
    if (typeof name !== "string") throw new ToolError("describeNamespace() 要一个组名");
    for (const ns of _catalogNamespaces.values()) {
      if (!_namespaceMatches(ns, name)) continue;
      const described = { name: ns.name };
      if (ns.description) described.description = ns.description;
      if (ns.instructions) described.instructions = ns.instructions;
      described.tools = _catalogTools
        .filter((t) => t.namespace === ns.name)
        .map((t) => _identifier(t.name));
      return described;
    }
    return null;
  }

  // 每次运行由宿主调用一次：按本次能力面绑定 `TOOLS`、`tools` 与 `ALL_TOOLS`，随后抹掉自身。
  function __initRuntime(toolList, catalog) {
    const names = Array.isArray(toolList) ? toolList : [];
    const target = Object.create(null);
    const aliases = Object.create(null);
    for (const raw of names) {
      const fn = _bindTool(raw);
      const ident = raw.replace(/-/g, "_");
      target[ident] = fn;
      if (raw !== ident) aliases[raw] = fn;
    }
    Object.freeze(target);
    Object.freeze(aliases);

    const toolsProxy = new Proxy(target, {
      get(t, prop) {
        if (typeof prop === "symbol") return undefined;
        if (Object.hasOwn(t, prop)) return t[prop];
        if (Object.hasOwn(aliases, prop)) return aliases[prop];
        if (_IGNORED_PROPS.has(prop)) return undefined;
        const close = _closestTools(names, prop);
        const hint = close.length
          ? `没有叫 ${prop} 的工具，最接近的是 ${close.join("、")}；能调的名字见 TOOLS。`
          : `没有叫 ${prop} 的工具；能调的名字见 TOOLS。`;
        throw new ToolError(hint);
      },
      has(t, prop) {
        return typeof prop === "string" && (Object.hasOwn(t, prop) || Object.hasOwn(aliases, prop));
      },
    });

    _catalogTools = Array.isArray(catalog?.tools) ? catalog.tools : [];
    _catalogNamespaces = new Map();
    for (const ns of Array.isArray(catalog?.namespaces) ? catalog.namespaces : []) {
      _catalogNamespaces.set(ns.name, ns);
    }

    const frozenNames = Object.freeze(names.slice());
    const allTools = Object.freeze(_catalogTools.map(_toolEntry));

    Object.defineProperties(globalThis, {
      TOOLS: { value: frozenNames, enumerable: true, writable: false, configurable: false },
      tools: { value: toolsProxy, enumerable: true, writable: false, configurable: false },
      ALL_TOOLS: { value: allTools, enumerable: true, writable: false, configurable: false },
    });
    delete globalThis.__initRuntime;
  }

  Object.defineProperties(globalThis, {
    ToolError: { value: ToolError, enumerable: false, writable: false, configurable: false },
    text: { value: text, enumerable: false, writable: false, configurable: false },
    console: { value: console, enumerable: true, writable: false, configurable: false },
    searchTools: { value: searchTools, enumerable: false, writable: false, configurable: false },
    describeTool: { value: describeTool, enumerable: false, writable: false, configurable: false },
    describeNamespace: { value: describeNamespace, enumerable: false, writable: false, configurable: false },
    search_tools: { value: searchTools, enumerable: false, writable: false, configurable: false },
    describe_tool: { value: describeTool, enumerable: false, writable: false, configurable: false },
    describe_namespace: { value: describeNamespace, enumerable: false, writable: false, configurable: false },
    __initRuntime: { value: __initRuntime, enumerable: false, writable: false, configurable: true },
  });
})();
