# 新存储的优化清单（D7 之后）

> 状态：**P1（§1、§2）已实施，其余待实施**（2026-10-26）。实施细节与验收见 §10；§1.1、§2.1 保留优化前的现状。
> 对象是 `docs/storage-durable.md` 落地后的 `durable.db` 读写路径与生成的流式进度。
>
> 对照：本机 `../pi` 提交 **`d5b3de22d`**（2026-10-25）的 `packages/durable`。移植基线 `8efd314fa`（2026-10-16）
> 之后 `packages/durable` 又有 36 个提交；本文用到的是：
>
> | pi 提交 | 内容 | 本文 |
> |---|---|---|
> | （基线已有，之后未改）`harness/context.ts` | `captureContextBounds` + `ContextRange` 增量上下文 | §1 |
> | `6c2fd73b0` deferred storage of throttled commits | 节流进度先在内存采纳、随后一次提交或 `withinMs` 内落库（spec §4.1） | §4 |
> | `da412b039` appendText | 部分回复按追加记 delta，不再整串重赋值 | §4 |
> | `f715a15c1` parse streamed tool-call arguments incrementally（#9265） | 流式工具参数线性解析，快照不再每个 delta 复制整串 | §5 |
> | `a30c2a81d` crash-loop guard | 任务连续被重启打断 `restartLimit`（5）次就停成 `blocked` | §6 |
>
> 本机仍跑旧版 komo（只有 `state.db` 与 `sessions/*/events.jsonl`，没有 `durable.db`），所以下文的影响是
> **按代码推的，没有实测**；量级参考：本机最长的旧会话 4661 行事件、`events.jsonl` 11 MB。

## 0. 结论

| 优先级 | 项 | 性质 | 动的地方 |
|---|---|---|---|
| P1 | §1 上下文读取：只读活动范围 + 按 conversation 增量缓存，持线时间缩到两次点查 | 性能（O(历史) → O(窗口)），解除跨 conversation 的阻塞 | runtime `durable/runtime.rs`、`harness/{generation,compaction,orchestration}`、gateway `submit.rs` |
| P1 | §2 macOS 上打开 `PRAGMA fullfsync` | **正确性**：兑现"返回即已落盘" | store `durable/schema.rs` |
| P2 | §3 gateway 读路径（SSE 补读、run 状态、任务进度）复用 §1 的缓存与已有的增量投影 | 性能（D5 已记为缺口） | gateway `service/durable/{feed,runs}.rs` |
| P2 | §5 流式快照：每个网络块都复制整段回复、重解析工具参数 | CPU，单次生成 O(n²) | runtime `llm/*/stateless.rs`、`harness/generation.rs` |
| P3 | §4 `komo.live` 的进度提交：整值重写，每 250 ms 一个事务 | 先测；超标照 pi 的 deferred 提交 | runtime `harness/live.rs`、`durable/line.rs` |
| P3 | §6 崩溃循环保护（`restartLimit`） | 健壮性：反复打死进程的任务不再无限重跑 | kernel `durable/task/`、runtime scheduler |
| P3 | §7 退役 document 的值 | 库增长 | store `durable/write.rs` |

不做的见 §8，顺序与测量见 §9。

## 1. 上下文读取（P1）

### 1.1 现状

每一轮生成从 entry 1 起读整段可见历史（含压缩 head 之前、早已不进上下文的部分），逐条 serde 成
`EntryRecord`，再 `derive_context` 从头推导：

- 入口 `TaskRuntime::entries`（`crates/komo-runtime/src/durable/runtime.rs:279`）；`context()` 的注释写着
  "不缓存……pi 的 range 缓存是性能优化，这里留给 D4 需要时再加"，之后没有加。pi 的 `ContextRange` 在移植基线里
  就有，是移植时省掉的。
- 一轮里的调用点：`prepare`（`harness/generation.rs:271`）、`request` 阶段按 `cutoff` 再推一次（`:542`）、
  超长救援（`:472`）；压缩任务两处（`harness/compaction/task.rs:197`、`:249`）；编排的 `fork_seed`
  （`orchestration/task.rs:82`，只要最新 entry ID 与一条 assistant entry）与查进度（`:485`）。
- **整段扫描在线上进行**：`entries` 走 `read_on_line`（`durable/line.rs:203`），持有与提交共用的 FIFO
  `gate` 直到最后一页读完。提交也在 `gate` 里等 fsync，所以一个长会话读历史期间，**所有 conversation 的
  提交都在排队**。这与设计 §3.1"读不经过线"不一致（偏离未登记）。
- gateway 每条用户输入还有一次整段读：`project_update_for`（`crates/komo-gateway/src/service/durable/submit.rs:294`）
  只为取当前展示的 `project` 段，读全部 entry 再 `derive_context`。

代价随会话长度线性增长，一个会话累计是平方：第 n 轮读 n 轮的历史。压缩只减少发给模型的东西，不减少读的东西。

### 1.2 pi-durable 的做法

`packages/durable/src/harness/context.ts`，scheduler 在 `src/harness/scheduler.ts:2278` 调用：

1. **线上只取边界**（`captureContextBounds`）：最新可见 entry 的 ID（`tail`）与 `tail` 处最新的 head 标记，
   两次 O(1) 读。
2. **线外扫范围**：`[head.head, tail]`。`tail` 及之前的 entry 不可变，线外读是安全的。
3. **增量**（`readContextFrom` + `ContextRange`）：scheduler 按 conversation 留上一次的范围（`#contexts`，
   忙时一直留，空闲 `contextRetentionMs` = 10 分钟后丢，spec 默认值表）。head 没变就只扫 `tail` 之后；新 entry 带
   `edits` / `head`、或是已有 edit 的目标时整段重推；`tail` 变小（按 `at` 截）就截前缀，不读库。
   `settled` / `open` 拆分保证只有最后一条 assistant 之后的消息需要重排工具结果。

### 1.3 方案

照搬上面三步，按 komo 的情况补几处：

1. **`ContextBounds` 与 `captureContextBounds`**：`Storage` 已有 `find_latest_head_marker` 与降序 `scan_entries(.., 1)`，
   不用加 trait 方法。`read_on_line` 只包这两次读。
2. **范围扫描移出线**：`scan_entries` 的 `min = head.head`、`max = tail`，走 `TursoStorage` 的读连接池。
   分页之间有新提交也无妨（`max` 钉死了上界，范围内的 entry 不可变）。
3. **`ContextRange` 缓存**：放在调度器共享状态里（`Arc<Mutex<HashMap<ConversationId, …>>>`），空闲 10 分钟淘汰，
   对齐 pi。存 `Arc<[EntryRecord]>`，调用方拿只读切片，不再每次 clone 整段。失效条件：
   - head 标记变了 → 重扫；
   - 新增 entry 带 `edits` / `head` 或是已有 edit 的目标 → 用已缓存的 entry 重推，不重读库；
   - 订阅提交线的 `PurgeConversations` → 删掉对应条目（purge 是唯一会让 entry 消失的写）。
   fork 的子 conversation 沿父链读；父在 `parent.at` 之前的 entry 不可变，不需要额外处理。
4. **`online_state` 跟着缓存一起存**：它跨压缩累计缓存债，不能只看 head 之后（`komo-kernel/src/durable/compaction.rs:432`）。
   函数注释写明"可切分：先折前缀再续折等于一次折完"，所以缓存里存折到 `tail` 为止的 `OnlineState`，增量时续折。
   首次建缓存要折一遍全历史；首版全量反序列化，测了再决定要不要把 `data.debt` 与 `usage.input` 提成列、只扫这几列。
5. **`cache_cold` 只看范围内**：它找最后一条正常结束的 assistant。`/new` 之后范围内还没有 assistant 时，结论从
   "按旧回复算冷 / 热"变成"没请求过、不算冷"。`/new` 之后前缀本来就换了，缓存账不受影响，但这是行为变化，要有测试钉住。
6. **`plan_system_entries` 只需要范围**：它用 `entries` 只是为了查 `view.entries` 里各 ID 的 `kind`
   （`harness/system.rs:194`），范围内的 entry 已经够。
7. **gateway 的 `project_update_for`**：改成读最新 head 之后的 `komo.system` entry，或直接复用 §1.3 的缓存（同一进程）。
8. **`fork_seed`**：只要最新 entry ID 与 `cx.assistant` 那一条，改成 `scan_entries(.., 1)` + `visible_entry`。

### 1.4 验收

- 结果等价：现有 `derive_context` 的用例全过；加一组性质测试——随机生成 entry 序列（含 edits、head、fork），
  任意切点上"增量读"与"从头读"的 `ContextView`、`OnlineState` 完全相同。
- 持线时间：一次 `TaskRuntime::context` 持 `gate` 的时间与会话长度无关（用 1 万条 entry 的会话量）。
- 不阻塞：长会话读上下文期间，另一个 conversation 的提交延迟不变（仿 `concurrent_backfills_do_not_stall_commits`）。
- `/new` 之后 `cache_cold` 的新行为有测试；`cargo test --workspace` 全绿。

## 2. macOS 上的 `fullfsync`（P1，正确性）

### 2.1 现状

- `schema::open`（`crates/komo-store/src/durable/schema.rs:309`）设了 `data_sync_retry = 1` 与
  `synchronous = FULL`，没设 `fullfsync`。
- turso 0.7.2 的 pager 默认 `FileSyncType::Fsync`（`turso_core/storage/pager.rs:1680`），WAL 收尾与 checkpoint
  都按 `get_sync_type()` 同步（同文件 `:3549` 等）；只有 `PRAGMA fullfsync` 才改成 `F_FULLFSYNC`（`io/unix.rs:461`）。
- macOS 上普通 `fsync(2)` 不冲盘的写缓存，掉电时"已提交、已回复调用方"的结构性提交可能丢。
- blob 用 Rust 的 `File::sync_all`（`komo-store/src/durable/blob.rs:65`），在 Apple 平台上是 `F_FULLFSYNC`——
  blob 比引用它的提交落得更实，effect sandwich 的次序没破，但"返回即已落盘"在 Mac 上不成立。
- §10 的 13 ms / 次是 Linux xfs 上测的；Mac 上没有数。
- 对照：pi 的 SQLite 驱动（`packages/chord/src/sqlite/node.ts:190`）是 `synchronous = NORMAL`、不设 fullfsync，
  komo 从设计起就比它严（设计 §2"不搬 NORMAL"），这里只是把同一个承诺在 Mac 上补齐。

### 2.2 方案

- `schema::open` 执行 `PRAGMA fullfsync = ON`（非 Apple 平台是 no-op，可以无条件执行）。它是连接级的，写连接必须设。
- 进度提交仍是 `synchronous = OFF`，不受影响。
- 加一条 `schema.rs` 测试：Apple 平台上 `PRAGMA fullfsync` 读回 1。

### 2.3 要先测的

- Mac 上结构性提交的延迟（`F_FULLFSYNC` 在 Apple SSD 上通常明显慢于 `fsync`）。用 D0 的 spike
  （`.scratch/komo-v08-rewrite/spikes/durable-d0/`）在本机跑 FULL / FULL+fullfsync / OFF 三档，写回
  `docs/storage-durable.md` §10。如果单次提交慢到一次典型 Run 的占线不可接受（§10 口径约 60 次结构性提交），
  再考虑 §8 里否掉的组提交，而不是放弃 fullfsync。

## 3. gateway 读路径（P2，D5 已记的缺口）

- SSE 补读（`service/durable/feed.rs` 的 `backfill`）每次读整个 Session 的 entry 与 submission，全量
  `legacy_events` 投影，再丢掉 `seq <= cursor` 的帧（D5 W2b 偏离②）。`runs.rs` 的 `load_log` 同样整段读、整段投影。
- `legacy_events` 已有增量入口（`Projection::project`，feed 直播在用）。做法：按 Session 缓存投影状态与已见的最大
  记录 ID，补读只投影之后的记录；冷启动仍全量一次。entry 部分与 §1 的缓存共用；submission 是可变记录，以 `commit_seq` 判新旧。
- 做完后放宽 `READ_PERMITS = 2`（D5 W5 的表：限 2 路是因为每次补读都在全量投影抢 CPU）。
- 会话列表全表扫、`FaultIndex` / `replay_recent` 的索引（D5 "留给 D7 / 之后"④）不在本文范围，按原计划。

## 4. `komo.live` 的进度提交（P3，先测）

### 4.1 现状

- `komo.live` 是一份整值 document：`run`、`generation`（含节流的部分回复）、`tools`（槽位 + 输出尾巴）、
  `compactions`、`compactionFailure`（`harness/live.rs:164`）。
- 每条在流的生成 / 工具每 250 ms 一次进度提交（`commit_partials`，`harness/generation.rs:564`）：整份替换，
  `synchronous = OFF`。fsync 已经省掉（§10：0.13 ms / 次），剩下的是**每次一个存储事务 + 整份 document 写进 WAL**，
  且每次提交都要拿线的 `gate`。结构性提交改 `run` / 槽位时，部分回复与输出尾巴也跟着重写。
- §10 记着"还没测：长期运行下 WAL checkpoint 的节奏与库增长"。

### 4.2 pi 新版本的做法

- **deferred 提交**（`6c2fd73b0`，spec §4.1）：提交带 `persistence: { kind: "deferred", withinMs }`，且只改已有的
  conversation document 时，**先在内存采纳**、立刻发布给本进程的观察者（watch、会话视图、agent 事件），落库推迟到
  最早的一个：下一次读写这份 document 的普通提交（并进那次事务）、`withinMs`（默认 1000 ms）到点、`close()`。
  生成的部分回复与工具输出节流都用它。pi 实测 32 路流式：存储事务 562/s → 99/s，磁盘 14.3 → 4.9 MB/s，CPU 29% → 22%；
  代价是崩溃最多丢约 1 秒的流式内容。`subscribeCommits()` 只看已落库的提交。
- **追加而不是重赋值**（`da412b039`）：部分回复的文本与思考按 `appendText` 记 delta。komo 不搬 Chord delta（设计 §3.3，
  理由与重新考虑的条件见 §8），这一条没法直接照搬；只取"追加"的思路，见 §4.3 的退路。

### 4.3 方案（测量超标时）

- 先测：一次长回复（50 KB 输出、10 轮工具）与 N 路并发流下，`durable.db-wal` 的峰值、checkpoint 次数、
  `komo.live` 单次替换字节数、线上每秒事务数。
- 超标时照 deferred 做，**但要先改一条不变量**：komo 现在是"SSE 只推已提交的东西"（设计 §3.6，pi 原 spec 不变量 3）。
  pi 改了 spec §1 / §13，让本进程观察者能看到只在内存的提交；komo 的 SSE feed 订阅的是提交线的发布，要分出
  "内存采纳"与"已落库"两种发布，feed 推前者、其余订阅者（投递、memory）只认后者。这是设计层面的改动，要在
  `docs/storage-durable.md` §3.6 / §11 记决定。
- 不想动不变量时的退路：把节流进度（部分回复、输出尾巴）拆进单独的 conversation 级 document（如 `komo.live.progress`），
  结构性提交不再携带它；节流间隔从 250 ms 放宽。

## 5. 流式快照的平方成本（P2）

### 5.1 现状

三家 provider 的无状态流在**每个网络块**之后都造一份完整的部分消息交给 `progress`：

- `llm/anthropic/stateless.rs:191`、`llm/chat/stateless.rs:184`、`llm/responses/stateless.rs:193`：
  `progress(&partial_message(.., assembled.content_so_far()))`。`content_so_far` 克隆已收到的全部文本与思考，
  工具调用的参数用 `lenient_arguments`（`llm/model_stream.rs:525`）对整段半截 JSON 做一次 `serde_json::from_str`
  （没收齐时失败、给 `{}`，但也已经扫了一遍）。
- 回调（`harness/generation.rs:578`）再 `partial.clone()` 一次放进 `latest`；`commit_partials` 每 250 ms 才取一次。
  也就是说，绝大多数快照造出来就被下一份覆盖了。
- 单次生成的成本是 O(块数 × 已收长度)。一个写 100 KB 文件的 `write` 调用，参数被逐块扫上千遍。

pi 的 #9265（`f715a15c1`）修的就是这个：`StreamingJsonParser` 每个 delta 只读一次、快照不可变，provider 的流状态
不再挂在内容块上；相关的 changelog 还记了一个 140 KB 回答 + 140 KB 工具调用让观察者持有 100 MB 以上的问题。

### 5.2 方案

- **改成拉取**：`progress` 只发"有变化"的信号（或块计数），`commit_partials` 到点时向 assembler 要一次快照。快照成本
  从"每块一次"降到"每 250 ms 一次"。`ModelStream` 的 `progress` 签名要改，三家 provider 与测试替身一起动。
- 进度快照里的工具参数不必是解析好的值：部分回复只用于展示，可以存原始参数文本（或只在收齐后解析），省掉
  `lenient_arguments` 的反复扫描。要核对 SSE / TUI 有没有读部分回复里工具参数的结构。
- 验收：`stream_tests.rs` 的字节等价测试不变；加一条"N 块的流里快照构造次数 ≤ 节流次数 + 1"的测试。

## 6. 崩溃循环保护（P3，健壮性）

- komo 的恢复是"`running` 一律回 `pending`、从 checkpoint 重跑"（设计 §5），没有次数上限。一个在下一个 checkpoint
  之前就把进程打死的任务（内存爆、栈溢出、触发 panic=abort 的依赖）会让 gateway 被 launchd / systemd 拉起后
  再死，无限循环。
- pi（`a30c2a81d`，spec §5.4 "Restarts"）：开库时每个幸存的 `running` 任务 `restarts + 1`；任务自己提交了新状态
  （新 checkpoint、等待、睡眠、结局）就清零；正常关闭不计数。到 `restartLimit`（默认 5）就停成 `blocked: restart_limit`，
  报告一次，`Harness.retryTask(id)` 手动放行。被停住的 run 任务让 conversation 保持忙。
- komo 落法：`TaskRecord` 加 `restarts`（serde 默认 0，旧记录照读），在开库对账里加一；超限的任务进
  `komo intervention list`（现在列 faulted 任务的那张清单），`komo intervention answer` 或新子命令放行。
  要先确认 gateway 的正常关闭会把 `running` 写回 `pending` 而不计数（对应 pi 的"close is not a crash"）。

## 7. 退役 document 的值（P3）

- store 里所有 document 读都带 `retired_seq IS NULL`（`komo-store/src/durable/storage.rs:811`、`:836`、`:862`），
  komo 又不做按 `asOf` 的 fork（`docs/storage-durable.md` §13.2 第一条偏离），所以**退役后的 `value` 没有读者**，
  只在 purge 整个 Session 时才删。
- 最大的一类是 codemode 的 `komo.nested`：成员里整值存子调用拿到的正文，不设上限（§13.2"嵌套结果整值存在
  `komo.nested` 成员里"）。
- 改法：`RetireDocument` 在同一条 UPDATE 里把 `value` 置成 `'null'`（行留着：`id` 仍占号，`retired_seq` 仍在，
  地址冲突判断不变）。终结 task 的 memo（codemode 时钟 / 熵最坏几百 KB）先不动：task 记录是恢复与 intervention 的依据，
  清之前要查清读者。
- 先在真库上看收益：`SELECT kind, SUM(LENGTH(value)) FROM documents WHERE retired_seq IS NOT NULL GROUP BY kind`。

## 8. 不做

- **Chord delta**（document 存"基值 + 操作增量"，pi 的 `@earendil-works/chord/delta` 与 spec §3.5）：2026-10-26 再评估，维持设计
  §2 / §3.3 的"不搬"。它在 pi 里解决四件事，komo 只沾一件：

  | pi 用它解决的 | komo |
  |---|---|
  | 高频小改的写放大 | 只有 `komo.live`（部分回复、输出尾巴）是高频大改；其余 document 小或只写一次（`komo.nested` 每个成员只写一次） |
  | document 历史与 `fork: asOf` | fork 只需要 `komo.agent`，D5 在 fork 时直接复制；没有回看旧值的需求 |
  | 跨进程副本、慢订阅方折叠 | 只有 Gateway 一个进程开库；SSE 的回复增量由 feed 自己算（`AssistantDelta` 取与上一份部分回复相比的后缀） |
  | Chord 对象模型与热重载 | 无关 |

  代价是移植操作编码、回放、基值 / 增量存储与 `checkpointWhen`，所有 document 读改成"读基值再回放"，conformance 跟着扩——换来的只是
  `komo.live` 一份 document 的写入量。这一份走 §4 的路线：先测；事务数是大头时用 deferred 提交（pi 的事务数下降来自它，不是 delta）；
  真要省字节时只给部分回复文本做追加段或拆 document。**重新考虑的条件**：§4 的三步都做了 `komo.live` 仍是瓶颈；出现回看 document
  历史或按分叉点 fork document 的真实需求；komo 要支持多进程或远端副本（例如跨设备同步会话）。
- **组提交**（多个提交闭包合成一次 fsync）：闭包读的是已提交状态（`Tx` 经 `Storage` 读），合批后后一个闭包看不到
  前一个暂存的写，要改 `Tx` 的读语义。只有 §2.3 或多会话并发测出提交线排队时再考虑。
- **去掉 `claim` 的五表 ID 查询**（`komo-store/src/durable/write.rs` 的 `owner_of`）：每条写五次主键查找，和一次
  fsync 比可以忽略；它挡的是调用方自带 ID 的错误。
- **删低选择性的 task 索引**（`tasks_by_kind` / `_by_abort_requested` / `_by_background`）：都有 `scan_tasks` 的过滤在用，
  写放大没测出问题前不动。
- **pi 的 sleeping 状态**（`f75b3240a`）：pi 为了让 Durable Object 之类的宿主睡眠时不占调用、能被外部唤醒。komo 的
  `retry` / `probe` 已经把截止时间写在 checkpoint 里（重启后照样接着睡），睡眠期间只占一个 tokio 任务（`runtime.rs:125`），
  Gateway 是常驻进程，没有对应的收益。
- **`afterFailedRun`**（`a30c2a81d`）：komo 在 D5 决定 8 已经定了"非正常收尾后排队的输入接着放置"，相当于 pi 的
  `continue` 且覆盖面更大（含取消与 faulted），不改。
- **只读打开 Harness**（`be14d9732`）：komo 只有 Gateway 一个进程开库，不适用。
- **工具 handoff 续跑**（`38ff99177`）不是存储优化，作为新功能另写在 [`handoff.md`](handoff.md)。
- **遥测**（`48909d987` / `2ec55d808`）：按任务阶段组织的 span 与存进记录的 trace 上下文，写在 [`observability.md`](observability.md) §3；
  本文 §9 要的提交耗时与事务率由那边 §9 第 6 步的存储指标提供。

## 9. 测量与顺序

1. 本机（macOS）跑 D0 spike 的三档同步（§2.3），同时测 §4.3 的 WAL 增长与事务率。
2. 做 §2（小改动，正确性）。
3. 做 §1；验收用 §1.4 的等价性质测试与持线时间测试。
4. 做 §3，放宽 `READ_PERMITS`，重测 D5 W5 那张表。
5. 做 §5（独立于存储，可与 3、4 并行）。
6. 视测量结果决定 §4 走 deferred 还是拆 document；§6、§7 排在之后。

每一步结束 `cargo test --workspace`，数字与决定写回 `docs/storage-durable.md` §10 / §11 / §13。

## 10. P1 实施记录（2026-10-26）

- **范围读取与缓存**：`komo-runtime/src/durable/context.rs` 在线上捕获 tail 与 head，在线外分页读取固定范围；
  生成、超长救援、压缩与 gateway 的 `project_update_for` 共用调度器缓存。普通追加只扫描新增 entry、复用已有贡献，
  只重排最后一条 assistant 之后的工具结果；edits 重推已有范围，head 改变时重扫活动范围，历史 cutoff 在同一 head 下截缓存。
- **全历史账本**：kernel 新增 `OnlineFold`，冷读时折一遍全历史，之后只续折新增 entry。它的 tail 独立于上下文的历史 cutoff，
  `refused_after` 在读取时按最后一条解除拒绝的 entry 叠加；压缩保留前缀缓存债，`/new` 归零。`cache_cold` 只看活动范围。
- **缓存生命周期**：读者共享 `Arc<ContextView>` 与 `Arc<Vec<EntryRecord>>`；后者替代方案里的 `Arc<[EntryRecord]>`，
  让没有旧快照读者时的追加能原地扩容，有读者时才复制。每分钟清理空闲满 10 分钟的 conversation，忙碌或仍在读取的保留；
  purge 同步移除缓存并使在飞读取失效，读取故障仍让提交线失败。
- **编排读历史**：`fork_seed` 读取最新 entry，再扫描本轮 assistant 到 tail 的范围。这里还要识别本轮已完成的调用，
  两次点查不够。任务进度仍需完整 transcript 确定 run 边界，但 `TaskRuntime::entries` 也改成线上只定 tail、线外分页。
- **macOS 同步**：写连接无条件启用 `PRAGMA fullfsync = ON`，Apple 平台测试读回 1；进度提交仍用 OFF。
  真盘微基准放在 `schema.rs` 的 ignored 测试 `measure_sync_modes`，复跑：
  `cargo test --workspace --lib measure_sync_modes --release -- --ignored --nocapture`。

验收覆盖随机序列的增量 / 全量上下文等价（12 个种子、每个 80 步，含 fork、head、edits、历史 cutoff）、
未来 edit 目标、多消息 entry、跨压缩账本与可变拒绝标记、`/new` 的缓存冷暖、快照复用、10 分钟淘汰、purge 与存储故障。
一万条历史的分页读取被刻意暂停时，另一个 conversation 的提交和上下文读取仍完成；暂停期间的新 entry 不进入已捕获范围。
完整测试、编译计时与同步基准数字记在 `storage-durable.md` §10 / §13.4。

**后续范围**：§3 的 SSE / run 投影缓存、§5 的流式快照尚未实施；§4 的真实 `komo.live` 长回复 / 并发 WAL 测量也还没有。
§6、§7 继续排在这些之后。此次微基准只比较同步档位，不能据此决定 deferred 提交或放宽 `READ_PERMITS`。
