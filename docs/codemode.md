# codemode：Monty 里的 Python 脚本

> 状态：已实现 Monty 引擎与持久化子调用（2026-10-15 起引擎由"CPython + 操作系统沙箱"换成 pydantic
> Monty；此前的实现见 git 历史）。脚本在 komo 自己起的 worker 子进程里跑，工具调用由宿主执行；
> 允许只读工具及有副作用 MCP。恢复借鉴 pi-durable 的子调用稳定 key、已存/当前 replay 双 Safe
> 与结果交付 journal；脚本的时钟与随机数还没有进 journal，所以恢复限制见 §7。设计权威为 `docs/komo_bot.md`。

## 1. 要解决什么

模型一次只能发几个工具调用，每个结果都整段进上下文。查十个文件、过滤 MCP 返回的大 JSON、
把三处结果拼成一张表，要么多轮来回，要么把一堆用不上的正文塞进上下文。

codemode 让模型写**一段脚本**：脚本里调工具、在本地过滤和汇总，只有脚本的输出回到模型。

## 2. 为什么是 Monty，不是操作系统沙箱

脚本能不能**放心地自动跑**、中断后能不能**整段重跑**，取决于脚本本身能不能碰外部世界。

| 方案 | 隔离 | 结论 |
|---|---|---|
| QuickJS（pi 的做法） | 引擎里没有文件 / 网络 / 进程 | 可行，新依赖 `rquickjs`，要写一层 Rust↔JS 桥；模型写 Python 更顺 |
| 裸 Python（komo 已有的 `python` 工具） | 无：脚本就是任意代码 | 内层调用再只读也没用，脚本自己就能改东西、联网，也不能整段重跑 |
| CPython + 操作系统沙箱（2026-10-15 前的做法：macOS Seatbelt、Linux Landlock + seccomp） | 进程级：不能写文件、不能联网、不能起子进程 | 能用，但要按平台各写一套约束、探解释器、启动时自检，自检不过就没有 codemode；Python 没有确定性 VM，恢复只能走已记录的前缀 |
| **pydantic Monty** | 语言层：解释器里没有 socket / subprocess / 文件，文件与环境变量只能经宿主放行的 OsCall | **采用** |

Monty 是 Rust 写的 Python 子集解释器，语言本身没有文件、网络、子进程，要碰宿主只能暂停到宿主手里
（工具调用、OsCall）。所以**不再需要操作系统沙箱**：没有 Seatbelt / Landlock / seccomp，没有解释器
探测，没有启动自检，也就没有"沙箱起不来所以不注册 codemode"这回事——surface 允许就能用。代价：

- 语言是 Python 子集：只有 `json` / `re` / `math` / `datetime` / `collections` / `itertools` / `functools`
  等少数标准库，没有 `os.path`、`glob`、`subprocess`、`hashlib`、`enum`、`csv` 等，**没有第三方库**
  （此前"托管环境里装好的库可以只读导入"没有了）。2026-10-15 POC 用 381 段真实 codemode 脚本验证：
  全部能编译；130 段有完整子调用录像的回放全部行为一致。
- 异常类型是封闭枚举，没有用户子类：`ToolError` 只能是 `RuntimeError` 的别名（§6）。
- Monty 版本钉死（`=1.1.0`）：它的快照与 `unstable` 访问器没有兼容保证，升级必须重跑回放集。
- 引擎是进程内的 Rust 代码，不是内核隔离：Monty 自身的 bug 理论上能越界。所以仍放进独立的 worker 进程
  （§4），宿主管限制与杀进程；这一层不是 OS 沙箱，不挡一个能利用 Monty 漏洞的脚本。

2026-10-15 的 POC 数据（macOS，release）：空脚本 spawn → done 中位 6.2 ms（`python3 -I -c pass` 47 ms）；
一次工具调用往返 17 µs。

## 3. 形状

```text
模型 ── codemode({"code": "..."}) ──▶ executor（与其他工具同一条路：能力面 → start_call）
                                         │ Operation::Codemode → 不调 Tool::execute，
                                         │ 交给 executor 自己的 run_codemode
                                         ▼
                           komo __codemode-worker（每次调用一个子进程，内跑 Monty）
                              stdin  ◀── 第一行：{code, tools, catalog, limits}；之后：工具调用的结果
                              stdout ──▶ {"call": name, "args": {...}} / {"done": {...}}
                                         │
                       每个 call ──▶ executor.nested_call（见 §5）──▶ 结果写回 stdin
                                         ▼
                           输出（text() 与 print）落进这次调用的 stdout，回到模型
```

- **外层是一次普通调用**：`tool.planned` / `tool.started` / `tool.result` 照常落账，输出照常
  投影（§8.3）。
- **内层调用单独持久化**：`codemode.journal` 按父 Run / Call 隔离；完整参数、计划和交付结果
  外置到同一 Session 的 `tool-output/`。这些事件不生成顶层 `tool.result`，kernel fold 不把它们
  加入模型消息；父结果仍经唯一的 `projection.rs` 投影。父 `result.calls` 只保留有上限的摘要。
- codemode 可能包含远端写操作，整体是调度屏障，不与同一轮其他只读调用并行。

## 4. worker 与限制

### 语言没有的，就是脚本碰不到的

- 没有 `socket`、`subprocess`、`ctypes`；`import` 只认 Monty 内置的那几个模块。
- 文件与环境变量要经宿主放行的 OsCall。这个版本 worker **一个都不放行**：`open` / `Path.*` 一律
  `PermissionError`，`os.getenv` 回默认值、`os.environ` 为空，`time.sleep` 照睡（受墙钟限制）。
  时钟与随机数用 Monty 自己的系统实现（还没有进 journal，见 §7）。
- worker 进程环境变量全清，不给 `LANG` 也不给任何凭证；stdin / stdout 只走协议，脚本的 `print`
  进 worker 内存里的缓冲（`<console>` 段），碰不到协议用的 fd。

### worker 进程

宿主为每次 codemode 调用起一个 worker：`<komo 可执行文件> __codemode-worker`（clap 隐藏子命令，不进
`--help`；函数体在 `komo-runtime` 的 `codemode::worker`，bin 只分发）。可执行文件是 Gateway 启动时
`current_exe()` 解析一次的路径。不用进程池：每次起一个 6 ms，换来每段脚本独立的内存与崩溃边界。
已知缺口：`komo update` / `scripts/install-local.sh` 原子换掉磁盘上的二进制之后、Gateway 重启之前，这台
Gateway 起的 worker 是新版本（路径没变）；worker 协议或 Monty 版本变了的话，这个窗口里的 codemode 调用
可能对不上。重启之后指纹不同的在途调用按 §7 settle 为 uncertain。（worker 与宿主之间没有版本握手，下一步再说。）
worker 在专用的大栈（64 MiB）线程里跑 Monty，不经 tokio。

协议同旧 driver：stdin/stdout 每行一个 JSON。

```text
宿主 → worker   第 1 行 {"code", "tools", "catalog", "limits"}；之后每行 {"ok": v} / {"error": "…"}
worker → 宿主   {"call": name, "args": {…}}（等答复）…  最后 {"done": {"outputs", "console", "error"}}
```

### 宿主管的限制

| 限制 | 由谁管 | 到限的样子 |
|---|---|---|
| 墙钟：`[execution] call_timeout_secs`（含等工具返回的时间） | 宿主 | SIGKILL，`脚本超时（Ns），已终止` |
| 内存：1 GiB 常驻内存 | 宿主每 100 ms 读 worker 的 RSS（macOS `proc_pid_rusage`、Linux `/proc/<pid>/statm`），超了 SIGKILL；Monty 另有同值的 `max_memory` 软限，预检单次大分配 | `脚本内存超限（…），已终止` / 脚本里 `MemoryError` |
| 取消 | 宿主：丢掉执行 future，`kill_on_drop` SIGKILL worker | 没有跨线程中断入口，只能杀进程；在途子调用沿用 §7 的恢复规则 |
| 子调用次数：256 | worker 数，journal 再核一遍 | 脚本里 `ToolError`；journal 侧越界是宿主终止条件 |
| 输出：`print` 与 `text()` 各 1 MiB | worker | 脚本里抛错 |
| 递归深度 | Monty 默认 1000 | `RecursionError` |

`monty-alloc`（Monty 自带的全局分配器计数）不能用：它要占 `#[global_allocator]`，与 turso 的 mimalloc
冲突（`the #[global_allocator] in this crate conflicts with global allocator in: turso`）。所以内存只能
软限 + 看门狗，缓慢增长有最多一个采样间隔的窗口。`setrlimit(RLIMIT_AS)` 在 macOS 上 `EINVAL`，也不用。

Monty 的 `max_turn_duration` 不含等宿主答复的时间，所以墙钟由宿主管；进程内的测试 worker（杀不掉）
才靠它兜底。

## 5. 脚本里的工具调用

`tools.<name>(**args)` → executor 的持久化子调用路径，依次：

1. 名字必须在当前 Run 的 `AgentSurface` 及 executor 工具表里；不回退到全局目录。
   `bash`、`python`、`edit`、`write`、`codemode`、`delegate`、`dispatch`、`follow`、`monitor`、
   `monitor_stop`、`update_plan` 不能在脚本里调（`tools/codemode.rs` 的 `NOT_IN_SCRIPTS`），也不进
   `TOOLS`。脚本正文里字面写着 `tools.<这些名字>(` 的，`codemode` 的 `prepare` 就拒、脚本不运行：
   实测模型常把 `tools.bash("git status")` 当成只读写进脚本，跑到那一行才被拒，整段作废，前面的
   MCP 写也可能已经做了（2026-10-13/14 十二次）。`getattr` 之类的绕法认不出，照样在这一步与第 2 步拒。
2. `tool.prepare(args)` 验证参数和约束。允许 `ReadFile`（read、rg）与 `McpCall`（含写操作）；
   本地 write / edit / bash / python、编排、`then_run` 仍在脚本外执行。
3. 完整参数、准备计划与 `plan.recovery.replay().and(tool.replay())` 先发布外置正文，再同步追加
   `intent` 记录。`started` 记录提交成功之后才允许执行工具。
4. 流式 stdout / stderr 保存在子调用自己的尝试目录；工具返回后发布完整原始输出，再发布
   脚本将接收的精确 dict 或错误。`delivered` 记录引用这些已发布内容，提交后才回送 Python。

顺序复用现有 `ToolOutputStore::publish` 与 `Ledger::append_audit`：外部正文同步/原子发布 →
JSONL append + sync → 日志索引数据库事务。这里没有子调用调度行，也没有 control_outbox 决策；
`append_audit` 仅复用 Coordinator 的串行追加、幂等 event_id 和索引提交机制。Gateway 是唯一开 state 的进程。

返回给脚本的 dict 保持原有接口：

```python
{"status": "completed" | "failed", "text": "<完整工具正文>", "result": <工具的结构化结果>}
```

普通执行/参数/不支持操作的错误可被 `try/except ToolError` 捕获；这些结果也持久化。
**uncertain、journal 读写/校验失败、恢复分歧是宿主终止条件**：宿主锁住错误，后续任何子调用都不执行，
父结果记为 uncertain 并返回模型。脚本捕获异常或伪造 done 不能解除该状态；模型据此核对外部状态。
stdout/stderr 底层写失败由宿主独立锁存；即使工具将其转成普通 Failed 或吞掉错误，仍禁止后续调用，
保留 started 未交付的中断证据。取消/超时收尾会重新读取并校验已交付 Reply 及 execution 引用；
已交付 uncertain 优先于普通 Cancelled/Failed，用户取消仍停止 Run。

例如，有副作用 MCP 可以和只读查询放进同一段脚本：

```python
result = tools.mcp__demo__bump()
text(result["text"])
```

MCP 的 `read_only` 仍由操作者声明，只决定中断后能否安全重做，不再决定 codemode 准入。

## 6. 模型看到的接口

工具 `codemode`，参数 `{"code": "<Python 源码>"}`。脚本是模块级代码：

| 名字 | 作用 |
|---|---|
| `tools.<name>(**args)` | 调工具，见 §5。名字里的 `-` 写成 `_`（`tools.mcp__dev_radius__search`）|
| `text(value)` | 加一段输出；字符串原样，其他值转 JSON（缩进 2、不转义非 ASCII） |
| `ToolError` | 工具调用失败时抛的异常；`RuntimeError` 的别名（见下） |
| `print(...)` | 进输出末尾的 `<console>` 段 |
| `TOOLS` | 这次脚本能调的工具名列表 |
| `ALL_TOOLS` | 能调的工具 `[{name, description}]`，含没声明给模型的（MCP 默认如此，`docs/mcp.md` §3） |
| `search_tools(query, limit=8, namespace=None)` | 按相关度排的 `[{name, description}]`（BM25，照 pi） |
| `describe_tool(name)` | `{name, description, parameters, namespace?}`，没有就是 `None` |
| `describe_namespace(name)` | 一台 MCP 服务器的 `{name, description?, instructions?, tools}`；认 `mcp__dev_radius`、`mcp__dev-radius`、`dev-radius`、`dev_radius` |

**`ToolError` 就是 `RuntimeError`**：Monty 的异常类型封闭、没有用户子类，宿主注入的类型也不能 `except`。
worker 在脚本前拼一段前置代码（`codemode/prelude.py`：`ToolError = RuntimeError`、`tools` 与下面的发现函数），
回溯里的行号按它的行数减回去。代价：`type(e).__name__` 是 `RuntimeError`，脚本自己抛的
`RuntimeError` 也会被 `except ToolError` 接住。

`tools` 也在前置代码里造：一个普通对象，`TOOLS` 里每个名字（`-` 换成 `_`）`setattr` 成一个闭包，闭包
再调宿主给的 `_call(name, args)`——所以 `f = tools.read`、`{"rg": tools.rg}`、`getattr(tools, name)(...)`
都能用。Monty 不支持 `__getattr__`，写错工具名（或调不在表里的名字）得到的是 `AttributeError: 'Tools'
object has no attribute 'x'`，不是原来的 `ToolError`（那条错误会列出能调的名字）；要找工具用 `TOOLS` /
`search_tools`。（POC 试过把 `tools` 做成宿主类实例、调用时暂停：未知名字能给出 `ToolError`，但
`f = tools.read` 取不到属性，`getattr` 动态派发全断，所以没采用。）

发现函数照 pi 的 `searchTools` / `describeTool` / `describeNamespace`：宿主把这次能调的工具说明、
schema 与所属服务器（含 instructions）随第一行交进 worker（`catalog`），查找与排序用前置代码里的
BM25 在 Monty 里做完（原来 `driver.py` 的那份，逻辑未动），不经宿主、不进 journal，也不进指纹（调用本身
已按工具名与参数核对）。分词与停用词同 pi；pi 只认 a-z0-9，这里另把中日韩字切成单字与相邻两字，中文说明
才搜得到。名字一律是脚本里的写法（`-` 换成 `_`）。

结果正文：`text()` 各段按顺序，`<console>` 段随后，失败时附 `脚本出错：` 与 traceback（Monty 的回溯格式，
行号是脚本自己的行），最后一行是调用摘要（`调用了 3 次工具：read×2、rg×1`）。超长照常由投影截头尾。

## 7. 计划、journal 与恢复

父计划是 `Operation::Codemode { code }`。`replay = Safe` / `SafeReread` 表示可以重新进入宿主的
journal 协议，**不表示整段工具操作可以重做**；`is_read_only()` 为 false，父调用是调度屏障。

首次启动先提交 `start`（指纹版本、代码哈希、Monty 版本、worker 协议版本、工具表、cwd）。子调用按宿主收到的
顺序编号 `c1..c256`，逻辑身份为父 Run / Call + 子序号；存储路径使用父 Call 与阶段/序号的 SHA-256
组成安全的 `cm-…` Call ID。它不依赖新的父 attempt，重启仍能认回同一子调用；每次实际执行的输出
使用父 attempt + 子身份派生的独立 Attempt ID，旧引用不会被覆盖；这也避免底层输出存储按
Attempt ID 索引时误取父调用正在写的流。脚本不能自选 key，也不能绕过宿主的 256 次限制。

| 恢复时已记录的阶段 | 处理 |
|---|---|
| `intent`，无 `started` | 确定未执行；当前能力面与 prepare 再核对后，用已存计划执行 |
| `started`，无 `delivered` | 已存与当前 `Tool::replay` 都为 Safe 才用原计划重跑；否则 uncertain，绝不重做 |
| `delivered` | 校验引用和完整工具输出，返回已存精确结果；包括 unsafe 成功、普通失败及 uncertain |
| `finished` | 直接返回已提交的整段输出，不启动 Python |

脚本每次完成后先外置整段 `ToolOutput` 并提交 `finished`，再交给父 executor 发布和 `finish_call`。
父结果发布前后中断时可以复用这个完整结果；父子工具结果不混进顶层 LLM 消息。

恢复逐次核对工具名、完整 JSON 参数与顺序；结束时检查全部记录是否被消费。代码、Monty 版本、worker 协议、
能力表、cwd 改变，参数或顺序改变，提前结束，或者引用损坏，均明确返回 uncertain。普通脚本异常
仍是 Failed；`started` 保存子尝试身份，恢复 unsafe 或时限/取消中断时读取该尝试留下的部分输出，
将有上限的 stdout/stderr 尾部及目录存入父 uncertain 正文，仍由父结果统一投影。
时限/取消打断正在执行的子调用时，持久化的父结果保留 uncertain（用户取消仍停止 Run）。
升级前没有 durable `start` 的中断父调用也返回 uncertain，不能按旧的“整段只读”假设重跑。
2026-10-15 换引擎时同理：旧 `start` 指纹里是 `driver` / `env`（CPython 时代），与新指纹对不上，
在途的 codemode 调用恢复时按分歧返回 uncertain，由模型核对外部状态（有测试守着）。

### 脚本的恢复边界

这不是 pi-durable 的确定性 JS VM：脚本的时钟与随机数现在用 Monty 的系统实现，没有进 journal，
纯计算与输出不保证字节确定（Monty 的 set / dict 序与 `hash()` 本身是确定的）。实现因此只允许恢复
**已记录的调用前缀**，不允许恢复期间发出任何新子调用。
恢复可重新取得已完成的写结果并生成汇总；需要继续未记录的工作时由模型另发调用。
即使新调用仍是只读，超出旧前缀也会明确失败，避免动态脚本在未提交窗口制造另一个写 key。
（下一步：时钟 / 熵走 Monty 的 `CallHost` 进 journal，恢复才能越过前缀继续；那一步之前保持现状。）

工具成功但 `delivered` 尚未提交时，不能猜测未引用的 output.json 代表可交付成功；unsafe 返回
uncertain，Safe 才能按双 Safe 准则重做。`start` 已提交但第一个 intent 未提交时，恢复禁止新增
调用并返回 uncertain。已提交 successful write 子结果始终复用，永不执行第二次。

## 8. 限制

- 时限：`[execution] call_timeout_secs`，到点 SIGKILL worker；完整输出落盘，模型视图最多 50 KiB / 2000 行；内层调用最多 256 次。
- 内存、输出、递归等 worker 侧限制见 §4 的表。
- 脚本之间不共享状态（pi 的 `store/load` 不做）。
- 不跑模型（pi 的 `models` 不做）。
- 脚本读不到文件系统：要读文件走 `tools.read`（这个版本没有文件挂载）。

## 9. 实现落点

| 位置 | 内容 |
|---|---|
| `komo-kernel` `types/plan.rs` | `Operation::Codemode` |
| `komo-runtime` `codemode/mod.rs` | `Worker`：起 worker 进程、协议对话、墙钟 / 内存看门狗 / 杀进程；`ScriptCatalog` |
| `komo-runtime` `codemode/worker.rs` | worker 本体（`komo __codemode-worker` 的函数体）：跑 Monty、翻译暂停、OsCall 一律拒 |
| `komo-runtime` `codemode/convert.rs` | `serde_json::Value` ↔ `MontyObject` |
| `komo-runtime` `codemode/prelude.py` | 拼在脚本前面：`ToolError`、`tools`、BM25 发现函数 |
| `komo-runtime` `limits.rs` | `resident_bytes`：worker 的常驻内存读数 |
| `komo-runtime` `tools/codemode.rs` | `CodemodeTool`：定义 + `prepare`；`execute` 不该被走到（与 `dispatch` 同一模式） |
| `komo-runtime` `executor/codemode.rs` | 起 worker、能力与 prepare 检查、子输出发布、宿主 fatal 锁、`start` 指纹 |
| `komo-runtime` `executor/codemode_journal.rs` | journal 阶段、稳定身份、分页读取与回放校验 |
| `komo-gateway` `service/mod.rs` | 启动时解析 `current_exe()`，注册工具并把 `Worker` 交给执行器 |
| `komo` `src/main.rs` | 隐藏子命令 `__codemode-worker`，只分发到 `codemode::worker::run` |

## 10. 验收

- 语言边界（进程内 worker，`codemode/tests.rs`）：脚本打不开文件、`import subprocess` / `socket` 失败、
  读不到环境变量，且协议（工具调用往返、`ToolError`、部分输出、256 次上限）照常通。
- 发现函数与迁移前一致：同一份固定目录，`search_tools`（BM25、中文分词、`namespace` 过滤）、
  `describe_tool`、`describe_namespace`、`ALL_TOOLS` 的结果与迁移前 CPython `driver.py` 跑出的逐项相同；
  `text()` 非字符串输出逐字节同 `json.dumps(indent=2, ensure_ascii=False, default=str)`。
- 真 worker 进程（`crates/komo/tests/codemode_worker.rs`）：脚本调 `read` 两次、过滤后 `text()`，只有
  过滤结果出来；`except ToolError` 接住失败的子调用；死循环在墙钟到点被 SIGKILL、父进程完好并能继续跑
  下一段；缓慢涨内存被 RSS 看门狗杀（256 MiB 上限，约 3 s）；打不开文件、环境变量为空；
  `komo --help` 不列出 `__codemode-worker`。
- 端到端（真 Gateway，进程内 worker，`komo-gateway/tests/codemode`）：脚本调 `read` 两次、过滤后
  `text()`，模型只看到过滤结果；字面写 `tools.write(` 的脚本在运行前被拒（`NOT_IN_SCRIPTS`，一行都不跑），
  `getattr(tools, "write")` 也调不到；工作区外的读放行；死循环到 `call_timeout_secs` 收尾，Run 照常结束。
- 持久化与恢复单测：完成的 unsafe 子调用不重复、intent 无 started 可执行、stored/current 双 Safe 的
  四种组合、参数/工具/顺序分歧、少调用/多调用、未提交交付窗口、uncertain 被捕获后仍不能续写、
  journal 分页、当前能力面/操作变化、脚本的前缀回放与超时中断；另有：换引擎前（CPython driver 时代）的
  `start` 指纹恢复时按分歧返回 uncertain、不起 worker。
- 真 Gateway + stdio MCP：脚本调用 bump 写计数文件、内层结果不生成模型消息；在 delivered 提交前/
  后故障停机，丢弃父 orphan 输出强制 journal 冷恢复，计数保持一次；分别得到 uncertain / completed。

测试里的 worker 是进程内的（线程 + socketpair，同一套协议；`Worker::in_process`，只在 `test` /
`test-support` 下存在），因为 `cargo test` 的进程自己不是 `komo`；杀进程这一类行为只能在真进程上验，
所以放在 `crates/komo/tests`。

2026-10-15 在 macOS 上验证（本机满载，其他 agent 同时在编译，耗时仅作参考）：

- `cargo fmt --check` 通过。
- `ulimit -n 8192; cargo test --workspace --no-fail-fast`：1893 passed、0 failed、8 ignored；其中
  新增 runtime `codemode` 模块测试 11 项与指纹回归 1 项、`komo` 的 `codemode_worker` 7 项，Gateway
  `codemode` 集成 5 项与 MCP 集成照常通过。
- 回放集（POC，`poc/monty` 分支的 `monty_check` / `monty_replay`）：381 段真实 codemode 脚本全部编译通过，
  130 段有完整子调用录像的回放全部行为一致。换成"`tools` 在前置代码里造"之后没有重跑这一套，升级 Monty 或
  改前置代码时要重跑。

旧引擎（CPython + Seatbelt / Landlock）的验证记录见 git 历史（2026-10-08 ～ 2026-10-13）。
