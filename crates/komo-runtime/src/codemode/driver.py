# codemode 驱动（docs/codemode.md）：在沙箱里跑模型的脚本。
#
# 协议走 stdin / stdout，每行一个 JSON：
#   komo → 驱动：第一行 {"code": ..., "tools": [...], "catalog": {"tools": [...], "namespaces": [...]}}；
#                之后每行是一次工具调用的答复
#                {"ok": {...}} 或 {"error": "..."}
#   驱动 → komo：{"call": name, "args": {...}} 或最后一行 {"done": {...}}
#
# 脚本的 print 进 console 缓冲区，不碰协议用的那个 fd。脚本仍能直接写那个 fd 伪造一行
# 协议——伪造出来的只能是"调一个工具"（与 tools.x() 同一道判定）或"我的输出是这些"，
# 换不来任何权限。
import io
import json
import os
import sys
import traceback

_proto_out = os.fdopen(os.dup(1), "w", buffering=1)
_proto_in = sys.stdin
sys.stdin = io.StringIO()
_console = io.StringIO()
sys.stdout = _console
sys.stderr = _console

_outputs = []
_calls = 0
MAX_CALLS = 256


class ToolError(Exception):
    pass


def _send(message):
    _proto_out.write(json.dumps(message, ensure_ascii=False) + "\n")
    _proto_out.flush()


def _call(name, args):
    global _calls
    _calls += 1
    if _calls > MAX_CALLS:
        raise ToolError("一段脚本最多调 %d 次工具" % MAX_CALLS)
    _send({"call": name, "args": args})
    line = _proto_in.readline()
    if not line:
        raise ToolError("komo 断开了")
    reply = json.loads(line)
    if "error" in reply:
        raise ToolError(reply["error"])
    return reply["ok"]


class _Tools:
    def __init__(self, names):
        self._names = {name.replace("-", "_"): name for name in names}

    def __getattr__(self, attr):
        name = self._names.get(attr)
        if name is None:
            raise ToolError("脚本里没有 %s；能调的是：%s" % (attr, "、".join(sorted(self._names))))

        def call(_args=None, **kwargs):
            args = dict(_args or {})
            args.update(kwargs)
            return _call(name, args)

        return call

    def __dir__(self):
        return sorted(self._names)


def text(value):
    if isinstance(value, str):
        _outputs.append(value)
    else:
        _outputs.append(json.dumps(value, ensure_ascii=False, indent=2, default=str))


# ---------------------------------------------------------------- 发现工具
#
# 照 pi 的 searchTools / describeTool / describeNamespace（BM25、同一套分词与停用词），全在沙箱里
# 对第一行带来的 catalog 做，不经 komo、不进 journal。pi 的分词只认 a-z0-9，这里另把
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


def main():
    request = json.loads(_proto_in.readline())
    names = request["tools"]
    catalog = _Catalog(request.get("catalog") or {})
    scope = {
        "__name__": "__main__",
        "tools": _Tools(names),
        "text": text,
        "ToolError": ToolError,
        "TOOLS": list(names),
        "ALL_TOOLS": [catalog.entry(tool) for tool in catalog.tools],
        "search_tools": catalog.search,
        "describe_tool": catalog.describe_tool,
        "describe_namespace": catalog.describe_namespace,
    }
    error = None
    try:
        exec(compile(request["code"], "<codemode>", "exec"), scope)
    except BaseException:
        error = traceback.format_exc(limit=-8)
    _send({"done": {"outputs": _outputs, "console": _console.getvalue(), "error": error}})


main()
