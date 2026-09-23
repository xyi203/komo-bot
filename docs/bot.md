**建议保留现有可恢复 Runtime，只新增一个 `komo-agent`，把“Agent 的身份、能力和上下文装配”与“安全执行、调度和恢复”分开。**

不要为每个 Bot 复制 Runtime，也不要为了 filesystem-shaped 重做存储。真正需要改变的是：**每个 Run 都明确绑定哪个 Agent、哪份能力集合、哪个工作目录和哪些上下文，而不是依赖 Gateway 的全局配置。**

本次查看时，`main` 指向 `27ae66c`，最新提交已经包含 skill 目录和 Python 结果展示的修正。下面依据当前代码，而不是早期的架构草案。([GitHub][1])

# 一、当前架构的问题，不是 Loop 不够抽象

当前工作区仍是六个 crate，`komo-runtime` 同时包含 Agent Loop、Executor、Policy、Recovery、Memory，以及 tools、skills、toolbox、Python runtime。

**底层执行机制已经有值得保留的边界。** `AgentLoop` 通过 Ledger 记录模型轮次，通过 Executor 执行工具；遇到审批和依赖等待时挂起、让出执行名额，而不是在进程里一直等人。存储层也已经区分了内容持久化与调度、授权状态的持久化。

主要问题集中在上层装配：

| 当前代码                                                          | 对后续扩展的影响                                  |
| ------------------------------------------------------------- | ----------------------------------------- |
| `Dispatcher` 将操作者的私聊路由到同一个 home session                       | 增加不同 Bot 后，不能继续沿用全局唯一主会话。                 |
| `ToolExecutor` 持有一份 `BTreeMap<String, Arc<dyn Tool>>`         | 能力集合绑定在执行器，而不是每个 Run；多 Agent 需要改变这个绑定位置。  |
| `GatewaySegments` 负责工作目录、历史回放、记忆、提示词及恢复位置                     | Gateway 同时承担传输层和上下文装配职责。                  |
| `GatewayState` 保存全局 `tool_names`、`skills_prompt` 和记忆 preamble | 多 Bot 的提示、skills 和能力差异继续加在这里，会让进程状态越来越复杂。 |

另外，**跨 Run 正文回放、`text_ref` 读取、父子 Run 上下文隔离已经有实现与回归测试；工具结果的 Full / Excerpt 投影也已经落地。** 这些不是待重做模块，应作为重构时必须保持的行为。

---

# 二、目标结构：新增一个能力层，不新增一套执行引擎

建议最终形成下面的依赖关系，箭头表示“依赖”：

```text
komo（bin）
├── komo-client ──────────────────────────→ komo-kernel
└── komo-gateway
    ├── komo-runtime ─→ komo-store ───────→ komo-kernel
    └── komo-agent ───────────────────────→ komo-kernel
```

**`komo-runtime` 和 `komo-agent` 不互相依赖。** Gateway 作为组装入口，把它们接起来。

建议的职责划分：

```text
komo-kernel
    身份类型、事件、执行计划、状态机、跨层契约、纯投影函数

komo-store
    Ledger、JSONL、Payload、ToolOutput、Checkpoint、数据库仓库

komo-runtime
    AgentLoop、ToolExecutor
    Policy、Approval、Recovery、Scheduler
    LLM 协议适配
    Memory 服务
    子进程管理、PythonHost

komo-agent
    AgentProfile
    AgentSurface
    PromptComposer、SkillSelector
    read / write / edit / rg / shell / python 的工具定义与参数处理
    skills、toolbox 的发现与描述
    Resource Namespace

komo-gateway
    HTTP / SSE、渠道接入
    身份认证、消息路由、回复投递
    生命周期与依赖组装
```

这里有两个重要修正。

## 2.1 不能把 `tools/` 和 `python_runtime/` 整包搬走

当前 `PythonTool::VerificationGate` 直接依赖 `PolicyEngine` 和审批仓库；`tools/process.rs` 则依赖恢复模块的 `ChildRegistry` 和执行器的取消逻辑。直接移动目录，会把反向依赖一起搬过去。

我建议这样拆：

**工具层负责描述“要做什么”。** 包括参数解析、工具 schema、构造执行计划、解释工具特有的结果。

**Runtime 负责保证“这件事怎样受控地发生”。** 包括授权、启动进程、登记进程、取消、回收、输出落盘和恢复核对的执行约束。

已有的 `PythonHost` 契约继续复用。进程执行和核对授权确实需要跨层时，再补窄接口；不要因此引入通用插件容器。

尤其是核对：**不能为了拆依赖，让 `verify` 变成一条不经过 Policy 的隐藏执行通道。**

## 2.2 保留现有 Loop，不急着抽象可替换 Loop

这次目标是多个 Bot 使用不同身份和能力，不是多个 Bot 使用不同执行算法。

所以先保持：

```text
不同 AgentProfile
        ↓
不同 AgentSurface + Prompt
        ↓
同一个 AgentLoop / ToolExecutor 实现
```

暂不做 Loop 插件、动态库加载、通用事件总线、跨机器 Agent 调度。它们不解决当前的主要耦合。

---

# 三、最重要的改造：能力集合从 Executor 移到 Run

这是我认为**第一笔架构改动应该落下的地方**。

当前执行器同时提供 `definitions()` 和按名字查找工具，两者来自同一份工具表。这个“一份事实”的性质很好；需要改变的是工具表的作用域。

目标应当是：

```text
全局工具实现目录
    read / write / edit / rg / shell / python
                     │
              AgentProfile 选择
                     ↓
            本次 Run 的 AgentSurface
                     │
          ┌──────────┴──────────┐
          ↓                     ↓
   生成模型工具 Schema      Executor 查找工具
```

**给模型看的工具和实际允许调用的工具，必须来自同一个 Run Surface。**

不能这样实现：

```text
给 researcher 的 schema 去掉 shell
但 Executor 仍然从全局工具表查 shell
```

否则模型只要生成这个名字，仍可能进入执行流程。

建议保留全局工具目录用于发现、构造和复用实例，但**执行时不能回退到全局目录**。某个工具不在本次 Surface 中，就明确拒绝。

例如：

```text
assistant
    tools = read, rg, python

coder
    tools = read, write, edit, rg, shell, python

reviewer
    tools = read, rg
```

这三个 Agent 共用工具实现和 Runtime，只是本次运行得到的能力集合不同。

## Surface 和 Policy 不承担同一个职责

建议明确为：

> **Surface 决定这个 Agent 能调用哪些接口；Policy 决定一次具体调用是否允许。**

`coder` 有 `write`，不等于可以写所有路径；`assistant` 有 `python`，也不等于任意代码自动放行。

而且，启用任意 shell / Python 代码时，它仍然拥有运行账号的操作系统权限。Surface 是逻辑能力边界，不应被描述为操作系统沙箱；当前 `Operation` 的定义也明确承认了这一点。

---

# 四、把 Agent、Session、Run 三种身份分清楚

## 4.1 `AgentProfile`：定义长期助手，不保存运行现场

建议 Profile 只负责可配置的长期定义：

```text
AgentProfile
    id
    instructions
    model
    enabled_tools
    skill_sources
    workspace
    memory_scope
    policy_scope
```

它不应该包含正在运行的任务、当前工具调用、取消令牌或“当前会话”。

也不要在进程里设置一个可变的 `current_agent`，再让各模块去读取。并发运行两个 Agent 时，这种隐式上下文容易串用。

## 4.2 `Session`：绑定一个 Agent，承载一段持续对话

当前 `SessionRow` 有标题、来源、工作目录、当前 Run、日志位置和生命周期状态，没有独立的 Agent 身份字段。

建议增加：

```text
session.agent_id
session.kind
```

其中 `kind` 先保持简单：主会话、普通会话、独立任务会话。不要一开始枚举所有平台和业务场景。

**一个 Session 的 `agent_id` 创建后不随消息路由变化。** 换助手应路由到另一个 Session，而不是给已有会话换人格、换能力，同时保留原来的全部上下文。

### `canonical_session` 在 Komo 中直接变成 `main_session(agent_id)`

当前 `home_session()` 根据 `origin = "home"` 寻找全局主会话。

建议改成：

```text
main_session("assistant") → session-A
main_session("coder")     → session-B
main_session("reviewer")  → session-C
```

主会话仍然只是普通 Session 的一个稳定入口，不是新的执行模式。

数据库应保证每个 Agent 只有一个有效主会话，创建过程也应具有并发唯一性，而不是多个入口各建一个，再挑最早的。

**不需要照搬 Hermes 的 `id → resolved_id` 压缩链。** 对 Komo 的拟议设计，压缩和边界变化只改变上下文窗口或检查点，不要求更换长期 `SessionId`。

### 委派仍属于 Run 语义

当前 delegate 在同一个 Session 内创建子 Run，并通过 `ReplayScope::Thread`（它自己那条续跑线）隔离上下文。

先保留它。不要为了增加 `SessionKind`，强行把现有每个子 Run 都迁成新 Session。

将来增加跨 Agent 委派时，再显式决定目标 Agent 的任务会话；**不要默认把所有子任务塞进目标 Bot 的主聊天。**

## 4.3 `Run`：固定本次执行身份与能力

目前 `AcceptInput` 已经保存本次 Run 的模型配置，说明“受理时确定运行配置”的基础存在。

建议沿这个方向扩展，而不是另建一套 Profile 运行机制：

```text
RunSnapshot（拟议）
    agent_id
    profile_revision
    surface_revision
    model
    resolved_workspace
    instructions_ref
    memory_scope
    resource_mounts
```

这里的关键不是字段数量，而是两个约束。

**第一，快照必须可恢复。** 只存 `profile_id` 或某个文件路径不够：审批期间文件可能已经修改。身份指令和能力描述要保存内容，或引用仍可读取的不可变版本；可以复用已有 PayloadStore，不必新造存储系统。

**第二，固定配置不等于冻结安全策略。** 普通 Profile 编辑影响后续 Run；但显式撤权、Deny 规则和停用状态仍应在执行时检查。旧快照不能成为绕过当前撤权的通行证。

---

# 五、Gateway 只路由；上下文装配与 Provider 编码分开

## 5.1 路由同时保留“从哪来”和“由谁执行”

多 Bot 下，建议明确区分：

```text
TransportIdentity
    平台
    bot 账号
    chat / thread
    操作者

ExecutionIdentity
    agent_id
    session_id
    run_id
```

例如两个平台可以继续对应同一个助手的主会话：

```text
飞书个人助手私聊 ───┐
                  ├── assistant / main
Telegram 助手私聊 ─┘

Telegram 编码助手 ─── coder / main
```

但群聊不应无条件进入私人主会话。

回复投递地址也应跟随受理记录保存，不能在任务完成时重新解析“当前默认 Bot”，否则热重载路由后，结果可能回到不同入口。

**平台去重键应包含 bot 账号维度，并让重投命中第一次受理的 Run。** 不要因为 Agent 路由配置改变，就把同一条平台消息重新当成新任务。

首阶段可以仍然只连接每个平台的一个账号，但这些身份不要合并成一个字段。

## 5.2 Context 装配应成为明确流程

当前 `GatewaySegments` 生成系统提示、回放消息和恢复位置，记忆正文又通过 `MemoryPreamble → LlmFactory` 接到模型适配器上。

建议拆成：

```text
Runtime
    读取已提交历史
    恢复未完成调用
    召回允许访问的记忆
    读取工具结果事实
              ↓
Agent Context 装配
    Profile 指令
    本次 Surface 的工具与资源目录
    与任务相关的 Skills
    带来源的记忆
    历史与工具结果投影
              ↓
完整 TurnRequest
              ↓
LLM Adapter：只负责协议编码、请求和响应解析
```

具体边界是：

**Runtime 决定哪些执行事实必须恢复、哪些数据可以读取；Agent 层决定如何组织身份、skills 和提示；Provider Adapter 不再自行寻找记忆或补人格指令。**

这也让切换 Chat Completions / Responses 时，不必重复实现 Agent 的上下文规则。

### Skills 保留最新修复，但继续走任务相关选择

最新提交修好了 YAML 块描述和目录展示；当前 frontmatter 仍主要是 `name`、`description`、`platforms`、`requires_tools`，不是之前讨论的 query-aware skill 选择。([GitHub][1])

下一步应是：

```text
Profile 允许的 skills
    → 当前 Surface 能力过滤
    → 当前任务相关性选择
    → 少量目录 + 高置信说明预加载
```

稳定身份与简短目录放在前面，任务相关内容放在后面。不要继续用“把目录字符上限翻倍”代替选择逻辑，也不要让每轮模型调用重新扫描和注入全部 Skills。

记忆同样需要作用域：默认读取本 Agent 的记忆，显式允许共享用户资料；过滤应进入检索过程，而不只是结果展示时遮掉其他 Agent 的记录。

---

# 六、filesystem-shaped：统一资源入口，不统一所有执行方式

建议第一阶段只覆盖：

```text
skill://...
tool://...
artifact://...
```

它们分别表示 Skill 文档、工具说明或 schema，以及已落盘产物和工具输出。

模型仍然使用现有工具：

```text
read("skill://review/SKILL.md")
read("tool://python/schema")
read("artifact://.../stdout")
rg(pattern="...", path="skill://")
```

**`tool://` 是工具说明入口，不是通过写文件执行工具。** 真实动作仍走 native tool call、ExecutionPlan 和 Policy。

在归属上，AgentSurface 决定挂载哪些资源；Runtime 负责对访问计划进行授权；Store 继续保存真实内容。

## 关键改动在执行计划，不在字符串替换

当前 `PlanTarget` 直接保存 `PathBuf`，Policy 对真实路径判断。

不能把 `skill://...` 直接当成一种“特殊文件路径”。建议引入明确的目标类型：

```text
TargetRef
    LocalPath
    ResourceUri
```

资源解析必须保留逻辑 URI、所属 Agent / Session、版本和实际读取目标。审批与审计应能回答“允许读取哪个逻辑资源”，而不只是“访问了哪个临时文件”。

这里还要注意：当前已经有 `ResourceRef`，用途是外部服务端点和凭证引用；不要复用这个名字把两种语义混在一起。

首阶段继续保持原定范围：不开放 memory/session 的任意写入，不给 shell/Python 自动展开 URI，不实现通用 VFS。原来的本地路径用法继续兼容。

---

# 七、并发单独推进，不必等完整多 Bot 架构完成

当前 Scheduler 已经支持多个 Run 并发，而 `ToolExecutor::execute_round` 仍逐个执行同轮工具。两种并发应明确分开。

建议保持：

```text
不同 Session 的 Run：受全局额度控制，可并发
同一持续对话的普通 Run：维持顺序
同一 Run 的工具：先支持有界只读并发
```

第一版工具并发只覆盖已确认只读、并且已经获得 Allow 的调用，例如：

```text
[read A, rg B]
       ↓ 等待这一批完成
    write C
       ↓
    read C
```

`write`、`edit`、`shell`、`python` 和 `delegate` 先作为顺序屏障。不根据模型声称“没有副作用”就并行。

要守住的行为是：遇到 Ask 后不再启动后续调用；挂起前收齐已启动调用的结果；完成事件按真实发生顺序记录，交给模型时按原始调用关系配对；恢复只处理未收尾调用，不重放整批。

**这个改动只需要 Run Surface 和执行边界稳定，不需要先完成资源命名空间、跨 Agent 消息或群聊。**

---

# 八、先修一处已确认的装配缺口，再分批重构

当前有一条具体路径值得先修：

`POST /v1/sessions` 收到 `workdir` 后，调用 `remember_workdir` 保存到内存；但执行段读取的是数据库中的 `SessionRecord.workdir`。存储层实际上已经有 `set_workdir_in`，而交互 `submit` 传入的 `AcceptInput.workdir` 是 `None`。**这条 API 路径存在“展示的工作目录与执行实际使用目录不一致”的风险，不只是重启后丢失。**

应通过现有 Store / Ledger 写入路径统一工作目录来源，删除这类双份事实，再在新增 RunSnapshot 时固定本次最终目录。

建议按以下改动批次实施：

| 批次               | 修改范围                                                               | 验收                                                     |
| ---------------- | ------------------------------------------------------------------ | ------------------------------------------------------ |
| **0：固定现有行为**     | 打通会话工作目录的持久化与读取；保留现有回放、审批、恢复回归测试                                   | 指定目录创建会话后，首次执行与重启恢复使用同一目录                              |
| **1：拆执行与能力边界**   | 引入默认 AgentSurface；工具 schema 与实际查找统一使用它；收口进程和核对授权接口；抽出 `komo-agent` | 默认行为不变；不在 Surface 的工具无法调用；Runtime 不依赖 Agent crate      |
| **1（续，2026-09-22 已做）** | `crates/komo-agent` 新建，只依赖 kernel：`skills/`（发现与目录）自 runtime 搬入，`surface_of` + `DELEGATE_TOOL` 自 gateway 搬入；`snapshot_fixture` 归入 kernel `test-support`；值类型留 kernel（事件/协议/store 按它们落盘）；`cargo tree` 确认 runtime 不见 agent | 同左（2026-09-22 本机：全量通过，只剩 `chat::skills::the_system_prompt_lists_the_skills_the_model_can_read` 一处失败——干净 HEAD 同样挂，是测试把 `platforms: [macos]` 写死成"本机非 mac"的假设，macOS 上必然命中，与本次拆分无关） |
| **2：支持多个 Agent** | AgentProfile、Session 归属、按 Agent 的主会话、RunSnapshot、记忆作用域和入口路由        | 两个 Agent 的提示、工具、目录、历史不串用；审批等待期间修改 Profile，旧 Run 不被静默替换 |
| **3：资源命名空间**     | `skill/tool/artifact` 只读入口，扩展 `read/rg`，执行计划识别资源目标                 | 本地路径兼容；资源越权被拒；产物引用在重启后仍可读                              |
| **独立批次：工具并发**    | 在第 1 批之后增加有界只读并发                                                   | 顺序屏障成立；取消能收尾；故障恢复不重复已完成调用                              |

重构期间，数据库、事件和计划格式的变更需要版本化：已有 Session 归入默认 Agent；旧日志不重写；旧审批计划继续按原版本校验哈希，不能因为补了新字段就失效或被重新解释。

每批同时更新 `docs/komo_bot.md` 的对应约定，并比较修改前后的 `cargo build --timings`，不要用“拆了 crate”代替依赖边界和编译结果的验证。

**最终方向是：一个共享执行内核，多个显式 AgentProfile，每个 Run 一份明确的 AgentSurface 和可恢复上下文。第一笔架构改动应先做 Run-scoped Surface，而不是先写 Bot 类或再拆一组服务。**

本次是源码静态设计审阅，未修改仓库，也未执行编译和测试；表中的验收项是改造要求，不是已通过的结果。

[1]: https://github.com/xyi203/komo-bot/commit/main "skill 目录补全，python 工具少一轮也不再骗人 · xyi203/komo-bot@27ae66c · GitHub"

