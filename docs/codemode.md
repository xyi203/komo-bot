# codemode：djs 里的 TypeScript / JavaScript 脚本

> 状态：现行规格。2026-11-24 起脚本引擎由 pydantic Monty（Python 子集）切换为 **djs**（纯 `no_std` Rust 的
> TypeScript / JavaScript 引擎；此前的 Monty 与更早的 CPython + OS 沙箱实现见 git 历史）。脚本在 komo 自己起的
> worker 子进程里跑，工具调用由宿主执行；允许 `read` / `rg`、`write` / `edit`、`bash`、有副作用 MCP 与只读 `komo`；
> 引擎自身**没有任何直接文件 / 网络 / 环境变量 / 子进程访问**，一切外部读写都经 `tools.*`。
>
> **2026-10-25 起跑在新存储上**（`docs/storage-durable.md` §4.7、§13.2）：外层是一个 `komo.tool` 任务，
> 脚本里每个子调用是它名下一个 `kind: "nested"` 的工具子任务，交付结果记在 task 级 document `komo.nested`
> 的槽位里，时钟 / 随机数 / 流式并行交付批次与脚本指纹是外层任务的 **memo**；恢复时整段重跑、已记录的原样回放、
> 越过记录之后照常执行（§7）。双 Safe、unsafe 不重做、分歧即 uncertain 的规则不变。设计权威为 `docs/komo_bot.md`。
>
> **2026-10-26 已实施**：脚本里可调用 `bash`，嵌套调用有 `komo.live.nestedTools` 槽位并共用 `probe` 恢复；
> bash 交给脚本的是有上限的尾部视图。方案与验收见 `docs/codemode-bash.md`。
>
> **2026-11-24 已实施：djs 引擎 + 预编译快照 + 流式并行**。所有 `tools.<name>(args)` 均返回原生 `Promise`，
> 支持顶层 `await` / `return`、TypeScript 类型标注、`Promise.all` / `Promise.allSettled` / `Promise.race`
> 与多跳流式并发流水线；`prelude.js` 预编译成进程级字节快照复原；worker 协议升到 5。

## 1. 要解决什么

模型一次只能发几个工具调用，每个结果都整段进上下文。查十个文件、并发调一批 MCP 工具再做多跳关联、过滤大 JSON、
把三处结果拼成一张表，要么多轮串行来回，要么把一堆用不上的正文塞进上下文。

codemode 让模型写**一段 TypeScript / JavaScript 脚本**：脚本里并发调工具、在本地过滤和汇总，只有脚本的输出回到模型。

## 2. 为什么是 djs

脚本能不能**放心地自动跑**、中断后能不能**整段重跑**，取决于脚本本身能不能碰外部世界与运行时是否完全确定。

| 方案 | 隔离与能力 | 结论 |
|---|---|---|
| 裸 Python（2026-10-15 已删） | 无：脚本就是任意代码 | 内层调用再只读也没用，脚本自己就能改东西、联网，也不能整段重跑 |
| CPython + 操作系统沙箱（2026-10-15 前） | 进程级（Seatbelt / Landlock + seccomp） | 平台差异大、启动自检易失败、无法提供确定性 VM |
| pydantic Monty（2026-10-15 ～ 2026-11-24） | 语言层 Python 子集 + `monty-fs` 只读挂载 | 子集缺口多（无用户异常子类、`__getattr__` 缺失）、拉入 90+ 依赖与重复版本、并行 `wait` 为全量批次屏障、仍需维护只读挂载与不可读名单 |
| **djs**（`djs-core` + `djs-prelude`） | **纯 `no_std` Rust 实现的 JS/TS 引擎**：零第三方依赖、无文件/网络/env/子进程、原生 TS + `async/await` + `Promise`、硬内存上限、可序列化快照、确定性 PRNG 与时钟注入 | **采用** |

djs 的关键收益：

- **语言与并发一等公民**：直接以 `Goal::AsyncBody` + `typescript: true` 执行用户脚本（用户代码行号 1:1，不拼前缀行），
  内置 `JSON` / `RegExp` / `Date` / `Map` / `Set` / `BigInt` / `Intl` 之外的完整 ES 标准库与原生 `Promise.all` /
  `Promise.allSettled` / `Promise.race`，以及真正的 `class ToolError extends Error` 与 `Proxy` 动态工具分派。
- **真正零直接 I/O**：djs 是纯 `no_std` VM，彻底移除了 `monty-fs` 与 `codemode_unreadable_paths`。
  读文件走 `await tools.read(...)` / `await tools.rg(...)`，写文件走 `await tools.write(...)` / `await tools.edit(...)`，
  需要第三方库或系统命令走 `await tools.bash(...)`。这也意味着脚本不再有“未记账的裸磁盘读取”——一切外部输入都经宿主记账。
- **预编译快照启动**：`prelude.js` 在每个 worker 进程里只执行一次并序列化为字节快照（`OnceLock`），每次脚本执行通过
  `Vm::from_bytes_trusted` 微秒级复原再绑定当次 `tools` 与 `catalog`。
- **强资源隔离**：内置 `max_heap_bytes` + `fatal_out_of_memory = true`（OOM 不可被 `try/catch` 吞掉），配合燃料片
  （`200_000`）+ 墙钟截止时间 + 独立 worker 进程与 RSS 看门狗。
- **依赖极简**：`djs-core` / `djs-prelude` / `djs-libm` / `djs-lzrc` 零第三方 crate 依赖；替换 Monty 后 `Cargo.lock`
  净减 94 个包，`cargo tree -d` 重复根由 82 降至 64（消除 `jiff 0.2`、`io-lifetimes`、`phf` 等重复版本）。

## 3. 形状

```text
模型 ── codemode({"code": "..."}) ──▶ executor（与其他工具同一条路：能力面 → start_call）
                                         │ Operation::Codemode → 不调 Tool::execute，
                                         │ 交给 harness 自己的 codemode 执行器
                                         ▼
                           komo __codemode-worker（每次调用一个子进程，内跑 djs）
                              stdout ──▶ 第一行握手 {"hello": {protocol, engine, komo}}；宿主核对，对不上就不发脚本
                              stdin  ◀── 第一行：{code, tools, catalog, cwd, limits}；之后：答复
                              stdout ──▶ {"call": name, "args": {...}, "id", "defer": true}
                                         / {"wait": [id..]}
                                         / {"os": 时钟或熵} / {"done": {...}}
                                         │
                       每个 call ──▶ harness 嵌套子任务（见 §5）──▶ 就绪结果流式写回 stdin
                                         ▼
                           输出（text() / return 与 console）落进这次调用的 stdout，回到模型
```

- **外层是一次普通调用**：一个 `komo.tool` 任务，意图 checkpoint 与结果 entry 照常（`docs/komo_bot.md` §8.5），输出照常投影（§8.3）。
- **内层调用是外层名下的嵌套工具任务**：输入 `{kind: "nested", parent, key: "c<n>", callId: "<外层 callId>/c<n>", name, arguments}`，
  `Ownership::Task(外层)`，与 task 级 document `komo.nested` 里 key 为 `c<n>` 的槽位（`{taskId, name, arguments, result?}`）
  **在同一次提交里建出**，同时在 `komo.live.nestedTools` 加一条带 `parentTaskId` 的 pending 槽位；嵌套任务终结的那笔提交把交给脚本的结果写进槽位的 `result`。结果不进 transcript，
  外层终结时槽位随它退役；父结果的 `result.calls` 只保留有上限的摘要。嵌套任务自己的输出照常落在它的尝试目录。
- codemode 可能包含远端写操作，整体是调度屏障，不与同一轮其他只读调用并行；脚本**里面**的
  `tools.*` 调用彼此流式并行（§5「流式并行」）。

## 4. worker 与限制

### 语言没有的，就是脚本碰不到的

- 没有文件系统 API（`fs`、`open`）、没有网络（`fetch`、`WebSocket`）、没有 `process` / 环境变量、没有子进程、没有 `setTimeout` / `setInterval`、没有 `import()`。
- **冻结内建对象**（`frozen_builtins = true`）：脚本无法篡改 `Object.prototype`、`Array.prototype`、`Promise.prototype` 等内建原型，也无法覆盖宿主注入的全局函数（`non-configurable` / `non-writable`）。
- **确定性运行时**：
  - `WeakRefs::Strong`：不暴露 GC 时机差异。
  - 启动时向宿主请求 8 字节熵（`Nondet::Urandom { size: 8 }`）通过 `vm.set_math_random_seed(seed)` 固定 `Math.random()` 序列。
  - 启动时与每次 `wait` 返回后向宿主请求一次时钟（`Nondet::Clock`）更新 `vm.set_now(ms)`，脚本内 `Date.now()` / `new Date()` 只看宿主注入的时间，且所有读数记进外层任务的 memo（§7）。
- worker 进程环境变量全清，不给 `LANG` 也不给任何凭证；stdin / stdout 只走协议，脚本的 `console.*` 与 `text()`
  进 worker 内存里的缓冲，碰不到协议用的 fd。

### worker 进程

宿主为每次 codemode 调用起一个 worker：`<komo 可执行文件> __codemode-worker`（clap 隐藏子命令，不进
`--help`；函数体在 `komo-tools` 的 `codemode::worker`，bin 只分发）。可执行文件是 Gateway 启动时
`current_exe()` 解析一次的路径。每次独立起子进程，换来每段脚本独立的内存与崩溃边界。

版本握手：`komo update` / `scripts/install-local.sh` 原子换掉磁盘上的二进制之后、Gateway 重启之前，这台 Gateway
起的 worker 是新版本（路径没变）。所以 worker 的**第一行输出**是握手 `{"hello": {protocol, engine, komo}}`，
宿主比协议号（`WORKER_PROTOCOL = 5`）与引擎构建 ID（`engine`，如 `djs-<8hex>`）：`komo` 版本号不同照常跑，
只记一条 warn 提示重启 Gateway——所以凡是会改 worker 行为的变化都要升 `WORKER_PROTOCOL`。
协议或引擎构建 ID 对不上就在发脚本之前干净失败（`ScriptOutcome.incompatible`）：工具结果是 Failed，
消息说明两边的版本并让 operator 重启 Gateway（`komo gateway restart`）；脚本一行没跑，重启之后同一个调用能照常跑。
只有恢复中（前几次尝试已经做过子调用）碰上它才记 uncertain。

worker 在专用的 8 MiB 栈线程里跑 djs（不经 tokio），并用 `catch_unwind` 兜底引擎异常。

协议：stdin/stdout 每行一个 JSON（`WORKER_PROTOCOL = 5`）。

```text
worker → 宿主   第 1 行 {"hello": {"protocol", "engine", "komo"}}
宿主 → worker   第 1 行 {"code", "tools", "catalog", "cwd", "limits"}；之后每行 {"ok": v} / {"error": "…"}
                或 {"results": [{"id", "ok"} | {"id", "error"}…]}（答 wait，仅含本次就绪子集）
worker → 宿主   {"call": name, "args": {…}, "id", "defer": true}（不等答复，立即继续推进脚本）、
                {"wait": [id…]}（等这些在途调用里至少一个就绪；回放时按录下的批次交付）、
                {"os": {"op": "clock"}} / {"os": {"op": "urandom", "size": n}}（等答复，见 §7）…
                最后 {"done": {"outputs", "console", "error"}}；宿主收到 done 先等未 await 的延后调用落下结果
```

### 宿主管的限制

| 限制 | 由谁管 | 到限的样子 |
|---|---|---|
| 墙钟：`[execution] call_timeout_secs`（含等工具返回的时间） | 宿主 | SIGKILL，`脚本超时（Ns），已终止` |
| 单段计算燃料与墙钟 | djs（每片 `200_000` 燃料 + `max_turn_ms` 检查） | 脚本报错退出（进程内测试 worker 与真实 worker 均生效） |
| 内存：默认 1 GiB（测试可调小） | djs 硬堆上限（`max_heap_bytes` + `fatal_out_of_memory = true`，不可被脚本 `try/catch`）+ 宿主每 100 ms 读 worker RSS 看门狗兜底 | `OutOfMemory` / `脚本内存超限（…），已终止` |
| 取消 | 宿主：丢掉执行 future，`kill_on_drop` SIGKILL worker | 在途子调用沿用 §7 的恢复规则 |
| 子调用次数：256 | worker 数，宿主再核一遍 | 超限的调用立即 reject `ToolError`；宿主侧越界是宿主终止条件 |
| 输出：`console.*` 与 `text()` 各 1 MiB | worker | 脚本里抛错 |
| 调用栈深度 | djs 默认栈深限制 | `RangeError` |

## 5. 脚本里的工具调用

`await tools.<name>({ ... })` → executor 的持久化子调用路径，依次：

1. **前置检查（`CodemodeTool::prepare`）**：
   - 先做语法编译检查（`worker::compile_check`，经 djs `Goal::AsyncBody` + `typescript: true` 解析），语法错直接拒绝、不启 worker；若疑似写了 Python 语法（如 `def ` / `import `），附带迁移到 TS/JS 的提示。
   - 检查禁止在脚本里调用的工具字面引用（`NOT_IN_SCRIPTS`：`codemode`、`task`、`monitor`、`monitor_stop`），在 `prepare` 阶段直接拒绝、一行不跑；动态下标绕法（`tools["task"]`）在运行时也由能力面与宿主双重拦截。
2. **名字与别名**：
   - 名字必须在当前 Run 的 `AgentSurface` 及 executor 工具表里；不回退到全局目录。
   - `tools` 是一个冻结的 `Proxy`：支持 `-` 与 `_` 互通别名（`tools.mcp__dev_radius__search` 与 `tools["mcp__dev-radius__search"]` 指向同一工具）、支持 `in` / `Object.keys` / 解构 / 存为变量。
   - 调用未知工具名时返回一个 reject `ToolError` 的异步函数（不破坏 `Promise.allSettled` 并发容错），并用编辑距离（Levenshtein）给出最接近的候选工具名建议。
   - 参数对象里值为 `undefined` 的键在桥接前自动剥离。
3. **计划与准入**：
   - `tool.prepare(args)` 验证参数和约束（版本绑定、komo 状态的保护名单照旧由工具自己做）。允许 `ReadFile`（`read`、`rg`）、`WriteFile`（`write`、`edit`）、`ShellCommand`（`bash`）、`McpCall`（含写操作）与 `komo` 工具的只读动作（`Operation::Komo` 且计划可安全重做）；komo 的写动作、编排、`then_run` 仍在脚本外执行。
   - 并行的两个 `edit` 改同一个文件时，后执行的那个按自己 `prepare` 时绑定的版本检查，得到“版本冲突”的 `ToolError`，不会悄悄覆盖前一个的改动。
4. **持久化与落盘**：
   - 嵌套任务走和顶层调用同一条 `call → execute → 结果`：意图 checkpoint（完整 `ExecutionPlan` 与 `plan.recovery.replay().and(tool.replay())`）提交成功之后才允许执行工具。
   - 流式 stdout / stderr 保存在嵌套任务自己的尝试目录；工具返回后发布完整原始输出，再在一次提交里终结嵌套任务并把脚本将接收的精确对象或错误写进 `komo.nested` 的槽位（`commit_nested`），之后才回送 JS。
   - `komo.live.nestedTools` 节流提交输出尾巴与子进程身份；终结清掉运行进度，外层收尾（含 fault / orphan）删掉它名下的行。`detach_after` 为 None，结果不进 transcript。

返回给脚本的对象保持统一结构：

```ts
{ status: "completed" | "failed", text: string, result: unknown }
```

bash 非零退出正常返回 `status: "failed"` 与 `result.exit_code`，脚本可检查后继续。
它的 `text` 用 kernel full 投影，放得下（30 KiB / 2000 行以内）给全文，放不下给开头 2 KiB 预览加裁剪说明，并附 `artifact://…/result`；
原始 stdout / stderr 留在 blob，脚本可以再调 `await tools.read(...)`。

普通执行/参数/不支持操作的错误 reject 为真正的 `ToolError`（`err instanceof ToolError && err instanceof Error`），可被 `try/catch` 捕获；这些结果也持久化。
**uncertain、`komo.nested` 读写/校验失败、恢复分歧是宿主终止条件**：宿主锁住错误，后续任何子调用都不执行，
父结果记为 uncertain 并返回模型。脚本捕获异常或伪造结束不能解除该状态。

### 流式并行

所有 `tools.<name>(args)` 均为异步函数：调用时 worker 立刻发 `{"call", "args", "id", "defer": true}` 给宿主并返回原生 `Promise`，脚本可继续同步发出下一批调用或挂接 `.then(...)` 形成多跳流水线。

当 djs 微任务队列清空、挂起等待在途 `Promise` 时，worker 发 `{"wait": [当前在途 id…]}`：

- **实时运行（流式交付）**：宿主等待这些 id 中**至少一个**完成（`>= 1`），再顺手用 `now_or_never` 捎上同一拍已就绪的其余 id，立即返回这批就绪子集；未完成的 id 留在宿主的在途集合里继续跑，worker 也将其留在 `outstanding` 里。因此快链路在慢调用尚未返回时就能立刻进入第二跳，不再被全量批次屏障卡住（支持 `Promise.race`、流式扇出/多跳流水线）。
- **确定性记录与回放**：宿主把每次 `wait` 的等待集与实际交付批次记作 `Nondet::Deliver { waiting }`（落进 `codemode.nondet.<n>` memo）。崩溃恢复重放时，宿主按 `DeliverOracle::next_recorded` 严格等待并按原顺序交付录下的同一批 id，保证 `Promise` 回调、后续 `c<n>` 发出顺序与首次运行完全一致（§7）。
- **序号与准入**：子调用序号 `c<n>` 在 worker 发来调用的那一刻同步分配，准入（核对记录、刷写此前攒下的 `nondet` memo、建嵌套任务与 `komo.nested` 槽位）按序号连续推进，执行并发。`result.calls` 与图片按序号排，不按完成先后。

| 写法 | 语义 |
|---|---|
| `const r = await tools.read({ path })` | 单个调用 |
| `const [a, b] = await Promise.all([tools.read({ path: p1 }), tools.rg({ pattern })])` | 并发发出、按参数顺序收齐；任一项 reject 整体 reject，其余已发出调用照常跑完落账 |
| `const rs = await Promise.allSettled([tools.read({ path: p1 }), tools.bash({ command })])` | 标准 `Promise.allSettled`：`{ status: "fulfilled", value }` 或 `{ status: "rejected", reason }` |
| `const fast = async () => { const a = await tools.x(); return tools.y({ id: a.text }); }; await Promise.all([fast(), tools.slow()])` | 流式多跳：`fast()` 在 `slow()` 还没返回时就能发出 `tools.y` |

没被 `await` 的调用也已发出，宿主在脚本 `done` 后等它们落下结果再收尾。256 次上限覆盖所有调用；超限的调用立刻在 worker 侧本地 reject `ToolError`（不发给宿主）。

## 6. 模型看到的接口

> 工具发现（`ALL_TOOLS` / `searchTools` / `describeTool` / `describeNamespace`）、子调用持久化与中断重跑语义、256 次调用上限、输出落盘与结果预算放在 `tool://codemode/doc`，工具 description 保持精简（见 `docs/komo_bot.md` §4「工具定义瘦身」）。

工具 `codemode`，参数 `{"code": "<TypeScript 或 JavaScript 源码>"}`。脚本以 `async` 函数体执行（`Goal::AsyncBody` + `typescript: true`），顶层可直接 `await` 与 `return`，行号与用户传入的 `code` 逐行对齐。

| 名字 | 作用 |
|---|---|
| `await tools.<name>(args)` | 调工具，见 §5。返回 `{ status, text, result }`，支持 `-` / `_` 别名 |
| `text(value)` | 追加一段输出；字符串原样，其他值转格式化 JSON（缩进 2、支持 `Map`/`Set`/`BigInt`/`Error`） |
| `return value` | 未调用 `text()` 时，非 `undefined` 的顶层返回值自动作为输出（与 `text()` 同格式） |
| `ToolError` | 真正的 `Error` 子类（`err instanceof ToolError`），工具调用失败或找不到工具名时抛出 |
| `console.log / warn / error / info / debug` | 进输出末尾的 `<console>` 段 |
| `TOOLS` | 这次脚本能调的工具名只读列表 |
| `ALL_TOOLS` | 常量数组：能调的工具 `[{ name, description }]`，含没声明给模型的 MCP 工具（`docs/mcp.md` §3） |
| `searchTools(query, limit = 8, namespace?)` | 按相关度排的 `[{ name, description }]`（BM25F 字段加权 + 弱动词同义扩展 + CJK 分词）；保留 `search_tools` 别名 |
| `describeTool(name)` | `{ name, description, parameters, namespace? }`，没有返回 `null`；保留 `describe_tool` 别名 |
| `describeNamespace(name)` | 一台 MCP 服务器的 `{ name, description?, instructions?, tools }`；认 `mcp__dev_radius` / `mcp__dev-radius` / `dev-radius` / `dev_radius`；保留 `describe_namespace` 别名 |

发现函数在 `prelude.js` 里本地做完（BM25F 字段加权 + 弱动词同义扩展 + CJK 分词），不经宿主、不记账，也不进指纹。

结果正文：正常时 `text()` 各段按顺序，`<console>` 段随后，最后是调用摘要（`调用了 3 次工具：read×2、rg×1`）。
子调用失败但脚本处理后正常结束时，失败计数放到正文首部，外层状态仍为 completed。
脚本异常时先给异常首行（`codemode_text::failure_prefix`）、调用摘要，再接脚本输出、console 与完整 JS 堆栈（行号即用户脚本行号）。
失败首部最多列 8 个子调用的 `c1..c256`、工具名、状态与原始输出的 `artifact://…/result` 入口。

## 7. 计划、嵌套任务与恢复

父计划是 `Operation::Codemode { code }`。`CodemodeTool::replay` 是 Safe（`SafeReread`），表示恢复时可以按 `docs/komo_bot.md` §8.6 的表**重跑整段脚本**（嵌套任务与 memo 把已发生的事原样接回），**不表示整段工具操作可以重做**；`is_read_only()` 为 false，父调用是调度屏障。

外层 `komo.tool` 任务执行 `Operation::Codemode` 时，先写 memo `codemode.start`（指纹版本 4：脚本哈希、引擎构建 ID `djs-<8hex>`、worker 协议版本 5、前置代码哈希、能力表、cwd；对不上就是回放分歧），再起 worker。子调用按宿主收到的顺序编号 `c1..c256`：第 `n` 个调用 = 一个 `kind: "nested"` 的工具任务（`callId` 为 `<外层 callId>/c<n>`，输入里带工具名与完整参数）加 `komo.nested` 里 key 为 `c<n>` 的槽位。

重启后外层任务从 checkpoint 回来，**起新 worker 从头重跑脚本**：第 `n` 个调用在 `komo.nested` 里找 `c<n>`，工具名与完整参数都对得上就等它的任务（不另建），对不上是分歧。任务停在哪由引擎按 §8.4 的表恢复：

| 嵌套任务重启时停在 | 处理 |
|---|---|
| `call`（意图未提交） | 确定未执行；当前能力面与 prepare 再核对后，用重新准备的计划执行 |
| `execute`，盘上有完整 `output.json` | 身份与哈希验证后复用；bash 的有限视图可从落盘事实重建。其他工具仍要求预览就是全部 stdout（或结果自带 `text`），否则继续下面的判断 |
| `execute`，槽位记录了本次尝试的子进程 | 进入 `probe`；Alive / Unknown 时继续等待，Gone 后回到 execute 的表 |
| `probe` | 从 checkpoint 的子进程身份与截止时间继续探；取消写 aborted，并在子结果里说明仍存活的 pid |
| `execute`，没有完整输出或存活子进程 | 已存与当前 `Tool::replay` 都为 Safe 才用原计划新尝试重跑；否则 uncertain，绝不重做 |
| 已终结，槽位里有 `result` | 校验引用与完整工具输出，返回已存精确结果；包括 unsafe 成功、普通失败及 uncertain |

### 脚本的恢复边界：完全确定性重跑

恢复时起一个新 worker **从头重跑脚本**。在 djs 下脚本没有直接文件读取，所有不确定源全部经宿主记账与回放：

1. **工具调用**（`komo.nested` 的交付结果）：逐次核对工具名与完整 JSON 参数，一致则原样回放，对不上即分歧（uncertain）。
2. **熵与时钟**（`codemode.nondet.<n>`）：启动时的 8 字节 PRNG 种子（`Nondet::Urandom { size: 8 }`）与每次时间刷新（`Nondet::Clock`）均由宿主回答并按批记成 memo，恢复时原样回放；请求种类或字节数对不上同样是分歧。
3. **流式并行交付批次**（`Nondet::Deliver { waiting }`）：每次 `wait` 实时运行的交付 id 列表作为 `nondet` 条目落账；恢复时宿主通过 `StateDeliver`（`DeliverOracle`）按录下的批次与顺序交付，确保并发多跳流水线的回调顺序与后续子调用编号逐次一致。

**memo 的增长**：宿主把 `nondet` 条目攒在内存里，**在下一个新子调用建任务之前**（以及收尾前）刷成一条 `codemode.nondet.<n>` memo（先写者赢），数量 ≤ 子调用数 + 1。单次脚本合计最多 10 000 条 `nondet` 记录（含时钟、熵与交付批次），超限则在脚本里抛错。

子调用的图片随嵌套结果存引用（`{mime, path, hash}`），外层收尾时按引用读回、写进外层自己的输出目录；回放的子调用同样带图。

## 8. 限制

- 时限：`[execution] call_timeout_secs`，包含等待工具与恢复后等待 probe 的时间，到点 SIGKILL worker；嵌套 bash 不转后台，长命令可在脚本外调。完整输出落盘，模型视图最多 30 KiB / 2000 行；内层调用最多 256 次。
- 内存、输出、燃料与栈深限制见 §4 的表。
- 脚本之间不共享状态（pi 的 `store/load` 不做）。
- 不跑模型（pi 的 `models` 不做）。
- djs 自身无直接文件 / 网络 / env / 子进程访问；文件读写经 `tools.read` / `rg` / `write` / `edit`，联网与外部库走 `tools.bash`。
- `nondet` 记录合计最多 10 000 条、单次 `urandom` 最多 64 KiB（§7）。

## 9. 实现落点

| 位置 | 内容 |
|---|---|
| `komo-kernel` `types/plan.rs` | `Operation::Codemode` |
| `komo-tools` `codemode/mod.rs` | `Worker`：起 worker 进程、协议对话（`WORKER_PROTOCOL = 5`）、流式 `wait` 与 `DeliverOracle`、墙钟 / 内存看门狗 / 杀进程；`ScriptCatalog`、`Nondet` |
| `komo-tools` `codemode/engine.rs` | 脚本引擎接缝：`ScriptEngine`（编译检查、运行）与 `EngineHost`（延后工具调用、流式等待、时钟 / 熵、`text()`） |
| `komo-tools` `codemode/worker.rs` | worker 本体（`komo __codemode-worker` 的函数体，8 MiB 栈线程）：握手、行协议、`EngineHost` 实现（调用次数上限、流式 `wait`、本地超限 reject、`text()` 总量上限） |
| `komo-tools` `codemode/djs.rs` | `DjsEngine`：进程级预编译 `prelude.js` 快照（`OnceLock` + `Vm::from_bytes_trusted`）、`Goal::AsyncBody` + `typescript: true`、硬 OOM、燃料片 + 截止时间、`JsonSink`、`build_id()` |
| `komo-tools` `codemode/prelude.js` | IIFE 封装的前置运行时：`ToolError`、`text()`、`console.*`、BM25+CJK 发现函数、`__initRuntime`（冻结 `Proxy` `tools`、`-`/`_` 别名、编辑距离建议） |
| `komo-tools` `limits.rs` | `resident_bytes`：worker 的常驻内存读数 |
| `komo-tools` `tools/codemode.rs` | `CodemodeTool`：定义 + `prepare`（语法编译检查 + `NOT_IN_SCRIPTS` 拦截）+ `CODEMODE_DOC` |
| `komo-tools` `executor/mod.rs` | `ToolExecutor::codemode_fingerprint`（版本 4，含 `engine_version()`） |
| `komo-runtime` `durable/harness/codemode.rs` | 外层脚本任务：`codemode.start` memo、worker 对话、`StateDeliver`（`DeliverOracle`）、时钟 / 熵 / 交付批次 memo、按 key 回放 `komo.nested`、`abort_owned` 收尾 |
| `komo-runtime` `durable/harness/codemode_text.rs` | 失败首部渲染（JS 首行错误提取）、子调用摘要与已执行调用清单 |
| `komo-runtime` `durable/harness/nested.rs` | `komo.nested` 槽位与 `commit_nested`、`nested::reusable` |
| `komo-runtime` `durable/harness/tool.rs` | `kind: "nested"` 分支：共用 `call → execute → probe → 结果` 与进度提交，槽位在 `komo.live.nestedTools` |
| `komo-gateway` `service/mod.rs` | 启动时解析 `current_exe()`，注册工具并把 `Worker` 交给执行器 |
| `komo` `src/main.rs` | 隐藏子命令 `__codemode-worker`，只分发到 `codemode::worker::run` |

## 10. 验收

**2026-11-24：切换 djs 引擎 + 预编译快照 + 流式并行（macOS）。**

- **语言、沙箱与协议（进程内 worker，`komo-tools::codemode::tests`，26/26 通过，0.13s）**：
  TypeScript 类型标注与顶层 `return`、`-`/`_` 工具名别名与 `undefined` 参数剥离、真正的 `ToolError instanceof Error`、
  未知工具名编辑距离建议、精确用户行号堆栈与部分输出保留、零文件/网络/env 与冻结内建原型、`Date.now()` / `Math.random()` 确定性注入、
  `text()` 格式化（`Map`/`Set`/`BigInt`/UTF-8）、BM25+CJK 发现函数（`searchTools`/`describeTool`/`describeNamespace` 及 snake_case 别名）、
  `Promise.all` / `Promise.allSettled` 并发、**流式多跳并发**（快链第二跳在慢调用阻塞期间立即发出并完成）、256 次调用上限、
  燃料+截止时间终止死循环、硬 `max_heap_bytes` OOM 不可被 `try/catch` 捕获、`prepare` 编译检查拒绝 Python 语法并附迁移提示、
  版本握手（`protocol` / `engine` 不匹配即拒、`komo` 不同照常跑）。
- **持久化、嵌套任务与崩溃恢复（`komo-runtime::durable::harness::tests::codemode`，31/31 通过，0.58s）**：
  嵌套 `c<n>` 槽位回放、双 Safe 策略、分歧锁死 uncertain、时钟/熵 memo 回放、**流式多跳流水线重启后按录下的 `Nondet::Deliver` 精确顺序回放**、
  嵌套 bash 有限视图与 probe 恢复、取消/超时收尾。
- **真 worker 进程（`crates/komo/tests/codemode_worker.rs` 9/9、`codemode_nested.rs` 4/4）**：
  真子进程读文件与过滤、捕获 `ToolError`、TypeScript 与日期格式化、发现函数、无 fs/net/env、硬堆上限终止内存增长、
  死循环墙钟 SIGKILL、假版本 worker 握手拒绝、`__codemode-worker` 不进 `--help`、嵌套 bash 非零退出、`Promise.allSettled` 并发互等、强杀重启回放。
- **真 Gateway 端到端（`komo-gateway` `tests/codemode` 7/7、`tests/mcp` 19/19）**：
  `read` 过滤、并行 `write`/`edit` 与版本冲突、bash 预览转 `read` 完整 artifact、`NOT_IN_SCRIPTS` 字面与下标拦截、
  超时收尾、MCP `codemode` 隐藏 schema + 脚本发现与副作用持久化。
- **全量工作区与构建账**：
  - `ulimit -n 8192 && cargo test --workspace --no-fail-fast`：**2205 passed、0 failed、11 ignored**；`cargo test -p komo-kernel --features ts` **398 passed**；`cargo check --examples --tests` 通过；`cargo fmt --check`、`git diff --check` 通过。
  - 依赖与编译：`Cargo.lock` **567 → 473 个包（−94）**，`cargo tree -d` 重复根 **82 → 64（−18）**；增量 `cargo build --timings` **29.92s**（`komo-tools` 3.03s），详见 `docs/komo_bot.md` §13.4。

> 历史验收记录（2026-10-15 Monty 迁移、2026-10-20 失败反馈、2026-10-26 嵌套 bash 与并行子调用，以及更早的 CPython + OS 沙箱）见 git 历史。
