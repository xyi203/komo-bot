# 拼在每段脚本前面（docs/codemode.md §6）。Monty 的异常类型是封闭枚举、没有用户子类，`ToolError`
# 只能是 `RuntimeError` 的别名；这段占着脚本前面的行，worker 把回溯里的行号按它的行数减回去。
# `_call` / `TOOLS` / `_CATALOG` 是宿主交进来的输入：调一次工具、能调的工具名、工具说明与所属组。
ToolError = RuntimeError


# `tools.<name>` 是真的属性（Monty 不支持 `__getattr__`），所以 `f = tools.read`、`getattr(tools, name)`
# 都能用；代价是写错工具名得到的是 AttributeError，不是 ToolError。
class Tools:
    pass


tools = Tools()


def _bind(name):
    def call(_args=None, **kwargs):
        args = dict(_args or {})
        args.update(kwargs)
        return _call(name, args)

    return call


for _name in TOOLS:
    setattr(tools, _name.replace("-", "_"), _bind(_name))

# ---------------------------------------------------------------- 发现工具
#
# 照 pi 的 searchTools / describeTool / describeNamespace（BM25、同一套分词与停用词），全在 worker 里
# 对 catalog 做，不经宿主、不进 journal。pi 的分词只认 a-z0-9，这里另把
# 中日韩字切成单字与相邻两字，中文说明才搜得到。re / math 用到才导入：不搜的脚本不多付启动时间。

DEFAULT_SEARCH_LIMIT = 8
_STOP_WORDS = {
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it",
    "of", "on", "or", "that", "the", "this", "to", "with",
}
_CJK_PATTERN = r"[\u3400-\u9fff\uf900-\ufaff\u3040-\u30ff\uac00-\ud7af]+"


def _stem(term):
    import re

    if len(term) > 4 and term.endswith("ies"):
        return term[:-3] + "y"
    if len(term) > 4 and re.search(r"(ches|shes|sses|xes|zes)$", term):
        return term[:-2]
    if len(term) > 3 and term.endswith("s") and not term.endswith("ss"):
        return term[:-1]
    return term


def _tokenize(value):
    import re

    value = re.sub(r"([a-z0-9])([A-Z])", r"\1 \2", value)
    value = re.sub(r"([A-Z]+)([A-Z][a-z])", r"\1 \2", value).lower()
    terms = [
        _stem(term)
        for term in re.split(r"[^a-z0-9]+", value)
        if term and term not in _STOP_WORDS
    ]
    for run in re.findall(_CJK_PATTERN, value):
        terms.extend(run)
        terms.extend(run[i : i + 2] for i in range(len(run) - 1))
    return terms


def _schema_text(schema, parts):
    if not isinstance(schema, dict):
        return
    if isinstance(schema.get("description"), str):
        parts.append(schema["description"])
    properties = schema.get("properties")
    if isinstance(properties, dict):
        for name, prop in properties.items():
            parts.append(name)
            _schema_text(prop, parts)
    _schema_text(schema.get("items"), parts)
    for key in ("anyOf", "oneOf", "allOf"):
        variants = schema.get(key)
        if isinstance(variants, list):
            for variant in variants:
                _schema_text(variant, parts)


def _identifier(name):
    import re

    return re.sub(r"[^A-Za-z0-9_]", "_", name)


class _Catalog:
    def __init__(self, catalog):
        self.tools = catalog.get("tools", [])
        self.namespaces = {ns["name"]: ns for ns in catalog.get("namespaces", [])}

    def _namespace_matches(self, namespace, query):
        query_id = _identifier(query)
        return any(
            name == query or _identifier(name) == query_id
            for name in (namespace["name"], namespace.get("server") or "")
            if name
        )

    def _namespace_of(self, tool):
        return self.namespaces.get(tool.get("namespace") or "")

    def _document(self, tool):
        parts = [tool["name"], tool["name"].replace("_", " "), tool.get("description") or ""]
        _schema_text(tool.get("parameters"), parts)
        namespace = self._namespace_of(tool)
        if namespace:
            parts += [
                namespace["name"],
                namespace.get("description") or "",
                namespace.get("instructions") or "",
            ]
        return " ".join(part for part in parts if part.strip())

    def entry(self, tool):
        return {"name": _identifier(tool["name"]), "description": tool.get("description") or ""}

    def search(self, query, limit=DEFAULT_SEARCH_LIMIT, namespace=None):
        import math

        if not isinstance(query, str):
            raise ToolError("search_tools() 要一个查询字符串")
        if not isinstance(limit, int) or isinstance(limit, bool) or limit <= 0:
            raise ToolError("search_tools() 的 limit 要是正整数")
        if namespace is not None and not isinstance(namespace, str):
            raise ToolError("search_tools() 的 namespace 要是字符串")
        tools = [
            tool
            for tool in self.tools
            if not namespace
            or (self._namespace_of(tool) and self._namespace_matches(self._namespace_of(tool), namespace))
        ]
        query_terms = list(dict.fromkeys(_tokenize(query)))
        if not query_terms or not tools:
            return []
        counts = []
        for tool in tools:
            tf = {}
            for term in _tokenize(self._document(tool)):
                tf[term] = tf.get(term, 0) + 1
            counts.append(tf)
        lengths = [sum(tf.values()) for tf in counts]
        average = (sum(lengths) / len(tools)) or 1
        idf = {}
        for term in query_terms:
            frequency = sum(1 for tf in counts if term in tf)
            idf[term] = math.log(1 + (len(tools) - frequency + 0.5) / (frequency + 0.5))
        k1, b = 1.2, 0.75
        scored = []
        for index, tool in enumerate(tools):
            score = 0.0
            for term in query_terms:
                count = counts[index].get(term)
                if not count:
                    continue
                norm = k1 * (1 - b + b * lengths[index] / average)
                score += idf[term] * (count * (k1 + 1)) / (count + norm)
            if score > 0:
                scored.append((score, index))
        # 分数一样时保持目录顺序（sorted 是稳定的）。
        scored.sort(key=lambda item: -item[0])
        return [self.entry(tools[index]) for _, index in scored[:limit]]

    def describe_tool(self, name):
        if not isinstance(name, str):
            raise ToolError("describe_tool() 要一个工具名")
        for tool in self.tools:
            if tool["name"] == name or _identifier(tool["name"]) == name:
                described = {
                    "name": _identifier(tool["name"]),
                    "description": tool.get("description") or "",
                    "parameters": tool.get("parameters") or {},
                }
                if tool.get("namespace"):
                    described["namespace"] = tool["namespace"]
                return described
        return None

    def describe_namespace(self, name):
        if not isinstance(name, str):
            raise ToolError("describe_namespace() 要一个组名")
        for namespace in self.namespaces.values():
            if not self._namespace_matches(namespace, name):
                continue
            described = {"name": namespace["name"]}
            for key in ("description", "instructions"):
                if namespace.get(key):
                    described[key] = namespace[key]
            described["tools"] = [
                _identifier(tool["name"])
                for tool in self.tools
                if tool.get("namespace") == namespace["name"]
            ]
            return described
        return None


_catalog = _Catalog(_CATALOG)
ALL_TOOLS = [_catalog.entry(tool) for tool in _catalog.tools]


def search_tools(query, limit=DEFAULT_SEARCH_LIMIT, namespace=None):
    return _catalog.search(query, limit, namespace)


def describe_tool(name):
    return _catalog.describe_tool(name)


def describe_namespace(name):
    return _catalog.describe_namespace(name)
