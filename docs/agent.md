# Komo Agent 层收口重构设计

## 1. 结论

当前 Komo 的主干架构已经基本稳定：

```text
AgentProfile
    ↓
Session(agent_id)
    ↓
RunSnapshot
    ↓
AgentSurface
    ↓
AgentLoop / ToolExecutor
    ↓
Ledger
```

现在最主要的问题已经不是 Runtime，而是：

> **Agent Context 的所有权没有真正落到 `komo-agent`。**

目前 `komo-agent` 已经拥有：

```text
Agent Surface（surface_of）
Skills（SkillRegistry 的发现，以及目录块的渲染 prompt_block）
```

但真正决定“模型这一轮看见什么”的逻辑散在三处（§1.1 有逐项核对）：

```text
komo-gateway/src/service/segment.rs
    system_prompt / subagent_prompt / with_instructions   系统提示正文
    replay / window / message_text                        历史 → ReplayMessage
    skills_block                                          只是转手调 komo-agent

komo-runtime/src/memory/preamble.rs
    render_injection                                      记忆注入段（含 token 预算截断）

komo-runtime/src/llm/{responses,chat,anthropic}.rs
    SystemPreamble 钩子                                    在适配器里把记忆段追加到系统提示后面
```

这导致当前实际边界变成：

```text
komo-agent
    ├── Surface
    └── Skills

komo-gateway
    ├── Routing
    ├── Run orchestration
    └── Agent Context Assembly   ← 越界

komo-runtime
    └── Memory presentation + 发送前改写系统提示   ← 越界，且对 TurnRequest 不可见
```

本次重构只解决一个问题：

> **让 `komo-agent` 成为完整的 Agent Context Boundary。**

目标状态：

```text
komo-gateway
     │
     │ 收集运行时数据
     ▼
ContextInput
     │
     ▼
komo-agent
ContextAssembler
     │
     ▼
AgentContext
     │
     ├── system prompt
     └── model history
     │
     ▼
komo-gateway
     │
     ├── Tool definitions
     └── AgentContext
     │
     ▼
TurnRequest
     │
     ▼
komo-runtime
```

这次**不改 Ledger、不改 Tool Pipeline、不改 Recovery、不引入 Plugin Framework，也不为了抽象而增加 trait。**
Memory 是唯一需要碰 runtime 的地方，单独放在最后一个阶段（§13、§20 Phase 5）。

---

## 1.1 现状核对（2026-09-23，对照代码）

| 这件事 | 现在在哪 | 说明 |
|---|---|---|
| 主 Agent 系统提示 | `segment.rs` `system_prompt` | 身份句 + 工作目录 + 工具名 + `RULES` + skills 块 |
| 子代理系统提示 | `segment.rs` `subagent_prompt` | 与上面重复了工作目录、工具名、`RULES`；另有任务与结果契约 |
| 身份指令置顶 | `segment.rs` `with_instructions` | 指令正文从冻结快照指向的 payload 读回 |
| Skills 目录块 | `komo-agent` `SkillRegistry::prompt_block` | Gateway 只拼 `OfferContext`（按本段工具名门控）。**已经在 agent 里** |
| 历史回放 | `segment.rs` `replay` / `window` / `message_text` | 输入是 kernel 的 `Surface`（`fold` 的结果），不是原始事件 |
| 工具结果正文 | `komo-kernel/src/projection.rs` `project` | 唯一一处投影；`replay` 负责从 `ToolOutputStore` 取 `body.preview` 与 artifacts 喂给它 |
| 记忆召回与钉住 | `komo-runtime` `MemoryManager::prepare_segment` | 按 `boundary` 钉在进程内 `pins` 表里，检查点记 `MemoryUse`。返回值 `Injection { text, uses }` **已经带着正文**，但 `segment.rs` 的 `recall_for` 只取了 `.uses` |
| 记忆渲染 | `komo-runtime/src/memory/preamble.rs` `render_injection` | 抬头 + 每行带来源、确认状态、时间与版本 + 按渲染后的行长截断预算 |
| 记忆进系统提示 | 各 LLM 适配器的 `SystemPreamble` 钩子 | 正文绕了一圈：`settle_selection` 写进 `selections` 表 → `MemoryPreamble` 按 run 取 → 三个适配器各自 `"\n\n"` 追加。`TurnRequest.system_prompt` **不是**最终发出去的系统提示 |
| 子代理隔离 | `segment()` | 不召回记忆、`ReplayScope::Thread`、`without_delegate`。已经在上游做了，符合 §12 |
| `skill://` 挂载 | `segment()` 里的 `ResourceMounts.skills` | 用的是 `SkillRegistry::list()`（全部 skill，含被盖住、被 disable 的），**不是**提示里那份目录。两者本来就不是同一个集合 |

`segment.rs` 共 2011 行，其中约 700 行是测试。

顺带发现的一个问题：`selections` 表只进不出。`MemoryManager::forget_run` 在生产代码里没有
调用方，长跑的 Gateway 里每个 Run 的注入段都留在内存里。Phase 5 删掉这张表，问题随之消失。

测试覆盖的缺口：集成测试（`tests/agents`、`tests/chat`）注入的是 `FakeLlm`，绕过了
`LlmFactory`，所以**没有任何测试看得到记忆段真的进了系统提示**。Phase 5 之后记忆段在
`TurnRequest.system_prompt` 里，`FakeLlm` 就能断言它。

---

# 2. 重构原则

## 2.1 Agent 层回答一个问题

`komo-agent` 应该只回答：

> 对于这个 Agent、这个 Run、这个时刻，模型应该看到什么？

因此它负责：

```text
Identity
Capabilities
Instructions
Conversation Context（选窗口、把已解析的消息变成 ReplayMessage）
Memory Presentation
Skills Presentation
Runtime Facts Presentation
Delegation Context
```

它不负责：

```text
调用模型
执行 Tool
审批
恢复
持久化
Memory 检索
Session 查询
Payload / ToolOutput 读取
调度
```

---

# 3. 最终职责边界

目标依赖关系保持不变：

```text
             komo-kernel
            /     |     \
           /      |      \
  komo-agent   komo-store
                   |
              komo-runtime
                   |
              komo-gateway
                /     \
       komo-agent     runtime
```

关键铁律：

```text
komo-agent
    ❌ 不依赖 komo-runtime
    ❌ 不依赖 komo-store
    ❌ 不访问 Ledger
    ❌ 不访问数据库
    ❌ 不调用 MemoryManager
    ❌ 不执行 Tool
```

它现在的依赖是 `komo-kernel`、`serde_json`、`tracing`（见 `crates/komo-agent/Cargo.toml`）。
本次重构预期**不需要新依赖**：子代理契约的 schema 渲染已经用 `serde_json`，日期格式化
（记忆行里的“观察于”）用 kernel 类型自带的 `date()`。如果真要加（例如 `time`），按
AGENTS.md 的规矩同时更新 `docs/komo_bot.md` §13.4 的依赖表与 `komo-agent` 那一行。

`runtime` 不依赖 `komo-agent` 这一条同样不变，它直接决定了 §13 的切法。

---

# 4. 各层重新定义

## 4.1 `komo-kernel`

继续保存跨层稳定 contract。现有的名字：

```rust
AgentId
AgentProfile
AgentSurface
RunSnapshot
DelegateSpec

Event / EventPayload        // 账本事件
Surface / SurfaceMessage    // fold 的结果
ReplayMessage               // 交给模型的一条消息（types/turn.rs）
ToolDefinition
TurnRequest
MemoryItem / MemoryUse

projection::project         // 工具结果的唯一投影（纯函数）
```

这里不要放：

```text
ContextAssembler
SkillRegistry
PromptBuilder
MemoryRetriever
```

Kernel 仍然保持“数据类型 + 跨边界协议”。`projection.rs` 留在 kernel 的理由不变：
“刚跑完”与“重启后回放”必须逐字节相同，渲染只能有一处（AGENTS.md）。Agent 层**调用**
它，不复制它。

---

## 4.2 `komo-agent`

重构后它应该成为：

```text
komo-agent
├── surface
├── skills
└── context
```

完整结构建议（`skills/` 按现有文件，不为本次重构拆分）：

```text
crates/komo-agent/src/

├── lib.rs
│
├── surface.rs
│
├── skills/
│   ├── mod.rs          SkillRegistry、OfferContext、catalog、prompt_block
│   ├── frontmatter.rs
│   └── tests.rs
│
└── context/
    ├── mod.rs          ContextInput、AgentContext、InvocationContext、assemble
    ├── history.rs      entries（纯选择） + ResolvedMessage → ReplayMessage
    ├── prompt.rs       主 / 子代理共用一套 section
    └── memory.rs       记忆注入段渲染（Phase 5 才搬进来）
```

不需要再拆新的 crate。

---

# 5. 最核心的新边界：ContextInput → AgentContext

整个重构围绕两个类型进行。

## ContextInput

```rust
pub struct ContextInput<'s> {
    /// 身份指令正文（已从冻结快照指向的 payload 读回）。
    pub instructions: Option<String>,

    /// 解析过的真实工作目录。
    pub workspace: PathBuf,

    /// 这一段真正挂上的工具名，按 Schema 的顺序。系统提示要列它们，skills 门控也按它，
    /// 所以 tool definitions 必须在 assemble 之前算好（§16）。
    pub tools: Vec<String>,

    /// 已解析正文的回放条目（§8）。借用 `Surface`，不复制消息。
    pub history: Vec<ResolvedMessage<'s>>,

    /// 这一段钉住的记忆注入段；`None` = 不注入（子代理、记忆关闭、没召回到）。
    /// 正文由 `context::memory::render` 渲染（§13），这里只是交给 assembler 放位置。
    pub memory: Option<String>,

    /// 已按 OfferContext 门控的 skills 目录；只有主 Agent 有。
    pub skills: Option<SkillCatalog>,

    pub invocation: InvocationContext,

    /// 工具结果投影的正文预算（`[execution] model_result_bytes`）。
    /// 与 `CallEnv` 用同一个值，否则"刚跑完"和"回放"渲染出来不一样。
    pub model_result_bytes: usize,
}
```

身份里的模型、能力面、记忆作用域不进来：它们决定的是 `TurnRequest.model`、tool
definitions 和召回范围，都在 I/O 阶段用掉了，assembler 用不到。

它代表：

> Gateway / Runtime 已经把所有需要 I/O 才能获得的数据准备好了。

这里不允许：

```rust
MemoryManager
Store / PayloadStore / ToolOutputStore
Coordinator
SessionHandle
ToolExecutor
```

这样的运行时对象进入。

---

## AgentContext

```rust
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<ReplayMessage>,
}
```

`messages` 直接是 kernel 的 `ReplayMessage`（带 `seq`、`tool_calls`、`tool_results`、
`provider_blocks`），不再造一个 `ModelMessage`。

第一版保持非常简单。

不要一开始设计：

```text
PromptGraph
ContextProvider
ContextPlugin
ContextMiddleware
```

目前没有需求。

核心 API：

```rust
// komo-agent/src/context/mod.rs
pub fn assemble(input: ContextInput<'_>) -> AgentContext;
```

用一个自由函数，不用 `ContextAssembler` 单元结构体：它没有状态，而这个 crate 现有的
入口（`surface_of`）也是自由函数。下文说“ContextAssembler”时指的就是它。

它应该是：

```text
deterministic
pure
easy to test
```

即：

```text
相同 ContextInput
       ↓
相同 AgentContext
```

读不出正文都发生在 I/O 阶段，处理照旧：外置 payload 哈希对不上 → Gateway
`halt_if_corrupt`；`output.json` 打不开 → 退回账本里那 ≤1 KiB 的 `preview`（现在
`replay` 里的 `.ok()`，不停段）。assemble 本身没有失败路径，签名里也就不带 `Result`。

---

# 6. 为什么不能让 `ContextAssembler` 自己去查 Memory

看起来最简单的设计可能是：

```rust
assembler
    .with_memory(memory_manager)
    .with_skills(skill_registry)
    .assemble(...)
    .await
```

不建议。

这样很快会变成：

```text
ContextAssembler
├── MemoryManager
├── SkillRegistry
├── Store
├── PayloadStore
├── SessionRepo
├── Clock
└── Config
```

最后只是把现在的 `segment.rs` 搬进 `komo-agent`。何况 `MemoryManager` 在 runtime，
agent 根本依赖不到它。

正确边界应该是两阶段：

```text
           I/O Phase
              │
              ▼
        Resolve Sources
              │
              ▼
         ContextInput
              │
──────────── boundary ────────────
              │
          Pure Phase
              │
              ▼
      ContextAssembler
              │
              ▼
         AgentContext
```

---

# 7. Gateway 应该留下什么

Gateway 并不是完全退出 Context 流程。

它仍然负责：

> 收集 Context 需要的事实。

建议新增：

```text
komo-gateway/src/service/context_sources.rs
```

搬过去的（为上下文取数的）：

```text
Identity / identity_for / ambient_identity / frozen_snapshot / delegate_parent
recall_for / recall_scopes
message_text + 工具结果的 ToolOutputStore 读取（history 的 I/O 那一半，§8）
skills 目录的读取（registry.offer(OfferContext) → SkillCatalog，§14）
```

留在 `segment.rs` 的（为执行取数的，不属于“模型看见什么”）：

```text
resumed / call_request / waiting_approval    续跑位置
run_roots / ResourceMounts                   CallEnv 的根与挂载
halt_if_corrupt                              失败收场
without_delegate                             能力收窄（它改的是 surface，不是上下文）
```

写成**自由函数、参数显式传入**，不写成 `impl GatewaySegments`：这样每个函数要用哪些
存储一眼看得出来，也能不造整个 `GatewaySegments` 就单测。`GatewaySegments` 上的方法
只剩把自己的字段递过去的一层。

它负责：

```text
RunSnapshot
     │
     ├── resolve instructions payload
     │
     ├── load session events → fold → Surface
     │
     ├── komo_agent::context::history::entries(&surface, scope)  ← 纯函数，先选
     │
     ├── resolve 被选中消息的外置 payload 与 tool output          ← 只读选中的
     │
     ├── obtain pinned memory context
     │
     ├── obtain skill catalog（registry + OfferContext）
     │
     └── collect runtime facts
     │
     ▼
ContextInput
```

可以先做成普通函数：

```rust
/// history 的 I/O 那一半：只对 `entries` 选中的条目读正文与 output.json。
async fn resolve_history<'s>(
    entries: Vec<Entry<'s>>,
    payloads: &PayloadStore,
    outputs: Option<&dyn ToolOutputStore>,
) -> Result<Vec<ResolvedMessage<'s>>, LedgerError>

/// 冻结身份（含指令正文）。三条路见现在的 identity_for。
async fn identity_for(
    events: &[Event],
    run: &RunId,
    payloads: &PayloadStore,
    fallback: Fallback<'_>,        // 当前配置、会话 workdir、workspaces/，兜底那条路用
    catalog: &[ToolDefinition],
) -> Result<Identity, LedgerError>
```

不要马上抽：

```rust
trait ContextSourceResolver
```

目前只有一个实现。

---

# 8. History 的边界怎么切

这是此次重构里最容易切错的一块。

现在是：

```text
events.jsonl
    ↓
fold                              kernel，纯
    ↓
Surface / SurfaceMessage
    ↓
window(surface, scope)            gateway，纯：选哪些 Run、Run 之间先来后到、
    ↓                                   历史 Run 只留用户正文与最终回复
message_text / ToolOutputStore    gateway，I/O：外置正文、output.json
    ↓
projection::project               kernel，纯：工具结果正文
    ↓
ReplayMessage
```

也就是说，“Ledger 事件格式”和“模型消息格式”之间**已经有一层**：`Surface`。账本以后
为恢复加的事件（`ToolStarted`、`ApprovalRequested`、`RunWaiting` …）先被 `fold` 吸收，
不会直接进模型上下文。所以这次**不再加一层平行于 `SurfaceMessage` 的 `ConversationItem`**，
只把两段纯逻辑搬到 agent，把 I/O 那段留在 Gateway：

```text
Ledger
  │
  ▼
Gateway: events → fold → Surface
  │
  ▼
komo-agent: history::entries(&Surface, scope) → Vec<Entry>                纯
  │
  ▼
Gateway: 对选中的消息解析正文 → Vec<ResolvedMessage>                      I/O
  │
  ▼
komo-agent: ResolvedMessage → ReplayMessage（工具结果调 kernel::project）  纯
```

具体的类型：

```rust
// komo-agent/src/context/history.rs

pub enum ReplayScope<'a> {
    Conversation(&'a RunId),   // 主对话：跨 Run 聚合，正在跑的那条带完整协议
    Thread(&'a [RunId]),       // 子代理：它那条线（`resumes` 链，旧→新，末尾是正在跑的这条）；
                               // 没续跑过就只有一条。规则同 Conversation（komo_bot.md §4、§8.3）
}

/// 一条要回放的消息，以及它按哪种方式回放。
pub struct Entry<'s> {
    pub message: &'s SurfaceMessage,
    pub kind: EntryKind,
}

pub enum EntryKind {
    /// 历史 Run（或不属于任何 Run 的消息）：只要正文，不要调用、结果与原生块。
    Transcript,
    /// 正在跑的那条 Run：完整协议。
    Protocol,
}

/// 纯函数：选窗口，并且已经筛掉历史 Run 里“不是用户正文、也不是最终回复”的那些。
/// Gateway 只对这里返回的条目读正文。
pub fn entries<'s>(surface: &'s Surface, scope: ReplayScope<'_>) -> Vec<Entry<'s>>;

/// 记忆召回用的查询文本（现在的 latest_user_text）。
pub fn latest_user_text(surface: &Surface) -> Option<String>;

/// Gateway 读回来的东西。
pub struct ResolvedMessage<'s> {
    pub entry: Entry<'s>,
    /// 内联正文，或外置正文按引用读回来的结果。
    pub text: Option<String>,
    /// 与 `entry.message.tool_results` 一一对应；`None` = output.json 没读成。
    /// 只有 `Protocol` 条目才会去读。
    pub outputs: Vec<Option<StoredOutput>>,
}

pub struct StoredOutput {
    pub preview: Option<String>,   // body.preview
    pub artifacts: Vec<ContentRef>,
}
```

“`output.json` 没读成就退回账本里那 ≤1 KiB 的 `preview`”、“历史条目正文为空就跳过”、
`provider_call_id` 找不到时用内部 call id，这些都是**展示规则**，在 agent 里做；Gateway
只报告读到了什么。

为什么要先选窗口再读正文、而不是全读一遍再交给 agent：现在只有**被选中**的消息会去读
外置正文。读全部会多读 payload，更要紧的是：一条落在窗口外、已经损坏的 payload 会让
这一段 `halt_if_corrupt`。这是行为变化，不许发生。

记忆召回的查询文本（`latest_user_text`，用 `window(surface, None)`）改为 agent 的
`history::latest_user_text`：Gateway 依赖 agent，这样调没问题。

工具结果只有 `kernel::projection` 一处投影，这条铁律不变。Agent 层调它，不自己拼
“完整输出在哪”那句话。

---

# 9. `Model-visible means reconstructable`

建议把 DeepSeek Harness 这一条思想正式写成 Komo invariant：

> 任何进入 Model Request 的历史信息，都必须能够从 Durable Ledger + RunSnapshot 重建。

但注意不要求：

```text
所有 System Prompt 都写进 events.jsonl
```

因为 Komo 已经有：

```text
RunSnapshot
instructions payload
profile revision
```

所以 Komo 的 invariant 更准确应该是：

```text
Model-visible Context
=
Durable RunSnapshot
+
Durable Session History
+
Pinned Segment Context
```

其中：

```text
Memory
```

目前已经有 segment pin 语义：同一个 `boundary` 内逐字复用注入段；进程内表丢了（重启），
就从检查点里的 `MemoryUse` 重新核对、再渲染。

因此不要每一轮重新 recall 后偷偷改变历史 segment。

这是现在设计里非常好的点，应继续保持。

**现有的两个缺口**（本次记录下来，不在本次修）：

1. **Skills 目录块没有钉住。** `skills_block` 每一段都从**活注册表**现渲染。审批一小时后
   才答复、期间 skills 被重载，续跑出来的系统提示就变了。`ResourceMounts` 的注释说
   “skill 挂载是这一次 Run 开始时的快照”，但续跑也是一次新的 `segment()`，同样读活
   注册表。要满足上面的 invariant，目录需要进 `RunSnapshot`，或者像记忆那样钉住。
2. **记忆段对 `TurnRequest` 不可见。** 它由适配器的 `SystemPreamble` 在发送前追加，
   `TurnRequest.system_prompt` 与实际发出去的不是同一个字符串（§13）。

---

# 10. System Prompt 收口

目前的：

```text
system_prompt()
subagent_prompt()
with_instructions()
skills_block()        （转手调 komo-agent 的 prompt_block）
```

应该全部离开 Gateway。记忆段（`render_injection` 与 `SystemPreamble` 钩子）在 Phase 5 才动。

统一变成：

```text
context/prompt.rs
```

内部可以使用简单的 section：

```rust
struct PromptSection {
    key: &'static str,
    content: String,
}
```

按**现在的顺序**组装（Phase 1 必须逐字相同，所以顺序照抄现状，不重新设计）：

```text
Instructions        身份指令，最前（with_instructions，§4.3）
    ↓
Identity            “你是 komo …” / “你是 komo 派出去的子代理 …”
    ↓
Delegated Task      只有子代理
    ↓
Runtime Environment 工作目录、可用工具
    ↓
Rules               RULES 三条
    ↓
Skills              只有主 Agent，空就不出现
    ↓
Result Contract     只有子代理
    ↓
Memory              现在由适配器追加在最后；Phase 5 并进来，位置仍在最后
```

Memory 必须留在最后：记忆段在同一段对话里逐字复用，放在尾部是为了不打掉前面那段的
服务端前缀缓存（`recall_for` 的注释）。

分隔符也照抄：主提示内部是 `\n`，skills 前是 `\n\n`，指令后是 `\n\n`。

现在不需要公开 `PromptSection`。

它只是 `komo-agent` 内部实现细节。

---

# 11. 主 Agent 与 Subagent 不应该维护两套 Prompt Builder

现在存在：

```text
system_prompt()
subagent_prompt()
```

两者重复了工作目录、工具名（含“这一段没有工具”）和 `RULES`。建议合并。

使用：

```rust
pub enum InvocationContext {
    Main,

    Delegated(DelegateSpec),   // kernel 现有类型：task、rounds、contract、parent、call
}
```

直接用 `DelegateSpec`，因为结果契约的 schema 要原样渲染进提示；只放 `task` 和
`parent_run_id` 是不够的。

ContextAssembler 根据 invocation 增加不同 section：

```text
Main

Instructions
Identity
Runtime Environment
Rules
Skills
（Memory）


Delegated

Instructions
Identity（子代理版，含“看不到主对话、记忆与 Skills”）
Delegated Task
Runtime Environment
Rules
Result Contract
```

而不是：

```text
main_prompt.rs

subagent_prompt.rs
```

逐渐复制两套逻辑。

---

# 12. Subagent 隔离由上游保证，不靠 Prompt

这里要继续坚持当前设计。

Child Run：

```text
不继承 parent history      ReplayScope::Thread（只有它自己那条线）
不继承 parent memory       delegate 存在时不调 recall_for
不继承 parent skill context
Surface 移除 delegate      without_delegate（runtime 编排里还有第二道）
```

这些现在都已经在 `segment()` 里、`ContextInput` 构造之前做掉了。重构后它们留在 Gateway
的 I/O 阶段：子代理的 `ContextInput` 里 `memory` 为空、`skills` 为空、`history` 只有它
自己那条线（没续跑过就是那一条 Run）。

不要变成：

```text
System Prompt:
“请不要读取父 Agent 的 Memory。”
```

子代理提示里那句“你看不到主对话、记忆与 Skills”是**告知**（让它知道缺什么就自己查），
不是隔离手段。

Capability isolation 永远优先于 Prompt isolation。

---

# 13. Memory 的正确边界

目标仍是：

```text
runtime:
    What memories?   召回谁、rerank、向量检索、System One、按段钉住、使用计数

agent:
    How does the agent see them?   抬头、每行格式、provenance 怎么说、预算怎么截、放在提示哪里
```

## 13.1 现在的路径

```text
MemoryManager::prepare_segment                           runtime
    钉住命中？→ 核对条目还在不在 → 沿用那一块
    否则 prepare：revalidate / 召回 → render_injection(items, max_tokens)
    → Injection { text, uses }
    settle_selection：记使用计数 + 写进 selections 表
                                         │
segment.rs recall_for                    │  只取 .uses → TurnRequest.memories（审计）
                                         │
MemoryPreamble（SystemPreamble 的实现）   ▼
    injection_for(run) ← selections 表
                                         │
三个 LLM 适配器 begin_turn               ▼
    system_prompt + "\n\n" + 正文        ← 实际发出去的系统提示
```

两个硬约束决定了切法：

1. **runtime 不能依赖 agent**，`MemoryManager` 里不能直接调 agent 的函数。
2. **预算按渲染后的行算**，而 `uses` 必须恰好是被渲染进去的那些条目（§9.7 的审计证据），
   钉住的也必须是那段逐字不变的文本（前缀缓存）。渲染和截断因此拆不开。

## 13.2 切法：渲染函数在 agent，由 Gateway 递给 MemoryManager

```rust
// komo-kernel/src/types/memory.rs（从 runtime 的 preamble.rs 挪过来，字段不变）
pub struct Injection {
    pub text: Option<String>,
    pub uses: Vec<MemoryUse>,
}

// komo-agent/src/context/memory.rs（render_injection 及其 HEADER / render_line 搬过来）
pub fn render(items: &[MemoryItem], max_tokens: u32) -> Injection;

// komo-runtime/src/memory/mod.rs
pub type RenderInjection = fn(&[MemoryItem], u32) -> Injection;
pub struct MemoryParts {
    ...
    pub render: RenderInjection,   // Gateway 装配时给 komo_agent::context::memory::render
}
```

`MemoryManager` 里其余的一行不改：钉住、核对、召回、使用计数照旧，只是把
`render_injection(...)` 换成 `(self.render)(...)`。

然后把绕的那一圈拆掉：

```text
segment / context_sources   recall_for 返回整个 Injection
                            text → ContextInput.memory，uses → TurnRequest.memories
assemble                    把 memory 放在系统提示最后，前面加 "\n\n"（§10）
runtime 删掉                 SystemPreamble、MemoryPreamble、LlmFactory::with_preamble、
                            三个适配器里的 preamble 字段、selections 表、injection_for、forget_run
gateway 删掉                 GatewayState.preamble 与 build_llm 的 preamble 参数
```

为什么用函数指针而不是“钉住条目、让 agent 渲染、再把 uses 交回来”：后者要把
`MemoryManager` 的钉住单位从文本改成条目，再在 Gateway 里多一次往返把 `uses` 交回去
记使用计数。两边都是行为敏感的地方（遗忘必须立刻生效、同一段逐字复用），改动面大，收益
只是“少一个函数参数”。`fn` 指针不是 trait、没有 `dyn`，也只有一个实现。

## 13.3 收益

- 实际发出去的系统提示就是 `TurnRequest.system_prompt`。§9 的 invariant 不再有例外，
  `FakeLlm` 能断言记忆段（§1.1 的测试缺口）。
- 三个适配器少一段相同的拼接代码，也不再各自决定分隔符。
- `selections` 表的内存泄漏随表一起消失。

## 13.4 MemoryManager 的测试怎么办

`render_injection` 的两个测试（五要素展示、预算截断）跟着函数搬到 `komo-agent`。其余
`MemoryManager` 测试要一个渲染函数：runtime 的测试里给一个最小的 `fn`（每条一行正文），
它们断言的是“同一段逐字复用”“遗忘的条目不回来”，与具体格式无关。runtime 不为此加对
`komo-agent` 的 dev-dependency。

# 14. Skills 同样如此

SkillRegistry 仍然留在 `komo-agent`，因为 Skills 本身属于 Agent capability。
**展示已经在 agent 里了**（`SkillRegistry::prompt_block`），Gateway 的 `skills_block` 只是
拼一个 `OfferContext` 再转手调用。

要做的只是把“读活注册表”和“渲染”在类型上分开：

```text
SkillRegistry（活的，RwLock 里）
    │  Gateway 在 I/O 阶段读一次
    ▼
SkillCatalog（值：按 OfferContext 门控后的条目 + 出了条目的根）
    │
    ▼
ContextInput
    │
    ▼
ContextAssembler（渲染成目录块）
```

这样：

```text
Registry = discovery

Assembler = presentation
```

互相不要混。`SkillCatalog` 是值，以后要钉住（§9 缺口 1）时，它就是被钉住的那一份。

具体改法：现在 `prompt_block` 内部是 `catalog_of(context)` + 拼根与目录行。拆成

```rust
impl SkillRegistry {
    pub fn offer(&self, context: &OfferContext) -> SkillCatalog;   // 扫目录、门控、定形状
    pub fn prompt_block(&self, context: &OfferContext) -> Option<String> {
        self.offer(context).prompt_block()                         // 保留，给别的调用方
    }
}
impl SkillCatalog {
    pub fn prompt_block(&self) -> Option<String>;                  // 纯渲染
}
```

`ResourceMounts.skills`（`skill://` 的挂载点）**不从这份目录来**：它用 `list()`，包括
被盖住、被 disable、门控不过的 skill——模型可以按名字读一个不在目录里的 skill。这是现有
行为，保持不变，挂载的读取留在 `segment.rs`。

---

# 15. AgentSurface 不要继续扩大职责

现在的 `AgentSurface` 是非常好的边界：

```text
哪些 Tool 对这个 Agent 可见/可执行
```

保持这个定义。

不要以后继续塞：

```rust
struct AgentSurface {
    tools: ...
    memory: ...
    prompt: ...
    skills: ...
    model: ...
}
```

否则它会变成另一个万能对象。

正确关系：

```text
AgentProfile
   │
   ├── surface
   ├── instructions
   ├── model
   ├── workspace
   └── memory scope

RunSnapshot
   │
   └── freeze above

AgentSurface
   │
   └── only capabilities
```

---

# 16. TurnRequest 最终在哪里构造

这一点我建议不要搬进 `komo-agent`。

`TurnRequest`（`komo-kernel/src/types/turn.rs`）除了 `system_prompt` / `messages` / `tools`，
还有 `session`、`run`、`model`、`memories`（审计用的 `MemoryUse`）和 `covers`。这些属于
Runtime 与 Ledger 的事。

顺序上有一个约束：**tool definitions 必须在 assemble 之前算好**，因为系统提示要列工具名，
skills 门控也按这组名字：

```text
executor.definitions_for(surface)
       │
       ├──────────────► tool names ──► ContextInput
       │                                   │
       │                                   ▼
       │                           ContextAssembler
       │                                   │
       │                                   ▼
       │                              AgentContext
       │                                   │
       └──────────────┬────────────────────┘
                      ▼
                 TurnRequest
```

`TurnRequest` 的组装继续留在 Gateway：

```rust
let tools = executor.definitions_for(&identity.surface);

let input = resolve_context_input(..., &tools).await?;

let context = komo_agent::context::assemble(input);

let request = TurnRequest {
    session,
    run,
    model,
    system_prompt: context.system_prompt,
    messages: context.messages,
    tools,
    memories,
    covers: None,
};
```

---

# 17. `segment.rs` 最终应该长什么样

`segment()` 产出的不只是 `TurnRequest`。它返回一个 `Segment`：

```text
Segment
├── request   TurnRequest            ← 本次重构只改它的 system_prompt / messages 来源
├── env       CallEnv                ← roots、ResourceMounts、surface、delegated、cancel
├── budget    Budget                 ← 子代理用 spec.rounds，其他按 source
├── resume    续跑的那些调用          ← resumed() 读账本
└── command   Cron 直跑命令
```

所以伪代码是：

```rust
async fn segment(&self, claimed, catalog) -> Result<Segment, HandlerError> {
    let (session, record, events, surface) = self.load(claimed).await?;

    let identity = self.identity_for(...).await?;          // context_sources.rs
    let delegate = delegate_of(&surface, &run);
    let identity = restrict_for(identity, &delegate);      // without_delegate

    let tools = self.tools_for(&identity.surface, catalog);

    let input = resolve_context_input(...).await?;         // context_sources.rs
    let context = komo_agent::context::assemble(input);

    let request = TurnRequest { .. };
    let env = self.call_env(..);
    let budget = self.budget_for(..);
    let resume = self.resumed(..).await?;
    let command = self.command_for(..).await;

    Ok(Segment { session, run, request, env, budget, resume, command })
}
```

它不应该再知道：

```text
Memory 怎么 format
Skill catalog 怎么裁剪
System Prompt 顺序
Subagent prompt 怎么写
历史 Surface 怎么变 ReplayMessage
instructions 文本怎么拼接
```

---

# 18. Runtime 不需要因为这次重构变化（Phase 1–4）

Phase 1–4 明确不动：

```text
AgentLoop
ToolExecutor
Policy
Approval
Tool concurrency
Coordinator
Recovery
Scheduler
Memory retrieval
LearningPass
Consolidator
LLM providers
```

Phase 5 只动两处：`MemoryManager` 的渲染函数改由 `MemoryParts` 传入，以及删掉适配器里的
`SystemPreamble` 钩子和它背后的 `selections` 表（§13.2）。召回、rerank、检索、钉住本身不动。

尤其不要趁机做：

```rust
trait AgentLoop
trait ContextAssembler
trait SkillRegistry
trait MemoryRetriever
```

当前仍然坚持 Komo 已有原则：

> 出现第二实现，再抽 abstraction。

---

# 19. 不引入 DeepSeek Harness 的 Plugin 模型

DeepSeek Harness 解决的是：

```text
通用 Agent Platform
任意 Loop
任意 Provider
任意 Tool Runtime
任意 Prompt Plugin
动态 Capability Composition
```

Komo 当前解决的是：

```text
单用户
长期运行
Durable
可恢复
受控副作用
多 Agent Profile
```

因此这次绝对不要增加：

```text
ContextPlugin
PromptPlugin
AgentPlugin
Waterfall
Hook Registry
Dynamic Service Container
```

（`SystemPreamble` 本身就是一个小小的 hook，Phase 5 删掉它，方向一致。）

如果未来真的出现：

```text
Coding Agent ContextAssembler

Research Agent ContextAssembler
```

都需要完全不同算法，再重新判断。

现在：

```text
InvocationContext
+
AgentProfile
+
ContextInput
```

足够表达差异。

---

## 19.1 不拆 AgentLoop：Claude 的缓存落在哪

DeepSeek Harness 把 Agent 与 AgentLoop 分开、Loop 可替换。我们考虑过照做，理由是“为
Claude 做更好的提示缓存”。结论是**不拆**，因为缓存命中率不由 loop 决定。

Anthropic 的缓存按前缀逐字节匹配（tools → system → messages），决定命中率的是：

| 因素 | 在哪一层 | 现状 |
|---|---|---|
| 前缀是否逐字节稳定 | 上下文装配（本次收进 agent） | skills 目录每段现渲染（§9 缺口 1）；历史 Run 折叠成“用户原话 + 最终回复”，工具往返在下一个 Run 里消失，前缀从上一句用户消息之后就变了 |
| 断点位置与 TTL | 适配器（`anthropic.rs`） | system、最后一个工具、最后一条 user 各一个断点，默认 5 分钟 TTL；审批后续跑时前缀多半已过期 |
| 记忆段位置 | Phase 5 后在 assembler | 系统提示末尾、同段逐字复用——已经是对的 |
| 能否观测命中率 | `TokenUsage` | 没有 `cache_read` / `cache_creation` 字段，量不出来 |

`AgentLoop` 做的是 `driver.next` → `record_round` → 执行工具 → Ask 挂起 → 按预算重试，
不影响请求体长什么样；而它承载的正是 §7、§8 那几条硬约束（`record_round` 先于
`complete`、Ask 让出名额、重启不重置预算）。多一个 loop 实现就要在两处把它们各做对一遍，
缓存问题却没有因此解决。与 provider 相关的差异已经有接口：`TurnDriver`（kernel 里的
trait），`AnthropicDriver` 自己维护 messages 与断点。

所以分工是：

```text
komo-agent        所有人依赖它：是谁、能做什么、看见什么。
                  “给 Claude 的上下文怎样保持前缀稳定”是看见什么的一部分，在 assemble 里。
komo-runtime      AgentLoop 只有一个，与模型无关。
TurnDriver        断点、TTL、协议细节，每个 provider 一份（已有）。
```

如果以后出现“另一种 loop 才做得到”的需求（不同的规划方式、交给 provider 自己的 tool
runner 驱动），那才是真正的第二实现，到时再抽 `Agent` trait（§18 的原则）。

缓存的具体改进排在收口之后（§23），先后顺序是：先加用量字段能看到命中率，再做 1 小时
TTL、按模型选择历史折叠策略、skills 目录钉住。

---

# 20. 推荐的迁移顺序

## Phase 0：先锁行为

`segment.rs` 里已经有一批测试，重构时跟着代码走：

```text
the_system_prompt_names_the_tools_that_are_actually_mounted
the_system_prompt_carries_the_skills_catalog_after_the_base_text
both_prompts_tell_the_model_to_search_with_rg
a_later_run_still_reads_what_the_earlier_one_said
a_replayed_round_names_the_artifacts_it_produced
the_running_run_keeps_its_whole_protocol
a_subagent_and_its_parent_are_two_different_conversations
an_externalized_message_comes_back_by_reference
a_payload_that_cannot_be_read_stops_the_segment
```

它们断言的是片段（“包含某句话”）。在这之上补**整份输出**的 golden：

```text
crates/komo-gateway/src/service/segment_golden.rs   segment 的子模块（能用它的私有函数）
crates/komo-gateway/src/service/golden/*.txt        基线文件
KOMO_UPDATE_GOLDEN=1 cargo test -p komo-gateway segment::golden   只在有意改提示时刷新
```

每个场景把“系统提示 + 记忆段（按适配器的方式 `\n\n` 追加）”和 `serde_json` 序列化的
`Vec<ReplayMessage>` 写成一个文件。skills 的临时目录替换成 `<SKILLS>`，工作目录用固定的
字面路径，时间戳固定，输出因此可复现。场景：

| 文件 | 覆盖 |
|---|---|
| `plain_main_agent` | 一句输入，无指令、skills、记忆 |
| `main_agent_without_tools` | “这一段没有工具” |
| `full_main_agent` | 身份指令（带首尾空白）、skills（一条被 `requires_tools` 门控掉）、三条记忆（三种来源与确认状态）、`/new` 之前的旧话、历史 Run 折叠、当前 Run 的输入在历史 Run 半轮中间落盘、还没被领走的 Run、历史 Run 中间一条**读不出来**的外置正文（不该被读）、外置的当前输入、截断成 Excerpt 的工具结果带产物、读不回 `output.json` 时退回账本预览的失败结果、`thinking` 原生块 |
| `subagent_free_text` | 子代理无契约，父的指令跟着走，只看得见自己那条 Run |
| `subagent_with_contract` | 子代理带 strict 契约，schema 原样进提示 |

Phase 0 的装配按现在 `segment()` 的写法组合现有函数；此后每个阶段只改测试里“怎么装配”
那一个函数，让它调新的生产入口，**场景和基线文件都不动**。Phase 3 之后它调的就是
`context_sources` + `komo_agent::context::assemble`，和 `segment()` 走同一条路。

tool definitions 不进基线：它们来自 `executor.definitions_for(surface)`，本次不碰那条路。

验证：

```text
重构前 == 重构后
```

这是整个迁移最重要的保障。

---

## Phase 1：只移动 Prompt

新增：

```text
komo-agent/src/context/prompt.rs
```

迁过去：

```text
system_prompt
subagent_prompt
with_instructions
RULES
```

Skills 块改为：Gateway 读注册表得到 `SkillCatalog`，agent 渲染。渲染代码本来就在
`komo-agent`，这里只是改调用路径。

Gateway 仍负责 history。记忆不动。

此阶段验证：

```text
所有 Context golden test 逐字相同
```

不要顺便“优化 Prompt”。

---

## Phase 2：移动 History 的纯逻辑

搬到 `komo-agent/src/context/history.rs`：

```text
window + finals 判定 + 按 Run 分组  →  entries(surface, scope)
latest_user_text
ResolvedMessage → ReplayMessage（调 kernel::projection::project；含退回账本 preview 的规则）
provider_call_id / tool_name / tool_call 这几个 Surface 查询
```

留在 Gateway（`context_sources.rs`）：

```text
message_text（PayloadStore）
ToolOutputStore::open 取 preview 与 artifacts
halt_if_corrupt
```

验证重点：

```text
tool call/result pairing
provider_call_id 回退到内部 call id
历史 Run 只留用户正文与最后一条有正文的回复
cancelled attempt
corrupt payload（窗口内停下，窗口外不读）
Run 之间先来后到
```

---

## Phase 3：引入 ContextInput / ContextAssembler

把：

```text
identity（含指令正文）
tool names
history
memory（此时是已渲染文本或空）
skills
invocation
```

统一进 `ContextInput`。

Gateway 从：

```text
“自己拼所有东西”
```

变成：

```text
“准备事实”
```

Agent 从：

```text
“几个 helper”
```

变成：

```text
“唯一 Context Assembly 入口”
```

---

## Phase 4：瘦身 `segment.rs`

I/O 助手搬进 `context_sources.rs`，`segment()` 只剩 §17 的形状。

删除 Gateway 中所有：

```text
Prompt formatting
Skill 门控之外的 Skill 处理
History/model conversion
```

最终目标：

> `segment.rs` 只描述一次 Run Segment 的生命周期。

---

## Phase 5：Memory 展示进 agent（动 runtime）

按 §13.2 做：`Injection` 挪进 kernel，`render_injection` 搬成
`komo_agent::context::memory::render`，`MemoryParts` 加 `render` 字段，删掉
`SystemPreamble` 那一整条路。

验证：

```text
golden 里 full_main_agent 的记忆段逐字不变（此时它已经在 TurnRequest.system_prompt 里）
memory 测试：同一段逐字复用、遗忘立刻生效、换段重算，照旧通过
新增集成测试：开着记忆时 FakeLlm 收到的 system_prompt 以记忆段结尾
rg SystemPreamble / injection_for / selections 在 crates/ 里没有结果
```

---

# 21. 验收标准

我建议把成功标准写死。

### 架构

```text
komo-agent 不依赖 store/runtime
```

必须成立。

```text
Gateway 不包含 System Prompt 文本
```

必须成立（Phase 1 后）。

```text
Gateway 不决定 Memory 如何展示
```

现在已经成立（展示在 runtime）。Phase 5 后变成“展示在 agent”。

```text
Gateway 不决定 Skill 如何展示
```

现在渲染已在 agent；Phase 1 后 Gateway 只负责读注册表。

```text
Agent Context 不直接访问 Ledger、PayloadStore、ToolOutputStore
```

必须成立。

```text
工具结果正文仍只经 kernel::projection 一处渲染
```

必须成立。

```text
komo-agent 的依赖变化已写进 docs/komo_bot.md §13.4
```

有变化时必须成立。`komo-agent` 的职责描述（§13.4 那一行、AGENTS.md 的 Crate layout）
补上“上下文装配”。

```text
TurnRequest.system_prompt == 实际发出去的系统提示
```

Phase 5 后必须成立：`crates/` 里搜不到 `SystemPreamble`、`injection_for`、`selections`。

---

### 行为

同样的：

```text
RunSnapshot
Session Ledger
Pinned Memory
Skill Catalog
```

必须生成同样：

```text
Model Request
```

---

### 代码形态

`segment()` 应主要只剩：

```text
load
resolve
assemble
invoke（产出 request / env / budget / resume / command）
```

而不是：

```text
format
filter
truncate
render
project
拼字符串
```

行数按实际能搬走的量估：非测试部分约 1300 行里，prompt 相关约 80 行、history 相关约
280 行搬到 agent，I/O 助手（`identity_for`、`recall_for`、`resumed`、`call_request`、
`message_text` 等）约 400 行搬到 `context_sources.rs`。对应的测试跟着代码走。
“降到原来的 1/3 以下”只有在 I/O 助手也搬出去时才做得到，所以衡量标准定为
**`segment()` 函数体只剩 §17 的形状**，不定行数。

---

# 22. 重构后的完整数据流

最终建议的数据流：

```text
                     AgentProfile
                          │
                          ▼
                       Session
                          │
                          ▼
                     RunSnapshot
                          │
             ┌────────────┴────────────┐
             │                         │
             ▼                         ▼
       Durable Sources            AgentSurface
             │                         │
     ┌───────┼────────┐                ▼
     │       │        │     ToolExecutor.definitions_for()
 history   memory   skills             │
     │       │        │                │ tool names
     └───────┼────────┘                │
             ▼                         │
  context_sources.rs (Gateway) ◄───────┤
             │                         │
             ▼                         │
         ContextInput                  │
             │                         │
             ▼                         │
        komo-agent                     │
     ContextAssembler                  │
             │                         │
             ▼                         │
        AgentContext                   │
      /              \                 │
system prompt       messages           │
      \              /                 │
       └──────┬─────┘                  │
              │                        │ tool definitions
              └────────────┬───────────┘
                           ▼
                      TurnRequest
                           │
                           ▼
                       AgentLoop
                           │
                           ▼
                      ToolExecutor
                           │
                           ▼
                         Ledger
```

这里最关键的是三个边界：

```text
Durable State
     ↓
ContextInput
```

这是 I/O → Agent Semantic 的边界。

```text
ContextInput
     ↓
AgentContext
```

这是 Agent Context Assembly 边界。

```text
AgentContext + tool definitions
     ↓
TurnRequest
```

这是 Agent → Runtime 的边界。

---

# 23. 完成这一步以后再做什么

完成 Context 收口之后，下一步才应该处理：

```text
Multi-Agent Product Surface
```

因为底层已经支持：

```text
AgentProfile
Session.agent_id
RunSnapshot
AgentSurface
delegate
```

缺的是：

```text
GET /v1/agents

POST /v1/sessions
{
  "agent": "coder"
}

TUI agent picker

peer/channel → agent routing
```

但我建议把它和本次重构分开。

原因是：

> **Context 收口是内部架构整理；Multi-Agent routing 是产品行为变化。**

两件事混在一次改动里，会大幅增加回归面。

§9 的两个缺口（skills 目录钉住、记忆段对 `TurnRequest` 可见）也不在收口的 Phase 1–4 里。
后者由 Phase 5 解决；前者是行为变化，单独做。

Claude 提示缓存（§19.1），按这个顺序，每一步都等上一步的数据：

```text
1. TokenUsage 加 cache_read / cache_creation（只加字段），Anthropic 适配器从 usage 里读出来
2. system 与 tools 的断点加 1 小时 TTL：对应“审批后再续跑”；写缓存更贵，用第 1 步的数据权衡
3. assemble 按模型选择历史策略：对 Claude 保留历史 Run 的完整协议（含 provider_blocks），
   前缀一直稳定，代价是上下文更长；其他模型维持现在的折叠
4. skills 目录进 RunSnapshot 或像记忆那样按段钉住（同时补上 §9 缺口 1）
```

---

# 24. 最终目标

完成以后，Komo 的几个核心问题应该有非常明确的答案：

```text
“这个 Agent 是谁？”
        ↓
AgentProfile / RunSnapshot


“这个 Agent 能做什么？”
        ↓
AgentSurface


“这个 Agent 当前看见什么？”
        ↓
komo-agent::ContextAssembler


“这些事实从哪里来？”
        ↓
komo-gateway::context_sources


“模型怎么运行？”
        ↓
komo-runtime::AgentLoop


“Tool 怎么执行？”
        ↓
komo-runtime::ToolExecutor


“状态如何恢复？”
        ↓
Ledger + Recovery
```

到这里，`Agent / Runtime / Gateway / Ledger` 四层的边界才算真正闭合。

---

## 推荐最终判断

这次不要继续“大重构”。

最值得做的就是一个非常克制的变化：

```text
segment.rs + memory/preamble.rs

        Context Assembly
              │
              ▼
          komo-agent
```

其余架构基本保持原样。

Komo 当前已经不缺新的抽象，缺的是：

> **让已有抽象真正拥有它应该拥有的职责。**

这是这次“收口”最核心的目标。
