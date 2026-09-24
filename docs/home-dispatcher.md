# Home 会话改为任务分发器

> 状态：设计，待确认（2026-09-24）。与 `docs/komo_bot.md` §11.2 的“操作者私聊落到同一个
> home session”相冲突的地方，以本文为准，落地时回写 §11.2。

## 1. 问题

§11.2 让操作者所有私聊（飞书 / Telegram / WeChat / TUI 的 `komo home`）落进**同一个
home session**，每条消息是这个会话里的一条 Run。两条既有规则叠在一起就出事：

1. **同一 Session 严格串行**（`crates/komo-store/src/repos/queue.rs` 的 `CLAIM_SQL`）。
   2026-09-23 17:56 从 Telegram 发的“查看状态”被模型做成一次 4 分钟以上的排查（其中
   两条带 `sleep` 的采样 shell 各跑了 100 s、75 s），这期间任何渠道再发的消息都排在它后面。
2. **历史只增不减**。home 会话已有 1084 条事件，空调、热水器、清理编译缓存、优化 skill
   混在一起；“查看状态”这种短句被理解成“接着查上一次没查清的疑点”。

实测数据：排队 0.1 s（不是并发名额的问题），模型每轮 2–10 s（正常），慢在任务本身、
以及一个慢任务堵住了整条私聊通道。

## 2. 目标形状

```text
tg / 微信 / 飞书私聊、komo home
        │
        ▼
home session（分发器）            一轮就结束：判断、派发、或直接答一句
  输入：这句话 + 任务看板（进行中与最近的任务：短号、标题、状态、一句话结果）
  工具：dispatch（新任务）/ follow（追问某个任务）
        │
        ├── 新任务 → 新建任务会话，立刻回“已派出 #3f2a：查空调状态”
        ├── 追问   → 投进那个任务会话
        └── 闲聊、问进度、不需要工具的问题 → 直接回答
        │
        ▼
任务会话 #3f2a、#91c0 …           各自独立的 Session，并发受调度器名额限制（现为 4）
  用正常的工作 Profile 跑；结果投回**消息来源的渠道**；审批照 §11.4 投来源 + home chat
```

已定的四个选择（2026-09-24）：

| 问题 | 选择 |
|---|---|
| 简单问题 | 分发器能答就答；需要工具的一律派任务 |
| 任务粒度 | **每个任务一个 Session**，追问进同一个 |
| 结果投递 | 发回消息来源的渠道 |
| 旧 home 历史 | 原会话改当分发器：追加一条 `conversation.boundary`，旧历史保留可查、不再进上下文 |

**为什么不用 `delegate`**：子 Run 与父在**同一个 Session**，父还要 `waiting + dependency`
等子跑完（`executor/mod.rs` 的 `delegate()` 返回 `RoundStop::Dependency`），home 照样被
堵住。任务必须在独立 Session 里，才真正并发，也才能 `komo resume <任务>` 在终端接着看。

## 3. 身份：谁以什么 Profile 跑

- **home 会话的 `agent_id` 不变**（仍是 `default_agent`，`main_session()` 找到的那一个），
  会话 id 不变，只是**它的 Run 用分发器 Profile 冻结**。
- 新配置段（热重载，§3）：

  ```toml
  [home]
  mode       = "dispatch"     # "session"（旧行为） | "dispatch"
  dispatcher = "dispatcher"   # 分发器用的 Profile
  worker     = "assistant"    # 任务会话用的 Profile；省略 = default_agent

  [agents.dispatcher]
  model        = "fast"                  # 便宜、低延迟的别名
  tools        = ["dispatch", "follow"]  # 没有 shell / read / python
  instructions = "..."                   # 见 §6
  ```

- `freeze_run`（`service/state.rs`）多一条：Session 是 home（`kind = main` 且
  `origin = home`）且 `mode = dispatch` → 取 `dispatcher` Profile；否则照旧。
- 任务会话：`SessionKind::Task`（`crates/komo-store/src/models/session.rs` 已定义、尚未使用），
  `agent_id = worker`，建会话时写死（`ensure_owned_in(..., SessionKind::Task)`）。

**不走“把 `default_agent` 改成 dispatcher”这条路**：main session 按 agent 一个，改默认值会
得到一个新的 home；TUI、HTTP 新建会话、群聊、Cron 的兜底身份也会一起变成分发器。

## 4. `dispatch` / `follow` 两个操作

### 4.1 语义

| 操作 | 参数 | 做什么 | 返回给分发器 |
|---|---|---|---|
| `dispatch` | `task`（自包含的任务描述）、`title`（≤ 30 字） | 建任务会话，以 `worker` 身份提交 `task` 为第一条输入 | `已派出 #3f2a：<title>` |
| `follow` | `task_id`（短号）、`text` | 把 `text` 作为新输入提交进那个任务会话 | `已转给 #3f2a` / `#3f2a 正在跑，这句排在它后面` |

两者都**不等任务结果**：提交成功就收尾。分发器的 Run 因此一轮就结束，home 不会被堵。

### 4.2 放在哪一层

工具只有 `ToolContext`，没有 Ledger 也没有 Gateway 句柄，`komo-runtime` 不依赖 Gateway。
所以照 `delegate` 的样子走**编排操作**，新增一个接缝：

```rust
// komo-kernel/src/traits.rs（所有 trait 都在 kernel，AGENTS.md）
#[async_trait]
pub trait TaskSpawner: Send + Sync {
    /// 建一个任务会话并提交第一条输入。结果投给派它的那条 Run 的来源渠道
    /// （`from` 的 `runs.peer`）。`request_key` 由 executor 按 `dispatch:{run}:{call}`
    /// 拼好传进来——幂等键落到 `AcceptInput::request_key` 上，账本按它去重。
    async fn spawn(
        &self,
        from: &RunId,
        request_key: RequestKey,
        spec: TaskSpec,
    ) -> Result<TaskHandle, SpawnError>;
    /// 往已有任务会话里再提交一条输入。`task_id` 是模型给的短号原文（不是已经解析出来
    /// 的 `SessionId`）——只有 Gateway 查得到"这个 home 名下有哪些任务会话"，解析这一步
    /// 因此在实现里做，不在 kernel/executor 这一侧。
    async fn follow(
        &self,
        from: &RunId,
        request_key: RequestKey,
        task_id: &str,
        text: &str,
    ) -> Result<FollowOutcome, SpawnError>;
}
```

- `Operation::Dispatch { task, title }` / `Operation::Follow { task_id, text }` 进同一个
  决策入口（§7.1），executor 在 `Operation::Delegate` 旁边分流，**授权之后**调
  `TaskSpawner`，立即以普通工具结果收尾（不返回 `Dependency`）。
- Gateway 实现 `TaskSpawner`（`crates/komo-gateway/src/service/tasks.rs`），复用已有原语：
  `SessionId::new_at` → `session::ensure_owned(worker, Task)` → `ledgers.open(&s, "task:{home}")`
  → `session::set_title_if_empty` → `GatewayState::submit(&s, key, task, Some(origin_peer),
  None)`。`submit` 会按 `worker` 冻结身份，并挂上把结果投回 `origin_peer` 的 watcher
  （`run_watch.rs`）。`origin_peer` 读父 Run 的 `runs.peer`，用 `ChannelPeer::parse`
  （§8 Fix 1 加的那个反解）解回 `ChannelPeer`。
- `request_key = "dispatch:{run}:{call}"` / `"follow:{run}:{call}"`：重放同一次调用只建
  一个任务（§8.5 的幂等）——executor 侧不再区分"首次"与"续跑"两条分支：`spawn` /
  `follow` 本身按请求键幂等，续跑（`start_call` 已经写过、还没写结果）时直接把结果落回
  那次尝试，不重新授权、不重新 `start_call`。
- Policy：两个操作在 strict / auto 下都 **Allow**——它们只是"开一个会话、提交一句话"，
  任务里的每一次调用照常各自过 Policy 与审批，不放宽任何一层。
- 恢复：`recovery = VerifyTarget`，与 `delegate` 同类；`Operation::Dispatch` /
  `Operation::Follow` 和 `Operation::Delegate` 一样在核对梯子之前就被分流，不会出现
  "结果不明"。
- `follow` 的短号解析不到 / 有歧义时，`TaskSpawner::follow` 返回
  `SpawnError::UnknownTask` / `SpawnError::Ambiguous`，executor 按普通工具失败收尾
  （`tool.started` + `tool.result: failed`）。这与 `delegate` 的 `resume` 校验不同——
  `resume` 在 `prepare` 之后、`start_call` 之前就挡下去（§4），因为账本能在 kernel 侧
  查到；短号要不要对应一个真的任务会话，只有 Gateway 的存储层答得出来，所以这一步放不
  到 `start_call` 之前，是一次刻意的取舍。

### 4.3 任务短号

短号 = 任务会话 id 末 4 位十六进制（UUIDv7 的随机段），看板渲染与解析都用它；同一 home 下
进行中与最近 N 个任务里撞号时，渲染自动延长到 6 位。不加列、不加表。

## 5. 任务看板：分发器看见什么

分发器的 `ContextInput` 多一块**任务看板**，走这次重构出来的边界：

```text
context_sources（Gateway，I/O）  查 home 名下的任务会话：kind = task、origin = task:{home}
                                 → 进行中的全部 + 最近完成的 N 个（默认 10、24 小时内）
                                 → 每条：短号、标题、状态、等待原因、最后一条回复的首行
komo-agent（纯）                 ContextInput.tasks: Option<TaskBoard> → 渲染成系统提示里的一段
```

- 渲染位置在 skills 之后、记忆之前（记忆仍在最后，保持前缀缓存，`docs/agent.md` §10）。
- **空看板也渲染一段**（落地决定，2026-09-24）：`## 任务看板\n（还没有任务）`，不是整段
  消失——分发器由此知道“现在没有任何任务”，不必去猜是工具没接上还是真的没有。`tasks`
  只在**这条 Run 用分发器 Profile 冻结**时才是 `Some`（哪怕是空看板）；其余会话恒
  `None`，提示因此逐字不变（golden 不受影响）。
- **状态/等待原因从 `runs` 表现读**（索引化的单行 / 按会话过滤读取），只有“最后一条回复
  的首行”走 `Ledger::run_end`——它按 `final_event` 定位那条事件附近的一页并把外置正文
  按引用读回来，不是 `http::sessions::summary_of` 那种“整段 JSONL fold 一遍”的读法
  （`crates/komo-gateway/src/service/context_sources.rs` 的 `task_board`）。“最近完成”的
  候选池先按会话 id（创建时间）截一批（`TASK_BOARD_RECENT_LIMIT * 5`），避免为一个用了
  很久的 home 读遍它全部历史任务会话的 `runs` 表。
- **可重建性**（`docs/agent.md` §9，落地决定，2026-09-24）：看板在**受理这一刻**（不是
  “这一段第一次装配”）算好、序列化进 payload，冻结进 `RunSnapshot.dispatcher_tasks_ref`
  ——与 `instructions_ref` 同一个字段、同一条理由（§4.3）：审批可能很久之后才答复，那时
  任务会话的状态早就变了，恢复出来的 Run 不能因此看见另一份看板。是否要带看板（即“这条
  Run 是不是分发器 Profile”）与 Profile 换成哪一份是**同一次判断**（`freeze_run` 里的
  `is_dispatcher_run`），不是续跑时重新猜一遍 `is_home_session` + `[home] mode`——那样在
  “校验通过之后配置又被改坏”的窗口里会算出两个不一致的答案，[home] mode 热重载也不该
  改变一条已经在跑的 Run 看到的看板。
  没有走**检查点**：`checkpoints` 表目前在生产代码里只有读（`recall_for` 读
  `CheckpointStore::latest`），没有任何写者把 `CheckpointRecord` 落盘——`MemoryUse` 的
  “记checkpoint”那条路本身还没接上。在这条基础设施补上之前，比照它验证不了“同一种机制”
  是不是真的可靠；而分发器的两个工具（`dispatch` / `follow`）在 §4.2 定成**恒 Allow**，
  从不产生审批等待，所以“审批很久之后才续跑、看到另一份看板”这个场景对分发器 Run 根本
  不会发生——唯一会让同一条 Run 重新装配一次的路径是模型调用本身的可恢复重试
  （`WaitReason::Retry`），这时候还没有任何工具调用被观察者依据旧看板做出决定，重新走
  一次“受理时刻”的账（而不是“上一次装配”的账）已经足够。因此选**受理时冻结**而不是
  “检查点”：机制更简单（复用已经在用、已经测过的 `RunAccepted.snapshot` + payload 引用
  这条路），且不需要先把检查点的写路径补起来。
- 任务会话的 origin 写成 `task:{home_session}`，看板查询只按它过滤，不需要新列。

home 自己的回放窗口不变（`docs/agent.md` §8）：历史 Run 只留“用户说了什么 + 分发器最后答了
什么”。一次交互两条消息，增长很慢；`/new` 照旧可用。

## 6. 分发器的提示

写进 `[agents.dispatcher] instructions`（运营者可改，热重载），默认内容的要点：

1. 你是分发器：需要读文件、跑命令、查设备、写东西的，**一律 `dispatch`**，不要自己做。
2. 用户在追问某个进行中或刚完成的任务（看板里有），用 `follow`，不要另开。
3. 闲聊、问“那个任务怎么样了”、不需要工具就能答的问题，直接答一两句。
4. `dispatch` 的 `task` 要自包含：把用户的原话和看板里相关任务的结论写进去——任务会话
   看不到 home 的对话。
5. 每次回复都短：派出去就说“已派出 #xxxx：标题”，不要复述任务。

一份可以直接抄进 `config.toml` 的样例（`worker` 省略时会退回 `default_agent`，这里写全）：

```toml
default_agent = "assistant"

[agents.assistant]
instructions = "你是助手：先读，再答。"
tools = ["read", "rg"]

[agents.dispatcher]
model        = "fast"                  # 便宜、低延迟的别名；具体选哪个见 §11 的待定项
tools        = ["dispatch", "follow"]  # 没有 shell / read / python，动手的事一律派出去
instructions = """
你是 komo 的分发器：只做判断和派活，不自己动手。

1. 需要读文件、跑命令、查设备、写东西的，一律用 dispatch 建一个新任务，不要自己做。
2. 用户在追问某个进行中或刚完成的任务（任务看板里能看到），用 follow 接着那一条，不要
   为同一件事另开一个任务。
3. 闲聊、问"那个任务怎么样了"、不需要工具就能答的问题，直接答一两句，不要派任务。
4. dispatch 的 task 参数要自包含：把用户的原话、以及任务看板里相关任务已经查到的结论都
   写进去——任务会话看不到这段 home 对话，写漏了它就无从查起。
5. 每次回复都短：派出去就说"已派出 #xxxx：标题"，不要复述任务内容，也不要猜任务多久
   能跑完。
"""

[agents.worker]
instructions = "你是任务执行者：把交给你的这一件事做完，查不到就如实说。"
tools = ["read", "rg", "shell"]

[home]
mode       = "dispatch"
dispatcher = "dispatcher"
worker     = "worker"          # 省略则任务会话用 default_agent
```

## 7. 确定性路由（不经模型）

Dispatcher（`crates/komo-gateway/src/dispatcher.rs`）在 `submit` 之前先看（落地，2026-09-24）：

- 只在**私聊**（`msg.is_private`）且 `[home] mode = "dispatch"` 时生效；群聊、会话模式
  一个字不改。
- 文本以 `#3f2a `（4–6 位十六进制 + 空白 + 非空正文）开头 → 在这个 home 名下的任务会话里
  按短号解析（`service::tasks::resolve_unique_task`，与 `follow` 共用同一份匹配
  `candidates_for`，不能有第二种口径）；**唯一命中**才直接把去掉短号的正文
  `state.submit()` 进那个任务会话，用消息的 `peer`，**不起分发器 Run**。
- 解析不到 / 有歧义 → 一律落到正常提交（交给分发器，它会看到看板，自己判断或问回去）；
  这一层不单独提示“有歧义”，那句话留给分发器自己接的 `follow`（模型侧的歧义提示，§4.2）。
- 回复某条任务消息即追问（Telegram `reply_to_message`、飞书引用）需要“投递消息 id → 任务
  会话”的映射，属于后续阶段（§9 Phase 4）。

## 8. 顺带要修的两处

1. **重启后聊天 Run 的回复会丢**。watcher 只在内存（`run_watch.rs`、`start_watching`），
   重启后没有代码按 `runs.peer` 重新挂上；审批有周期补投，最终回复没有。任务比现在的
   私聊 Run 跑得更久，更容易跨重启——**这是分发器的前置条件**：启动时对所有未终态、
   `peer` 非空的交互 Run 重新 `watch_interactive_run`。
2. **`ambient_identity` 忽略会话的 `agent_id`**，总用默认 Profile（`context_sources.rs`）。
   任务会话一律走 `submit`（带冻结快照）就碰不到它；但兜底路径应改为先看会话的
   `agent_id`，否则一条没有快照的 Run 会在任务会话里以错误身份跑。

## 9. 分阶段

| 阶段 | 内容 | 验收 |
|---|---|---|
| 0 | §8 两处修复 | 交互 Run 跑到一半重启 Gateway，完成后回复仍投到来源渠道，且只投一次；没有快照的 Run 在绑定了 `worker` 的会话里以 `worker` 身份跑 |
| 1 | `[home]` 配置与校验、`freeze_run` 分流、`dispatcher` Profile。切换到分发器模式没有专门的 CLI 命令：用 `/new`（对话边界）另起一段，旧 home 历史保留可查、不再进上下文 | `mode = session` 时行为与现在逐字相同（沿用 context golden）；切到 `dispatch` 后 home 的新 Run 用分发器身份，工具只有两个 |
| 2 | `TaskSpawner` 接缝、`Operation::Dispatch/Follow`、任务会话（`SessionKind::Task`、`origin = task:{home}`、标题）| 从 tg 发“查空调状态”：home Run 一轮收尾并回“已派出 #xxxx”；任务会话以 `worker` 身份跑，结果投回 tg；同一调用重放只建一个任务；任务里的审批投 tg + home chat |
| 3（已完成，2026-09-24） | 任务看板（context_sources 取数 + `komo-agent` 渲染 + 冻结）、`#短号` 确定性路由、`komo session list` 显示 kind / 所属 home | 一个 4 分钟的任务在跑时，从微信再发“1+1 等于几”几秒内得到回答；`#xxxx 再看看功耗` 不经模型进对应任务；看板里能看到进行中任务的等待原因 |
| 4（后续）| 回复消息即追问；长任务进度提示（超过 N 秒回一句“还在查：……”）| — |

## 10. 不做的事

- 不改 Session 串行规则：分发器一轮就结束，串行不再是问题；任务之间本来就是不同 Session。
- 不引入任务队列 / 任务表：任务就是 Session，状态就是它当前 Run 的状态，看板是查询。
- 不让分发器等任务结果再转述：结果由任务会话的 watcher 直接投回渠道，分发器只在被问到时
  从看板读一句。
- Cron 不经分发器：Cron 本来就是每次触发一个新会话（`scheduler/cron.rs`）。

## 11. 待定

- 分发器模型：需要一个低延迟、会用工具的别名；`config.toml` 里现有哪些别名适合，落地时实测一轮耗时再定。
- ~~看板里“最近完成”的条数与保留时长~~（Phase 3 落地，2026-09-24）：定为 10 条 / 24 小时，
  暂不可配置——`TASK_BOARD_RECENT_LIMIT` / `task_board_recent_window()`
  （`crates/komo-gateway/src/service/context_sources.rs`）。要做成可配置的话按
  `[home]` 的口径加两个字段，属于后续小改动，不影响这次的边界。
- ~~任务会话多了之后 `komo session list` 是否默认隐藏已完成的任务会话~~（Phase 3 落地，
  2026-09-24）：**不隐藏**——`session list` 一贯的默认（§8.10）已经是“只藏逻辑删除过的”，
  任务会话不因为“是任务”就多一层过滤；改用一个 `任务·#短号` 前缀标出哪些行是任务
  （`crates/komo-client/src/render/mod.rs`），操作者用它跟看板对号。看板本身已经只挑
  “进行中 + 最近 10 条”，长期堆积的旧任务会话不会出现在分发器的提示里，只会留在
  `session list` 的完整清单中——如果这份清单将来因为任务多起来变得不好读，再单独做
  `--all` 那道口子。
