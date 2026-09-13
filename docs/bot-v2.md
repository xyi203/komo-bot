# Komo：Rust 个人 Agent 框架设计

版本：v0.4 · 日期：2026-09-13 · 状态：设计稿，尚未实现。

本文定义一个全新项目。Komo 是程序名称；架构只依据本文的需求与约束。

## 1. 已确定的范围

**一个 Rust 程序、五个基础工具、一套可恢复的 Agent Runtime。**

| 项目     | 决策                                                                         |
| -------- | ---------------------------------------------------------------------------- |
| 部署     | Fedora Server 与 macOS 均可独立运行，每个实例保存自己的数据                  |
| 进程入口 | 单个可执行程序 `komo`，包含聊天客户端和 Gateway                              |
| 聊天     | `komo` 直接进入聊天；Gateway 未运行时自动启动                                |
| 执行方式 | LLM 原生 tool call，由 Rust 校验、审核和执行                                 |
| 基础工具 | 严格保留 `read`、`write`、`edit`、`shell`、`python`                          |
| 能力扩展 | Python 模块保存到 toolbox，AI 可以编写、测试和迭代                           |
| 定时任务 | Cron 触发同一套 Agent Runtime                                                |
| 持久化   | 通用 Session、Run、调用记录、检查点和审批状态，支持 resume                   |
| 操作审核 | 统一 Policy；需要人工审核时通过 CLI 处理                                     |
| 自动记忆 | 自动积累偏好、事实和经验，区分用户陈述、工具观察、模型推断与用户确认         |
| 主动记录 | 用户明确要求保存的内容写入现有 Memos，原文以 Memos 为准                      |
| 记忆检索 | 关键词与向量混合检索，索引保存在本机，可重建                                 |
| 模型配置 | 主模型、记忆整理模型、记忆向量模型分别配置；统一提供 effort 并按模型能力校验 |
| 后续入口 | Telegram 复用 Gateway 的会话和运行接口                                       |

代码优化、HA、网页搜索、记事是工具组合的使用场景，不成为专用的 Rust 工具或 Runtime 业务类型。

首版保持单操作者、单实例运行方式，不引入实例同步、跨机器调度、外部消息队列或独立向量数据库服务。支持向量检索不要求部署额外服务。

## 2. 总体结构

```text
komo 聊天 / resume / cron 管理
              │
         HTTP / SSE
              │
        Gateway 常驻进程
              │
     ┌────────┼───────────┐
     │        │           │
 Session   Agent Runtime  Cron Scheduler
  Store       │           │
     │        └── 同一个 Run 提交入口
     │
     └── 上下文 / 事件 / 检查点 / 审批
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

Gateway 持有模型连接、数据库、工具环境和运行状态。CLI 负责提交输入、显示进度、回答审批和恢复会话。

每个实例的 Gateway 自己执行任务。CLI 从 Mac 连接 NAS 的 Gateway 时，命令与 Python 都在 NAS 虚拟机中执行。

## 3. 命令与 Gateway 生命周期

以下是拟定命令，不代表已经安装或实现。

| 命令                                         | 行为                                                                |
| -------------------------------------------- | ------------------------------------------------------------------- |
| `komo`                                       | 确保本机 Gateway 就绪，创建新 Session，进入聊天                     |
| `komo resume SESSION_ID`                     | 恢复指定会话；有待处理运行时展示并接续                              |
| `komo session list`                          | 查看会话列表和状态                                                  |
| `komo run inspect RUN_ID`                    | 查看执行过程、工具结果与产物                                        |
| `komo run cancel RUN_ID`                     | 请求取消运行                                                        |
| `komo gateway`                               | 启动后台 Gateway，等待就绪后返回                                    |
| `komo gateway --foreground`                  | 前台运行 Gateway，供服务管理器及调试使用                            |
| `komo gateway status/stop/restart`           | 管理后台进程                                                        |
| `komo approval list/show/approve/reject`     | 查看和处理待审核操作                                                |
| `komo cron add/list/run/pause/resume/remove` | 管理定时任务                                                        |
| `komo memory list/search/show`               | 查看自动记忆、来源与确认状态；search 支持 hybrid / keyword / vector |
| `komo memory confirm/forget`                 | 确认具体版本或停用自动记忆；不删除 Memos 原文                       |
| `komo memory index status/rebuild`           | 查看索引覆盖率或重建当前向量索引                                    |
| `komo config check`                          | 校验各模型、effort、向量参数及其他配置                              |

聊天启动顺序：

1. 读取当前实例的连接配置和发现文件。
2. 检查 Gateway 健康状态与实例身份，不能只凭 PID 或端口判断。
3. 本机实例未运行时，请求系统服务管理器启动。
4. 等待服务就绪，超时则返回具体诊断信息。
5. 建立聊天连接并订阅会话事件。

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
}
```

这是接口示意，辅助类型省略。`ApprovedPlan` 只能由执行器在策略允许或有效审批后构造，字段不对外开放。prepare 可以解析参数和检查元信息，但不能通过导入未知 Python 模块、运行命令等方式提前执行未审核代码。

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

## 6. Agent Loop

```text
持久化用户输入或 Cron 触发
  → 创建 Run
  → 从 Session 读取上下文
  → 固定本次 Run 的模型配置快照
  → MemoryManager 按当前任务召回有效记忆
  → 请求 LLM
  → 接收完整一轮回复
      ├─ 有 tool calls
      │    → 保存完整 assistant 回复和调用计划
      │    → 逐个 prepare → policy → execute
      │    → 保存每个结果
      │    → 按 call_id 回传结果
      │    → 下一轮 LLM
      └─ 正常结束且无未完成调用
           → 保存最终回复
           → Run 完成
           → 标记 Memory 整理任务待处理
           → 后台提取、校验、持久化并更新索引
```

工具名称和参数来自 provider 原生字段，不从自然语言或代码块推断。完整 assistant 消息与每个工具结果保持调用 ID 配对，这是原生 function calling 的执行方式。[Function calling](https://developers.openai.com/api/docs/guides/function-calling)

首版顺序执行同一轮的多个调用，减少文件操作顺序歧义。不同 Session 可并发，同一 Session 的 Run 顺序执行；等待审批的 Run 保留会话顺序位置，不允许后续 Run 越过它执行。等待运行排到前面时才装配上下文，避免读到过期历史。

Gateway 设置总轮数、活动执行时限、输出长度和子进程并发预算。等待用户时释放运行名额，保留已消耗预算。

普通工具失败作为结果交给模型修正；模型回复截断或调用参数未收齐时不能开始执行。远端写入结果不明时停止自动重试，转为结果核对。

普通追问可以作为 assistant 回复结束本轮；用户下一条输入开启同一 Session 的新 Run。工具审批则暂停原 Run，待决策后继续原调用，不增加第六个交互工具。

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

审批绑定一份不可变的执行计划，包括 operation_id、来源、关联 Run / ToolCall（如有）、规范化参数、工作目录、目标版本、脚本或模块版本、环境版本及相关资源引用。界面显示具体动作、原因、改动和已有验证结果。

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
 → 审批请求与 waiting_approval 状态事务提交
 → 释放执行名额
 → CLI 展示
 → 用户批准或拒绝
 → 原子记录决策
 → 继续前重新校验并消费授权
 → 执行或返回拒绝结果
```

审批记录保存在数据库。重复回答幂等，已取消或已完成调用不能再次执行。授权消费和调用 started 状态在同一事务内提交；此后进程崩溃可能产生结果未知，不能凭批准记录推断操作已经完成。

拒绝作为明确结果交回模型。后续换一个基础工具仍要检查同一权限目标，不能自动绕过禁止规则。

Cron、交互聊天与 resume 都经过这一条路径。

## 8. 通用 Session 存储

### 8.1 三个核心对象

| 对象     | 含义                             |
| -------- | -------------------------------- |
| Session  | 连续对话、工作目录和上下文的载体 |
| Run      | 一次用户输入或一次触发引起的执行 |
| ToolCall | 一次有独立执行状态的具体调用     |

每个调用有 Runtime 分配的内部 ID，同时保留 provider call_id 和模型轮次；重复的 provider ID 不能误命中其他轮次。

### 8.2 数据结构

| 表                       | 主要内容                                                                   |
| ------------------------ | -------------------------------------------------------------------------- |
| sessions                 | 标题、来源、工作目录、当前 Run、最新事件序号                               |
| runs                     | 输入、状态、预算、执行位置、结果、模型配置快照、memory_work 状态与处理游标 |
| session_events           | 按 Session 递增的 seq、Run、事件类型、格式版本、内容或产物引用             |
| tool_calls               | 调用 ID、参数、执行计划、代码版本、状态、结果                              |
| checkpoints              | 已覆盖的事件序号、格式版本、上下文引用、记忆 ID / revision、执行游标       |
| approval_requests        | 具体待审核动作、计划哈希、回答与消费状态                                   |
| policy_grants            | 有范围、来源、版本及有效条件的授权                                         |
| cron_jobs                | 定时任务定义与版本                                                         |
| cron_firings             | 唯一触发记录及关联 Session / Run                                           |
| memory_items             | 自动记忆内容、作用域、确认状态、生命周期、revision 与时间信息              |
| memory_evidence          | 原始事件或外部记录引用、来源版本 / 摘要哈希、提取与确认依据                |
| memory_fts               | 记忆文本的可重建关键词索引                                                 |
| memory_vectors           | 记忆版本、内容哈希、索引代次与向量                                         |
| memory_index_generations | 向量空间指纹、构建状态、进度与当前生效代次                                 |

SQLite 是状态与事件的持久化入口。运行状态、调用状态与对应事件在一个事务内提交；不在事务中等待 LLM、用户或子进程。

### 8.3 减少重复存储

- 追加新的消息和执行事件，不在每一轮重写整个会话。
- 模型请求保存上下文引用、记忆 ID / revision、实际解析的模型 / effort 和工具版本，不重复存完整历史。
- 收齐模型回复或工具结果后批量提交，不逐 token 写数据库。
- 长输出保存在 artifacts，数据库保存完整内容引用和有界预览。
- 先原子写入并确认产物文件，再提交数据库引用；失败遗留文件可按未引用对象清理。
- 在完整模型轮次或工具完成处更新检查点；恢复读取检查点与其后的事件。
- 检查点是可重建缓存，格式失效或不匹配时重新读取事件。
- 上下文按完整调用与结果单元裁剪；摘要保留覆盖范围与原文引用。

### 8.4 执行状态

```text
Run:
queued → running → completed / failed / cancelled
             ↘ waiting_approval → running
             ↘ interrupted

ToolCall:
planned → started → completed / failed
                    ↘ uncertain
```

完整模型回复和该轮所有 planned 调用必须先持久化。每次调用先保存 started，再执行真实操作，最后保存结果。

Gateway 重启后保留 queued 与 waiting_approval；此前 running 的 Run 标记 interrupted，先检查调用记录。

| 调用状态                        | resume 行为                              |
| ------------------------------- | ---------------------------------------- |
| completed / failed 且结果已保存 | 使用原结果，不重放已执行动作             |
| planned，确定尚未执行           | 重新检查 Policy 后可以执行               |
| started，但没有结果             | 标记 uncertain，核对真实状态后决定下一步 |
| waiting_approval                | 展示原请求；批准后校验并继续             |
| Run 已完成                      | 加载历史，等待下一条用户输入             |

取消或确认中断后，不会执行的调用形成明确的取消结果；started 而结果未知的调用形成未知状态说明。再次请求模型前必须补齐该轮调用与结果配对，不能伪造成功。

resume 从已记录的执行位置继续；不保证任意外部副作用 exactly-once。Python 首版只恢复到工具调用之间，不恢复到脚本中间某一行。需要分阶段恢复的长脚本应自己持久化进度，并在不确定时核对外部结果。

## 9. Memory：自动积累、可信来源与混合检索

### 9.1 两类内容，分别保存

| 内容                               | 保存位置                                      | 使用方式                                                  |
| ---------------------------------- | --------------------------------------------- | --------------------------------------------------------- |
| 用户主动要求保存的笔记、想法和记录 | 现有 Memos                                    | 通过已审核的 toolbox.memos 保存、搜索和读取，返回原文链接 |
| 自动积累的偏好、项目事实和执行经验 | 本机 SQLite 的 memory_items / memory_evidence | MemoryManager 提取、校验、召回、更新和遗忘                |
| 本次任务的工作状态                 | Session / Run / ToolCall                      | 用于上下文与 resume，不自动等同于长期事实                 |
| 关键词与向量索引                   | 同一 SQLite 数据库                            | 从有效记忆重建，不能取代原文与来源                        |

长期记忆和当前激活的上下文分别管理：Context Window 是模型请求的容量限制；Working Memory 是本次任务实际选入的历史、状态、相关记忆和工具结果。不能把整个记忆库塞进每轮请求。

首版向量检索覆盖自动 Memory，以及其中已经保存的 Memos 来源摘要。它不表示已索引全部 Memos 内容；查找主动笔记仍查询 Memos。Memos 全量语义索引需要另行明确导入范围，当前不增加同步器。

### 9.2 记忆的数据模型

确认状态与来源分开保存，避免把“模型从用户原话整理”误写为“用户确认了模型摘要”。

| 字段                      | 含义                                                                    |
| ------------------------- | ----------------------------------------------------------------------- |
| id / revision             | 稳定 ID 与递增内容版本；修改产生新版本                                  |
| content / kind            | 简短、尽量单一的陈述；preference / fact / experience                    |
| scope                     | personal、project 或 environment，绑定当前操作者、稳定项目 ID 或实例 ID |
| provenance                | user_statement / tool_observation / model_inference                     |
| confirmation              | unconfirmed / user_confirmed                                            |
| state                     | candidate / active / contested / superseded / forgotten                 |
| evidence_refs             | 原始 Session 事件、工具产物或 Memos 实例 / 记录 ID、时间与来源版本      |
| observed_at / valid_until | 事实的观察时间与可选有效期，区别于入库时间                              |
| created_at / updated_at   | 记忆管理时间                                                            |
| extraction_metadata       | 提取模型、effort、提示词版本、处理来源游标                              |
| usage                     | 使用次数与最近使用时间，只度量使用情况，不增加真实性                    |

证据与内容版本分别保留。用户确认绑定具体 id / revision 和操作者事件；模型返回的 user_confirmed 字段没有写入权限。明确的用户原话可标为 active + user_statement + unconfirmed，展示为“自动整理自用户陈述”；模型推断默认 candidate，不作为已确认事实注入。

用户主动保存到 Memos 表示“要求保留这段内容”，其中可能有引用、假设或待办，不等于确认了其中每一句话。只有用户明确确认的具体陈述才能获得 user_confirmed。

NAS 和 Mac 的自动 Memory 各自独立。项目作用域使用实例内稳定的项目 ID，可映射多个临时仓库副本；工作路径相似不自动意味着同一项目。

### 9.3 自动积累与更新

MemoryManager 对 AgentRuntime 提供三个主要入口：recall、process_completed_run、apply_user_decision。提取、去重、证据校验、冲突处理与索引协调隐藏在该模块内部；它不向 LLM 注册额外工具。

处理流程：

```text
Run 正常完成，与结果一起提交 memory_work = pending
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
  ├─ 关键词召回：FTS5 + 标签 / 精确值 / 短字符串匹配
  └─ 向量召回：独立 embedding 模型 → 当前向量空间 → 余弦相似度
       → 按排名融合（RRF），避免直接相加不同量纲的分数
       → 去重，核对当前 revision、来源与冲突状态
       → 按条数和 token 预算选入上下文
       → 返回内容、来源、确认状态、时间与版本
```

默认 hybrid；同时支持 keyword 和 vector 供明确选择与诊断。每路建议最多召回 40 项，合并后最多注入 8 项、约 1500 tokens；这些是可调整的初始预算，不强行填满。没有足够相关内容时返回空；相似度只用于相关性，不授予真实性或操作权限。

关键词索引使用 FTS5；采用 trigram 时，小于三个 Unicode 字符的查询需要有界子串匹配后备，不能让“空调”等两字查询无结果。[SQLite FTS5](https://www.sqlite.org/fts5.html)

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
 → 事务插入 cron_firing
 → 创建独立 Session / Run
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
- 同一 Job 上一次仍在执行或等待审批时，跳过本次并记录原因。
- Gateway 停机期间错过的触发不集中补跑。
- 时区明确保存；同一日历时间因夏令时出现两次时，两次不同 UTC 时刻分别视为计划时间；不存在的本地时间跳过。
- 手动 run 使用独立请求幂等键，不冒充定时触发。
- 新增危险操作暂停等待审批，不能因无人值守而自动放行。
- Job、模块或权限发生变化时重新匹配授权。
- Cron 的结果去原 Session 查看，也可以用 resume 接手。

调度与执行状态提交后再通知内存队列；启动与周期扫描负责补齐通知丢失的情况。

## 11. 数据目录与记录习惯

```text
~/.komo/
├── config.toml
├── policy.toml
├── state.db
├── runtime/             # 发现文件、锁等
├── toolbox/             # 保存的 Python 能力及说明
├── python-envs/         # 受管理的环境版本
├── workspaces/          # 项目副本和普通工作文件
├── artifacts/           # 工具输出、代码快照、报告
└── logs/
```

自动 Memory 保存在 state.db，用户主动记录保存在 Memos。artifacts 保存运行产物，toolbox 保存可复用 Python 能力；清理会话不能顺带删除这些持久内容。会话来源需要清理时，先为仍引用它的记忆保留必要的、经脱敏的证据摘录与来源元信息，或停用无法再验证的条目，不能留下伪装有效的引用。

记忆正文、来源和确认记录需要持久保留；关键词与向量索引可从原文重建，禁止为了重建索引删除整个数据库。Memos 原文由 Memos 自己的备份策略覆盖。

数据目录可通过配置或 KOMO_HOME 改变。相对路径固定按配置文件所在目录解析。每个实例独占自己的数据库；SQLite WAL 数据库存放在运行机器的本地磁盘，不能作为多机器共享数据库。[SQLite WAL](https://www.sqlite.org/wal.html)

备份使用一致数据库快照并包含被引用的持久文件；不要仅复制运行中的 state.db 而忽略 WAL。

## 12. 通信、技术栈与项目结构

### 12.1 通信

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
| GET /v1/runs/{id}                | 执行详情                                       |
| POST /v1/runs/{id}/cancel        | 取消                                           |
| GET /v1/approvals                | 待审核列表，列表项包含具体动作与计划           |
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

除最小健康检查外统一认证。提交输入、审批、Cron 与 Memory 变更都支持幂等请求键；同一键对应不同内容则拒绝。

SSE 事件带 Session 内递增序号，断线后按游标补读。数据库事件是补读来源，内存通知仅提示有新数据。CLI 退出或连接中断不取消后台运行，取消通过明确操作发起。

### 12.2 技术选择

| 用途                  | 选择                                                     |
| --------------------- | -------------------------------------------------------- |
| 异步与进程 IO         | Tokio、tokio-util                                        |
| HTTP / SSE            | Axum                                                     |
| HTTP 客户端与模型调用 | Reqwest                                                  |
| CLI                   | Clap                                                     |
| 状态数据库            | SQLite + SQLx                                            |
| 关键词检索            | SQLite FTS5；中文短查询补充有界子串匹配                  |
| 向量存储与检索        | SQLite 保存向量，Rust 在过滤后执行精确余弦检索           |
| 向量生成              | 独立 EmbeddingClient，使用 memory.embedding 配置         |
| 配置与结构化数据      | TOML、Serde                                              |
| Schema 与异步工具接口 | Schemars、async-trait                                    |
| 日志                  | tracing                                                  |
| Python                | 独立解释器进程和虚拟环境                                 |
| Cron                  | 支持指定五字段语义及时区的 Rust 调度库，落地时做语义验证 |

Axum 已提供 SSE 响应；SQLx 提供 SQLite 配置和事务能力。实现时锁定实际依赖版本并验证 Fedora/macOS 构建。[Axum SSE](https://docs.rs/axum/latest/axum/response/sse/index.html) · [SQLx SQLite](https://docs.rs/sqlx/latest/sqlx/sqlite/struct.SqliteConnectOptions.html)

首版生成模型与向量模型分别接入一种明确的协议，由具体适配器实现。base_url、model、effort 与凭证引用可配置；相同协议可以连接不同端点。不同协议后续按需要增加，不把“OpenAI compatible”当成所有字段都兼容的保证。生成适配器必须保留协议回放所需的消息块和元数据。

### 12.3 模型角色与统一 effort 配置

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

### 12.4 Rust 目录

```text
komo/
├── Cargo.toml
├── migrations/
├── src/
│   ├── main.rs
│   ├── cli.rs
│   ├── client.rs
│   ├── gateway.rs
│   ├── protocol.rs
│   ├── agent.rs
│   ├── llm.rs
│   ├── embedding.rs
│   ├── memory.rs
│   ├── memory_index.rs
│   ├── session.rs
│   ├── store.rs
│   ├── policy.rs
│   ├── approval.rs
│   ├── cron.rs
│   ├── config.rs
│   ├── python_runtime.rs
│   └── tools/
│       ├── mod.rs
│       ├── read.rs
│       ├── write.rs
│       ├── edit.rs
│       ├── shell.rs
│       └── python.rs
└── tests/
```

一个 Cargo package、一个二进制，先按模块组织。没有明确需要独立编译或发布的部分，暂不拆成大量 crate。

## 13. 实现顺序与验收

| 阶段                   | 交付                                                       | 验证                                                             |
| ---------------------- | ---------------------------------------------------------- | ---------------------------------------------------------------- |
| 1. 进程与会话骨架      | komo、Gateway 自动启动、HTTP/SSE、Session/Run 存储         | 多个 CLI 同时启动仅产生一个实例；断线后能查看原会话              |
| 2. AgentLoop 与 Policy | 模型往返、执行计划、Allow/Ask/Deny、审批持久化             | 危险操作批准前不执行；重复批准不重复执行；Deny 不被授权覆盖      |
| 3. 五个工具            | 文件、进程、Python 环境与输出处理                          | 文件版本冲突可见；取消停止子进程；未知工具无法调用               |
| 4. resume              | 检查点、事件尾部恢复、调用状态核对                         | 已完成动作不重放；started 无结果明确标为未知                     |
| 5. toolbox 迭代        | 保存模块、候选测试、版本审核与启用                         | 调用使用已批准且已测试版本；模块更新使旧授权失效                 |
| 6. Cron                | 持久调度、去重、重叠处理、人工接手                         | 重启不重复创建同次触发；新危险操作等待审批                       |
| 7. 模型配置与 Memory   | 独立记忆 / 向量模型、effort 校验、自动提取、混合召回与遗忘 | 不串用模型配置；推断不自行确认；过期或遗忘内容不召回；重建可恢复 |
| 8. 场景与双平台验收    | 真实代码仓库、测试 HA 设备、Memos 主动记录                 | 两平台独立运行；记录从 Memos 找回；代码交付含验证证据            |

存储和 Policy 的基本约束从第二阶段开始贯穿全部工具，不能在所有功能完成后才补审核。

首个端到端验收：启动 komo 自动拉起 Gateway，通过模型调用 write 生成工作文件；关闭客户端并重启 Gateway，resume 原会话，再读出原文件。随后验证一次待审批 shell 在重启后仍等待，批准后只执行一次已确认的调用；若模拟执行结果丢失，则进入未知状态核对而不是自动重跑。

Memory 与模型验收覆盖：

- 同一偏好换一种中文表述仍可语义召回，同时保留原始来源；中文两字查询能走关键词后备。
- 用户原话、工具观察、模型推断、用户确认在展示和上下文中均可区分；候选不因反复提取而升级。
- 切换聊天模型或 effort 不影响独立配置的记忆模型；记忆提取、冲突整理均使用自己的模型与 effort。
- 未设置 effort 不发送该字段；不支持的 effort 在请求前拒绝；模型 400 不触发静默删参重试。
- 同维度但不同模型 / 版本生成的向量不混用；修改记忆时旧 revision 不召回；重建中崩溃可以接续。
- 向量服务不可用时 hybrid 明示降级，vector-only 明确报不可用；不能把故障解释为“没有相关记忆”。
- confirm / forget 使用预期 revision，重复请求幂等；resume 和旧检查点不能重新注入已遗忘内容。
- 主动记录写入 Memos 后返回 ID / 链接，查询回到原文；删除或修改原文后更新关联摘要，写入未知时先核对。
- 在目标 NAS 与 Mac 上测量至少 1 千与 1 万条代表性记忆的索引时间、检索 P95 和内存，再决定是否需要近似向量索引；不预先宣称性能达标。
