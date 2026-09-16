"""komo 的解释器驱动（§5.1、§5.2）。

作业从 stdin 读，结构化结果写到 KOMO_RESULT_PATH 指向的文件——**不走 stdout**。
脚本自己的 print 是给人看的输出，控制协议是给 Gateway 读的，两者混在一根管子里，
一个打印了 JSON 的脚本就能伪造执行结果。

两种调用形式（§5.2）：

- code：任意代码。脚本可以设 `result` 返回 JSON 可表达的数据。
- call：已保存模块中**明确导出**的函数。导出 = 模块 `__all__` 里列了这个名字；
  查的是 `module.__dict__`，不是 `getattr`——后者会顺着 `__getattr__` 走到任意属性。
"""

import json
import os
import sys


def _jsonable(value):
    """能过 json 的原样留下，过不去的换成它的字符串形式并标注。"""
    try:
        json.dumps(value)
        return value
    except (TypeError, ValueError):
        return {"__repr__": repr(value), "__type__": type(value).__name__}


def _run_code(job):
    source = job.get("code")
    if not isinstance(source, str):
        raise ValueError("code 模式需要一个字符串 code")
    globals_ = {"__name__": "__komo__", "__builtins__": __builtins__, "result": None}
    exec(compile(source, "<komo>", "exec"), globals_)  # noqa: S102
    return globals_.get("result")


def _run_call(job):
    import importlib

    module_name = job.get("module")
    function_name = job.get("function")
    if not isinstance(module_name, str) or not isinstance(function_name, str):
        raise ValueError("call 模式需要 module 与 function")
    if function_name.startswith("_"):
        raise PermissionError(f"{module_name}.{function_name} 不是导出函数")

    module = importlib.import_module(module_name)
    exported = getattr(module, "__all__", None)
    if exported is None:
        raise PermissionError(
            f"{module_name} 没有声明 __all__；call 模式只调用明确导出的函数"
        )
    if function_name not in exported:
        raise PermissionError(f"{module_name} 没有导出 {function_name}")

    # __dict__ 而不是 getattr：不开放任意属性查找。
    try:
        function = module.__dict__[function_name]
    except KeyError:
        raise AttributeError(f"{module_name}.{function_name} 不存在") from None
    if not callable(function):
        raise TypeError(f"{module_name}.{function_name} 不是可调用对象")

    args = job.get("args") or {}
    if not isinstance(args, dict):
        raise ValueError("args 必须是对象")
    return function(**args)


def main():
    out = {"status": "completed", "result": None, "error": None}
    try:
        job = json.load(sys.stdin)
        mode = job.get("mode")
        if mode == "code":
            out["result"] = _jsonable(_run_code(job))
        elif mode == "call":
            out["result"] = _jsonable(_run_call(job))
        else:
            raise ValueError(f"不认识的 mode：{mode!r}")
    except BaseException as error:  # noqa: BLE001 —— 失败要成为结果，不是一个空文件
        import traceback

        out["status"] = "failed"
        out["error"] = "".join(
            traceback.format_exception_only(type(error), error)
        ).strip()
        out["traceback"] = traceback.format_exc()

    path = os.environ.get("KOMO_RESULT_PATH")
    if not path:
        print("KOMO_RESULT_PATH 没有设置", file=sys.stderr)
        raise SystemExit(2)
    # 先写临时文件再 rename：读到的要么是完整结果，要么什么都没有。
    temporary = path + ".partial"
    with open(temporary, "w", encoding="utf-8") as handle:
        json.dump(out, handle, ensure_ascii=False, default=repr)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)
    sys.stdout.flush()
    sys.stderr.flush()
    raise SystemExit(0 if out["status"] == "completed" else 1)


main()
