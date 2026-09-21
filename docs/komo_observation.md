# Komo Observation / Artifact 与 Context Projection 设计

> Status: Proposal —— **一半已落地（2026-09-21）**，见下面「落地情况」
> Scope: Tool Result、Blob/Artifact、Context Projection、Observation Recall
> Inspired by: SoL-Pi ObservationPack / Evidence-Preserving Reducer
> Target: Komo first production-ready harness

---

## 落地情况（2026-09-21）

这份提案里**「事实与视图分离」**那一半已经落地，落在 `docs/komo_bot.md` §8.3 与代码里。
另一半没有照它做，理由逐条写在表格里——**不要**按本文的 crate 名（`komo-core` /
`komo-session` / `komo-tools`）去对上，仓库里的划分是 `kernel` / `store` / `runtime` /
`gateway`。

| 提案里的东西 | 落地成什么 |
|---|---|
| Observation | `tool.result` 事件 + `ToolResultBody`：事实层本来就在，改名不增值 |
| Artifact / ArtifactStore | `output.json` + `stdout.txt` / `stderr.txt`；`ToolOutputStore` + `PayloadStore` 就是那个 store（`artifacts/` 仍是空目录，目前没有生产者） |
| 「完整保存、不 truncate」 | 已经如此：进程输出上限只停转发、不杀进程 |
| ObservationView（Full / Excerpt） | `komo-kernel/src/projection.rs` 一处纯函数：抬头 + 头尾各留一段 + 省略量 + **可直接 `read` 的完整输出路径**；"刚跑完"与"回放"逐字节相同 |
| §15 的 Projection Policy（阈值） | `[execution] model_result_bytes`（默认 8 KiB，`ConfigSnapshot` 热生效）；账本里那 ≤1 KiB 是**行预算**，两件事分开 |
| §17 的 `observation_read` | **不新增工具**（§4 不新增第七个）：§8.3 早就把恢复路径定为 `read`，这次补的是让 `tool-output/` / `artifacts/` 成为**只读根** |
| §14 Handle 视图 / 按请求投影 | **未做**：要等真实会话的 recall 次数（§49）——模型自己写的那段正文够用时，"哪次请求用哪个视图"是过度设计 |
| §25 / §26 Context Compact | **未做**，而且提案里的前提不成立：komo 目前没有 compaction 这一层 |
| §40 并行工具调用 | **未做**：§6 明确首版顺序执行同一轮的多个调用（减少文件操作顺序歧义） |
| §41 Action Fusion、§43 Reducer、§47 指标 | **未做**，也还没到做的时候（Reducer 要模型 + 验证 + 成本记账，收益未测量） |

---

## 1. 结论

Komo 应该把当前：

```text
Tool
 ↓
ToolCallOutcome
 ↓
大结果截断 / 落 blob
 ↓
直接进入 LLM context
```

改造成：

```text
Tool
 ↓
完整 Observation
 ↓
持久化 Event Log + Artifact Store
 ↓
Context Projector
 ↓
适合当前模型请求的 Observation View
```

核心原则是：

> **Tool 执行结果是事实，LLM Context 只是事实的一种投影。**

因此：

- ToolRunner 不负责决定模型最终看到多少内容。
- 大输出不应该因为 context 限制而被永久截断。
- Event Log 保存完整事实或完整事实的可恢复引用。
- ContextAssembler / ContextProjector 决定每次 provider request 使用：

  - Full
  - Excerpt
  - Handle

- Agent 可以通过 `observation_read` 按需恢复原始内容。
- Observation 与 Artifact 是基础能力，后续 `rg`、并发 tool、Action Fusion、Context Compact、Reducer 都构建在这层之上。

第一阶段不追求复杂压缩算法。

优先解决：

```text
完整保存
   +
便宜投影
   +
按需恢复
```

---

# 2. 为什么现在要做

Komo 接下来会增加：

```text
rg
shell_exec
read_file
web_fetch
并行 tool calls
```

这些工具都可能产生大量输出。

例如：

```bash
rg "ToolRunner" .
```

可能产生：

```text
80 KB
```

如果直接进入 session message history：

```text
turn 1: 80 KB

turn 2:
重新发送 80 KB

turn 3:
重新发送 80 KB

turn 10:
仍然重新发送 80 KB
```

实际 agent 已经不一定需要这些内容，但 provider request 仍然持续支付：

```text
token cost
+
context space
+
KV/cache cost
+
模型 attention
```

当前常见的解决方法是：

```text
超过 N KB
↓
truncate
```

但 truncate 存在一个根本问题：

> 它把“context optimization”变成了“information destruction”。

例如：

```text
cargo test
```

输出：

```text
前 20 KB
...
真正错误在第 43 KB
...
后 10 KB
```

如果 ToolRunner 截掉中间内容，之后 agent 无法恢复。

所以 Komo 应该把：

```text
truncate
```

改成：

```text
store + project
```

---

# 3. 第一原则

## 3.1 Execution Truth 与 Model Context 分离

ToolRunner 的职责是：

```text
执行工具
↓
记录真实结果
```

而不是：

```text
执行工具
↓
猜模型以后需要哪些内容
```

因此应明确区分：

```text
Observation
```

和：

```text
ObservationView
```

其中：

```text
Observation
= 真实执行结果

ObservationView
= 当前 provider request 中展示给模型的内容
```

一个 Observation 可以产生不同 View：

```text
Observation #123
     │
     ├─ request 1 → Full
     │
     ├─ request 2 → Full
     │
     ├─ request 3 → Excerpt
     │
     └─ request 8 → HandleOnly
```

Observation 本身不发生变化。

---

# 4. 核心模型

建议把概念分成三层：

```text
Observation
Artifact
ObservationView
```

---

## 4.1 Observation

Observation 表示一次 Tool Call 得到的逻辑结果。

示意：

```rust
pub struct Observation {
    pub id: ObservationId,

    pub tool_call_id: ToolCallId,

    pub status: ObservationStatus,

    pub content: ObservationContent,

    pub metadata: ObservationMetadata,
}
```

其中：

```rust
pub enum ObservationStatus {
    Success,
    Error,
    Cancelled,
    TimedOut,
}
```

---

# 5. ObservationContent

最重要的是不要简单设计成：

```rust
String
```

建议：

```rust
pub enum ObservationContent {
    Inline {
        text: String,
    },

    Artifact {
        artifact: ArtifactRef,
        preview: Option<String>,
    },

    InlineAndArtifact {
        text: String,
        artifact: ArtifactRef,
    },
}
```

其中语义为：

### Inline

结果很小：

```text
pwd
→ /home/yi/komo
```

直接保存。

### Artifact

结果很大：

```text
rg
cargo test
large HTTP response
```

完整正文放 Artifact Store。

Observation 中保存引用。

### InlineAndArtifact

模型当前需要一个 preview：

```text
前 4 KB
...
后 4 KB
```

同时保留完整 artifact。

---

# 6. Artifact

Artifact 不是 Observation。

Observation 是：

> “某个 tool call 产生了什么结果。”

Artifact 是：

> “一块可以被持久化和再次读取的数据。”

例如：

```text
ToolCall #42
    ↓
Observation #88
    ↓
Artifact blob: sha256:abc123
```

建议：

```rust
pub struct ArtifactRef {
    pub id: ArtifactId,

    pub media_type: String,

    pub byte_len: u64,

    pub sha256: String,

    pub storage: ArtifactStorage,
}
```

第一版：

```rust
pub enum ArtifactStorage {
    Blob {
        path: String,
    },
}
```

未来可以扩展：

```text
object storage
database
remote URL
compressed blob
```

但第一版不需要。

---

# 7. 为什么 Observation 和 Artifact 要分开

因为未来会出现：

```text
Observation
 ├─ stdout artifact
 ├─ stderr artifact
 ├─ generated file
 └─ structured metadata
```

比如：

```bash
cargo test
```

逻辑结果可能是：

```text
exit_code = 101
duration = 15s
stdout = artifact A
stderr = artifact B
```

所以长期来看更合理的是：

```rust
pub struct Observation {
    pub id: ObservationId,

    pub tool_call_id: ToolCallId,

    pub status: ObservationStatus,

    pub summary: Option<String>,

    pub artifacts: Vec<ArtifactRef>,

    pub inline: Option<String>,

    pub metadata: ObservationMetadata,
}
```

---

# 8. Event Log

Event Log 必须保存 Execution Truth。

例如：

```json
{
  "type": "ToolCallOutcome",
  "seq": 101,
  "tool_call_id": "call_01",
  "observation_id": "obs_01",
  "status": "success",
  "inline": null,
  "artifacts": [
    {
      "id": "artifact_01",
      "media_type": "text/plain",
      "byte_len": 82341,
      "sha256": "..."
    }
  ]
}
```

重要原则：

> Event Log 中不能只保存已经 truncate 的字符串。

否则 Observation Recall 无法恢复真实信息。

---

# 9. Artifact Store

当前 Komo 已经计划：

```text
data/
  sessions/
  blobs/
```

可以直接利用。

例如：

```text
data/
  blobs/
    ab/
      abcdef123456...
```

推荐使用 content-addressed storage：

```text
sha256(content)
```

作为底层存储 key。

好处：

```text
天然去重
+
完整性验证
+
事件恢复简单
```

逻辑 Artifact ID 不一定等于 SHA：

```text
artifact_id
    ↓
sha256
```

但第一版甚至可以直接使用 sha256。

---

# 10. Context Projection

这是整个设计真正重要的部分。

现有：

```text
Event Log
    ↓
ContextAssembler
    ↓
Messages
```

建议改成：

```text
Event Log
    ↓
Conversation Reconstruction
    ↓
Context Projector
    ↓
Provider Messages
```

Context Projector 不修改历史。

它只针对：

```text
本次 provider request
```

产生 context view。

---

# 11. ObservationView

建议：

```rust
pub enum ObservationView {
    Full,

    Excerpt {
        head_bytes: usize,
        tail_bytes: usize,
    },

    Handle,

    Omit,
}
```

第一版甚至只需要：

```text
Full
Excerpt
Handle
```

---

# 12. Full View

模型看到：

```text
Tool: rg

src/foo.rs:...
src/bar.rs:...
...
```

适合：

```text
小结果
刚产生的结果
agent 明确 recall 的内容
```

---

# 13. Excerpt View

例如：

```text
[Observation obs_123]
Size: 82.3 KB

--- BEGINNING ---

...

--- omitted 72 KB ---

--- END ---

...

Use observation_read(id="obs_123") to inspect more.
```

注意：

> omitted 不等于 lost。

---

# 14. Handle View

更老、更不重要的 Observation：

```text
[Observation obs_123]

Tool: rg
Status: success
Size: 82.3 KB
Artifact: artifact_abc

Content is not included in the current context.

Use observation_read:
  observation_read(id="obs_123")
```

这样一个原本：

```text
80 KB
```

的 tool result，可以降到：

```text
几十～几百 token
```

---

# 15. Projection Policy

第一版不要做复杂模型。

直接 deterministic。

例如：

```text
result <= 8 KB
    ↓
Full
```

```text
result > 8 KB
    ↓
Artifact
```

对于大 Observation：

```text
刚产生后的前 2 次 model request
    ↓
Excerpt

之后
    ↓
Handle
```

这里可以借鉴 SoL-Pi 的思想，但阈值不要视为固定设计。

推荐：

```rust
pub trait ObservationProjectionPolicy {
    fn project(
        &self,
        observation: &Observation,
        context: &ProjectionContext,
    ) -> ObservationView;
}
```

第一版：

```text
DefaultProjectionPolicy
```

即可。

不要一开始搞插件化 policy engine。

---

# 16. 为什么第一次也不一定需要 Full

对于：

```text
rg
```

80 KB 输出，即使第一次全部塞给模型也可能没有意义。

因此建议：

```text
小结果
→ Full

大结果
→ Excerpt
```

而不是：

```text
第一次无脑 Full
```

这里和 SoL-Pi 可以稍微不同。

因为 Komo 从一开始就知道：

```text
Artifact
```

的存在。

---

# 17. observation_read Tool

需要增加：

```text
observation_read
```

而不是只提供：

```text
read_blob
```

因为 Agent 应该面对语义层：

```text
Observation
```

而不是存储实现：

```text
Blob
```

建议 API：

```json
{
  "id": "obs_123",
  "offset": 0,
  "limit": 16384
}
```

返回：

```text
Observation obs_123
Bytes 0-16384 / 82341

...
```

支持：

```text
offset
limit
```

第一版足够。

未来再增加：

```text
line_start
line_end
search
```

---

# 18. observation_read 本身也是 Tool

调用：

```text
LLM
 ↓
observation_read
 ↓
ToolCallIntent
 ↓
ToolCallOutcome
```

这样它自然进入：

```text
Event Log
Policy
Tracing
Cancellation
```

无需特殊旁路。

---

# 19. 与 rg Tool 的关系

Observation 模型确定之后，`rg` 就非常简单。

```text
rg
 ↓
native process
 ↓
stdout
 ↓
Observation
```

不需要 `rg` 自己设计：

```text
最多返回 100 lines
超过后截断
```

它只需要提供合理的执行级限制，例如：

```text
timeout
max process output
```

这里的：

```text
max process output
```

应该理解为：

> 防止机器资源耗尽。

而不是：

> 防止 LLM context 太大。

这是两个完全不同的问题。

---

# 20. Resource Safety 与 Context Safety 分离

必须区分：

## Runtime limit

例如：

```text
最大读取 100 MB
进程 timeout 120s
```

解决：

```text
OOM
磁盘爆炸
恶意工具
无限输出
```

## Context limit

例如：

```text
80 KB result
↓
只投影 8 KB
```

解决：

```text
token
attention
provider limit
cost
```

不要把两者混在：

```text
truncate result to 8 KB
```

里。

---

# 21. 与并行 Tool Calls 的关系

这个模型天然支持并行。

例如模型一次产生：

```text
rg "ToolRunner"
rg "ContextAssembler"
read Cargo.toml
```

执行：

```text
            ┌─ rg A ──→ Observation A
Model ──────┼─ rg B ──→ Observation B
            └─ read ──→ Observation C
```

join 后：

```text
A = 60 KB → Excerpt
B = 40 KB → Excerpt
C = 3 KB  → Full
```

Context Projector 再组合：

```text
A excerpt
B excerpt
C full
```

ToolRunner 不需要为并发专门考虑 context。

这正是拆层的好处。

---

# 22. Tool 并发仍由 Execution Layer 负责

不要让 Context Projection 知道：

```text
parallel
serial
```

Execution：

```text
ToolCall
 ↓
Scheduler
 ↓
ToolRunner
 ↓
Observation
```

Projection：

```text
Observation[]
 ↓
ContextProjector
 ↓
Model messages
```

两个系统可以独立演进。

---

# 23. 与 Action Fusion 的关系

未来可能支持：

```json
{
  "tool": "write_file",
  "path": "...",
  "content": "...",
  "then_run": {
    "tool": "shell_exec",
    "command": "cargo test"
  }
}
```

但底层仍然产生：

```text
Mutation Observation
Validation Observation
```

或者一个 Composite Observation：

```text
Action
 ├─ mutate
 └─ validate
```

Observation 层不应该为 Action Fusion 做特殊 hack。

---

# 24. Action Fusion 不应该和 Parallel 混起来

两者语义不同。

## Parallel

```text
A
B
C
```

没有依赖。

可以：

```text
A ─┐
B ─┼→ join
C ─┘
```

## Fusion

```text
write
 ↓
cargo test
```

具有严格 dependency。

因此未来 execution planner 可以表达：

```text
serial dependencies
+
parallel siblings
```

但第一版不需要 DAG scheduler。

---

# 25. 与 Context Compact 的关系

Observation Projection 和 Context Compact 不应该是同一个机制。

Projection：

```text
减少 Tool Result 重放
```

Compact：

```text
压缩历史 reasoning / conversation
```

顺序应该是：

```text
Event History
    ↓
Observation Projection
    ↓
估算 context size
    ↓
如果仍然过大
    ↓
Compaction
```

因为很多时候只是几个大 `rg` / shell output 造成 context 膨胀。

Projection 后可能已经无需 compact。

---

# 26. 对现有 100k Compact 策略的影响

现有设计：

```text
> 100k
↓
summary oldest 60%
```

可以保留作为第一阶段 hard trigger。

但建议未来变成：

```text
             Context Pressure
                    │
          ┌─────────┴──────────┐
          │                    │
Observation Projection    Conversation Compact
          │                    │
 cheap/reversible       expensive/lossy-ish
```

因此优先：

```text
Projection
```

再：

```text
Compaction
```

---

# 27. Observation Projection 是 Reversible 的

这是它最大的价值之一。

```text
Full
↓
Excerpt
↓
Handle
```

信息仍然存在。

Agent 可以：

```text
observation_read
```

恢复。

而 summary：

```text
original conversation
↓
summary
```

通常无法完整恢复。

所以：

> reversible optimization 应该优先于 lossy optimization。

---

# 28. Evidence-Preserving Reducer 放到后面

未来可以增加：

```text
Artifact
 ↓
cheap model
 ↓
structured receipt
```

例如：

```json
{
  "summary": "cargo test failed",
  "evidence": ["error[E0308]: mismatched types", "src/main.rs:42"]
}
```

Harness 再验证：

```text
evidence
```

确实存在于原 artifact。

如果不存在：

```text
discard reducer result
```

但这一阶段不应该现在做。

原因：

```text
Observation Handle 已经解决主要 token 问题。
```

Reducer 增加：

```text
模型依赖
验证逻辑
异步调用
成本 accounting
失败路径
```

收益暂时不值得复杂度。

---

# 29. ToolRunner 修改建议

当前逻辑大致：

```text
resolve
↓
validate
↓
policy
↓
ToolCallIntent
↓
execute
↓
truncate / blob
↓
ToolCallOutcome
```

建议变成：

```text
resolve
↓
schema validate
↓
policy
↓
ToolCallIntent fsync
↓
execute
↓
normalize raw output
↓
ObservationBuilder
↓
ArtifactStore
↓
ToolCallOutcome fsync
```

其中 ToolRunner 不再负责：

```text
给模型展示什么
```

---

# 30. ObservationBuilder

可以增加一个非常薄的内部组件：

```rust
pub struct ObservationBuilder {
    artifact_store: Arc<dyn ArtifactStore>,
    inline_threshold: usize,
}
```

行为：

```text
<= threshold
    ↓
Inline

> threshold
    ↓
Artifact + preview
```

这里是存储策略。

不是 context policy。

---

# 31. ArtifactStore Trait

建议放在 `komo-core`：

```rust
#[async_trait]
pub trait ArtifactStore: Send + Sync {
    async fn put(
        &self,
        content: Bytes,
        media_type: &str,
    ) -> Result<ArtifactRef>;

    async fn read(
        &self,
        artifact: &ArtifactRef,
        range: Option<ByteRange>,
    ) -> Result<Bytes>;
}
```

具体实现：

```text
komo-session
或
komo-tools
```

需要根据目前 crate 职责最后决定。

但接口必须在 core。

---

# 32. 更推荐放到哪个 crate

如果遵守 Komo：

> lib crate 之间只依赖 `komo-core`

那么比较自然的是：

```text
komo-core
  ArtifactStore trait
  Observation types

komo-session
  LocalArtifactStore
  Event Log
  checkpoint

komo-tools
  ObservationBuilder / ToolRunner

komo-agent
  ContextProjector
  observation_read wiring

komo
  dependency injection
```

但有一个问题：

```text
komo-tools
```

不能依赖：

```text
komo-session
```

所以在组装时注入：

```rust
Arc<dyn ArtifactStore>
```

即可。

---

# 33. 推荐的数据流

最终：

```text
                         ┌───────────────────┐
                         │     Agent Loop    │
                         └─────────┬─────────┘
                                   │
                            Model Tool Call
                                   │
                                   ▼
                          ┌────────────────┐
                          │   ToolRunner   │
                          └───────┬────────┘
                                  │
                                execute
                                  │
                                  ▼
                           Raw Tool Output
                                  │
                                  ▼
                      ┌─────────────────────┐
                      │ Observation Builder │
                      └─────────┬───────────┘
                                │
                    ┌───────────┴────────────┐
                    │                        │
                  small                    large
                    │                        │
                  inline              ArtifactStore
                    │                        │
                    └───────────┬────────────┘
                                │
                          Observation
                                │
                          Event Log fsync
                                │
                                ▼
                     ┌─────────────────────┐
                     │  Context Projector  │
                     └─────────┬───────────┘
                               │
                        Full / Excerpt /
                            Handle
                               │
                               ▼
                             Model
```

---

# 34. Session Recovery

Artifact 必须兼容现有 crash recovery。

因为：

```text
ToolCallIntent fsync
↓
execute
↓
artifact write
↓
ToolCallOutcome fsync
```

可能在任意位置 crash。

建议顺序：

```text
ToolCallIntent fsync
↓
execute
↓
ArtifactStore.put
↓
ToolCallOutcome fsync
```

如果：

```text
artifact 已写
但 Outcome 未写
```

则只是产生 orphan artifact。

可以接受。

后续 GC 即可。

绝不能：

```text
Outcome 已记录 artifact
但 artifact 还没持久化
```

否则 recovery 会引用不存在的数据。

---

# 35. Blob GC

第一版甚至不需要实时 GC。

未来：

```text
扫描 Event Log
↓
收集 referenced artifacts
↓
删除未引用 artifact
```

即可。

content-addressed store 也使 GC 很容易。

---

# 36. Observation ID 与 Tool Call ID

不要把两者混为一个。

```text
ToolCallId
```

表示：

```text
模型请求执行一个 action
```

ObservationId：

```text
该 action 产生的 observation
```

现在可能：

```text
1 ToolCall → 1 Observation
```

未来可能：

```text
1 ToolCall
 ├─ stdout observation
 ├─ generated file observation
 └─ validation observation
```

所以 ID 分开值得。

---

# 37. Structured Tool Result

长期来看不应该所有工具都返回：

```text
String
```

例如：

```rust
pub struct ToolOutput {
    pub text: Option<String>,

    pub data: Option<serde_json::Value>,

    pub artifacts: Vec<ProducedArtifact>,

    pub metadata: ToolOutputMetadata,
}
```

但第一阶段不要同时重构所有 Tool API。

可以先：

```text
String
↓
ObservationBuilder
```

之后再扩展。

---

# 38. 第一阶段最小实现

建议只做以下内容。

## Core

新增：

```text
ObservationId
ArtifactId
ArtifactRef
ArtifactStore
Observation
```

## Session

实现：

```text
LocalArtifactStore
```

## Tools

ToolRunner：

```text
large output
↓
ArtifactStore
```

不再永久 truncate。

## Agent

实现：

```text
ContextProjector
```

## Built-in Tool

新增：

```text
observation_read
```

就够了。

---

# 39. 第一阶段明确不做

为了控制 scope，暂时不做：

```text
LLM reducer
semantic summary
vector retrieval
artifact compression
artifact encryption
DAG tool scheduler
generic extension framework
dynamic projection plugins
line index
artifact search index
Action Fusion
```

这些都不是当前主要矛盾。

当前主要矛盾是：

> **大 Tool Result 同时承担了“事实存储”和“模型上下文”两个职责。**

先把这两个职责拆开。

---

# 40. 第二阶段：Parallel Tool Calls

Observation 层稳定后：

```text
Model
↓
N Tool Calls
↓
classify dependency
↓
independent calls parallel
↓
Vec<Observation>
↓
Context Projector
```

第一版甚至不需要 dependency analyzer。

对于模型同一次 response 返回的多个 tool calls：

```text
默认视为 independent
```

如果 provider/tool semantics 明确允许，即可并行。

具有显式 mutation dependency 的操作后面再处理。

---

# 41. 第三阶段：Action Fusion

支持：

```text
mutation + validation
```

例如：

```text
write_file
then_run cargo test
```

减少：

```text
mutation
↓
model
↓
validation
```

中间一次 model round-trip。

但必须限制 Fusion 范围。

推荐最开始只允许：

```text
write/edit
+
read-only validation command
```

不要做任意：

```text
tool A
then tool B
then tool C
```

否则很快演变成 workflow engine。

---

# 42. 第四阶段：Semantic Context Compact

Context Compact 应从：

```text
token threshold only
```

发展到：

```text
hard pressure
+
semantic boundary
+
economic decision
```

Hard：

```text
快达到 context window
→ 必须 compact
```

Soft：

```text
subtask completed
↓
预计还有很多 turns
↓
compact 能降低未来 replay 成本
↓
compact
```

这里可以再引入：

```text
Plan / Task State
```

作为 semantic boundary。

---

# 43. 第五阶段：Evidence-Preserving Reducer

用于：

```text
超大 log
编译日志
测试日志
web 页面
```

流程：

```text
Artifact
↓
cheap reducer
↓
summary + evidence
↓
deterministic evidence verification
↓
Observation View
```

原则：

> 模型可以提出压缩结果，但程序验证其 provenance。

---

# 44. ContextProjector 接口

建议第一版：

```rust
pub trait ContextProjector: Send + Sync {
    fn project(
        &self,
        history: &[SessionEvent],
        context: &ProjectionContext,
    ) -> Result<Vec<ModelMessage>>;
}
```

但这里要注意：

ContextProjector 不应承担：

```text
history reconstruction
```

长期更干净的结构是：

```text
Session Events
↓
ConversationState
↓
ContextProjector
↓
ModelMessage[]
```

所以可以最终设计成：

```rust
fn project(
    &self,
    conversation: &ConversationState,
    context: &ProjectionContext,
) -> Result<ProjectedContext>;
```

---

# 45. ProjectionContext

未来 policy 需要知道：

```rust
pub struct ProjectionContext {
    pub provider: String,

    pub model: String,

    pub context_window: usize,

    pub request_index: u64,

    pub token_budget: usize,
}
```

第一版不需要全部精确。

可以只有：

```text
request_index
```

和：

```text
budget
```

---

# 46. Projection 应该保持 deterministic

相同：

```text
ConversationState
ProjectionContext
```

最好产生相同：

```text
ProjectedContext
```

这样：

```text
debug
replay
benchmark
```

会简单很多。

不要一开始让 LLM 参与 Context Projection。

---

# 47. Logging / Observability

应该记录：

```text
ObservationCreated
ArtifactStored
ObservationProjected
ObservationRecalled
```

但不一定都进入 durable Session Event。

区分：

## Durable events

```text
ToolCallIntent
ToolCallOutcome
```

## Trace events

```text
Projected Full
Projected Excerpt
Projected Handle
```

Context projection 更适合 tracing，而不是污染事件历史。

---

# 48. 指标

至少记录：

```text
tool_output_bytes
artifact_bytes
projected_bytes
observation_recall_count
```

后面可以算：

```text
projection_ratio
=
projected bytes / original bytes
```

以及：

```text
recall rate
=
recalled observations / packed observations
```

如果：

```text
recall rate 很高
```

说明投影过于激进。

如果：

```text
recall rate 很低
```

说明节省有效。

---

# 49. Benchmark

不要只测 token。

建议至少测：

```text
任务成功率
provider input tokens
tool call 次数
model request 次数
observation recall 次数
总 latency
```

例如固定几个 coding task：

```text
查找 bug
修改
运行 test
继续修改
```

对比：

```text
baseline
vs
Observation Projection
```

目标不是单纯：

```text
token 越低越好
```

而是：

```text
能力基本不下降
+
token 明显下降
```

---

# 50. Failure Cases

## Artifact 丢失

```text
Observation exists
artifact missing
```

`observation_read` 返回明确错误：

```text
artifact unavailable
```

不要 panic。

## Invalid offset

正常参数错误。

## Binary content

不要默认 UTF-8。

Artifact 记录：

```text
media_type
```

只有：

```text
text/*
application/json
```

等才允许作为普通 Observation Text 投影。

## Huge binary

只返回 metadata。

---

# 51. Secret / Sensitive Output

Observation Store 与 Context Projection 分离之后，还有一个额外好处：

可以在 Projection 层增加：

```text
redaction
```

例如：

```text
Tool raw result
↓
Artifact Store
↓
Context projector
↓
secret filtering
↓
Provider
```

但这里要非常谨慎。

Artifact Store 是否保存 secret 属于另一套 security policy。

第一版不要顺手解决。

---

# 52. 对 Komo Core Architecture 的影响

最终 Komo 的职责分层会更明确：

```text
komo-core
    contracts

komo-session
    truth / durability

komo-tools
    execution

komo-agent
    reasoning + context projection

komo-policy
    permission

komo-memory
    long-term semantic memory

komo-gateway
    channels
```

其中：

```text
Observation
```

属于：

```text
execution truth
```

而不是：

```text
memory
```

不要把 Observation Store 和长期 Memory 混为一谈。

---

# 53. Observation 与 Memory 的区别

Observation：

```text
cargo test 输出
rg 查询结果
HTTP response
```

特点：

```text
session-local
高保真
短期工作上下文
来源明确
```

Memory：

```text
用户偏好
项目知识
长期事实
```

特点：

```text
cross-session
semantic
精选
长期存在
```

所以：

```text
Observation != Memory
```

但未来可以：

```text
Observation
↓
agent 判断有长期价值
↓
memory_save
```

---

# 54. Observation 与 Working Memory

如果用认知模型类比：

```text
Provider Context
≈ Working Memory

Observation Store
≈ External Episodic Workspace

MemoryStore
≈ Long-term Memory
```

Context 中不需要一直摆着所有 Observation。

需要时：

```text
retrieve
```

即可。

这和人不会一直把所有读过的 terminal output 放在工作记忆里是一个道理。

---

# 55. 最终目标

希望 Komo 从：

```text
LLM context
=
conversation history
+
所有 tool outputs
```

演进成：

```text
                Durable Truth
                     │
         ┌───────────┴───────────┐
         │                       │
 Conversation Events        Observations
                                 │
                              Artifacts
         │                       │
         └───────────┬───────────┘
                     │
              Context Projector
                     │
        ┌────────────┼────────────┐
        │            │            │
      Full        Excerpt       Handle
        │            │            │
        └────────────┴────────────┘
                     │
                    LLM
```

最核心的一句话：

> **Komo 不应该把 LLM 当前看不到的信息等价为“系统没有这份信息”。**

完整事实应该持久化。

Context 只是当前 reasoning step 所需要的视图。

---

# 56. 推荐实施顺序

### M1 — Observation 基础类型

实现：

```text
Observation
ArtifactRef
ArtifactStore
LocalArtifactStore
```

验收：

```text
大结果可完整落盘并重新读取
```

---

### M2 — ToolRunner 接入

修改：

```text
ToolRunner
```

大输出写 Artifact。

验收：

```text
rg 产生 1 MB output
Event Log 不丢失结果
```

---

### M3 — Context Projection

实现：

```text
Full / Excerpt / Handle
```

验收：

```text
后续 provider request 不重复发送 1 MB result
```

---

### M4 — observation_read

Agent 可以恢复：

```text
任意 Observation range
```

验收：

```text
模型看到 Handle 后能主动获取原始内容
```

---

### M5 — Metrics

记录：

```text
original bytes
projected bytes
recall
```

验收：

```text
能够定量比较优化前后 context 成本
```

---

# 57. 当前优先级

建议 Komo 接下来按：

```text
1. rg native tool
2. Observation / Artifact
3. Context Projection
4. observation_read
5. parallel tool execution
6. Action Fusion
7. semantic compaction
8. evidence-preserving reducer
```

其中 2～4 最好在大量新增输出型 tool 之前稳定下来。

否则：

```text
rg
grep
shell
web
```

分别发展出自己的：

```text
truncate
limit
preview
```

逻辑，之后还需要重新统一。

---

# 58. 最后原则

这套设计遵循几个 Komo 应该长期坚持的 Harness 原则：

### 事实与视图分离

```text
Execution Truth != Model Context
```

### 可逆优化优先

```text
Handle / Recall
```

优于：

```text
Truncate
```

### Storage 与 Reasoning 解耦

ToolRunner 不决定 LLM context。

### LLM 做语义判断，程序维护系统不变量

未来 Reducer 可以由 LLM 做。

但：

```text
artifact integrity
evidence verification
permission
recovery
```

必须由 deterministic code 保证。

### 不把 Harness 做成 Workflow Engine

先解决：

```text
Agent reasoning efficiency
```

而不是构建：

```text
通用 DAG runtime
```

---

## Decision

Komo 下一阶段采用：

> **Observation + Artifact Store + Context Projection + Observation Recall**

作为 Tool Result 的统一模型。

现有：

```text
“大结果落 blob + truncate”
```

调整为：

```text
“大结果完整落 Artifact + Context Projection + 按需 Recall”
```

这是后续 Tool Parallel、Action Fusion、Semantic Compaction 和 Evidence-Preserving Reduction 的共同基础。
