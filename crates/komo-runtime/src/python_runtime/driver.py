"""komo 的解释器驱动（§5.1、§5.2）。

作业从 stdin 读，结构化结果写到 KOMO_RESULT_PATH 指向的文件——**不走 stdout**。
脚本自己的 print 是给人看的输出，控制协议是给 Gateway 读的，两者混在一根管子里，
一个打印了 JSON 的脚本就能伪造执行结果。

两种调用形式（§5.2）：

- code：任意代码。脚本可以设 `result` 返回 JSON 可表达的数据。
- call：已保存模块中**明确导出**的函数。导出 = 模块 `__all__` 里列了这个名字；
  查的是 `module.__dict__`，不是 `getattr`——后者会顺着 `__getattr__` 走到任意属性。

执行边界（§7.3）：`KOMO_DENIED_IMPORTS` 列出的目录里的东西 **import 不进来**。
toolbox 的候选（`.staging/`）与历史快照（`.versions/`）在那张名单上，所以
「模型通过导入未知模块提前执行未审核代码」这条路是关着的——候选目录本来就不是合法的
包名，钩子挡的是绕开包名直接把它加进 `sys.path` 的那一手。

这不是沙箱，文档也没有声称它是：任意代码一旦启动就有其运行账号的权限（§7.3），
`exec(open(...).read())` 之类挡不住。挡的是 *import* 这条被点名的路径。
"""

import importlib.machinery
import json
import os
import sys


def _denied_roots():
    """不许 import 的目录（§7.3）。名单为空就不装钩子。"""
    raw = os.environ.get("KOMO_DENIED_IMPORTS") or ""
    return [os.path.realpath(line) for line in raw.split("\n") if line.strip()]


class _DenyFinder:
    """排在 sys.meta_path 最前面的查找器：解析到禁区里就拒绝，否则让开。

    它不自己找模块——先问标准的 `PathFinder`"这个名字会落到哪个文件"，再看那个文件在不
    在禁区里。拒绝是 ImportError 的子类，所以 `try: import ...` 看得见它，而不是一个
    看不懂的崩溃。

    `PathFinder` 在**装钩子之前**就抓在手里（模块顶层的 import），而且 `find_spec` 里
    带一个重入标记：钩子自己触发的任何 import 都会再走一遍 meta_path，不挡住这一层就是
    一次必然的无限递归。
    """

    def __init__(self, roots):
        self.roots = roots
        self.inside = False

    def find_spec(self, fullname, path=None, target=None):
        if self.inside:
            return None
        self.inside = True
        try:
            spec = importlib.machinery.PathFinder.find_spec(fullname, path, target)
        finally:
            self.inside = False
        if spec is None:
            return None
        origin = getattr(spec, "origin", None)
        locations = list(getattr(spec, "submodule_search_locations", None) or [])
        for candidate in [origin] + locations:
            if not candidate or candidate in ("built-in", "frozen", "namespace"):
                continue
            real = os.path.realpath(candidate)
            for root in self.roots:
                if real == root or real.startswith(root + os.sep):
                    raise ImportError(
                        "%s 解析到 %s：那是未经审核的 toolbox 目录，不能 import（§7.3）"
                        % (fullname, candidate)
                    )
        return None  # 不拦就让开，正常的查找链接着走。


def _install_import_guard():
    roots = _denied_roots()
    if roots:
        sys.meta_path.insert(0, _DenyFinder(roots))


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
        # 钩子先装：读作业之前就装上，任何一条 import 都躲不过它。
        _install_import_guard()
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
