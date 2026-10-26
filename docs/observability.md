# komo 可观测性

> 状态：2026-10-19 认可第一版；**2026-10-26 按新存储与 pi-durable 的遥测重写 §1、§3、§5、§9**（提议，未实施）。
> 第一版已实现、现在仍在工作的：依赖、`[telemetry]`、`init_tracing` 接 OTel 层、`komo doctor` 的导出状态、日志单写轮转与
> 空闲降级（§4、§6）；后台 span（`komo.memory.extract` · `komo.memory.consolidate` · `komo.cron.firing` · `komo.delivery` ·
> `komo.mcp.connect`）与记忆 / 投递 / Cron / MCP 指标；辅助模型调用（记忆、重排、标题、分支摘要、embedding）的 `chat` /
> `embeddings` span 与 LLM 指标；`run inspect` 与召回诊断（§5）。**主链路（Run、模型请求、工具）的 span 与指标随 D7b 删旧代码
> 一起没了**，见 §1。与 [`komo_bot.md`](komo_bot.md) 冲突时，涉及本文范围（遥测、日志、运维视图）的以本文为准；落地后把对应规格
> 写回这里，不再另起文件。
> 文中不带文件名的章节号（§3、§8.3、§13.3 …）指 `komo_bot.md`，记忆的 §9.x 指 [`memory.md`](memory.md)。
> 对照：本机 `../pi` 提交 `d5b3de22d` 的 `packages/durable`（README "Telemetry"、`src/harness/telemetry.ts` 的
> `DURABLE_TELEMETRY_SCHEMA`，来自 `48909d987` / `2ec55d808`）与 `packages/ai/src/telemetry.ts`（`AI_TELEMETRY_SCHEMA`）。

## 1. 现状（2026-10-26 盘点）

第一版（2026-10-19）的埋点挂在旧执行路径上：`invoke_agent` 在 `AgentRunHandler::run`，`komo.round` 在 `AgentLoop::run`，
主对话的 `chat` 经 `telemetry::turn` 包住有状态的 `TurnDriver`，`execute_tool` 在 `ToolExecutor::execute_ready`。D5 把生产默认
换成新存储、D7b 删掉旧路径之后，**新路径上一处埋点都没有**：`crates/komo-runtime/src/durable/` 与三家无状态 provider
（`llm/{anthropic,chat,responses}/stateless.rs`）里没有 span、没有 `komo::metrics` 事件，整个执行路径只有 8 条 `tracing` 日志。

| 第一版的名字 | 现在 |
|---|---|
| `invoke_agent`、`komo.run.duration` / `komo.run.rounds` | **没有**（旧 `AgentRunHandler` 已删） |
| `komo.round` | **没有**（旧 `AgentLoop` 已删） |
| 主对话与压缩摘要的 `chat`、`gen_ai.client.token.usage` / `operation.duration`（role = main / compaction）、`komo.llm.retries` | **没有**：生成与压缩任务直接调 `ModelStream::stream`，不经 `telemetry::turn` |
| `execute_tool`、`komo.tool.duration` | **没有**：工具任务没有埋点 |
| 辅助调用的 `chat`（memory.extract / memory.consolidate / rerank / title）、`embeddings` | 在（仍走 `telemetry::turn` / `ask` / `embed`） |
| `komo.memory.*` / `komo.cron.firing` / `komo.delivery` / `komo.mcp.connect` 及其指标 | 在 |

连带的缺口：

- **Grafana 看板**（`docs/grafana/komo.json`）的"模型"一组只剩辅助调用，"Run""工具"两组、主对话的重试、`invoke_agent` 链路面板全空。
  §9 末尾的 §14 待验证项（压缩值不值、缓存命中、CJK 估算偏差）现在没有数据可看。
- **effort 两头都没记**：assistant 消息的附加字段只有 `api` / `provider` / `model` / `usage` / `providerBlocks`
  （`llm/model_stream.rs:480`），档位只在生成任务的 checkpoint 里；无状态 provider 的完成日志（`anthropic/stateless.rs:137` 等）
  也没有 `effort` 字段，旧 driver 是有的（`llm/anthropic.rs:434`、`llm/responses.rs:356`）——§13.3"实际用的强度记进日志"在新路径上失效。
  pi 在 `d5b3de22d` 补的是同一件事（assistant 消息记 `thinkingLevel`）。
- **引擎报告被吞**：gateway 起引擎用 `EngineOptions::default()` 的 `report`（空函数，`service/durable/core.rs:69`）。调度器自己的报告另写
  一条 warn（`durable/scheduler.rs:224`），但任务阶段经 `TaskRuntime::report` 发的（部分回复提交失败 `harness/generation.rs:1140`、
  工具进度提交失败 `harness/tool.rs:1289`）在生产里**连日志都没有**。
- **存储没有任何指标**：提交延迟、等提交线 `gate` 的时间、每秒事务数、WAL 大小都不可见；`docs/storage-durable-perf.md` 里
  "先测再说"的几项没有持续的数据来源。
- 第一版 §1 列的旧问题（Run 的 token 不落盘、`/v1/runs/{id}` 写死 `ended_at` 等）随 §5 与新存储都已解决或不再适用，不再列出。

## 2. 原则

1. **事实与遥测分开。** 事实（`durable.db`、Session 目录）是产品行为依赖的东西，必须本地完整；遥测是为了看趋势和排障，后端不在时
   丢了可以接受。任何功能都不读遥测。
2. **埋点只写 `tracing`。** runtime / gateway 只用 `tracing` 宏开 span、发事件；指标也写成 `tracing` 事件的字段。一份埋点同时流向
   文件日志与 OpenTelemetry。OpenTelemetry 的包只进 bin。**跨进程 / 跨重启接续链路要用的 trace 上下文**经一个注入口
   （§4"链路接续"）由 bin 实现，runtime 只存取不透明的字符串。
3. **不上报内容。** 提示词、回复、工具参数与输出、记忆正文、交接说明都不进 span 与指标；只有 id、名称、计数、耗时、状态。
   失败只记固定的类别（`error.type`），pi 的阶段 span 带截断到 500 字符的诊断，komo 不带——诊断里可能有调用方内容。
4. **按标准命名。** 模型调用、Agent、工具调用用 OpenTelemetry GenAI 语义约定的名字（`gen_ai.*`）；约定没有的用 `komo.*`。
   实现时以届时的 semconv 版本为准核对每个名字。
5. **指标维度低基数。** 会话、run、任务、entry、调用 id 只放在 span 上，不做指标维度。
6. **只在提交之后发。** 描述"已经发生的事"的事件与指标（entry 追加、输入结算、run 结束、交接）只在那次提交成功之后发
   （pi："entry, submission, checkpoint, replay and report events after commit"）；被拒或失败的提交不留痕迹。

## 3. 上报什么

### 3.1 链路：照 pi-durable 按任务阶段组织

旧设计是"一个 Run 段一个 `invoke_agent`"，并承认"span 活不过进程，用 `komo.run.id` 在 Tempo 里把几段查到一起"。新引擎的执行
单位是**任务阶段**（checkpoint 之间的一段），而 pi-durable 的遥测正是按这个组织的：每个阶段一个 span，trace 上下文**存进
submission 与 task 记录**，重启后的阶段接回原来的 trace。照搬：

```text
komo.submit                    一次受理（一条输入 / write）：受理调用 + 它的提交
 │  gen_ai.agent.name · gen_ai.conversation.id · komo.session.kind · komo.submission.id · komo.submission.type
 │  komo.submit.outcome（placed / queued / duplicate / rejected）
 │  ── trace 上下文存进 submission 记录；这条输入起的 run 的任务都继承它 ──
 ├─ komo.phase                 一个任务阶段的一次调用（含它的决定提交），不含持久等待与重启
 │   │  komo.task.id · komo.task.name（komo.generation / komo.tool / komo.compaction / komo.background）
 │   │  komo.phase（prepare / request / retry / tools / call / execute / probe / orchestrate …）
 │   │  komo.phase.kind（run / abort）· komo.task.attempt · komo.task.resumed（本进程第一次调用这个任务）
 │   │  komo.phase.outcome（next=<阶段> / waiting / sleeping / completed / failed / faulted / aborted）
 │   │  工具任务另带 gen_ai.tool.name · gen_ai.tool.call.id
 │   ├─ chat                   generation 的 request、compaction 的摘要请求（§3.3 的属性 + komo.llm.effort）
 │   │    事件 komo.llm.first_chunk
 │   ├─ execute_tool           工具任务 execute 阶段里真正执行的那次尝试
 │   └─ 事件（提交之后）：komo.entry（id、kind）· komo.submission（结算：done / unanswered + reason）
 │        komo.checkpoint（阶段在调用中途前进）· komo.replay（嵌套调用从 komo.nested 回放）
 │        komo.report（type）· komo.handoff（字数，docs/handoff.md）
 └─ komo.phase …               下一个 generation、它的工具任务……都挂在同一个 trace 下
后台（各自一棵树，不变）：
 komo.memory.extract · komo.memory.consolidate · komo.cron.firing · komo.delivery · komo.mcp.connect
```

继承规则（pi README "Telemetry" 的 propagation 一节，按 komo 的任务改写）：

- **输入起一棵新 trace**，带一个 link 指向调用方当前的 span（HTTP 请求、渠道消息处理、`komo.cron.firing`）。write（`/new`、
  label、手动压缩的摘要）是调用方 span 的子 span，不起新 trace。同一个 `request_id` 重复提交保留原 submission 与原 trace。
- **任务继承创建者的 trace**：`inbox::start_run` 建的第一个 generation 取 run 首个输入的 trace；同一轮交棒的下一个 generation、
  它建的工具任务、阻塞 / 后台压缩都继承。一个 run 由多条排队输入一起起时，取第一条的 trace，其余输入经 span link 连上。
  插话（steer）的 `komo.submit` link 到正在跑的 run 的 trace，不另起 run。
- **后台任务另起 trace**：`task` 派出的子 conversation 与 `monitor` 的消息不继承父的 trace（子会话可以跑很久，挂在父 trace 下会让
  一棵树无限长），而是新 trace + link 到派出它的那个 `komo.phase`。交回的 `task.result` 输入照第一条规则起新 trace，link 到子 run。
- **重启**：开库后第一次调用某个任务时 `komo.task.resumed = true`，span 的父取任务记录里存的 trace 上下文——同一棵 trace 里能看到
  "进程在这里断过"。没有存（旧记录、遥测关着）就从根起。
- **阶段 span 只在 INFO 级**、钩子（`AgentHooks` / `OrchestrationHooks`）不另开 span（pi："Hooks add no spans"）。
- 失败的阶段（`failed` / `faulted`，以及调度器写的 `orphaned`）把 span 状态设为 error，`error.type` = `task_failed` / `task_faulted` /
  `task_orphaned`；`aborted` 保持 ok（取消与关闭都不是错误）。

### 3.2 指标

| 指标 | 类型 | 维度 | 状态 |
|---|---|---|---|
| `gen_ai.client.token.usage` | histogram | `gen_ai.request.model`、`komo.llm.role`、`gen_ai.token.type`（input / output / cache_read / cache_write / reasoning；没报的不发） | 辅助调用在；main / compaction **待补** |
| `gen_ai.client.operation.duration`（秒） | histogram | `gen_ai.request.model`、`komo.llm.role`、`error.type` | 同上 |
| `komo.llm.first_chunk.duration`（秒） | histogram | `gen_ai.request.model`、`komo.llm.role` | **新增**（pi `pi.ai.first_chunk.duration`） |
| `komo.llm.retries` | counter | `gen_ai.request.model`、`komo.retry.cause` | **待补**（generation 的 `retry` 阶段） |
| `komo.run.duration`（秒）· `komo.run.rounds` | histogram | `komo.run.outcome`、`komo.run.source`、`komo.session.kind` | **待补**（run 结束的那次提交之后） |
| `komo.tool.duration`（秒） | histogram | `gen_ai.tool.name`、`komo.tool.status` | **待补** |
| `komo.phase.duration`（秒） | histogram | `komo.task.name`、`komo.phase`、`komo.phase.outcome` | **新增**（pi `pi.durable.phase.duration`） |
| `komo.tasks.active` | up-down counter | `komo.task.name` | **新增**（pi `pi.durable.tasks.active`） |
| `komo.reports` | counter | `komo.report.type` | **新增**（pi `pi.durable.reports`） |
| `komo.durable.commit.duration`（秒） | histogram | `komo.commit.kind`（structural / progress）、`komo.commit.outcome`（ok / rejected / failed） | **新增**：存储层 |
| `komo.durable.gate.wait`（秒） | histogram | `komo.gate.op`（commit / read） | **新增**：等提交线的时间 |
| `komo.handoffs` | counter | `komo.session.kind` | **新增**（`docs/handoff.md`） |
| `komo.memory.injected` · `.used` · `komo.memory.extract.skipped` | counter | 同第一版 | 在 |
| `komo.delivery.failures` · `komo.cron.failures` · `komo.mcp.connect.failures` | counter | 同第一版 | 在 |

指标名保留第一版的（`komo.run.*`、`komo.tool.*`、`gen_ai.*`），看板查询不用改；时长单位仍是秒（pi 用毫秒，komo 不跟）。

### 3.3 span 与属性的名字（核对表）

埋点辅助函数在 `komo-runtime/src/telemetry.rs`；span 都是 INFO，指标事件都是 `target = "komo::metrics"`、DEBUG。

| span | 位置（新） | 属性 |
|---|---|---|
| `komo.submit` | `Harness::submit` / `admit_in_tx` 的公开入口（gateway 的 `submit_input`、Dispatcher、cron 受理） | 见 §3.1 |
| `komo.phase` | `durable/scheduler.rs` 调 `TaskKind::run_phase` 处（`:878`）与 `run_abort`（`:893`）：一处埋点覆盖所有任务 | 见 §3.1 |
| `chat` | `telemetry::stream`（新）：包住 `ModelStream::stream`，由 generation 的 `request` 与 compaction 的摘要请求调用；辅助调用仍走 `turn` / `ask` | `gen_ai.operation.name`、`gen_ai.request.model`、`gen_ai.response.model`、`gen_ai.response.id`、`gen_ai.usage.input_tokens`、`gen_ai.usage.output_tokens`、`komo.usage.cache_read_tokens`、`komo.usage.cache_write_tokens`、`komo.usage.reasoning_tokens`、`komo.llm.role`、`komo.llm.effort`、`komo.llm.attempt`（checkpoint 里的 `attempt`，从 1 起）、`error.type`（HTTP 状态码 / timeout / incomplete / transport / unsupported_effort / unknown，与 `telemetry::error_type` 相同；另加 `context_overflow`——`LlmError` 没有这个变体，取 `is_context_overflow` 识别出的超长） |
| `embeddings` | `telemetry::embed`（不变） | 同第一版 |
| `execute_tool` | 工具任务 `execute` 阶段执行一次尝试处（`harness/tool.rs`）；被拒、不认识的工具名、编排调用（`task` / `monitor` 走 `orchestrate`）不开 | `gen_ai.operation.name`、`gen_ai.tool.name`、`gen_ai.tool.call.id`、`komo.tool.attempt`、`komo.tool.status`（completed / failed / uncertain）、`komo.tool.exit_code`、`komo.tool.output_bytes`、`error.type` |
| 后台 span | 不变（`memory/mod.rs:999`、`memory/offline.rs:185`、`service/durable/cron_watch.rs:39`、`deliveries.rs:113`、`mcp.rs:649`） | 同第一版 |

| 事件 | 发在 | 属性 |
|---|---|---|
| `komo.entry` | 当前 `komo.phase` / `komo.submit` | `komo.entry.id`、`komo.entry.kind` |
| `komo.submission` | 同上 | `komo.submission.id`、`komo.submission.status`（done / unanswered）、`komo.submission.reason`、`komo.answer.entry_id` |
| `komo.checkpoint` | `komo.phase` | `komo.phase`（提交的新阶段） |
| `komo.replay` | codemode 外层的 `komo.phase` | `gen_ai.tool.name` |
| `komo.report` | 当前 span（没有就只计数） | `komo.report.type`、`komo.task.id` |
| `komo.llm.first_chunk` | `chat` | — |
| `komo.handoff` | generation `tools` 阶段的 `komo.phase` | `komo.handoff.chars` |

`gen_ai.token.type` 的 `cache_read` / `cache_write` / `reasoning` 是 komo 的扩展取值；`gen_ai.agent.name`、`gen_ai.conversation.id`、
`gen_ai.tool.call.id`、`gen_ai.response.id` 对照写第一版时的 GenAI semconv，升级 SDK 时再核一遍。

**不进 span 与指标的**：提示词、回复、工具参数与输出、记忆正文、交接说明；会话 / run / 任务 / entry / 调用 id 只在 span 与事件上，
指标事件里一个都没有。

**仍未做（第一版留下的）**：`komo.memory.same` · `komo.memory.contested`（等提取重写）；`komo.memory.recall` span；Cron 在 Run
起来之前就失败的触发没有 span。第一版计划的 `komo.compaction` span 由 `komo.phase`（`komo.task.name = komo.compaction`）代替，决策的
token 账作为它的属性（`komo.compaction.variant`、`archive_tokens`、`post_compaction_tokens`）。

缓存命中率（cache_read ÷ input）、费用（token × 单价）、成功率都在 Grafana 里用查询或记录规则算，komo 里不放价格表。

## 4. 实现

**依赖（2026-10-19 核对，只进 bin）**

| 依赖 | 配置 | 理由 |
|---|---|---|
| `opentelemetry` / `opentelemetry_sdk` | `0.33`，`trace`、`metrics` | SDK 只在 bin 初始化 |
| `opentelemetry-otlp` | `0.33`，`default-features = false, features = ["http-proto", "reqwest-blocking-client", "trace", "metrics"]` | OTLP/HTTP + protobuf；走工作区同一份 reqwest 0.13（只多开它的 `blocking` 特性）；`prost ^0.14` 已在依赖树里（turso 同步引擎）；不开 `grpc-tonic`、`logs`。**用 blocking 而不是 `reqwest-client`**：SDK 0.33 的批量 span 处理器与周期指标读取器都在自己的线程上用 `futures_executor::block_on` 驱动导出，那里没有 tokio reactor，async 版 reqwest 会 panic（已读 SDK 源码确认）。blocking 客户端自带一个内部 runtime；因此 shutdown 在 async 里要走 `spawn_blocking` |
| `tracing-appender` | `0.2`，`default-features = false` | 日志按天轮转、保留 7 个文件（§6）；tokio-rs 官方 |
| `tracing-opentelemetry` | `0.34`（对应 `opentelemetry 0.33`），默认特性含 `metrics` | `OpenTelemetryLayer` 把 span 转成链路；`MetricsLayer` 把带 `histogram.` / `monotonic_counter.` / `counter.` 前缀字段的 tracing 事件转成指标——所以 runtime / gateway 不依赖任何 OTel 包 |

链路接续不加新依赖：W3C TraceContext 传播器在 `opentelemetry_sdk` 里。

**初始化（已实现）。** `crates/komo/src/telemetry.rs` 建管线，`init_tracing` 只在前台 Gateway 进程且配了端点时接 `OpenTelemetryLayer`
与 `MetricsLayer`；CLI / TUI 从不导出。端点是启动期键，`main` 在装 tracing 之前用 `load_config` 读一次。资源属性
`service.name = komo`、`service.version`、`service.instance.id`（本机主机名）。退出时 `spawn_blocking` 里 flush（最多等 3 秒）。

**链路接续（新）。** pi 把 trace 上下文作为不透明的 `trace` 字段存进 submission 与 task 的 JSON（"without a storage migration"），
任务的阶段 span 从里面取父。komo 照做，但要守住"OTel 包只进 bin"：

- `komo-runtime` 定义一个注入口（`telemetry::Propagation`）：`inject(&Span) -> Option<String>`（取当前 span 的 W3C `traceparent`）、
  `extract(&Span, &str)`（把存下的上下文设成 span 的父）、`link(&Span, &str)`（加 span link）。默认实现什么都不做。
- bin 用 `tracing-opentelemetry` 的 `OpenTelemetrySpanExt` + `TraceContextPropagator` 实现它，启动时装进引擎；没配端点就是默认实现。
- 记录格式：`SubmissionRecord` 与 `TaskRecord` 各加可选的 `trace: Option<String>`（serde 默认 `None`、`None` 时不序列化，旧记录照读；
  JSON 列，不改 DDL）；`TaskRecord` 再加不可变的 `links: Vec<String>`（多条输入起一个 run 时）。建任务时默认继承创建者的 `trace`。
- 存的只是 trace id 与 span id，不是内容，原则 3 不受影响。

**提交之后才发（新）。** `Tx` 加 `after_commit(FnOnce())`：闭包里排进去的回调只在这次提交成功之后、在提交调用方的任务里（也就是
当前 `komo.phase` span 内）执行，被拒或失败时丢弃。`komo.entry` / `komo.submission` / run 结束的指标 / `komo.handoff` 都经它发。
run 结束的指标在 `live::end_run`（`harness/live.rs:265`）处排队：它手里有被移除的 `RunControl`（`rounds`、`tokens`）；时长 =
提交时刻 − 首个输入的 `SubmissionRecord.created_at`。

**引擎报告（新）。** `EngineOptions.report` 的签名加一个固定的类别（`kind: &'static str`：`task_faulted`、`task_error`、`progress_commit`、
`hook_error` …，对应 pi 的 `ReportInfo.type`）；gateway 装一个实现：打 warn 日志、在当前 span 上发 `komo.report`、计数 `komo.reports`。

**存储指标（新）。** runtime `durable/line.rs` 的 `Line::commit` / `read_on_line` 量等 `gate` 的时间（`komo.durable.gate.wait`），
`commit_to_storage` 量提交耗时与结果（`komo.durable.commit.duration`）。都是本地计时，不进 store 的 SQL 路径。

**埋点约定（不变）。**

- 链路：span 用 INFO 级；OTel 层收 INFO 及以上，与 `KOMO_LOG` 无关。
- 指标：`tracing` 事件，`target: "komo::metrics"`，DEBUG 级，字段前缀 `histogram.` / `monotonic_counter.` / `counter.`
  （`MetricsLayer` 的约定；up-down counter 用 `counter.`）。
- 过滤是**每层各自的**：文本日志（文件、stderr）按 `KOMO_LOG`，并且永远排除 `komo::metrics`；OTel 两层各用 `Targets`（默认 INFO，
  `komo::metrics` 放到 DEBUG）。

**配置（不变）。**

```toml
[telemetry]
# OTLP/HTTP 端点；省略 = 不上报，埋点仍写文件日志。
otlp_endpoint = "http://127.0.0.1:4318"
```

`otlp_endpoint` 是启动期键，改了按 §3 的规则**报告**需要重启；值必须是 http(s) 地址；空串等于没写。信号路径由 komo 补。

**失败与背压（不变）。** 链路用批量处理器、指标用周期读取器（默认 60 秒）；队列有上限，满了丢弃。后端不通只在第一次失败与恢复时各记
一条 warn / info，从不阻塞 Run。`komo doctor` 显示端点与最近一次 span 导出成功或失败的时间与原因（`TelemetryStatus`，`/healthz` 带出）。

## 5. 本地事实与视图

产品自身要用的，不依赖遥测后端：

| 项 | 状态 |
|---|---|
| `run inspect` 与 `/v1/runs/{id}` | 在（D5 在新存储上重做，`service/durable/runs.rs`）：`model`、`duration_ms`、`failure`、`tokens`（没人报的缺席，不是 0）、`compactions`、`recall`、`memories` |
| **assistant 消息记 effort**（新） | 无状态 provider 造最终消息（`llm/model_stream.rs` 的 `final_message`）时，配置里**显式**给了档位就在附加字段写 `effort`（与 §13.3 的 `EffortSetting` 一致：没配置 = 服务端默认，不写，不虚构）。生成任务恢复后重发也带着它（档位在 `request` checkpoint 的 `ModelConfig` 里）。`run inspect` 的 `model` 旁边显示它；导出照带（pi 的 `thinkingLevel` 是同一件事，取值不同，不改名） |
| **provider 完成日志带 effort**（新） | `llm/*/stateless.rs` 的 "model stream completed / failed" 日志加 `effort` 字段，恢复 §13.3 的要求 |
| 投递 | 不加命令；投递失败进 `komo.delivery` span 与指标 |

## 6. 日志（已实现，不变）

- 只写一份：Gateway 自己写 `logs/gateway.log.<日期>`（`tracing-appender` 按天轮转，保留 7 个），纯文本无 ANSI；stderr 层只在 stderr
  是终端时开。launchd 仍把 stdout / stderr 收进 `gateway.out.log` / `gateway.err.log`，里面只剩 panic 之类绕过 tracing 的输出。
- 周期任务什么都没改时降到 DEBUG。
- 日志仍是纯文本，给人排障用；任何统计都不从日志里解析。日志进 Loki 不在这一版（要多一个 `opentelemetry-appender-tracing`）。

## 7. 本地后端（不变）

用 Grafana 官方的 `grafana/otel-lgtm`：一个容器里有 OTLP 接收（4318）、Prometheus、Tempo、Loki、Grafana（3000）。

```bash
docker run -d --name komo-lgtm --restart unless-stopped \
  -p 127.0.0.1:3000:3000 -p 127.0.0.1:4318:4318 \
  -v ~/.komo-lgtm:/data grafana/otel-lgtm:<版本>
```

- 端口只绑 127.0.0.1，镜像版本钉死。NAS 上的实例在自己的 `config.toml` 里指向自己的后端。
- 看板 JSON 在 `docs/grafana/komo.json`（uid `komo-overview`）：
  - 导入：`python3 -c 'import json;print(json.dumps({"dashboard":json.load(open("docs/grafana/komo.json")),"overwrite":True}))' | curl -s -u admin:admin -H 'Content-Type: application/json' -d @- http://127.0.0.1:3000/api/dashboards/db`
  - 时长单位是秒。计数器第一次发生事件才会出现，此前面板为空是正常的。
  - **要改的**（§9 第 6 步）：链路面板从 `invoke_agent` 改查 `komo.submit` 的 trace；新增"任务阶段"一组（`komo.phase.duration` p95 按任务 / 阶段、
    `komo.tasks.active`、`komo.reports`）、"存储"一组（提交耗时 p50 / p95 按 kind、等 gate 的时间、提交速率）、首包延迟、交接次数。

## 8. 不做

- `/metrics`（Prometheus 拉取）：OTLP 推送已经覆盖。
- komo 内的统计命令与遥测表：汇总交给 Prometheus / Grafana。
- 价格表与费用字段：Grafana 记录规则里算（pi 在 span 上记 `cost.total`，komo 不跟）。
- 内容采集：见原则 3。
- pi 的逐内容块事件（`pi.ai.block`：每个文本 / 思考 / 工具调用块的字数）：komo 先只要首包事件，块级等有排障需要再加。

## 9. 落地步骤

| 步 | 内容 | 验证 |
|---|---|---|
| 1 | `telemetry::stream` 包住 `ModelStream::stream`（`chat` span、token / 耗时 / 首包指标）；generation 的 `retry` 阶段计 `komo.llm.retries`；assistant 消息与 provider 日志记 effort（§5） | 测试：`capture` 断言一次生成请求的 `chat` span 属性、没报的 token 不出现为 0；effort 显式时消息带、没配置时不带 |
| 2 | `komo.phase`：调度器两处调用点，`komo.tasks.active`、`komo.phase.duration`；`execute_tool` 与 `komo.tool.duration` | 测试：一个两轮带工具的 run 产生的阶段 span 序列（prepare → request → tools → call → …）与结果属性；faulted 的阶段 span 是 error |
| 3 | `Tx::after_commit`；`komo.entry` / `komo.submission` / run 结束指标 / `komo.handoff` | 测试：被拒的提交不发事件；run 结束发一次 `komo.run.duration` 与 `komo.run.rounds` |
| 4 | 引擎报告：`report` 带类别，gateway 接上 | 测试：部分回复提交失败时有 warn 日志与 `komo.reports` 计数 |
| 5 | 链路接续：`Propagation` 注入口与 bin 实现、`trace` / `links` 字段、`komo.submit`、继承规则、`komo.task.resumed` | 测试（内存 exporter，在 bin）：一个 run 的阶段 span 同属 `komo.submit` 的 trace；重启后的阶段接在同一 trace 下且 `resumed = true`；`task` 派出的子 run 是新 trace 并 link 回去；旧记录（无 `trace`）照常跑 |
| 6 | 存储指标；看板 JSON 按 §7 改 | 本机跑一个真 Run，Tempo 里看到整棵树，看板每个面板有数 |

第 1–4 步各自独立，先做；第 5 步改记录格式（只加可选字段），最后做。每步结束 `cargo test --workspace`，把实现了的名字从 §3 的"待补"
改成在用。

**落地后能回答的 §14 待验证项**：压缩值不值（`komo.compaction` 任务的阶段 span + 缓存读写）、CJK token 估算偏差（估算 vs
`gen_ai.usage.input_tokens`）、Anthropic 第 4 个缓存断点的命中；以及 `docs/storage-durable-perf.md` 的 §2.3 / §4.3 测量（提交耗时、
事务率）。

## 10. 待定

- 链路采样：现在全采（个人用量很小），量上来再定。阶段 span 比旧的段 span 多（一个工具调用至少两个阶段），量要在第 2 步之后看一眼。
- 记忆的采纳（`used`）依赖 §9.10 的提取调用返回 `used: [编号]`。
- 后台子会话"另起 trace + link"还是"继承父 trace"（§3.1）：pi 是继承（"Nested tasks and background compactions inherit the creating phase"），
  komo 的 `task` 子会话可以跑几个小时、收很多条监听消息，先按另起定，看了真实的树再说。
