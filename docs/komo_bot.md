# Komo：Rust 个人 Agent 框架设计

版本：v0.8 · 日期：2026-09-15 · 状态：设计稿，尚未实现。

本文定义一个全新项目。Komo 是程序名称；架构只依据本文的需求与约束。

v0.8 相对 v0.7 的变化：状态数据库改为 Turso + toasty（§8.2）；工程结构改为七个 crate（2026-09-22 抽出 `komo-agent`），编译预算列为验收项（§13.4）；飞书 / Telegram / WeChat 聊天入口与 TUI 进入首版，审批主要在聊天里完成（§11）；新增 Skills（§5.6）；补齐 trait 清单（§13.5）；渠道的身份匹配（谁是操作者、哪个会话是 home chat）只放 config.toml / .env，不进数据库（§11.2）；配置热重载，改 config.toml / .env / policy.toml 不重启 Gateway（§3）。全新实现，不迁移旧代码与旧数据。

## 1. 已确定的范围

**一个 Rust 程序、六个基础工具、一套可恢复的 Agent Runtime。**

| 项目     | 决策                                                                                             |
| -------- | ------------------------------------------------------------------------------------------------ |
| 部署     | Fedora Server 与 macOS 均可独立运行，每个实例保存自己的数据                                      |
| 进程入口 | 单个可执行程序 `komo`，包含 TUI 聊天客户端和 Gateway                                            |
| 聊天     | `komo` 直接进入 TUI 聊天；Gateway 未运行时自动启动                                                |
| 执行方式 | LLM 原生 tool call，由 Rust 校验、审核和执行                                                     |
| 基础工具 | 严格保留 `read`、`write`、`edit`、`shell`、`python`                                              |
| 能力扩展 | Python 模块保存到 toolbox，AI 可以编写、测试和迭代                                               |
| 程序性说明 | Skills：人写的 `SKILL.md`，模型用 `read` 读取，不增加工具                                       |
| 定时任务 | Cron 触发同一套 Agent Runtime                                                                    |
| 持久化   | 每个 Session 一个数据目录，集中保存 JSONL、大正文、tool output 和产物；Turso（MVCC）保存调度状态、授权与索引 |
| 操作审核 | 统一 Policy；需要人工审核时在飞书 / Telegram / WeChat 里回答，TUI 与 CLI 为辅                    |
| 自动记忆 | 自动积累偏好、事实和经验，区分用户陈述、工具观察、模型推断与用户确认                             |
| 主动记录 | 用户明确要求保存的内容写入现有 Memos，原文以 Memos 为准                                          |
| 记忆检索 | 关键词与向量混合检索，索引保存在本机，可重建                                                     |
| 模型配置 | 主模型、记忆整理模型、记忆向量模型分别配置；统一提供 effort 并按模型能力校验                     |
| 聊天入口 | 飞书、Telegram、WeChat 首版都在，复用 Gateway 的会话、运行与审批接口；每个渠道一个 feature       |
| 工程结构 | 七个 crate 按依赖重量与变更频率划分；冷编与增量编译时间是验收项                                  |

代码优化、HA、网页搜索、记事是工具组合的使用场景，不成为专用的 Rust 工具或 Runtime 业务类型。

首版保持单操作者、单实例运行方式，不引入实例同步、跨机器调度、外部消息队列或独立向量数据库服务。支持向量检索不要求部署额外服务。

## 2. 总体结构

```text
komo TUI / resume / 操作命令          飞书 ws · Telegram 轮询 · WeChat iLink
              │                                     │
         HTTP / SSE                          Channel → Dispatcher
              │                                     │
        Gateway 常驻进程 ◀───────────────────────────┘
              │
     ┌────────┼───────────┐
     │        │           │
 Session   Agent Runtime  Cron Scheduler
  Store       │           │
     │        └── 同一个 Run 提交入口
     │
     └── JSONL 事件 + 独立 tool output + state.db 状态 / 索引
                    │
                Agent Loop ↔ LLM
                    │
               Tool Executor
                    │
                 Policy
             ┌──────┼──────┐
           Allow   Ask     Deny
             │      │       └── 明确的拒绝结果
             │      └── 保存审批，暂停，等待回答
             │
      read / write / edit / shell / python
                                      │
                               已保存的 toolbox
```

MemoryManager 在 Run 开始时装配相关记忆，在 Run 完成后异步整理新证据。它管理本机记忆与检索索引；访问 Memos 仍通过已审核的 Python 模块，不新增模型工具。

Gateway 持有模型连接、数据库、Session JSONL 写入器、工具环境、运行状态和三个聊天渠道。TUI 负责提交输入、显示进度、回答审批和恢复会话；聊天渠道做同样的事，并且是审批的主入口（§11）。

每个实例的 Gateway 自己执行任务。CLI 从 Mac 连接 NAS 的 Gateway 时，命令与 Python 都在 NAS 虚拟机中执行。

## 3. 命令与 Gateway 生命周期

以下是拟定命令，不代表已经安装或实现。

| 命令                                         | 行为                                                                        |
| -------------------------------------------- | --------------------------------------------------------------------------- |
| `komo`                                       | 确保本机 Gateway 就绪，进入 TUI 聊天。**会话在第一条消息送出之前才创建**——空跑一眼就退出，不在账本和磁盘上留下空壳 |
| `komo resume SESSION_ID`                     | 连接原会话并查看进度或处理待办；恢复调度由 Gateway 自动进行，不重复创建 Run |
| `komo session list`                          | 查看会话列表和状态；`--all` 才列出已逻辑删除的会话（§8.10）                  |
| `komo session delete/purge SESSION_ID`       | 逻辑删除（`closing` → `deleted`）与回收（`purged`）。**删除只有这一条路**：`--now` 把未完成 Run 各写一条明确取消，`purge` 前先算引用（§8.10） |
| `komo run inspect RUN_ID`                    | 查看执行过程、工具结果与产物                                                |
| `komo run cancel RUN_ID`                     | 请求取消运行                                                                |
| `komo gateway`                               | 启动后台 Gateway，等待就绪后返回                                            |
| `komo gateway --foreground`                  | 前台运行 Gateway，供服务管理器及调试使用                                    |
| `komo gateway status/stop/restart`           | 管理后台进程                                                                |
| `komo intervention list/show/answer`         | 待处理清单：审批、结果不明、阻塞三类一张表；等价于聊天里的 `/pending` 与 `/answer`（§7.5、§11.3） |
| `komo cron add/list/run/pause/resume/remove` | 管理定时任务                                                                |
| `komo memory list/search/show`               | 查看自动记忆、来源与确认状态；search 支持 hybrid / keyword / vector         |
| `komo memory confirm/forget`                 | 确认具体版本或停用自动记忆；不删除 Memos 原文                               |
| `komo memory index status/rebuild`           | 查看索引覆盖率或重建当前向量索引                                            |
| `komo config check`                          | 校验各模型、effort、向量参数、渠道名单及其他配置；只读，不改变运行中的 Gateway |
| `komo config reload`                         | 让 Gateway 立即重载配置（等价于文件保存后的自动重载或 `SIGHUP`）；校验失败则保留旧配置并返回错误 |
| `komo channel list/probe`                    | 渠道清单与连通性核对（飞书 tenant token、Telegram `getMe`、微信凭证文件）；不经 Gateway |
| `komo channel wechat login`                  | 终端显示二维码完成微信登录，凭证写入数据目录                                |
| `komo skills list/inspect/enable/disable`    | Skills 目录（§5.6）；只读文件系统，不经 Gateway                             |
| `komo toolbox list/inspect/test/enable [--version]/disable` | toolbox 模块（§5.3–5.4）；**经 Gateway**——启用是一次审批，审批只有 Gateway 打得开 |
| `komo update`                                | 从 GitHub release 换掉当前这个可执行文件（§13.6）。**不经 Gateway**：换的是磁盘上那份二进制，不是在跑的那个进程 |

聊天启动顺序：

1. 读取当前实例的连接配置和发现文件。
2. 检查 Gateway 健康状态与实例身份，不能只凭 PID 或端口判断。
3. 本机实例未运行时，请求系统服务管理器启动。
4. 等待服务就绪，超时则返回具体诊断信息。
5. 建立聊天连接。**新会话在这里还没有创建**：它在第一条消息送出之前才建，订阅也才在那一刻建立（`komo resume` / `komo home` 手里本来就有会话，照旧先补读历史再订阅）。

Gateway 获得数据目录进程锁并完成存储校验后，自动扫描未完成运行；不必等用户打开 CLI 或发送 resume。恢复与新请求共用调度器，恢复扫描本身不等待全部旧任务完成才提供服务。这次扫描就是 §8.9 的 reconcile：会话是否还能服务、哪条 `running` 已经没主人（租约过期或旧实例遗留）、哪条停在等待上的该放行，与 Run 自己的位置一起在下一次领取之前判定完。

**就绪不等投递补发。** 上一次没送到的投递（§11.4）在"Gateway 就绪"**之后**的后台补发，不压住第 4 步的等待：补发是网络 I/O，一条 pending 一个平台往返，积压多少条就等多久——线上十来个卡住的投递是几秒，上百条会拖过客户端的就绪超时。补发晚一步没有代价：投递记录先写后发，按 `DeliveryId` 幂等，重来一次也不会重复发。**顺序上唯一的硬要求是渠道登记之后**——渠道没登记时冲刷等于什么都没干。

Gateway 对数据目录持有进程锁。多个 CLI 同时启动时，只允许一个 Gateway 接管实例；启动失败不能通过删除仍有效的锁来强行重试。

**就绪也不等模型探测。** §9.5 那一次"省略 `dimensions` 时先探一次维度"同样是网络 I/O，同样排在"Gateway 就绪"**之后**的后台：端点慢、或者在收连接却不回话时，这一次探测要等满模型超时（本机实测一个只收不回话的端点 = 120s），压在就绪前面就是 `komo gateway restart` 看上去卡住。探测落定前检索这一侧按 §9.4 如实降级——hybrid 走关键词并带降级说明，vector-only 报**不可用**；**不是** `VectorUnconfigured`：配了 alias 只是后端还没在手上，那是"这一刻不通"，不是"配置错了"。探到维度就把向量后端装上，同一台实例的向量臂立刻可用；探不到就一直空着，与端点本来就不通时同一个行为。

**配置热重载。** `config.toml`、`.env`、`policy.toml` 改了不用重启 Gateway。三个触发方式落到同一个函数：文件 mtime 变化（每秒轮询一次，不引入 inotify 依赖，编辑器的临时文件与原子替换都覆盖到）、`komo config reload`、`SIGHUP`。流程固定：

1. 重新解析三个文件成一份**完整的**新快照，跑与 `komo config check` 相同的校验；任何错误 → 旧快照原样保留，错误写日志并投递到 home chat，重载命令返回该错误。**校验不过的配置永远不会被装上**，哪怕只错一个键。
2. 校验通过 → 原子替换进程内唯一的 `Arc<ConfigSnapshot>`（arc-swap）。**读配置的地方按用途读当前快照，不缓存**：Dispatcher 判定 Principal 时读 `allow_from` / `groups`，Notifier 解析 home chat 时读 `home_chat`，Policy 每次决策读规则，新 Run 在 `accept_input` 时抓一份模型 / effort 快照存进 `runs.config_snapshot`——所以名单改完，下一条消息就按新名单判；策略改完，下一次决策就按新规则；模型改完，下一个 Run 用新模型，**正在跑的 Run 与后台记忆任务继续用它们各自开始时抓的快照**（§13.3），不在半路换模型。
3. 逐段比对新旧快照，只重建变化了的东西：某个 `[channels.*]` 的 `enabled` 或凭证变了 → 只停掉并重启那个渠道的 `serve`（飞书重连 ws、Telegram 重新起轮询），其他渠道不受影响，未 ack 的入站消息按平台的至少一次投递重来；模型 `base_url` / 凭证变了 → 换掉对应 `LlmClient` 实例；向量模型变了 → 走 §9.5 的空间指纹与索引代次流程，不在重载里偷偷重建索引。
4. 一小组键**只在启动时生效**：数据目录 / `KOMO_HOME`、监听地址与端口、数据库路径、Python 环境根目录。这些键改了，重载照常完成其余部分，然后明确报告"以下键需要 `komo gateway restart`"，不静默忽略，也不假装已生效。

重载事件带新旧快照的差异（键名，不带值，凭证更不带）写进 Gateway 日志；`komo doctor` 显示当前生效配置的加载时间与来源文件 mtime，两者不一致就是"文件改了但没装上"，把上一次校验错误一并印出来。

Fedora 使用 systemd 管理，Mac 使用 launchd；服务管理器运行前台形式的 Gateway。**单元名是全局的（`komo-gateway.service` / `dev.komo.gateway`），只有默认数据目录 `~/.komo` 拥有它。** 别的 `KOMO_HOME` 走 `komo gateway` / `stop` / `restart` 会被**拒绝**并告诉你去前台跑 `KOMO_HOME=… komo gateway --foreground`：那条路会把单元文件改写成指向这个数据目录，现役服务随即被换掉——2026-09-21 实测过一次，`KOMO_HOME=/tmp/… komo config check` 经 `connect_or_start` 起服务，把现役单元换成了一份 debug 构建 + 沙箱数据目录。`komo gateway status` 会印出单元实际在服务哪个数据目录，与当前 `KOMO_HOME` 不一致时明说。**单元文件只设 `KOMO_HOME`（与可选的 `KOMO_LISTEN`），不加载 `.env`、不含任何凭证**：launchd 没有 `EnvironmentFile` 的等价物，只能把值抄进 plist，那是把密钥复制到第二个文件；走进程环境还会让 `.env` 的热重载失效，并让每个子进程与日志都可能看到它。`.env` 由 Gateway 自己读成 `Secrets`（§3 热重载覆盖它），需要凭证的 Python 模块按**变量名**声明（§5.3），Gateway 在起子进程时按名注入。Mac 若后续需要操作用户桌面应用，应按登录用户的执行环境配置。后台服务不会让睡眠中的电脑继续执行任务。[Fedora systemd](https://fedoraproject.org/wiki/Packaging:Systemd) · [Apple launchd](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)

默认监听回环地址。首版远程访问可通过 SSH 转发连接；直接开放网络监听时需要 HTTPS 和认证。选择远程实例时，不因连接失败而启动一个本机替代实例。`status` 与 `stop` 不隐式启动服务。

## 4. 六个基础工具

| 工具     | 输入与输出重点                                       | 执行约束                                             |
| -------- | ---------------------------------------------------- | ---------------------------------------------------- |
| `read`   | 路径、读取范围；返回文本和文件版本                   | 大文件截断，明确显示未读范围                         |
| `write`  | 路径、完整内容、可选预期版本                         | 原子替换；覆盖现有文件需检查版本                     |
| `edit`   | 路径、明确匹配内容、替换内容、预期版本               | 匹配失败返回错误，不模糊猜测                         |
| `rg`     | 正则、搜索路径、文件 glob；返回「路径:行号:正文」    | 只读，与 `read` 同一条判定（范围内 Allow、范围外 Ask）；这台机器上没有 ripgrep 就不挂这个工具 |
| `shell`  | 命令、工作目录、超时；返回退出码和输出               | 管理进程组、限制输出、支持取消                       |
| `python` | 代码或已保存模块调用；返回结果、输出和产物           | 使用受管理解释器，绑定代码版本                       |

**搜索走 `rg` 工具，不拼 shell 命令。** 两者结果一样，判定不一样：`rg` 的计划是一次**只读**动作（`Operation::ReadFile`，目标是搜索范围），于是"搜工作目录"是 Allow、"搜范围外"要问人，和 `read` 完全一致；而 `shell` 的任意命令一律 Ask——一次只读搜索每次都停下来等人，是把审批额度浪费在不需要判断的事情上。HTTP 请求通过 Python 库或命令完成；Git、构建、测试、HA 和记录查询都组合这些基础工具。

`rg` 内嵌 ripgrep 的**书库**——[`grep`](https://github.com/BurntSushi/ripgrep/tree/master/crates/grep)（匹配、搜索、打印）加 [`ignore`](https://github.com/BurntSushi/ripgrep/tree/master/crates/ignore)（遍历：隐藏文件、`.gitignore`、覆盖 glob）——**进程内跑**：不起子进程，机器上也不需要装 `rg`。要书库不要命令，是因为外部二进制要么没装、要么各家版本不同；而遍历规则本来就该用这套经过验证的实现，不该自己再写一遍。正则语法就是 Rust regex（只连 `grep-regex`，不带 PCRE2）。遍历与搜索是阻塞的，所以跑在 `spawn_blocking` 上、匹配经队列流式写进输出存储（§8.3），取消与超时靠一个停止标志让那边收手。

**委派（`delegate`）不是第七个工具，是一条编排操作。** 模型可以把一个自包含子任务交给**一条子 Run**：它以 `Operation::Delegate` 进同一个决策入口（§7.1 那一行），执行时在同一个 Session 里受理一条**子 Run**，父 Run 进 `waiting + dependency` 等它——子 Run 用的是同一套六个工具，它自己的每一次调用照常过 Policy 与审批，所以"能不能做"这一层没有被放宽，放宽的只是"这一件事由谁来做"。子代理只拿得到任务本身（不继承父的消息历史、不继承父的上下文），默认 8 轮预算，**深度只有一层**（子 Run 不能再委派）。结果可以带一个契约：子代理把结构化结果放进它最后一条回复，父侧续跑时用**同一份校验器**复验（§8.6）。

工具执行的公共能力放在 ToolExecutor：参数校验、执行计划生成、Policy 判断、审批处理、执行状态保存、取消和输出限制。

**每次运行有它自己的能力面（`AgentSurface`）。** 这次允许调用哪些工具是一个显式的集合：**交给模型的工具 schema 与执行时查找的表是同一份**，装配时由 schema 反推，所以两边不可能分家。schema 里没有的名字，执行器不认——模型自己拼出这个名字，拿到的是"这次运行的工具集里没有它"，而不是"进去试试看"。执行器手里那份全局工具目录只用于**发现与构造**（`komo skills` 的 `requires_tools` 门控也问它），**执行时一次都不回退到它**：能回退就等于能力边界只是一句建议。子代理已经是这条边界的第一个使用者（它的 schema 里没有 `delegate`，于是它也调不动 `delegate`）。

工具**能不能**调用由能力面决定；一次**具体调用**允不允许由 Policy 决定（§7）。两者不是同一件事：`coder` 有 `write`，不代表它可以写任何路径。

```rust
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError>;

    async fn execute(
        &self,
        plan: ApprovedPlan,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError>;

    /// §8.6：对 started 而无结果的调用核对目标状态。默认 Unavailable → `waiting + intervention`（等人判）。
    /// write / edit 用内容哈希覆盖它；python 交给模块自带的核对函数。
    async fn verify(
        &self,
        _plan: &ExecutionPlan,
        _ctx: &ToolContext,
    ) -> Result<Verification, ToolError> {
        Ok(Verification::Unavailable)
    }
}
```

这是接口示意，辅助类型省略。`ApprovedPlan` 只能由执行器在策略允许或有效审批后构造，字段不对外开放。prepare 可以解析参数和检查元信息，但不能通过导入未知 Python 模块、运行命令等方式提前执行未审核代码。

`ExecutionPlan` 带 `recovery: RecoveryMode`（§8.6 四种恢复方式之一）与 `plan_hash`，由 `prepare` 填写，模型参数不能指定。`ApprovedPlan` 的封闭要跨 crate 成立（类型定义在 kernel，执行器在 runtime，见 §13.4），所以靠类型状态而不是可见性：

```rust
pub struct ApprovedPlan { plan: ExecutionPlan, proof: Proof }
pub struct Proof(ProofKind);                     // 字段私有，无公开构造函数

impl PolicyDecision   { pub fn into_proof(self) -> Option<Proof> { … } }   // 只有 Allow 换得出
impl ConsumedApproval { pub fn into_proof(self) -> Proof { … } }           // 只有消费成功的审批换得出
impl ApprovedPlan     { pub fn new(plan: ExecutionPlan, proof: Proof) -> Self { … } }
```

`Proof` 只有这两个来源。测试通过 `test_support::proof()` 拿替身，feature 门控，不进正式构建。

ToolContext 由 Gateway 创建，包含 Session / Run / ToolCall 身份、工作目录、权限和取消信号，不能由模型参数提升。

shell 与 Python 子进程使用明确的环境变量集合。取消时停止所属进程组并等待回收，而不仅仅是丢弃异步等待。Tokio 默认不会因为进程 handle 被丢弃就终止进程。[Tokio process](https://docs.rs/tokio/latest/tokio/process/index.html)

### 资源命名空间：`skill://` / `tool://` / `artifact://`

模型仍然只用现有的 `read` / `rg`，把资源 URI 当路径交给它们。**URI 不是"特殊文件路径"**：它在执行计划里是一个明确的目标类型（`TargetRef`），计划同时带着**逻辑** URI 与解析出来的**真实**目标——审批与审计回答的是"允许读哪个逻辑资源"，而规则的路径匹配看的仍是真实路径。

```text
skill://<skill>/<path…>                      → 那份 skill 的目录 + <path…>（只读）
skill://                                     → 配置里的 skill 根（`rg` 的搜索根）
tool://<tool>/schema|doc                     → 虚拟入口：内容由运行时按能力面现算
tool://                                      → 这次能力面里的工具名
artifact://files/<path…>                     → sessions/<id>/artifacts/<path…>（只读；产物按 Run 分目录，所以通常是 `files/<run>/<名字>`）
artifact://<run>/<call>/<attempt>/<stdout|stderr|result>
                                             → sessions/<id>/tool-output/…（只读）
```

- **`tool://` 是说明入口，不是执行入口**：读它不改任何状态；真实动作仍走 native tool call、`ExecutionPlan` 与 Policy。
- **越权当场是错**：`..`、空段、`tool://` 里能力面之外的工具、别的会话的 `run`、没有会话目录，一律在 `prepare` 就拒绝（`ToolError::Failed` 说清原因），**不进计划就没有执行**；解析链接之后还要再核一次"仍在挂载点里面"。
- **挂载由装配决定，不能由模型参数提升**：skill 根来自当前配置快照的 `paths.skill_dirs`，会话内容目录来自这一次的 Session，`tool://` 只认这一次的 `AgentSurface`。三者随 `ToolContext` / `CallEnv` 走。
- **计划里的两种目标**：本地路径（序列化成 `{"path": …}`，与改造前逐字相同——旧审批计划仍按原哈希校验）与资源（`{"uri": …, "resolved": …}`）。规则表因此多一条 `paths = virtual`：**没有磁盘目标的读**（`tool://`）由它一条覆盖；路径那几条规则对虚拟目标一律**不命中**——给虚拟入口编一个假路径才是真的越权口子。
- **首阶段不做**：memory / session 的任意写入、shell / Python 里的 URI 展开、通用 VFS。本地路径的用法一个字都不变。

产物由运行时登记：Python 子进程拿 `KOMO_ARTIFACT_DIR`（= `<会话内容目录>/artifacts/<run>`）作为产出目录，跑完把这次新出现的文件记进工具结果的 `artifacts` 引用（**相对会话目录的路径**，落在 `output.json` 的 `body.artifacts` 里），投影那一处把它映射成 `artifact://files/<相对 artifacts 的那一段>`（映射函数在 kernel，落盘与投影共用同一个，§8.3）。工具输出那一条的引用直接由 §8.3 的投影给出（`artifact://<run>/<call>/<attempt>/stdout`），重启之后照样读得回来——它是**按引用回放**，不是把正文再抄一遍。

## 5. Python 执行与能力保存

### 5.1 执行环境

Gateway 管理一个 Python 虚拟环境；Rust 通过独立子进程调用解释器。首版每次 Python 调用创建新进程，不依赖跨调用的内存变量。

每次返回结构化结果：

```text
status / result / stdout / stderr / artifacts
```

脚本可以设置 `result` 返回 JSON 可表达的数据；普通 print 输出单独收集。解释器驱动的协议使用独立通道或明确封装，脚本输出不能混入控制协议。

长期状态写入文件；依赖按锁定清单安装。环境变更经过 Policy 并串行进行，正在使用的环境版本保持不变。依赖升级需要新的环境版本，成功后再切换后续调用。

### 5.2 两种 Python 调用形式

任意代码：

```json
{
  "mode": "code",
  "code": "from pathlib import Path\nresult = [p.name for p in Path('.').iterdir()]"
}
```

已保存模块：

```json
{
  "mode": "call",
  "module": "toolbox.ha",
  "function": "turn_off",
  "args": {
    "entity_id": "light.living_room"
  }
}
```

两种形式都属于同一个 `python` 工具。`call` 模式只加载 toolbox 中明确导出的函数，依据已审核版本、参数定义和授权范围执行。它不是直接开放 Python 任意属性查找。

任意 code 模式不会因为 import 了已审核模块而自动获得同样授权。

`mode` 是参数里的判别字段，但**模型偶尔会漏写**（真实会话里报的是 `missing field mode`，白跑一轮）。所以 Runtime 按参数形状补上：有 `code` 就是 `code`、有 `module` / `function` 就是 `call`，两个都写或都没写才报错。补进去的值落在 `plan.args` 里，**授权与计划哈希看的是补完之后的那份**——少一个判别字段不该换来一次失败往返，也不该让策略看到两种形状。

`python` 交给模型的那段正文（§8.3 的投影）按这个顺序取：错误 → 结构化结果 → **stdout 的尾巴** → "没有返回值，也没有输出"。脚本只 `print` 是常见跑法，而字面量 `null` 会让模型以为工具坏了、改用 `shell` + `python3` 把同一件事重跑一遍（真实会话里就这么白花了两轮）。完整 stdout 照旧在 `stdout.txt` 里，抬头写着它的字节数。

### 5.3 toolbox 文件布局

```text
toolbox/
├── __init__.py
├── README.md
├── ha.py
├── web_search.py
├── memos.py
├── tests/
└── .staging/
```

README 与模块说明提供用法。LLM 通过 read 查看说明，再通过 python 调用。保存工具不会扩张模型侧的六个工具 Schema。

可见的树之外还有两处**隐藏**位置（2026-09-17 落地）：`.staging/<m>.json` 存候选元数据（版本、测试结果），`.versions/<m>/{<ver>.py, test_<ver>.py, <ver>.json, enabled.json}` 存每个版本的快照与「当前启用哪个」——§5.4 要求记录实际使用的模块版本并保存快照。版本号 = 代码哈希前 12 位 + 依赖锁哈希前 8 位。`code` 模式的解释器装了一个排在 `sys.meta_path` 最前的查找器，按解析后的落点拒绝 `.staging` / `.versions` 的导入（§7.3）；它挡的是 import 这条路，`exec(open(...).read())` 不在承诺内。

HA、Memos、搜索服务地址和凭证引用通过配置传给已授权模块：模块用 `__komo_env__ = ["MEMOS_TOKEN", …]` 声明它要哪些**变量名**，变量名进执行计划（`ResourceRef`），Gateway 起子进程时从 `Secrets`（`.env`）按名解析注入，`.env` 改了下一次调用即生效；解析不到就不设置并告警一次，由模块自己报「未配置」。原始凭证不放进模型提示词、不进计划、不进事件、不打印到对话或日志，也不进服务单元文件（§3）。Memos 是随 komo 安装的首个内置已启用模块（同名模块已存在则不覆盖）。

### 5.4 AI 迭代流程

```text
读取模块和使用说明
  → write/edit 写入候选版本
  → Policy 审核候选代码的执行
  → 运行测试并保存结果
  → 展示差异、依赖变化和验证证据
  → Policy 审核启用操作
  → 校验候选哈希与已测版本一致
  → 原子替换当前版本
```

候选文件可以在授予的 staging 范围内修改。候选执行、依赖安装和启用新版本分别按真实权限评估，不能用“正在测试”跳过审核。

每次调用记录代码内容哈希、实际使用的自定义模块版本和 Python 环境版本，相关代码保存快照。版本变化使旧版本授权失效。首版不支持无法确定所执行版本的动态代码下载后自动调用。

旧调用的恢复使用已保存结果；需要执行尚未开始的调用时，使用绑定版本并重新检查当前策略。

### 5.5 Memos 作为主动记录的原文来源

用户说“记一下”“保存这段内容”时，Agent 调用 `python → toolbox.memos`，沿用同一 Policy 审核后写入 Memos。模块提供创建、查询、读取、修改等导出函数，具体函数按需要保存与审核；成功后返回记录 ID 和原文链接。

查询用户主动保存的记录时，通过模块查询 Memos 并读取原文。自动 Memory 可以保留相关摘要与 Memos 引用，但不建立第二份完整笔记库，也不把模型对笔记的推断视为用户确认。

Memos 使用用户访问令牌进行 API 认证；接入时核对用户部署的具体版本，不能把 latest 文档当成该实例的接口保证。[Memos API](https://usememos.com/docs/integrations/api-access) · [查询接口](https://usememos.com/docs/api/latest/memoservice/ListMemos)

写入失败要明确报告；请求已发出但响应丢失时标记 uncertain，先核对远端结果，不能直接重试导致重复记录。模块不得在 Memos 不可用时静默改存本地并声称已完成。

### 5.6 Skills：程序性说明

Skills 是**人写的程序性说明**——"做 X 时按这几步、用这几个 toolbox 函数"；toolbox 是**可执行能力**。两者互补，不合并，共同构成 §5.3 README 那一层"模型先读说明再动手"的知识面。

```text
~/.komo/skills/<name>/SKILL.md                     主目录
<workspace>/skills/, <workspace>/.claude/skills/   项目自带
~/.agents/skills/, ~/.claude/skills/               与其他本地 agent 共享，只读
```

- `SKILL.md` frontmatter：`name`、`description`，可选 `platforms:`、`requires_tools:`（对 5 个基础工具或 toolbox 模块名）。
- 搜索路径有序，**同名先到先得**；每次查询重扫目录，编辑或新增无需重启。
- 只有系统提示里的**目录行**（名字 + 一句描述，总量有上限）是启动时快照，为了提示前缀稳定；`platforms:` / `requires_tools:` 只门控这份目录，不门控加载。实现上这一块是两行开头加若干目录行：先给**按序的根**（只列真的出了条目的那些，顺序就是搜索顺序——"同名先到先得"因此落得下来），再给目录行；模型按这个顺序去找 `<根>/<名字>/SKILL.md` 并 `read` 它。一条能露面的都没有时这一块整个不出现（不留空标题）。启动时算一次，配置重载时按新快照重算，此外不随文件变化——目录行是提示前缀的一部分，不能每段都不一样。**整批装得下「名字 + 一句描述」就用它，装不下就只留名字**——列全比列得详细要紧：166 个 skill 的实测里，2000 字符的预算只留下字母表前 16 条，`log-diagnosis` 根本没进提示，模型为了找它去 `ls` 了整个目录；只留名字是 2752 字符、装得下全部（默认上限已按这个实测改成 4000）。名字都装不下时末尾会写「另有 N 条没列出来」：**「没有它」和「没列出来」是两件事**。
- **没有 skill 工具。**模型通过 `read` 读 `SKILL.md`——写本地路径，或者写 `skill://<名字>/SKILL.md`（§4.7，逻辑入口，审批与审计显示的是它）；skills 目录是 Policy 里的只读根（§4.7 的资源根也在 `ToolContext.roots` 里），读取 Allow。Skill 里写的"可以直接执行"不构成授权，Policy 只看 `ExecutionPlan`。
- **目录行的门控按这一次装配的能力面**：某个 Agent 的面里没有 `python` 时，`requires_tools: [python]` 的 skill 不进它的目录行；而 `skill://` 仍然读得到——门控管的是"要不要摆在眼前"，不是"能不能读"。
- Cron Job 可以声明 `skills = ["…"]`，触发时把这些 SKILL.md 正文预载进首轮上下文。
- `komo skills list | inspect <name> | enable | disable`；`disable` 只从目录行隐藏，不删文件。没有安装 / 候选 / 治理流程——Skills 由人写、由人放进目录。

## 6. Agent Loop

```text
持久化用户输入或 Cron 触发
  → JSONL 持久保存输入，state.db 提交可执行 Run 后确认接收
  → 调度器领取 Run
  → 从 Session 读取上下文
  → 固定本次 Run 的模型配置快照
  → MemoryManager 按当前任务召回有效记忆
  → 请求 LLM
  → 接收完整一轮回复
      ├─ 有 tool calls
      │    → 将完整 assistant 回复和调用计划追加到 JSONL 并同步落盘
      │    → 逐个 prepare → policy → execute
      │    → 先持久保存独立 tool output，再追加 JSONL 结果引用
      │    → 提交 state.db 状态与索引
      │    → 按 call_id 回传结果
      │    → 下一轮 LLM
      └─ 正常结束且无未完成调用
           → 将最终回复与完成事件写入 JSONL 并同步落盘
           → state.db 提交 Run 完成
           → 标记 Memory 整理任务待处理
           → 后台提取、校验、持久化并更新索引
```

工具名称和参数来自 provider 原生字段，不从自然语言或代码块推断。完整 assistant 消息与每个工具结果保持调用 ID 配对，这是原生 function calling 的执行方式。[Function calling](https://developers.openai.com/api/docs/guides/function-calling)

**同一轮的多个调用：只读的可以同时在飞，其余是屏障。** 够格的是 `Operation::ReadFile` 这一类计划（`read`、`rg`）里**没有上一世要接**的那次调用——续跑里"已经 `start` 过"的那些要走核对梯子（§8.6），顺序由账本钉死；`write`、`edit`、`shell`、`python`、`delegate`，以及任何会停下来的判定（审批 / 核对 / 取消），都是屏障——屏障之前已经在飞的先收尾，再按调用顺序做它。并发上限是执行器自己的预算（默认 4 条）。三条不变量不因为并发而改变：`start_call` 返回之后才允许产生副作用（§8.5）；遇到审批先收已启动调用的尾，**而且审批行在那之后才落**——它要是比 Run 的挂起早出现一整个批次，操作者答得比 Run 停下还早，那条答复就落进了空窗（§7.4）；完成事件按真实完成顺序落账，交给模型的那一份按**原始调用顺序**配对。

不同 Session 可并发，同一 Session 的 Run 顺序执行；等待审批的 Run 保留会话顺序位置，不允许后续 Run 越过它执行。等待运行排到前面时才装配上下文，避免读到过期历史。

Gateway 设置总轮数、活动执行时限、输出长度和子进程并发预算。等待用户时释放运行名额，保留已消耗预算。

普通工具失败作为结果交给模型修正；模型回复截断或调用参数未收齐时不能开始执行。远端写入结果不明时停止自动重试，转为结果核对。

普通追问可以作为 assistant 回复结束本轮；用户下一条输入开启同一 Session 的新 Run。工具审批则暂停原 Run，待决策后继续原调用，不增加第六个交互工具；决策通常来自聊天渠道（§11）。

Run 完成仅表示本轮结束。测试是否通过、性能是否改善、设备是否到达目标状态，都要依据具体执行证据报告。

### 6.1 Agent、Session 与 Run：三种身份

| 对象 | 是什么 | 住在哪 |
| --- | --- | --- |
| `AgentProfile` | 一个助手的长期定义：身份指令、模型、能给的工具、允许的 Skill、工作目录、记忆作用域 | 操作者写在 `config.toml` 里 |
| Session | 一段持续对话，**绑定一个 Agent**（`sessions.agent_id`），不随消息路由改人格 | `state.db` 的行 |
| `RunSnapshot` | 某一次执行受理时冻结的身份与能力 | 账本里那次受理上 |

**没有隐含的默认助手。** `default_agent` 指名"没有归属的入口（TUI、CLI、Cron）走谁"，`[agents.<id>]` 是唯一写法：一个都不声明就是**配置不完整**，`komo config check` / 启动会拒绝，并把该写的形状印出来。只声明一个 Agent 时它就是 `default_agent`——那不是隐含默认，是没有第二个答案。

```toml
default_agent = "assistant"

[agents.assistant]
instructions = "你是家里那个助手，回答用中文。"
model = "chat"                     # [model.<alias>] 的 alias；省略 = [models].default
tools = ["read", "rg", "python"]   # 省略 = 目录里全部；[] = 一个都不给
workspace = "home"                 # 相对数据目录；省略 = [paths] workspaces_dir
memory_scope = "personal"

[agents.coder]
instructions = "你在仓库里干活，动手前先把计划说清楚。"
tools = ["read", "write", "edit", "rg", "shell", "python"]
workspace = "code/komo"
```

两个字段各带一次"省略 ≠ 空"的区分，写在类型里（§4 的能力面）：`tools` 省略 = 目录里全部，`[]` = 一个都不给；写了目录里没有的工具名**不算数**，装配时 warn 出来，模型看到的 schema 里也不会出现它（`AgentProfile::surface` 把不认识的名字单独返回，就是为了不静默采纳一份写错的配置）。

**Session 的归属创建后不变。** 换助手 = 路由到另一个 Session，而不是给已有会话换人格、同时留着原来是另一个助手的上下文。每个 Agent 的主会话（`kind = 'main'`）在数据库里唯一：两个入口并发地要它，只落一条。

**升级路径**：`sessions.agent_id` 为空 = 升级前建的会话。它按 `default_agent` **归属一次并写下来**（只写一次，之后不随配置漂移）；归属的 Agent 在现行配置里不存在时，那条会话**拒绝执行**并进操作者清单（§7.5），不静默换成别的助手。

### 6.2 冻结的是身份，不是安全策略

受理一条 Run 时算出 `RunSnapshot` 落进账本：身份指令**正文进 `payloads/`（存内容，不存路径）**——审批可能一小时之后才答复，那时那个文件早被改过——连同 Agent 的 id、Profile 的内容指纹、模型、解析过的目录、这次的能力面与记忆作用域。

于是两句话都成立：**审批等待期间改 Profile，在飞的 Run 不受影响**（它按自己的快照恢复），下一条 Run 用新的一版；而**撤权与 Deny 不冻结**——身份与能力来自快照，"这次调用允不允许"每次执行都按当前 Policy 判（§7）。旧快照不会变成绕过撤权的通行证。

## 7. Policy 与审核

### 7.1 统一决策

```rust
pub enum PolicyDecision {
    Allow { reason: String },
    Ask { reason: String },
    Deny { reason: String },
}
```

Policy 检查准备好的 ExecutionPlan：来源、操作、工具、代码或命令、参数、真实目标路径、工作目录、版本、凭证引用与已有授权。除工具调用外，Memory 内部变更使用独立 operation_id 和来源引用走同一决策入口；它不是第六个 LLM 工具。LLM 提供的“风险低”或 Python 模块自称“安全”均不构成授权。

优先级：

```text
执行环境不可突破的限制
  > 明确 Deny
  > 有效的范围授权
  > 配置 Allow
  > 默认 Ask
```

| 操作                                          | 初始策略建议                                                         |
| --------------------------------------------- | -------------------------------------------------------------------- |
| 已授权范围内读取普通文件                      | Allow                                                                |
| 已授权 workspace / artifacts 范围内写入和修改 | Allow，覆盖时检查版本                                                |
| 访问范围外文件或敏感内容                      | Ask；命中显式禁用规则则 Deny                                         |
| 修改启用中的 toolbox 或 Python 环境           | Ask，展示具体差异                                                    |
| 任意 shell / Python code                      | Ask；有匹配的明确执行授权时允许                                      |
| 恢复流程发起的核对调用（`PlanSource::Verification` + Python call） | Allow；核对只读、绑定已审核的模块版本、由执行器而非模型发起——不放行则默认配置下每次核对都答 Unknown，核对函数等于没有（§8.6） |
| Python call                                   | 按已审核版本、导出函数、参数与授权范围判断                           |
| 自动提取记忆与生成索引                        | 在配置的来源、模型端点和记忆范围内 Allow；推断不能自行升级为用户确认 |
| Memos 的写入、修改或删除                      | 按 Python 模块版本、函数、参数与用户指令范围审核                     |
| 权限扩大或修改 Policy                         | 通过操作者配置流程处理，不能由模型自行放宽                           |
| 委派一个子任务（`Operation::Delegate`）       | 按操作者意图：strict 下 Ask（展示任务正文与结果契约），auto 下 Allow。**子代理自己的每一次调用仍各自按上面各行判断**——委派不放宽任何一层 |

**两套建议，操作者选一套。** 上面那张表落成 `RuleTable::initial()`（"strict"）；另一套是
`RuleTable::auto()`（"auto"）——**不审批**：一条 `Ask` 都不留，agent 一路跑到底，不打断人。
`policy.toml` 顶层用 `mode` 选基表，文件里的 `[[rules]]` 追加在基表之后；不写 `mode` 时
文件本身就是整张表（"我全都要自己写"，也是这个键出现之前唯一的行为）。`mode` 与 `default`
不能同时写：默认结论跟着基表走，两个答案放在一起是矛盾，宁可起不来也不猜。

```toml
mode = "auto"          # 或 "strict"；省略 = 下面自己写整张表（这时才写 default）
```

auto 只有两个动作：默认结论 `Allow`，**没有一条 `Ask` 规则**；剩下的唯一一条 `Deny` 是
`policy-change`（§7.1 第 9 行）。那条不是操作者设的边界，而是设计里那句"模型不能自行放宽
自己的权限"——今天没有哪个工具产生这个操作，留着是为了那天它出现时不必再想一遍。

**auto 与 strict 的差别就是"要不要人看一眼"这一件事，别的什么都没动**：范围外的文件、
toolbox / Python 环境变更、模型发起的模块调用，在 strict 里都要问，在 auto 里都直接放行。
不审批模式不能只在顺手的命令上成立——名字叫什么就得是什么。

**代价必须说清楚：不审批 = 这个进程能做的事，agent 都能做。** 首版没有能约束任意代码的
执行环境（`confines_arbitrary_code = false`，§7.3），所以 auto 之下 agent 可以读 `.env`、
`rm -rf`、把数据发出去，一个都不问；§7.3 那句话的另一面正是这一条：**没有沙箱时，"别
打扰我"和"这里有真正的禁区"不可能同时成立**。所以 auto 是操作者在文件里**显式选择**的，
不是默认；默认那份 strict 至少让人每一次都看见。

**想要一张网，只能自己加规则——并且要知道那只是网。** `Matcher::command_patterns` 是给
这件事用的：命中形状的规则可以 `deny`（不问、直接不许）或 `ask`（仍然问一下）。匹配是
**归一后的子串**（连续空白压成一个空格、两边降为小写，所以 `RM   -RF /` 与 `rm -rf /` 是
同一件事）：

```toml
mode = "auto"

# 想拦住的形状自己写：下面是那三类里最不会误伤的一批（一次手滑就没了 / 把控制权交出去 /
# 把密钥念进模型上下文），删掉哪条就少拦哪条。
[[rules]]
id = "catastrophic-shapes"
effect = "deny"
reason = "这几类形状不问你，直接不许"
scopes = ["once"]
requires_isolation = false

[rules.matcher]
operations = ["shell_command"]
command_patterns = [
  "rm -rf /", "rm -rf ~", "rm -rf *", "--no-preserve-root",  # 递归删除的灾难形状
  "mkfs", "dd of=/dev/", "shred ", "> /dev/sd",              # 磁盘
  "sudo ", "chown -r", "chmod -r 777",                        # 提权
  "| sh", "| bash", "|sh", "|bash",                           # 管道进解释器
  ".env", "gateway.json", "credentials.json", ".ssh", "id_rsa", "printenv",  # 凭据
]
```

**它不是边界，别当成边界用。** 形状清单认的是命令文本：`rm -r -f /`、`find -delete`、
变量拼出来的命令全在网外，真正的边界只有 §7.3 的执行环境。上面那份清单来自线上证据——
一条 WeChat 会话在找不到数据时去读 `runtime/gateway.json`（里面有 API 的 Bearer token）与
`.env`——它挡的是手滑，不是有意绕过。

### 7.2 审批对象与范围

审批绑定一份不可变的执行计划，包括 operation_id、来源、关联 Run / ToolCall（如有）、规范化参数、工作目录、目标版本、脚本或模块版本、环境版本及相关资源引用。界面——聊天里的审批消息或 TUI 弹窗——显示具体动作、原因、改动和已有验证结果，渲染见 §11.3。

首版提供：

- 本次调用授权：只批准眼前的执行计划。
- 本次 Run 的范围授权：例如指定目录的写入，或明确命令模板。
- Cron Job 授权：绑定 Job 版本、工具或模块版本、参数范围和权限。
- **一次答一批**：一条 Run 里连着几个命令、几个 Run 各自卡在等待上时，操作者一次答复把**此刻待处理的全部**答了（TUI 的 `a`、聊天与 CLI 的 `/approve all`）。它**不是第四条范围**：名单里的每一条各自落一条决定、各自换一份凭据、各自写一条审计事件——只是把 N 次按键变成一次。名单由发起方列出（"此刻看到的那些"），协议里没有"全部"这个词，服务端不会在答复到达时才决定名单，因而也不会把答复之后新出现的请求一起答掉。**批量的范围固定为本次调用**：范围授权绑的是一份具体的计划（shell 绑整条命令、Python 绑模块与版本），一批互不相干的计划共用一个范围，只能是替操作者猜一个他没看过的答复；他说了范围却被按本次答时，回执必须**说出来**，不静默降级。

不把“同意一次 Python”解释为“今后任意脚本均可执行”。审批不覆盖显式 Deny。批准后再次校验目标和版本，变化则重新评估。

### 7.3 执行边界

内置文件工具可以在 Rust 中执行路径与版本限制。任意 shell / Python 代码一旦启动，则具有其运行账号的操作系统权限；cwd 和参数检查不是完整进程沙箱。

因此，首版的人工作用是批准具体代码执行或明确信任范围。若配置要求强制禁止某类文件或网络访问，而当前执行环境无法约束任意代码，Policy 必须拒绝不受约束的 shell / Python，直到提供相应隔离。不能一面声明绝对禁止，一面让脚本任意访问。

已审核模块可按明确版本与参数信任其实现，但模块的权限声明本身不能证明隔离有效。

### 7.4 审批状态与恢复

```text
Ask
 → 先持久保存待审核计划的 JSONL 引用
 → 审批请求、`waiting + approval` 与审计待写事件在 state.db 事务提交
 → 释放执行名额
 → 投递到 Run 的来源会话与 home chat（§11.4），TUI 同时可见（TUI 的弹窗读的是补写进 JSONL 的 `approval.requested`，补写在请求落库后**立即**触发，§8.5；`run.waiting` 只说"停在审批上"，短 ID 不在这条事件里）
 → 用户在聊天里或 TUI 中批准或拒绝
 → state.db 原子记录决策及审计待写事件
 → 继续前重新校验并消费授权
 → 执行或返回拒绝结果
```

审批记录保存在数据库。重复回答幂等，已取消或已完成调用不能再次执行。tool.started 先写入 JSONL 并同步，首次授权消费与调用 started 索引随后在同一 state.db 事务内提交；此后进程崩溃可能产生结果未知，不能凭批准记录推断操作已经完成。

审批决定以 state.db 中经过操作者认证的记录为准；JSONL 中的审批事件用于展示与审计，不能自行创建授权。审计待写事件按 8.5 的方式补写。

一次性授权绑定逻辑 ToolCall 和具体计划，重启不清除它。恢复时若确定原动作未发生，或同一操作具备经过验证的幂等重试条件，可在原授权范围内继续；已经消费授权本身不是重试依据。范围、计划、版本或有效期变化才重新审核，审批无需用户因重启再答一次。

拒绝作为明确结果交回模型。后续换一个基础工具仍要检查同一权限目标，不能自动绕过禁止规则。

Cron、交互聊天与 resume 都经过这一条路径。

### 7.5 Intervention：一切"需要人判断"的同一个入口

审批只是"需要人判断"的一种。§8.4 / §8.6 还会产出另外两类：**结果不明**（副作用可能已经发生、没有可靠恢复方式）与**阻塞**（引用损坏、无法确认旧执行已结束、会话内容与账本对不上）。今天它们是三套互不相干的出口——审批有短 ID、有清单、有四个界面；结果不明只有一条事件与一句投递；而"上一次执行没收尾"在 `resume` 里被直接续跑。操作者因此会撞上最难解释的现象：**会话停着不动，而 `komo approval list` 是空的**。

统一成一个概念：**Intervention = 一条未完成的 Run 停在某个只有人能回答的问题上**。

| 种类 | 它从哪里派生（权威） | 句柄 | 问题 | 可答的结论 |
|---|---|---|---|---|
| `approval` | `approval_requests` 里待处理的那一行（§7.4） | 短 ID（待处理集合内唯一，§11.3） | 这份执行计划放不放行 | `approve` / `reject`，范围按 §7.2 三种 |
| `verify` | `tool_calls` 里那条 `uncertain` 的调用（§8.6） | Run ID | 上次那个调用**到底发生没有** | `satisfied` / `not_performed` / `abandon` |
| `blocked` | 其余 `waiting + intervention` 的 Run（§8.4） | Run ID | 前提没了：引用损坏、旧执行者未确认、会话不可服务 | `resolve` / `abandon` |

三条约束把这个清单钉死：

1. **清单是派生视图，不是第四张表。** 权威仍是 `runs` 与 `approval_requests`；`GET /v1/interventions` 是这两张表的并集查询，`kind` 由"有没有一条 uncertain 调用"当场判出。多一张 `interventions` 表等于多一处会与权威漂移的状态，而这次改造的全部理由就是不要那个（§8.9）。**子代理（§4）的审批因此自动出现在同一张清单里**：子 Run 与它的调用本来就在这两张表里，"谁派的"不是清单该关心的维度——操作者批的是那一次具体的动作。
2. **挡着会话的每一条都必须在清单里，而且与"后面领不走"是同一次判定。** §8.4 的"未完成"是四个非终态；其中**停在人身上的**（`waiting + approval`、`waiting + intervention`）必须逐条出现，且由同一个谓词回答"这条 Run 现在挡不挡队"——就是 `WaitReason::needs_a_person()` 与那段 `earlier` 子查询。两处判定分家就会出现"卡住但清单为空"。`waiting + retry`（等时钟）与 `waiting + dependency`（等另一条 Run）不进清单：**它们不是在等人**，但仍然挡着同 Session 后面的 Run——唯一的例外是父 Run 正等着它派出去了的那条子 Run，那条必须能跑，否则父子互等（§4）。
3. **结论只走一条路：核对 → 按 §8.4 行事，没有"我说它发生了"。** `satisfied` 给那次调用补一条"核对后目标已满足"的结果，原 Run 继续；`not_performed` 把那次调用标成"确定没执行"再入队（一次性授权因此按 §7.4 的原范围重放，而不是拿"已消费"当重试许可）；`resolve` 不强行放行，它只要求**重新观察并重新决策**一次；`abandon` 是终态取消。没有"我确认副作用已发生"这种结论——操作者可能看错，账本一旦这么记就再也纠不回来。

出口有四个，语义相同：`komo intervention list | show | answer`、TUI 的待处理清单、聊天里的 `/pending` 与 `/answer`（§11.3）、`GET|POST /v1/interventions`。投递沿用 §11.4 那一条：**没有界面在看这个会话时进 home chat**——那是任务在问，不是在报告（§10）。批量答复不新增一套语义，沿用 §7.2 的"名单由发起方列出"。

## 8. 通用 Session 存储与自动恢复

### 8.1 三个核心对象

| 对象     | 含义                                                      |
| -------- | --------------------------------------------------------- |
| Session  | 连续对话、工作目录和上下文的载体；有生命周期状态（§8.10） |
| Run      | 一次用户输入或一次触发引起的持久任务；重启前后保持同一 ID |
| ToolCall | 一次有独立执行状态的具体调用                              |

每个调用有 Runtime 分配的内部 ID，同时保留 provider call_id 和模型轮次；重复的 provider ID 不能误命中其他轮次。Run 可以经历多次进程执行，但不会因为重启而创建新业务任务；ToolCall 是逻辑动作，tool_attempts 记录它实际执行过的各次尝试。

本节的“任务完成”是 Run 已有明确终态，不能根据最后一条助手消息猜测。长期 Memory、任务摘要和模型生成的计划均不能代替执行账本。

**三者的权威都在 state.db，Session 目录是内容。** Run 的状态、队列与授权在数据库里，`sessions/{id}/` 里放的是消息、计划、输出与产物（§8.2）。这个分工的唯一后果是：**目录与数据库对不上时，以数据库为准，并且必须有人把对不上的那一半处置掉**——这就是 §8.9 的 reconcile，它取代"发现不一致就报损坏然后卡住"。Session 自身也只是一个可以被关掉、被逻辑删除、被回收的对象（§8.10），不是只能追加的。

### 8.2 存储分工

**每个 Session 的内容集中保存在 sessions/{session_id}/ 下。** JSONL 记录调用、状态、引用和预览；大参数、大模型回复、完整结果、stdout / stderr 分别保存在该目录的子目录中。state.db 维护调度状态、索引与授权，JSONL 与数据库都不重复保存完整 tool output。

| 保存位置                                                           | 内容                                                                         | 权威来源与恢复方式                                             |
| ------------------------------------------------------------------ | ---------------------------------------------------------------------------- | -------------------------------------------------------------- |
| sessions/{session_id}/events.jsonl                                 | 消息、调用与准备计划、开始 / 结束事件、参数和输出引用、摘要与完成事件        | **内容**的权威来源（事件顺序与调用关系）；完整内容通过引用读取。**状态与生命周期不在这里**——那是 state.db |
| sessions/{session_id}/payloads/                                    | 超限的模型消息或执行计划正文，包含大参数                                     | JSONL 保留引用与内容哈希；同一参数不再另存重复全文             |
| sessions/{session_id}/tool-output/{run_id}/{call_id}/{attempt_id}/ | output.json，以及按需保存的 stdout.txt / stderr.txt                          | 每次执行的完整工具输出，包含错误详情；按尝试独立、完成后不可变 |
| state.db（Turso，MVCC）                                           | Session 生命周期、Run 元数据、任务队列、领取代次、工具状态与引用、审批、投递记录、Cron、Memory | **生命周期、调度与授权的唯一权威**；执行内容索引与派生状态可从 JSONL 补齐（§8.9） |
| sessions/{session_id}/artifacts/{run_id}/                          | 工具生成的二进制文件、脚本快照与报告                                         | 独立产物，按 Run 分目录；正文里以 `artifact://files/<run>/<名字>` 给出引用（§4.7）；不重复复制到 tool-output |

state.db 中的主要表：

| 表                          | 主要内容                                                                                                                           |
| --------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| sessions                    | 标题、来源、工作目录、当前 Run、**生命周期状态与状态变更时刻**（§8.10）、JSONL 路径、applied_seq 和已应用字节位置                     |
| runs                        | 请求键与输入哈希、输入 / 最终结果事件引用、状态、来源与身份、授权引用、预算、有效期、领取代次、重试时间、配置快照、Memory 处理游标 |
| session_log_index           | event_id、Session seq、Run、事件类型、文件偏移、记录长度和完整性摘要；不存正文                                                     |
| tool_calls                  | 逻辑调用 ID、参数 / 计划 / 结果事件引用、计划哈希、恢复方式、外部幂等键和状态                                                      |
| tool_attempts               | 调用 ID、尝试序号、执行实例、进程身份、状态、时间与事件引用                                                                        |
| checkpoints                 | 已覆盖的 seq、JSONL 字节位置、格式版本、上下文与记忆版本引用、执行游标                                                             |
| approval_requests           | 短 ID（待处理集合内唯一）、计划引用与哈希、操作者决策、有效范围与消费状态                                                          |
| policy_grants               | 有范围、来源、版本及有效条件的授权                                                                                                 |
| control_outbox              | state.db 控制事务产生、尚待补写到 JSONL 的审计事件                                                                                   |
| deliveries                  | 主动投递记录：目标、内容引用、pending / sent / deferred（§11.4）                                                                   |
| cron_jobs                   | 定时任务定义、版本与授权                                                                                                           |
| cron_firings                | 唯一触发记录、不可变触发快照及 Session / Run 引用                                                                                  |
| memory_items                | 自动记忆内容、作用域、确认状态、生命周期、revision 与时间信息                                                                      |
| memory_evidence             | 来源事件或外部记录引用、提取与确认依据                                                                                             |
| memory_terms                | 索引时分词的关键词列（memory_id、revision、terms），可重建（§9.4）                                                                 |
| memory_vectors              | f32 BLOB + 维度 + 代次，可重建                                                                                                     |
| memory_index_generations    | 向量空间指纹、构建状态、进度与生效代次                                                                                             |

control_outbox 只保存控制事件，例如审批请求和回答，不复制消息或工具结果。它和队列表都在同一个 state.db 中，不增加外部消息服务。

**state.db 是 Turso，MVCC 模式，通过 toasty 访问。** 文件同步和数据库提交各自有持久化边界，不能称为跨文件原子事务；但两边都是真的落盘。**Turso 0.7.2 已核实（2026-09-16）：MVCC 与 WAL 两条提交路径都在提交内 fsync——MVCC fsync 逻辑日志 `state.db-log`，WAL fsync `state.db-wal`——同步档位是每连接状态，默认 `SyncMode::Full`（比 SQLite 在 WAL 下默认 NORMAL 更严），`PRAGMA synchronous` 可设可读回（`turso_core` 的 `mvcc/database/mod.rs:3107` / `storage/pager.rs:4297` / `translate/pragma.rs:647,1548`；strace 与 `synchronous=OFF` 对照实验见 `.scratch/komo-v08-rewrite/spikes/store.md`）。因此原先预备的「outbox 补写到 JSONL 后才确认已批准」这道保护不启用**：审批决定一经数据库提交即已落盘，客户端可以立刻得到确认，outbox 只承担审计补写与顺序，不承担耐久性。内容权威仍然是 Session 目录，每一步先 `sync_all` 再往下走，state.db 是调度与授权的权威。

两条随之而来的运维事实：① `state.db-log` / `state.db-wal` 与 `state.db` 同等重要，备份和清理不能只拷主文件；② Turso 默认 `data_sync_retry = false`，此时 fsync **出错是 `panic!` 而不是返回 `Err`**（`storage/pager.rs:4330`）。Gateway 在 `Db::connect` 里显式 `PRAGMA data_sync_retry = 1`，把「磁盘写失败」变成一个可以进 `waiting + intervention` 的错误，而不是一个把整个进程带走的 panic。

引擎事实与由此而来的规则：

| 事实 | 规则 |
|---|---|
| MVCC `concurrent_writes`：**只有 `BEGIN CONCURRENT` 事务**才并发提交；冲突的提交**失败而不是等待**。toasty 只在显式 `db.transaction()` 上发 `BEGIN CONCURRENT`，autocommit 单语句走普通写事务，且驱动不设 `busy_timeout`——实测 4 个写入器并发写 200 行不同主键，autocommit 只成 6 次，包进事务 200 次全成 | **每个写操作、包括只有一条语句的，都放进 `db.transaction()`，再整个包进 `with_write_retry`**；回滚后干净重跑，绝不双重应用。重试条件是 `toasty_core::Error::is_serialization_failure()`（驱动把 `Busy` / `BusySnapshot` / 消息含 `conflict` 的错误都归到这里），不是字符串匹配。闭包只依赖入参和事务内读到的状态，里面不 `await` 模型、子进程、文件同步或用户；重试超限报 `Contended`，按 §8.4 进 `waiting + retry`（`cause = contended`） |
| MVCC 支持 `AUTOINCREMENT`（原子序列，`turso_core` 的 `test_autoincrement_works_in_mvcc`，2026-09-16 实测 CREATE / INSERT / `sqlite_sequence` 正常）——但 komo 不用它 | 每个主键仍是 `String` UUIDv7：ID 要在写库之前就存在（JSONL 先写、事件里带 ID、跨进程恢复按 ID 对账），数据库生成的自增值满足不了这个顺序。Session 内的 `seq` 是 JSONL 写入器分配的整数列，不由数据库生成 |
| MVCC 下自定义索引模块不可创建（FTS 不可用）——已实测：`CREATE INDEX … USING fts (…)` 报 `Custom index modules are not supported in MVCC mode`（`translate/index.rs:70`），`CREATE VIRTUAL TABLE … USING fts5` 报 `Virtual tables are not supported in MVCC mode`（`translate/schema.rs:1692`） | 关键词检索在索引时分词写入 `memory_terms`（§9.4） |
| toasty 的类型化 API 能表达带条件的 `UPDATE`（`Model::filter(...)` 支持非索引列、`AND`、`IS NULL`），但**拿不到受影响行数**——查询目标的 `.exec()` 恒为 `Ok(())`，命中 0 行与 1 行不可区分；`ALTER TABLE`、`instr`、`vector_distance_cos` 也不在类型化 API 里 | 这四件事全部走 toasty 自带的 raw SQL 口子 `toasty::sql::statement(..) -> Result<u64>` / `toasty::sql::query(..) -> Vec<Value>`（占位符 `?1`、`?2`；`Db` 与 `Transaction` 上都可用，因此 raw 语句照样在事务里）。**不需要第二个 `turso::Database` 句柄**；raw SQL 仍只允许出现在 store crate 的三个模块：schema、keyword index、run claim |
| 每个 db 文件由进程独占锁定 | Gateway 是唯一打开 state.db 的进程，CLI 与 TUI 永远走 HTTP（§3） |
| `turso` 的 `mimalloc` 特性是全局分配器 | 工作区只能有一个 turso 版本，且与 `toasty-driver-turso` 的 pin 同 major；其他 crate 不得声明 `#[global_allocator]`。工作区把 turso 写成 `default-features = false, features = ["mimalloc"]`，关掉 `fts`（§13.4） |
| toasty `push_schema` 只对新文件执行，且不幂等 | 见下面的 schema 演进 |
| 提交在 Turso 上确实 fsync（MVCC 写 `state.db-log`，WAL 写 `state.db-wal`），默认 `synchronous=FULL` | 审批决定提交即持久，不需要 outbox-先-确认；`state.db-log` 与主库同等重要；连接建立时设 `data_sync_retry=1`，让 fsync 错误可报告而非 panic |

Schema 演进没有迁移脚本目录。每个 toasty 模型旁边放它的 `*_TABLE_DDL` 常量；`Db::connect` 对已存在的文件逐表 `CREATE TABLE IF NOT EXISTS`、逐列 `ALTER TABLE ADD COLUMN`，对新文件让 toasty 建表。**这一趟补列必须在连接池建起来之前、用一条普通（非 MVCC）连接做完**（实测 2026-09-20：MVCC 连接上的 `ALTER TABLE` 返回 `Ok`、日志照打，重开即无——旧库升级会静静地起不来）；建池之后 `ensure_schema` 只做核对，文件库缺列就**报错**，内存库才由它补。同一个坑还有一条：turso 的读游标拖着一条读事务，**同一条连接上"边读边改"会 panic**在 `SetCookie`，所以那趟迁移是"先读清、丢掉连接、只用一条只写连接改"。一个测试对每张表断言 toasty 为空库生成的 DDL 与常量**字节相等**——模型改了列却没改常量，测试挂。新列必须 `NOT NULL DEFAULT …` 或可空；退役的列继续写空值，不删。耐久表（`sessions`、`runs`、`approval_requests`、`policy_grants`、`cron_jobs`、`cron_firings`、`memory_items`、`memory_evidence`、`deliveries`）只允许加法变更——`sessions` 与 `runs` 也在这里，因为生命周期与队列从内容里长不出来：目录没了还能从数据库说清"它曾经是什么"，反过来不成立（§8.10）；可重建表（`session_log_index`、`tool_calls`、`tool_attempts`、`checkpoints`、`memory_terms`、`memory_vectors`、`memory_index_generations`）按行或按代次从 JSONL / 原文重建，从不删文件——一个文件里同时住着耐久表，"删掉重来"不存在。

### 8.3 JSONL 格式、写入与读取

首版每个 Session 目录下一个 events.jsonl，payloads、tool-output 和 artifacts 均位于旁边的子目录。UTF-8 编码，每条事件占一个物理行；内容中的换行由 JSON 序列化转义，禁止多行美化输出。Komo 要求每条完整记录以换行结束，便于识别写到一半的末尾。[JSON Lines 格式](https://jsonlines.org/)

每行包括 v、seq、event_id、session_id、run_id、at、type 和 data。涉及工具时带 Runtime call_id 和 attempt_id，并保留 provider call_id 与轮次。seq 在 Session 内按追加顺序严格递增；event_id 标识一次逻辑事件，重复提交同一 ID 必须幂等，内容不同则报错。控制审计补写保留原始发生时间和操作引用，不凭日志行相邻推断审批关系。

以下是简化的调用片段；输出正文不在事件中，示例省略引用大小和哈希，实际准备计划还包含资源版本、代码快照和权限引用：

```jsonl
{"v":1,"seq":41,"event_id":"evt-41","session_id":"sess-1","run_id":"run-1","at":"2026-09-15T08:00:00Z","type":"assistant.message","data":{"round":3,"tool_calls":[{"call_id":"call-7","provider_call_id":"pc-7","name":"python","arguments":{"mode":"code","code":"result = 1 + 1"}}]}}
{"v":1,"seq":42,"event_id":"evt-42","session_id":"sess-1","run_id":"run-1","at":"2026-09-15T08:00:01Z","type":"tool.started","data":{"call_id":"call-7","attempt_id":"attempt-1","plan_ref":"evt-41"}}
{"v":1,"seq":43,"event_id":"evt-43","session_id":"sess-1","run_id":"run-1","at":"2026-09-15T08:00:02Z","type":"tool.result","data":{"call_id":"call-7","attempt_id":"attempt-1","status":"completed","output_ref":{"path":"tool-output/run-1/call-7/attempt-1/output.json"},"preview":"result = 2"}}
```

所有工具的完整输出都单独保存，不区分大小。output.json 保存结构化结果或错误正文，并绑定 Session / Run / ToolCall / attempt、计划哈希和完成状态；stdout / stderr 如有则写入独立文本文件，在 output.json 中引用。JSONL 的 tool.result 只保留状态、耗时等元信息、output_ref 和最多 1 KiB 的可选预览。

调用参数默认小量内联；单次参数超过初始 4 KiB 限制时，将包含它的模型消息正文存到 payloads，以 arguments_ref 指向文件内的对应字段。较大的准备计划同样外置。正文和输出引用包含相对于当前 Session 目录的受控路径、文件大小、内容哈希及必要的字段定位；不在 JSONL、参数文件和原始回复中各复制一份大参数。output.json 内部引用 stdout / stderr 时也使用同一个 Session 目录作为基准，不依赖进程工作目录或 output.json 所在子目录；解析引用时禁止越出该 Session 的内容目录。

stdout / stderr 在运行时流式写入 .partial 文件，避免在 Gateway 内存里积累完整输出。进程结束并收齐输出后，同步并完成文件，再原子写入 output.json；最后才能发布 JSONL 结果引用。因中断只留下的 .partial 文件可以用于诊断，不能当作完成结果。

读取历史或恢复调用时按引用加载需要的内容；组装模型上下文仍遵守输出预算，超限时提供截断提示和可读取的完整文件引用。**这里有两个预算，不是一个**：`tool.result` 那 ≤1 KiB 是**账本的行预算**（每条 JSONL 行都要小），交给模型多少由 `[execution] model_result_bytes` 说了算（§6），完整正文落在 `output.json` 里。模型看到的正文由**一处纯函数**投影出来（`komo-kernel` 的 `projection`）：抬头（工具、状态、耗时、stdout / stderr 大小）+ 头尾各留一段并写明省了多少 + 可直接 `read` 的**引用**（`artifact://<run>/<call>/<attempt>/stdout`，§4.7）。从前这里印的是绝对路径；改成 URI 之后，「超限的正文在哪」与「允许读哪个逻辑资源」是同一句话。因此**刚跑完的那一次与重启之后回放必须逐字节相同**——投影只吃落盘的事实（事件 + `output.json`），不掺任何只活在内存里的东西。那条路径必须真能读：**当前 Session 获授权的输出（`tool-output/`、`artifacts/`）是一段只读根**，`read` 得到它、写它一律 Deny（§8.10 第 4 条），不开放普通工具修改这些记录。摘要或预览不能代替恢复所需的原始参数与结果。

实现约束：

- Gateway 对每个 Session 使用一个串行写入器，模型、工具、审批审计与 Memory 来源标记均经过它，不能各自追加导致行内容交错。
- 完整记录写入后清空用户态缓冲，再同步文件；同步失败就停止该步骤，不能执行后续副作用或向客户端确认持久完成。新建文件和目录还要处理目录持久化。Rust 的 File::sync_all 提供文件同步接口，关闭文件不等于已验证同步成功。[Rust File](https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all)
- 完整模型回复和该轮调用计划保持为一个逻辑 assistant 事件：正文小则内联，大则引用已经完整持久化的 payload 文件，避免只恢复出半轮调用。工具输出先独立持久化，再一次提交对应结果事件，不按流式 token 写 JSONL。
- 正常修改只追加新事件；纠正、取消和摘要覆盖关系通过新记录表达，不重写旧行。
- 读取依据事件 ID / seq；字节偏移只是加速索引，校验不符时重新扫描并重建。每轮通过检查点与尾部增量读，避免反复解析完整会话。
- 模型请求保存所用事件范围、模型 / effort、工具与记忆版本引用；摘要正文作为 JSONL 事件保存，检查点不再复制整段历史。
- **回放给模型的是"这一段对话"，不是"这一条 Run"**：最新一个 `conversation.boundary` 之后的消息按 Run 分组、Run 之间先来后到；**正在跑的那条 Run 带完整协议**（`provider_blocks`、工具调用与结果），**历史 Run 只发布两件事——用户说了什么（`run.accepted`）、它最后答了什么**。三条理由：① 历史那半轮在取消 / 失败时合法地停在"有调用、没结果"上（§8.4），照搬进新请求就是 provider 400；② 几十轮工具往返会随对话长度线性涨上下文，而对"接着聊"没有增量；③ 子代理跑过的那几轮不属于父的对话，父的窗口里没有它的过程，子代理也拿不到父的历史（§4）。还没被领走的 Run 整个不进（§8.4 的不越过）。**上一轮说的话因此必须还在**：少了它，"去查一下"指的是哪件事就只能靠猜。正文超限外置时按引用读回来——少读一次，模型看到的就是一条空的 user 消息，而它该看到的是原话。
- Memory 的证据引用使用 Session ID + event_id / seq，稳定定位原始消息或工具结果。
- **读事件不得创建或修改内容。** `Ledger::read` 与 `session_log::read_events` 是只读路径：文件不在就返回空（新会话本来就没有日志，`GET /v1/sessions/{id}/events` 依赖这一点），**绝不 `create`**。写路径（受理、`conversation.boundary`、挂起、终态）才建目录与文件。少了这条界线，"内容被删掉"会被一次读**静默地补成一个空日志**——于是 §8.9 的观察永远看不到它本该报出来的那件事（线上冒烟抓过一次：对账读会话就把目录建回来了）。
- JSONL、state.db 和引用产物均为 Gateway 的受保护状态；普通工具不能通过 write / edit / shell / python 直接修改。回放只能接受 Gateway 写入的合法记录，不能把模型生成的 JSON 当作执行事实或授权。

尾部恢复时，先校验数据库已经引用的记录和检查点，再处理尚未应用的后续记录。只有末尾不完整、且不被已提交引用覆盖的行，才能先隔离保留原始字节再截断；完整有效行不能因为索引落后被删除。文件中间损坏、已提交范围缺失或哈希不匹配时停止受影响会话的自动执行，不能跳过坏行或退回旧检查点继续。

未知事件保留原始内容与序号；影响执行、权限或调用配对的未知版本必须暂停恢复并要求兼容版本，不能按无关日志忽略。

### 8.4 重启后的默认行为

**Gateway 自动接续能够确定恢复位置的任务。用户只处理审批、时效或结果不明等确实需要判断的情况。**

**状态回答"能不能跑"，`WaitReason` 回答"为什么不能跑"。** 这两件事分开，是因为它们混在一起时的症状正是"排队二十分钟不知道在等什么"：一个 `waiting_approval`、一个 `needs_attention`，都答不出在等谁、等到什么时候。所以状态只有八个，等什么有四类：

| 状态 | 含义 | 可领取 |
|---|---|---|
| `accepted` | 已受理：请求键预留了 ID，输入正文可能还没写完（§8.5） | 否 |
| `queued` | **现在就能跑，只缺 worker** | 是 |
| `running` | 正在执行。`claimed_by IS NULL` 的是刚被回收、还没判完的孤儿（§8.9） | 否 |
| `waiting` | 当前不能跑，`WaitReason` 说得出在等什么 | 只有 `retry` 到点之后 |
| `completed` / `failed` / `cancelled` / `abandoned` | 四个终态 | 否 |

| `WaitReason` | 在等什么 | 谁放它出来 | 进 §7.5 清单 |
|---|---|---|---|
| `approval` | 一份执行计划的答复（§7.4） | 操作者答复；答复仍有效时自动接续 | 是 |
| `retry` | 一次有界退避到点（`attempts` / `not_before` / `cause`） | 时钟：`wake_at <= now` | 否——不是在等人 |
| `intervention` | 一条 Intervention 的答复（§7.5：结果不明、前提没了） | 操作者答复 | 是 |
| `dependency` | **另一条 Run** 进终态：同 Session 里更早的那条输入（本节的次序规则），或者是它派出去的那条子 Run（§4 的委派） | 那条 Run 进终态，reconcile 把它放回 `queued` | 否——不是在等人 |

**退避只用于可以安全重试的失败**：限流、连不上、5xx、本地写争用，`RetryCause` 把它们分得开（`RateLimited` 要尊重服务端给的 `Retry-After`，`Contended` 退几毫秒就够——混成一个数字就只能取最保守值，每次 429 都白等）。工具那一侧"结果不明"**不能**用它：那是副作用有没有发生都不知道，必须进 `intervention` 让人核对（§8.6）。两者混起来就是"重试成功"掩盖掉那个窗口，正是 §8.6 要禁止的事。

**`Queued` 与 `Waiting` 是这个模型的验收口径**：`Queued` 的 Run 一定有活干，`Waiting` 的 Run 一定说得出在等什么。查"它怎么还不跑"时，这两句话各回答一半。

重启后逐行：

| 重启前停在哪里 | Gateway 启动后怎么处理 |
| --- | --- |
| 输入已在 JSONL 持久保存，Run 尚未开始（`accepted`） | 补齐 state.db 索引后自动入队 |
| 输入请求只预留了 ID，正文尚未完整写入 | 保持 `accepted`；等待同请求键重传，不能执行缺失输入 |
| 正在请求 LLM，完整回复尚未保存 | 丢弃未完成输出，以已保存上下文重新请求；可能再次产生模型费用 |
| assistant 回复和调用计划已保存 | 沿用原计划，从未完成的 ToolCall 继续 |
| JSONL 已有结果引用且完整输出校验通过，state.db 可能落后 | 先补齐结果索引和状态，复用原输出，不重放动作 |
| 调用 planned，确定尚未执行 | 校验原计划与当前权限后自动执行 |
| 调用有结果、**没有 started**（一次没执行过的调用：工具名不认识、参数准备不出来、放行被拒） | 按已有结果收口，不重跑；回放时把那条 `tool.started` 缺席的尝试行补上（`fail_call` 写下的就是它） |
| 调用 started，没有结果 | 先核对外部效果，按 §8.6 决定是否安全继续 |
| `waiting + approval` | 保留原请求；已答复且仍有效的批准自动接续 |
| `waiting + retry` | 沿用已保存的次数与到点时刻，到点再尝试 |
| `waiting + intervention` | **原样保留**：它是清单里的一条，等操作者答复——恢复扫描不替人答，也不偷偷往下跑 |
| `waiting + dependency` | 原样保留；谁放它出来由 reconcile 判（它等的那条 Run 进终态 → 回 `queued`） |
| 已保存最终结果，但客户端没有收到 | 补发或补读原结果，不重新执行任务 |
| 四个终态 | 保持终态，不因重启自动开启新一轮 |
| Session 在 `closing` / `deleted`，Run 还没跑完 | 照跑或停在等待，但不新开；`--now` 的逻辑删除已把它们各写一条明确的取消（§8.10） |
| Session 内容缺失或被回收（`state` 不是 `purged` 却读不出目录 / JSONL / 引用的输出） | 不许领这条 Run，停成 `waiting + intervention`，理由里说清缺什么（§8.9） |
| 无法确认旧执行已结束 | 不许重复启动，停成 `waiting + intervention` 等人 |

内部状态示意：

```text
Run（状态 × 理由）:
                       ┌─────────────┐
                       │  accepted   │  已受理；正文可能还没落全
                       └──────┬──────┘
                              │ 正文落盘 + queued 提交
                       ┌──────▼──────┐
                       │   queued    │  现在就能跑，只缺 worker
                       └──────┬──────┘
                              │ claim（带租约）
                       ┌──────▼──────┐
                  ┌────│   running   │────┐
                  │    └──────┬──────┘    │
              停在外因         │ 正常收尾   │ 出错 / 取消 / 放弃
                  │           │           │
                  ▼           ▼           ▼
            ┌───────────┐  completed   failed / cancelled / abandoned
            │  waiting  │
            └─────┬─────┘
                  │ 条件满足：答复 / 到点 / 前一条 Run 进终态
                  ▼
                queued

WaitReason: approval · retry{attempts, not_before, cause} · intervention · dependency{run}

ToolCall:
planned → started → completed / failed
                  └─ uncertain → 核对完成 / 确定可重试 / 等待处理

Session（§8.10）:
active → closing → deleted → purged
```

**四个终态各自是什么**：`completed` 正常结束；`failed` 有明确错误；`cancelled` 用户明确取消（不自动复活）；`abandoned` 是**操作者在清单上放弃**——与取消分开记："不是用户不想跑了，而是这件事不会再有下文了"，事后统计要分得开。

**同一 Session 后面的 Run 不越过前面的**：前一条没进终态时，后一条是 `waiting + dependency` 而不是 `queued`——它写得出在等谁，而不是一句"排队中"。**判"谁在前"用的是输入事件的 `seq`**（§8.3：seq 在 Session 内按追加顺序严格递增），不是 Run ID 的字典序：UUIDv7 同一毫秒内的低位是随机的，拿它当先后会把两条几乎同时到达的输入排反，而次序判错意味着后一条会越过前一条（provider 400 的来源）。这条次序在三个地方用同一份判据：受理那一刻（写 `queued` 还是 `waiting + dependency`）、`due` 的候选、`claim` 的守卫。**停在人身上的那两类（`approval`、`intervention`）逐条出现在 §7.5 的清单里，而且"挡住队列"与"进清单"是同一个判定**——两处分家就会出现"卡住但清单为空"。

用户明确取消的 Run 不自动复活；普通错误达到重试或执行预算后进入 `failed`。Gateway 关闭或系统重启属于中断，**不等于用户取消**，也不再留一个叫 `interrupted` 的状态：领取权交还之后，那条 Run 由 reconcile 当场判成 `queued`（安全）或 `waiting + intervention`（副作用不明）。已经授权执行但有时间限制的动作，先检查有效期；过期不能按旧指令直接产生新的外部影响。

### 8.5 JSONL 与 state.db 的写入顺序

消息和调用事件遵循“先持久化外置正文（如有），再持久化 JSONL，最后提交 state.db 状态与引用”。每个 Session 的协调入口串行完成追加与索引推进；applied_seq 只能推进连续、已校验的事件前缀。数据库事务中不等待模型、用户、子进程或文件同步。

```text
收到输入
  → state.db 用请求键预留 Run ID，状态 `accepted`，仅存输入哈希与来源
  → JSONL 追加 run.accepted（包含完整输入和绑定 Run ID），同步文件
  → state.db 事务写入事件引用、queued 与 applied_seq
  → 向客户端确认已接收
  → 调度器领取 Run

收到完整模型回复
  → 大消息或大参数先持久保存到 payloads（如有）
  → JSONL 写入完整 assistant 事件与全部调用计划的内联内容或引用，同步文件
  → state.db 事务建立该轮 planned 调用与事件索引

执行一个调用
  → 校验计划、代码、权限与资源当前状态
  → 需要外置的准备计划先持久保存，再将计划引用与 tool.started 追加到 JSONL 并同步
  → state.db 事务提交 started、执行尝试、事件索引和首次授权消费
  → 确认文件与数据库两步都完成后，才执行真实动作
  → 持久保存 stdout / stderr（如有）、结果正文和 output.json，完成文件与目录同步
  → JSONL 追加只含元信息与 output_ref 的 tool.result 并同步
  → state.db 事务更新结果引用、调用状态、事件索引和 applied_seq
  → 继续下一调用或下一轮模型

结束任务
  → JSONL 追加包含最终回复的 run.completed 并同步
  → state.db 事务更新终态、最终事件引用和 Memory 待处理标记
  → 通过 SSE 通知客户端
```

同一请求键重发时返回原 Run，内容哈希不同则拒绝。`accepted` 的完整输入若已在 JSONL 中，启动修复就能补为 `queued`；如果正文从未写完，则等待客户端重传，不能凭输入哈希补造用户指令。Cron 在创建 firing 与预留 Run 的事务中保留不可变触发快照，可以据此完成尚未写入的触发输入。

恢复先处理已同步但未索引的 JSONL 尾部，再判断任务是否需要执行。例如 tool.result 已经完整持久保存且引用输出校验通过、数据库仍显示 started，必须先补齐为已有结果，不能直接按 uncertain 重试。若结果引用存在但对应输出缺失或哈希不符，停止受影响任务，不能重跑来掩盖数据损坏。输出文件已写完但 tool.result 尚未提交时，只有完整校验文件内的调用身份、计划、完成状态与内容后才能补记结果；仅凭路径存在不足以宣告成功。

回放只补索引与派生执行状态，不调用工具、不发送外部请求、不消费授权，也不能覆盖 state.db 已记录的用户取消或权限撤销。tool.started 本身不能证明已产生副作用或已经通过授权消费；缺少 tool.result 的调用仍进入核对流程。

审批等控制操作的权威在 state.db，采用相反方向的审计补写：

```text
state.db 事务提交审批决定与 control_outbox（固定 event_id）
  → Session 写入器将审计事件追加到 JSONL 并同步
  → state.db 标记 outbox 已交付并更新日志索引
```

重启后重发同一 outbox 事件先按 event_id 去重，已写入就复用原事件位置。审批生效不依赖审计补写成功；客户端恢复时可直接查询数据库中的原决定。JSONL 中的审计副本无法反向生成新的授权，outbox 也不承载完整工具结果。

**补写由写入触发，周期只是兜底。** 上面那三步是顺序，不是节拍：`control_outbox` 一有新行就叫醒补写器（请求那条在 Run 停在待审批上时叫，回答那条在决定入队后叫），否则"随后补写"会变成"最多晚一分钟补写"。**这一分钟是看得见的**：界面（TUI 的审批弹窗、任何按 SSE 待处理帧走的东西）等的正是 `approval.requested`——`run.waiting` 只说"停在审批上"，短 ID 在补写的那一条身上；同理，别的界面知道一条请求已经答过也要靠 `approval.decided`。周期（60s）留着兜底：漏叫的、别处写进去的，一拍之内照样补上。

完整模型块、工具调用 ID、原始参数、结果和 provider 回放所需字段通过 JSONL 及其引用文件完整保留。未完成的流式模型输出没有执行权限；只有完整计划持久保存且执行前置状态已提交后才能调用工具。

普通模型超时可按有界退避重试，次数、已用预算和下次时间都持久化。重启不重置预算；模型请求结果与用量都未知时，保留预留额度和未知标记，不能将这次消耗当成零。

### 8.6 工具恢复：先判断是否发生，再决定是否重试

Runtime 无法对任意 shell / Python 和外部服务共同提交一个原子事务。副作用已发生、完整输出与结果事件尚未持久保存时，必须保留 uncertain，而不是用“重试成功”掩盖这段窗口。

执行计划包含一种经过验证的恢复方式，不能由模型随意填入“可重试”：

| 恢复方式         | 自动处理条件                                                             |
| ---------------- | ------------------------------------------------------------------------ |
| 可安全重做的读取 | 例如普通文件读取；记录这次重新读取的实际时间和结果，不冒充重启前的观察   |
| 支持外部幂等键   | 外部接口确实保证去重；所有尝试复用同一逻辑操作的键和参数，核对键的有效期 |
| 可以核对目标状态 | 使用审核过的核对逻辑，返回已达到目标、确定未执行 / 可重试、冲突或未知    |
| 无可靠恢复方式   | 停在 `waiting + intervention`；不自动从头执行整个脚本                    |

文件 write / edit 可记录修改前后内容哈希与目标身份。若恢复时目标已是预期内容，则记录“核对后目标已满足”；仍是原内容且其他前提成立，可重新执行同一原子修改；出现第三种内容则视为冲突。不能把文件已存在当作写入成功，也不能编造丢失的 stdout 或原始返回值。

**委派用"可以核对目标状态"那一行，而它的核对对象是我们自己的账本。** 一条 `delegate` 调用的结果不在别处，就在它派出去的那条子 Run 的终态里（`Ledger::run_end`）：子 Run 正常结束 → 那次调用按已完成收尾，结果交给父 Run（有契约时按**父侧计划里那份**契约复验，与子代理自己用的是同一份校验器）；子 Run 失败 / 取消 / 放弃 → 按失败收尾，理由里写明它怎么结束的；子 Run 还没有终态 → 继续等。**这里不会出现"结果不明"**：副作用发生过没有，是我们账本里的一条事实，不是对外部世界的猜测——那张表把这种情形与"任意 shell 命令"分开的用意正在于此。子 Run 自己是普通 Run，它的恢复照 §8.4 逐行走。

已保存的 Python 模块可提供与版本绑定的核对函数，由执行器通过同一 python 执行机制调用；核对本身仍经过 Policy。它不增加第六个模型工具。代码、依赖、参数以及核对逻辑都必须对应已审核的执行计划，模块自称幂等不构成证明。

典型情况：

- 已保存某次测试的完整结果：恢复后直接使用，不重复跑。
- 测试进程中断且结果丢失：只有已确认可安全重做的测试才自动重跑；任意 shell 命令不能仅凭名称被判定安全。
- HA 的“设为关闭”：可以读取当前状态核对目标；“切换一次”不能据此重复调用，且恢复前要检查时效。
- 创建 Memos 记录或 PR：优先保存远端 ID，或使用接口实际支持的幂等键 / 可验证关联 ID。只有内容相似、时间接近，不能证明就是原调用创建的记录；无法确认时请用户处理。

一次重试沿用 ToolCall ID，新增 tool_attempts；完成状态附带实际证据和核对方式。已完成或已失败且结果已保存的调用不重做。重复尝试、后台核对与新模型调用都计入原任务预算。

### 8.7 Gateway 启动与进程回收

```text
取得实例锁，校验数据库与协议版本
  → 创建本次启动身份（旧的 running 从此都算"别人的"）
  → 校验受影响 Session 的 JSONL，修复未索引尾部和派生状态
  → 补写控制审计 outbox，核对尚未接收完整的输入
  → **回收没有主人的 running**：交还领取权（不改状态），再按 §8.4 判成 queued 或 waiting
  → 放掉到点的 retry、放掉依赖已终态的 dependency
  → 检查旧子进程与未完成调用
  → 按 Session 顺序恢复原 Run
  → 可继续的提交统一队列
  → 需要人答复的保留 waiting（它们已经在 §7.5 的清单里）
  → 周期扫描（reconcile），补齐内存通知丢失的任务
```

**就绪（HTTP 监听起、发现文件有效）之后**才做两件不影响调度事实的事：渠道起来后按平台补发它名下积压的投递，以及启动时那一次整体补发（§11.4）。两件都在后台，就绪不等它们——它们是网络 I/O，积压多少就等多久；顺序上唯一的硬要求是**渠道登记之后**（§3）。

启动扫描就是 §8.9 的对账，它和 Cron、审批回调和手动 resume 共用同一个领取入口。领取通过数据库条件更新与递增代次保证同一 Run 只被一个执行者接管；旧代次不能继续提交新状态。单实例使用进程锁与数据库事务即可，不引入跨机器选主或分布式队列。

领取就是下面这条语句，`rows affected == 1` 是接管成功，`== 0` 是别人先到（表名 / 列名以 store 的模型为准）：

```sql
-- RunQueue::claim：领取 = 条件更新 + 递增代次 + 起租约
UPDATE runs
   SET state            = 'running',
       claimed_by       = ?1,   -- 本次启动身份（执行实例 ID）
       claim_generation = claim_generation + 1,
       claimed_at       = ?2,
       lease_until      = ?3    -- ?2 + 租约；跑的过程中由心跳续租
 WHERE id               = ?4
   AND claimed_by IS NULL
   AND (state = 'queued'
        OR (state = 'waiting' AND wait_kind = 'retry' AND wake_at <= ?2))
   AND EXISTS (SELECT 1 FROM sessions s
                WHERE s.id = runs.session_id AND s.state IN ('active','closing'))
```

候选由一条普通查询给出，领取一个一条。**"现在就能跑"这件事只有一处定义**，两份查询同一个谓词；同 Session 的次序也在这里（`earlier` 那段，§8.4）：

```sql
-- RunQueue::due
SELECT r.id FROM runs AS r
 WHERE r.claimed_by IS NULL
   AND (r.state = 'queued'
        OR (r.state = 'waiting' AND r.wait_kind = 'retry' AND r.wake_at <= ?1))
   AND EXISTS (SELECT 1 FROM sessions s
                WHERE s.id = r.session_id AND s.state IN ('active','closing'))
   AND NOT EXISTS (
       SELECT 1 FROM runs AS earlier
        WHERE earlier.session_id = r.session_id
          AND earlier.id         < r.id
          AND earlier.state NOT IN ('completed','failed','cancelled','abandoned'))
 ORDER BY r.wake_at
 LIMIT ?2
```

领取之后，这个执行者写账本的每一条状态提交都带同一道代次围栏；`rows affected == 0` 意味着自己已经是旧代次，**停止这个任务的一切写入**，不重试、不降级。状态与"在等什么"一次写完——它们本来就是同一条事实的两个维度：

```sql
-- 代次围栏（每次状态提交；wait_kind / wait_ref / wake_at 只在 state='waiting' 时有值）
UPDATE runs
   SET state = ?1, wait_kind = ?2, wait_ref = ?3, wake_at = ?4, updated_at = ?5
 WHERE id = ?6 AND claim_generation = ?7 AND claimed_by = ?8
```

回收**只交还领取权，不判状态**：

```sql
-- 启动回收：旧实例遗留的 running
UPDATE runs SET claimed_by = NULL
 WHERE state = 'running' AND claimed_by IS NOT NULL AND claimed_by <> ?1

-- 租约过期：同一个实例里 handler 任务死了/卡住了
UPDATE runs SET claimed_by = NULL
 WHERE state = 'running' AND claimed_by = ?1 AND lease_until < ?2
```

交还之后那一行的形状是 **`running` 且 `claimed_by IS NULL`**——一个可查询的孤儿（§8.9 的 `unowned_running`），它**不可领取**，所以不存在"调度器抢在 reconcile 前面把它领走"的竞态；同一次对账立刻按 §8.4 把它落成 `queued`（尾部安全）或 `waiting + intervention`（副作用不明）。**判断在这里，不在 SQL 里**：那条 UPDATE 看不见 JSONL 尾部，也看不见上一条调用停在哪。

**租约只用来发现"没人管了"，不用来抢活。** handler 每 `TTL/3` 续一次 `lease_until`；对账见到过期只是**信号**，还要过一道"这个持有者真的不在（`claimed_by <> self`）或我自己内存里已不再持有它"才回收。少这一道，一次二十分钟的调用会在主人还活着的时候被第二个执行者抢走——那不是恢复，那是真的重复副作用（§8.6）。

四条都通过 `toasty::sql::statement(...).bind(...).exec(&mut tx).await?` 执行，`u64` 就是受影响行数。**竞争失败的第一手信号是错误而不是 0 行**：并发竞争同一行时，败者拿到 `serialization failure`（MVCC 事务里是 `Write-write conflict`，autocommit 是 `database is locked`），只有在胜负已分之后再跑才会拿到 `0`。因此 `claim` 也包在 `with_write_retry` 里——它是少数几个「可以安全重跑的写」，因为守卫写在 `WHERE` 子句里：重跑一次要么还是没人领（赢），要么已经有人领了（`0`，判为输）。实测 120 轮（raw）+ 100 轮（走 toasty）、每轮 2–4 个竞争者，恰好一个赢家，没有 0 赢家也没有多赢家（`spikes/store.md`）。

JSONL 追加和状态提交都校验领取代次。领取代次只防止旧执行者写账本，不能撤销已经发出的外部动作。因此必须先确认旧执行已停止，再考虑启动替代执行。

首版不承诺让 shell / Python 进程本身跨 Gateway 重启继续存活。正常停机时停止接收新执行，给正在完成的工具短暂收尾时间，持久化可用结果；超时则回收所属进程及子进程，剩余任务交还领取权、由对账判成 `queued` 或 `waiting + intervention`。关键文件原子修改完成后再停，不在半次替换中主动取消。

异常退出可能遗留子进程，不能仅凭一个 PID 判断是否为原进程；结合平台监督信息、启动身份和进程启动时间核对。无法确认旧执行已结束时，阻止该任务重复启动并显示原因。Tokio 的 Child handle 被丢弃并不默认终止进程，因此 kill_on_drop 不能取代异常退出和进程树回收方案。[Tokio process](https://docs.rs/tokio/latest/tokio/process/index.html)

长脚本要拆成多个可观察的工具步骤，或由已审核脚本保存阶段进度。首版恢复到工具调用边界，不恢复 Python 内存、调用栈或某一行。脚本只有“进度标记”而没有副作用核对，仍不足以保证安全续跑。

### 8.8 恢复所需的上下文与用户体验

未完成 Run 引用的 Session 目录及其 JSONL、payloads、tool-output、产物，还有外部工作目录、代码与环境快照、审批和配置，都不可被普通清理策略删除，**也不许直接 `rm`**：删除只有 `komo session delete` / `purge` 一条路，走 §8.10 的状态阶梯。tool-output 不作为临时日志按天直接清扫；已结束会话清理时也先处理恢复、Memory 和审计引用，显式清理后不得将缺失输出伪装成仍可回放。目录丢失、脚本版本不可用、权限被撤销或模型协议不再可回放时，保存具体原因并等待处理，不能换到另一个目录或新版脚本自动重来；**目录丢了而 Session 状态不是 `purged`，是"数据库与内容对不上"的一种，由 reconcile 报出来（§8.9）**，不是让下一次运行对着空上下文说话。

原始请求来源与操作者身份也要保存。Cron 恢复后仍是 Cron，不能因为重启后由本机 Gateway 发起，就取得交互操作者的额外权限。Memory 的过期、遗忘和版本检查照常进行。

CLI 不承担恢复调度。打开 komo 时可以看到“2 个任务已接续，1 个等待审批”；komo resume SESSION_ID 连接到原任务，多个客户端同时连接不会开启多个执行。自动恢复无需再次询问是否继续，只有新增权限、过期动作、冲突或无法核实的效果才打断用户。

**退出 TUI 就是暂停（`Ctrl-C`）**：Run 留在 Gateway 里照跑，退出时印出 `komo resume <会话>`——会话 Id 平时只在身份行上闪过一次，退出这一刻正是需要它的时刻。暂停在任何界面状态下都有效，审批弹窗开着也一样（弹窗里的 `Esc` 是拒绝，不是退出）。一个字没发就退出时还没有会话，什么都不印。

SSE 从已同步且索引完成的 JSONL 事件补读；审批当前状态可直接查询 state.db，不依赖用户一直在线。之后接入 Telegram 等主动推送渠道时，应为待发送结果持久保存投递记录，不能为补发一条结果消息重跑任务。

用户处理 uncertain 时，应先看到原操作、已有证据和待确认事项。如果决定终止，则补齐明确的取消 / 未知结果；若允许继续，也不能伪造原调用成功。任何后续模型请求都保持完整的调用与结果配对。

### 8.9 reconcile：DB 是权威，其余一切都是派生

权威只有一处（§8.1）：**state.db 说"应该是什么"，Session 目录说"内容是什么"**。两者之间没有跨文件事务——先 `sync_all` 再提交数据库（§8.5），所以任何时刻崩掉都可能留下"半个事实"。今天处理半个事实的办法是**报损坏、然后停下**；它对真正的损坏是对的，对**没有主人的状态**（orphan）是错的：一条 handler 任务自己 panic 掉的 `running`、一个被手工 `rm -rf` 掉的会话目录、一条永远不会有人来答的 `waiting + intervention`，都会让整个 Session 从此停在那里，而原因只落在日志里。

reconcile 是一次**只读观察 + 只写状态**的对账，它回答三个问题，且只回答这三个：

| 观察 | 结论 |
|---|---|
| 这条未完成的 Run，它的 Session 还在服务范围里吗（§8.10：只有 `active` / `closing` 服务）？内容读得出来吗（目录、JSONL、被引用的输出）？ | 能 → 交给 §8.4 决策表逐行走；不能 → **不许领**，把这条 Run 停成一个 `blocked` Intervention（§7.5），理由写清是"会话已删"还是"内容缺失" |
| 这条 `running` 的 Run，它的 `claimed_by` 还活着吗（租约没过期，或过期了但持有者确已结束）？ | 不是 → 按 §8.7 **只交还领取权**（状态仍是 `running`、没有主人 = 一个可查询的孤儿），再由 §8.4 决定续跑还是核对 |
| 这条停在等待上的 Run，条件满足了吗？ | `retry` 到点了 → 放回 `queued`；`dependency` 的前一条进终态了 → 放回 `queued`；都还没 → 原样不动 |

**它永远不做四件事**：不调用工具、不发送外部请求、不消费授权、不改写任何内容——§8.5 对回放的限定原样适用。它写下的每一条状态都走正向顺序（先 JSONL 后数据库，或纯数据库的状态提交），并且幂等：同一批输入跑十遍与跑一遍结果相同。

**触发点三个**：Gateway 启动（§8.7 那次扫描就是它）、`AUDIT_TICK` 那一拍的周期兜底（§8.5 的补写周期顺手做一次，代价是几十条 `SELECT`）、以及显式的 `POST /v1/reconcile`（`komo doctor --reconcile`）。首版**不做**异步 GC、不做引用计数、不做跨 Session 死链自动修复——那些不是当前的主要矛盾。这里要求的就是三件事：**逻辑删除可靠、不重复副作用、orphan 能被自动发现并说清**。

一条硬约束顺带落在这里：**领取一个 Run 之前必须确认它的会话还在服务**。今天的 `DUE_SQL` / `CLAIM_SQL`（§8.7）只看 Run 自己的状态与同 Session 的先后次序，于是"目录被手工删掉、Run 还在队列里"会被**照常领走并按空上下文执行**——那是把一个已经不存在的对话续上一轮，比停下来更糟。两条 SQL 都要加"Session 存在且状态可服务"的守卫；reconcile 负责把这种情况**说清楚**，守卫负责让它**不发生**。

### 8.10 Session 生命周期：Closing → Deleted → Purged

Session 不是只能追加的对象。删掉一个会话今天等于 `rm -rf sessions/{id}`：数据库那一行还在、未完成的 Run 还在队列里、下一轮模型请求会带着空上下文跑起来，而操作者手上没有任何一条命令能做对这件事——这正是 §7.5 开头那个现象的另一种形态。给出三个状态，**删内容只能是最后一步，而且必须有人明确下令**：

| 状态 | 谁能进 | 新输入 | 队列 | 内容 | 列表 / resume |
|---|---|---|---|---|---|
| `active` | 默认 | 接受 | 正常 | 在 | 列出，可 resume |
| `closing` | `komo session delete`（逻辑删除的受理） | **拒绝** | 未完成的 Run 照 §8.4 跑完或停在等待；**不新开** | 原样不动 | 列出并标注"正在关闭"，可 resume 看最后一程 |
| `deleted` | `komo session delete --now`，或 reconcile 判定"已无未完成 Run" | 拒绝 | 空 | 仍在——逻辑删除**不碰内容** | 默认不列出（`--all` 可见），resume 拒绝并说明 |
| `purged` | `komo session purge <id>`，且前置检查全过 | 拒绝 | 空 | **已删除**，只剩墓碑行 | 不列出；`komo session show <id>` 仍答得出它曾经存在、何时被回收 |

四条规则：

1. **状态权威是 `sessions.state` 一列**（`ALTER TABLE ADD COLUMN`、`NOT NULL DEFAULT 'active'`，§8.2 只允许加列），外加一个"状态变更时刻"。JSONL 里追加 `session.closing` / `session.deleted` / `session.purged` 作为**审计副本**——与审批同向（§8.5 的反向补写）：数据库先提交，事件随后补写；读事件**不产生状态**。
2. **`closing → deleted` 是 reconcile 的判定，不是时钟**：只有这个 Session 再没有非终态 Run 时才推进。在那之前它一直是 `closing`，而"还有谁没跑完"正好是 §7.5 清单答得出的。操作者要立刻走完，`komo session delete --now` 把未完成的 Run 全部按 `abandon` 处置（各写一条明确的取消），再进 `deleted`。
3. **`purged` 之前先算引用，而且状态先落、内容后删。** `memory_evidence` 指向这个 Session 事件的行、`checkpoints`、`deliveries` 里未送出的行、`cron_firings` 的 Session 引用都要先处置——停用无法再验证的记忆条目、把投递标成终止——然后：**数据库先提交 `purged` 这个墓碑，再删内容**，最后对账把"标了 `purged` 而内容还在"的会话收尾（幂等，删一半被杀也不会留下说不清的状态，因为 `purged` 已经声明了"这个目录要没了"）。**删不掉就说清楚，不假装成功**（§8.8 的原话照旧）；引用检查不过就 409 并列出要先处置什么。`purged` 之后 `jsonl_path` 这类列保留原值，但那个目录不该再被创建或读取：任何试图往 `purged` 会话写内容的路径都是 bug，不是"重新开始"。
4. **禁止直接 `rm`。** 三层：命令层——删除只有一条路（`komo session delete` / `purge`），没有"手工删目录"这条经验路径；Policy 层——默认规则里工具对数据目录（`sessions/`、`state.db*`、`runtime/`、`.env`）的写入是 `Deny`，shell 的灾难形状由 §7.1 的 `command_patterns` 再兜一层（**那是手滑网，不是墙**，§7.3 已经说过它认不出变量拼出来的命令）；reconcile 层——真正的保证在这里：目录没了而状态不是 `purged`，对账会把它**报出来**，而不是等下一轮模型对着空上下文说话。

## 9. Memory：自动积累、可信来源与混合检索

### 9.1 两类内容，分别保存

| 内容                               | 保存位置                                      | 使用方式                                                  |
| ---------------------------------- | --------------------------------------------- | --------------------------------------------------------- |
| 用户主动要求保存的笔记、想法和记录 | 现有 Memos                                    | 通过已审核的 toolbox.memos 保存、搜索和读取，返回原文链接 |
| 自动积累的偏好、项目事实和执行经验 | 本机 state.db 的 memory_items / memory_evidence | MemoryManager 提取、校验、召回、更新和遗忘                |
| 本次任务的工作状态                 | Session / Run / ToolCall                      | 用于上下文与 resume，不自动等同于长期事实                 |
| 关键词与向量索引                   | 同一个 state.db                                 | 从有效记忆重建，不能取代原文与来源                        |

长期记忆和当前激活的上下文分别管理：Context Window 是模型请求的容量限制；Working Memory 是本次任务实际选入的历史、状态、相关记忆和工具结果。不能把整个记忆库塞进每轮请求。

首版向量检索覆盖自动 Memory，以及其中已经保存的 Memos 来源摘要。它不表示已索引全部 Memos 内容；查找主动笔记仍查询 Memos。Memos 全量语义索引需要另行明确导入范围，当前不增加同步器。

### 9.2 记忆的数据模型

确认状态与来源分开保存，避免把“模型从用户原话整理”误写为“用户确认了模型摘要”。

| 字段                      | 含义                                                                                  |
| ------------------------- | ------------------------------------------------------------------------------------- |
| id / revision             | 稳定 ID 与递增内容版本；修改产生新版本                                                |
| content / kind            | 简短、尽量单一的陈述；preference / fact / experience                                  |
| scope                     | personal、project 或 environment，绑定当前操作者、稳定项目 ID 或实例 ID               |
| provenance                | user_statement / tool_observation / model_inference                                   |
| confirmation              | unconfirmed / user_confirmed                                                          |
| state                     | candidate / active / contested / superseded / forgotten                               |
| evidence_refs             | JSONL 的 Session ID + event_id / seq、工具产物或 Memos 实例 / 记录 ID、时间与来源版本 |
| observed_at / valid_until | 事实的观察时间与可选有效期，区别于入库时间                                            |
| created_at / updated_at   | 记忆管理时间                                                                          |
| extraction_metadata       | 提取模型、effort、提示词版本、处理来源游标                                            |
| usage                     | 使用次数与最近使用时间，只度量使用情况，不增加真实性                                  |

证据与内容版本分别保留。用户确认绑定具体 id / revision 和操作者事件；模型返回的 user_confirmed 字段没有写入权限。明确的用户原话可标为 active + user_statement + unconfirmed，展示为“自动整理自用户陈述”；模型推断默认 candidate，不作为已确认事实注入。

用户主动保存到 Memos 表示“要求保留这段内容”，其中可能有引用、假设或待办，不等于确认了其中每一句话。只有用户明确确认的具体陈述才能获得 user_confirmed。

NAS 和 Mac 的自动 Memory 各自独立。项目作用域使用实例内稳定的项目 ID，可映射多个临时仓库副本；工作路径相似不自动意味着同一项目。

### 9.3 自动积累与更新

MemoryManager 对 AgentRuntime 提供三个主要入口：recall、process_completed_run、apply_user_decision。提取、去重、证据校验、冲突处理与索引协调隐藏在该模块内部；它不向 LLM 注册额外工具。

处理流程：

```text
run.completed 在 JSONL 持久保存后，state.db 提交终态与 memory_work = pending；索引补齐时也幂等补上该标记
  → 后台领取该 Run 尚未处理的新证据
  → 记忆模型提取结构化候选，不提供执行工具
  → Rust 校验来源角色、引用位置、作用域与内容
  → 准备内部 Memory 变更计划并检查 Policy
  → 按来源游标与预期 revision 事务提交
  → 标记相关索引待更新
  → 后台生成向量，确认版本未变后入库
```

runs 中记录 pending / processing / done / error 与处理游标。进程崩溃后可重新领取，重复处理相同来源不重复新增；索引失败独立重试，不撤销已经保存的记忆正文。

提取来源限定为新发生的用户陈述和可验证工具结果；已有记忆注入、助手摘要、模型生成报告和维护任务自己的输出不算新的独立证据。取消、失败或结果未知的动作不能整理成成功经验。Cron 只在任务授予的范围内积累有证据的事实和经验，不能从自己的报告推断用户偏好。

自动整理不保存原始密钥、令牌或无关敏感输出。HA 当前开关状态等易变信息应在执行前实时查询，不当作长期设备事实。发送给记忆模型或向量端点的文本仍受来源与端点授权约束，不因“内部维护”自动绕过 Policy。

### 9.4 关键词与向量混合检索

```text
当前用户输入 + 少量任务上下文
  → 限定操作者、作用域、active 状态与有效期
  ├─ 关键词召回：memory_terms 的 instr 匹配 + 标签 / 精确值 / 短字符串匹配
  └─ 向量召回：独立 embedding 模型 → 当前向量空间 → 余弦相似度
       → 按排名融合（RRF），避免直接相加不同量纲的分数
       → ［可选］判断层重排：短名单比注入条数宽，只重排、不增删
       → 去重，核对当前 revision、来源与冲突状态
       → 按条数和 token 预算选入上下文
       → 返回内容、来源、确认状态、时间与版本
```

默认 hybrid；同时支持 keyword 和 vector 供明确选择与诊断。每路建议最多召回 40 项，合并后最多注入 8 项、约 1500 tokens；这些是可调整的初始预算，不强行填满。没有足够相关内容时返回空；相似度只用于相关性，不授予真实性或操作权限。

关键词索引不用 FTS5（Turso MVCC 下不可用，§8.2）：索引时把内容经 `lexical_terms` 切成 token 串写入 `memory_terms.terms`（首尾带空格），查询时每个 token 一个 `instr(terms, ' tok ') > 0`，命中数按 IDF 加权。`lexical_terms` 对 CJK 连续字符切 bigram、对 ASCII 切单词并小写化——"空调"本身就是一个 bigram，两字查询天然命中，不需要子串后备。IDF 在查询时用 `memory_terms` 行数和每个 token 的命中行数算；1 万条量级是一次全表 `instr`，§14 要求实测 P95，这里不预先宣称达标。

**重排是可选的一步，而且是"只重排、不增删"。** 融合（RRF）能找到候选，却排不出细微的先后：两条讲同一件事的记忆可能只差一位。开着 `memory.retrieval.rerank` 时，短名单（`rerank_shortlist`，默认 20，必须比 `top_k` 宽——一样宽只是换个顺序，校验会拒绝这种组合）交给一个**判断后端**（`[typesafe]`，TypeSafe「System One」/ Jev）：一次请求、一条 `Choice`，选项是短名单里每条记忆的 id 加一个 `none`，`state` 里是这次的用户输入与各条正文的一行摘要；回来的概率表**本身就是一张排序**，代码按它排序后照旧按 `top_k` 截。四条边界写在实现里：

- **候选集合与融合结果逐条相同**——这一步只决定"谁进得了 `top_k`"，丢候选是阈值的事；
- 它挑中 `none`（都不相关）时**不采纳它的排序**，保持融合顺序；
- 判断层不可用（连不上 / 超时 / 答复解不开）时同样保持融合顺序，只留一条 warn——**"判断不可用"绝不能变成"没有相关记忆"**，也不能让这一次召回整个失败；
- 判据是路径与开关两件事：`rerank` 关着时**一个请求都不发**（后端装在手上也不发）。

代价要写清楚：开着它，**候选记忆的正文（截到 200 字符）会被发到第三方端点**——所以它默认关，凭证据只从 `.env` 读（`TYPESAFE_API_KEY`，与模型后端同一约定：快照里只有变量名）。本机实测（2026-09-21，4 条候选、中文输入）：一次请求 0.6s、约 650 输入 token，Jev 把"热水器/空调"那两条排到了前面。

首版将向量保存为带编码和维度信息的 f32 数据，由 Rust 在作用域过滤后做精确余弦检索。进程可缓存当前代次的向量，缓存按预算管理且可从数据库重建；无须部署独立向量服务。后续只有在真实数据规模与延迟测试表明必要时，才在 memory_index 内部替换为近似检索。

向量服务短暂故障时，hybrid 退化为关键词并在检索元信息中标记 degraded / 原因 / 覆盖率；vector 模式明确返回不可用。未配置向量模型却选择 hybrid / vector 属于配置错误，不能静默变成长期关键词模式。

**"配了 alias 但后端还没在手上"算前一种，不算配置错误。** 维度探测（§9.5）在 Gateway 就绪之后的后台跑（§3），探测落定前 hybrid 照常退化为关键词、vector-only 照常报"后端不可用"；把这一段时间报成 `VectorUnconfigured` 会把"等一个还没回来的人"说成"你把配置写错了"，也让这一轮召回整条失败而不是降级给出关键词结果。

后台刚入库但尚未生成向量的记忆仍可通过关键词命中。每次模型请求前复查被选条目的有效性，**同一段对话里沿用并逐字复用**那一次的注入——注入段拼在 system 消息里，而服务端的前缀缓存按**最长公共前缀**命中：每轮重新召回、重新渲染（哪怕只是条目顺序或某条的记忆 revision 数字变了），整个请求从第一条消息起就与上一次不同，对话历史那一大段前缀的缓存全部失效，钱与延迟都付在重复的前缀上。所以召回与渲染的单位是**一段对话**（`conversation.boundary` 之间，聊天里的 `/new` 划开），不是一轮：同一段里一个字节都不改，换段或这一段第一次用时才重新召回，也不逐轮重复请求 embedding。代价写在这里：本段内新记下、或本来召回到了但这一段没选中的条目，要**下一段**才进上下文；被改写的条目在本段内仍按原样注入（正文里带 `id@revision`，读的人知道引用的是哪一版）。要立刻刷新，`/new` 开一段就行。**遗忘不在这个代价里**：沿用的每一轮都要照 §9.4 核对被选条目的有效性，被遗忘或失效的条目立刻把它那一块作废重算——"立刻停用"（§9.2）优先于前缀稳定，验收项 `a_forgotten_memory_never_comes_back_into_a_turn` 钉的就是它。

### 9.5 向量索引与模型切换

一个 embedding_space 指纹至少包括：

- 协议与端点身份、模型 ID、可获得的模型版本或操作者指定的 revision。
- 实际维度、文本预处理版本、文档 / 查询前缀规则、向量归一化与距离规则。
- 影响向量生成的参数，包括实际支持时的 effort；凭证值不进入指纹。

查询与文档必须使用同一空间规定的模型与各自输入规则。同维度不代表同一空间；服务端复用模型名称但换了权重时，也需要更新 revision 或明确强制重建。

memory_vectors 按 memory_id + memory_revision + content_hash + generation_id 定位。生成任务在事务外调用模型，入库前检查正文、版本、状态与指纹未变；不满足则丢弃过期结果。校验返回数量、维度、有限数值和非零范数，不能接受截断或结构错误的向量。

更换向量模型、维度或预处理方式时：

1. 创建新索引代次，旧代次保持独立；当前查询不能把新 query vector 与旧向量比较。
2. 按稳定游标批量重建，在提交时检查内容版本，更新期间的增量也纳入新代次。
3. 重建中使用关键词维持查询，展示向量尚未就绪的状态。
4. 新代次追平截至切换时的有效条目后事务切换，随后按保留规则清理旧索引。
5. 重启后继续未完成代次；失败不删除记忆正文，也不伪装成索引已完成。

模型调用期间不持有数据库事务。并发修改的条目如果无法及时向量化，先只走关键词，不能使用旧内容向量。切换后新增条目继续增量更新。

只更换记忆整理模型或其 effort，不需要重建全部向量；只有它产生并被接受的内容修改才触发对应条目的更新。

### 9.6 冲突、失效与遗忘

重复来源幂等合并；不同时间发生的同类事件仍分别保留，例如两次更换滤芯。判断冲突前先检查作用域和时间区间，不能把不同项目的偏好合并。

新推断不能覆盖用户陈述，自动整理不能静默改写用户已确认内容。存在实质冲突时保留证据并标记 contested，暂停正常召回，等待用户纠正或确认新版本；旧版本随后标为 superseded。使用次数和多次模型复述不能增加确认等级。

关联 Memos 的记忆在引用原文前重新读取并核对更新时间或内容哈希；原文改动使关联摘要待更新，删除或失去访问权限时停止把它当成当前有效证据。网络故障只能标为无法验证，不能推断原文已删除。当前不实现跨实例自动同步。

komo memory confirm / forget 携带预期 revision。confirm 必须来自受信任的操作者交互；Agent 通过 shell 调用管理命令只能请求确认，不能自签用户确认事件。工具子进程不注入操作者管理凭证。

forget 立即停用内容，并使关键词、向量缓存与检查点中的引用失效。保留最小排除标记，阻止同一来源被后台重提取；后来出现相同主张也先进入候选，不能自动恢复为有效记忆。

遗忘自动 Memory 不等于删除 Session 原文或 Memos 笔记。原对话仍可能出现在用户主动恢复的历史中，界面必须说明遗忘范围；删除原始记录另行按 Policy 操作。

### 9.7 与 resume 和权限的关系

模型请求与检查点保存记忆 ID / revision 和检索配置版本；resume 重新核对当前状态、有效期与来源，过期检查点不能恢复已经遗忘的记忆。历史请求保留当时用了哪些条目的审计证据，但新请求只重新激活当前允许的版本。

Memory 内容以带来源的数据进入上下文，不能成为系统指令、Policy 授权或自我更新工具的依据。“用户偏好自动执行”这样的记忆也不能替代有效审批。

## 10. Cron

Cron Scheduler 只负责产生 Run，复用 AgentLoop、Policy、工具和存储。

```text
发现到期
 → state.db 事务插入 cron_firing、触发快照并预留 Session / `accepted` Run
 → JSONL 持久保存触发输入，state.db 提交 queued
 → 普通队列执行
 → 保存结果和产物
 → 更新本次触发状态
```

每个 Job 包含名称、五字段 cron 表达式、时区、prompt、工作目录、enabled、执行预算、重叠策略、`notify` 及版本化授权。`notify` 三档 `always`（默认）/ `on_error` / `never` **只过滤结果的投递**；Run 停在任一待处理 Intervention（审批、结果不明、阻塞，§7.5）上时三档都投 home chat——那是任务在问，不是在报告，一条没人看见的提问等于这个 Job 从此停在那里。每次触发是一条带状态的记录（`queued` / `running` / `ok` / `error` / `waiting` / `skipped`），重叠跳过与错过太久（超过 Job 自己的间隔）都留一条 `skipped`，`@at` 一次性永不过期。可指定该 Job 的主模型与 effort；覆盖按完整模型配置解析，不能影响记忆整理或向量模型。下面示例所需的搜索与 Memos 操作仍须匹配具体模块版本及授权。

```bash
komo cron add --name morning-summary \
  --schedule "0 9 * * *" \
  --timezone Asia/Shanghai \
  --prompt "搜索今天关注的技术动态，整理后保存到 Memos"

komo cron list
komo cron run JOB_ID
komo cron pause JOB_ID
komo cron resume JOB_ID
komo cron remove JOB_ID
```

模型需要管理 Cron 时通过 shell 调用这些命令；CLI 连接 Gateway，不直接访问数据库，也不增加专用 cron tool。修改调度属于持久副作用，执行计划展示完整调度与指令，并经过 Policy。

默认行为：

- 唯一键 `job_id + scheduled_at_utc` 防止同一计划时间重复创建运行。
- 同一 Job 上一次仍未结束（含等待审批、重试或结果核对）时，跳过本次并记录原因。
- Gateway 停机期间错过的触发不集中补跑；停机前已经持久创建、尚未完成的 Cron Run 则按原 Run 自动恢复，这是两种不同情况。
- 时区明确保存（IANA 名，`TimeZone` 只存名字）；同一日历时间因夏令时出现两次时，两次不同 UTC 时刻分别视为计划时间；不存在的本地时间跳过。**这两条与 croner 的内建策略相反**（它把缺口里的固定时刻抬到缺口之后、把重复的只算较早一次），所以由 kernel 的 `cron::search_from`（跳缺口）和 `cron::repeated_twin`（补后一半）自己实现，换 croner 版本时要一起看。时区解析走 `ZoneResolver` port（§13.5）：kernel 不带 tzdb，真 tzdb 实现在 runtime；没有 tzdb 时 `FixedOffsetZone` 是**明确的降级**（答不出重复与缺口，等于当这个区没有夏令时），不是默认。
- 手动 run 使用独立请求幂等键，不冒充定时触发。
- 新增危险操作暂停等待审批，不能因无人值守而自动放行。
- Job、模块或权限发生变化时重新匹配授权。
- Cron 的结果去原 Session 查看，也可以用 resume 接手。

触发输入在 JSONL 持久保存且 queued 已提交后再通知内存队列；启动与周期扫描负责补齐未完成接收、索引落后和通知丢失的情况。

## 11. 聊天入口与审批

聊天渠道复用 Gateway 的会话、运行和审批接口，并且是**审批的主入口**——操作者大多不在终端前。TUI 与 CLI 是人在终端前时的辅助入口。三个渠道首版都在。

### 11.1 渠道与分发

```text
飞书 ws 长连接 ─────────┐
Telegram long polling ─┤
WeChat iLink (DM) ─────┼─→ Dispatcher::handle(InboundMessage)
HTTP API (TUI / CLI) ──┘      1. request_key 去重（durable，§8.5 的请求键）
                              2. 解析 Principal（config.toml 的 `allow_from`）
                              3. 解析 Conversation → SessionId
                              4. 冲刷该会话 pending 的投递（§11.4）
                              5. 命令？→ 直接处理并 ack
                              6. 普通文本 → Ledger::accept_input → 排队
                       回复：订阅该 Session 的事件流，Run 终态时把最终 assistant 消息发回来源会话；
                             待处理 Intervention（审批、结果不明、阻塞）走 Notifier（§11.4）
```

- `request_key`：飞书是 `feishu:{event_id}`（`header.event_id`，消息事件与卡片回调同一个字段；官方对 2.0 事件的去重建议就是「通过事件结构中的 `event_id` 字段判断事件唯一性」），Telegram 是 `telegram:{update_id}`（long polling 重投的单位是整个 `Update`，`offset` 未推进就原样再取一次，普通消息与 `callback_query` 因此共用这一个键），WeChat 是 `wechat:{from_user_id}:{client_id}`——iLink 的 wire 消息**没有 `msg_id`**，字段只有 `from_user_id` / `to_user_id` / `client_id` / `create_time_ms` / `message_type` / `message_state` / `context_token` / `item_list`，唯一的逐消息标识是 `client_id`（**发送方**微信客户端生成的 UUID，不是服务端投递 id），所以加 `from_user_id` 前缀，`client_id` 为空时回退到 `(from, 内容哈希, 60s 窗口)`；微信的重投有两个来源——拉取游标只活在进程内存里、重启从空游标开始，以及服务端回了消息却回空游标时 SDK 把同一批原样再交一次——所以微信的去重是必需的，不是以防万一。飞书 / Telegram 不带 `chat_id` 前缀：两个 id 在单个应用 / 单个 bot 内已唯一，而 Telegram inline 模式的按钮回调根本没有 chat，硬拼会逼出占位值。三个平台都是至少一次投递（飞书官方原话「即使成功接收，仍会收到重复消息」；未在 3 秒内响应会按 15s / 5min / 1h / 6h 重推最多 4 次，ws 长连接同样有超时重推），重发命中同一 Run；**命令也去重**——重发的 `/approve` 不会批准两次。这正是 §8.5 请求键的语义，不另做 inbox 表。
- **去重键只管平台重投，不管用户连点**：两次真实点击带着两个不同的 `event_id` / `update_id`，是两条合法输入。挡住它的是审批本身的幂等（§11.3：已决定的返回原决定）。两层分开，不要用组合键去兼职。
- 飞书 ws 在独立线程上跑 openlark 的运行时，事件通过 channel 交给 tokio 侧的 `Channel::serve`；ack 在 Dispatcher 返回 `InboundAck` 之后发，不是收到就 ack。
- 渠道交给 Dispatcher 的是 `InboundMessage { peer: ChannelPeer, is_private, sender, text, request_key }`，**不是 Session ID**。哪个会话属于 Dispatcher 与存储。

### 11.2 身份与会话

**渠道的身份匹配信息全部在配置文件里，不进 state.db。** 谁是操作者、哪些会话可以说话、哪个会话是 home chat，都是操作者对自己机器的一次性声明——和模型、策略规则是同一类东西，改动就是编辑文件，Gateway 热重载（§3）后立即生效，没有配对码、没有 `komo pair`、没有 `/sethome`，也没有对应的表和 HTTP 接口。原因：这份信息只有操作者一个人写，写它的场合一定是人在终端前；放进数据库只会多出一套 CLI、一组接口和一张耐久表来维护一个本来就该是静态的名单，而数据库丢了还要靠这张表决定谁能审批。

```toml
# ~/.komo/config.toml —— 行为键
[channels.feishu]
enabled    = true
allow_from = ["ou_xxx"]            # 操作者的 open_id；为空则没人能说话
home_chat  = "oc_xxx"              # 主动投递（审批请求等）的目标会话
groups     = ["oc_yyy"]            # 允许响应的群；缺省 = 只响应 allow_from 的私聊

[channels.telegram]
enabled    = true
allow_from = [123456789]           # 操作者的 user id
home_chat  = 123456789

[channels.wechat]
enabled    = true
allow_from = ["wxid_xxx"]          # iLink 的 from
```

```dotenv
# ~/.komo/.env —— 只放凭证；config.toml 里没有任何 secret
FEISHU_APP_ID=cli_xxx
FEISHU_APP_SECRET=...
TELEGRAM_BOT_TOKEN=...
# 微信凭证由 `komo channel wechat login` 写到 ~/.komo/wechat/credentials.json，不在 .env
```

- **Principal**：发送者在该渠道 `allow_from` 里就是操作者，否则**拒绝**——回一条固定提示，带上发送者在该平台的 id（`ou_xxx` / `123456789` / `wxid_xxx`），操作者把它抄进 `allow_from` 即可；消息不进入 Run，也不留任何记录。`allow_from` 为空的渠道等于只出不进：还能作 `home_chat` 收投递，但没人能通过它下指令。**审批命令只接受操作者。**
- **怎么知道自己的 id**：任何人对机器人说 `/id`，机器人回 `{platform}:{chat_id}` 与发送者 id——被拒绝的提示里也带着同样的信息。这是唯一的"发现"手段，没有别的准入流程。
- **Conversation**：操作者的**私聊**（飞书 DM、Telegram DM、WeChat、TUI）落到同一个 **home session**——早上在微信说的话，回到终端接着说；飞书 / Telegram 的群聊按 `{platform}:{chat_id}` 各自一个 Session，只有 `groups` 列出的群会被响应，群里只响应 @机器人 的消息并剥掉提及，且发送者仍须在 `allow_from` 里。WeChat 只有 DM。
- **改名单不改数据库，也不重启**：`allow_from` / `home_chat` / `groups` 每条消息、每次投递都从当前配置快照读（§3 热重载第 2 步），文件保存后下一条消息就按新名单判定；`komo config check` 与重载共用同一套校验（id 形态、`home_chat` 所属渠道必须 enabled 且有凭证），校验不过则旧名单继续生效并在 home chat 报错。`komo doctor` 把"某渠道 enabled 但 `allow_from` 为空"当作警告列出。
- Cron 触发的 Run 来源仍是 Cron（§8.8）；在聊天里 `/approve` 它的等待，不会让它获得交互操作者的权限。

### 11.3 审批在聊天里

§7.2 要求界面显示"具体动作、原因、改动和已有验证结果"。聊天里的一条审批请求是一个 `Outbound::ApprovalRequest`，内容由 executor 组装，**渲染**由各渠道实现：

| 内容 | 来源 | 飞书 | Telegram | WeChat |
|---|---|---|---|---|
| 短 ID | `approval_requests.short_id`，待处理集合内唯一，4 位 base32 | 交互卡片标题 + 按钮"批准 / 拒绝" | 文本 + 内联按钮"批准 / 拒绝" | 文本 |
| 动作 | `ExecutionPlan`：工具、命令 / 代码、真实目标路径、cwd、版本 | 卡片 markdown 代码块 | 代码块 | 纯文本；超长截断并提示到 TUI 看全文 |
| 改动 | write / edit 的 diff（截断）；toolbox 启用的版本差异与测试结果 | 卡片折叠区 | 代码块 | 同上 |
| 原因 | `PolicyDecision::Ask { reason }` | 卡片备注 | 引用块 | 纯文本 |
| 范围 | 本次调用 / 本次 Run 范围 / Cron Job（§7.2） | 按钮只给"本次"；范围授权用命令 | 同左 | 命令 |

按钮回答后，飞书卡片原地更新为"已批准 / 已拒绝 · 谁 · 何时"，Telegram 编辑原消息去掉按钮——决定过的请求不该还长着可点的按钮。

命令（三个渠道相同）：

| 命令 | 行为 |
|---|---|
| `y` / `n`（也认 `yes` / `no`） | 待处理只有**一条**时的最短答复：等价于 `/approve` / `/reject`，省掉抄短 ID——手机上那 4 位才是真正的摩擦。**只认单个词**（`y 7K2M` 是半懂不懂的写法，当普通消息），而且**没有待处理审批时它不是命令**：模型问"要不要…"、操作者回个 `n`，那是回话，不是"拒绝一条不存在的审批"。解析在网关，判断"真的有人在等"也在网关（§11.1 第 5 步） |
| `/approve <short_id>` / `/reject <short_id>` | 打 `POST /v1/interventions/{handle}/answer`；已答复的返回原结论，不报错 |
| `/answer <handle> <结论>` | 答复另外两类（§7.5）：`/answer <run_id> satisfied`（核对后目标已满足，原 Run 继续）、`not_performed`（确定没执行，重新入队）、`resolve`（前提已处理，重新观察并重新决策）、`abandon`（取消这个 Run）。`/approve` / `/reject` 就是审批类的两个结论，走同一条路 |
| `/approve` / `/reject`（无 ID） | 操作者只有**一个**待处理请求时生效；多于一个则列出并要求指明 |
| `/approve all` / `/reject all` | 打 `POST /v1/interventions/answers`，把**此刻待处理的全部审批**一次答了（§7.2）；每条各自落一条结论，回执点名答了哪几条。**批量的范围是本次调用**——要范围请逐条 `/approve <短ID> run` |
| `/approve <short_id> run` | 本次 Run 的范围授权（§7.2 第二种）；只对 Policy 标记为可范围化的计划生效，`Deny` 不可覆盖 |
| `/approve <short_id> cron` | Cron Job 的范围授权（§7.2 第三种），绑定 Job 与其版本；只对来源是 Cron 的请求出现（Policy 对 Cron 来源的 Ask 自动多给这一档） |
| `/pending` | 列出**全部**待处理 Intervention（§7.5）与各自的句柄：审批给短 ID，结果不明与阻塞给 Run ID，并写清这一条该答什么 |
| `/new` | 当前 Session 追加 `conversation.boundary`，不切 Session |
| `/cancel` | 取消该 Session 当前 Run |
| `/status` | 当前 Run 状态、待审批数 |
| `/id` | 回显 `{platform}:{chat_id}` 与发送者 id，供抄进 config.toml 的 `allow_from` / `home_chat` / `groups`；任何人可用，也是唯一不要求操作者身份的命令 |

**TUI 是同一个决策接口的第四个界面，它的答案必须自己说出来。** 聊天里的请求自带按钮（飞书）或命令（Telegram / WeChat），TUI 只有一块弹窗，所以弹窗底部是一张**答案菜单**：一行一个答案，`↑` / `↓` 移动高亮、`Enter` 确认，行首那个字母（`r` / `y` / `n` / `a`）是这一行的直通键，习惯直接按键的人不必先移动高亮。可选的行只有 Policy 真给了的那些，顺序固定——`r` 本次任务默认通过（本次 Run 范围，只在 Policy 标了可范围化时出现，且排第一 = `Enter` 的默认落点）、`y` 只批准本次调用、`n` 拒绝本条（`Esc` 同义）、`a` 全部批准（只在待处理多于一条时出现，并写出有几条）；没有这个答案就不占一行——列一个按下去没反应的键比不列它更糟。正文会滚（`PgUp` / `PgDn`），菜单与边框底栏那一行不滚：窄终端上正文几乎一定放不下，而一个滚出屏幕的「Enter 确认」等于没有提示。待处理条数同时出现在状态行上。弹窗没打开而审批还在等（请求还没投到、详情取不回来、事件流断了）时，输入框的提示行要**说出路**（`/pending` 看清单、`/approve <短ID>`、`/approve all`），而不是只显示一句"等待审批"——那是审批的主界面之外唯一还能看见它的地方。

按钮回调与文本命令走同一个 `Dispatcher::handle`，**去重键与普通消息同源**（§11.1）：飞书卡片回调（`card.action.trigger`）用 `feishu:{event_id}`，Telegram `callback_query` 用 `telegram:{update_id}`——`callback_query` 是 `Update` 的一个字段，不是比 `update_id` 更细的投递单位。回调里带的 `approval_id` 是渠道回传的数据，只用来**定位**请求；批准与否仍由 Dispatcher 核对 Principal 后决定，回调负载不构成授权。

**去重之外还有一层幂等，两层各管一件事。** 去重键挡平台重投（飞书至少一次投递，ws 断线重连与 3 秒超时都会重推；Telegram 的 `offset` 未推进就重取同一个 `Update`），幂等键挡用户连点——同一人连点「批准」两次是两条合法输入、两个不同的 `event_id` / `update_id`，只有按 `approval_id` 幂等才能让第二次得到「已决定」而不是第二次执行。因此**不**把去重键换成 `(open_message_id, action.value, operator.open_id)` 这类组合键：那是个幂等键，用作投递键会把语义不同的两次点击也静默吞掉。

**飞书卡片是 JSON 2.0（`schema: "2.0"`），而 2.0 去掉了 `note` 组件与 `action` 模块。** 官方的不兼容变更写明：2.0 不再支持 note 与 action（`tag` 为 `action`），且 2.0 对不认识的组件是**整张卡打回**而不是忽略。所以"卡片备注"（原因、有效期、结论那一行）是普通文本组件加 `notation` 字号与灰色；一行按钮是 `column_set`，每列一个 button，那一块带固定的 `element_id`——决定之后那张无按钮的卡靠它整块摘掉。踩过的坑在 §14：带 `note` 的卡片被平台拒（`230099 / 200861 unsupported tag note`），于是审批请求**一条都到不了聊天里**，而失败只落在网关日志的一行 WARN 上。

**决定后的原地更新用 `PATCH /open-apis/im/v1/messages/{message_id}`，不用回调响应体。** 飞书官方给了三条路：回调响应里直接回传新卡片（须在 3 秒内）、用回调 `token` 延时更新（30 分钟内、最多 2 次、且必须在响应回调之后）、以及无条件 PATCH（仅 `interactive` 消息、仅 14 天内发送的消息、单条 5 QPS、更新前后 `config` 均须 `update_multi:true`）。komo 取第三条：审批的决定先落 Ledger 再回写界面，这件事跨越了回调的 3 秒预算；而且 PATCH 是普通 REST 调用，与「ws 长连接还是 HTTP 回调」无关，不必赌 ws 客户端能不能回传响应帧。回调本身只需尽快返回，必要时带一个 `toast`。Telegram 侧对应 `editMessageReplyMarkup` 去掉按钮：官方的 48 小时编辑限制只约束「非机器人自己发送且不含 inline keyboard 的 business message」，机器人自己发的审批卡片不受时限；编辑失败按**非致命**处理——决定已在 Ledger 里，界面回写失败不改变结论，也不要去匹配错误文案（官方明示 `error_code` 内容将来会变）。

一条普通消息在 Run 等待审批时到达：Run 不会被越过（§6），消息排在它后面；不做"插话替换审批"这类特殊路径。

### 11.4 主动投递

§8.8："应为待发送结果持久保存投递记录，不能为补发一条结果消息重跑任务。"

- `Notifier::deliver` 先在 `deliveries` 表写一行（目标、内容引用、状态 `pending`），再发送，成功后标 `sent`。重启后 `pending` 的行补发，按 `DeliveryId` 幂等。
- **补发在就绪之后跑，而且同一时刻只跑一趟。** 就绪（§3 第 4 步）不等它：它是网络 I/O，一条一个平台往返，积压多少就等多久。三个入口——渠道起来时按平台冲刷、启动时整体冲刷、微信入站前按会话冲刷——共用一把锁：两趟并发地在同一批 `pending` 行上跑，同一行会被送两次（发送在结算之前）。
- **待处理 Intervention（§7.5）的投递目标**：Run 的来源会话，**加上** home chat（若不同）。两处都能回答，第二个答复得到"已决定"。来源是 Cron 或已断开的 TUI 时只有 home chat。审批、结果不明、阻塞三类共用这一条——"需要人判断"不该因为种类不同而有不同的到达率。
  补充一条（2026-09-19 实现并按线上反馈收窄）：**屏幕前有人在看这个会话时（HTTP 的 SSE 订阅在）不投 home chat**。TUI / HTTP 来源的 Run 没有 chat 对端，照上面那句它只剩 home chat 一个出口，而那条卡片是一次网络往返（实测几秒到几十秒），人正盯着弹窗的时候它只会晚到、答完还多一条回写。判据是**客户端订阅数**（`EventHub::watch` 的守卫，HTTP SSE 处理器持有），不是广播的订阅者总数——Run 的看客自己也订阅同一个广播。人走掉之后仍挂着的审批由周期（与审计补写同一拍）补投，投过的不重复：§10 的"不能因为无人值守就没人知道它在等"没有被这条放松。
- home chat 解析：只看配置——每个 enabled 渠道的 `home_chat`（§11.2），没有运行时覆盖；一个都没配时返回错误给调用方，**不静默丢弃**。多个渠道都配了时的默认顺序是**飞书 > Telegram > WeChat**：前两者能通过 API 对任意已加入的会话主动推送，微信不能（下一条）。
- **WeChat 的平台约束**：DM 回推依赖回复令牌 `context_token`——每条入站消息自带一个，SDK 按 `user_id` 存在**进程内存**里，源码里没有过期逻辑，唯一的失效路径是会话过期（`errcode -14`）清空整张表；真正挡住主动推送的是**进程重启即全丢**，不是令牌到期。没有令牌时 `send` 直接返回 `NoContext`，不会联网去补——这就是 `Deferred` 的精确触发条件（按错误**变体**匹配，不按字符串）。因此进程启动后用户没发过消息时**无法主动推送**。对应处理：该渠道的 `deliver` 在没有令牌时把行留在 `pending` 并返回 `Deferred`；用户下一条消息到达时 Dispatcher 先冲刷该会话的 `pending` 投递（§11.1 第 4 步），再处理新消息。审批请求因此不会丢，只会晚到；`home_chat` 里排在它前面的飞书或 Telegram 会先送到。待验证：服务端是否接受跨进程的旧 `context_token`——若接受，把每个 `user_id` 的最新令牌持久化到 state.db 即可让重启后的主动推送直接成功，`Deferred` 退化为「这台机器从没收过这个人的消息」一种情形。另：SDK 的 `message_type` / `message_state` 枚举没有 unknown 兜底，服务端多一个值就整批反序列化失败并无限退避重试，渠道会静默卡死——komo 的 wechat 渠道把连续 N 次 JSON 错误升级为 home chat 告警。
- TUI 的 SSE 是另一个 Notifier 实现，不持久化——连接断了从事件流补读。

### 11.5 渠道实现的约束

每个渠道一个 **feature 门控的模块**，SDK 依赖只在该 feature 下进入构建；渠道模块只能依赖 kernel 的类型和 `Inbound` / `Notifier` 两个 trait（§13.5），不能碰 runtime。`cargo tree -d` 有新重复版本时必须在 PR 里按渠道归因。`komo channel list | probe` 对每个已配置渠道做一次连通性核对（飞书拿 tenant token、Telegram `getMe`、微信检查凭证文件），`komo doctor` 汇总。

## 12. 数据目录与记录习惯

```text
~/.komo/
├── config.toml          # 行为配置：模型、渠道的 allow_from / home_chat / groups 等
├── .env                 # 凭证：飞书 app secret、Telegram bot token、模型 key
├── policy.toml
├── state.db             # Turso；调度状态、审批、投递记录、Cron、Memory 与内容索引
├── sessions/
│   └── <session-id>/
│       ├── events.jsonl # 调用、状态、输出引用、简短预览与消息
│       ├── payloads/    # 大参数、大模型回复和大计划正文
│       ├── tool-output/
│       │   └── <run-id>/<call-id>/<attempt-id>/
│       │       ├── output.json # 完整结果或错误正文、执行身份、文件引用
│       │       ├── stdout.txt  # 按需保存
│       │       └── stderr.txt  # 按需保存
│       └── artifacts/  # 当前会话的报告、脚本快照和生成文件
├── runtime/             # 发现文件、锁等
├── toolbox/             # 保存的 Python 能力及说明
├── skills/              # 人写的 SKILL.md（§5.6）
├── wechat/              # 微信登录凭证
├── python-envs/         # 受管理的环境版本
├── workspaces/          # 项目副本和普通工作文件
└── logs/                # Gateway 自身的运行日志
```

Session 内容以目录为单位管理和归档，不再使用顶层 tool-output 或 artifacts 分散存放会话数据。自动 Memory 保存在 state.db，用户主动记录保存在 Memos；可复用 Python 能力继续保存在 toolbox，实际项目副本保存在 workspaces。会话 artifacts 中需要长期保留的报告和文件，不因清理聊天历史而自动删除；不能直接删除整个 Session 目录代替引用检查。**会话的删除只有一条入口**：`komo session delete` 进 `closing`、对账判定后进 `deleted`、`komo session purge` 才真的删内容（§8.10）——`rm -rf sessions/{id}` 不在流程里，它今天造成的正是"数据库说有、内容说没有"的那种对不上（§8.9）。会话来源需要清理时，先为仍引用它的记忆保留必要的、经脱敏的证据摘录与来源元信息，或停用无法再验证的条目，不能留下伪装有效的引用。重建内容索引不删除 JSONL；清理已结束 Session 的 JSONL 需要同时处理索引、检查点和引用关系。

记忆正文、来源和确认记录需要持久保留；关键词与向量索引可从原文重建，禁止为了重建索引删除整个数据库。Memos 原文由 Memos 自己的备份策略覆盖。

数据目录可通过配置或 KOMO_HOME 改变。配置项中的相对路径按配置文件所在目录解析；JSONL、payloads 与 output.json 中的正文 / 输出引用统一按对应 Session 目录解析。每个实例独占自己的数据库；Turso 对 db 文件持进程独占锁，文件放在运行机器的本地磁盘，不能作为多机器共享数据库。

备份在明确的一致性水位上取得数据库快照，以及各 Session 目录内的 JSONL 已同步前缀、payloads、tool-output 和被引用产物。可短暂暂停持久化协调入口来建立水位，再按固定长度复制追加文件；不能只备份 state.db，也不能把不同时间点的 JSONL 和数据库随意拼接。state.db 的授权数据不能单靠会话 JSONL 重建；渠道的身份匹配（`allow_from` / `home_chat`）不在 state.db，备份 config.toml 与 .env 即可。

## 13. 通信、技术栈与项目结构

### 13.1 通信

CLI 通过 HTTP 发命令，通过 SSE 观察运行。Gateway 内部采用函数调用，无需在本机模块之间再发 HTTP。

最小接口：

| 接口                             | 行为                                           |
| -------------------------------- | ---------------------------------------------- |
| GET /healthz                     | 最小健康检查与实例标识                         |
| POST /v1/sessions                | 创建会话                                       |
| GET /v1/sessions                 | 列出会话（默认不含已逻辑删除的，`?all=1` 才列） |
| GET /v1/sessions/{id}            | 会话详情与运行状态                             |
| GET /v1/sessions/{id}/events     | 按游标获取或订阅事件                           |
| POST /v1/sessions/{id}/runs      | 提交新输入；会话不在 `active` 时 409 并说清是 `closing` / `deleted` / `purged` |
| POST /v1/sessions/{id}/resume    | 检查恢复位置，恢复可继续的运行或返回待处理状态 |
| POST /v1/sessions/{id}/boundary  | 追加 `conversation.boundary`（聊天里的 `/new`）  |
| POST /v1/sessions/{id}/delete    | 逻辑删除：进 `closing`（`{"now":true}` 立刻把未完成 Run 各写一条取消并进 `deleted`）。**不碰内容**（§8.10） |
| POST /v1/sessions/{id}/purge     | 回收内容进 `purged`；引用检查不过则 409 并列出要先处置什么（§8.10） |
| POST /v1/reconcile               | 立刻跑一次对账（§8.9）；幂等，返回这次判定了什么 |
| GET /v1/runs/{id}                | 执行详情                                       |
| POST /v1/runs/{id}/cancel        | 取消                                           |
| GET /v1/interventions            | 待处理清单：审批、结果不明、阻塞三类（§7.5）。每条带句柄、种类、问题与可答的结论 |
| GET /v1/approvals                | **审批记录（含已决定）**，给 `komo run inspect` 答"这一步是谁放行的"（§7.4）；**不是待处理清单**——那一个走 `/v1/interventions` |
| GET /v1/interventions/{handle}   | 单项详情；审批类的详情就是 §7.2 要展示的那一份（计划、改动、原因、范围） |
| POST /v1/interventions/{handle}/answer | 答复：`approve` / `reject`（范围按 §7.2）、`satisfied` / `not_performed` / `abandon` / `resolve`；已答复的返回原结论，不报错 |
| POST /v1/interventions/answers   | 一次答一批（§7.2、§11.3 的 `/approve all`）：名单由发起方列出，每条各自落一条结论 |
| GET /v1/cron                     | 列出定时任务                                   |
| POST /v1/cron                    | 创建定时任务                                   |
| POST /v1/cron/{id}/run           | 手动触发                                       |
| PATCH /v1/cron/{id}              | 更新定义或启停                                 |
| DELETE /v1/cron/{id}             | 移除后续调度，已有执行历史保留                 |
| GET /v1/memories                 | 按作用域、状态和查询条件列出记忆               |
| GET /v1/memories/{id}            | 内容、版本、证据与确认记录                     |
| POST /v1/memories/{id}/confirm   | 操作者确认指定 revision                        |
| POST /v1/memories/{id}/forget    | 停用指定 revision 并失效索引                   |
| GET /v1/memory-index             | 当前空间、进度、覆盖率和错误                   |
| POST /v1/memory-index/rebuild    | 幂等提交重建任务                               |
| GET /v1/models                   | completion alias 菜单：显示名、上游 model、provider、协议、上下文与 effort 档位 |
| GET /v1/config/check             | 当前配置的校验结果（与热重载同一个函数）       |
| POST /v1/config/reload           | 热重载；校验不过 422 并保留原配置               |
| GET /v1/home-session             | 操作者的 home session（§11.2）                 |
| GET /v1/toolbox · GET /v1/toolbox/{m} · POST /v1/toolbox/{m}/test · POST /v1/toolbox/{m}/enable · POST /v1/toolbox/{m}/disable | toolbox 模块清单、详情、候选测试、启用（产生一次审批）、停用（§5.4） |

除最小健康检查外统一认证。提交输入、审批、Cron 与 Memory 变更都支持幂等请求键；同一键对应不同内容则拒绝。

聊天渠道不经 HTTP：它们在 Gateway 进程内通过 `Inbound` / `Notifier`（§13.5）调用与这些接口相同的函数，Dispatcher 是两边共用的入口（§11.1）。

SSE 事件带 Session 内递增序号，断线后按游标补读。JSONL 事件是内容补读来源，state.db 索引负责定位，内存通知仅提示有新数据。CLI 退出或连接中断不取消后台运行，取消通过明确操作发起。

### 13.2 技术选择

| 用途                  | 选择                                                                     |
| --------------------- | ------------------------------------------------------------------------ |
| 异步与进程 IO         | Tokio、tokio-util                                                        |
| HTTP / SSE            | Axum                                                                     |
| HTTP 客户端与模型调用 | Reqwest                                                                  |
| CLI                   | Clap                                                                     |
| 状态数据库            | Turso（MVCC，关 `fts`）+ toasty；raw SQL 走 `toasty::sql`，限于 schema / 关键词索引 / 领取三处（§8.2） |
| 对话与执行记录        | Session JSONL，由 Serde 序列化；大正文外置                               |
| 工具完整输出          | 按执行尝试保存 output.json / stdout.txt / stderr.txt，流式写入后持久发布 |
| 关键词检索            | state.db 的 `memory_terms` 列：索引时 CJK bigram + ASCII 词分词，查询 `instr`，IDF 加权（§9.4） |
| 向量存储与检索        | state.db 保存向量（f32 BLOB + 维度 + 代次），Rust 在过滤后执行精确余弦检索               |
| 向量生成              | 独立 EmbeddingClient，使用 memory.embedding 配置                         |
| 配置与结构化数据      | TOML、Serde；进程内唯一 `Arc<ConfigSnapshot>` 用 arc-swap 原子替换，mtime 轮询触发热重载（§3） |
| Schema 与异步工具接口 | Schemars、async-trait                                                    |
| 日志                  | tracing                                                                  |
| Python                | 独立解释器进程和虚拟环境                                                 |
| Cron                  | 支持指定五字段语义及时区的 Rust 调度库，落地时做语义验证                 |
| TUI                   | ratatui + crossterm，Markdown 用 pulldown-cmark；仅 komo-client                          |
| 飞书                  | openlark，仅 `websocket` 特性；事件自己解析，回复走 reqwest（feature `feishu`）           |
| Telegram              | reqwest 手写 Bot API 长轮询，无 SDK                                                      |
| WeChat                | wechatbot（iLink，DM）；feature `wechat`                                                 |
| TLS                   | reqwest `rustls-no-provider` + rustls `ring`，启动时安装 provider；不引入 aws-lc          |

Axum 已提供 SSE 响应；toasty 的 turso 驱动提供连接与事务，MVCC 冲突重试由 komo 自己包（§8.2）。实现时锁定实际依赖版本并验证 Fedora/macOS 构建。[Axum SSE](https://docs.rs/axum/latest/axum/response/sse/index.html) · [toasty](https://docs.rs/toasty)

生成模型有两个独立协议适配器：OpenAI Responses API（`api_backend = "responses"`）与 Chat Completions（`api_backend = "chat_completions"`）。两者分别处理请求形状、流式终态、tool call 拼接和原生历史回放；切换协议时只用标准化的文字与 tool call 历史，不把一种协议的私有块直接发给另一种协议。向量协议是 OpenAI-compatible `/embeddings`（`embeddings`）与 Ollama `/api/embed`（`ollama_embeddings`）两种。`model_provider` 只表示一组可继承的连接默认值，`api_backend` 才决定适配器；因此 OpenAI、OpenRouter 与自建网关可以复用协议实现，但不会被误认为同一个供应商。

### 13.3 模型角色与统一 effort 配置

模型先进入统一目录，再由角色引用：

| 配置                    | 职责 |
| ----------------------- | ---- |
| `model_providers.<name>` | 一组可复用的 `base_url`、`api_backend`、`env_key` 与超时默认值 |
| `model.<alias>`          | 一项模型；`type = "completion"` 或 `"embedding"`，可覆盖 provider 的任一连接字段 |
| `models.default`         | 默认对话模型 alias；Session / Cron 的 `model` 参数也使用 alias |
| `memory.model`           | 记忆提取模型 alias；省略时使用 `models.default` |
| `memory.embedding`       | 向量模型 alias；hybrid / vector 检索时必填 |

解析优先级固定为“模型项 > model provider > 非敏感默认值”。进入 `ConfigSnapshot` 前，每个 alias 已经展开成完整配置；运行时、Cron 持久化和热重载不再临时拼接字段。`env_key` 是凭证所在的环境变量名（兼容旧拼法 `api_key_env`），凭证值仍只存在 `.env` / 进程环境中。

以下是配置模板，模型名称与服务地址为占位值，effort 示例需要替换为目标模型支持的值：

```toml
[model_providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_backend = "chat_completions"
env_key = "OPENROUTER_API_KEY"
timeout_secs = 120

[model.union]
type = "completion"
model_provider = "openrouter"
model = "stealth/union-alpha"
name = "union-alpha"
context_window = 200000
effort = "medium"

[model.memory]
type = "completion"
model_provider = "openrouter"
model = "openai/gpt-5.4-mini"
# 单项可覆盖 provider 默认值：
api_backend = "responses"
effort = "low"

[model.embedding]
type = "embedding"
api_backend = "ollama_embeddings"
base_url = "https://embedding.example.com/v1"
model = "YOUR_EMBEDDING_MODEL"
env_key = "OLLAMA_API_KEY"
# dimensions 省略时使用模型返回维度，校验后固定到索引代次。
# 这一次探测在 Gateway 就绪**之后**的后台跑（§3），不压住就绪；落定前向量臂按 §9.4 降级。
# revision 可用于标识服务端同名模型的权重版本。

[models]
default = "union"

[memory]
enabled = true
model = "memory"
embedding = "embedding"

[memory.retrieval]
mode = "hybrid"
candidate_limit = 40
top_k = 8
max_tokens = 1500

[execution]
# 交给模型的工具结果正文上限（字节）。完整输出永远在 sessions/<id>/tool-output/ 下，
# 这里限的是"这一次给模型看多少"：超了就头尾各留一段、写明省了多少，并给出可直接 read 的
# 引用（§8.3；`artifact://<run>/<call>/<attempt>/…`，§4.7）。改完对新起的 Run 立即生效（§3）。省略 = 8192。
model_result_bytes = 8192
```

`memory.model` 省略时使用默认 completion alias 的完整配置，包括 effort；它不继承某个 Session 或 Cron 的临时覆盖。显式引用时必须指向 completion，`memory.embedding` 必须指向 embedding。若明确选择 keyword 模式，可以不配置 embedding。`GET /v1/models` 只列 completion alias；选择 alias 会携带完整端点、协议和凭证引用，而不是只替换上游 model id。

effort 的行为统一，取值按协议和具体模型校验：

- 未配置：不发送 effort 字段，由服务端采用默认行为；不等同于显式 none。
- 显式配置：适配器确认目标模型支持后转换为该协议参数。low / medium / high / max 等值不是所有模型共享的固定集合。
- 不支持：komo config check 和配置启用流程返回具体错误，指出模型、配置位置及支持值；不静默忽略或强行映射为另一档。
- 能力未知：需要显式能力声明或在实际启用前完成最小探测。无法确定时拒绝该显式参数；运行中收到不支持参数的响应，也不能删掉 effort 自动重试。
- embedding：配置结构保留可选 effort，但普通向量接口没有该参数时必须省略；dimensions、批大小和输入长度是另一组控制，不用 effort 冒充。
- 后续新增辅助模型、重排模型等角色，也复用同一配置解析与校验入口，不另藏固定 model 或 effort。

不同模型的 effort 可用档位确实不同；例如 Claude 的官方文档按模型列出支持档位，Ollama embed 的请求则列出输入、维度等参数，没有通用 effort 字段。[Claude effort](https://platform.claude.com/docs/en/build-with-claude/effort) · [Ollama embed](https://docs.ollama.com/api/embed)

配置热重载（§3）：模型 / effort / 凭证改动对**新** Run 立即生效；当前 Run 与后台记忆任务保留各自开始时抓取的配置快照，跑完才换。resume 默认沿用原运行的模型 / effort；若操作者明确切换，记录配置变更事件并重新验证协议历史可回放性。任何情况下都不能通过重放已完成工具来适应新模型。

每次生成与向量请求记录角色、模型身份、实际 effort 或 provider_default、耗时和错误；不记录密钥。具体服务可能更改默认值，所以未设置 effort 时只记录“服务端默认”，不能虚构当时采用的强度。索引任务还记录向量空间、代次与条目版本。

### 13.4 Crate 划分与编译预算

编译速度是架构约束，不是事后优化。基线：对一个同类 Rust 工作区（9 crate、约 97k 行，同样依赖 Turso / toasty / axum / reqwest）做冷编 `cargo build --timings`，总墙钟 64.7s，关键路径 `turso_core` 13.5s → `toasty` → 持有 toasty 模型的 crate 13.5s → 二进制 9.0s。三个结论：**toasty 派生宏展开极贵**（11k 行编得和整个 Turso 引擎一样久），持有模型的 crate 必须最小、最少被重编；**二进制 crate 在关键路径末端**，任何下层改动都要再付一次，它必须薄；最重的三方单元（`aws-lc-sys` 17s 构建脚本、`tantivy` / `zstd-sys` 14s、`rusqlite`）全部来自默认特性而非需求。

划分原则：按**依赖重量**和**变更频率**切，不按 DDD 层。重依赖（toasty/turso、axum、reqwest、ratatui、渠道 SDK）各自只出现在一个 crate；高频改动的代码所在的 crate 不依赖任何派生宏重的东西；被所有人依赖的 crate 不含 I/O、不含 tokio；二进制只有 clap 分发。

```text
komo-kernel    值类型、状态机（Run / ToolCall / Session 生命周期）、事件与 fold、Policy 规则引擎、
               cron 到期计算、§8.4 恢复决策表与 §8.9 reconcile 的判定（都是纯函数）、
               Intervention 的类型与派生规则、协议线格式、全部 trait（§13.5）。
               deps: serde, serde_json, uuid, time, thiserror, async-trait, sha2, croner（`default-features = false`，
               无 chrono；它自己仍拉 derive_builder / darling / strum，接受）。不依赖 tokio，不带 tzdb。

komo-store     session_log（JSONL 写入器 / 范围读 / 尾部校验）、payloads、tool_output、
               Turso Db + toasty 模型 + ensure_schema、repositories、Coordinator（impl Ledger）、checkpoint。
               唯一依赖 toasty/turso 的 crate。deps: kernel, toasty(turso only), turso, tokio(fs, sync), sha2

komo-runtime   agent loop、executor、tools/{read,write,edit,rg,shell,python}、python_runtime、
               policy（config → 规则）、approvals、memory（MemoryManager、索引、代次）、llm 与 embedding 适配器、
               scheduler、recovery、config。日常改动落在这里；它从不重新展开 toasty 宏。
               deps: kernel, store, reqwest, tokio, grep + ignore（`rg` 工具）、arc-swap。
               不依赖 komo-agent。

komo-agent     Agent 的身份、能力与上下文装配（`docs/bot.md` 批次 1，2026-09-22 抽出）：
               Profile → 能力面选择（`surface_of`，目录里没有的名字不算数）、skills 发现与
               目录行（`SkillRegistry`，§5.6）、编排操作名（`DELEGATE_TOOL`）。只描述"要做
               什么"，不执行；值类型（`AgentProfile` / `AgentSurface` / `RunSnapshot`）留在
               kernel（事件、协议与 store 模型按它们落盘，搬出来会把依赖指反）。
               deps: kernel, serde_json, tracing。

komo-gateway   axum 路由（§13.1）、SSE、认证、进程锁与发现文件、launchd/systemd 集成、
               Dispatcher、飞书 / Telegram / WeChat 渠道、Notifier 实现、deliveries、审批消息渲染。
               组装 runtime 与 agent（两者互不依赖）。
               deps: kernel, runtime, agent, axum, tower-http, reqwest, openlark(feature "feishu"), wechatbot(feature "wechat")

komo-client    HTTP + SSE 客户端、发现文件读取、ratatui 聊天 TUI（含审批弹窗、Markdown 渲染）、
               操作子命令的输出渲染。只依赖 kernel——不认识 store / runtime。
               deps: kernel, reqwest, tokio, ratatui, crossterm, pulldown-cmark

komo (bin)     clap 分发：`komo` / `komo resume` → client 的 TUI；操作子命令 → client；`komo gateway` → gateway；
               `komo update`（§13.6）是 bin 里唯一自己发 HTTP 的地方。
               deps: client, gateway, clap, reqwest, flate2, tar
```

依赖只向下，三条支线在 bin 汇合：

```text
kernel ← store ← runtime ← gateway ─┐
kernel ← agent ←──────── gateway ───┤
kernel ← client ────────────────────┴─ komo (bin)
```

`gateway` 看不到 toasty 类型（store 只通过 runtime 的构造函数暴露）也看不到 ratatui；`runtime` 看不到 axum，也**不依赖 agent**（装配是 gateway 的事）；`client` 只认协议类型。改 TUI 不重编服务端，改服务端不重编 TUI；冷编时三条支线并行。什么时候再拆：runtime 里 memory 一旦超过 8k 行或引入自己的重依赖，拆成 `komo-memory`；触发条件是编译时间实测，不是模块数。

依赖清单与禁用项：

| 依赖 | 配置 | 理由 |
|---|---|---|
| `toasty` | `default-features = false, features = ["turso"]` | 去掉 `sqlite` 特性拉进来的 rusqlite / libsqlite3-sys |
| `turso` | 与 `toasty-driver-turso` 同 major，工作区唯一 | mimalloc 全局分配器只能有一个 |
| `reqwest` | `default-features = false, features = ["rustls-no-provider", "json", "stream", "charset"]` | `rustls` 特性 = `__rustls-aws-lc-rs`；`no-provider` 后由 komo 在 `main` 里 `rustls::crypto::ring::default_provider().install_default()`，且必须在飞书 ws 线程启动之前 |
| `rustls` | `default-features = false, features = ["ring"]` | 同上 |
| `tokio` | 按需特性，不用 `full` | kernel 不依赖 tokio |
| `axum` | 默认 + `tower-http/cors` | 仅 gateway；gateway 自己直接声明 `tokio`（`rt-multi-thread`, `signal`, `fs`），不靠 axum 传递 |
| `tower-http` | `0.6`，`default-features = false, features = ["cors"]` | 与 reqwest 对齐（两个大版本都要 `^0.6`）；选 0.7 只会多一条自找的重复版本 |
| `sha2` | `0.10` | openlark-core 用 0.10；选 0.11 会把 `digest` / `block-buffer` / `crypto-common` / `cpufeatures` 一起劈成两份 |
| `croner` | `default-features = false`（W2 确认无 chrono 时的时刻表达） | kernel 已有 `time`，不要第二套日期时间库；croner 4 默认特性拉进 chrono + derive_builder / darling / strum |
| `grep` + `ignore` | `0.4`，`default-features = false`；仅 komo-runtime（`rg` 工具） | 内嵌 ripgrep 的搜索与遍历，不起子进程：外部 `rg` 要么没装、要么各家版本不同，而遍历规则（隐藏文件、`.gitignore`、覆盖 glob）本来就该用这套实现。默认特性集是空的，写出来是钉住两件事：**不开 `pcre2`**（"正则语法 = Rust regex"是对模型可见的契约）与不引 SIMD 的 `avx-accel`。带进来的 13 个包（`globset`、`walkdir`、`termcolor`、`crossbeam-deque`、`encoding_rs_io`、`memmap2`、`bstr` 等）全是纯 Rust、无 C 工具链，且没有新的重复版本（2026-09-21 对 `cargo tree -d` 核过） |
| `arc-swap` | 默认；仅 komo-runtime（`config`） | §3 热重载的唯一 `Arc<ConfigSnapshot>` 原子替换 |
| 判断后端（TypeSafe「System One」） | **不新增依赖**：走已有的 `reqwest`（`rustls-no-provider`）+ 同一个 ring provider；凭证只从 `.env` 的 `TYPESAFE_API_KEY` 读 | §9.4 的可选重排用它。默认关；开着时才会发请求，且会把候选记忆的正文（截到 200 字符）发给第三方端点 |
| `tracing-subscriber` / `rustls`（**dev-only**） | 仅 komo-runtime 的 `[dev-dependencies]` | 前者用来抓 §47/§48 的行内指标做断言，后者让真机验收测试自己装一次 crypto provider（装 provider 是进程的事，见上面 `reqwest` 行）。**不进发布构建** |
| `clap` | `derive` | 仅 bin |
| `flate2` / `tar` | `flate2` 显式 `default-features = false, features = ["rust_backend"]`（miniz_oxide），`tar 0.4` | 仅 bin：`komo update` 解发布包（§13.6）。两者都是纯 Rust（不带 C 工具链），且**不进 gateway / runtime 的图**——它们谁也不更新自己；`rust_backend` 写死是不跟上游默认后端变 |
| `ratatui` + `crossterm` + `pulldown-cmark` | 仅 komo-client | 在 client 支线上，与 gateway 并行编译 |
| `syntect` / `two-face`（代码高亮） | 可选，按编译预算实测再定 | 纯 UI 增强，不是需求 |
| `openlark` | 仅 gateway，feature `feishu`（默认开），`default-features = false, features = ["websocket"]` | 只要 ws 长连接；事件负载用 `register_raw` 拿原始 JSON、自己的宽容 serde 结构解析。带来 `prost 0.13`（turso 同步引擎用 0.14）和一份 `tokio-tungstenite` 重复——接受，不再自己额外引入 tungstenite |
| `wechatbot` | 仅 gateway，feature `wechat`（默认开）；**`[patch.crates-io]` 指向 `vendor/wechatbot`**（0.4.0 原样复制，只改 `Cargo.toml` 一行：reqwest `0.12` 默认特性 → `0.13`，`default-features = false, features = ["json", "rustls-no-provider", "http2", "charset"]`） | 上游所有发布版本都写 `reqwest = { version = "0.12", features = ["json"] }`，默认特性开着 = native-tls = openssl-sys，自己一个 feature 都没有；cargo 特性只加不减，工作区里无论怎么声明都关不掉。patch 后复用 komo `main` 装的同一个 ring provider，openssl / native-tls / hyper-tls 整条 C 构建链消失，**构建不再依赖任何系统库**（Fedora 不必 `openssl-devel`，Mac 不必 `brew install openssl@3`），且 reqwest 只剩 0.13 一份（wechatbot 用到的 reqwest API 面极小，0.13 全保留；2026-09-16 实测默认特性全量构建通过）。不选 vendored openssl：OpenSSL 3.6.3、1210 个 .c，`make depend` / `install_dev` 串行，估 2–3 分钟且在 wechat 关键路径起点，会取代 turso 链成为新关键路径。升级 wechatbot = 重新复制 + 重打这一行，见 `vendor/README.md` |
| `qrcode` | `default-features = false`；微信登录二维码渲染为终端字符 | 不需要 `image` |
| **不引入** | rmcp / image | MCP 不在首版 |
| `tantivy` / `zstd-sys`（经 `turso` 默认特性 `fts`） | **已做**：`[patch.crates-io]` 一份 `toasty-driver-turso`（`vendor/toasty-driver-turso`，上游原包，只改 manifest 里 `turso` 那几行），工作区的 `turso` 也写 `default-features = false, features = ["mimalloc"]` | 实测（2026-09-16）：冷编 71.5s → 64.2s，编译单元 577 → 515，单元耗时总和 −36s CPU，`tantivy` / `zstd` / `lz4_flex` 从依赖树消失，少一条 C 工具链。墙钟只省 7s 是因为它们与 `aws-lc-sys` 并行、不在关键路径。功能零损失：turso 的 FTS 是索引方法，MVCC 直接拒绝。升级时整包替换 + 重加那几行，CI 用 `cargo tree -e features -i turso` 断言只剩 `mimalloc`；见 `vendor/README.md` |

三个渠道各自一个 feature，默认全开。feature 的用处不是裁功能，是让 `cargo tree -d` 能按渠道归因重复版本，以及某家 SDK 坏掉时能单独关掉它继续构建。`cargo tree -d` 进入 CI：出现新的重复版本要有理由。

编码规则：

- 接缝上 `dyn`，不泛型：`Vec<Box<dyn Tool>>`、`Arc<dyn LlmClient>`、`Arc<dyn Ledger>`。泛型执行器让每个组合单独实例化，编译时间和二进制都付账，而这里没有需要内联的热路径。
- 派生宏只用 `serde`、`toasty::Model`（store 内）、`clap::Parser`（bin 内）、`thiserror::Error`。不用 `strum`、`derive_more`、`bon`。
- 测试放在代码旁 `#[cfg(test)]`；跨 crate 需要的替身放在各 crate 的 `test-support` feature 后面，只作为 dev-dependency 启用。
- kernel 的 `lib.rs` 只有模块声明。
- 一个 Session 目录的三个写入点（JSONL、payloads、tool-output）全部在 store，runtime 只见 `Ledger` 和 `ToolOutputStore`。

```toml
[profile.dev.package."*"]
debug = "none"               # 第三方不带调试信息

[profile.dev]
debug = "line-tables-only"   # 自己的 crate 保留行号即可

[profile.release]
strip = "symbols"
lto = "thin"
codegen-units = 16
```

编译预算——第一阶段结束时在目标 Mac 与 Fedora 上各测一次并记入仓库，之后每个阶段重测；下面是初始预算，实测后校准：

| 场景 | 命令 | 初始预算 | 骨架实测（2026-09-16，Fedora） |
|---|---|---|---|
| 冷编 | `cargo build` | ≤ 60s（基线 65s：去掉 aws-lc、rusqlite、MCP、image；保留 ratatui 与三家渠道 SDK；若 openlark 拉回 aws-lc，+20s） | 70.2s（8 核 / 11 GB；`--no-default-features --features feishu,telegram`——本机缺 `openssl-devel`，`wechat` 无法构建。关键路径 `zstd-sys` 构建脚本 10.5s → `tantivy` 14.6s → `turso_core` 26.8s → `turso_sync_*` → `toasty-driver-turso` 5.1s → `toasty` 3.3s → 四个 komo crate 0.4s。openlark 确实把 aws-lc 拉了回来：`aws-lc-sys` 构建脚本 32.4s，但并行不在关键路径上。三个渠道全关时 68.5s） |
| 改 runtime 一行 | `touch crates/komo-runtime/src/agent.rs && cargo check` | ≤ 5s | 0.34s（骨架为空，只验证扇出形状：runtime → gateway → bin） |
| 改 client 一行 | `touch crates/komo-client/src/tui/app.rs && cargo check` | ≤ 5s；不得触发 gateway / runtime / store 重编 | 0.32s；只重检 komo-client 与 bin，gateway / runtime / store 未重编 |
| 改 store 一个模型 | `touch crates/komo-store/src/models/run.rs && cargo check` | ≤ 12s（toasty 展开不可避免，但只影响 store 及以上） | 0.37s（骨架里还没有 toasty 模型，此数不代表展开成本；文件现为 `src/models.rs`） |
| 改 kernel 一个类型 | `cargo check` | 全量，允许 ≤ 20s；这是有意为之的代价 | 0.41s（六个 crate 全部重检，骨架为空） |
| `cargo test --workspace` | 增量 | ≤ 30s | — |

预算不达标时，先用 `--timings` 找关键路径，再决定拆 crate 或换依赖——不凭感觉拆。

### 13.5 trait 边界

一个 trait 只在两种情况下存在：**有第二个实现**（生产之外还有测试替身，或有多个后端），或**要做编译防火墙**（上层依赖 trait 就够，不需要看到 toasty / reqwest / axum 类型）。其余组件是具体类型，直接构造、直接测；不为"以后可能换"预留 trait。所有 trait 定义在 kernel，实现散布在 store / runtime / gateway。接缝上一律 `Arc<dyn Trait>` / `Box<dyn Trait>`，异步 trait 用 `async_trait`（对象安全，一次堆分配相对模型往返可忽略）。

| trait | 生产实现 | 测试实现 | 为什么是 trait |
|---|---|---|---|
| `Ledger` | `Coordinator`（store）：正文 → JSONL → 数据库 | `MemLedger` | Agent Loop 的唯一写入口；让 loop 和 executor 的测试不碰文件与 Turso |
| `ToolOutputStore` | 文件流式写入 + 原子发布（store） | 内存 | 子进程 stdout/stderr 流式落盘，`shell` / `python` 测试需要替身 |
| `RunQueue` | `toasty::sql::statement` 条件更新 + 代次（store，无第二个 turso 句柄） | 内存 | 调度器、恢复扫描、Cron、手动 resume 共用的领取入口；并发领取测试 |
| `ApprovalRepo` | toasty（store） | 内存 | executor 等待、聊天 / TUI 答复、outbox 补写三方共用 |
| `CronRepo` | toasty（store） | 内存 | 调度器 + CLI + Gateway API |
| `MemoryRepo` | toasty + `memory_terms`（store） | 内存 | `MemoryManager` 的全部状态读写；召回排序逻辑要在无数据库下测 |
| `LlmClient` / `TurnDriver` | 每种协议一个适配器（runtime） | 脚本化 driver | 主模型、记忆模型是同一 trait 的两个实例；loop 测试用脚本化回合 |
| `EmbeddingClient` | OpenAI-compatible / Ollama（runtime） | 固定向量 | 空间指纹与维度校验要在无网络下测 |
| `PythonHost` | 子进程 + venv 版本（runtime） | 假宿主 | `python` 工具、toolbox 启用流程、核对函数调用都经它 |
| `Tool` | 5 个（runtime） | — | §4 已定；executor 只认 trait |
| `Policy` | 规则引擎（runtime） | 表驱动规则 | 同步、纯函数；授权在 `PolicyContext` 里传入，不在内部查库 |
| `Channel` / `Inbound` / `Notifier` | 飞书、Telegram、WeChat、HTTP API、SSE（gateway） | 内存渠道 | 首版就有三个真实现；审批请求的渲染是各渠道实现的事 |
| `Clock` | 系统时钟 | 可拨时钟 | Cron 到期、`valid_until`、重试退避、审批有效期全部依赖时间 |
| `ZoneResolver` | tzdb 实现（runtime，W3 选 crate） | `ScriptedZoneResolver`（可编脚本的假时区）/ `FixedOffsetZone`（降级） | §10 的夏令时两条规则要在无 tzdb 下测；kernel 不带时区数据库 |

不是 trait 的东西：`AgentLoop`、`ToolExecutor`、`Scheduler`、`Recovery`、`MemoryManager`、`SkillRegistry`、`Coordinator`、`Dispatcher`——各只有一个实现，依赖上表的 trait 就可测。`SessionRepo`、`ToolCallRepo`、`CheckpointRepo`、`OutboxRepo` 是 store 内部的具体类型，只被 `Coordinator` 用。**审批与 Intervention 都不需要 `Approver` trait**：§6 定了审批暂停 Run，`Policy` 答 `Ask` 后 executor 写 `approval_requests` 并 `Ledger::suspend`；清单（§7.5）是 `runs` 与 `approval_requests` 的并集查询，没有需要注入的第二实现。飞书卡片按钮、Telegram / WeChat 命令、TUI 弹窗、CLI 子命令都打到 `POST /v1/interventions/{handle}/answer`（一次答一批时是 `POST /v1/interventions/answers`，逐条落结论），调度器把 Run 重新入队，executor 从 `ApprovalRepo` 消费授权再执行；`satisfied` / `not_performed` 走 executor 的收尾与 `RecoveryStore::requeue`，`abandon` 走 `Ledger::complete`。渠道之间的差别只在渲染（§11.3），不在决策。

关键签名（接口示意，辅助类型省略；`Tool` 见 §4）：

```rust
/// 每个 Session 一个实例，串行。每个方法对应 §8.5 的一段箭头，
/// 方法内部完成"外置正文（如有）→ JSONL 追加并同步 → 数据库事务"的顺序。
#[async_trait]
pub trait Ledger: Send + Sync {
    /// 预留 Run（`accepted`）→ run.accepted → `queued`。同一 request_key 返回原 Run，哈希不同则拒绝。
    async fn accept_input(&self, input: AcceptInput) -> Result<Accepted, LedgerError>;
    /// 完整 assistant 回复 + 本轮全部调用计划，一个逻辑事件；返回 Runtime 分配的 ToolCallId。
    async fn record_round(&self, run: &RunId, round: AssistantRound) -> Result<Vec<ToolCallId>, LedgerError>;
    /// tool.planned：准备好的执行计划落盘（§8.4 第 6 行要求"planned 而未执行"是可分辨的状态）。
    async fn plan_call(&self, call: &ToolCallId, plan: &ExecutionPlan) -> Result<EventId, LedgerError>;
    /// tool.started + 执行尝试 + 首次授权消费，同一事务；返回后才允许产生真实副作用。
    async fn start_call(&self, call: &ToolCallId, plan: &ExecutionPlan, grant: Option<GrantUse>) -> Result<AttemptId, LedgerError>;
    /// 输出已由 ToolOutputStore 发布；这里只写 tool.result 元信息与引用。
    async fn finish_call(&self, attempt: &AttemptId, published: PublishedOutput) -> Result<(), LedgerError>;
    /// 一次**没有执行过**的调用的结论（工具名不认识、参数准备不出来、放行被拒、子代理不能
    /// 再委派）：它有结论、没有尝试，所以建的是那条 `tool.started` 缺席的尝试行。
    /// **必须写**：`tool.result` 缺席的调用在账本上永远悬着，从账本重建的转写就会带着一个
    /// 没有输出的 `function_call`，provider 直接 400。
    async fn fail_call(&self, call: &ToolCallId, attempt: &AttemptId, published: PublishedOutput) -> Result<(), LedgerError>;
    /// 停在某个外部条件上（§8.4）：状态变 `waiting`，理由进 `WaitReason`，释放执行名额。
    async fn suspend(&self, run: &RunId, wait: Wait) -> Result<(), LedgerError>;
    /// 调用前这一轮的最终回复必须已作为 message.assistant 落盘（record_round → complete）；
    /// run.completed 的 final_message 只用于补读，不进消息面。
    async fn complete(&self, run: &RunId, end: RunEnd) -> Result<(), LedgerError>;
    /// 按 seq 分页读事件（短事务，limit=0 由实现定页大小）；EventBatch.next 为 None 即读到末尾。
    async fn read(&self, session: &SessionId, from: Seq, limit: u32) -> Result<EventBatch, LedgerError>;
    /// `/new`：追加 conversation.boundary，不切 Session。
    async fn boundary(&self, session: &SessionId) -> Result<Seq, LedgerError>;
    /// control_outbox 审计补写，按 event_id 幂等；不创建授权。
    async fn append_audit(&self, session: &SessionId, event_id: &EventId, payload: EventPayload, occurred_at: OffsetDateTime) -> Result<Seq, LedgerError>;
}

#[async_trait]
pub trait ToolOutputStore: Send + Sync {
    /// 为一次尝试打开流式写入器（stdout/stderr 写 .partial）。
    async fn begin(&self, attempt: &AttemptRef) -> Result<Box<dyn OutputWriter>, StoreError>;
    /// 收齐后同步、原子写 output.json，返回带路径、大小、哈希的引用。
    async fn publish(&self, writer: Box<dyn OutputWriter>, result: ToolResultBody) -> Result<PublishedOutput, StoreError>;
    /// 按引用读取并校验哈希；不匹配返回 Corrupt，不返回内容。
    async fn open(&self, output: &OutputRef) -> Result<VerifiedOutput, StoreError>;
}

/// 同步、纯函数。授权、来源、当前有效范围都通过 ctx 传入。
pub trait Policy: Send + Sync {
    fn decide(&self, plan: &ExecutionPlan, ctx: &PolicyContext) -> PolicyDecision;
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    /// 一次 Run 的一个执行段。工具 Schema、系统提示、记忆注入在这里装配一次。
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError>;
}

#[async_trait]
pub trait TurnDriver: Send {
    /// 一次完整的 provider 往返。First 是首轮；ToolResults 按 call_id 回传上一轮结果。
    async fn next(&mut self, input: RoundInput) -> Result<Round, LlmError>;
    fn usage(&self) -> TokenUsage;
}

#[async_trait]
pub trait EmbeddingClient: Send + Sync {
    /// §9.5 的空间指纹；凭证不进入。
    fn space(&self) -> &EmbeddingSpace;
    async fn embed(&self, kind: InputKind, texts: &[String]) -> Result<Vec<Vector>, EmbedError>;
}

#[async_trait]
pub trait PythonHost: Send + Sync {
    /// 新进程执行；stdout/stderr 流入 sink；取消时终止进程组并等待回收。
    async fn run(&self, job: PythonJob, sink: &mut dyn OutputWriter, cancel: CancelToken) -> Result<PythonResult, PyError>;
    fn env_version(&self) -> EnvVersion;
}

#[async_trait]
pub trait RunQueue: Send + Sync {
    /// 条件更新 + 递增代次；返回 None 表示没有可领取的 Run（调度器：下一个到期的）。
    async fn claim(&self, executor: &ExecutorId) -> Result<Option<Claimed>, StoreError>;
    /// 领取指定 Run（手动 resume、恢复扫描）；同样的条件更新，两个执行者只一个成功。
    async fn claim_run(&self, run: &RunId, executor: &ExecutorId) -> Result<Option<Claimed>, StoreError>;
    /// 交还名额（等审批 / 等重试 / 执行者退出）；代次不对就什么都不做。
    async fn release(&self, claimed: &Claimed) -> Result<(), StoreError>;
}

#[async_trait]
pub trait ApprovalRepo: Send + Sync {
    /// 决定幂等：已决定的返回原决定。
    async fn decide(&self, id: &ApprovalId, decision: ApprovalDecisionRecord) -> Result<ApprovalDecisionResponse, RepoError>;
    /// 消费授权换 Proof：Once 比计划哈希并标记 consumed；Run / CronJob 范围走 Grant::covers(plan)，
    /// 不因一次使用作废，但 ConsumedApproval 记下用的是哪条 grant。Deny 上 executor 根本不来这里。
    async fn consume(&self, id: &ApprovalId, plan: &ExecutionPlan, now: OffsetDateTime) -> Result<ConsumedApproval, RepoError>;
    // create / get / find_by_short_id / list_pending / grants_for_run / grants_for_job 略
}

/// kernel 不带 tzdb；两个方向都要，cron 的搜索两头都走。
pub trait ZoneResolver: Send + Sync {
    fn resolve(&self, zone: &str, local: PrimitiveDateTime) -> Result<ZoneResolution, ZoneError>; // Single / Ambiguous(早, 晚) / Gap
    fn offset_at(&self, zone: &str, instant: OffsetDateTime) -> Result<UtcOffset, ZoneError>;
}

#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &'static str;
    async fn serve(&self, inbound: Arc<dyn Inbound>, shutdown: Shutdown) -> Result<(), ChannelError>; // kernel 无 anyhow
}

/// Gateway 交给渠道的唯一入口。渠道不知道 Session 是什么。
#[async_trait]
pub trait Inbound: Send + Sync {
    async fn handle(&self, msg: InboundMessage) -> Result<InboundAck, GatewayError>;
}

#[async_trait]
pub trait Notifier: Send + Sync {
    /// 先持久化投递记录再发送。Sent = 已送达；Deferred = 渠道此刻无法推送（微信无回复令牌），
    /// 行留在 pending，由下一条入站消息触发冲刷。
    async fn deliver(&self, target: &DeliveryTarget, msg: Outbound) -> Result<Delivery, DeliverError>;
}
pub struct Delivery { pub id: DeliveryId, pub state: DeliveryState /* Sent | Deferred */ }

pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}
```

**W3 落地后的契约修订**（2026-09-16；实现见 `crates/komo-kernel/src/traits.rs`，与上面的示意不一致时以代码为准）：

- `Tool::execute(&self, plan: ApprovedPlan, ctx: &ToolContext, sink: &mut dyn OutputWriter)`：工具**借用**本次尝试的流式写入器，发布仍在 executor 手里（§8.5 的下一步）。
- `ApprovalRepo::consume(&self, id, plan: &ExecutionPlan, intent: ConsumeIntent, now)`，`ConsumeIntent::{First, KnownNotToHaveRun}`：已消费的一次性授权对 `First` 是可分辨失败（`GrantMismatch`），只有核对确认「原动作未发生」才允许重用——§7.4 的两句话各落一处。
- `Ledger::start_run(&self, run, executor, generation)` 写 `run.started`；领取（`RunQueue::claim` / `claim_run`）只改行，`rows affected` 是胜负的唯一信号。
- `CronRepo::advance(&self, id, next_run_at, status, last_error)`：推进槽位**不递增版本**，否则每次触发都作废绑定该 Job 的授权。
- `Wait::Approval { approval, call: Option<ToolCallId>, attempt: Option<AttemptId> }`；`RunEnd::Completed { final_message, rounds }`。
- `StoreError::{VersionConflict, GrantMismatch}` + `From<StoreError> for RepoError`：store 的事务闭包只有一条错误通道。
- protocol：`ManualCronRunRequest`、`BoundaryRequest`、`ApprovalListQuery`、`MemoryScope` 的 `Display` / `FromStr`（`personal` | `project:<id>` | `environment:<id>`）、`GET /v1/models` → `ModelsResponse`、`GET /v1/config/check`、`POST /v1/config/reload`、`SseEvent::AssistantDelta`（**只在 SSE 上，永不进 JSONL**；`message.assistant` 仍是一次完整回复）。§13.1 的接口表相应多这三个端点。
- `Ledger::run_end(&self, run: &RunId) -> Result<Option<RunEnd>, LedgerError>`：一条 Run 的终态（只读，不带消息历史）。委派的核对走它（§4、§8.6）——父 Run 续跑时用它把那次 `delegate` 调用收尾，而不是去翻会话的整段事件；"子 Run 结束了没有、怎么结束的"是调度层要知道的一件事，不该逼它自己做一遍 fold。

### 13.6 安装与升级

发布产物由 `.github/workflows/release.yml` 在 `v*` 标签上构建，四个平台各一个包：`komo-darwin-arm64.tar.gz`、`komo-darwin-amd64.tar.gz`、`komo-linux-amd64.tar.gz`、`komo-linux-arm64.tar.gz`，外加一份共用的 `SHA256SUMS`（`sha256sum` 的默认两列格式）。**包内只有一个成员 `komo`**。linux 包在 Ubuntu 22.04 上原生构建（ring 与 mimalloc 编 C，不走交叉编译），glibc 下限因此是 2.35；darwin 两个架构都在 arm64 runner 上出。

装与升级是同一条约定的两个入口：`install.sh`（仓库根，`curl -fsSL …/main/install.sh | bash`，认平台 → 问 `releases/latest` 或 `KOMO_VERSION` → 下包与 `SHA256SUMS` → `shasum`/`sha256sum` 核对 → `tar -xzf` → 落到 `komo.new` 再 `mv -f`）和 `komo update`（§3，用 Rust 自己走一遍同一套名字与校验，`crates/komo/src/update.rs`）。仓库名、资产名、校验和文件名三处必须一致：那两个文件加这里。

`komo update` 的顺序是**下载 → 校验 sha256 → 解包 → 试跑 `--version` → 同目录 `rename`**。三条不变量：

1. **换上去是最后一步，而且是一次 `rename`**。中途任何一步失败（校验和不对、包坏了、试跑不过），现在装着的那个 komo 一个字节都没动；临时文件与目标同目录，跨文件系统的 rename 会退化成复制。自我更新唯一不可接受的结局不是"没更新成"，是"更新成一个跑不起来的东西"。
2. **试跑要求新二进制自报的版本就是标签版本**。glibc 太旧、架构不对、包被改过都在这一步挡下来。这条同时把发布流程绑住了：标签与 `[workspace.package].version` 不一致的包会被拒收，所以 `release.yml` 里那个 `guard` 作业是这条不变量的另一半，不是可选项。
3. **只换磁盘上那份，不碰在跑的进程**。发现文件里核对得上一个在跑的 Gateway 时就打印 `komo gateway restart`，**不替操作者重启**：重启会打断正在跑的 Run 与等在那里的审批，那不该由一条更新命令决定。

只报告，不降级：装着的比最新发布新（源码构建的开发机、或者刚回滚过）时只说一句"发布的比它旧"，什么都不做。

已核实（2026-09-20，本机 macOS arm64）：`install.sh --prefix` 走真实 `v0.0.2` 发布包，核对与安装都通过，包内成员名就是 `komo`；`komo update` 打真实 `api.github.com`，正确判定"发布的比它旧"并停在原地；`update.rs` 的下载 → 校验 → 解包三段用真实 `v0.0.2` 资产生跑过一遍——`SHA256SUMS` 认得、GNU tar 打的包解得出来（50318240 字节，与 `install.sh` 装出来的那份一致），试跑按预期拒收了标签对不上的那个包（`v0.0.2` 的二进制自报 `0.1.0+00b23c6`，正是上面第 2 条要挡的形状）。发布流程自身（四平台构建 + 校验和 + provenance + 发布）要等 v0.8 的第一个 `v*` 标签才会真跑一次。

## 14. 实现顺序与验收

| 阶段 | 交付 | 验证 |
|---|---|---|
| 1. 进程与会话骨架 | 六个 crate 骨架（§13.4）；komo、Gateway 自动启动、HTTP/SSE；`komo-store`：Turso 连接、`ensure_schema` + DDL 对齐测试、`with_write_retry`、统一 Session 目录与状态索引；`komo-client`：HTTP/SSE 客户端与 TUI 骨架；**编译预算首次测量** | 多个 CLI 同时启动仅产生一个实例；断线后能查看原会话；两个写入器对不同 Session 并发提交不互相阻塞，对同一行冲突时一方重试成功且只应用一次；DDL 对齐测试挂掉能定位到列；改 client 一行不重编 gateway；保存一份合法的 config.toml 后 `komo doctor` 一秒内显示新 mtime 已生效，保存一份非法的则旧配置继续生效且 home chat / 日志有具体错误 |
| 2. AgentLoop 与 Policy | 模型往返、执行计划、Allow/Ask/Deny、审批持久化；`Ledger`、`Policy`、`ApprovedPlan` 类型状态；loop 用 `MemLedger` + 脚本化 `TurnDriver` 测 | 危险操作批准前不执行；重复批准不重复执行；Deny 不被授权覆盖；不构造 `Proof` 就调不到 `execute`（编译期）；`Ask` 后 Run 让出名额且 TUI 弹出审批；同一轮的两条只读调用同时在飞（`read` / `rg`），写与审批是屏障、取消要收齐已在飞的那些（§6）；**不在本次运行能力面里的工具，模型拼出名字也执行不了**，而交给模型的 schema 与执行时的表来自同一份（§4） |
| 3. 六个工具 | 文件、搜索、进程、Python 环境与输出处理；`verify` 默认实现与 write/edit 的哈希核对 | 文件版本冲突可见；取消停止子进程；未知工具无法调用；`rg` 与 `read` 走同一条判定（工作目录内的搜索不产生审批），且它不依赖机器上装没装 `rg` |
| 4. 聊天入口（飞书 + Telegram + WeChat） | `Channel` / `Inbound` / `Notifier`、Dispatcher、`allow_from` / `home_chat` / `groups` 配置与 `/id`、`deliveries`、审批渲染、短 ID、飞书卡片与 Telegram 内联按钮、`komo channel probe` | 同一 `event_id` / `update_id` / `msg_id` 重发只产生一个 Run，同一人连点两次第二次得到「已决定」；不在 `allow_from` 的发送者被拒且不留记录，`/id` 对其仍可用；把发送者加进 `allow_from` 并保存后，不重启 Gateway 其下一条消息即进入 Run；`/approve` 与按钮回调重发只批准一次；来源会话与 home chat 都收到请求且第二个答复得到"已决定"；决定后卡片 / 消息原地更新；Gateway 重启后 pending 投递补发一次；飞书 ws 断线重连不丢事件也不重跑 Run；微信在用户未发消息前 `Deferred`，发消息后先收到积压的审批请求 |
| 5. 自动恢复与 resume | 持久队列、启动扫描、领取去重（`RunQueue` 条件更新 + 代次，`toasty::sql`）、检查点、调用核对与子进程回收；恢复决策表在 kernel 里是纯函数 | 重启自动接续原 Run；已完成动作不重放；未知效果不盲目重试；两个执行者并发 `claim` 同一 Run 只有一个成功；决策表对 §8.4 每一行有一个单元测试；重启后等待中的审批仍能在手机上批 |
| 6. toolbox 迭代 + Skills | 保存模块、候选测试、版本审核与启用；`SkillRegistry`、目录行门控、`read` 只读根 | 调用使用已批准且已测试版本；模块更新使旧授权失效；新增 SKILL.md 无需重启即被 `komo skills list` 看到；`requires_tools` 不满足时不出现在提示里但仍可 inspect；toolbox 启用的审批在微信里能看到版本差异与测试结果 |
| 7. Cron | 持久调度、去重、重叠处理、人工接手 | 重启不重复创建同次触发；新危险操作等待审批；Cron 的等待在聊天里 `/approve` 后按 Cron 权限继续，不升权 |
| 8. 模型配置与 Memory | 独立记忆 / 向量模型、effort 校验、自动提取、混合召回（`memory_terms` + 向量）与遗忘 | 不串用模型配置；推断不自行确认；过期或遗忘内容不召回；重建可恢复 |
| 9. 场景与双平台验收 | 真实代码仓库、测试 HA 设备、Memos 主动记录；编译预算复测 | 两平台独立运行；记录从 Memos 找回；代码交付含验证证据；冷编与增量编译在预算内，或有记录在案的偏差原因 |
| 10. 生命周期与对账 | Session 的 `closing → deleted → purged`（§8.10）、Intervention 统一清单与四种结论（§7.5）、reconcile 的三条判定与领取守卫（§8.9） | **逻辑删除可靠**：`delete` 之后新输入被拒、未完成 Run 各写一条明确取消、内容一个字节没动；`purge` 在引用未处置时 409 并列出要先处理什么；`purged` 之后没有任何路径再创建那个目录。**清单堵死漏网**：`approval list` 为空而会话被挡住这种组合构造不出来——"挡队"与"进清单"同源，且每一条 `waiting + approval` / `waiting + intervention` 都在清单里。**orphan 自动 reconcile**：目录被手工删掉而状态不是 `purged` → 对账报出来且 Run 不被领走（不出现"按空上下文执行一轮"）；handler 任务 panic 留下的 `running`（租约过期 + 存活判定）被回收；无法确认旧执行结束的那条停成清单里的一条 |

存储和 Policy 的基本约束从第二阶段开始贯穿全部工具，不能在所有功能完成后才补审核。

首个端到端验收：启动 komo 自动拉起 Gateway，通过模型调用 write 生成工作文件，在任务的后续步骤前重启 Gateway；不打开 CLI 也应自动接续原 Run，随后 resume 原会话查看结果，再读出原文件。随后验证一次待审批 shell 在重启后仍等待，在飞书或 Telegram 里批准后只执行一次已确认的调用；若模拟执行结果丢失，则进入未知状态核对而不是自动重跑。

恢复故障注入验收：

| 强制中断位置                                         | 预期结果                                                         |
| ---------------------------------------------------- | ---------------------------------------------------------------- |
| Run 已提交，内存队列尚未收到通知                     | 启动或周期扫描找到原 Run，自动执行一次                           |
| 输入已持久保存且 queued 已提交，客户端确认响应丢失   | 客户端用同一请求键重发，仍返回原 Run                             |
| LLM 输出尚未收齐                                     | 重试模型请求；未收齐的调用从未执行                               |
| 完整计划已同步到 JSONL，第一个工具尚未 started       | 自动执行原计划，调用 ID 不变                                     |
| started 已提交，真实动作还没发出                     | 保守核对，不能从 started 单独推断已执行或未执行                  |
| Memos 等外部写入成功，完整输出与结果事件尚未持久保存 | 借助可靠幂等或关联信息核对；没有证据则等待处理，不产生第二条记录 |
| 输出文件和 JSONL 引用已持久保存，state.db 尚未更新     | 校验输出后补结果索引，复用原输出，工具不重做                     |
| output.json 已完成但 JSONL 结果事件尚未写入          | 校验身份、计划及完成状态后补记结果；不能只凭文件存在判断         |
| 输出文件引用已提交，但正文缺失或被修改               | 停止受影响任务，报告引用损坏，不通过重跑补造旧结果               |
| 用户批准已保存，审计 JSONL 尚未补写                  | 从 state.db 读取有效授权，outbox 幂等补写，不重新询问              |
| JSONL 最后半行未被任何提交引用                       | 隔离尾部后修复；按最后完整记录恢复，未知副作用仍须核对           |
| JSONL 已提交范围缺失或中间损坏                       | 停止受影响会话，报告损坏，不跳过记录或重做动作                   |
| 最终结果已同步到 JSONL，SSE 尚未送达                 | 客户端补读原结果，Run 保持 completed                             |
| 旧子进程仍存活，或恢复和 resume 同时触发             | 先阻止重复执行；核实进程结束，并且只有一个领取者                 |
| 用户取消后重启，或连续恢复失败耗尽预算               | 已取消的不复活；失败有明确终态，不无限循环                       |
| 逻辑删除进行到一半被杀（已进 `closing`，未完成 Run 还没处置完） | 重启后仍是 `closing`：Run 照 §8.4 走完或停在等待，不推进到 `deleted`，内容一个字节没动 |
| `purge` 进行到一半被杀（墓碑已提交、内容删了一半） | 对账把"标了 `purged` 而内容还在"的会话幂等收尾；不表现成损坏，也没有 Run 被领走 |
| 会话目录被手工删除，而状态仍是 `active` / `closing` | Run 不被领走（不按空上下文执行一轮），对账把这条不一致报成 `blocked` Intervention |

目录与引用验收：工具事件、大参数、完整输出及 stdout / stderr 都位于同一 Session 目录；改变 Gateway 当前工作目录不影响引用读取；不同 Session 或不同 attempt 不能互相覆盖。清理历史时保留未完成任务、Memory 证据和持久产物仍需引用的文件。**读路径不写**：把会话目录删掉之后，`GET /v1/sessions/{id}/events`、会话详情、对账与恢复扫描都不得把它建回来，而且对账必须把这条 Run 停成 `waiting + intervention` 并说明缺内容。

**资源命名空间验收（2026-09-22）**：`read("skill://<skill>/SKILL.md")` 与 `rg(path="skill://")`（多根 → 多条目标）端到端成立，且**审批与审计里显示的是逻辑 URI**（`PlanTarget::describe()` 形如 `skill://review/SKILL.md（<真实路径>）`），读一份 skill **不产生审批**（skill 目录是只读根）；`tool://<工具>/schema|doc` 只在**这一次的能力面**里可读（面外写 `tool://write/schema` 在 `prepare` 就被拒、不进计划），`tool://` 是虚拟入口、没有磁盘目标；`..` / 空段 / 指向挂载点之外的链接 / 没有会话内容目录这几类同样在 `prepare` 被拒；超限正文给出的 `artifact://<run>/<call>/<attempt>/…` 与产物入口 `artifact://files/<run>/<名字>` **在 Gateway 重启之后仍读回同一份正文**；本地路径的用法与改造前逐字相同（计划里仍是 `{"path": …}`，旧审批计划按原哈希校验）。

测试同时断言实际副作用次数、Run / ToolCall 身份、授权使用、预算和事件配对；仅检查“恢复后状态变成 running”不算通过。每个断点做进程终止测试，并单独安排操作系统重启 / 存储持久性验证。

Memory 与模型验收覆盖：

- 同一偏好换一种中文表述仍可语义召回，同时保留原始来源；中文两字查询直接命中 bigram。
- 用户原话、工具观察、模型推断、用户确认在展示和上下文中均可区分；候选不因反复提取而升级。
- 切换聊天模型或 effort 不影响独立配置的记忆模型；记忆提取、冲突整理均使用自己的模型与 effort。
- 未设置 effort 不发送该字段；不支持的 effort 在请求前拒绝；模型 400 不触发静默删参重试。
- 同维度但不同模型 / 版本生成的向量不混用；修改记忆时旧 revision 不召回；重建中崩溃可以接续。
- 向量服务不可用时 hybrid 明示降级，vector-only 明确报不可用；不能把故障解释为“没有相关记忆”。
- confirm / forget 使用预期 revision，重复请求幂等；resume 和旧检查点不能重新注入已遗忘内容。
- 主动记录写入 Memos 后返回 ID / 链接，查询回到原文；删除或修改原文后更新关联摘要，写入未知时先核对。
- 在目标 NAS 与 Mac 上测量至少 1 千与 1 万条代表性记忆的索引时间、检索 P95 和内存，再决定是否需要近似向量索引；不预先宣称性能达标。

**待验证事项**——设计依赖但尚未在目标版本上核实的点。每一项在对应阶段开始前验证，结论写回本文：

| 项 | 影响 | 若不成立 |
|---|---|---|
| Turso MVCC 模式提交是否 fsync；能否读回同步设置 | §8.2 权威边界 | **已核实（2026-09-16，spikes/store.md）：提交 fsync，MVCC 写 `state.db-log`、WAL 写 `state.db-wal`，默认 `synchronous=FULL` 且可读回可改写；`synchronous=OFF` 后 strace 里 fsync 全部消失，说明不是空实现。§8.2 的 outbox-先-确认保护不启用。** 附带：`state.db-log` 与主库同等重要；`data_sync_retry` 默认 false 时 fsync 出错是 panic，连接时设成 1。未核实：真正的断电 / 内核崩溃持久性（只做了 25/25 的进程 abort） |
| toasty 0.10 是否支持带条件的 UPDATE 并返回受影响行数 | §8.7 领取代次 | **已核实（2026-09-16，spikes/store.md）：条件能表达，行数拿不到——查询目标的 `.exec()` 恒 `Ok(())`。改用 toasty 自带的 `toasty::sql::statement -> Result<u64>`，`Transaction` 上也能用，所以不需要第二个 `turso::Database` 句柄（§8.2、§13.5 已改）。并发正确性已验：220 轮 × 2–4 竞争者恰好一个赢家。附带纠正：autocommit 单语句写在 4 写入器下 200 次只成 6 次，每个写都必须进 `db.transaction()`（§8.2 已改）** |
| `reqwest` `rustls-no-provider` 下没有其他依赖重新启用 `aws-lc-rs`；`openlark` 是否把它拉回来 | §13.4 冷编预算 | **已核实（2026-09-16，骨架实测）：拉回来了**，肇事者是 `openlark-core 0.20` 显式声明 reqwest 的 `rustls` 特性（= `__rustls-aws-lc-rs`），特性可加不可减，`default-features = false` 压不住，要挡只能 `[patch]` openlark-**core** 把该特性换成 `rustls-no-provider`。代价实测：`aws-lc-sys` 构建脚本 32s 但 8 核并行、不在关键路径，墙钟只 +1.7s（68.5s → 70.2s）。**决定：接受，不维护 patch。** |
| `instr` 全表关键词臂在 1 万条时的 P95 | §9.4 | 加 `memory_terms` 的按 token 倒排表（仍是普通表，仍可重建） |
| Turso 长读事务（SSE 游标补读）与并发写提交的快照语义 | §11.1 回复订阅 | 读路径改为短事务分页 |
| `toasty-driver-turso` 能否通过 `[patch]` 关闭 `turso` 的 `fts` 默认特性且链接正常 | §13.4 | **已核实并已做（2026-09-16）**：能链接，toasty 读写照常；冷编 −7.3s、−62 编译单元。见 §13.4 依赖表 |
| iLink 消息是否带稳定 `msg_id` 可作 `request_key` | §11.1 微信去重 | **已核实（2026-09-16，wechatbot 0.4.0 源码）：没有 `msg_id`。** 逐消息的唯一候选是 `client_id`——发送方客户端生成的 UUID，`String` 非 Option。**键改为 `wechat:{from_user_id}:{client_id}`**，`(from, 内容哈希, 60s 窗口)` 降级为 `client_id` 为空时的回退。已确认两条重投来源：拉取游标不持久化（重启从空游标开始）、「回了消息却回空游标」时 SDK 原地重取。**剩余未核实（需真机登录，步骤见 `spikes/wechat.md` §5）**：① 入站 `client_id` 的形态与是否恒非空；② 空游标对服务端意味着「重发未确认」还是「从现在开始」——后者是丢消息，比重复严重，会要求把游标持久化进 state.db；③ 重投时 `client_id` 是否不变；④ 旧 `context_token` 能否跨进程使用 |
| `wechatbot` 的 native-tls 在 Fedora 上链系统 openssl 是否顺利；与 `rustls-no-provider` 的 reqwest 0.13 共存 | §13.4 | **已核实（2026-09-16，源码 + 实测）：共存没有问题，但 native-tls 这条路整个不走了。** 共存侧：native-tls 走 openssl 自己的 `Once`，与 rustls 无共享状态，`CryptoProvider::install_default()` 仍只由 `main` 调一次；原文「一份 reqwest / hyper 重复」里 hyper 是错的，锁文件里 hyper / hyper-util / rustls / hyper-rustls 各只有一份。TLS 侧：上游让 reqwest 0.12 默认特性开着且自己没有 feature，工作区无法关掉。**决定并已做：`vendor/wechatbot` + `[patch.crates-io]`，reqwest 升 0.13 走 rustls**，openssl 整条链消失，reqwest 重复也消失，默认特性全量构建通过，Fedora 不必装 `openssl-devel`。vendored openssl 作为退路而非首选（估 2–3 分钟且在关键路径起点） |
| `lark-websocket-protobuf` 是否需要 `protoc` | 构建工具链要求 | **已核实（2026-09-16）：不需要。** 0.1.2 无 `build.rs`，依赖树里没有 prost-build / tonic-build / protobuf-codegen，本机无 protoc 编译通过。安装说明不加 `protobuf` |
| 飞书卡片回调的 `event_id`、Telegram `callback_query_id` 在重推时是否保持不变 | §11.3 按钮去重 | **已核实（2026-09-16，官方文档）**：飞书对 2.0 事件与回调的官方去重建议就是「通过 `event_id` 字段判断事件唯一性」，配合「至少发送一次」与 15s/5min/1h/6h 最多 4 次重推，等价于保证重推携带同一 `event_id`；Telegram long polling 重投的单位是整个 `Update`（`offset` 未推进即原样再取），`update_id` 与其中的 `callback_query.id` 一并不变。**两个键都稳定，组合键退路作废**；去重键只挡平台重投，用户连点由 `approval_id` 幂等承担（已写入 §11.3）。剩余未核实：① 卡片回调自身的重推间隔 / 次数（官方只在「事件」侧给表）；② ws 模式下卡片回调载荷与 HTTP 是否逐字段一致；③ openlark 的 ws 客户端能否回传卡片响应帧——设计已改用 PATCH 绕开，不阻塞 |
| ws 上 `card.action.trigger` 能不能到 komo（按钮回调） | §11.3 卡片按钮 | **源码核实（2026-09-18，`openlark-client 0.20.0/src/ws_client/frame_handler.rs:106`）：不能。** 数据帧按 header 的 `type` 分发，`"event" \| ""` 进事件分发，**`"card"` 是 `debug!("Card frame received, skipping")` 后返回 `None`——丢掉，连 ack 都不回**；`"card"` 这个分支的存在本身就说明飞书把卡片回调归成 `card` 帧。所以按钮要么等上游支持，要么 vendored openlark 自己接上（`[patch.crates-io]` 已有 wechatbot / toasty 的先例）。**未定：** 是否值得为此维护一个 patch——在那之前，答复的路是文本命令（`/approve <短ID>`、`/approve all`），卡片正文已写出命令；卡片回调仍需在开放平台的事件订阅里配置，否则连 `card` 帧都不会来 |
| Telegram 把消息编辑成与现状相同内容时返回的错误（官方未文档化，且声明 `error_code` 内容会变） | §11.3 决定后去掉按钮的幂等重试 | 不匹配错误文案；编辑失败一律当非致命，决定以 Ledger 为准 |
| 补发积压投递的真实代价（启动路径） | §3 第 4 步、§11.4 | **已核实（2026-09-18，本机实测）：它就是"重启好慢"的全部。** `komo gateway restart` 9.0s：bootout + 等待 launchctl 卸完 + bootstrap 只占 0.22s（分别 6ms / 220ms / 11ms），**约 5.1s 花在渠道起来时的按平台补发、2.9s 花在启动时的整体补发**——两者都在 "Gateway 就绪" 之前同步跑，每条 pending 一个平台往返（飞书 ~290ms），而当时积压的投递因为卡片被平台拒（下一行）永远送不出去。改成就绪之后后台跑之后：**2.7s**，其中进程启动到就绪 1.8s。剩余 1.25s 是启动时的 embedding 维度探测（本地 ollama 往返），与补发无关，**已做（2026-09-18，本机 Fedora 实测）**：探测移出就绪路径（§3「就绪也不等模型探测」）——把向量端点指向一个只收连接不回话的 socket 时，修复前发现文件与 `Gateway 就绪` **都在 120.12s**（等满模型超时），修复后 **0.16s**；探测改在后台，`docs/komo_bot.md` 的那 1.25s 同样不再落在重启上。回归测试 `komo-gateway/tests/memory/startup.rs`（预修版本 5s 超时失败） |
| 飞书卡片 2.0 支持哪些组件 | §11.3 卡片渲染 | **已核实（2026-09-18，官方不兼容变更 + 线上报文）：2.0 不再支持 `note` 组件与 `action` 模块**，且 2.0 对不认识的组件是**整张卡打回**而不是忽略。线上表现：每一个审批请求都被拒（`http 400 / code 230099`，`ErrCode 200861 unsupported tag note`），**审批一条都到不了聊天里**，而失败只落在网关日志的一行 WARN 上——审批的主入口（§11.3）整个是死的，操作者只看得到"等待审批"的 Run。替代写法已按官方给的来：备注 = 普通文本组件 + `notation` 字号 + 灰色；按钮行 = `column_set` 每列一个 button，那一块带固定 `element_id` 供决定后整块摘掉 |
| reconcile 一拍的真实成本（目标：1 万 Run / 1 千 Session）与它该排在哪个周期 | §8.9 的周期兜底 | 从 `AUDIT_TICK` 那一拍拆出来，按更粗的间隔跑（例如 5 分钟），或只对"启动后还没对过账的那些"跑；启动时那一次无论如何都要跑 |
| `sessions.state` 之外是否还需要一个"回收进行中"的中间态 | §8.10 的 `purge` | 当前设计靠"墓碑先落、内容后删"取得幂等，不需要第四个状态；若实测发现"内容删到一半"无法与"内容被外部删掉"区分，再补一个状态列值（仍是加列/加值，不改 schema 形状） |
| **既有的 state.db 能不能加上新列**（§8.2 那句"schema 变化只增不改、`ensure_schema` 连上时补列"） | 升级路径：任何一个加了列的新版本在旧库上都起不来 | **已实测（2026-09-20，委派那两列 `runs.parent_run_id` / `runs.delegate`，本机 macOS + 真实 Turso MVCC 库）：补列会静默丢掉。** 现象：旧二进制建的库 → 新二进制启动 → 日志有 `补一列 table="runs" column="parent_run_id"` 两条 → 但同进程随后的每一条用到该列的语句都报 `Parse error: no such column: parent_run_id`（领取 SQL、待处理清单、对账、建会话全中），**重启也没用**；与此同时把 `state.db` 单独复制出来、由另一个进程打开时，同一段 `ensure_schema` 又能"补上"并打印出带新列的 DDL（所以库里没有落盘、那个进程看到的只是自己那份视图）。**新装的库完全正常**（真机端到端委派已验证），坏的只有"旧库 + 新列"这一条路。待办：查 Turso MVCC 下 DDL 的持久化语义（是否必须走非 MVCC 连接 / 是否要升级 turso），再决定补列是改成"用原始连接迁移"还是"表重建"。 **已定位并已改（2026-09-20，同日）：MVCC 连接上的 DDL 不落盘**（`Ok` + 日志照打，重开即无），所以补列改走**建池之前**——`Db::connect` 在 `toasty::Db::builder` 之前用**普通（非 MVCC）连接**把已存在的文件补到当前 schema（`migrate_file`：建缺失表 → `ALTER TABLE ADD COLUMN` → 建缺失索引），全部幂等；`ensure_schema` 退成**守卫**：文件库再缺列就**报错**（"补列必须走建池之前的迁移"），只有内存库（没有文件）还由它补。实现上还有个坑：turso 的读游标拖着一条读事务，**同一条连接上"边读边改"会 panic**在 `vdbe/execute.rs` 的 `SetCookie`（`invalid transaction state for SetCookie: TransactionState::Read, should be write`），所以那段是"一次读清 → 丢连接 → 只用一条只写连接改"。**已验证（真库副本，本机 macOS）**：副本 `sessions` 少 `state`/`state_changed_at`、`runs` 少 8 列 → `Db::connect` 之后 12/31 列齐全、另开一条连接读得回（确实落盘），按模型读 sessions 25 行（含 `origin`）、runs 正常。回归测试两条 `a_missing_column_is_added_on_reopen` / `the_delegate_columns_are_added_to_an_existing_runs_table`：**在普通连接上造旧形状、在另一条新连接上确认落盘**，原先那两条在池连接上造形状，两边都在空转——这正是真机上漏过去的原因。未核实：断电 / 内核崩溃下的持久性，以及升级 turso 后行为是否改变。 |
| 模型一轮里提了两个调用、其中一个没有结果时，会话后面的新 Run 会不会被毒住 | §8.3 回放窗口 | **已实测（2026-09-20，真实 provider）：会。** 旧二进制下模型一轮提了两个调用，一个被拒/没执行，那条 `message.assistant` 留在会话里；其后**同一会话的每一条新 Run** 首个模型请求都被 provider 400 拒（`No tool output found for tool call call_00_…`），且会连着重试。当时用"回放窗口按 Run 过滤"（`service/segment.rs` 的 `window(surface, Some(run))`）挡住了它——**那是拿对话连续性换的**（见本节末与下一行）。**已改（2026-09-21）**：窗口改回"这一段对话"，毒化由两件事挡住——① 正跑的那条 Run 之外的消息**不带协议**（历史 Run 只发布用户正文与它最后答的正文），"有调用、没结果"的半轮因此进不了新请求；② `fail_call` 与按引用恢复让每一次调用都真的有个结果（下面那句）。回归测试：`service::segment::tests::a_later_run_still_reads_what_the_earlier_one_said`（历史那轮的 `tool_calls` / `provider_blocks` 一条都不出现）、`the_running_run_keeps_its_whole_protocol`（正跑的那条一个都不能少）。**已修（2026-09-20，本机真实会话复盘）**：线上那次 400 的调用是两次 `delegate`——两个漏法都补上了。①`resumed()` 重建调用时不按引用读回外置的 `arguments` / `plan`（§8.3 原话要求"读取历史或恢复调用时按引用加载需要的内容"）：委派的任务正文 6 KB 被外置，重 `prepare` 拿到的是一份 `null` 参数，当场失败。②`execute_one` 的"未知工具 / prepare 失败 / 放行被拒 / 子代理不能再委派"四条分支把结论**只交给模型、不写账本**（`Ledger::fail_call` 就是补这一笔）：那次调用于是永远悬着，父的窗口里两个 `function_call` 一前一后，provider 报的是前面那个。回归测试：`service::segment::tests::an_externalized_argument_and_plan_come_back_by_reference`、`service::tests::a_delegated_task_over_the_inline_limit_still_settles_the_parent_call`（去掉任一处的回填就失败）、`service::tests::a_call_that_cannot_run_still_gets_a_result_in_the_ledger`（去掉 `fail_call` 就失败）。 |
| 超限的**正文**（`run.accepted.text` / `message.assistant.text` 过 4 KiB）在回放窗口里是不是也需要按引用读回 | §8.3 的"读取历史或恢复调用时按引用加载" | **已核实（2026-09-20，真实会话）**：会外置（本次会话里两条子 Run 的 `run.accepted` 与一条 `message.assistant` 都是 `text: null` + `text_ref`），而当时全仓**没有任何一处**读 `text_ref`——`replay()` 交给模型的用户消息因此是空的（没炸只是因为子代理的任务正文同时也在它的系统提示里，正常 Run 的用户输入很少过 4 KiB）。**已做（2026-09-21）**：`replay()` 现在按引用读回正文（`segment.rs` 的 `message_text`：内联优先、`text_ref` 次之，哈希由 `PayloadStore::open` 校验，读不出来或不是 UTF-8 都算会话缺内容 → 停下来报告而不是发一条空消息）。回归测试：`service::segment::tests::an_externalized_message_comes_back_by_reference`（用户输入与模型回复两条路）、`a_payload_that_cannot_be_read_stops_the_segment`。 |
| 内嵌 `grep` + `ignore` 之后冷编还在不在 60s 预算内 | §13.4 的编译预算 | **已实测（2026-09-21，本机 macOS M5，`cargo build --timings`、空 target 目录）：加之前 1m10s（`/tmp` 里 HEAD 的干净 worktree），加之后 1m10s——差值落在噪声里。** 新带进来的 13 个包（`globset` / `walkdir` / `termcolor` / `crossbeam-deque` / `memmap2` / `bstr` / `encoding_rs_io` 等）与 `turso_core` / `aws-lc-sys` 并行，不在关键路径上。**顺带发现：60s 这个预算当时就已经超了**（§13.4 记的 2026-09-16 是 64.2s），与这次改动无关——要么把预算调到实测值，要么回去找关键路径，待办。 |
| 工具结果的正文该给模型多少、由谁渲染 | §8.3 的投影与预算 | **已做（2026-09-21）**：`[execution] model_result_bytes`（默认 8 KiB，热生效）+ `komo-kernel/src/projection.rs` 一处纯函数；事实只有落盘的（事件 + `output.json` 里的 `body.preview`），所以"刚跑完"与"回放"逐字节相同（`komo-gateway/tests/observation` 里有断言）。**未核实**：8 KiB 这个默认值对真实任务够不够——要等真实会话的 recall 次数（§49 的指标）出来再调，别凭感觉改。**已补（2026-09-21）**：那四个数现在有得数了——每份观察落盘多少字节（`tool_output_bytes`）、产物多少（`artifact_bytes`）、投影给模型多少（`projected_bytes`），以及"模型回头 `read` 我们落盘的观察"的次数（`observation_recall_count`，判据是路径落在本 Session 的 `tool-output/` 或 `artifacts/` 底下）。四者都只进 trace（`komo::observation`），不进事件流（§47）；抓取与断言见 `komo-runtime/src/executor/tests.rs` 里那两条（`the_projection_reports_how_many_bytes_it_kept_and_stored`、`reading_back_an_observation_counts_as_a_recall`）。**未接**：§6 同句里的"活动执行时限"（`ExecutionLimits::call_timeout`，300s）仍是代码默认值，没进配置——要么一起接，要么在 §3 里明确它是 start-only。 |
| `read` 一次读回来的正文够不够模型用 | §4 与 §8.3 | **已做（2026-09-21）**：`read` 交给模型的那一段从 400 字符改到 8 KiB（`read::PREVIEW_BYTES`），投影再按预算收。**未核实**：真实仓库里"读一个文件要几次 `read`"——如果还是很多次，说明该按结构切（一次给整段函数）而不是按字节。 |
| 记忆召回的重排值不值（一次判断请求换来的排序） | §9.4 的可选重排 | **已做（2026-09-21）**：`[typesafe]` + `memory.retrieval.rerank` / `rerank_shortlist`——短名单（比 `top_k` 宽，校验拦「一样宽」的组合）交给一条 `Choice`，按回来的概率表排序，再照旧按 `top_k` 截；**只重排不增删**，挑中 `none` 或后端不可用都保持融合顺序。**已实测（2026-09-21，本机真机，4 条候选 / 中文输入）**：`jev-1.13.0` 一次 0.6s、约 650 输入 token，顺序 `[m-4, m-1, m-2, m-3]`——热水器与空调那两条排到了前面。离线验收在 `komo-runtime/src/memory/tests.rs`（抬高本来排不进的条目、`none` 不采纳、后端坏了照常召回、开关关着不发请求），真机那条是 `memory/rerank.rs` 里的 `mod live`（`--ignored`，要 `TYPESAFE_API_KEY`）。**未核实**：真实规模（成百上千条）下短名单取多宽、这一次请求值不值——按真实会话的命中率调，别凭感觉改 |
| 系统提示里的 skill 目录行够不够用（166 个 skill 的机器） | §5.6 的目录行 | **已实测（2026-09-21，本机真实语料 + `komo skills list`）：不够，而且坏在两处。** ① `description: >` / `|` 这些 YAML 块标量没有解析——取值是字面量 `>`，325 份 `SKILL.md` 里 111 份的描述是这么写的，目录行长成 `- log-diagnosis：>`；② 2000 字符的预算按目录序 `continue` 丢弃，166 个不同名字里只剩 16 条进提示。真实会话里模型的第一批调用是读 `cart-loong-diff` / `ask-user`（正是那 16 条里的两条），找 `log-diagnosis` 靠的是 `ls` 整个目录，多花了好几轮。**已做（2026-09-21）**：frontmatter 支持块标量（`>` 折行 / `|` 字面，含 chomping，描述里的冒号不再被当成键）；目录改成**整批**定形状——描述装得下就是"名字 + 一句描述"，装不下就只留名字（166 条名字 2752 字符），名字都装不下时末尾写"另有 N 条没列出来"；默认上限 2000 → 4000。回归测试：`frontmatter::tests::a_folded_description_is_joined_into_one_sentence`、`skills::tests::every_skill_keeps_its_name_when_the_descriptions_do_not_fit`、`a_folded_description_becomes_one_catalog_line`。**未做**：按当前输入排序的 top-K——排序块不能进 system 前缀（§9.4 的前缀缓存），先看补全之后还错不错。 |
