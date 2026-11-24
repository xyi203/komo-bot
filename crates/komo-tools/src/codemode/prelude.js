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

  // ---------------------------------------------------------------- 工具发现（BM25F + 弱同义词扩展，含 CJK）
  const DEFAULT_SEARCH_LIMIT = 8;
  const _STOP_WORDS = new Set([
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it",
    "of", "on", "or", "that", "the", "this", "to", "with",
  ]);
  const _CJK_RE = /[\u3400-\u9fff\uf900-\ufaff\u3040-\u30ff\uac00-\ud7af]+/g;

  // 常见工具动词的弱权重同义扩展：精确命中始终权重 1.0 排第一；
  // 当服务端没有字面匹配的 `search_*` 而用 `list_*(filter=...)` 等形式建模时仍能稳进前排。
  const _SYNONYMS = Object.freeze({
    search: [["find", 0.45], ["query", 0.45], ["filter", 0.45], ["lookup", 0.4], ["grep", 0.4], ["list", 0.25]],
    find: [["search", 0.45], ["query", 0.45], ["filter", 0.45], ["lookup", 0.4], ["grep", 0.4], ["list", 0.25]],
    query: [["search", 0.45], ["find", 0.45], ["filter", 0.45], ["list", 0.25]],
    filter: [["search", 0.45], ["query", 0.45], ["find", 0.4], ["list", 0.3]],
    list: [["get", 0.25], ["search", 0.25], ["find", 0.25], ["filter", 0.25]],
    get: [["read", 0.4], ["fetch", 0.4], ["show", 0.35], ["view", 0.35], ["describe", 0.3]],
    read: [["get", 0.4], ["fetch", 0.35], ["view", 0.35], ["open", 0.3]],
    create: [["add", 0.45], ["new", 0.4], ["insert", 0.4], ["post", 0.3]],
    add: [["create", 0.45], ["new", 0.4], ["insert", 0.4], ["append", 0.35]],
    update: [["edit", 0.45], ["modify", 0.45], ["patch", 0.45], ["set", 0.35]],
    edit: [["update", 0.45], ["modify", 0.45], ["patch", 0.45], ["write", 0.3]],
    delete: [["remove", 0.45], ["rm", 0.4], ["drop", 0.35], ["destroy", 0.35], ["archive", 0.25]],
    remove: [["delete", 0.45], ["rm", 0.4], ["drop", 0.35]],
    "搜索": [["search", 0.45], ["find", 0.4], ["query", 0.4], ["filter", 0.4], ["list", 0.25]],
    "查找": [["search", 0.45], ["find", 0.45], ["query", 0.4], ["filter", 0.4], ["list", 0.25]],
    "查询": [["query", 0.45], ["search", 0.45], ["filter", 0.4], ["get", 0.3], ["list", 0.25]],
    "筛选": [["filter", 0.45], ["search", 0.4], ["query", 0.4], ["list", 0.25]],
    "列出": [["list", 0.45], ["get", 0.25]],
    "列表": [["list", 0.45], ["get", 0.25]],
    "创建": [["create", 0.45], ["add", 0.4], ["new", 0.35]],
    "新建": [["create", 0.45], ["add", 0.4], ["new", 0.4]],
    "修改": [["update", 0.45], ["edit", 0.45], ["patch", 0.4]],
    "更新": [["update", 0.45], ["edit", 0.4], ["patch", 0.4]],
    "删除": [["delete", 0.45], ["remove", 0.45]],
    "读取": [["read", 0.45], ["get", 0.4], ["fetch", 0.35]],
    "获取": [["get", 0.45], ["read", 0.4], ["fetch", 0.4]],
  });

  const _FIELDS = Object.freeze([
    { w: 3.0, b: 0.5 },  // 0: 工具名 + 命名空间名
    { w: 2.0, b: 0.5 },  // 1: 工具描述 + 参数名
    { w: 1.0, b: 0.3 },  // 2: 参数 schema 描述
    { w: 0.35, b: 0.25 }, // 3: 命名空间描述与 instructions（独立归一，不稀释单工具长度）
  ]);

  function _stem(term) {
    if (term.length > 4 && term.endsWith("ies")) return term.slice(0, -3) + "y";
    if (term.length > 4 && /(ches|shes|sses|xes|zes)$/.test(term)) return term.slice(0, -2);
    if (term.length > 3 && term.endsWith("s") && !term.endsWith("ss")) return term.slice(0, -1);
    return term;
  }

  function _tokenize(value) {
    if (!value) return [];
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

  function _schemaParts(schema, propNames, descParts) {
    if (!schema || typeof schema !== "object" || Array.isArray(schema)) return;
    if (typeof schema.description === "string") descParts.push(schema.description);
    if (schema.properties && typeof schema.properties === "object") {
      for (const [name, prop] of Object.entries(schema.properties)) {
        propNames.push(name);
        _schemaParts(prop, propNames, descParts);
      }
    }
    _schemaParts(schema.items, propNames, descParts);
    for (const key of ["anyOf", "oneOf", "allOf"]) {
      if (Array.isArray(schema[key])) {
        for (const variant of schema[key]) _schemaParts(variant, propNames, descParts);
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

  function _toolFieldTexts(tool) {
    const ns = _namespaceOf(tool);
    const propNames = [];
    const schemaDescs = [];
    _schemaParts(tool.parameters, propNames, schemaDescs);
    const ident = [
      tool.name,
      tool.name.replace(/[_-]/g, " "),
      ns ? ns.name : "",
      ns ? ns.server || "" : "",
    ].join(" ");
    const meta = [tool.description || "", propNames.join(" ")].join(" ");
    const schema = schemaDescs.join(" ");
    const nsDoc = ns ? [ns.description || "", ns.instructions || ""].join(" ") : "";
    return [ident, meta, schema, nsDoc];
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
    const rawTerms = Array.from(new Set(_tokenize(query)));
    if (!rawTerms.length || !pool.length) return [];

    const rawSet = new Set(rawTerms);
    const allLookupTerms = new Set(rawTerms);
    const concepts = rawTerms.map((term) => {
      const cands = [{ term, w: 1.0, exact: true }];
      const syns = _SYNONYMS[term];
      if (syns) {
        for (const [syn, w] of syns) {
          if (!rawSet.has(syn)) {
            cands.push({ term: syn, w, exact: false });
            allLookupTerms.add(syn);
          }
        }
      }
      return cands;
    });

    const fieldCount = _FIELDS.length;
    const toolTfs = [];
    const toolLens = [];
    const fieldSums = new Array(fieldCount).fill(0);
    const docFreq = new Map();

    for (const tool of pool) {
      const texts = _toolFieldTexts(tool);
      const tfs = [];
      const lens = [];
      const seenInTool = new Set();
      for (let f = 0; f < fieldCount; f++) {
        const tf = new Map();
        const terms = _tokenize(texts[f]);
        lens.push(terms.length);
        fieldSums[f] += terms.length;
        for (const t of terms) {
          tf.set(t, (tf.get(t) || 0) + 1);
          if (allLookupTerms.has(t)) seenInTool.add(t);
        }
        tfs.push(tf);
      }
      for (const t of seenInTool) {
        docFreq.set(t, (docFreq.get(t) || 0) + 1);
      }
      toolTfs.push(tfs);
      toolLens.push(lens);
    }

    const avgLens = fieldSums.map((s) => s / pool.length || 1);
    const idf = new Map();
    for (const term of allLookupTerms) {
      const freq = docFreq.get(term) || 0;
      idf.set(term, Math.log(1 + (pool.length - freq + 0.5) / (freq + 0.5)));
    }

    const k1 = 1.2;
    const scored = [];
    for (let i = 0; i < pool.length; i++) {
      let rawScore = 0;
      let coveredWeight = 0;
      for (const cands of concepts) {
        let best = 0;
        let bestW = 0;
        for (const cand of cands) {
          let pseudoTf = 0;
          for (let f = 0; f < fieldCount; f++) {
            const c = toolTfs[i][f].get(cand.term);
            if (!c) continue;
            const { w, b } = _FIELDS[f];
            pseudoTf += w * (c / (1 - b + (b * toolLens[i][f]) / avgLens[f]));
          }
          if (pseudoTf <= 0) continue;
          const s = cand.w * idf.get(cand.term) * ((pseudoTf * (k1 + 1)) / (pseudoTf + k1));
          if (s > best) {
            best = s;
            bestW = cand.w;
          }
        }
        if (best > 0) {
          rawScore += best;
          coveredWeight += bestW;
        }
      }
      if (rawScore > 0) {
        const boost = concepts.length > 1 ? 1 + 0.5 * (coveredWeight / concepts.length) : 1;
        scored.push([rawScore * boost, i]);
      }
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
        return async function unknownTool() {
          throw new ToolError(hint);
        };
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
