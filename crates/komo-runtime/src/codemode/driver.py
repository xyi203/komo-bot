# codemode 驱动（docs/codemode.md）：在沙箱里跑模型的脚本。
#
# 协议走 stdin / stdout，每行一个 JSON：
#   komo → 驱动：第一行 {"code": ..., "tools": [...]}；之后每行是一次工具调用的答复
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


def main():
    request = json.loads(_proto_in.readline())
    names = request["tools"]
    scope = {
        "__name__": "__main__",
        "tools": _Tools(names),
        "text": text,
        "ToolError": ToolError,
        "TOOLS": list(names),
    }
    error = None
    try:
        exec(compile(request["code"], "<codemode>", "exec"), scope)
    except BaseException:
        error = traceback.format_exc(limit=-8)
    _send({"done": {"outputs": _outputs, "console": _console.getvalue(), "error": error}})


main()
