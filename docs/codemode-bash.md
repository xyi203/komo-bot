# codemode 脚本里调 bash：嵌套调用的 live 槽位

> 状态：**已实施**（2026-10-26），验收记录见 §7，TUI 画子调用见 §8，§1 保留实施前的现状。来源：pi-durable 的嵌套调用（本机 `../pi` 提交 `d5b3de22d`：
> `packages/durable/docs/spec.md` §7.3「nested calls」、`src/harness/tool.ts` 的 `pi.live.nestedTools`、
> `src/codemode/codemode.ts` 对 bash 非零退出的处理）。文中 §x 不带文件名时指 `codemode.md`。

## 0. 结论

让 codemode 脚本像 pi 一样能调 `bash`（`tools.bash(command=...)`）。放开本身只改两处判断；真正要补的是
**嵌套调用没有 `komo.live` 槽位**这处对 pi-durable 的偏离（`docs/storage-durable.md` §13.2）：顶层 bash 起的
子进程身份记在槽位里，重启后由 `probe` 阶段等它结束再判；嵌套任务没有槽位，脚本里的 bash 一重启就只能
记 `uncertain`，哪怕那个进程还在跑。

做法照 pi-durable 的结构：`komo.live` 加一张 `nestedTools`，嵌套任务和顶层一样写意图、节流写输出尾巴与
子进程身份，`probe` 与顶层共用一条路。另外把嵌套 bash 交给脚本的正文改成留尾的有限视图，不把整份 stdout
存进 `komo.nested`。

不在这次范围：脚本里的 `edit` / `write`（另议）、嵌套调用的 `detach_after`、TUI 画嵌套调用（§3.5；后来在 §8 补上）。

## 1. 现状

脚本里的 `tools.bash(...)` 不进 Monty，暂停到宿主，宿主把它当一个 `kind: "nested"` 的工具任务执行（§5）。
所以 Monty"没有子进程"并不挡它，挡它的是三处显式判断：

| 位置 | 做的事 |
|---|---|
| `tools/codemode.rs` `NOT_IN_SCRIPTS` | `bash` 不进 `TOOLS`；脚本正文字面写着 `tools.bash(` 的，`prepare` 就拒、整段不运行 |
| `harness/tool.rs` 嵌套分支（`NOT_IN_SCRIPTS.contains`） | 绕过字面检查的（`getattr`）在这次子调用上按失败收尾 |
| `harness/tool.rs` 嵌套分支（`Operation` 检查） | 只放行 `ReadFile` / `McpCall`，且不带 `then_run` |

当初禁掉，是把 codemode 定成"组合，不是新能力"（`docs/komo_bot.md` §4），本地写入与命令留在脚本外。
这条边界现在已经不成立：脚本里能调有副作用的 MCP（2026-10-13 起），顶层 bash 本来就不受限（§7 删掉审批），
禁掉它只是让模型多一轮来回（§5 记的"十二次整段作废"就是模型把 `tools.bash("git status")` 写进脚本）。

`docs/storage-durable.md` §13.2 记过偏离"嵌套调用没有 `komo.live` 槽位"，理由是"脚本里放行的只有读文件与
MCP，不起 `run_child_detachable` 的子进程，所以也用不上 `probe`"。放开 bash 后这条理由没了。

## 2. pi-durable 的做法

- 脚本里能调任何工具，bash 也一样。非零退出不抛异常，脚本拿到 `{ output, exitCode }`（`codemode.ts:276`）。
- 嵌套调用建任务的同一次提交里，在 `pi.live.nestedTools` 推一个槽位 `{callId, parentCallId, parentTaskId,
  name, taskId, arguments, status}`（`harness/tool.ts:782`）；运行中的输出与 `pi.live.tools` 走同一条节流
  提交（`:485`）；终结时写 `summary`（`isError`、`durationMs`、`usage`、有上限的错误文字）；外层收尾时连同
  槽位一起退役。
- 交给脚本的结果不存 `output`（只给模型读的那份），"an image or a `bash` tail" 只存一次（spec §7.3）。
- 重启后不探子进程：没声明 `replay` 的工具按 `unsafe`，在 `execute` 的就是 `interrupted`。**`probe` 是
  komo 自己加的**（旧设计 §8.7"阻止重复启动比重复执行安全"，D4c），pi 没有这一层。

所以 komo 的缺口是两处差异叠起来的：komo 多了 `probe`，又少了嵌套槽位。补的是后者，照 pi 的结构。

## 3. komo 的做法

### 3.1 放开 bash

- `NOT_IN_SCRIPTS` 去掉 `bash`：它进 `TOOLS`、字面检查不再拒它。`edit`、`write`、`codemode`、`task`、
  `monitor`、`monitor_stop` 不变。
- 嵌套分支的 `Operation` 检查加上 `ShellCommand`，仍然要求 `then_run` 为空（组合调用的第二步是一条命令，
  脚本里直接写两次调用就是）。
- 能力面不变：这次运行的工具集里没有 `bash`，脚本里也调不到（`ToolSet::get`）。
- 脚本拿到的值沿用现有接口 `{"status", "text", "result"}`。bash 非零退出时 `ToolOutput` 是 `Ok` 而
  `status = failed`，所以脚本拿到 `status: "failed"` 与 `result.exit_code`，**不抛 `ToolError`**，与 pi 一致；
  起不来进程、超时之类 `ToolError` 才抛。

### 3.2 `komo.live.nestedTools`

`LiveState` 加一个字段，形状与 `tools` 的 `ToolSlot` 对齐，多两项归属：

```json
"nestedTools": [
  { "callId": "call_3/c2", "parentTaskId": 9, "taskId": 14, "name": "bash",
    "status": "running", "replay": "unsafe", "attempt": 1,
    "pid": 4242, "pgid": 4242, "startedAt": 1761206400000,
    "output": "…输出尾巴…", "droppedBytes": 0 },
  { "callId": "call_3/c1", "parentTaskId": 9, "taskId": 12, "name": "rg", "status": "done" }
]
```

不放参数：参数在嵌套任务的输入与 `komo.nested` 成员里，bash 命令可能很长，进度提交每次都整值重写
`komo.live`，不该每次带着它。

| 时机 | 提交 | 槽位 |
|---|---|---|
| 脚本发出新子调用 | `harness/codemode.rs` 建嵌套任务 + `komo.nested` 成员的那一次 | 推一个 `pending` |
| 意图 | `tool.rs` 的意图提交（今天嵌套只写 checkpoint） | `running`，写 `replay` / `attempt`，清进度 |
| 运行中 | `commit_output`，经 `commit_progress`（`synchronous=OFF`） | 输出尾巴、`droppedBytes`、子进程身份 |
| 安全重跑 | `resolve` 的新尝试提交 | `attempt + 1`，清进度 |
| 嵌套任务终结 | `commit_nested`（终结 + 交付结果同一次） | `done` / `failed`，清掉尾巴与子进程身份 |
| 外层终结 | 外层 `commit_result` | 删掉 `parentTaskId` 等于外层的所有行 |

大小：脚本里的子调用是串行的（Monty 每次暂停等一个答复，没有并发入口），所以同一个外层任务同一时刻最多
一行在跑、带尾巴；已结束的行只有几个字段，最多 256 行（子调用上限），随外层终结一起删。外层永远晚于名下
嵌套任务终结（`abort_owned` 之后才收尾），删的时候不会有还在写的行。

代码上让 `LiveState::slot` / `slot_mut` 按任务 ID 在 `tools` 与 `nestedTools` 两张表里找，`tool.rs` 现在
用 `slotted = call.nested.is_none()` 跳过槽位的几处（意图、重跑、`commit_output`）就不用再分支；嵌套任务
照样起 `commit_output`，同时保留 `captured`（交给脚本的正文从它算，§3.4）。

### 3.3 probe 共用

`recorded_child` 按任务 ID 读槽位（§3.2 的统一查找），嵌套任务在 `execute` 恢复时：

| 嵌套任务重启时停在 | 今天 | 之后 |
|---|---|---|
| `execute`，盘上有完整 `output.json` 且 `nested::reusable` | 复用 | 不变 |
| `execute`，槽位记着这次尝试的子进程 | （没有槽位，不会走到） | 进 `probe`，与顶层同一条：探到结束再回到 `resolve` |
| `execute`，没有子进程记录 | 双 Safe 重跑，否则 `uncertain` | 不变 |

外层这边不用改：重跑脚本时第 `n` 个调用按 key 找回同一个嵌套任务、等它终结，它在 `probe` 里就一直等。
取消照现有规则：外层 `abort_owned` → 嵌套任务的 abort handler 写 `aborted`，不杀那个进程，正文里写明
"重启前起的子进程（pid N）还在运行"；脚本一侧视为结果不明，整段 `uncertain`。

### 3.4 交给脚本的 bash 正文

今天嵌套结果的 `text` 是 `captured`，也就是整份 stdout，原样存进 `komo.nested`。`read` 一个文件尚可，
`cargo test` 的几 MB 日志不该进 `durable.db` 的 document。改成：

- bash 的 `text` 取 `projection.rs` 的 full 视图（留尾，默认 50 KiB / 2000 行），超限时带 `output`
  里已有的 `artifact://…/result` 引用，脚本要全文可以 `tools.read` 它；
- 这份视图由已发布的 `output.json` 正文、持久化耗时与 stdout / stderr 引用决定；
  `nested::reusable` 对 bash 按"完整输出通过身份 / 哈希验证、有限视图可重建"复用，不再要求预览等于全部 stdout。
  脚本已收到的精确视图存进 `komo.nested`，之后回放直接用那份字节；不会额外在内存收集完整 bash stdout。

其他工具的 `text` 不变（`read` 仍是整份文件）。

### 3.5 不改的，写进限制

- **时限**：外层 `[execution] call_timeout_secs` 的墙钟包含等子调用的时间。脚本里跑长命令会连同整段脚本
  一起超时；长命令仍在脚本外调。§8 写明。
- **转后台**：嵌套调用的 `detach_after` 仍是 `None`。转后台的结果要交回一个已经结束的工具调用，脚本此时
  还在等这次答复，没有接手方。
- **并发**：codemode 本来就是同一轮里的屏障（`docs/komo_bot.md` §6），不变。
- **TUI**：槽位让嵌套调用的进度有了数据，但 SSE feed（`service/durable/feed.rs`）当时只把 `komo.live`
  里的部分回复投成 `AssistantDelta`，顶层工具的输出尾巴也没有投，TUI 也还不画 codemode 的子调用
  （`docs/komo_bot.md` §11，工具调用怎么画那一段）。子调用的展示已在 §8 补上；把工具的输出尾巴（顶层
  与嵌套）接到 SSE 仍是另一件事。

## 4. 改动落点

| 位置 | 内容 |
|---|---|
| `komo-runtime` `tools/codemode.rs` | `NOT_IN_SCRIPTS` 去掉 `bash`；工具说明与 `CODEMODE_DOC` 里"bash 只能在脚本外调"改成只列 `edit` / `write` / 编排；`refused_calls` 的测试改例子 |
| `komo-runtime` `durable/harness/live.rs` | `NestedToolSlot` 或给 `ToolSlot` 加 `parent_task_id`；`LiveState.nested_tools`；`slot` / `slot_mut` 两表查找；删外层名下行的函数 |
| `komo-runtime` `durable/harness/codemode.rs` | 建嵌套任务的提交里推 `pending` 槽位 |
| `komo-runtime` `durable/harness/tool.rs` | 嵌套放行 `ShellCommand`；意图、重跑、`commit_output` 去掉 `slotted` 分支；外层收尾删行 |
| `komo-runtime` `durable/harness/nested.rs` | `commit_nested` 改槽位状态；bash 的 `text` 取留尾视图；`reusable` 的 bash 条件 |
| `docs/codemode.md` | 文首状态、§4 文件段、§5 第 1–3 条、§7 恢复表加 `probe` 一行、§8 限制、§9 落点 |
| `docs/komo_bot.md` | §4 codemode 一段与"搜索走 `rg`"一段里"脚本里只放行只读与 MCP" |
| `docs/storage-durable.md` | §4.7 嵌套一段、§13.2 那条偏离改成"已按 pi 补上" |

`komo.live` 只加可选字段（`#[serde(default)]`），旧 document 照常解码，`LIVE_VERSION` 不动。

## 5. 测试

- `harness/tests/codemode.rs`：脚本调 `bash` 拿到 `status` / `text` / `exit_code`，非零退出不抛；输出超限时
  `text` 留尾并带 `artifact://` 引用，`tools.read` 能读全文；`komo.nested` 成员不含整份 stdout。
- `harness/tests/codemode.rs`（probe 场景）：嵌套 bash 报告进程身份后"崩溃"，重启时嵌套任务进 `probe`、外层重跑脚本等它；
  进程结束后 unsafe 记 `uncertain`、有完整 `output.json` 则复用；取消时写 `aborted` 且正文点名 pid。
- 槽位生命周期：建任务 `pending` → 意图 `running` → 进度带 pid → 终结清尾巴 → 外层终结后 `nestedTools`
  里没有它的行；旧 `komo.live`（没有 `nestedTools`）解码通过。
- `crates/komo/tests/codemode_nested.rs`（真 worker）：脚本调真实 bash，stdout / stderr 与退出码 7 正常往返，脚本继续。
- `komo-gateway/tests/codemode/durable.rs`：真实 bash 的有限尾部含 artifact 入口，脚本里继续 `tools.read` 读回完整 stdout / stderr。
- `tools/codemode.rs`：字面 `tools.bash(` 不再被拒，`tools.edit(` 仍拒。

## 6. 未决

- 脚本里的 `edit` / `write` 是否一起放开。它们不起子进程，没有本文的缺口，但改动的语义（版本绑定、
  `then_run`）要单独看。
- 等待 `probe` 的嵌套任务期间外层墙钟照走：重启后若进程跑得比 `call_timeout_secs` 还久，外层会超时、
  整段 `uncertain`。目前接受；真遇到再考虑恢复时给外层续时。

## 7. 实施记录（2026-10-26）

- 采用 `NestedToolSlot { parent_task_id, #[serde(flatten)] slot: ToolSlot }`，序列化为
  `komo.live.nestedTools`；字段可选，`LIVE_VERSION` 保持 1，旧 document 解码通过。
- pending 槽位、任务与 `komo.nested` 成员同笔创建。顶层与嵌套共用意图、节流进度、probe、安全重跑与清理；
  完成的子槽位清掉尾巴与进程身份，外层收尾（含 fault / orphan）移除名下行。
- bash 结果使用同一处 kernel 投影，输入来自已发布的正文、流引用与持久化耗时；
  不再额外收集完整 stdout。超限时保留尾部与 artifact 入口，其他工具的完整正文接口保留。
- 补齐嵌套宿主 `Timeout` / `Cancelled` 的持久化状态：fresh 执行与完整输出重建都保持 uncertain，
  脚本捕获异常也不能执行后续调用。bash 的非零退出仍是 failed 字典，可以检查后继续。

新增 12 条测试覆盖旧格式、槽位查找与生命周期、约 300 KB stdout 的 blob / 有限视图分离、
Alive / Unknown / Gone 的 probe、取消结果里的 pid、安全重跑、完整输出在交付前崩溃后的精确复用、
宿主超时、真 worker 的非零退出往返，以及真 Gateway 内继续 `read` 完整 artifact。
工作区测试总数与 `cargo build --timings` 记录在 `docs/storage-durable.md` §13.5。

本次没有扩展 `write` / `edit`、嵌套转后台、脚本并发或 SSE / TUI 子调用展示；对应限制仍见 §3.5、§6。
子调用展示随后单独做了，见 §8。

## 8. TUI 画子调用（2026-10-26）

照 pi 的 codemode 扩展（本机 `../pi` 提交 `d5b3de22d`）：
`packages/coding-agent/src/extensions/codemode/renderer.ts`（子调用列在脚本那一块里，每个一行：状态、
工具名、参数、耗时；折叠最多 8 个，展开印原因）、`tool.ts` 的 `CodemodeNestedCall`、`execute.ts`（每次子调用
开始 / 结束都 `onUpdate` 一份整份快照）、`core/nested-tool-calls.ts`（参数与错误的限额），以及
`modes/interactive/interactive-mode.ts` 里带 `parentToolCallId` 的工具事件不单独成块。komo 的做法：

- **一个视图**：kernel `protocol/nested.rs` 的 `NestedCallView {id, tool, args, state, elapsed_ms?, error?, output?}`。
  参数用 `bounded_args` 收窄（字符串 200 字符、数组 / 对象 16 项、3 层），状态用 `state_of`（结果不明优先，
  再看宿主 `ok` 与工具 status），错误留 500 字节，最多 256 个（与子调用上限一致）。
- **跑完的**：runtime 的 `NestedCall` 多存一份收窄后的 `arguments` 与 `uncertain`，进外层 `output.json` 的
  `result.calls`。gateway 投影 codemode 的 `komo.tool-result` 时先读这份 `output.json`（哈希核对，
  `service/durable/nested_view.rs`），经 `legacy_events_with` / `Projection::attach_result` 收窄后放进事件
  `body.result = {calls, error?}`。补读（JSON 分页与 SSE 重连）和直播走同一条路，所以两边逐帧相同。
  只在事件上，不进模型 transcript：模型看到的仍是脚本输出加一句计数。
- **还在跑的**：新增只在 SSE 上的 `SseEvent::NestedCalls {run, call, calls}`，每帧是一个外层调用的整份快照。
  来源是 `komo.live.nestedTools`（`parentTaskId` 对上 `tools` 槽位得到外层 call）加各子调用的 `komo.nested`
  成员（有结果的用结果）。`komo.live` 一变就重算，只推内容变了的外层；`id:` 与 `AssistantDelta` 一样取当前
  `last_seq`。SSE 补读结束（含 lag 后当场补读）再追加一帧当前快照，重连也看得到在跑的子调用。
- **TUI**：`ToolLine.nested` 由 `NestedCalls` 整份替换（外层已终结就不再接受），由 `tool.result` 的
  `calls` 定稿。`tool_view::nested_lines` 把每个子调用当成一次调用，用顶层那套按工具拆开的头一行
  （`read 路径:起-止`、`bash $ 命令`……）画在脚本与结果之间、缩进一格，失败与结果不明的接一行原因。
  折叠时每个只占一行、最多 8 个：没跑完、失败、结果不明的先留，其余留最近的，并写明“另有 N 个子调用没列出
  （Ctrl-T 展开）”；`Ctrl-T` 全列，原因整段给，并补上完整结果的 `artifact://` 入口。

限制：

- gateway 拿不到 `sessions_dir`（还没接上配置）或外层 `output.json` 读不到 / 哈希对不上时，结果事件不带
  `calls`：补读的话不画已完成的子调用，直播的话留着最后一份快照（可能还是“在跑”的记号）。
  `cron_watch` 的补读仍不带目录，那里的 codemode 结果同样不带 `calls`。
- 在跑的快照只在 SSE 上，不落库；JSON 分页拿不到它。
- 子调用的精确结果随外层终结从 `komo.nested` 退役，之后只剩 `result.calls` 的摘要与各自的
  `artifact://` 入口。
- 旧记录没有 `arguments`：从截断过的 `args` 文字解，解不开就当一段文字画。
- 没有 pi 的 `cost` 字段；komo 的子调用不走模型，没有费用可记。

测试：kernel `protocol::nested` 3 条、`sse` 帧往返 1 条、`legacy_events` 补读与直播同帧 1 条；runtime
`the_outer_result_lists_each_sub_call_with_bounded_args_and_its_failure`；gateway `nested_view` 3 条；client
`tool_view` 2 条（头一行与原因、折叠 / 展开）、`app` 1 条（快照替换、结果定稿、晚到快照不倒回）。
