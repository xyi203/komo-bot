# codemode：Monty 里的 Python 脚本

> 状态：已实现 Monty 引擎与持久化子调用（2026-10-15 起引擎由"CPython + 操作系统沙箱"换成 pydantic
> Monty；此前的实现见 git 历史）。脚本在 komo 自己起的 worker 子进程里跑，工具调用由宿主执行；
> 允许只读工具及有副作用 MCP；文件**只读**，写文件走 write / edit / bash。恢复借鉴 pi-durable 的子调用
> 稳定 key、已存/当前 replay 双 Safe 与结果交付 journal；脚本的时钟与随机数经宿主、进 journal，
> 恢复时整段重跑、已记录的原样回放、越过记录之后照常执行（§7）。设计权威为 `docs/komo_bot.md`。

## 1. 要解决什么

模型一次只能发几个工具调用，每个结果都整段进上下文。查十个文件、过滤 MCP 返回的大 JSON、
把三处结果拼成一张表，要么多轮来回，要么把一堆用不上的正文塞进上下文。

codemode 让模型写**一段脚本**：脚本里调工具、在本地过滤和汇总，只有脚本的输出回到模型。

## 2. 为什么是 Monty，不是操作系统沙箱

脚本能不能**放心地自动跑**、中断后能不能**整段重跑**，取决于脚本本身能不能碰外部世界。

| 方案 | 隔离 | 结论 |
|---|---|---|
| QuickJS（pi 的做法） | 引擎里没有文件 / 网络 / 进程 | 可行，新依赖 `rquickjs`，要写一层 Rust↔JS 桥；模型写 Python 更顺 |
| 裸 Python（当时已有的 `python` 工具，2026-10-15 已删除） | 无：脚本就是任意代码 | 内层调用再只读也没用，脚本自己就能改东西、联网，也不能整段重跑 |
| CPython + 操作系统沙箱（2026-10-15 前的做法：macOS Seatbelt、Linux Landlock + seccomp） | 进程级：不能写文件、不能联网、不能起子进程 | 能用，但要按平台各写一套约束、探解释器、启动时自检，自检不过就没有 codemode；Python 没有确定性 VM，恢复只能走已记录的前缀 |
| **pydantic Monty** | 语言层：解释器里没有 socket / subprocess，文件与环境变量只能经宿主放行的 OsCall（只放行读） | **采用** |

Monty 是 Rust 写的 Python 子集解释器，语言本身没有文件、网络、子进程，要碰宿主只能暂停到宿主手里
（工具调用、OsCall、时钟与熵）。所以**不再需要操作系统沙箱**：没有 Seatbelt / Landlock / seccomp，没有解释器
探测，没有启动自检，也就没有"沙箱起不来所以不注册 codemode"这回事——surface 允许就能用。代价：

- 语言是 Python 子集：只有 `json` / `re` / `math` / `datetime` / `collections` / `itertools` / `functools`
  等少数标准库，没有 `os.path`、`glob`、`subprocess`、`hashlib`、`enum`、`csv` 等，**没有第三方库**
  （此前"托管环境里装好的库可以只读导入"没有了）。Monty 做不到的，模型改用 `bash` 跑 `python3`——
  codemode 只管汇总与过滤，不追求完整 Python。2026-10-15 POC 用 381 段真实 codemode 脚本验证：
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
                              stdout ──▶ 第一行握手 {"hello": 版本}；宿主核对，对不上就不发脚本
                              stdin  ◀── 第一行：{code, tools, catalog, cwd, protected, limits}；之后：答复
                              stdout ──▶ {"call": name, "args": {...}} / {"os": 时钟或熵} / {"done": {...}}
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
- **文件只读**。worker 把宿主的 `/` 用 `monty-fs` 只读挂载进来（`open()` / `pathlib` 的读、`iterdir`、
  `exists` / `stat` 都能用，相对路径按这次调用的 cwd；`os.getcwd()` 就是它）。每个 OsCall 先在 worker 里拦，
  再交给挂载：
  - 路径先规范化（`..`、符号链接都解掉，与 `write` / `edit` 同一个 `real_path`），落在 komo 自己的状态里就
    `PermissionError`——`<数据目录>/sessions/`、`state.db*`、`runtime/`、`.env`，**读也不行**。名单就是
    `write` / `edit` 用的那一份（`tools/protect.rs` 的 `Protected`；Gateway 装配时交给 `Worker::protecting`，
    随请求发进 worker），只有一处定义。符号链接指到状态里也认得出（比对的是真实路径）；硬链接认不出。
  - 写类一律拒：`write_text` / `write_bytes` / append / `mkdir` / `unlink` / `rmdir` / `rename` / `open` 的
    `w` `a` `x` 模式，`PermissionError`，消息指路"用 write / edit 工具"。挂载本身也是只读的，是第二道。
  - 读到的是读的那一刻的磁盘，**不进 journal**；恢复时重读，内容变了会在后面的调用参数上露出（§7）。
  - 挂载（cap-std）不跟绝对路径的符号链接，所以交给它的是已经解掉链接的真实路径；`is_symlink` 看链接本身，不改写。
- 环境变量：`os.getenv` 回默认值、`os.environ` 为空。
- 时钟与随机数：`time.*`、`datetime.now()`、`date.today()`、`os.urandom`、`random` 的起始状态都经宿主回答并进 journal
  （Monty 的 `DateTimeSource::CallHost` / `RandomStart::CallHost`，见 §7）。`time.sleep` 在 worker 里睡
  （Monty 每次最多 10 s），总时间受墙钟限制，到点整个 worker 被杀。
- worker 进程环境变量全清，不给 `LANG` 也不给任何凭证；stdin / stdout 只走协议，脚本的 `print`
  进 worker 内存里的缓冲（`<console>` 段），碰不到协议用的 fd。

### worker 进程

宿主为每次 codemode 调用起一个 worker：`<komo 可执行文件> __codemode-worker`（clap 隐藏子命令，不进
`--help`；函数体在 `komo-runtime` 的 `codemode::worker`，bin 只分发）。可执行文件是 Gateway 启动时
`current_exe()` 解析一次的路径。不用进程池：每次起一个 6 ms，换来每段脚本独立的内存与崩溃边界。
版本握手：`komo update` / `scripts/install-local.sh` 原子换掉磁盘上的二进制之后、Gateway 重启之前，这台 Gateway
起的 worker 是新版本（路径没变）。所以 worker 的**第一行输出**是握手 `{"hello": {protocol, monty, komo}}`，
宿主与自己的版本比，对不上就在发脚本之前干净失败（`ScriptOutcome.incompatible`）：工具结果是 Failed，
消息说明两边的版本并让 operator 重启 Gateway（`komo gateway restart`）；脚本一行没跑，不记 `finished`，
重启之后同一个调用能照常跑。只有恢复中（前几次尝试已经做过子调用）碰上它才记 uncertain。
worker 在专用的大栈（64 MiB）线程里跑 Monty，不经 tokio。

协议同旧 driver：stdin/stdout 每行一个 JSON。

```text
worker → 宿主   第 1 行 {"hello": {"protocol", "monty", "komo"}}
宿主 → worker   第 1 行 {"code", "tools", "catalog", "cwd", "protected", "limits"}；之后每行 {"ok": v} / {"error": "…"}
worker → 宿主   {"call": name, "args": {…}}（等答复）、{"os": {"op": "clock"}} / {"os": {"op": "urandom", "size": n}}
                （等答复，见 §7）…  最后 {"done": {"outputs", "console", "error"}}
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
   `bash`、`edit`、`write`、`codemode`、`task`（2026-10-16 之前是 `delegate`、`dispatch`、`follow`）、`monitor`、
   `monitor_stop` 不能在脚本里调（`tools/codemode.rs` 的 `NOT_IN_SCRIPTS`），也不进
   `TOOLS`。脚本正文里字面写着 `tools.<这些名字>(` 的，`codemode` 的 `prepare` 就拒、脚本不运行：
   实测模型常把 `tools.bash("git status")` 当成只读写进脚本，跑到那一行才被拒，整段作废，前面的
   MCP 写也可能已经做了（2026-10-13/14 十二次）。`getattr` 之类的绕法认不出，照样在这一步与第 2 步拒。
2. `tool.prepare(args)` 验证参数和约束。允许 `ReadFile`（read、rg）与 `McpCall`（含写操作）；
   本地 write / edit / bash、编排、`then_run` 仍在脚本外执行。
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

每次调用都是独立脚本，`json`、`re`、`time` 等要在本段显式 import。Monty 1.1.0 没有
`datetime.datetime.fromtimestamp` / `utcfromtimestamp`；Unix 秒的展示使用已在真实 worker
验证的写法（支持小数与负时间戳，只展示到秒）：

```python
import time
ts = 1729324800.25
text(time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(float(ts))))
```

固定 UTC+8 在 `gmtime` 入参加 `8 * 3600`，不处理地区夏令时。这里仅补工具契约与精确的
错误提示，不自动导入、不改前置代码、Monty 版本或恢复指纹。

子工具的外层固定为 `{status, text, result}`：先检查 `status`，`failed` 可以正常返回字典，
不自动抛 `ToolError`；脚本可自行处理后继续。`read` 的正文是 `text`，MCP 的 `result` 是原生
CallToolResult，业务结构可能在 `structuredContent`，或需显式解析 `content` 中的 text，不能
统一假设外层有 `data`。路径先用 `rg` / `list_files` 定位；独立项可以逐项捕获 `ToolError`，保留成功项。

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
都能用。Monty 不支持 `__getattr__`（1.1.0 里 `class T: def __getattr__` 定义得了、但属性缺失时不调它，
仍是 `AttributeError`），写错工具名（或调不在表里的名字）得到的是 `AttributeError: 'Tools'
object has no attribute 'x'`，不是原来的 `ToolError`（那条错误会列出能调的名字）；要找工具用 `TOOLS` /
`search_tools`——`codemode` 的工具说明里点明了这一点。（POC 试过把 `tools` 做成宿主类实例、调用时暂停：未知名字能给出 `ToolError`，但
`f = tools.read` 取不到属性，`getattr` 动态派发全断，所以没采用。）

发现函数照 pi 的 `searchTools` / `describeTool` / `describeNamespace`：宿主把这次能调的工具说明、
schema 与所属服务器（含 instructions）随第一行交进 worker（`catalog`），查找与排序用前置代码里的
BM25 在 Monty 里做完（原来 `driver.py` 的那份，逻辑未动），不经宿主、不进 journal，也不进指纹（调用本身
已按工具名与参数核对）。分词与停用词同 pi；pi 只认 a-z0-9，这里另把中日韩字切成单字与相邻两字，中文说明
才搜得到。名字一律是脚本里的写法（`-` 换成 `_`）。

结果正文：正常时 `text()` 各段按顺序，`<console>` 段随后，最后是调用摘要
（`调用了 3 次工具：read×2、rg×1`）。子调用失败但脚本处理后正常结束时，失败计数放到正文首部，
外层状态仍为 completed。脚本异常时先给异常原因、精确修复提示、调用摘要，再接脚本输出、console
和完整 traceback（Monty 的回溯格式，行号是脚本自己的行）。非 bash 结果的模型投影保留头部，
因此大段输出不会先把失败原因挤掉；没有另开一条模型投影路径。

失败首部最多列 8 个子调用的 `c1..c256`、工具名、状态与原始输出的 `artifact://…/result` 入口，
提醒已执行的子调用不会因脚本报错回滚，核对后继续剩余步骤。超过时说明省略数量，完整清单仍在
外层 `output.json` 的 `result.calls`，统一投影在截断时给出完整输出入口。它不能阻止模型另起一次新写调用。

`result.calls` 新增 `id`、可选 `status` 与可选 `output`（已发布子输出入口）；`ok` 仍只表示宿主正常
返回，失败计数同时检查 `ok` 与工具自身的 `status=failed`。工具没有返回 ToolOutput 时不编造 status，
首部显示“未返回工具结果”；prepare 阶段被拒且未执行时也不会给出虚假的子执行输出入口。
引用来自 journal 中的原尝试，恢复时不替换成当前父 attempt。旧 `finished` 输出逐字复用，
不会补新字段或重新生成提示；新结构不改写历史 output.json。

## 7. 计划、journal 与恢复

父计划是 `Operation::Codemode { code }`。`replay = Safe` / `SafeReread` 表示可以重新进入宿主的
journal 协议，**不表示整段工具操作可以重做**；`is_read_only()` 为 false，父调用是调度屏障。

首次启动先提交 `start`（指纹：指纹版本、代码哈希、前置代码哈希、Monty 版本、worker 协议版本、工具表、cwd）。子调用按宿主收到的
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

恢复逐次核对工具名、完整 JSON 参数与顺序；结束时检查已记录的调用与读数是否都被消费。代码、前置代码、Monty
版本、worker 协议、能力表、cwd 改变，参数或顺序改变，提前结束，或者引用损坏，均明确返回 uncertain。普通脚本异常
仍是 Failed；`started` 保存子尝试身份，恢复 unsafe 或时限/取消中断时读取该尝试留下的部分输出，
将有上限的 stdout/stderr 尾部及目录存入父 uncertain 正文，仍由父结果统一投影。
时限/取消打断正在执行的子调用时，持久化的父结果保留 uncertain（用户取消仍停止 Run）。
升级前没有 durable `start` 的中断父调用也返回 uncertain，不能按旧的“整段只读”假设重跑。
2026-10-15 换引擎时同理：旧 `start` 指纹里是 `driver` / `env`（CPython 时代）；同一天时钟与熵进 journal
（worker 协议 1 → 2、指纹加前置代码哈希），此前 Monty 第一版（时钟与熵不进 journal）的在途调用同样对不上。
这些调用恢复时按分歧返回 uncertain，由模型核对外部状态（有测试守着）。

### 脚本的恢复边界：整段重跑，记录的原样回放，越过记录之后照常执行

恢复时起一个新 worker **从头重跑脚本**。宿主一边回放 journal，一边把脚本当作和第一次同一段确定性程序：

- 已记录的子调用按序回放：每次调用核对工具名与完整参数，一致就把已交付的结果原样还给脚本（unsafe 成功、
  普通失败、uncertain 都一样），**不重新执行**；对不上就是分歧，整段 uncertain。
- 已记录的时钟与随机数读数（见下）按序原样还回去。
- 记录用完之后，脚本发出的**新**子调用走正常流程：prepare → intent → started → 执行 → delivered。
  `started` 而没 `delivered` 的 unsafe 调用仍然 uncertain、绝不重做（上表，双 Safe 准则不变）。

**为什么重跑得出同样的东西**（这是 pi-durable 在 JS VM 里做的事，这里靠 Monty 的性质加宿主记账）：

- Monty 没有哈希随机化，`set` / `dict` 的迭代序是确定的（插入序）；`hash()` 也是确定的。同一段代码、同一批
  输入，纯计算每次一样。
- 不确定的来源只有三类，全走宿主：**工具调用**（journal 的 `delivered`）、**时钟与熵**（下）、**文件读取**。
  前两类记账、回放；文件读取**不记**，恢复时重读——文件内容变了，脚本可能走到不同的分支，但它要影响外部
  世界只能经下一次工具调用，而那次调用的完整参数会和记录比对，对不上就 uncertain，**在执行任何新调用之前**。
  所以分歧总是在副作用发生之前被发现，不会在记录之外悄悄多做一件事。内容变了、但没改变后面任何调用的参数，
  那就无关紧要，脚本照常完成。
- `time.sleep`、`print` / `text()` 输出不产生外部效果（输出只在脚本结束时一并交出）。

**时钟与熵**：worker 把 Monty 的 `DateTimeSource` 与 `RandomStart` 设成 `CallHost`。`time.time()` 一族、
`datetime.now()`、`date.today()`（读 Unix 秒）与 `os.urandom(n)`、`random` 给自己起初始状态要的 2496 字节
都暂停到宿主：宿主回答真实值，并按序记进 journal（`nondet` 记录：`[{request, value}]`，正文外置到
`tool-output/`，fold 不看，与其他 `codemode.journal` 事件一样）。恢复时已记录的读数按序原样还回去，请求
的种类（clock / urandom 及字节数）与记录对不上同样是分歧，记录没读完脚本就结束也是。`time.monotonic()` /
`perf_counter()` 也是读这个时钟（答的是挂钟秒数）；`process_time()` 恒为 0。

**journal 的增长**：读数不是一次一条记录。宿主把读到的读数攒在内存里，**在下一次新子调用写 `intent` 之前**
（以及写 `finished` 之前）一并落成一条 `nondet` 记录——因为只有子调用的参数和最终输出能把这些值带到脚本
之外，所以先落账再提交 `intent` 就够了；之间崩了，没落账的读数没有被任何已提交的外部效果用过，恢复时重新读
就是。结果：一个读时钟的紧循环（几万次）也只多一条记录，记录数 ≤ 子调用数 + 1（≤ 257），不随读取次数涨。
单条记录的体积和单次脚本里读数的个数成正比，所以单段脚本合计最多读 10 000 次（含恢复时已记录的），再读
就在脚本里抛错（`一段脚本最多读 10000 次时钟 / 随机数`）；单次 `os.urandom` 最多 64 KiB。

工具成功但 `delivered` 尚未提交时，不能猜测未引用的 output.json 代表可交付成功；unsafe 返回
uncertain，Safe 才能按双 Safe 准则重做。`start` 已提交但第一个 intent 未提交时，恢复照常从头跑（没有任何子调用做过）。已提交 successful write 子结果始终复用，
永不执行第二次。

## 8. 限制

- 时限：`[execution] call_timeout_secs`，到点 SIGKILL worker；完整输出落盘，模型视图最多 50 KiB / 2000 行；内层调用最多 256 次。
- 内存、输出、递归等 worker 侧限制见 §4 的表。
- 脚本之间不共享状态（pi 的 `store/load` 不做）。
- 不跑模型（pi 的 `models` 不做）。
- 文件只读（§4）；要写文件、联网、装库，改用 `write` / `edit` / `bash`（`python3`），Monty 做不到的都是这条路。
- 读时钟与随机数合计最多 10 000 次、单次 `os.urandom` 最多 64 KiB（§7）。

## 9. 实现落点

| 位置 | 内容 |
|---|---|
| `komo-kernel` `types/plan.rs` | `Operation::Codemode` |
| `komo-runtime` `codemode/mod.rs` | `Worker`：起 worker 进程、协议对话、墙钟 / 内存看门狗 / 杀进程；`ScriptCatalog` |
| `komo-runtime` `codemode/worker.rs` | worker 本体（`komo __codemode-worker` 的函数体）：握手、跑 Monty、翻译暂停、只读文件挂载与状态名单、时钟 / 熵问宿主 |
| `komo-runtime` `codemode/convert.rs` | `serde_json::Value` ↔ `MontyObject` |
| `komo-runtime` `codemode/prelude.py` | 拼在脚本前面：`ToolError`、`tools`、BM25 发现函数 |
| `komo-runtime` `limits.rs` | `resident_bytes`：worker 的常驻内存读数 |
| `komo-runtime` `tools/protect.rs` | `Protected`：komo 自己的状态名单，`write` / `edit` / `bash` 与 codemode worker 共用 |
| `komo-runtime` `tools/codemode.rs` | `CodemodeTool`：定义 + `prepare`；`execute` 不该被走到（与 `task` 同一模式） |
| `komo-runtime` `executor/codemode.rs` | 起 worker、能力与 prepare 检查、子输出发布、宿主 fatal 锁、`start` 指纹 |
| `komo-runtime` `executor/codemode_journal.rs` | journal 阶段、稳定身份、分页读取与回放校验 |
| `komo-gateway` `service/mod.rs` | 启动时解析 `current_exe()`，注册工具并把 `Worker` 交给执行器 |
| `komo` `src/main.rs` | 隐藏子命令 `__codemode-worker`，只分发到 `codemode::worker::run` |

## 10. 验收

- 语言与文件边界（进程内 worker，`codemode/tests.rs`）：`import subprocess` / `socket` 失败、读不到环境变量；
  文件可读（`open` / `Path.read_text` / `iterdir` / `exists` / `stat` / 相对路径 / 绝对符号链接）；
  `.env`、`state.db*`、`sessions/x`、`runtime/x` 读被拒，含 `..` 与指向它们的符号链接；每一种写
  （`write_text` / `write_bytes` / `open('w'|'a'|'r+')` / `mkdir` / `unlink` / `rmdir` / `rename`）被拒且磁盘不变；
  时钟与熵由宿主回答；握手对不上的 worker 在脚本运行前被拒；协议（工具调用往返、`ToolError`、部分输出、
  256 次上限）照常通。
- 发现函数与迁移前一致：同一份固定目录，`search_tools`（BM25、中文分词、`namespace` 过滤）、
  `describe_tool`、`describe_namespace`、`ALL_TOOLS` 的结果与迁移前 CPython `driver.py` 跑出的逐项相同；
  `text()` 非字符串输出逐字节同 `json.dumps(indent=2, ensure_ascii=False, default=str)`。
- 真 worker 进程（`crates/komo/tests/codemode_worker.rs`）：脚本调 `read` 两次、过滤后 `text()`，只有
  过滤结果出来；`except ToolError` 接住失败的子调用；死循环在墙钟到点被 SIGKILL、父进程完好并能继续跑
  下一段；缓慢涨内存被 RSS 看门狗杀（256 MiB 上限，约 3 s）；文件可读、写被拒、环境变量为空；
  komo 状态（含 `..` 与符号链接）连读都拒；假的"另一版"worker（握手协议号不同）被拒、脚本没运行；
  `komo --help` 不列出 `__codemode-worker`。
- 端到端（真 Gateway，进程内 worker，`komo-gateway/tests/codemode`）：脚本调 `read` 两次、过滤后
  `text()`，模型只看到过滤结果；字面写 `tools.write(` 的脚本在运行前被拒（`NOT_IN_SCRIPTS`，一行都不跑），
  `getattr(tools, "write")` 也调不到；工作区外的读放行；死循环到 `call_timeout_secs` 收尾，Run 照常结束。
- 持久化与恢复单测：完成的 unsafe 子调用不重复、intent 无 started 可执行、stored/current 双 Safe 的
  四种组合、参数/工具/顺序分歧、少调用/多调用、未提交交付窗口、uncertain 被捕获后仍不能续写、
  journal 分页、当前能力面/操作变化、超时中断；换引擎前（CPython driver 时代）与时钟进 journal 之前
  （worker 协议 1）的 `start` 指纹恢复时按分歧返回 uncertain、不起 worker。越过记录的前缀继续执行：
  崩在第 N 次调用交付之后，恢复回放 N 次、第 N+1 次只执行一次，时钟 / 随机数与崩溃前记下的完全一致
  （`datetime.now()` / `time.time()` / `random.random()` 的值作为调用参数，回放时逐字节核对）；读的文件在两次
  运行之间变了、导致下一次调用的参数不同 → uncertain、之后不再执行任何子调用；读数的种类对不上记录 / 少读
  → 分歧；时钟紧循环只落两条 `nondet` 记录（每批一条），读数上限 10 000；握手对不上的 worker 让这次调用
  干净失败（Failed、消息要求重启 Gateway、不记 `finished`），换成同版 worker 后同一调用照常跑。
- 真 Gateway + stdio MCP：脚本调用 bump 写计数文件、内层结果不生成模型消息；在 delivered 提交前/
  后故障停机，丢弃父 orphan 输出强制 journal 冷恢复，计数保持一次；分别得到 uncertain / completed。

测试里的 worker 是进程内的（线程 + socketpair，同一套协议；`Worker::in_process`，只在 `test` /
`test-support` 下存在），因为 `cargo test` 的进程自己不是 `komo`；杀进程这一类行为只能在真进程上验，
所以放在 `crates/komo/tests`。

2026-10-15 在 macOS 上验证（本机满载，其他 agent 同时在编译，耗时仅作参考）：

- `cargo fmt --check` 通过。
- `ulimit -n 8192; cargo test --workspace --no-fail-fast`：1907 passed、0 failed、8 ignored（时钟 / 熵进 journal、
  只读文件、版本握手这一步新增 runtime `codemode` 模块 5 项、journal 恢复 8 项、`komo` 的 `codemode_worker` 2 项）。
- 回放集（`crates/komo/examples/codemode_replay.rs`，对**当前** worker 与前置代码；脚本与录像是从本机会话
  只读抽出的数据，不进仓库）：384 段真实 codemode 脚本全部编译通过；其中 132 段有完整子调用录像，回放的
  调用序列（工具名 + 完整参数）全部与录像一致（132/132）：112 段干净跑完，20 段以脚本异常结束——都是录像里
  那次调用本来就返回了错误（被拒绝的 shell / edit、路径不存在）或脚本自己的 bug（`NameError`），与当时一致。
  升级 Monty 或改前置代码时要重跑。

2026-10-20 失败反馈优化验证（macOS）：

- 新增回归覆盖 payload failed 与宿主错误的区分、脚本处理子失败后正常完成、70KB 输出后
  的 NameError 在 2KiB 投影首部可见、部分写入后的原尝试引用、摘要上限、旧 finished 原样回放；
  真实 worker 验证 UTC / 固定 UTC+8 / 小数 / 负时间戳。改前实测 3 项行为测试失败，改后通过。
- 主仓 `cargo test --workspace --offline --locked`：2065 passed、0 failed、8 ignored
  （含此前后台完成通知修复）；`cargo fmt --all --check` 与 `git diff --check` 通过。
- 隔离 worktree `cargo build --timings --offline --locked`：增量 dev 构建 1m32s；报告
  `cargo-timing-20261020T072354554Z-b0081c9e8e56a87a.html`，target 位于
  `/Users/xiangyi/code/komo-session-observability/target/cargo-timings/`。切换工作区导致多个 crate
  重新编译，此耗时不作冷构建收益对比。
- 两天 32 条真实失败记录中，27 条用录下的子工具回复与时钟/熵复现原异常；4 条涉及未保存的
  发现目录、1 条没有可回放 journal，未计入有效回放。没有调用业务 MCP；这是失败复现，
  不是旧脚本自动变成功。重试次数的改善仍需安装后用新 session 验证。

旧引擎（CPython + Seatbelt / Landlock）的验证记录见 git 历史（2026-10-08 ～ 2026-10-13）。
