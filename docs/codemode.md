# codemode：沙箱里的 Python 脚本

> 状态：已实现沙箱与持久化子调用。Python 仍在操作系统沙箱中运行，工具调用由宿主执行；
> 允许只读工具及有副作用 MCP。恢复借鉴 pi-durable 的子调用稳定 key、已存/当前 replay 双 Safe
> 与结果交付 journal；Python 没有确定性 VM，所以恢复限制见 §7。设计权威为 `docs/komo_bot.md`。

## 1. 要解决什么

模型一次只能发几个工具调用，每个结果都整段进上下文。查十个文件、过滤 MCP 返回的大 JSON、
把三处结果拼成一张表，要么多轮来回，要么把一堆用不上的正文塞进上下文。

codemode 让模型写**一段脚本**：脚本里调工具、在本地过滤和汇总，只有脚本的输出回到模型。

## 2. 为什么是"沙箱里的 Python"，不是 QuickJS

脚本能不能**放心地自动跑**、中断后能不能**整段重跑**，取决于脚本本身能不能碰外部世界。

| 方案 | 隔离 | 结论 |
|---|---|---|
| QuickJS（pi 的做法） | 引擎里没有文件 / 网络 / 进程 | 可行，新依赖 `rquickjs`，要写一层 Rust↔JS 桥 |
| 裸 Python（komo 已有的 `python` 工具） | 无：脚本就是任意代码 | 内层调用再只读也没用，脚本自己就能改东西、联网，也不能整段重跑 |
| **Python + 操作系统沙箱** | 进程级：不能写文件、不能联网、不能起子进程、只能读解释器自己 | **采用**：零新依赖，模型写 Python 最顺，还能用托管环境里装好的库 |

2026-10-08 在 macOS 27 上实测（`sandbox-exec` + 下面 §4 的配置）：写 `~` 下的文件、连
`1.1.1.1:80`、`subprocess.run`、读 `~/.komo/.env`、列 `~/.ssh` 全部 `PermissionError`；
stdin / stdout 正常。

## 3. 形状

```text
模型 ── codemode({"code": "..."}) ──▶ executor（与其他工具同一条路：能力面 → start_call）
                                         │ Operation::Codemode → 不调 Tool::execute，
                                         │ 交给 executor 自己的 run_codemode
                                         ▼
                           sandbox-exec + python3 -I driver.py（子进程）
                              stdin  ◀── 第一行：{code, tools, catalog}；之后：工具调用的结果
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

## 4. 沙箱

### macOS

`sandbox-exec -p <profile> <python> -I -c <driver>`，Seatbelt 配置按解释器现生成：

```scheme
(version 1)
(deny default)
(allow process-exec (literal "<sys.executable>") (literal "<它的真实路径>") <roots>)
(allow process-fork)
(allow file-map-executable)
(allow file-read-metadata)
(allow file-read* <roots>
                  (subpath "/System") (subpath "/usr/lib") (subpath "/private/var/db/dyld")
                  (literal "/dev/null") (literal "/dev/urandom") (literal "/"))
(allow sysctl-read)
(allow file-write-data (literal "/dev/null"))
```

- `<roots>` 是 `sys.prefix`、`sys.base_prefix` 与它们 `canonicalize` 之后的真实路径（去重）各一条
  `subpath`。Seatbelt 按解析后的真实路径匹配：Homebrew 的框架版 Python 报的 prefix 走
  `/opt/homebrew/opt/…` 这条符号链接，dyld 却从 `Cellar/…` 加载 `Python.framework`，只放前者
  就起不来（2026-10-14 实测 3.14：`Library not loaded … file system sandbox blocked open()`）。
- **没有** `network*`、没有 `file-write*`（`/dev/null` 除外）、`process-exec` 只放解释器与它的安装目录
  （框架版 Python 启动时会再 exec 一次安装目录里的 `Python.app`；起来的仍在同一个沙箱里）。
- 解释器的路径在 Gateway 启动时探一次（`sys.executable`、它的真实路径、`sys.prefix`、
  `sys.base_prefix`）。托管 venv（§5.1）的 `site-packages` 在 `sys.prefix` 下，所以**装好的库
  可以只读导入**；没建 venv 时退回 `PATH` 里版本最高的 `python3.N`（3.10 起；一个都没有才是裸
  `python3`——macOS 上那常是 3.9），只有标准库。
- 脚本不落盘：driver 用 `-c` 传入，脚本正文走 stdin。
- 环境变量清空，只给 `LANG`；凭证一个都不给。

### 自检，失败就不注册

启动时用同一份配置跑一段自检脚本：读、写一个沙箱外的探针文件（这边先写好），连一个 socket，起一个
子进程，**四样都必须被拒**、探针文件不能被改、stdin/stdout 必须通。任何一条不成立（没有
`sandbox-exec` / Landlock、配置被系统拒绝、某样没拦住），
`codemode` 就不进工具目录，日志写明原因。**不退化成没有沙箱的 Python。**

### Linux

不起外部程序（不用 bubblewrap），解释器直接起，fork 之后、exec 之前在 `pre_exec` 里把自己关进去：
`PR_SET_NO_NEW_PRIVS` → `landlock_restrict_self` → `seccomp`。规则集与 BPF 过滤器在父进程里一次建好
（要分配、开文件），子进程里只做这三个系统调用。走裸系统调用（`libc`，原本就在依赖树里），不引
`landlock` / `seccompiler`。

**Landlock**（`codemode/landlock.rs`）——把当前 ABI 认得的文件操作全部纳入管辖，然后只放：

| 路径 | 权限 |
|---|---|
| 解释器的真实路径、它的 ELF 加载器（读 `PT_INTERP`，如 `/usr/lib64/ld-linux-x86-64.so.2`） | 执行 + 读 |
| `sys.prefix`、`sys.base_prefix`、`/lib`、`/lib64`、`/usr/lib`、`/usr/lib64`、`/etc/ld.so.cache`、`/dev/urandom` | 读 |
| `/dev/null` | 读写 |

- 执行权只给那两个文件：`subprocess` / `os.execv` 起别的程序一律 `EACCES`。这一条 Codex 与 Grok
  Build 都没有做（它们只管文件与网络）；Landlock 的执行权正好补上，且共享库是 `mmap` 进来的，
  不需要执行权。
- ABI ≥ 4 时 TCP 的 `bind` / `connect` 也纳入管辖、一条不放；ABI ≥ 6 时信号与抽象 Unix socket
  不出沙箱（给父进程发信号 `EPERM`）。
- 规则集按 ABI 版本取，但**内核没有 Landlock 就不注册 codemode**（不像 Codex 旧后端那样尽力而为）。

**seccomp**——先核架构（不是本机架构就杀进程；x86_64 上 x32 的调用号一律拒），然后以下 `EPERM`：
`socket`、`socketpair`、`connect`、`bind`、`listen`、`accept(4)`、`sendto`、`sendmsg`、`sendmmsg`、
`io_uring_*`、`ptrace`、`process_vm_readv/writev`、`bpf`、`perf_event_open`。

- Landlock 管不到 UDP 与 Unix socket（Codex 正是因此改用 bubblewrap 的网络命名空间）。这里
  `socket()` 直接拒，新 socket 一个都开不出来；`connect` 一类再拒一遍，防继承下来的 socket fd。
  io_uring 能绕开 `socket()`，一并拒。
- 不用 bubblewrap：要装外部二进制，Ubuntu 24.04 默认还禁了非特权用户命名空间；而 codemode 的
  子进程只需"读解释器、走 stdin/stdout"，Landlock + seccomp 足够，且什么都不用装。

2026-10-10 在 Fedora 44（内核 7.2，Landlock ABI ≥ 6）实测：自检四项全拒；另读 `~/.komo/.env`、
列 `~/.ssh` 与 `~`、读 `/etc/passwd`、写 `/tmp`、`mkdir`、UDP `sendto`、连 D-Bus 的 Unix socket、
`os.kill(父进程, 0)`、fork 后 `execv /usr/bin/ls` 全部被拒；`import ssl`、读 `/usr`、写 `/dev/null`
正常；系统 `python3` 与 `python3 -m venv` 建的 venv 都能用。

参考：Codex `codex-rs/linux-sandbox`（bubblewrap + seccomp，旧后端是 Landlock）、Grok Build
`xai-grok-sandbox`（进程级 Landlock + 子进程 `pre_exec` 里手写的 seccomp 网络过滤器）。

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
| `text(value)` | 加一段输出；字符串原样，其他值转 JSON |
| `print(...)` | 进输出末尾的 `<console>` 段 |
| `TOOLS` | 这次脚本能调的工具名列表 |
| `ALL_TOOLS` | 能调的工具 `[{name, description}]`，含没声明给模型的（MCP 默认如此，`docs/mcp.md` §3） |
| `search_tools(query, limit=8, namespace=None)` | 按相关度排的 `[{name, description}]`（BM25，照 pi） |
| `describe_tool(name)` | `{name, description, parameters, namespace?}`，没有就是 `None` |
| `describe_namespace(name)` | 一台 MCP 服务器的 `{name, description?, instructions?, tools}`；认 `mcp__dev_radius`、`mcp__dev-radius`、`dev-radius`、`dev_radius` |

发现函数照 pi 的 `searchTools` / `describeTool` / `describeNamespace`：宿主把这次能调的工具说明、
schema 与所属服务器（含 instructions）随第一行交进沙箱（`catalog`），查找与排序都在沙箱里做完，不经
宿主、不进 journal，也不进指纹（调用本身已按工具名与参数核对）。分词与停用词同 pi；pi 只认 a-z0-9，
这里另把中日韩字切成单字与相邻两字，中文说明才搜得到。名字一律是脚本里的写法（`-` 换成 `_`）。

结果正文：`text()` 各段按顺序，`<console>` 段随后，失败时附 `脚本出错：` 与 traceback，最后一行
是调用摘要（`调用了 3 次工具：read×2、rg×1`）。超长照常由投影截头尾。

## 7. 计划、journal 与恢复

父计划是 `Operation::Codemode { code }`。`replay = Safe` / `SafeReread` 表示可以重新进入宿主的
journal 协议，**不表示整段工具操作可以重做**；`is_read_only()` 为 false，父调用是调度屏障。

首次启动先提交 `start`（协议版本、代码/driver 哈希、工具表、cwd、环境版本）。子调用按宿主收到的
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

恢复逐次核对工具名、完整 JSON 参数与顺序；结束时检查全部记录是否被消费。代码、driver、能力表、
cwd、环境版本改变，参数或顺序改变，提前结束，或者引用损坏，均明确返回 uncertain。普通脚本异常
仍是 Failed；`started` 保存子尝试身份，恢复 unsafe 或时限/取消中断时读取该尝试留下的部分输出，
将有上限的 stdout/stderr 尾部及目录存入父 uncertain 正文，仍由父结果统一投影。
时限/取消打断正在执行的子调用时，持久化的父结果保留 uncertain（用户取消仍停止 Run）。
升级前没有 durable `start` 的中断父调用也返回 uncertain，不能按旧的“整段只读”假设重跑。

### Python 的恢复边界

这不是 pi-durable 的确定性 JS VM：Python 的时间、随机数、线程、导入库等不被冻结，纯计算与输出
也不保证字节确定。实现因此只允许恢复**已记录的调用前缀**，不允许恢复期间发出任何新子调用。
恢复可重新取得已完成的写结果并生成汇总；需要继续未记录的工作时由模型另发调用。
即使新调用仍是只读，超出旧前缀也会明确失败，避免动态脚本在未提交窗口制造另一个写 key。

工具成功但 `delivered` 尚未提交时，不能猜测未引用的 output.json 代表可交付成功；unsafe 返回
uncertain，Safe 才能按双 Safe 准则重做。`start` 已提交但第一个 intent 未提交时，恢复禁止新增
调用并返回 uncertain。已提交 successful write 子结果始终复用，永不执行第二次。

## 8. 限制

- 时限：`[execution] call_timeout_secs`，到点杀进程组；完整输出落盘，模型视图最多 50 KiB / 2000 行；内层调用最多 256 次。
- 脚本之间不共享状态（pi 的 `store/load` 不做）。
- 不跑模型（pi 的 `models` 不做）。

## 9. 实现落点

| 位置 | 内容 |
|---|---|
| `komo-kernel` `types/plan.rs` | `Operation::Codemode` |
| `komo-runtime` `codemode/`（`mod.rs` + `driver.py`） | 解释器探测、自检、子进程协议 |
| `komo-runtime` `codemode/seatbelt.rs` / `landlock.rs` / `unsupported.rs` | 按平台把子进程关进去：Seatbelt 配置 / Landlock + seccomp / 没有 |
| `komo-runtime` `tools/codemode.rs` | `CodemodeTool`：定义 + `prepare`；`execute` 不该被走到（与 `dispatch` 同一模式） |
| `komo-runtime` `executor/codemode.rs` | 沙箱执行、能力与 prepare 检查、子输出发布、宿主 fatal 锁 |
| `komo-runtime` `executor/codemode_journal.rs` | journal 阶段、稳定身份、分页读取与回放校验 |
| `komo-gateway` `service/mod.rs` | 启动时探测 + 自检，通过才注册工具并接上执行器 |

## 10. 验收

- 沙箱自检：读写沙箱外的文件 / 联网 / 起子进程被拒（macOS 与开了 Landlock 的 Linux 上跑，其他跳过）。
- 端到端（真 Gateway）：脚本调 `read` 两次、过滤后 `text()`，模型只看到过滤结果；脚本里调
  `write` 抛错、文件没被写；脚本死循环到时限被杀。（原来的"脚本里读工作区外的文件（Ask）抛错"与"`codemode` 在 strict 下 Allow"随
  Policy 于 2026-10-11 删除。）

- 持久化与恢复单测：完成的 unsafe 子调用不重复、intent 无 started 可执行、stored/current 双 Safe 的
  四种组合、参数/工具/顺序分歧、少调用/多调用、未提交交付窗口、uncertain 被捕获后仍不能续写、
  journal 分页、当前能力面/操作变化，以及真实 Python 的前缀回放与超时中断。
- 真 Gateway + stdio MCP：脚本调用 bump 写计数文件、内层结果不生成模型消息；在 delivered 提交前/
  后故障停机，丢弃父 orphan 输出强制 journal 冷恢复，计数保持一次；分别得到 uncertain / completed。

2026-10-13 在 macOS 当前工作树验证：

- `cargo fmt --package komo-kernel --package komo-runtime --package komo-gateway`、
  `cargo fmt --check`、`git diff --check` 均通过。
- `ulimit -n 8192; cargo test --workspace --no-fail-fast -- --test-threads=4`：1856 passed、
  0 failed、8 ignored；runtime 633、Gateway lib 247、codemode 集成 4、MCP 集成 6、recovery 集成 28 均通过。
  默认并发首轮曾触发 `Too many open files`，故本轮限制测试并发并提高该进程的软上限。
- `cargo build --timings`：exit 0，增量构建 13.25s；报告位于 `target/cargo-timings/`。
  未做冷构建计时，未在 Linux 上验证本次变更。

独立审查修复后的相关验证（macOS，以上全 workspace 数字为修复前记录）：

- 新增 3 项回归：stdout/stderr 写失败后 Python 捕获仍不能续写（同时覆盖工具转为 Failed 与吞错）、
  delivered uncertain 已提交后取消仍持久化 Uncertain 且停止回合、Reply 格式与 execution 引用校验。
- `cargo test -p komo-runtime codemode -- --test-threads=4`：25 passed、0 failed、0 ignored。
- `cargo test -p komo-runtime -- --test-threads=4`：636 passed、0 failed、2 ignored；doc-tests 0 passed、0 failed。
- `cargo fmt --package komo-runtime`、`cargo fmt --check`、`git diff --check` 均通过。

提交前（macOS，含上面的修复、注释整理与 `docs/komo_bot.md` 同步）：`cargo fmt --check`、`git diff --check`
通过；`ulimit -n 8192; cargo test --workspace --no-fail-fast`（默认并发）：1857 passed、0 failed、8 ignored。
