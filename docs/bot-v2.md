# Komo：Rust 个人 Agent 框架设计

版本：v0.2 · 日期：2026-09-13 · 状态：设计稿，尚未实现。

本文定义一个全新项目。Komo 是程序名称；架构只依据本文的需求与约束。

## 1. 已确定的范围

**一个 Rust 程序、五个基础工具、一套可恢复的 Agent Runtime。**

| 项目 | 决策 |
|---|---|
| 部署 | Fedora Server 与 macOS 均可独立运行，每个实例保存自己的数据 |
| 进程入口 | 单个可执行程序 `komo`，包含聊天客户端和 Gateway |
| 聊天 | `komo` 直接进入聊天；Gateway 未运行时自动启动 |
| 执行方式 | LLM 原生 tool call，由 Rust 校验、审核和执行 |
| 基础工具 | 严格保留 `read`、`write`、`edit`、`shell`、`python` |
| 能力扩展 | Python 模块保存到 toolbox，AI 可以编写、测试和迭代 |
| 定时任务 | Cron 触发同一套 Agent Runtime |
| 持久化 | 通用 Session、Run、调用记录、检查点和审批状态，支持 resume |
| 操作审核 | 统一 Policy；需要人工审核时通过 CLI 处理 |
| 个人记录 | 持久化 Markdown 等普通文件，通过基础工具保存和查询 |
| 后续入口 | Telegram 复用 Gateway 的会话和运行接口 |

代码优化、HA、网页搜索、记事是工具组合的使用场景，不成为专用的 Rust 工具或 Runtime 业务类型。

首版保持单操作者、单实例运行方式，不引入实例同步、跨机器调度、外部消息队列或向量数据库。

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

Gateway 持有模型连接、数据库、工具环境和运行状态。CLI 负责提交输入、显示进度、回答审批和恢复会话。

每个实例的 Gateway 自己执行任务。CLI 从 Mac 连接 NAS 的 Gateway 时，命令与 Python 都在 NAS 虚拟机中执行。

## 3. 命令与 Gateway 生命周期

以下是拟定命令，不代表已经安装或实现。

| 命令 | 行为 |
|---|---|
| `komo` | 确保本机 Gateway 就绪，创建新 Session，进入聊天 |
| `komo resume SESSION_ID` | 恢复指定会话；有待处理运行时展示并接续 |
| `komo session list` | 查看会话列表和状态 |
| `komo run inspect RUN_ID` | 查看执行过程、工具结果与产物 |
| `komo run cancel RUN_ID` | 请求取消运行 |
| `komo gateway` | 启动后台 Gateway，等待就绪后返回 |
| `komo gateway --foreground` | 前台运行 Gateway，供服务管理器及调试使用 |
| `komo gateway status/stop/restart` | 管理后台进程 |
| `komo approval list/show/approve/reject` | 查看和处理待审核操作 |
| `komo cron add/list/run/pause/resume/remove` | 管理定时任务 |

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

| 工具 | 输入与输出重点 | 执行约束 |
|---|---|---|
| `read` | 路径、读取范围；返回文本和文件版本 | 大文件截断，明确显示未读范围 |
| `write` | 路径、完整内容、可选预期版本 | 原子替换；覆盖现有文件需检查版本 |
| `edit` | 路径、明确匹配内容、替换内容、预期版本 | 匹配失败返回错误，不模糊猜测 |
| `shell` | 命令、工作目录、超时；返回退出码和输出 | 管理进程组、限制输出、支持取消 |
| `python` | 代码或已保存模块调用；返回结果、输出和产物 | 使用受管理解释器，绑定代码版本 |

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
├── tests/
└── .staging/
```

README 与模块说明提供用法。LLM 通过 read 查看说明，再通过 python 调用。保存工具不会扩张模型侧的五个工具 Schema。

HA 地址、搜索服务地址和凭证引用通过配置传给已授权模块。原始凭证不放进模型提示词，不打印到对话或日志。

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

## 6. Agent Loop

```text
持久化用户输入或 Cron 触发
  → 创建 Run
  → 从 Session 读取上下文
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
```

工具名称和参数来自 provider 原生字段，不从自然语言或代码块推断。完整 assistant 消息与每个工具结果保持调用 ID 配对，这是原生 function calling 的执行方式。[Function calling](https://developers.openai.com/api/docs/guides/function-calling)

首版顺序执行同一轮的多个调用，减少文件操作顺序歧义。不同 Session 可并发，同一 Session 的 Run 顺序执行。等待运行排到前面时才装配上下文，避免读到过期历史。

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

Policy 检查准备好的 ExecutionPlan：工具、代码或命令、参数、真实目标路径、工作目录、版本、凭证引用与已有授权。LLM 提供的“风险低”或 Python 模块自称“安全”均不构成授权。

优先级：

```text
执行环境不可突破的限制
  > 明确 Deny
  > 有效的范围授权
  > 配置 Allow
  > 默认 Ask
```

| 操作 | 初始策略建议 |
|---|---|
| 已授权范围内读取普通文件 | Allow |
| 已授权 workspace / records 范围内写入和修改 | Allow，覆盖时检查版本 |
| 访问范围外文件或敏感内容 | Ask；命中显式禁用规则则 Deny |
| 修改启用中的 toolbox 或 Python 环境 | Ask，展示具体差异 |
| 任意 shell / Python code | Ask；有匹配的明确执行授权时允许 |
| Python call | 按已审核版本、导出函数、参数与授权范围判断 |
| 权限扩大或修改 Policy | 通过操作者配置流程处理，不能由模型自行放宽 |

### 7.2 审批对象与范围

审批绑定一份不可变的执行计划，包括 Run、ToolCall、规范化参数、工作目录、目标版本、脚本或模块版本、环境版本及相关资源引用。界面显示具体动作、原因、改动和已有验证结果。

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

| 对象 | 含义 |
|---|---|
| Session | 连续对话、工作目录和上下文的载体 |
| Run | 一次用户输入或一次触发引起的执行 |
| ToolCall | 一次有独立执行状态的具体调用 |

每个调用有 Runtime 分配的内部 ID，同时保留 provider call_id 和模型轮次；重复的 provider ID 不能误命中其他轮次。

### 8.2 数据结构

| 表 | 主要内容 |
|---|---|
| sessions | 标题、来源、工作目录、当前 Run、最新事件序号 |
| runs | 输入、状态、预算、执行位置、结果 |
| session_events | 按 Session 递增的 seq、Run、事件类型、格式版本、内容或产物引用 |
| tool_calls | 调用 ID、参数、执行计划、代码版本、状态、结果 |
| checkpoints | 已覆盖的事件序号、格式版本、上下文引用、执行游标 |
| approval_requests | 具体待审核动作、计划哈希、回答与消费状态 |
| policy_grants | 有范围、来源、版本及有效条件的授权 |
| cron_jobs | 定时任务定义与版本 |
| cron_firings | 唯一触发记录及关联 Session / Run |

SQLite 是状态与事件的持久化入口。运行状态、调用状态与对应事件在一个事务内提交；不在事务中等待 LLM、用户或子进程。

### 8.3 减少重复存储

- 追加新的消息和执行事件，不在每一轮重写整个会话。
- 模型请求保存上下文引用、配置和工具版本，不重复存完整历史。
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

| 调用状态 | resume 行为 |
|---|---|
| completed / failed 且结果已保存 | 使用原结果，不重放已执行动作 |
| planned，确定尚未执行 | 重新检查 Policy 后可以执行 |
| started，但没有结果 | 标记 uncertain，核对真实状态后决定下一步 |
| waiting_approval | 展示原请求；批准后校验并继续 |
| Run 已完成 | 加载历史，等待下一条用户输入 |

取消或确认中断后，不会执行的调用形成明确的取消结果；started 而结果未知的调用形成未知状态说明。再次请求模型前必须补齐该轮调用与结果配对，不能伪造成功。

resume 从已记录的执行位置继续；不保证任意外部副作用 exactly-once。Python 首版只恢复到工具调用之间，不恢复到脚本中间某一行。需要分阶段恢复的长脚本应自己持久化进度，并在不确定时核对外部结果。

## 9. Cron

Cron Scheduler 只负责产生 Run，复用 AgentLoop、Policy、工具和存储。

```text
发现到期
 → 事务插入 cron_firing
 → 创建独立 Session / Run
 → 普通队列执行
 → 保存结果和产物
 → 更新本次触发状态
```

每个 Job 包含名称、五字段 cron 表达式、时区、prompt、工作目录、enabled、执行预算、重叠策略及版本化授权。

```bash
komo cron add --name morning-summary \
  --schedule "0 9 * * *" \
  --timezone Asia/Shanghai \
  --prompt "搜索今天关注的技术动态，整理后保存到 records"

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

## 10. 数据目录与记录习惯

```text
~/.komo/
├── config.toml
├── policy.toml
├── state.db
├── runtime/             # 发现文件、锁等
├── toolbox/             # 保存的 Python 能力及说明
├── python-envs/         # 受管理的环境版本
├── workspaces/          # 项目副本和普通工作文件
├── records/             # 长期保留的个人记录
├── artifacts/           # 工具输出、代码快照、报告
└── logs/
```

records 是约定的持久目录，不需要 Notes 表或专用接口。模型用 write 保存、用 shell / Python 搜索、用 read 读取原文。记录应包含时间与必要来源；查询结果引用实际文件和内容。清理会话不删除 records 或 toolbox。

数据目录可通过配置或 KOMO_HOME 改变。相对路径固定按配置文件所在目录解析。每个实例独占自己的数据库；SQLite WAL 数据库存放在运行机器的本地磁盘，不能作为多机器共享数据库。[SQLite WAL](https://www.sqlite.org/wal.html)

备份使用一致数据库快照并包含被引用的持久文件；不要仅复制运行中的 state.db 而忽略 WAL。

## 11. 通信、技术栈与项目结构

### 11.1 通信

CLI 通过 HTTP 发命令，通过 SSE 观察运行。Gateway 内部采用函数调用，无需在本机模块之间再发 HTTP。

最小接口：

| 接口 | 行为 |
|---|---|
| GET /healthz | 最小健康检查与实例标识 |
| POST /v1/sessions | 创建会话 |
| GET /v1/sessions | 列出会话 |
| GET /v1/sessions/{id} | 会话详情与运行状态 |
| GET /v1/sessions/{id}/events | 按游标获取或订阅事件 |
| POST /v1/sessions/{id}/runs | 提交新输入 |
| POST /v1/sessions/{id}/resume | 检查恢复位置，恢复可继续的运行或返回待处理状态 |
| GET /v1/runs/{id} | 执行详情 |
| POST /v1/runs/{id}/cancel | 取消 |
| GET /v1/approvals | 待审核列表，列表项包含具体动作与计划 |
| GET /v1/approvals/{id} | 单项审批详情 |
| POST /v1/approvals/{id}/decision | 批准或拒绝 |
| GET /v1/cron | 列出定时任务 |
| POST /v1/cron | 创建定时任务 |
| POST /v1/cron/{id}/run | 手动触发 |
| PATCH /v1/cron/{id} | 更新定义或启停 |
| DELETE /v1/cron/{id} | 移除后续调度，已有执行历史保留 |

除最小健康检查外统一认证。提交输入、审批和 Cron 变更都支持幂等请求键；同一键对应不同内容则拒绝。

SSE 事件带 Session 内递增序号，断线后按游标补读。数据库事件是补读来源，内存通知仅提示有新数据。CLI 退出或连接中断不取消后台运行，取消通过明确操作发起。

### 11.2 技术选择

| 用途 | 选择 |
|---|---|
| 异步与进程 IO | Tokio、tokio-util |
| HTTP / SSE | Axum |
| HTTP 客户端与模型调用 | Reqwest |
| CLI | Clap |
| 状态数据库 | SQLite + SQLx |
| 配置与结构化数据 | TOML、Serde |
| Schema 与异步工具接口 | Schemars、async-trait |
| 日志 | tracing |
| Python | 独立解释器进程和虚拟环境 |
| Cron | 支持指定五字段语义及时区的 Rust 调度库，落地时做语义验证 |

Axum 已提供 SSE 响应；SQLx 提供 SQLite 配置和事务能力。实现时锁定实际依赖版本并验证 Fedora/macOS 构建。[Axum SSE](https://docs.rs/axum/latest/axum/response/sse/index.html) · [SQLx SQLite](https://docs.rs/sqlx/latest/sqlx/sqlite/struct.SqliteConnectOptions.html)

首版接入一种明确的 LLM 协议，base_url、model 与凭证引用可配置。适配器必须保留协议回放所需的消息块和元数据，不能假定不同服务所有字段均兼容。

### 11.3 Rust 目录

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

## 12. 实现顺序与验收

| 阶段 | 交付 | 验证 |
|---|---|---|
| 1. 进程与会话骨架 | komo、Gateway 自动启动、HTTP/SSE、Session/Run 存储 | 多个 CLI 同时启动仅产生一个实例；断线后能查看原会话 |
| 2. AgentLoop 与 Policy | 模型往返、执行计划、Allow/Ask/Deny、审批持久化 | 危险操作批准前不执行；重复批准不重复执行；Deny 不被授权覆盖 |
| 3. 五个工具 | 文件、进程、Python 环境与输出处理 | 文件版本冲突可见；取消停止子进程；未知工具无法调用 |
| 4. resume | 检查点、事件尾部恢复、调用状态核对 | 已完成动作不重放；started 无结果明确标为未知 |
| 5. toolbox 迭代 | 保存模块、候选测试、版本审核与启用 | 调用使用已批准且已测试版本；模块更新使旧授权失效 |
| 6. Cron | 持久调度、去重、重叠处理、人工接手 | 重启不重复创建同次触发；新危险操作等待审批 |
| 7. 场景与双平台验收 | 真实代码仓库、测试 HA 设备、个人记录 | 两平台独立运行；记录可找回；代码交付含验证证据 |

存储和 Policy 的基本约束从第二阶段开始贯穿全部工具，不能在所有功能完成后才补审核。

首个端到端验收：启动 komo 自动拉起 Gateway，通过模型调用 write 保存记录；关闭客户端并重启 Gateway，resume 原会话，再通过基础工具读出原记录。随后验证一次待审批 shell 在重启后仍等待，批准后只执行一次已确认的调用；若模拟执行结果丢失，则应进入未知状态核对而不是自动重跑。

