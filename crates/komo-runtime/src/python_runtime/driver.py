"""komo 的解释器驱动（§5.1）。

作业从 stdin 读，结构化结果写到 KOMO_RESULT_PATH 指向的文件——**不走 stdout**。
脚本自己的 print 是给人看的输出，控制协议是给 Gateway 读的，两者混在一根管子里，
一个打印了 JSON 的脚本就能伪造执行结果。

任意代码：脚本可以设 `result` 返回 JSON 可表达的数据。

这不是沙箱，文档也没有声称它是：任意代码一旦启动就有其运行账号的权限（§7.3）。
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


def main():
    out = {"status": "completed", "result": None, "error": None}
    try:
        job = json.load(sys.stdin)
        # 作业读完就把 0 号描述符换成 /dev/null：脚本起的子进程会继承它，管道当 stdin 时
        # rg 这类"没给路径就搜 stdin"的工具会去搜这条空管道，静默返回零匹配。
        devnull = os.open(os.devnull, os.O_RDONLY)
        os.dup2(devnull, 0)
        os.close(devnull)
        mode = job.get("mode")
        if mode != "code":
            raise ValueError(f"不认识的 mode：{mode!r}")
        out["result"] = _jsonable(_run_code(job))
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
