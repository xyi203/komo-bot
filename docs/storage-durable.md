# 存储重设计：整套采用 pi-durable 的提交模型

> 状态：**实现中（2026-10-25）**。D0–D3、D4a、D4b 已完成并合入 `main`；D4c 的工具任务 `probe` 阶段与 `komo.live.tools[].pid`、生成 / 工具任务在 Turso 上的接线测试、**压缩**（`komo.compaction` 阻塞与后台、衰减写成 `edits`、上下文超长时压缩重试）已合入集成分支 `d4c`，codemode 嵌套调用与 `task` / `monitor` 还在做；进度与实现中的偏离见 §13。新旧代码并存，新存储还没接到 gateway。全部完成后改写 `docs/komo_bot.md` §8，本文留作推导记录。
>
> 对照基线：本机 `../pi` 提交 `8efd314fa` 的 `packages/durable`（`docs/spec.md` 为规范，`README.md`）。
> pi-durable 自称 **Experimental**，API 随版本变；pi coding-agent 在这个提交上**并不依赖它**（仍是
> `core/session-manager` 的树）。所以这里借的是**规范里的契约**，按固定提交移植成 Rust，不追它的 API。

## 0. 结论

**能整套借，但借的是"一条提交线 + 不可变 entry + 持久任务状态机 + 收件箱"这四件，不是它的库。**
落到 komo 上的形状是：

- **state.db 成为唯一权威**，对话正文（entry）、Run / 调用状态（task / submission）、可变状态
  （document）、cron / 投递在**同一个数据库事务**里提交。今天"内容归 JSONL、调度归 state.db"的两权威
  分工、§8.5 的跨文件写入顺序、§8.9 处理"半个事实"的 reconcile、`control_outbox`、`session_log_index`、
  `checkpoints`、`payloads/`、`derive.rs` 全部消失。
- **大正文仍在 Session 目录**（`tool-output/`、图片、`artifacts/`），但只是"副作用产物"：先写文件并同步，
  再提交引用它的 entry——这就是 pi-durable 的 effect sandwich，不是第二个权威。
- **events.jsonl 退成导出格式**（pi v3 树形 JSONL，`komo session export`），不再是存储。
- **会话树改用 pi-durable 的 fork 表达**：一个分支 = 一个从父 entry 处 fork 出来的 conversation，komo 的
  Session 是一组 conversation 加一个"当前分支"指针。昨天落地的 §8.11 用户行为全部保留，底层换掉。
- 不搬：Chord（document 只存整值，不存 delta）、extension registry / hooks、JSONL 后端、
  `synchronous=NORMAL`、Cloudflare 后端。

代价：komo-store 的账本核心和 komo-runtime 的 agent loop / executor / recovery 要重写（§9 分阶段），
以及**放弃"内容是可以 `grep` 的纯文本文件"**这个性质（§7）。

## 1. 为什么值得

今天的复杂度几乎都来自"两个权威之间没有事务"：

| 现在要处理的事 | 根源 | 新模型下 |
|---|---|---|
| §8.5 的写入顺序（正文 → JSONL `sync_all` → db 事务）与反向的 outbox 审计补写 | 两个文件、两个持久化边界 | 一次提交，没有顺序问题 |
| §8.4 里 `accepted`（"预留了 ID、正文还没写完"）、"JSONL 有结果但 db 落后"、"`run.absorbed` 已同步但 db 没提交" 等行 | 半个事实 | 不可能出现：受理与正文同一事务 |
| §8.9 reconcile 对"目录与数据库对不上"的处置 | 同上 | 只剩"blob 文件被人删了"一种 |
| `session_log_index`、`checkpoints`、applied_seq、尾部增量读、末尾半行隔离截断 | JSONL 是权威，db 是它的索引 | 没有 JSONL 权威，也就没有索引 |
| `derive.rs`：实时提交与回放两条路径派生同一组可重建表 | 可重建表是 JSONL 的投影 | 没有投影，表就是事实 |
| `with_write_retry`、MVCC 冲突、`Contended` 重试原因 | 多个写入者 | 一条提交线，没有并发写 |
| JSONL 与 db 各 fsync 一次 | 两个边界 | 每步一次 fsync |

pi-durable 的核心规则正好是这件事的解：

> A Session atomically commits immutable entries, full task records, and Chord-tracked documents.
> Only committed state is observable.（spec 开头）

komo 已经借过它的重放判据（§8.6"已存 / 当前 replay 双 Safe"），那条规则在 pi-durable 里是
`pi.tool` 任务 `execute` 恢复阶段的一行（spec §8.4）；整套借过来，§8.6 就不再是一条孤立规则，而是工具任务的
定义本身。

## 2. 搬什么、改什么、不搬什么

| pi-durable 的东西 | 处理 | 说明 |
|---|---|---|
| 一条 mutation line，一次 commit 原子覆盖所有写入（spec §1 不变量 1–8，§4） | **照搬** | 整个 Gateway 一条线（§3.1） |
| 不可变 entry，`model` / `data` / `head` / `edits` / `byTaskId`（§2） | **照搬** | 取代 JSONL 事件与 pi v3 entry |
| 上下文推导：最新 head 起、edits 后者赢、结果挪到调用后、缺结果补错误、去掉 aborted/error（§2.1 规则 1–10） | **照搬** | 取代 `context::tree` 的路径选择与 `repair_pending` |
| fork：`parent.at`，子沿父链读（§2.1、§3.7） | **照搬** | 会话树的底层（§4.2） |
| Task 状态机：pending/running/waiting/completing/terminal，整值 checkpoint，phase handler，step 规则 1–6（§5.1） | **照搬** | 取代 Run 状态机 + `RunQueue::claim` 的领取代次 |
| effect sandwich：先提交意图、做副作用、再提交结果（§5.2） | **照搬** | 已经是 komo §8.5 的精神，现在变成一种结构 |
| 所有权与结构化并发：task 拥有 task / conversation，abort 自底向上，`completing`（§5.4、§5.5） | **照搬** | 取代 `task` 工具的父子账（§4.6） |
| Submission + `pi.inbox` + boundary（postTools / final，steer / followUp）（§6） | **照搬** | 取代 `waiting + dependency`、`run.absorbed`、请求键（§4.3） |
| `pi.live`：run 控制、节流提交的生成 / 工具进度（§8.2） | **照搬，进度节流** | "可见即已持久"（§3.6） |
| 内置任务 `pi.generation` / `pi.tool` / `pi.compaction`（§8） | **照搬结构**，阶段内容用 komo 的 | provider driver、投影、压缩算账仍是 komo 的 |
| 嵌套工具调用（`kind: "nested"`，NestedCallDoc）（§7.3、§7.6） | **照搬** | 取代 `codemode.journal`（§4.7） |
| Storage trait + conformance 套件（§10） | **照搬接口形状** | Turso 实现 + 内存实现；内存实现取代 `MemLedger` |
| 数字 ID，全局单调 `mintId`（§2、§10） | **照搬**，对外 ID 另存 | §3.4 |
| Chord document（base + delta、Astra 操作、watch） | **不搬** | document 只存整值，每次替换；komo 的 document 都小（§3.3） |
| Extension / registry / hooks / sections / wraps（§7） | **不搬** | komo 有自己的 Profile、能力面、Tool trait；system 段照 `pi.system` entry 落 |
| JSONL 后端（main + sidecar + 提交标记） | **不搬** | 它整段历史常驻内存，README 自己说长跑用 SQLite |
| SQLite `synchronous = NORMAL` | **不搬** | komo 保持 Turso 默认 `FULL`（§8.2 已核实） |
| fork 拿到新的 provider session 身份 | **按 komo 改** | `task context: "fork"` 要复用父的提示缓存（`docs/background-tasks.md` §2b） |

## 3. 新存储模型

### 3.1 一条提交线

- `komo-store` 里一个 `Session`（为免和 komo 的 Session 混淆，代码里叫 `Line`）：一个 tokio 任务持有
  唯一的写连接，按到达顺序串行执行提交闭包。闭包内只能读表与 document、追加写入，**不许 `await` 模型、
  子进程、文件同步或用户**（pi-durable §4，也是 komo 现有规则）。
- 一次提交 = 一个 `db.transaction()`；提交成功后才在内存里采纳并发布（SSE、等待者）。
- **存储错误让整条线失败**（spec §10.1）：Gateway 停止受理、关掉，由 launchd/systemd 拉起后从已提交状态
  恢复。这比今天"磁盘写失败进 `waiting + intervention`"更粗，但正确：提交了没有是未知的，重开是唯一干净的
  判断方式；客户端重发带请求键，命中已受理的那条。
- 读不经过线：Turso 读连接直接读已提交状态。
- 写全部经过线，包括 cron、投递、Memory。**大批量写（记忆向量重建）切成多次小提交**，不长时间占线。
- 吞吐（D0 实测，§10）：一次 fsync 提交约 13 ms，单线上限约 **70 次/秒**，MVCC 与 WAL 一样。线是串行的，
  这 13 ms 别的 conversation 也得排队，所以**只写进度的提交不 fsync**（§3.6），结构性提交（entry、任务状态、
  submission）照常 `synchronous=FULL`。一次典型 Run 的结构性提交约 60 次、占线约 0.8 s。

### 3.2 记录

沿用 pi-durable 的四张记录表 + document 表，外加 komo 自己的表，**全部在同一个 state.db、同一次提交里**：

| 表 | 内容 | 来源 |
|---|---|---|
| `conversations` | id、`parent {conversation, at}`、`owner {conversation, task}`、所属 komo Session | pi-durable |
| `entries` | id、conversation、kind、`model`（消息 JSON）、`data`、`head`、`edits`、`by_task` | pi-durable |
| `tasks` | id、conversation、kind、version、input、owner、background、abort 标记、state（含 checkpoint）、memos、时刻 | pi-durable |
| `submissions` | id、conversation、type、status、entry / answer、reason、`request_id` | pi-durable |
| `documents` | id、kind、scope（session 级 / conversation 级 / task 级）、key、整值、版本、生命周期 | pi-durable，去掉 delta |
| `sessions` | 对外 UUIDv7、标题、来源、cwd、生命周期（§8.10）、**当前分支**（conversation id）、根 conversation | komo |
| `cron_jobs` / `cron_firings` | 同今天；firing 与它创建的 submission **同一提交** | komo |
| `deliveries` | 同今天；run 收尾时与答复 entry **同一提交**生成 | komo |
| `memory_*` | 同今天；证据引用改为 `entry id`（旧证据见 §8） | komo |

退役（只加不删的规则照旧：老表留着不读，§8.2）：`runs`、`tool_calls`、`tool_attempts`、
`session_log_index`、`checkpoints`、`control_outbox`。

### 3.3 Document

只存整值 JSON，每次提交替换；不做 delta、不做历史回看。komo 需要的几个：

| kind | scope | 内容 | 对应今天的 |
|---|---|---|---|
| `komo.agent` | conversation，fork 按 `asOf` | Profile 名、模型、effort、cwd、能力面引用 | `runs` 上的配置快照、RunSnapshot |
| `komo.live` | conversation | `run {task, inputs}`、生成进度（节流的部分回复）、工具槽位与输出尾巴、重试到点、压缩进行中 | `runs.state`、内存里的 SSE 流 |
| `komo.inbox` | conversation，fork 为 `initial` | 排队的 submission（steer / followUp / write） | `waiting + dependency` 的 Run |
| `komo.usage` | conversation | 累计用量与费用 | `runs` 的用量列 |
| `komo.provider` | conversation | provider session 身份（缓存亲和） | 无 |
| `komo.memory_cursor` | conversation | Memory 处理到哪个 entry | `runs` 的 Memory 游标 |
| `komo.nested` | task | 嵌套调用的槽位与交付结果 | `codemode.journal` |

`komo.live` 的部分回复每次节流都整值替换；单条回复通常几 KB，可以接受。真的太大再考虑存 delta。

### 3.4 ID

- entry / task / submission / conversation / document 用**全局单调数字 ID**（`mintId`，线内计数器，
  开库时取各表最大值）。ID 顺序就是创建顺序——这正是今天 UUIDv7 同毫秒乱序、只好另加 `seq` 来定次序的那个坑
  （§8.4"判谁在前用输入事件的 seq"）。
- 对外稳定的 ID 保持 UUIDv7 字符串：komo Session（`komo resume SESSION_ID`）、投递、cron。
- AGENTS.md 的"never AUTOINCREMENT"不受影响：计数器在线内分配，不是数据库自增；只有一条写入者，没有
  MVCC 热点冲突。

### 3.5 外部正文（blob）

工具输出、stdout / stderr、图片、产物仍在 `sessions/{session}/` 下，路径改按任务 ID：
`tool-output/{tool_task}/{attempt}/output.json|stdout.txt|stderr.txt|image-<n>.<ext>`。规则：

1. 运行时流式写 `.partial`（同今天，§8.3）。
2. 收尾：同步文件 → 改名 → 同步目录 → **再**提交引用它的结果 entry。blob 先于引用，所以"引用指向不存在的
   文件"只可能来自人为删除。
3. blob 写完后不再改；`artifact://` 引用映射到它们（旧 URI 见 §8）。
4. base64 不进任何记录（§8.3 的图片规则不变）：entry 的 `model` 里图片块存 `{mime, path, hash}`，组请求时
   读回。这是对 pi-durable"消息原样存"的唯一偏离。

### 3.6 可见即已持久

pi-durable 不变量 3："There is no volatile publication path." SSE 只推已提交的东西：entry 追加、document
替换（带提交 seq，作为 SSE 游标）。流式 token 与工具输出走 `komo.live` 的**节流提交**：崩溃最多丢一个节流窗口
（spec §8 开头）。节流间隔可配，默认 250 ms。

**只写进度的提交用 `PRAGMA synchronous = OFF`**（D0 实测每次 0.13 ms，FULL 是 13 ms）：线在这类提交前把同步档位
切到 OFF，结构性提交前切回 FULL。进度提交只改 `komo.live` 里的进度字段，从不追加 entry、不改任务状态。安全性：
日志是只追加的（WAL 的 `-wal`、MVCC 的 `-log`），下一次 FULL 提交的 fsync 会把之前没同步的进度一起落盘，所以
任何掉电后剩下的状态都是一个提交前缀；最多丢掉最后一次结构性提交之后的进度，正是 pi-durable 说的"丢一个节流
窗口"。进程崩溃不丢（写已经交给内核，页缓存还在）——这一条是按机制推的，D0 只用 strace 核对了 OFF 时确实不 fsync，没做崩溃注入，D2 的崩溃测试里补。§11 决定 4 不用退回易失例外。

### 3.7 后端

- **生产：Turso，WAL 模式，新文件。** D0 实测：单写入者下 MVCC 与 WAL 的提交延迟一样；MVCC 下读不在事务里
  会大量报 `database is locked`，在事务里也只能和提交差不多的频率读到（300 次提交期间读成 601 次，WAL 是
  2 万多次）。单线用不上 MVCC 的并发提交，只剩它的读代价，所以用 WAL。新版本用一个
  **新库文件**（不迁移，§8；老 `state.db` 原样留在盘上），不在老库上切日志模式。cron、投递、Memory 表在新库里从空开始。
  MVCC 专属的规则（DDL 必须走非 MVCC 连接、`BEGIN CONCURRENT`、`with_write_retry`）随之退役。
- **测试：内存实现**，进 `komo-kernel` 的 `test-support`，取代 `MemLedger`。两个实现跑同一套 conformance 测试
  （移植 pi-durable 的 `registerStorageConformance` 用例）。
- 不做 JSONL 后端；`komo session export SESSION_ID` 输出 pi v3 树形 JSONL（带 blob 引用），供 pi 打开、备份、
  `grep`。

## 4. komo 概念怎么映射

### 4.1 总表

| komo 今天 | 新模型 |
|---|---|
| Session | `sessions` 行 + 一组 conversation（根 + 各分支 fork）+ 当前分支指针 |
| events.jsonl 的消息事件 / pi v3 message entry | `entries`（`komo.user` / `komo.assistant` / `komo.tool-result` / `komo.system` …） |
| Run | 一条 input submission + `komo.live.run` + 一串 `komo.generation` 任务 |
| Run 状态 `accepted` | **消失**：受理与正文同一提交 |
| `queued` / `running` | 任务 `pending` / `running`；重启后 `running` 一律回 `pending`（spec §5.1 reconciliation） |
| `waiting + retry` | 生成任务的 `retry` 阶段（`until` 在 checkpoint 里，次数随 `attempt` 持久化） |
| `waiting + dependency` | **消失**：后到的输入在 `komo.inbox` 里排队，boundary 按 ID 序放 |
| `waiting + intervention` | 见 §6.2 |
| `absorbed`（插话） | steer 模式的 submission，在 postTools boundary 放进当前 run（spec §6） |
| `completed` / `failed` / `cancelled` / `abandoned` | submission `done` / `unanswered{reason}`；任务 outcome `completed` / `failed` / `aborted` |
| ToolCall + tool_attempts | 一个 `komo.tool` 任务；尝试序号在 checkpoint 里，blob 按尝试分目录 |
| `planned` → `started` → result | `call` 阶段 → 提交 `execute`（意图）→ 结果 entry + terminal |
| `/new`（`conversation.boundary`） | `komo.reset` entry，`head: "self"` |
| 压缩 `context.compacted compacted` | `komo.compaction` entry，`head` = 第一条保留的 entry |
| 衰减 `context.compacted decayed` | 一条只带 `edits`（`replace`）的 write entry，把有资格的结果换成短视图（§4.4） |
| `context_edit` | `edits` 原生支持 |
| `label` / `session_info` | 只用于展示的 write entry（spec §6：不改上下文，忙时也立刻落） |
| `branch_summary` | 新分支里的一条 write entry |
| `memory.injected` | 一条 `komo.memory` entry（带 `model`，进上下文） |
| `codemode.journal` | 嵌套工具任务（§4.7） |
| `task` 工具、`context: "fork"` | 子任务 / 子 conversation（§4.6） |

### 4.2 会话树

pi coding-agent 的树（一个文件里多分支）在 pi-durable 里没有原生对应，对应物是 fork。映射：

- **根分支**是 Session 的根 conversation。
- **在 entry E 之后分叉**（编辑历史输入 = 在那条输入的父 entry 处分叉）= `fork(at = E)` 建一个子 conversation，
  当前分支指针指向它，编辑后的输入作为它的第一条 submission。子沿 `parent.at` 读父的 entry，不复制。
- **`/tree` 视图**：从根 conversation 出发，把各 fork 挂在各自的 `parent.at` 上，拼出 pi 的树。需要
  `conversations(parent_conversation, id)` 索引——pi-durable 只按 owner 建了索引，这是 komo 加的。
- **导航不追加 entry**：只改 `sessions.current_branch`。和 pi 一样，选中 user message 时把原文放回输入框，
  提交时才 fork。
- **`/fork`**（建独立 Session）：新 `sessions` 行，根 conversation fork 自所选位置。**`/clone`** 要求原会话被回收后
  仍可读（§8.11），所以 clone **复制** entry 与 blob，不用 `parent.at`。
- **Run 固定自己的路径**：Run 属于它所在的 conversation，导航换的是"下一条输入去哪个分支"，跑着的 Run 不动。
  §8.11 的"有未结束 Run 时导航 409"技术上可以放宽，**首版保留**（§11 决定 5），以后需要再放开。
- `label` / 命名 / 分支摘要落在对应 conversation 里。

§8.11 列的用户行为（导航、编辑成分支、fork、clone、label、name、`/compact`、分支摘要）全部保留，HTTP 路由与
TUI 不变，变的只有 gateway 下面的实现和 kernel 的树结构。

### 4.3 收件箱与次序

今天的三条规则——"同一 Session 后面的 Run 不越过前面的"、插话、同请求键返回原 Run——全部变成 pi-durable §6
的同一张表：

- 空闲且收件箱为空：输入直接 `placed`，同一提交建 run 与生成任务。
- 忙或收件箱非空：`queued` 进 `komo.inbox`。
- postTools boundary 放 steer（插话），final boundary 放 followUp（下一条输入）。次序就是 submission ID 序，
  不再需要单独的 seq 判据。
- `request_id` 在 conversation 内去重；渠道消息 ID、cron firing、`task-result:{run}` 都是请求键。
- 失败的 run 只把自己的输入记 `unanswered`，收件箱不动；下一条输入到来时一起跑 boundary（spec §6）。

### 4.4 工具结果的模型视图

pi-durable 把结果的模型视图**存进 entry**；komo 今天在回放时从完整事实**现算**（`projection.rs`）。合并方式：

- 收尾时 `projection.rs` 算出 full 视图，写进结果 entry 的 `model`。完整输出仍在 blob。"刚跑完"与"重启后回放"
  逐字节相同，因为存下的就是那份——这条性质由"同一函数算两次"改成了"只算一次"。
- 衰减：`compaction::decide` 的时机与缓存账都不变，决定换的时候提交一条 write entry，`edits` 里每个目标 =
  `replace` 成 decayed 视图。回放按 edits 选视图，不再需要 `RunView.decayed`。
- `[execution] model_result_bytes` 改了只影响之后的结果，旧结果按当时存下的视图——和今天"RunSnapshot 冻结投影
  设置"效果一样。

### 4.5 生成任务

`komo.generation` 的阶段照 pi-durable §8.3（`prepare` / `request` / `retry` / `poll` / `tools`），阶段里做的事是
komo 的：

- `prepare`：装配上下文（`komo-agent::context`，输入换成 entry 范围）、能力面、系统段（变了才写 `komo.system`
  entry）、压缩判断（`compaction::decide`，阻塞阈值时建压缩子任务并 `waiting`）。
- `request`：checkpoint 记 `cutoff`（这次请求包含的最新 entry）、模型、effort。重启回到 `request` = 丢弃未完成的
  流式输出（已节流提交的部分回复转成一条 `aborted` 的 assistant entry），按已提交上下文重发——与 §8.4 第 3 行一致。
- `retry`：§8.5 的退避（2 s 起、60 s 封顶、10 次）放进 checkpoint，模型成功一轮就清零。
- `tools`：按工具轮次建工具子任务并 `waiting`；顺序轮（codemode 是调度屏障）按 `pending` 逐个建。

### 4.6 后台任务（`task` 工具）

- `task` 派出 = 工具任务在同一提交里建一个 **owned conversation**（`context: "fork"` 时 `fork(at = 当前调用的
  assistant entry)`，否则新根）并 submit 第一条输入；工具任务以"已派出"完成，子会话是 **background** 的，
  不挡父的 idle（spec §5.4）。
- 交回结果 = 子 run 收尾的提交里，往父 conversation submit 一条带 `request_id = task-result:{run}` 的输入。
  原子、幂等，不需要今天的单独派送路径。
- fork 不换 provider session 身份（偏离 pi-durable，见 §2 末行）。

### 4.7 codemode

codemode 外层是一个 `komo.tool` 任务；脚本里每个子调用是一个 `kind: "nested"` 的工具子任务，结果写进
`komo.nested` document 的槽位（spec §7.3）。恢复规则不变：已交付的子结果原样接回，unsafe 子调用不重做
（`docs/codemode.md` §7）。时钟与随机数的宿主应答作为 memo 记在外层任务上（spec §5.2 的 memo：先写者赢）。
Monty worker 进程、只读挂载、受保护路径都不变。

## 5. 恢复

§8.4 的整张表缩成 pi-durable 的开库对账（spec §5.1）加工具任务的 `execute` 阶段：

| 重启前停在哪里 | 处理 |
|---|---|
| 任务 `running` | 回 `pending`，checkpoint 与 abort 标记保留，从 checkpoint 所在阶段重跑 |
| 生成任务在 `request` | 已提交的部分回复转成 `aborted` entry；按已提交上下文重发 |
| 工具任务在 `call`（意图未提交） | 确定没执行过，正常执行 |
| 工具任务在 `execute`，盘上有身份齐全、哈希对得上的 `output.json` | 补提交结果 entry，复用原输出（今天 §8.4 第 5 行的那一类，现在只因"blob 写完、结果没提交就崩"出现） |
| 工具任务在 `execute`，没有完整输出 | 已存与当前 replay 都 Safe → 新尝试重跑；否则 `uncertain` 结果附 `.partial` 末尾交给模型（§8.6 原文不变） |
| 工具任务在 `execute`，记录的子进程还活着 | `probe` 阶段：每轮调度探一次，结束了再按上一行判（§8.7 的规则，变成一个阶段） |
| `waiting` | 原样；等的任务进终态后由调度器重新评估 |
| submission `queued` | 在收件箱里，下一个 boundary 放 |
| 结果已提交，客户端没收到 | 客户端按 SSE 游标补读；不重做 |

消失的行：`accepted` 正文没写完、JSONL 已有结果而 db 落后、`run.absorbed` 已同步而 db 未提交、
`waiting + approval` 旧行（导入时处理，§8）、`waiting + dependency`。

§8.9 reconcile 只剩一件事：**被引用的 blob 读不到**。读到时把那个结果按"引用损坏"处置（§6.2），不扫全库。

## 6. komo 有、pi-durable 没有的

### 6.1 Session 生命周期（§8.10）

pi-durable 没有删除。komo 保留 `closing → deleted → purged`：

- `deleted`：`sessions` 行打标记，收件箱撤回，活任务 abort。
- `purged`：一次提交删掉该 Session 所有 conversation 的 entry / task / submission / document 行，之后删 blob 目录。
  **被别的 conversation 用 `parent.at` 引用的不许 purge**——`/fork` 出的独立 Session 会读它。所以 `/fork` 也应
  改成复制（与 `/clone` 一样），只有同一 Session 内的分支用 `parent.at`。这样跨 Session 永远没有共享，purge 的
  判据就只有一条。
- "entry 不可变、ID 不复用"照旧：删的是整个 Session，ID 不会被重新分配。

### 6.2 Intervention

审批删掉之后，剩下需要人的只有：blob 引用损坏，以及子进程探不出是否结束（§8.7 已改为每拍重探、不必等人）。
提议：

- 子进程：§5 的 `probe` 阶段，不进清单。
- blob 损坏：工具任务 `faulted`（spec §10.1"用户代码只让自己那个单元失败"），run 的输入 `unanswered`，
  原因写明缺哪个文件；`komo intervention list` 列 faulted 的任务。

这样 intervention 不再是一种等待状态，`WaitReason` 整个退役（§11 决定 3）。

### 6.3 其他保留项

- 每次提交 fsync，`synchronous=FULL`，`data_sync_retry=1`。
- 完整工具输出按尝试保存、不可变；模型视图有界并给 `artifact://` 引用。
- `write` / `edit` 拒写 komo 状态（`sessions/`、`state.db*`、`runtime/`、`.env`），codemode 的读限制不变。
- Gateway 是唯一打开状态的进程（实例锁），CLI / TUI 走 HTTP。
- 配置热重载：`komo.agent` 存名字，每阶段按当前快照解析（pi-durable 也是"names resolved at each use"）；
  已开始的请求阶段用 checkpoint 里冻结的模型与 effort。

## 7. 代价与风险

1. **内容不再是纯文本文件。** 今天可以 `tail -f sessions/x/events.jsonl`、`grep`、直接拷目录备份。之后要
   `komo session export`，备份要连 `state.db-log` 一起拷（今天也是）。AGENTS.md 的"内容权威是 Session 目录"
   "Nothing rewrites a JSONL line""§8.5 写入顺序"三条结构规则要改写。
2. **单库单线。** state.db 坏了，所有会话一起坏；今天 JSONL 至少还能按会话手工救。缓解：每日 `VACUUM INTO`
   快照 + 导出。
3. **§8.11 刚落地就换底层。** 用户行为和 HTTP / TUI 不变，但 kernel 的 `session/`、`session_events.rs`、
   `tree_codec.rs`、store 的 `session_tree.rs` 会被 fork 模型取代。
4. **重写面大。** komo-store 的 coordinator / ledger / queue / recovery / reconcile / outbox / session_log /
   checkpoint / payloads（约 1.1 万行）基本重写；komo-runtime 的 agent loop、executor、recovery、scheduler 改成
   任务阶段；`Ledger` / `RunQueue` trait 换成 `Line` + `Storage`。
5. **pi-durable 是实验性的。** 固定在 `8efd314fa` 移植契约；之后它变了不跟。
6. **进度提交变多。** 节流提交生成与工具进度会增加写放大，需要 §10 实测。

## 8. 不迁移（2026-10-23 定）

**不做兼容，新版本直接用新格式起步。** 2026-10-23 之前的"先排空、再导入收尾"方案（原 §8.1，以及为它准备的
`komo gateway drain` 前置版本）作废，没有实现。

- 新版本只认 `<data_dir>/durable.db` 与新的 blob 目录；**不读**旧的 `sessions/*/events.jsonl`、旧 `state.db` 的
  `runs` / `tool_calls` 等表，不导入、不翻译未结束的 Run。
- 旧数据**原地留着、不删**：`sessions/`、`state.db*` 照旧在盘上，想看可以用旧二进制或直接读文件；新版本不碰它们，
  也不在启动时清理。
- 因此不需要：`legacy_refs` 映射表、旧 `artifact://` URI 解析、旧 Memory 证据的重定位、旧格式解码器在新路径上的
  兼容、"旧二进制打开新库要拒绝"的保护（`durable_meta` 的格式版本仍保留，给新格式自己以后演进用）。
- cron 定义、Memory 条目、投递记录这些在旧 `state.db` 里的东西也**不带过来**；新版本从空开始。
- AGENTS.md 里只为兼容旧记录存在的规则（`approval.*` 退役事件要能解码、`waiting + approval` 旧行按批准续跑、
  退役的 delivery / outbox 行要跳过、"Schema 只做加法"对新库首个版本之前的改动）在 D7 一并删改。
- `komo session export`（pi v3 树形 JSONL）仍然要做，它是新格式的导出，不是兼容。

## 9. 分阶段

每阶段结束 `cargo test --workspace` 全绿、`cargo build --timings` 记录。

| 阶段 | 内容 | 验收 |
|---|---|---|
| D0 基准（**已完成 2026-10-23**） | 在 Turso 上测单线提交吞吐（每次 fsync）、节流进度的写放大、MVCC vs WAL | 数字写回本文 §10 |
| D1 kernel 模型（**已完成 2026-10-23**） | entry / conversation / task / submission / document 值类型，上下文推导（§2.1 规则 1–10），fork 遍历，`Storage` trait，内存实现 + conformance 用例 | 移植 pi-durable 的上下文与 fork 用例全部通过 |
| D2 store（**已完成 2026-10-23**） | `Line` + Turso `Storage`，新表 DDL 与字节对等测试，blob 写入顺序 | conformance 在 Turso 上通过；崩溃注入（提交前 / 后杀进程）后重开一致 |
| D3 任务调度（**已完成 2026-10-23**） | 任务状态机、step 规则、reservation / reconciliation、waiting / completing / abort 自底向上 | 移植 pi-durable 的 child-tasks / abort / restart 示例 |
| D4a 无状态模型调用（**已完成 2026-10-23**） | 三家 provider 的 `ModelStream::stream`：每次给完整 context，返回一条 assistant 消息；路由 `RoutingStream` | 请求体与旧的有状态 driver 逐字节相同 |
| D4b 生成与工具（**已完成 2026-10-23**） | `komo.generation` / `komo.tool`，收件箱与 boundary，`komo.live` 节流 | §5 恢复表逐行有测试；§8.6 中断正文逐字一致 |
| D4c 压缩与其余工具（压缩部分**已完成 2026-10-24**，见 §13.1） | `komo.compaction`（阻塞与后台）、衰减写成 `edits`、上下文超长时压缩重试、codemode 嵌套调用、编排工具（`task` / `monitor`）、子进程 probe 阶段 | 现有 `tests/recovery`、`tests/agents` 的对应场景改写后通过 |
| D5 gateway 与树 | 会话树映射到 fork、HTTP / SSE 改读新存储、`task` / cron / 投递 | §8.11 的端到端测试（`session_tree/tests.rs`）原样通过 |
| D6 导出 | `komo session export SESSION_ID`：输出 pi v3 树形 JSONL（带 blob 引用）；不导入旧会话（§8） | 导出的文件能被 pi 的 session-manager 读入，分支与当前位置一致 |
| D7 收尾 | 删旧 ledger 代码，改写 `docs/komo_bot.md` §8、AGENTS.md 结构规则、§14 验收表 | — |

## 10. D0 实测（2026-10-23）

本机（8 核，xfs 根分区），turso 0.7.2 release，`synchronous=FULL`，`data_sync_retry=1`，库放在真盘上（scratchpad 是
tmpfs，fsync 不算数）。代码：`.scratch/komo-v08-rewrite/spikes/durable-d0/`。

| 测量 | MVCC | WAL |
|---|---|---|
| 提交：2 KiB entry + task 替换 + document 替换，2000 次 | 71/s，p50 13.7 ms，p99 20 ms | 71/s，p50 13.5 ms，p99 29 ms |
| 提交：只替换 2 KiB document，2000 次 | 72/s，p50 13.4 ms | 74/s，p50 13.3 ms |
| 进度提交 `synchronous=OFF`（与 FULL 提交 9:1 交错） | 562/s，p50 0.13 ms | 576/s，p50 0.12 ms |
| 同时另一连接读最近 200 条：不开事务 | **602 成 / 3518 次 `database is locked`** | 21763 成 / 0 错 |
| 同上：`BEGIN … COMMIT` 包住 | 601 成 / 0 错（被压到提交频率） | 21479 成 / 0 错 |
| 典型 Run 提交数（10 轮，每轮生成 8 s、2 个工具各 3 s），进度节流 250 / 500 / 1000 ms | 621 / 341 / 201 | 同 |
| 同上，提交合计耗时（进度也 FULL） | 8.8 s / 4.8 s / 2.7 s | 8.3 s / 4.7 s / 2.8 s |

读法：

- **fsync 是唯一的成本**，13 ms 一次，两种模式一样；每次提交写几 KB 与只改一行没有差别。
- 进度全按 FULL 提交时，250 ms 节流下一次 Run 占线 8.5 s（Run 本身约 140 s，约 6%），十几个 conversation 同时忙
  就会排队。进度改用 `synchronous=OFF` 后，同一次 Run 的占线降到约 0.9 s（60 次结构性提交 × 13 ms + 560 次 × 0.13 ms）。
- 测试程序踩过一个坑，记下：同一个值反复 UPDATE，WAL 认出页面没变就不写也不 fsync（测出 3.6 万次/秒的假数字）；
  基准里每次写的值必须不同。strace 核对过：值不同时每次提交一次 fsync。
- MVCC 读被锁这一条**可能也影响现在的生产代码**：今天的读经 toasty 连接池，D0 只测了裸 turso 连接，没测 toasty 那一层，
  值得单独查一次现有读路径会不会碰到这个错误。

还没测：长期运行下 WAL checkpoint 的节奏与库增长。

## 11. 决定（2026-10-23 全部按建议定）

1. **接受"内容不再是 JSONL 文件"**：state.db 是唯一权威，events.jsonl 只做导出；AGENTS.md 的三条结构规则在 D7 改写。
2. **会话树用 fork 表达**（§4.2）；**`/fork` 与 `/clone` 都复制**，跨 Session 不共享（§6.1）。
3. **Intervention 退役**：子进程走 `probe` 阶段自动重探，blob 损坏记 faulted，清单列 faulted 任务；`WaitReason` 退役（§6.2）。
4. **流式进度走节流提交**（§3.6），SSE 只推已提交的东西；D0 之后补充：进度提交用 `synchronous=OFF`，不用退回易失例外。
5. ~~未结束的 Run：先排空、剩下的导入时收尾~~ → **不迁移**（2026-10-23 改定，§8）：新版本直接用新格式，旧数据原地留着不读。
6. **有未结束 Run 时仍禁止导航**（首版，§4.2）。

7. **agent loop**：照 pi 的钩子组织，通用 Task 引擎一起移植（§12.4）；去掉执行段、有状态 driver、租约与段装配（§12.3）。

8. **WAL、新库文件**（D0 之后定，§3.7）。

## 12. Agent loop：按 pi 的钩子组织，跑在 Task 引擎上（2026-10-23 定）

### 12.1 现在为什么绕

komo 的一次 Run 今天要经过：`RunQueue::claim`（租约 + 心跳）→ `AgentRunHandler` → Gateway 的
`SegmentSource` 装配上下文（`service/segment.rs`、`context_sources.rs`）→ `AgentLoop::run` 跑一个**执行段**
→ 段以 `SegmentOutcome` 结束（`Completed` / `Suspended` / `Failed` / `Cancelled` / `Recompose` /
`Interjected`）→ 压缩或插话时**回到 Gateway 重新装配**再开一段。模型这一侧是有状态的 `TurnDriver`：它在内存里
攒着 provider 原生的对话，衰减要靠 `RoundInput::ToolResults.revised` 按 `provider_call_id` 就地换。

相关代码约 1 万行：`agent/` ~4.5k（含测试）、`executor/mod.rs` 2.4k、`recovery/` ~1.8k、gateway 的
`segment.rs` + `context_sources.rs` ~2.1k，再加 ledger 侧。绕的原因有三个：上下文由 Gateway 从 JSONL 折出来、
loop 只能拿到一段；driver 有状态，改前缀只能"就地换"或"整段重来"；等人、等子 Run 要让出执行名额。新存储下
这三个原因都没了。

### 12.2 pi 的形状

pi `packages/agent/src/agent-loop.ts` 的 `runLoop`（约 170 行）就是两层 while：

```text
loop {                                   // 外层：有 follow-up 就接着跑
  while 有工具调用 || 有 steering {       // 内层：一轮一轮
    prepareNextTurn(上一轮)  → 可整体换掉 context（压缩就在这里）
    steering 消息追加进 context
    prepareRequest           → 可换 context / model / effort
    assistant = stream(context)          // 无状态：每次给完整 context
    error / aborted → 结束
    工具调用 → 执行（并行或顺序）→ 结果追加进 context
    finishTurn               → 可以要求结束
    steering = getSteeringMessages()
  }
  followUp = getFollowUpMessages(); 有就 continue，没有就 break
}
```

loop 不知道存储、不知道压缩算法、不知道收件箱：全是钩子。context 就是一个消息数组。

### 12.3 komo 的新 loop

**一个函数、一份消息数组、一组钩子**，形状照 pi，钩子由存储层实现，每个钩子边界就是一次提交：

| pi 钩子 | komo 里做什么 | 提交什么 |
|---|---|---|
| 开始 / `agentLoopContinue` | 从 entry 推导 context（§2.1 规则），按尾巴决定从哪继续（§12.4） | — |
| `prepareNextTurn` | `compaction::decide`：衰减 → 返回换了视图的 context；压缩 → 摘要请求后返回新 context | 衰减的 `edits` entry / `komo.compaction` entry |
| `getSteeringMessages` | 收件箱 postTools boundary（插话） | 放置 submission，追加 user entry |
| `prepareRequest` | 能力面、系统段、工具表；变了才写 | `komo.system` entry |
| `stream` | **无状态** provider 调用：`(model, context, options) -> AssistantMessage`；流式进度节流写 `komo.live` | assistant entry（含 `provider_blocks`） |
| 执行工具 | §12.5 | 意图（`komo.live` 槽位）→ 结果 entry |
| `finishTurn` | 预算（轮数、token）、取消 | — |
| `getFollowUpMessages` | 收件箱 final boundary（下一条输入）；当前输入 settle `done` | submission settle + 新 run 的 user entry |

随之删掉的：

- `Segment` / `SegmentOutcome` 的 `Recompose` 与 `Interjected`：压缩在 `prepareNextTurn` 里**直接换 context**，插话就是
  steering 消息，都不用回 Gateway。
- `Suspended`：`WaitReason` 已退役（§11 决定 3）。
- `TurnDriver` 与 `revised`：driver 无状态，衰减就是下一次请求用了换过的 context。**提示缓存不受影响**——前缀
  字节没变就命中，变不变由 `compaction::decide` 那本账决定，这本来就是现在的规则。
- `RunQueue::claim` 的租约、心跳、领取代次：Gateway 一个进程、一条提交线，conversation 忙不忙就是
  `komo.live.run` 在不在（pi-durable §6），不需要租约防双跑。
- Gateway 侧的段装配：context 推导是 kernel 里的纯函数，loop 自己调。

### 12.4 恢复：通用 Task 引擎（2026-10-23 定：一起移植）

pi-durable 的 Task 引擎（§5：pending / running / waiting / completing / terminal、step 规则、reservation 与
reconciliation、所有权遍历、abort 自底向上）**照搬**，作为执行的底座。loop 不是一个长跑的 async 函数，而是
pi-durable 那样**拆在提交点上**：

- `komo.generation`：一轮。阶段 `prepare`（= `prepareNextTurn` + steering + `prepareRequest`）→ `request`
  （= `stream`）→ `retry` / `poll` → `tools`（建工具子任务并 `waiting`）；一轮结束把 run 交给下一个 generation，
  或在 final boundary 结束（= `finishTurn` + `getFollowUpMessages`）。
- `komo.tool`：一个调用。`call`（prepare，提交意图）→ `execute`（只由恢复进入，§5 的重放表）。
- `komo.compaction`：压缩，可以被 generation 阻塞地等，也可以后台跑（pi-durable §8.7 的两个阈值）。

**代码按 pi 的 `runLoop` 来读**：§12.3 的钩子表就是各阶段的内容，每个阶段一个小函数，阶段之间的状态只在
checkpoint 里。重启时 `running` 回 `pending`，从 checkpoint 所在阶段继续——不靠"看尾巴猜"，checkpoint 写明了。

引擎同时承担这几件事：

- **后台 `task`**：工具任务拥有子 conversation（background），`context: "fork"` 时从当前 assistant entry fork；
  交回结果仍是往父 conversation 提交一条带 `request_id` 的输入（§4.6）。以后要做"同步等子任务"，就是 `waiting`
  加 `on: [子任务]`，不用新机制。
- **codemode 嵌套调用**：`kind: "nested"` 的工具子任务（§4.7）。
- **取消**：`abortTask` / `Conversation.abort()`，abort 自底向上，每个任务在自己的 abort handler 里写 aborted 结果。
- **子进程探测**：工具任务的 `probe` 阶段，`sleep(until)` 后再探（§5）。

### 12.5 工具执行

照 pi 的 `executeToolCalls`：prepare（校验参数、能力面、`Tool::prepare`）→ execute → finalize，只读的并行、
有副作用的与 codemode 顺序执行（今天的规则）。每个调用在 komo 里多两次提交，正是 effect sandwich：

```text
prepare 通过 → 提交槽位 {call, replay, attempt, pid?}（意图）
execute      → blob 流式写 .partial，进度节流写槽位尾巴
finalize     → blob 收尾同步 → 提交结果 entry（projection.rs 的 full 视图）+ 清槽位
```

恢复时没结果的调用：没有槽位 = 确定没执行，照常执行；有槽位 = §5 的表（完整 `output.json` 复用；双 Safe 重跑；
子进程还活着就 probe；否则中断结果交给模型）。

### 12.6 规模与对本方案的影响

- 引擎：`scheduler.ts` 近 2000 行 TS 是最大的一块；Rust 版去掉 Chord watch 与扩展 registry 之后会小一些。
- 生成与工具任务按 pi-durable 的规模估：`generation.ts` 715 行 + `tool.ts` 918 行；provider driver 改成无状态
  函数，三家各自变小；executor 去掉恢复分支后只剩 prepare / execute / finalize 与并行调度。
- §9 的 D3（任务调度）保留；D4 是"generation / tool / compaction 三个任务 + 收件箱与 boundary + 无状态 driver"。
- 结论：**存储与执行都照 pi-durable；每个任务阶段里的逻辑按 pi agent loop 的钩子组织**，读代码时能对着
  `runLoop` 一段段找到。

## 13. 实现进度与偏离（2026-10-23）

### 13.1 位置与测试（D4c 在集成分支 `d4c`，其余已合入 `main`）

| 阶段 | 位置 | 测试 |
|---|---|---|
| D1 | `crates/komo-kernel/src/durable/`（`ids` `message` `records` `context` `fork` `storage` `memory` `conformance`） | 34 项；conformance 24 个用例 |
| D2 | `crates/komo-store/src/durable/`（`schema` `write` `line` `ids` `blob` `storage` `crash_tests`） | 22 项，含 Turso 上的 conformance 与崩溃注入 |
| D3 | `crates/komo-kernel/src/durable/task/`（纯逻辑）、`crates/komo-runtime/src/durable/`（`line` `tx` `kind` `runtime` `scheduler` `engine`） | kernel 37 项、runtime 52 项 |
| D4a | `crates/komo-runtime/src/llm/{model_stream,routing_stream,stream_tests}.rs`、各 provider 的 `stateless.rs` | 字节等价：9 种场景 × 3 家 |
| D4b | `crates/komo-runtime/src/durable/harness/`（`hooks` `inbox` `live` `system` `generation` `tool` `submit` `entries`） | 约 50 项 |
| D4c（probe 与接线，`d4c-probe-gaps`） | `harness/tool.rs`（`Checkpoint::Probe`、`ChildRef`、`resolve`）、`harness/live.rs`（槽位 `pid` / `pgid` / `startedAt`）、`harness/hooks.rs`（`HarnessDeps::probe`、`probe_interval`）、`recovery/processes.rs`（`IdentityProbe`）、kernel `OutputWriter::child_started`、`tools/process.rs`（起进程后报告）；gateway `RoutedOutputs::find_published`；`ToolExecutor::tools()` 公开 | `harness/tests/probe.rs` 10 项（含真进程的身份核对）、`harness/tests/turso.rs` 6 项（Turso 文件库上的生成 / 工具任务 / 重启）、`IdentityProbe` 3 项、`child_started` 1 项、`RoutedOutputs` 1 项 |
| `komo init` | `komo-runtime/src/config/init.rs`；bin 的 `Command::Init` 只分发 | init 7 项、bin 1 项加一条 clap 解析断言 |
| D4c 压缩（`d4c-compaction`） | 纯逻辑 `crates/komo-kernel/src/durable/compaction.rs`（切点、摘要输入序列化、衰减候选与 `edits`、从 entry 折在线状态）；`crates/komo-runtime/src/durable/harness/compaction/`（`setup` 钩子值 · `plan` 缓存账决策 · `task` `komo.compaction` 任务）；`generation.rs` 的 `prepare` / 超长分支；`live.rs` 的 `compactions` 与 `compactionFailure`；`Harness::compact`；`AgentHooks::compaction` | kernel 11 项、runtime 25 项（`plan` 8 项单测 + `tests/compaction.rs` 17 项集成，含 Turso `Storage` 上的超长救援与重开回放） |

合入时（`durable-integration` 分支）强制重编后 `cargo test --workspace`：**2558 通过，0 失败**。D4c 的 probe 与接线（`d4c-probe-gaps`，基于 `main` 的 52ae527c）：**2587 通过，0 失败，11 忽略**。D4c 压缩（`d4c-compaction`，同一基线）：**2594 通过，0 失败，11 忽略**。

### 13.2 实现与本文的偏离（都已接受）

- **document 只存当前值**（D1）：没有历史，所以 fork 无法按 `asOf` 复制 document（§3.3 里 `komo.agent` 那一行）。fork 时怎么带 document，D5 定。
- **同一 conversation 里 `request_id` 重复会被拒绝**（D1）：pi 的内存后端静默覆盖；这里更严。
- **ID 上限 `i64::MAX`**（D1）：数据库整数列是有符号 64 位。
- **进度提交是显式的**：`Storage::commit_progress`（D2）与 `Tx::mark_progress`（集成时改）。不按"整批都是 document 改动"推断——只改 document 的提交也可能是已确认给客户端的结构性改动。
- **校验失败只拒绝这一笔、线照常可用；只有引擎 / IO 错误让线失败**（D2）。pi 把所有存储错误都当致命。
- **新库的写走裸 `turso::Connection`**，不经 toasty：要逐次切 `PRAGMA synchronous`，连接池做不到。裸 SQL 在 `komo-store/src/durable/{schema,write,line,storage}.rs`；AGENTS.md"裸 SQL 只许在三个模块"这条在 D7 改写。
- **Task 引擎的所有权索引更简单**（D3）：不做 pi 的候选集增量裁剪，每轮扫活任务、从已提交记录重新推出标记。取消用 `CancellationToken`；阶段里的 panic 按错误处理。
- **工具轮按组执行**（D4b）：连续的只读调用一组并行，其他调用各自一组顺序执行；pi 只有全并行 / 全顺序。
- **不可用的工具也建工具任务**（D4b），结果经输出存储落盘，视图与今天一致；pi 直接在生成提交里写结果。
- **工具输出路径是 `tool-output/{conversation}/{tool_task}/{attempt}/`**（D4b），不是 §3.5 的两级，复用现有输出存储。
- **`komo.usage` 没做**（D4b）：预算用的轮数与 token 记在 `komo.live.run`。新增未答原因 `budget`。
- **意图 checkpoint 存完整 `ExecutionPlan`**（D4b），Safe 重跑用原计划。
- **Anthropic 衰减时的额外缓存断点**要调用方传 `revised_from`（D4a），对应旧 driver 的行为。
- **子进程身份由工具经 `OutputWriter::child_started` 报告**（D4c）：kernel 的 trait 加了一个默认空实现的方法，`tools/process.rs` 的 `run_child_detachable` 起进程后调它；工具任务的 `Tee` 把身份记进输出尾巴的同一份快照，叫醒进度提交（不等 250 ms 节流），经 `commit_progress` 写进槽位的 `pid` / `pgid` / `startedAt`（Unix 毫秒）。槽位在重跑与收尾时连同输出尾巴一起清掉。旧 `ChildRegistry` 的登记照旧写，新路径不读它。
- **`probe` 是第三个 checkpoint 阶段**（D4c）：`execute` 恢复时槽位里记着这次尝试的 pid 就写 `Probe { …同 execute…, child, until }` 并返回，`probe` 阶段 `sleep_until(until)` → 探一次 → 还在就 `until += probe_interval`（默认 5 s，`HarnessSettings`）再来，结束了回到 `execute` 那张表（`resolve`，不再探子进程）。每一轮探测是一次 checkpoint 提交；重启后从 checkpoint 里的 `child` 与 `until` 接着探，不依赖槽位。`output.json` 完整性先于子进程判断：盘上已有完整输出就直接复用，不进 probe。
- **探测返回 `Unknown` 当作还在**（§8.7：阻止重复启动比重复执行安全），所以核实不了的子进程会让任务一直 probe，直到进程确实没了或用户取消。
- **取消 probe 中的任务不终止那个子进程**（D4c）：abort handler 照常写 `aborted` 结果（带 `.partial` 末尾），再探一次，进程还在就在正文里写明"重启前这次调用起的子进程（pid N）取消时还在运行，komo 没有终止它"。旧 `monitors.rs` 会按身份核对后 `kill_process_group`；工具任务这边没有杀进程的注入口，暂不做。
- **新增 `IdentityProbe`，不改 `SysProcessProbe`**（D4c）：登记行带 `started_at` 时核对起来的时刻（Linux 读 `/proc/<pid>/stat` 第 22 字段 + `btime` + `CLK_TCK`，其他平台 `ps -o stat=,etime=`，容差 10 s），对不上就是 pid 被复用 → `Gone`；僵尸进程也是 `Gone`；读不出来 `Unknown`。旧路径（`LockHolderLiveness`、gateway 锁）仍用只看 pid 在不在的 `SysProcessProbe`，行为不变。
- **工具任务读自己的槽位用一次"不改状态的提交"**（D4c，`recorded_child`）：`TaskRuntime` 没有读 document 的入口，没有为此扩引擎。
- **测试里的 Turso 世界**（D4c）：`World::turso(dir)` 用真时钟与真文件输出存储；`restart` 是关引擎（连库一起关）、`TursoStorage::open` 重新开库、重建引擎。引擎关着时读辅助不可用。
- **压缩的在线状态从 entry 折，不单独存 document**（D4c）：旧实现的"请求次数、上下文增量、缓存债、冷却"是从事件日志折的，现在从 entry 折（`durable::compaction::online_state`）——请求次数与上下文大小读 assistant 消息的用量，压缩 / 衰减欠下的债记在各自 entry 的 `data.debt`，`head` 指自己的 entry（`/new`）归零。这比放 document / checkpoint 更稳：重启、fork 之后重折一遍就回来，没有第二份真相。唯一不在 entry 里的是旧的 `compaction_refused`（"刚压失败过"）：在 `komo.live.compactionFailure`，用任务 ID 当位置标记（任务 ID 与 entry ID 同一计数），之后出现一条完成的工具结果就解除。缓存是否已冷看最后一条正常结束的 assistant 消息的时间戳。
- **后台阈值默认关、不过缓存账**（D4c）：`CompactionSettings.background_tokens`（pi 的 `backgroundTokens`）= 0 不启用；到了 `阻塞阈值 - background_tokens` 就在后台起压缩，只要求摘要比被收的那段小、压完在阻塞阈值之下，不再看旧的经济账（那本账估不出剩余请求，只剩冷缓存与窗口保护两条路，后台阈值本来就低于窗口保护线）。`[compaction]` 配置里还没有对应的键。
- **超长恢复不看 `[compaction] enabled`**（D4c）：同旧实现，不压这条 run 就失败了；pi 要求策略开着。其余照 pi：一个生成只救一次（checkpoint 里的 `compacted`），救完又超长就 `model_error`；旧实现的"压成摘要之后至少有一次成功请求"也保留（折账读 `requests_since_last_summary`）。
- **摘要请求**（D4c）：两条消息、没有工具——系统提示与 `<conversation>…</conversation>` + 指令（pi 的 `serializeConversation` 格式，思考块与 system 消息不进，工具结果截 2000 字符），不再是旧的单条中文渲染。默认提示是 pi 的结构化检查点格式（`SummaryPrompt::default`），gateway 经 `AgentHooks::compaction` 换成 komo-agent 的。模型、提示、`tail`、`firstKept`、时间戳钉在 checkpoint，重启后重发逐字节相同。
- **切点用 pi 的 `selectCut`**（D4c）：以 entry 为单位，候选是以 user / assistant 消息开头的 entry，工具结果与 system entry 从不是候选，也不落在"调用与结果之间"的 user 上；旧的 `find_cut` 只认助手回复开头，两者同样保证调用不会和结果拆开。
- **衰减 entry 的 kind 是 `komo.decay`**（D4c）：只带 `edits`（`replace`，目标 = 结果 entry，替换消息 = 原结果 + 正文换成 `data.decayed`，图片块留着）和 `data { decayed: [entry id], debt, decision }`。与 `komo.system` 同一次提交追加，所以 `cutoff` 之后的请求就带短视图；这一轮的 `revised_from`（第一条被换的结果在请求里的下标）写进 `request` 的 checkpoint，重启后重发带同一个值。`full_sends` 的"完整给过几次" = 它之后的 assistant 消息数。
- **摘要的用量没计**（D4c）：`komo.usage` 还没做（D4b 起就没有），摘要请求的 token 不进累计用量。
- **没有 `beforeCompact` 钩子**（D4c）：pi 的 `hooks.beforeCompact`（调用方给摘要或拒绝）没移植；手动压缩用 `Harness::compact`（conversation 所有、不是 background、`instructions` 可选）。
- **`TaskRuntime::find_document`**（D4c，`durable/runtime.rs`）：阶段里读已提交 document 的只读方法（`prepare` 要在建任务之前读 `komo.live.compactions` 与 `compactionFailure`）。

### 13.3 已知缺口（按阶段）

- **D4c**（压缩、probe 已做完，剩下的）：codemode、`task`、`monitor` 还没有执行路径。
- **D4c 的 probe**：没有杀进程的注入口（取消只在结果里说明子进程还在）；`bash` 转后台（`detach_after`）交给 gateway 的监听接手后，那条路径不写槽位也不进 probe；只有走 `run_child_detachable` 的工具会报告子进程（codemode worker、MCP 子进程不报）。
- **压缩 · D5**：gateway 实现 `AgentHooks::compaction`（`[compaction]` 配置 → `CompactionSettings`，另加 `background_tokens` 的配置键；komo-agent 的摘要提示与摘要模型；默认实现只是"压缩关着 + pi 的提示"）；`/compact` 路由接 `Harness::compact`；`komo.live.compactions` / `compactionFailure` 还没有客户端展示；fork 不复制 document，所以 `compactionFailure`（"刚压失败过"）不随 fork 继承，其余在线状态是从 entry 折的、自然继承。
- **压缩 · 真机**：Anthropic 的 `revised_from` 缓存断点只测到"请求里带了值"，没有对真 provider 验证过命中；摘要请求没有 `cacheRetention: none` 的对应选项（无状态调用没有这个旋钮），摘要请求会写进提示缓存。
- **D5**：gateway 实现 `AgentHooks`（`ToolExecutor::tools()` 与 `RoutedOutputs::find_published` 已就绪）；`conversations.session_id` 现在总是 NULL，`/tree` 按 Session 查分支时补；`TursoStorage` 的读共用一条连接，读多了再换连接池。
- **崩溃安全**：进度提交 `synchronous=OFF` 后"进程崩溃不丢"是按机制推的，崩溃注入测试只跑在 tmpfs 上，没有真盘掉电验证。Turso 接线测试里的"重启"是关库再开库（干净关闭），不是杀进程。
- **开发环境**：多个 worktree 共用一个 `target/` 时，cargo 可能复用别的 worktree 编出来的 crate；`scripts/install-local.sh` 现在在上一次不是本检出编的时候先 `touch` `crates/*/src/lib.rs`，但手动 `cargo test` 共用 target 前仍要自己 `touch crates/*/src/lib.rs`。

