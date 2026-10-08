# codemode：沙箱里的 Python 脚本

> 状态：设计 + 首版落地（2026-10-08）。借鉴 pi 的 codemode（`packages/coding-agent/docs/codemode.md`），
> 引擎换成 komo 已有的 Python，隔离交给操作系统沙箱。与 `docs/komo_bot.md` 冲突处以本文为准。

## 1. 要解决什么

模型一次只能发几个工具调用，每个结果都整段进上下文。查十个文件、过滤 MCP 返回的大 JSON、
把三处结果拼成一张表，要么多轮来回，要么把一堆用不上的正文塞进上下文。

codemode 让模型写**一段脚本**：脚本里调工具、在本地过滤和汇总，只有脚本的输出回到模型。

## 2. 为什么是"沙箱里的 Python"，不是 QuickJS

脚本能不能**免审批**、中断后能不能**整段重跑**，取决于脚本本身能不能碰外部世界。

| 方案 | 隔离 | 结论 |
|---|---|---|
| QuickJS（pi 的做法） | 引擎里没有文件 / 网络 / 进程 | 可行，新依赖 `rquickjs`，要写一层 Rust↔JS 桥 |
| 裸 Python（komo 已有的 `python` 工具） | 无：脚本就是任意代码 | 每段都要审批，失去意义 |
| **Python + 操作系统沙箱** | 进程级：不能写文件、不能联网、不能起子进程、只能读解释器自己 | **采用**：零新依赖，模型写 Python 最顺，还能用托管环境里装好的库 |

2026-10-08 在 macOS 27 上实测（`sandbox-exec` + 下面 §4 的配置）：写 `~` 下的文件、连
`1.1.1.1:80`、`subprocess.run`、读 `~/.komo/.env`、列 `~/.ssh` 全部 `PermissionError`；
stdin / stdout 正常。

## 3. 形状

```text
模型 ── codemode({"code": "..."}) ──▶ executor（与其他工具同一条路：Policy → start_call）
                                         │ Operation::Codemode → 不调 Tool::execute，
                                         │ 交给 executor 自己的 run_codemode
                                         ▼
                           sandbox-exec + python3 -I driver.py（子进程）
                              stdin  ◀── 第一行：{code, tools}；之后：工具调用的结果
                              stdout ──▶ {"call": name, "args": {...}} / {"done": {...}}
                                         │
                       每个 call ──▶ executor.nested_call（见 §5）──▶ 结果写回 stdin
                                         ▼
                           输出（text() 与 print）落进这次调用的 stdout，回到模型
```

- **外层是一次普通调用**：`tool.planned` / `tool.started` / `tool.result` 照常落账，输出照常
  投影（§8.3）。
- **内层调用不单独落账**：结果只回到脚本；外层 `output.json` 的 `result.calls` 里留一份有上限
  的摘要（工具、截断的参数、状态、耗时；最多 256 条）。这与 pi 相同——它成立的前提是 §5：
  内层只有只读调用，重跑整段没有副作用。

## 4. 沙箱

### macOS（首版）

`sandbox-exec -p <profile> <python> -I -c <driver>`，Seatbelt 配置按解释器现生成：

```scheme
(version 1)
(deny default)
(allow process-exec (literal "<sys.executable>") (subpath "<sys.prefix>") (subpath "<sys.base_prefix>"))
(allow process-fork)
(allow file-map-executable)
(allow file-read-metadata)
(allow file-read* (subpath "<sys.prefix>") (subpath "<sys.base_prefix>")
                  (subpath "/System") (subpath "/usr/lib") (subpath "/private/var/db/dyld")
                  (literal "/dev/null") (literal "/dev/urandom") (literal "/"))
(allow sysctl-read)
(allow file-write-data (literal "/dev/null"))
```

- **没有** `network*`、没有 `file-write*`（`/dev/null` 除外）、`process-exec` 只放解释器与它的安装目录
  （框架版 Python 启动时会再 exec 一次安装目录里的 `Python.app`；起来的仍在同一个沙箱里）。
- 解释器的路径在 Gateway 启动时探一次（`sys.executable`、它的真实路径、`sys.prefix`、
  `sys.base_prefix`）。托管 venv（§5.1）的 `site-packages` 在 `sys.prefix` 下，所以**装好的库
  可以只读导入**；没建 venv 时退回系统 `python3`，只有标准库。
- 脚本不落盘：driver 用 `-c` 传入，脚本正文走 stdin。
- 环境变量清空，只给 `LANG`；凭证一个都不给。

### 自检，失败就不注册

启动时用同一份配置跑一段自检脚本：写一个临时文件、连一个 socket、起一个子进程，**三样都必须
被拒**、stdin/stdout 必须通。任何一条不成立（没有 `sandbox-exec`、配置被系统拒绝、某样没拦住），
`codemode` 就不进工具目录，日志写明原因。**不退化成没有沙箱的 Python。**

### Linux（后续）

Landlock（文件只读白名单、禁写）+ seccomp（禁 `socket` / `connect` / `execve`），在 `pre_exec`
里装上，与 Codex 的 Linux 沙箱同一路数。首版 Linux 上自检不通过 → 不注册 codemode。

## 5. 脚本里的工具调用

`tools.<name>(**args)` → executor 的 `nested_call`，依次：

1. **能力面**：名字必须在这次 Run 的能力面里（与顶层调用同一份 `AgentSurface`）。`codemode`、
   `delegate`、`dispatch`、`follow` 不能在脚本里调。
2. `tool.prepare(args)` 出计划。
3. **只读**：`plan.operation.is_read_only()` 必须成立——`read`、`rg`、`[mcp.servers] read_only`
   里的 MCP 工具。否则抛 `ToolError`："`<name>` 会改东西，请在脚本外直接调用"。
4. **Policy 必须直接 Allow**（不查授权表）。`Ask` → 抛错："这一步要审批（原因），脚本里不能停下
   来等人，请在脚本外直接调用"；`Deny` → 抛错带原因。
5. 用 Policy 的 `Proof` 执行；输出收进内存（上限 1 MiB）。

返回给脚本的是一个 dict：

```python
{"status": "completed" | "failed", "text": "<工具的正文输出>", "result": <output.json 里的结构化结果>}
```

`read` 的 `text` 是文件内容，`rg` 是匹配行，MCP 是渲染过的文本，`result` 是完整 `CallToolResult`。
调用失败（含第 3、4 步）抛 `ToolError`，`try/except` 接得住。

## 6. 模型看到的接口

工具 `codemode`，参数 `{"code": "<Python 源码>"}`。脚本是模块级代码：

| 名字 | 作用 |
|---|---|
| `tools.<name>(**args)` | 调工具，见 §5。名字里的 `-` 写成 `_`（`tools.mcp__dev_radius__search`）|
| `text(value)` | 加一段输出；字符串原样，其他值转 JSON |
| `print(...)` | 进输出末尾的 `<console>` 段 |
| `TOOLS` | 这次脚本能调的工具名列表 |

结果正文：`text()` 各段按顺序，`<console>` 段随后，失败时附 `脚本出错：` 与 traceback，最后一行
是调用摘要（`调用了 3 次工具：read×2、rg×1`）。超长照常由投影截头尾。

## 7. Policy 与恢复

- 计划：`Operation::Codemode { code }`，代码进计划哈希。
- `OperationMatch::Codemode`：strict 与 auto 下都 Allow（`codemode-allow`）——脚本自己碰不到外部，
  它里面的每一次调用都各自过 Policy。
- 恢复：`SafeReread`。中断后整段重跑：内层只有只读调用，重跑看到的是"现在"的状态，与重读同理。
- `is_read_only()` 为真：可以和同一轮的其他只读调用并行。

## 8. 限制

- 时限：`[execution] call_timeout_secs`，到点杀进程组；输出上限 1 MiB；内层调用最多 256 次。
- 脚本之间不共享状态（pi 的 `store/load` 不做）。
- 不跑模型（pi 的 `models` 不做）。

## 9. 实现落点

| 位置 | 内容 |
|---|---|
| `komo-kernel` `types/plan.rs`、`policy/rules.rs`、`policy/defaults.rs` | `Operation::Codemode`、`OperationMatch::Codemode`、`codemode-allow` |
| `komo-runtime` `codemode/`（`mod.rs` + `driver.py`） | 解释器探测、Seatbelt 配置、自检、子进程协议 |
| `komo-runtime` `tools/codemode.rs` | `CodemodeTool`：定义 + `prepare`；`execute` 不该被走到（与 `dispatch` 同一模式） |
| `komo-runtime` `executor` | `with_codemode(sandbox)`；`execute_authorized` 里按 `Operation::Codemode` 分流到 `run_codemode`；`nested_call` |
| `komo-gateway` `service/mod.rs` | 启动时探测 + 自检，通过才注册工具并接上执行器 |

## 10. 验收

- 沙箱自检：写文件 / 联网 / 起子进程三样被拒（macOS 上跑，其他平台跳过）。
- 端到端（真 Gateway）：脚本调 `read` 两次、过滤后 `text()`，模型只看到过滤结果；脚本里调
  `write` 抛错、文件没被写；脚本里读工作区外的文件（Ask）抛错；脚本死循环到时限被杀。
- Policy：`codemode` 在 strict 下 Allow。
