# Komo：Rust 个人 Agent 框架设计

版本：v0.8 · 日期：2026-09-15 · 状态：设计稿，尚未实现。

本文定义一个全新项目。Komo 是程序名称；架构只依据本文的需求与约束。

v0.8 相对 v0.7 的变化：状态数据库改为 Turso + toasty（§8.2）；工程结构改为六个 crate，编译预算列为验收项（§13.4）；飞书 / Telegram / WeChat 聊天入口与 TUI 进入首版，审批主要在聊天里完成（§11）；新增 Skills（§5.6）；补齐 trait 清单（§13.5）。全新实现，不迁移旧代码与旧数据。

## 1. 已确定的范围

**一个 Rust 程序、五个基础工具、一套可恢复的 Agent Runtime。**

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
| 工程结构 | 六个 crate 按依赖重量与变更频率划分；冷编与增量编译时间是验收项                                  |

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
| `komo`                                       | 确保本机 Gateway 就绪，创建新 Session，进入 TUI 聊天                        |
| `komo resume SESSION_ID`                     | 连接原会话并查看进度或处理待办；恢复调度由 Gateway 自动进行，不重复创建 Run |
| `komo session list`                          | 查看会话列表和状态                                                          |
| `komo run inspect RUN_ID`                    | 查看执行过程、工具结果与产物                                                |
| `komo run cancel RUN_ID`                     | 请求取消运行                                                                |
| `komo gateway`                               | 启动后台 Gateway，等待就绪后返回                                            |
| `komo gateway --foreground`                  | 前台运行 Gateway，供服务管理器及调试使用                                    |
| `komo gateway status/stop/restart`           | 管理后台进程                                                                |
| `komo approval list/show/approve/reject`     | 查看和处理待审核操作；等价于聊天里的 `/approve` / `/reject`（§11.3）        |
| `komo cron add/list/run/pause/resume/remove` | 管理定时任务                                                                |
| `komo memory list/search/show`               | 查看自动记忆、来源与确认状态；search 支持 hybrid / keyword / vector         |
| `komo memory confirm/forget`                 | 确认具体版本或停用自动记忆；不删除 Memos 原文                               |
| `komo memory index status/rebuild`           | 查看索引覆盖率或重建当前向量索引                                            |
| `komo config check`                          | 校验各模型、effort、向量参数及其他配置                                      |
| `komo channel list/probe`                    | 渠道清单与连通性核对（飞书 tenant token、Telegram `getMe`、微信凭证文件）；不经 Gateway |
| `komo channel wechat login`                  | 终端显示二维码完成微信登录，凭证写入数据目录                                |
| `komo pair approve/revoke/list`              | 准入聊天发送者（§11.2）                                                     |
| `komo skills list/inspect/enable/disable`    | Skills 目录（§5.6）；只读文件系统，不经 Gateway                             |

聊天启动顺序：

1. 读取当前实例的连接配置和发现文件。
2. 检查 Gateway 健康状态与实例身份，不能只凭 PID 或端口判断。
3. 本机实例未运行时，请求系统服务管理器启动。
4. 等待服务就绪，超时则返回具体诊断信息。
5. 建立聊天连接并订阅会话事件。

Gateway 获得数据目录进程锁并完成存储校验后，自动扫描未完成运行；不必等用户打开 CLI 或发送 resume。恢复与新请求共用调度器，恢复扫描本身不等待全部旧任务完成才提供服务。

Gateway 对数据目录持有进程锁。多个 CLI 同时启动时，只允许一个 Gateway 接管实例；启动失败不能通过删除仍有效的锁来强行重试。

Fedora 使用 systemd 管理，Mac 使用 launchd；服务管理器运行前台形式的 Gateway。Mac 若后续需要操作用户桌面应用，应按登录用户的执行环境配置。后台服务不会让睡眠中的电脑继续执行任务。[Fedora systemd](https://fedoraproject.org/wiki/Packaging:Systemd) · [Apple launchd](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)

默认监听回环地址。首版远程访问可通过 SSH 转发连接；直接开放网络监听时需要 HTTPS 和认证。选择远程实例时，不因连接失败而启动一个本机替代实例。`status` 与 `stop` 不隐式启动服务。

## 4. 五个基础工具

| 工具     | 输入与输出重点                             | 执行约束                         |
| -------- | ------------------------------------------ | -------------------------------- |
| `read`   | 路径、读取范围；返回文本和文件版本         | 大文件截断，明确显示未读范围     |
| `write`  | 路径、完整内容、可选预期版本               | 原子替换；覆盖现有文件需检查版本 |
| `edit`   | 路径、明确匹配内容、替换内容、预期版本     | 匹配失败返回错误，不模糊猜测     |
| `shell`  | 命令、工作目录、超时；返回退出码和输出     | 管理进程组、限制输出、支持取消   |
| `python` | 代码或已保存模块调用；返回结果、输出和产物 | 使用受管理解释器，绑定代码版本   |

搜索通过 shell 或 Python 完成；HTTP 请求通过 Python 库或命令完成。Git、构建、测试、HA、网页搜索和记录查询都组合这些基础工具。

工具执行的公共能力放在 ToolExecutor：参数校验、执行计划生成、Policy 判断、审批处理、执行状态保存、取消和输出限制。

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

    /// §8.6：对 started 而无结果的调用核对目标状态。默认 Unavailable → needs_attention。
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

README 与模块说明提供用法。LLM 通过 read 查看说明，再通过 python 调用。保存工具不会扩张模型侧的五个工具 Schema。

HA、Memos、搜索服务地址和凭证引用通过配置传给已授权模块。原始凭证不放进模型提示词，不打印到对话或日志。

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
- 只有系统提示里的**目录行**（名字 + 一句描述，总量有上限）是启动时快照，为了提示前缀稳定；`platforms:` / `requires_tools:` 只门控这份目录，不门控加载。
- **没有 skill 工具。**模型通过 `read` 读 `SKILL.md`；skills 目录是 Policy 里的只读根，读取 Allow。Skill 里写的"可以直接执行"不构成授权，Policy 只看 `ExecutionPlan`。
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

首版顺序执行同一轮的多个调用，减少文件操作顺序歧义。不同 Session 可并发，同一 Session 的 Run 顺序执行；等待审批的 Run 保留会话顺序位置，不允许后续 Run 越过它执行。等待运行排到前面时才装配上下文，避免读到过期历史。

Gateway 设置总轮数、活动执行时限、输出长度和子进程并发预算。等待用户时释放运行名额，保留已消耗预算。

普通工具失败作为结果交给模型修正；模型回复截断或调用参数未收齐时不能开始执行。远端写入结果不明时停止自动重试，转为结果核对。

普通追问可以作为 assistant 回复结束本轮；用户下一条输入开启同一 Session 的新 Run。工具审批则暂停原 Run，待决策后继续原调用，不增加第六个交互工具；决策通常来自聊天渠道（§11）。

Run 完成仅表示本轮结束。测试是否通过、性能是否改善、设备是否到达目标状态，都要依据具体执行证据报告。

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
| Python call                                   | 按已审核版本、导出函数、参数与授权范围判断                           |
| 自动提取记忆与生成索引                        | 在配置的来源、模型端点和记忆范围内 Allow；推断不能自行升级为用户确认 |
| Memos 的写入、修改或删除                      | 按 Python 模块版本、函数、参数与用户指令范围审核                     |
| 权限扩大或修改 Policy                         | 通过操作者配置流程处理，不能由模型自行放宽                           |

### 7.2 审批对象与范围

审批绑定一份不可变的执行计划，包括 operation_id、来源、关联 Run / ToolCall（如有）、规范化参数、工作目录、目标版本、脚本或模块版本、环境版本及相关资源引用。界面——聊天里的审批消息或 TUI 弹窗——显示具体动作、原因、改动和已有验证结果，渲染见 §11.3。

首版提供：

- 本次调用授权：只批准眼前的执行计划。
- 本次 Run 的范围授权：例如指定目录的写入，或明确命令模板。
- Cron Job 授权：绑定 Job 版本、工具或模块版本、参数范围和权限。

不把“同意一次 Python”解释为“今后任意脚本均可执行”。审批不覆盖显式 Deny。批准后再次校验目标和版本，变化则重新评估。

### 7.3 执行边界

内置文件工具可以在 Rust 中执行路径与版本限制。任意 shell / Python 代码一旦启动，则具有其运行账号的操作系统权限；cwd 和参数检查不是完整进程沙箱。

因此，首版的人工作用是批准具体代码执行或明确信任范围。若配置要求强制禁止某类文件或网络访问，而当前执行环境无法约束任意代码，Policy 必须拒绝不受约束的 shell / Python，直到提供相应隔离。不能一面声明绝对禁止，一面让脚本任意访问。

已审核模块可按明确版本与参数信任其实现，但模块的权限声明本身不能证明隔离有效。

### 7.4 审批状态与恢复

```text
Ask
 → 先持久保存待审核计划的 JSONL 引用
 → 审批请求、waiting_approval 与审计待写事件在 state.db 事务提交
 → 释放执行名额
 → 投递到 Run 的来源会话与 home chat（§11.4），TUI 同时可见
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

## 8. 通用 Session 存储与自动恢复

### 8.1 三个核心对象

| 对象     | 含义                                                      |
| -------- | --------------------------------------------------------- |
| Session  | 连续对话、工作目录和上下文的载体                          |
| Run      | 一次用户输入或一次触发引起的持久任务；重启前后保持同一 ID |
| ToolCall | 一次有独立执行状态的具体调用                              |

每个调用有 Runtime 分配的内部 ID，同时保留 provider call_id 和模型轮次；重复的 provider ID 不能误命中其他轮次。Run 可以经历多次进程执行，但不会因为重启而创建新业务任务；ToolCall 是逻辑动作，tool_attempts 记录它实际执行过的各次尝试。

本节的“任务完成”是 Run 已有明确终态，不能根据最后一条助手消息猜测。长期 Memory、任务摘要和模型生成的计划均不能代替执行账本。

### 8.2 存储分工

**每个 Session 的内容集中保存在 sessions/{session_id}/ 下。** JSONL 记录调用、状态、引用和预览；大参数、大模型回复、完整结果、stdout / stderr 分别保存在该目录的子目录中。state.db 维护调度状态、索引与授权，JSONL 与数据库都不重复保存完整 tool output。

| 保存位置                                                           | 内容                                                                         | 权威来源与恢复方式                                             |
| ------------------------------------------------------------------ | ---------------------------------------------------------------------------- | -------------------------------------------------------------- |
| sessions/{session_id}/events.jsonl                                 | 消息、调用与准备计划、开始 / 结束事件、参数和输出引用、摘要与完成事件        | 事件顺序与调用关系的权威来源；完整内容通过引用读取             |
| sessions/{session_id}/payloads/                                    | 超限的模型消息或执行计划正文，包含大参数                                     | JSONL 保留引用与内容哈希；同一参数不再另存重复全文             |
| sessions/{session_id}/tool-output/{run_id}/{call_id}/{attempt_id}/ | output.json，以及按需保存的 stdout.txt / stderr.txt                          | 每次执行的完整工具输出，包含错误详情；按尝试独立、完成后不可变 |
| state.db（Turso，MVCC）                                            | Session / Run 元数据、任务队列、领取代次、工具状态与引用、审批、投递记录、Cron、Memory | 调度与授权权威；其中执行内容索引和派生状态可从 JSONL 补齐 |
| sessions/{session_id}/artifacts/                                   | 工具生成的二进制文件、脚本快照与报告                                         | 独立产物，通过工具输出中的引用定位；不重复复制到 tool-output   |

state.db 中的主要表：

| 表                          | 主要内容                                                                                                                           |
| --------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| sessions                    | 标题、来源、工作目录、当前 Run、JSONL 路径、applied_seq 和已应用字节位置                                                           |
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

**state.db 是 Turso，MVCC 模式，通过 toasty 访问。** 文件同步和数据库提交各自有持久化边界，不能称为跨文件原子事务；数据库提交是否 fsync 在 Turso 上尚未核实（§14 待验证事项），设计不依赖它：内容权威始终是 Session 目录，每一步先 `sync_all` 再往下走，state.db 只是调度与授权的权威。若核实结果是提交不保证落盘，审批决定要补一道保护——决定与 `control_outbox` 同一事务写入，outbox 补写到 JSONL 后才向客户端确认"已批准"；JSONL 里的审计副本仍不能创建授权，只用来在数据库丢失该行时拒绝执行并要求操作者重答。

引擎事实与由此而来的规则：

| 事实 | 规则 |
|---|---|
| MVCC `concurrent_writes`：不同 Session 的写入器并发提交，冲突的提交**失败而不是等待** | 每个写操作包在 `with_write_retry`；多表序列放在真实事务里、再包进 retry——回滚后干净重跑，绝不双重应用。闭包只依赖入参和事务内读到的状态，里面不 `await` 模型、子进程、文件同步或用户；重试超限报 `Contended`，按 §8.4 进 `waiting_retry` |
| MVCC 拒绝 `AUTOINCREMENT` | 每个主键是 `String` UUIDv7；Session 内的 `seq` 是 JSONL 写入器分配的整数列，不由数据库生成 |
| MVCC 下自定义索引模块不可创建（FTS 不可用） | 关键词检索在索引时分词写入 `memory_terms`（§9.4） |
| toasty 的类型化 API 表达不了 `ALTER TABLE`、`instr`、`vector_distance_cos`、带条件的 `UPDATE` | `Db` 持有第二个 `turso::Database` raw 句柄；raw SQL 只允许出现在 store crate 的三个模块：schema、keyword index、run claim |
| 每个 db 文件由进程独占锁定 | Gateway 是唯一打开 state.db 的进程，CLI 与 TUI 永远走 HTTP（§3） |
| `turso` 默认特性启用 `mimalloc` 全局分配器 | 工作区只能有一个 turso 版本，且与 `toasty-driver-turso` 的 pin 同 major；其他 crate 不得声明 `#[global_allocator]` |
| toasty `push_schema` 只对新文件执行，且不幂等 | 见下面的 schema 演进 |

Schema 演进没有迁移脚本目录。每个 toasty 模型旁边放它的 `*_TABLE_DDL` 常量；`Db::connect` 对已存在的文件逐表 `CREATE TABLE IF NOT EXISTS`、逐列 `ALTER TABLE ADD COLUMN`，对新文件让 toasty 建表。一个测试对每张表断言 toasty 为空库生成的 DDL 与常量**字节相等**——模型改了列却没改常量，测试挂。新列必须 `NOT NULL DEFAULT …` 或可空；退役的列继续写空值，不删。耐久表（`approval_requests`、`policy_grants`、`cron_jobs`、`cron_firings`、`memory_items`、`memory_evidence`、`deliveries`）只允许加法变更；可重建表（`session_log_index`、`tool_calls`、`tool_attempts`、`checkpoints`、`memory_terms`、`memory_vectors`、`memory_index_generations`）按行或按代次从 JSONL / 原文重建，从不删文件——一个文件里同时住着耐久表，"删掉重来"不存在。

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

读取历史或恢复调用时按引用加载需要的内容；组装模型上下文仍遵守输出预算，超限时提供截断提示和可读取的完整文件引用。当前 Session 获授权的输出可通过 read 只读访问，不开放普通工具修改这些记录。摘要或预览不能代替恢复所需的原始参数与结果。

实现约束：

- Gateway 对每个 Session 使用一个串行写入器，模型、工具、审批审计与 Memory 来源标记均经过它，不能各自追加导致行内容交错。
- 完整记录写入后清空用户态缓冲，再同步文件；同步失败就停止该步骤，不能执行后续副作用或向客户端确认持久完成。新建文件和目录还要处理目录持久化。Rust 的 File::sync_all 提供文件同步接口，关闭文件不等于已验证同步成功。[Rust File](https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all)
- 完整模型回复和该轮调用计划保持为一个逻辑 assistant 事件：正文小则内联，大则引用已经完整持久化的 payload 文件，避免只恢复出半轮调用。工具输出先独立持久化，再一次提交对应结果事件，不按流式 token 写 JSONL。
- 正常修改只追加新事件；纠正、取消和摘要覆盖关系通过新记录表达，不重写旧行。
- 读取依据事件 ID / seq；字节偏移只是加速索引，校验不符时重新扫描并重建。每轮通过检查点与尾部增量读，避免反复解析完整会话。
- 模型请求保存所用事件范围、模型 / effort、工具与记忆版本引用；摘要正文作为 JSONL 事件保存，检查点不再复制整段历史。
- Memory 的证据引用使用 Session ID + event_id / seq，稳定定位原始消息或工具结果。
- JSONL、state.db 和引用产物均为 Gateway 的受保护状态；普通工具不能通过 write / edit / shell / python 直接修改。回放只能接受 Gateway 写入的合法记录，不能把模型生成的 JSON 当作执行事实或授权。

尾部恢复时，先校验数据库已经引用的记录和检查点，再处理尚未应用的后续记录。只有末尾不完整、且不被已提交引用覆盖的行，才能先隔离保留原始字节再截断；完整有效行不能因为索引落后被删除。文件中间损坏、已提交范围缺失或哈希不匹配时停止受影响会话的自动执行，不能跳过坏行或退回旧检查点继续。

未知事件保留原始内容与序号；影响执行、权限或调用配对的未知版本必须暂停恢复并要求兼容版本，不能按无关日志忽略。

### 8.4 重启后的默认行为

**Gateway 自动接续能够确定恢复位置的任务。用户只处理审批、时效或结果不明等确实需要判断的情况。**

| 重启前停在哪里                                        | Gateway 启动后怎么处理                                       |
| ----------------------------------------------------- | ------------------------------------------------------------ |
| 输入已在 JSONL 持久保存，Run 尚未开始                 | 补齐 state.db 索引后自动入队                                   |
| 输入请求只预留了 ID，正文尚未完整写入                 | 保持 ingesting；等待同请求键重传，不能执行缺失输入           |
| 正在请求 LLM，完整回复尚未保存                        | 丢弃未完成输出，以已保存上下文重新请求；可能再次产生模型费用 |
| assistant 回复和调用计划已保存                        | 沿用原计划，从未完成的 ToolCall 继续                         |
| JSONL 已有结果引用且完整输出校验通过，state.db 可能落后 | 先补齐结果索引和状态，复用原输出，不重放动作                 |
| 调用 planned，确定尚未执行                            | 校验原计划与当前权限后自动执行                               |
| 调用 started，没有结果                                | 先核对外部效果，按 8.6 决定是否安全继续                      |
| 等待审批                                              | 保留原请求；已答复且仍有效的批准自动接续                     |
| 等待临时故障重试                                      | 沿用已保存的次数与 next_retry_at，到期再尝试                 |
| 已保存最终结果，但客户端没有收到                      | 补发或补读原结果，不重新执行任务                             |
| completed / failed / cancelled                        | 保持终态，不因重启自动开启新一轮                             |

内部状态示意：

```text
Run:
ingesting → queued → running → completed / failed / cancelled
            ├─ waiting_approval → queued
            ├─ waiting_retry   → queued
            ├─ interrupted     → 核对后 queued / needs_attention
            └─ needs_attention → 操作者处理后 queued / cancelled

ToolCall:
planned → started → completed / failed
                  └─ uncertain → 核对完成 / 确定可重试 / 等待处理
```

waiting_approval、waiting_retry、interrupted、needs_attention 都是未完成状态。审批或处理完成后继续原 Run；同一 Session 后续 Run 不越过它，其他 Session 正常运行。

用户明确取消的 Run 不自动复活；普通错误达到重试或执行预算后进入 failed。Gateway 关闭或系统重启属于中断，不等于用户取消。已经授权执行但有时间限制的动作，先检查有效期；过期不能按旧指令直接产生新的外部影响。

### 8.5 JSONL 与 state.db 的写入顺序

消息和调用事件遵循“先持久化外置正文（如有），再持久化 JSONL，最后提交 state.db 状态与引用”。每个 Session 的协调入口串行完成追加与索引推进；applied_seq 只能推进连续、已校验的事件前缀。数据库事务中不等待模型、用户、子进程或文件同步。

```text
收到输入
  → state.db 用请求键预留 Run ID，状态 ingesting，仅存输入哈希与来源
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

同一请求键重发时返回原 Run，内容哈希不同则拒绝。ingesting 的完整输入若已在 JSONL 中，启动修复就能补为 queued；如果正文从未写完，则等待客户端重传，不能凭输入哈希补造用户指令。Cron 在创建 firing 与预留 Run 的事务中保留不可变触发快照，可以据此完成尚未写入的触发输入。

恢复先处理已同步但未索引的 JSONL 尾部，再判断任务是否需要执行。例如 tool.result 已经完整持久保存且引用输出校验通过、数据库仍显示 started，必须先补齐为已有结果，不能直接按 uncertain 重试。若结果引用存在但对应输出缺失或哈希不符，停止受影响任务，不能重跑来掩盖数据损坏。输出文件已写完但 tool.result 尚未提交时，只有完整校验文件内的调用身份、计划、完成状态与内容后才能补记结果；仅凭路径存在不足以宣告成功。

回放只补索引与派生执行状态，不调用工具、不发送外部请求、不消费授权，也不能覆盖 state.db 已记录的用户取消或权限撤销。tool.started 本身不能证明已产生副作用或已经通过授权消费；缺少 tool.result 的调用仍进入核对流程。

审批等控制操作的权威在 state.db，采用相反方向的审计补写：

```text
state.db 事务提交审批决定与 control_outbox（固定 event_id）
  → Session 写入器将审计事件追加到 JSONL 并同步
  → state.db 标记 outbox 已交付并更新日志索引
```

重启后重发同一 outbox 事件先按 event_id 去重，已写入就复用原事件位置。审批生效不依赖审计补写成功；客户端恢复时可直接查询数据库中的原决定。JSONL 中的审计副本无法反向生成新的授权，outbox 也不承载完整工具结果。

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
| 无可靠恢复方式   | 停在 needs_attention；不自动从头执行整个脚本                             |

文件 write / edit 可记录修改前后内容哈希与目标身份。若恢复时目标已是预期内容，则记录“核对后目标已满足”；仍是原内容且其他前提成立，可重新执行同一原子修改；出现第三种内容则视为冲突。不能把文件已存在当作写入成功，也不能编造丢失的 stdout 或原始返回值。

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
  → 校验受影响 Session 的 JSONL，修复未索引尾部和派生状态
  → 补写控制审计 outbox，核对尚未接收完整的输入
  → 创建本次启动身份
  → 将旧执行实例的 running 标为 interrupted
  → 检查旧子进程与未完成调用
  → 按 Session 顺序恢复原 Run
  → 可继续的提交统一队列
  → 需要审批或核对的保留待处理状态
  → 周期扫描，补齐内存通知丢失的任务
```

启动扫描、Cron、审批回调和手动 resume 共用同一个领取入口。领取通过数据库条件更新与递增代次保证同一 Run 只被一个执行者接管；旧代次不能继续提交新状态。单实例使用进程锁与数据库事务即可，不引入跨机器选主或分布式队列。

JSONL 追加和状态提交都校验领取代次。领取代次只防止旧执行者写账本，不能撤销已经发出的外部动作。因此必须先确认旧执行已停止，再考虑启动替代执行。

首版不承诺让 shell / Python 进程本身跨 Gateway 重启继续存活。正常停机时停止接收新执行，给正在完成的工具短暂收尾时间，持久化可用结果；超时则回收所属进程及子进程，剩余任务标为 interrupted。关键文件原子修改完成后再停，不在半次替换中主动取消。

异常退出可能遗留子进程，不能仅凭一个 PID 判断是否为原进程；结合平台监督信息、启动身份和进程启动时间核对。无法确认旧执行已结束时，阻止该任务重复启动并显示原因。Tokio 的 Child handle 被丢弃并不默认终止进程，因此 kill_on_drop 不能取代异常退出和进程树回收方案。[Tokio process](https://docs.rs/tokio/latest/tokio/process/index.html)

长脚本要拆成多个可观察的工具步骤，或由已审核脚本保存阶段进度。首版恢复到工具调用边界，不恢复 Python 内存、调用栈或某一行。脚本只有“进度标记”而没有副作用核对，仍不足以保证安全续跑。

### 8.8 恢复所需的上下文与用户体验

未完成 Run 引用的 Session 目录及其 JSONL、payloads、tool-output、产物，还有外部工作目录、代码与环境快照、审批和配置，都不可被普通清理策略删除。tool-output 不作为临时日志按天直接清扫；已结束会话清理时也先处理恢复、Memory 和审计引用，显式清理后不得将缺失输出伪装成仍可回放。目录丢失、脚本版本不可用、权限被撤销或模型协议不再可回放时，保存具体原因并等待处理，不能换到另一个目录或新版脚本自动重来。

原始请求来源与操作者身份也要保存。Cron 恢复后仍是 Cron，不能因为重启后由本机 Gateway 发起，就取得交互操作者的额外权限。Memory 的过期、遗忘和版本检查照常进行。

CLI 不承担恢复调度。打开 komo 时可以看到“2 个任务已接续，1 个等待审批”；komo resume SESSION_ID 连接到原任务，多个客户端同时连接不会开启多个执行。自动恢复无需再次询问是否继续，只有新增权限、过期动作、冲突或无法核实的效果才打断用户。

SSE 从已同步且索引完成的 JSONL 事件补读；审批当前状态可直接查询 state.db，不依赖用户一直在线。之后接入 Telegram 等主动推送渠道时，应为待发送结果持久保存投递记录，不能为补发一条结果消息重跑任务。

用户处理 uncertain 时，应先看到原操作、已有证据和待确认事项。如果决定终止，则补齐明确的取消 / 未知结果；若允许继续，也不能伪造原调用成功。任何后续模型请求都保持完整的调用与结果配对。

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
       → 去重，核对当前 revision、来源与冲突状态
       → 按条数和 token 预算选入上下文
       → 返回内容、来源、确认状态、时间与版本
```

默认 hybrid；同时支持 keyword 和 vector 供明确选择与诊断。每路建议最多召回 40 项，合并后最多注入 8 项、约 1500 tokens；这些是可调整的初始预算，不强行填满。没有足够相关内容时返回空；相似度只用于相关性，不授予真实性或操作权限。

关键词索引不用 FTS5（Turso MVCC 下不可用，§8.2）：索引时把内容经 `lexical_terms` 切成 token 串写入 `memory_terms.terms`（首尾带空格），查询时每个 token 一个 `instr(terms, ' tok ') > 0`，命中数按 IDF 加权。`lexical_terms` 对 CJK 连续字符切 bigram、对 ASCII 切单词并小写化——"空调"本身就是一个 bigram，两字查询天然命中，不需要子串后备。IDF 在查询时用 `memory_terms` 行数和每个 token 的命中行数算；1 万条量级是一次全表 `instr`，§14 要求实测 P95，这里不预先宣称达标。

首版将向量保存为带编码和维度信息的 f32 数据，由 Rust 在作用域过滤后做精确余弦检索。进程可缓存当前代次的向量，缓存按预算管理且可从数据库重建；无须部署独立向量服务。后续只有在真实数据规模与延迟测试表明必要时，才在 memory_index 内部替换为近似检索。

向量服务短暂故障时，hybrid 退化为关键词并在检索元信息中标记 degraded / 原因 / 覆盖率；vector 模式明确返回不可用。未配置向量模型却选择 hybrid / vector 属于配置错误，不能静默变成长期关键词模式。

后台刚入库但尚未生成向量的记忆仍可通过关键词命中。每次模型请求前复查被选条目的有效性，正常情况下沿用 Run 的选择，不逐轮重复请求 embedding。新用户输入或任务发生实质变化时再召回。

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
 → state.db 事务插入 cron_firing、触发快照并预留 Session / ingesting Run
 → JSONL 持久保存触发输入，state.db 提交 queued
 → 普通队列执行
 → 保存结果和产物
 → 更新本次触发状态
```

每个 Job 包含名称、五字段 cron 表达式、时区、prompt、工作目录、enabled、执行预算、重叠策略及版本化授权。可指定该 Job 的主模型与 effort；覆盖按完整模型配置解析，不能影响记忆整理或向量模型。下面示例所需的搜索与 Memos 操作仍须匹配具体模块版本及授权。

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
- 时区明确保存；同一日历时间因夏令时出现两次时，两次不同 UTC 时刻分别视为计划时间；不存在的本地时间跳过。
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
                              2. 解析 Principal（allow_from / pairing）
                              3. 解析 Conversation → SessionId
                              4. 冲刷该会话 pending 的投递（§11.4）
                              5. 命令？→ 直接处理并 ack
                              6. 普通文本 → Ledger::accept_input → 排队
                       回复：订阅该 Session 的事件流，Run 终态时把最终 assistant 消息发回来源会话；
                             审批请求、needs_attention 走 Notifier（§11.4）
```

- `request_key`：飞书是 `feishu:{chat_id}:{message_id}`，Telegram 是 `telegram:{chat_id}:{update_id}`，WeChat 是 `wechat:{from}:{msg_id}`。三个平台都是至少一次投递（飞书 ws 断线重连后会重推未 ack 的事件），重发命中同一 Run；**命令也去重**——重发的 `/approve` 不会批准两次。这正是 §8.5 请求键的语义，不另做 inbox 表。
- 飞书 ws 在独立线程上跑 openlark 的运行时，事件通过 channel 交给 tokio 侧的 `Channel::serve`；ack 在 Dispatcher 返回 `InboundAck` 之后发，不是收到就 ack。
- 渠道交给 Dispatcher 的是 `InboundMessage { peer: ChannelPeer, is_private, sender, text, request_key }`，**不是 Session ID**。哪个会话属于 Dispatcher 与存储。

### 11.2 身份与会话

- **Principal**：`allow_from` 里的发送者是操作者；其余必须配对（`komo pair approve <code>`，码盐哈希、限速、1h 过期）。未配对的消息得到一条配对提示，不进入 Run。**审批命令只接受操作者。**
- **Conversation**：操作者的**私聊**（飞书 DM、Telegram DM、WeChat、TUI）落到同一个 **home session**——早上在微信说的话，回到终端接着说；飞书 / Telegram 的群聊按 `{platform}:{chat_id}` 各自一个 Session，群里只响应 @机器人 的消息并剥掉提及。WeChat 只有 DM。
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
| `/approve <short_id>` / `/reject <short_id>` | 打 `POST /v1/approvals/{id}/decision`；已决定的返回原决定，不报错 |
| `/approve` / `/reject`（无 ID） | 操作者只有**一个**待处理请求时生效；多于一个则列出并要求指明 |
| `/approve <short_id> run` | 本次 Run 的范围授权（§7.2 第二种）；只对 Policy 标记为可范围化的计划生效，`Deny` 不可覆盖 |
| `/pending` | 列出待处理审批及其短 ID |
| `/new` | 当前 Session 追加 `conversation.boundary`，不切 Session |
| `/cancel` | 取消该 Session 当前 Run |
| `/status` | 当前 Run 状态、待审批数 |
| `/sethome` | 把当前会话设为 home chat（§11.4） |

按钮回调与文本命令走同一个 `Dispatcher::handle`：飞书卡片回调（`card.action.trigger`）的 `request_key` 是 `feishu:{chat_id}:{event_id}`，Telegram `callback_query` 的是 `telegram:{chat_id}:{callback_query_id}`。回调里带的 `approval_id` 是渠道回传的数据，只用来**定位**请求；批准与否仍由 Dispatcher 核对 Principal 后决定，回调负载不构成授权。

一条普通消息在 Run 等待审批时到达：Run 不会被越过（§6），消息排在它后面；不做"插话替换审批"这类特殊路径。

### 11.4 主动投递

§8.8："应为待发送结果持久保存投递记录，不能为补发一条结果消息重跑任务。"

- `Notifier::deliver` 先在 `deliveries` 表写一行（目标、内容引用、状态 `pending`），再发送，成功后标 `sent`。重启后 `pending` 的行补发，按 `DeliveryId` 幂等。
- **审批请求的投递目标**：Run 的来源会话，**加上** home chat（若不同）。两处都能回答，第二个答复得到"已决定"。来源是 Cron 或已断开的 TUI 时只有 home chat。
- home chat 解析：`/sethome` 的设置 > 配置 `home_chat`（可列多个渠道）> 无目标时返回错误给调用方，**不静默丢弃**。配置候选的默认顺序是**飞书 > Telegram > WeChat**：前两者能通过 API 对任意已加入的会话主动推送，微信不能（下一条）。
- **WeChat 的平台约束**：DM 回推依赖用户最近一条消息带来的回复令牌（进程内、有时效）；进程启动后用户没发过消息时**无法主动推送**。对应处理：该渠道的 `deliver` 在没有令牌时把行留在 `pending` 并返回 `Deferred`；用户下一条消息到达时 Dispatcher 先冲刷该会话的 `pending` 投递（§11.1 第 4 步），再处理新消息。审批请求因此不会丢，只会晚到；`home_chat` 里排在它前面的飞书或 Telegram 会先送到。
- TUI 的 SSE 是另一个 Notifier 实现，不持久化——连接断了从事件流补读。

### 11.5 渠道实现的约束

每个渠道一个 **feature 门控的模块**，SDK 依赖只在该 feature 下进入构建；渠道模块只能依赖 kernel 的类型和 `Inbound` / `Notifier` 两个 trait（§13.5），不能碰 runtime。`cargo tree -d` 有新重复版本时必须在 PR 里按渠道归因。`komo channel list | probe` 对每个已配置渠道做一次连通性核对（飞书拿 tenant token、Telegram `getMe`、微信检查凭证文件），`komo doctor` 汇总。

## 12. 数据目录与记录习惯

```text
~/.komo/
├── config.toml
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

Session 内容以目录为单位管理和归档，不再使用顶层 tool-output 或 artifacts 分散存放会话数据。自动 Memory 保存在 state.db，用户主动记录保存在 Memos；可复用 Python 能力继续保存在 toolbox，实际项目副本保存在 workspaces。会话 artifacts 中需要长期保留的报告和文件，不因清理聊天历史而自动删除；不能直接删除整个 Session 目录代替引用检查。会话来源需要清理时，先为仍引用它的记忆保留必要的、经脱敏的证据摘录与来源元信息，或停用无法再验证的条目，不能留下伪装有效的引用。重建内容索引不删除 JSONL；清理已结束 Session 的 JSONL 需要同时处理索引、检查点和引用关系。

记忆正文、来源和确认记录需要持久保留；关键词与向量索引可从原文重建，禁止为了重建索引删除整个数据库。Memos 原文由 Memos 自己的备份策略覆盖。

数据目录可通过配置或 KOMO_HOME 改变。配置项中的相对路径按配置文件所在目录解析；JSONL、payloads 与 output.json 中的正文 / 输出引用统一按对应 Session 目录解析。每个实例独占自己的数据库；Turso 对 db 文件持进程独占锁，文件放在运行机器的本地磁盘，不能作为多机器共享数据库。

备份在明确的一致性水位上取得数据库快照，以及各 Session 目录内的 JSONL 已同步前缀、payloads、tool-output 和被引用产物。可短暂暂停持久化协调入口来建立水位，再按固定长度复制追加文件；不能只备份 state.db，也不能把不同时间点的 JSONL 和数据库随意拼接。state.db 的授权与配置数据不能单靠会话 JSONL 重建。

## 13. 通信、技术栈与项目结构

### 13.1 通信

CLI 通过 HTTP 发命令，通过 SSE 观察运行。Gateway 内部采用函数调用，无需在本机模块之间再发 HTTP。

最小接口：

| 接口                             | 行为                                           |
| -------------------------------- | ---------------------------------------------- |
| GET /healthz                     | 最小健康检查与实例标识                         |
| POST /v1/sessions                | 创建会话                                       |
| GET /v1/sessions                 | 列出会话                                       |
| GET /v1/sessions/{id}            | 会话详情与运行状态                             |
| GET /v1/sessions/{id}/events     | 按游标获取或订阅事件                           |
| POST /v1/sessions/{id}/runs      | 提交新输入                                     |
| POST /v1/sessions/{id}/resume    | 检查恢复位置，恢复可继续的运行或返回待处理状态 |
| POST /v1/sessions/{id}/boundary  | 追加 `conversation.boundary`（聊天里的 `/new`）  |
| GET /v1/runs/{id}                | 执行详情                                       |
| POST /v1/runs/{id}/cancel        | 取消                                           |
| GET /v1/approvals                | 待审核列表，列表项包含短 ID、具体动作与计划   |
| GET /v1/approvals/{id}           | 单项审批详情                                   |
| POST /v1/approvals/{id}/decision | 批准或拒绝                                     |
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
| GET /v1/pairings                 | 待准入与已准入的聊天发送者                     |
| POST /v1/pairings/{code}/approve | 准入一个配对码                                 |
| DELETE /v1/pairings/{id}         | 撤销准入                                       |

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
| 状态数据库            | Turso（MVCC）+ toasty；raw `turso` 句柄限于 schema / 关键词索引 / 领取三处（§8.2）      |
| 对话与执行记录        | Session JSONL，由 Serde 序列化；大正文外置                               |
| 工具完整输出          | 按执行尝试保存 output.json / stdout.txt / stderr.txt，流式写入后持久发布 |
| 关键词检索            | state.db 的 `memory_terms` 列：索引时 CJK bigram + ASCII 词分词，查询 `instr`，IDF 加权（§9.4） |
| 向量存储与检索        | state.db 保存向量（f32 BLOB + 维度 + 代次），Rust 在过滤后执行精确余弦检索               |
| 向量生成              | 独立 EmbeddingClient，使用 memory.embedding 配置                         |
| 配置与结构化数据      | TOML、Serde                                                              |
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

首版生成模型与向量模型分别接入一种明确的协议，由具体适配器实现。base_url、model、effort 与凭证引用可配置；相同协议可以连接不同端点。不同协议后续按需要增加，不把“OpenAI compatible”当成所有字段都兼容的保证。生成适配器必须保留协议回放所需的消息块和元数据。

### 13.3 模型角色与统一 effort 配置

模型配置按实际用途独立解析：

| 配置             | 职责                             | 与其他角色的关系                                |
| ---------------- | -------------------------------- | ----------------------------------------------- |
| model            | 对话、计划、tool call 与任务输出 | Session / Cron 可覆盖本角色；运行时固定配置快照 |
| memory.model     | 记忆提取、去重和冲突整理         | 可使用独立服务、凭证、model 和 effort           |
| memory.embedding | 记忆文本与检索查询的向量生成     | 独立端点与模型，固定向量空间；不继承聊天模型    |

所有模型配置复用 ModelConfig 的公共字段：provider、base_url、model、api_key_env、可选 effort、超时等。Embedding 配置额外支持模型 revision、可选 dimensions 和必要输入规则。生成与 embedding 仍由各自的客户端和能力校验处理，不向 embedding 端点发送聊天工具字段。

以下是配置模板，模型名称与服务地址为占位值，effort 示例需要替换为目标模型支持的值：

```toml
[model]
provider = "openai_compatible"
base_url = "https://llm.example.com/v1"
model = "YOUR_CHAT_MODEL"
api_key_env = "KOMO_LLM_API_KEY"
effort = "medium"

[memory]
enabled = true

[memory.model]
provider = "openai_compatible"
base_url = "https://memory-llm.example.com/v1"
model = "YOUR_MEMORY_MODEL"
api_key_env = "KOMO_MEMORY_API_KEY"
effort = "low"

[memory.embedding]
provider = "openai_compatible"
base_url = "https://embedding.example.com/v1"
model = "YOUR_EMBEDDING_MODEL"
api_key_env = "KOMO_EMBEDDING_API_KEY"
# dimensions 省略时使用模型返回维度，校验后固定到索引代次。
# revision 可用于标识服务端同名模型的权重版本。
# effort 只有该 embedding 接口和模型支持时才能显式设置。

[memory.retrieval]
mode = "hybrid"
candidate_limit = 40
top_k = 8
max_tokens = 1500
```

memory.model 整段省略时，继承配置文件中主模型的完整配置，包括 effort；它不继承某个聊天 Session 或 Cron 的临时覆盖。显式配置该段时是独立完整配置，不能拼接一个模型名和另一个模型的 effort / 端点。embedding 必须独立指定；若明确选择 keyword 模式，可以不配置 embedding。

effort 的行为统一，取值按协议和具体模型校验：

- 未配置：不发送 effort 字段，由服务端采用默认行为；不等同于显式 none。
- 显式配置：适配器确认目标模型支持后转换为该协议参数。low / medium / high / max 等值不是所有模型共享的固定集合。
- 不支持：komo config check 和配置启用流程返回具体错误，指出模型、配置位置及支持值；不静默忽略或强行映射为另一档。
- 能力未知：需要显式能力声明或在实际启用前完成最小探测。无法确定时拒绝该显式参数；运行中收到不支持参数的响应，也不能删掉 effort 自动重试。
- embedding：配置结构保留可选 effort，但普通向量接口没有该参数时必须省略；dimensions、批大小和输入长度是另一组控制，不用 effort 冒充。
- 后续新增辅助模型、重排模型等角色，也复用同一配置解析与校验入口，不另藏固定 model 或 effort。

不同模型的 effort 可用档位确实不同；例如 Claude 的官方文档按模型列出支持档位，Ollama embed 的请求则列出输入、维度等参数，没有通用 effort 字段。[Claude effort](https://platform.claude.com/docs/en/build-with-claude/effort) · [Ollama embed](https://docs.ollama.com/api/embed)

Gateway 启动时解析一次配置，修改后通过重启生效；保留当前 Run 与后台记忆任务使用的配置快照。resume 默认沿用原运行的模型 / effort；若操作者明确切换，记录配置变更事件并重新验证协议历史可回放性。任何情况下都不能通过重放已完成工具来适应新模型。

每次生成与向量请求记录角色、模型身份、实际 effort 或 provider_default、耗时和错误；不记录密钥。具体服务可能更改默认值，所以未设置 effort 时只记录“服务端默认”，不能虚构当时采用的强度。索引任务还记录向量空间、代次与条目版本。

### 13.4 Crate 划分与编译预算

编译速度是架构约束，不是事后优化。基线：对一个同类 Rust 工作区（9 crate、约 97k 行，同样依赖 Turso / toasty / axum / reqwest）做冷编 `cargo build --timings`，总墙钟 64.7s，关键路径 `turso_core` 13.5s → `toasty` → 持有 toasty 模型的 crate 13.5s → 二进制 9.0s。三个结论：**toasty 派生宏展开极贵**（11k 行编得和整个 Turso 引擎一样久），持有模型的 crate 必须最小、最少被重编；**二进制 crate 在关键路径末端**，任何下层改动都要再付一次，它必须薄；最重的三方单元（`aws-lc-sys` 17s 构建脚本、`tantivy` / `zstd-sys` 14s、`rusqlite`）全部来自默认特性而非需求。

划分原则：按**依赖重量**和**变更频率**切，不按 DDD 层。重依赖（toasty/turso、axum、reqwest、ratatui、渠道 SDK）各自只出现在一个 crate；高频改动的代码所在的 crate 不依赖任何派生宏重的东西；被所有人依赖的 crate 不含 I/O、不含 tokio；二进制只有 clap 分发。

```text
komo-kernel    值类型、状态机、事件与 fold、Policy 规则引擎、cron 到期计算、
               §8.4 恢复决策表（纯函数）、协议线格式、全部 trait（§13.5）。
               deps: serde, serde_json, uuid, time, thiserror, async-trait, croner。不依赖 tokio。

komo-store     session_log（JSONL 写入器 / 范围读 / 尾部校验）、payloads、tool_output、
               Turso Db + toasty 模型 + ensure_schema、repositories、Coordinator（impl Ledger）、checkpoint。
               唯一依赖 toasty/turso 的 crate。deps: kernel, toasty(turso only), turso, tokio(fs, sync), sha2

komo-runtime   agent loop、executor、tools/{read,write,edit,shell,python}、python_runtime、
               policy（config → 规则）、approvals、memory（MemoryManager、索引、代次）、llm 与 embedding 适配器、
               scheduler、recovery、skills、config。日常改动落在这里；它从不重新展开 toasty 宏。
               deps: kernel, store, reqwest, tokio

komo-gateway   axum 路由（§13.1）、SSE、认证、进程锁与发现文件、launchd/systemd 集成、
               Dispatcher、飞书 / Telegram / WeChat 渠道、Notifier 实现、deliveries、审批消息渲染。
               deps: kernel, runtime, axum, tower-http, reqwest, openlark(feature "feishu"), wechatbot(feature "wechat")

komo-client    HTTP + SSE 客户端、发现文件读取、ratatui 聊天 TUI（含审批弹窗、Markdown 渲染）、
               操作子命令的输出渲染。只依赖 kernel——不认识 store / runtime。
               deps: kernel, reqwest, tokio, ratatui, crossterm, pulldown-cmark

komo (bin)     clap 分发：`komo` / `komo resume` → client 的 TUI；操作子命令 → client；`komo gateway` → gateway。
               deps: client, gateway, clap
```

依赖只向下，两条支线在 bin 汇合：

```text
kernel ← store ← runtime ← gateway ─┐
kernel ← client ────────────────────┴─ komo (bin)
```

`gateway` 看不到 toasty 类型（store 只通过 runtime 的构造函数暴露）也看不到 ratatui；`runtime` 看不到 axum；`client` 只认协议类型。改 TUI 不重编服务端，改服务端不重编 TUI；冷编时两条支线并行。什么时候再拆：runtime 里 memory 一旦超过 8k 行或引入自己的重依赖，拆成 `komo-memory`；触发条件是编译时间实测，不是模块数。

依赖清单与禁用项：

| 依赖 | 配置 | 理由 |
|---|---|---|
| `toasty` | `default-features = false, features = ["turso"]` | 去掉 `sqlite` 特性拉进来的 rusqlite / libsqlite3-sys |
| `turso` | 与 `toasty-driver-turso` 同 major，工作区唯一 | mimalloc 全局分配器只能有一个 |
| `reqwest` | `default-features = false, features = ["rustls-no-provider", "json", "stream", "charset"]` | `rustls` 特性 = `__rustls-aws-lc-rs`；`no-provider` 后由 komo 在 `main` 里 `rustls::crypto::ring::default_provider().install_default()`，且必须在飞书 ws 线程启动之前 |
| `rustls` | `default-features = false, features = ["ring"]` | 同上 |
| `tokio` | 按需特性，不用 `full` | kernel 不依赖 tokio |
| `axum` | 默认 + `tower-http/cors` | 仅 gateway |
| `clap` | `derive` | 仅 bin |
| `ratatui` + `crossterm` + `pulldown-cmark` | 仅 komo-client | 在 client 支线上，与 gateway 并行编译 |
| `syntect` / `two-face`（代码高亮） | 可选，按编译预算实测再定 | 纯 UI 增强，不是需求 |
| `openlark` | 仅 gateway，feature `feishu`（默认开），`default-features = false, features = ["websocket"]` | 只要 ws 长连接；事件负载用 `register_raw` 拿原始 JSON、自己的宽容 serde 结构解析。带来 `prost 0.13`（turso 同步引擎用 0.14）和一份 `tokio-tungstenite` 重复——接受，不再自己额外引入 tungstenite |
| `wechatbot` | 仅 gateway，feature `wechat`（默认开） | 它 pin `reqwest 0.12` + native-tls，带来一份 reqwest / hyper 重复——接受。Linux 下**不** vendored openssl：每台机器在本机构建，Fedora 装 `openssl-devel` 链系统库 |
| `qrcode` | `default-features = false`；微信登录二维码渲染为终端字符 | 不需要 `image` |
| **不引入** | rmcp / image | MCP 不在首版 |
| `tantivy` / `zstd-sys`（经 `turso` 默认特性 `fts`） | 可选：`[patch.crates-io]` 一份 `toasty-driver-turso`，把 `turso` 改成 `default-features = false, features = ["mimalloc"]` | 只影响冷编（约 14s）；FTS 在 MVCC 下本来就不可用。留到冷编成为痛点再做 |

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

| 场景 | 命令 | 初始预算 |
|---|---|---|
| 冷编 | `cargo build` | ≤ 60s（基线 65s：去掉 aws-lc、rusqlite、MCP、image；保留 ratatui 与三家渠道 SDK；若 openlark 拉回 aws-lc，+20s） |
| 改 runtime 一行 | `touch crates/komo-runtime/src/agent.rs && cargo check` | ≤ 5s |
| 改 client 一行 | `touch crates/komo-client/src/tui/app.rs && cargo check` | ≤ 5s；不得触发 gateway / runtime / store 重编 |
| 改 store 一个模型 | `touch crates/komo-store/src/models/run.rs && cargo check` | ≤ 12s（toasty 展开不可避免，但只影响 store 及以上） |
| 改 kernel 一个类型 | `cargo check` | 全量，允许 ≤ 20s；这是有意为之的代价 |
| `cargo test --workspace` | 增量 | ≤ 30s |

预算不达标时，先用 `--timings` 找关键路径，再决定拆 crate 或换依赖——不凭感觉拆。

### 13.5 trait 边界

一个 trait 只在两种情况下存在：**有第二个实现**（生产之外还有测试替身，或有多个后端），或**要做编译防火墙**（上层依赖 trait 就够，不需要看到 toasty / reqwest / axum 类型）。其余组件是具体类型，直接构造、直接测；不为"以后可能换"预留 trait。所有 trait 定义在 kernel，实现散布在 store / runtime / gateway。接缝上一律 `Arc<dyn Trait>` / `Box<dyn Trait>`，异步 trait 用 `async_trait`（对象安全，一次堆分配相对模型往返可忽略）。

| trait | 生产实现 | 测试实现 | 为什么是 trait |
|---|---|---|---|
| `Ledger` | `Coordinator`（store）：正文 → JSONL → 数据库 | `MemLedger` | Agent Loop 的唯一写入口；让 loop 和 executor 的测试不碰文件与 Turso |
| `ToolOutputStore` | 文件流式写入 + 原子发布（store） | 内存 | 子进程 stdout/stderr 流式落盘，`shell` / `python` 测试需要替身 |
| `RunQueue` | raw SQL 条件更新 + 代次（store） | 内存 | 调度器、恢复扫描、Cron、手动 resume 共用的领取入口；并发领取测试 |
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

不是 trait 的东西：`AgentLoop`、`ToolExecutor`、`Scheduler`、`Recovery`、`MemoryManager`、`SkillRegistry`、`Coordinator`、`Dispatcher`——各只有一个实现，依赖上表的 trait 就可测。`SessionRepo`、`ToolCallRepo`、`CheckpointRepo`、`OutboxRepo` 是 store 内部的具体类型，只被 `Coordinator` 用。审批也不需要 `Approver` trait：§6 定了审批暂停 Run，`Policy` 答 `Ask` 后 executor 写 `approval_requests` 并 `Ledger::suspend`，飞书卡片按钮、Telegram / WeChat 命令、TUI 弹窗、CLI 子命令都打到 `POST /v1/approvals/{id}/decision`，调度器把 Run 重新入队，executor 从 `ApprovalRepo` 消费授权再执行。渠道之间的差别只在渲染（§11.3），不在决策。

关键签名（接口示意，辅助类型省略；`Tool` 见 §4）：

```rust
/// 每个 Session 一个实例，串行。每个方法对应 §8.5 的一段箭头，
/// 方法内部完成"外置正文（如有）→ JSONL 追加并同步 → 数据库事务"的顺序。
#[async_trait]
pub trait Ledger: Send + Sync {
    /// 预留 Run（ingesting）→ run.accepted → queued。同一 request_key 返回原 Run，哈希不同则拒绝。
    async fn accept_input(&self, input: AcceptInput) -> Result<Accepted, LedgerError>;
    /// 完整 assistant 回复 + 本轮全部调用计划，一个逻辑事件；返回 Runtime 分配的 ToolCallId。
    async fn record_round(&self, run: &RunId, round: AssistantRound) -> Result<Vec<ToolCallId>, LedgerError>;
    /// tool.started + 执行尝试 + 首次授权消费，同一事务；返回后才允许产生真实副作用。
    async fn start_call(&self, call: &ToolCallId, plan: &ExecutionPlan, grant: Option<GrantUse>) -> Result<AttemptId, LedgerError>;
    /// 输出已由 ToolOutputStore 发布；这里只写 tool.result 元信息与引用。
    async fn finish_call(&self, attempt: &AttemptId, published: PublishedOutput) -> Result<(), LedgerError>;
    /// waiting_approval / waiting_retry / needs_attention，释放执行名额。
    async fn suspend(&self, run: &RunId, wait: Wait) -> Result<(), LedgerError>;
    async fn complete(&self, run: &RunId, end: RunEnd) -> Result<(), LedgerError>;
    /// 按 seq 范围读事件；引用正文按需加载并校验哈希。
    async fn read(&self, session: &SessionId, from: Seq) -> Result<EventBatch, LedgerError>;
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
    /// 条件更新 + 递增代次；返回 None 表示没有可领取的 Run。
    async fn claim(&self, executor: &ExecutorId) -> Result<Option<Claimed>, StoreError>;
}

#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &'static str;
    async fn serve(&self, inbound: Arc<dyn Inbound>, shutdown: Shutdown) -> anyhow::Result<()>;
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

## 14. 实现顺序与验收

| 阶段 | 交付 | 验证 |
|---|---|---|
| 1. 进程与会话骨架 | 六个 crate 骨架（§13.4）；komo、Gateway 自动启动、HTTP/SSE；`komo-store`：Turso 连接、`ensure_schema` + DDL 对齐测试、`with_write_retry`、统一 Session 目录与状态索引；`komo-client`：HTTP/SSE 客户端与 TUI 骨架；**编译预算首次测量** | 多个 CLI 同时启动仅产生一个实例；断线后能查看原会话；两个写入器对不同 Session 并发提交不互相阻塞，对同一行冲突时一方重试成功且只应用一次；DDL 对齐测试挂掉能定位到列；改 client 一行不重编 gateway |
| 2. AgentLoop 与 Policy | 模型往返、执行计划、Allow/Ask/Deny、审批持久化；`Ledger`、`Policy`、`ApprovedPlan` 类型状态；loop 用 `MemLedger` + 脚本化 `TurnDriver` 测 | 危险操作批准前不执行；重复批准不重复执行；Deny 不被授权覆盖；不构造 `Proof` 就调不到 `execute`（编译期）；`Ask` 后 Run 让出名额且 TUI 弹出审批 |
| 3. 五个工具 | 文件、进程、Python 环境与输出处理；`verify` 默认实现与 write/edit 的哈希核对 | 文件版本冲突可见；取消停止子进程；未知工具无法调用 |
| 4. 聊天入口（飞书 + Telegram + WeChat） | `Channel` / `Inbound` / `Notifier`、Dispatcher、pairing、`deliveries`、审批渲染、短 ID、飞书卡片与 Telegram 内联按钮、`komo channel probe` | 同一 `message_id` / `update_id` / `msg_id` 重发只产生一个 Run；`/approve` 与按钮回调重发只批准一次；来源会话与 home chat 都收到请求且第二个答复得到"已决定"；决定后卡片 / 消息原地更新；Gateway 重启后 pending 投递补发一次；飞书 ws 断线重连不丢事件也不重跑 Run；微信在用户未发消息前 `Deferred`，发消息后先收到积压的审批请求 |
| 5. 自动恢复与 resume | 持久队列、启动扫描、领取去重（`RunQueue` 条件更新 + 代次，raw SQL）、检查点、调用核对与子进程回收；恢复决策表在 kernel 里是纯函数 | 重启自动接续原 Run；已完成动作不重放；未知效果不盲目重试；两个执行者并发 `claim` 同一 Run 只有一个成功；决策表对 §8.4 每一行有一个单元测试；重启后等待中的审批仍能在手机上批 |
| 6. toolbox 迭代 + Skills | 保存模块、候选测试、版本审核与启用；`SkillRegistry`、目录行门控、`read` 只读根 | 调用使用已批准且已测试版本；模块更新使旧授权失效；新增 SKILL.md 无需重启即被 `komo skills list` 看到；`requires_tools` 不满足时不出现在提示里但仍可 inspect；toolbox 启用的审批在微信里能看到版本差异与测试结果 |
| 7. Cron | 持久调度、去重、重叠处理、人工接手 | 重启不重复创建同次触发；新危险操作等待审批；Cron 的等待在聊天里 `/approve` 后按 Cron 权限继续，不升权 |
| 8. 模型配置与 Memory | 独立记忆 / 向量模型、effort 校验、自动提取、混合召回（`memory_terms` + 向量）与遗忘 | 不串用模型配置；推断不自行确认；过期或遗忘内容不召回；重建可恢复 |
| 9. 场景与双平台验收 | 真实代码仓库、测试 HA 设备、Memos 主动记录；编译预算复测 | 两平台独立运行；记录从 Memos 找回；代码交付含验证证据；冷编与增量编译在预算内，或有记录在案的偏差原因 |

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

目录与引用验收：工具事件、大参数、完整输出及 stdout / stderr 都位于同一 Session 目录；改变 Gateway 当前工作目录不影响引用读取；不同 Session 或不同 attempt 不能互相覆盖。清理历史时保留未完成任务、Memory 证据和持久产物仍需引用的文件。

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
| Turso MVCC 模式提交是否 fsync；能否读回同步设置 | §8.2 权威边界 | 启用 §8.2 的 outbox-先-确认保护 |
| toasty 0.10 是否支持带条件的 UPDATE 并返回受影响行数 | §8.7 领取代次 | 直接用 raw SQL（已计入设计） |
| `reqwest` `rustls-no-provider` 下没有其他依赖重新启用 `aws-lc-rs`；`openlark` 的 tokio-rustls 是否以默认特性把它拉回来（特性可加不可减） | §13.4 冷编预算 | 接受 aws-lc，冷编预算 +20s；或 `[patch]` 一份 openlark-client 改成 `default-features = false` |
| `instr` 全表关键词臂在 1 万条时的 P95 | §9.4 | 加 `memory_terms` 的按 token 倒排表（仍是普通表，仍可重建） |
| Turso 长读事务（SSE 游标补读）与并发写提交的快照语义 | §11.1 回复订阅 | 读路径改为短事务分页 |
| `toasty-driver-turso` 能否通过 `[patch]` 关闭 `turso` 的 `fts` 默认特性且链接正常 | §13.4 可选项 | 放弃该项，冷编多 ~14s |
| iLink 消息是否带稳定 `msg_id` 可作 `request_key` | §11.1 微信去重 | 用 `(from, 内容哈希, 60s 窗口)` 近似去重，并标明微信侧可能重复 |
| `wechatbot` 的 native-tls 在 Fedora 上链系统 openssl 是否顺利；与 `rustls-no-provider` 的 reqwest 0.13 共存 | §13.4 | 退回 vendored openssl，Linux 冷编多 1–2 分钟 |
| `lark-websocket-protobuf` 是否需要 `protoc`（本地缓存的 0.1.2 没有 build.rs，看起来是预生成代码） | 构建工具链要求 | 安装说明里加 `protobuf` |
| 飞书卡片回调的 `event_id`、Telegram `callback_query_id` 在重推时是否保持不变 | §11.3 按钮去重 | 改用 `(message_id, action.value)` 组合键 |
