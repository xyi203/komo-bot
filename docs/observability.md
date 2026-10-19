# komo 可观测性

> 状态：设计，2026-10-19 认可。**已实现**：依赖、`[telemetry]`、`init_tracing` 接 OTel 层、`komo doctor` 的导出状态、日志单写轮转与空闲降级（§4、§6）；§3.1 的 `invoke_agent` / `komo.round` / `chat` / `execute_tool` 埋点与 §3.2 的模型 / Run / 工具指标（名字见 §3.3）；`message.assistant.reasoning_tokens`；§5 的 `run inspect` 与召回诊断。其余未实现。与 [`komo_bot.md`](komo_bot.md) 冲突时，涉及本文范围（遥测、日志、运维视图）的以本文为准；落地后把对应规格写回这里，不再另起文件。
> 文中不带文件名的章节号（§3、§8.3、§13.3 …）指 `komo_bot.md`，记忆的 §9.x 指 [`memory.md`](memory.md)。

## 1. 现状（2026-10-19 盘点）

缺的大多不是"没采集"，而是采了没留下，或者留下了没处看。

**采了没留下**

- Run 的 token 合计算出来后丢掉：`agent/handler.rs` 收尾时 `SegmentOutcome::Completed { .. }` 不取 `usage`，只有内存里的预算检查用过它。§8.4 写的"已用预算持久化"也没有做。
- reasoning token 不落盘：`TokenUsage.reasoning` 在 `coordinator/ledger.rs` 写 `message.assistant` 时没有带上。
- 每次模型请求的耗时、实际模型、effort、服务商请求 id 只进 tracing 文本日志（`llm/responses.rs`、`llm/anthropic.rs`、`llm/chat.rs`）。
- 失败、重试的模型请求不留事件，错误正文只在 `tracing::warn`。
- 辅助调用（记忆提取、判关系、重排、标题、压缩之外）的 token 一个都没记；重排的用量直接是 `Default`（`memory/rerank.rs`）。
- `PassReport`、召回降级、投递失败、MCP 连接状态只进日志。

**留下了没处看**

- `/v1/runs/{id}`：`summary_of`（`http/runs.rs`）把 `ended_at` 写死为 `None`，`memories` 恒为空；`RunDetail` 没有失败原因、模型、token、耗时。
- `deliveries` 表有 `state / attempts / last_error`，没有任何命令或接口能查。
- 工具耗时在 `tool.result.elapsed_ms` 与 `tool_attempts` 里，没有任何汇总；`run inspect` 也不显示耗时与输出大小。
- 缓存命中率只在 TUI 底栏现算。
- 没有 `/metrics`，也没有任何统计命令。

**日志**：`init_tracing`（`crates/komo/src/main.rs`）装两个 fmt 层，stderr 被 launchd 收进 `gateway.err.log`，同时追加到 `logs/gateway.log`，同一行写两份；纯文本，不轮转；对账什么都没做时每分钟一行 INFO。

**不是 bug 的一处**：本机 5114 条带用量的 `message.assistant` 里 `cache_write_tokens` 全空。§13.3 已写明只有 Anthropic 报缓存写入，这台机器用的是 Responses / Chat Completions 后端，空是对的（"没报的留空，不记成 0"）。

## 2. 原则

1. **事实与遥测分开。** 事实（JSONL、state.db）是产品行为依赖的东西，必须本地完整；遥测是为了看趋势和排障，后端不在时丢了可以接受。任何功能都不读遥测。
2. **埋点只写 `tracing`。** runtime / gateway 只用已有的 `tracing` 宏开 span、发事件；指标也写成 `tracing` 事件的字段。一份埋点同时流向文件日志与 OpenTelemetry，不存在"没配 OTel 时退回日志"这种第二条路。OpenTelemetry 的包只进 bin。
3. **不上报内容。** 提示词、回复、工具参数与输出、记忆正文都不进 span 和指标；只有 id、名称、计数、耗时、状态。GenAI 语义约定里内容采集本来就是显式开启项。
4. **按标准命名。** 模型调用、Agent、工具调用用 OpenTelemetry GenAI 语义约定的名字（`gen_ai.*`）；约定没有的用 `komo.*`。实现时以届时的 semconv 版本为准核对每个名字。
5. **指标维度低基数。** 会话、Run、调用 id 只放在 span 上，不做指标维度。

## 3. 上报什么

### 3.1 链路

```text
invoke_agent                 一个 Run 段（受理/恢复 → 完成/挂起/失败）
 │  gen_ai.agent.name · gen_ai.conversation.id = 会话 id · komo.run.id · komo.run.source
 │  komo.session.kind · komo.run.outcome
 ├─ komo.memory.recall       mode · degraded · candidates · injected · dropped_by_budget
 ├─ komo.round               round
 │   ├─ chat                 gen_ai.request.model · gen_ai.response.model · gen_ai.response.id
 │   │                       gen_ai.usage.input_tokens / output_tokens · 缓存读 / 写 · reasoning
 │   │                       effort · error.type · komo.llm.attempt
 │   └─ execute_tool         gen_ai.tool.name · gen_ai.tool.call.id · komo.tool.status
 │                           komo.tool.exit_code · komo.tool.output_bytes · komo.tool.attempt
 └─ komo.compaction          variant（compacted / decayed / skipped）· 决策的 token 账
后台（各自一棵树）：
 komo.memory.extract         §9.10 一批一个：session.kind · runs · window_chars · skip_reason · used
                             returned · applied · injected · used · same · contested
   └─ chat                   用途 komo.llm.role = memory.extract
 komo.memory.consolidate     §9.8 一组一个
 komo.cron.firing · komo.delivery · komo.mcp.connect
```

- **Run 段而不是整个 Run。** Run 可以挂起、跨重启恢复，span 活不过进程。一段一个 `invoke_agent`，用 `komo.run.id` 在 Tempo 里把几段查到一起。
- **失败的模型请求也是一个 `chat` span**，带 `error.type` 与第几次尝试；重试是同一个 round 下的兄弟 span。
- 辅助调用都是 `chat` span，用 `komo.llm.role`（main / compaction / memory.extract / memory.consolidate / rerank / title / embedding）区分。

### 3.2 指标

| 指标 | 类型 | 维度 |
|---|---|---|
| `gen_ai.client.token.usage` | histogram | 模型、`komo.llm.role`、token 类型（input / output / cache_read / cache_write / reasoning） |
| `gen_ai.client.operation.duration` | histogram（秒） | 模型、`komo.llm.role`、`error.type` |
| `komo.llm.retries` | counter | 模型、原因 |
| `komo.run.duration` · `komo.run.rounds` | histogram | outcome、source、session.kind |
| `komo.tool.duration` | histogram | 工具名、status |
| `komo.memory.injected` · `.used` · `.same` · `.contested` | counter | session.kind |
| `komo.memory.extract.skipped` | counter | 原因 |
| `komo.delivery.failures` · `komo.cron.failures` · `komo.mcp.connect.failures` | counter | 渠道 / job / 服务器 |

### 3.3 已埋点的名字（2026-10-19，semconv 核对表）

埋点代码在 `komo-runtime/src/telemetry.rs`（辅助函数）与 `agent/` · `executor/` · `llm/` · `memory/`。span 都是 INFO，指标事件都是 `target = "komo::metrics"`、DEBUG。

| span | 位置 | 属性 |
|---|---|---|
| `invoke_agent` | `AgentRunHandler::run`，一次领取（含压缩 / 插话后的重新装配）一个 | `gen_ai.operation.name`、`gen_ai.agent.name`（Agent id）、`gen_ai.conversation.id`（会话 id）、`komo.run.id`、`komo.run.source`（interactive / cron / memory / verification）、`komo.session.kind`（main / normal / task）、`komo.run.outcome`（completed / suspended / failed / cancelled / error） |
| `komo.round` | `AgentLoop::run`，包住 `chat` 与该轮的 `execute_tool`；续跑上一段停在半路的轮次时只有 `execute_tool` | `komo.round` |
| `chat` | `telemetry::turn`：主对话、压缩摘要、记忆提取、记忆整理、标题；`telemetry::ask`：重排 | `gen_ai.operation.name`、`gen_ai.request.model`、`gen_ai.response.model`、`gen_ai.response.id`、`gen_ai.usage.input_tokens`、`gen_ai.usage.output_tokens`、`komo.usage.cache_read_tokens`、`komo.usage.cache_write_tokens`、`komo.usage.reasoning_tokens`、`komo.llm.role`、`komo.llm.effort`、`komo.llm.attempt`（从 1 起）、`error.type`（HTTP 状态码 / timeout / incomplete / transport / unsupported_effort / unknown） |
| `embeddings` | `telemetry::embed`：记忆索引与查询向量 | `gen_ai.operation.name`、`gen_ai.request.model`、`komo.llm.role = embedding`、`error.type` |
| `execute_tool` | `ToolExecutor::execute_ready`，一次执行尝试一个 | `gen_ai.operation.name`、`gen_ai.tool.name`、`gen_ai.tool.call.id`、`komo.tool.attempt`、`komo.tool.status`（completed / failed / uncertain）、`komo.tool.exit_code`、`komo.tool.output_bytes`（落盘的 output.json + stdout + stderr）、`error.type`（status 不是 completed 时） |

| 指标 | 类型 | 维度 |
|---|---|---|
| `gen_ai.client.token.usage` | histogram | `gen_ai.request.model`、`komo.llm.role`、`gen_ai.token.type`（input / output / cache_read / cache_write / reasoning；没报的类型不发） |
| `gen_ai.client.operation.duration` | histogram（秒） | `gen_ai.request.model`、`komo.llm.role`、`error.type`（失败时） |
| `komo.llm.retries` | counter | `gen_ai.request.model`、`komo.retry.cause`（rate_limited / server / transport） |
| `komo.run.duration`（秒）· `komo.run.rounds` | histogram | `komo.run.outcome`、`komo.run.source`、`komo.session.kind` |
| `komo.tool.duration`（秒） | histogram | `gen_ai.tool.name`、`komo.tool.status` |
| `komo.memory.used` | counter | `komo.session.kind`（`memory/mod.rs`：一批里通过校验的"被用上"条数，§9.10；与 `usage.count` 的曝光分开） |

语义约定里 `gen_ai.token.type` 只有 input / output，`cache_read` / `cache_write` / `reasoning` 是 komo 的扩展取值；`gen_ai.agent.name`、`gen_ai.conversation.id`、`gen_ai.tool.call.id`、`gen_ai.response.id` 对照的是写这一版时的 GenAI semconv，升级 SDK 时再核一遍。

两处与 §3.1 图不一致的实际形状：

- **重试不是同一个 round 下的兄弟。**模型请求失败后 Run 让出名额去等退避（§8.4），重试发生在下一次领取里，所以失败的 `chat` 在一棵 `invoke_agent` 树里、重试的 `chat` 在另一棵里；用 `komo.run.id` 与 `komo.llm.attempt`（从 `runs.retry_attempts` 续数）把它们对上。`telemetry::turn` 本身对同一父 span 下的多次调用给出兄弟 span（单测覆盖）。
- **`execute_tool` 只覆盖真正执行的尝试。**被拒绝 / 不认识的工具名、编排类调用（`task`、`monitor`、`delegate` 一类）不开这个 span。

**不进 span 与指标的**：提示词、回复、工具参数与输出、记忆正文；会话 / Run / 调用 id 只在 span 上，指标事件里一个都没有（`komo-runtime/src/agent/handler.rs` 的埋点测试逐条断言）。

**重排的用量**：`SystemOneUsage` 改成 `Option`，没报就是未知；`chat`（role = rerank）上只记后端真报了的数。

**未做**（后续步骤）：`komo.memory.*`、`komo.compaction`、`komo.cron.*`、`komo.delivery`、`komo.mcp.connect` 的 span，以及记忆 / Cron / 投递 / MCP 的指标（第 5 步）；`init_tracing` 里的 OpenTelemetry 层与 `[telemetry]` 配置（第 2 步，另一条线）。

缓存命中率（cache_read ÷ input）、费用（token × 单价）、成功率都在 Grafana 里用查询或记录规则算，komo 里不放价格表。

## 4. 实现

**依赖（2026-10-19 核对，只进 bin）**

| 依赖 | 配置 | 理由 |
|---|---|---|
| `opentelemetry` / `opentelemetry_sdk` | `0.33`，`trace`、`metrics` | SDK 只在 bin 初始化 |
| `opentelemetry-otlp` | `0.33`，`default-features = false, features = ["http-proto", "reqwest-blocking-client", "trace", "metrics"]` | OTLP/HTTP + protobuf；走工作区同一份 reqwest 0.13（只多开它的 `blocking` 特性）；`prost ^0.14` 已在依赖树里（turso 同步引擎）；不开 `grpc-tonic`、`logs`。**用 blocking 而不是 `reqwest-client`**：SDK 0.33 的批量 span 处理器与周期指标读取器都在自己的线程上用 `futures_executor::block_on` 驱动导出，那里没有 tokio reactor，async 版 reqwest 会 panic（已读 SDK 源码确认）。blocking 客户端自带一个内部 runtime；因此 shutdown 在 async 里要走 `spawn_blocking` |
| `tracing-appender` | `0.2`，`default-features = false` | 日志按天轮转、保留 7 个文件（§6）；tokio-rs 官方 |
| `tracing-opentelemetry` | `0.34`（对应 `opentelemetry 0.33`），默认特性含 `metrics` | `OpenTelemetryLayer` 把 span 转成链路；`MetricsLayer` 把带 `histogram.` / `monotonic_counter.` 前缀字段的 tracing 事件转成指标——所以 runtime / gateway 不依赖任何 OTel 包 |

加入时更新 §13.4 依赖表，`cargo tree -d` 核对没有新重复，`cargo build --timings` 核对冷编预算。

**初始化（已实现）。** `crates/komo/src/telemetry.rs` 建管线，`init_tracing` 只在前台 Gateway 进程且配了端点时接 `OpenTelemetryLayer` 与 `MetricsLayer`；CLI / TUI 从不导出。端点是启动期键，`main` 在装 tracing 之前用 `load_config` 读一次（读不出来就不上报，`run` 自己会带原因失败）。资源属性 `service.name = komo`、`service.version`、`service.instance.id`：此时 Gateway 的运行 id 还没生成，取**本机主机名**（Mac 与 NAS 分得开）。退出时 `spawn_blocking` 里 flush（最多等 3 秒）。

**埋点约定。** 后面各步的埋点按这份约定写：

- 链路：span 用 INFO 级；OTel 层收 INFO 及以上，与 `KOMO_LOG` 无关。
- 指标：`tracing` 事件，`target: "komo::metrics"`，DEBUG 级，字段前缀 `histogram.` / `monotonic_counter.` / `counter.`（`MetricsLayer` 的约定）。写法 `tracing::event!(target: "komo::metrics", tracing::Level::DEBUG, histogram.xxx = v, 维度 = ..)`。
- 过滤是**每层各自的**，没有全局过滤：文本日志（文件、stderr）按 `KOMO_LOG`，并且永远排除 `komo::metrics`；OTel 两层各用 `Targets`（默认 INFO，`komo::metrics` 放到 DEBUG）。所以 `KOMO_LOG` 调多低都不影响导出，指标事件也不会进日志文件（`main.rs` 的测试断言了这一点）。

**配置。**

```toml
[telemetry]
# OTLP/HTTP 端点；省略 = 不上报，埋点仍写文件日志。
otlp_endpoint = "http://127.0.0.1:4318"
```

`otlp_endpoint` 是启动期键（exporter 在启动时建好），快照里是 `start_only.otlp_endpoint`，改了按 §3 的规则**报告**需要重启，不静默忽略。值必须是 http(s) 地址，否则整份配置装不上；空串等于没写。信号路径（`/v1/traces`、`/v1/metrics`）由 komo 补。

**失败与背压。** 链路用批量处理器、指标用周期读取器（默认 60 秒）；队列有上限，满了丢弃。后端不通只在第一次失败与恢复时各记一条 warn / info，从不阻塞 Run（测试：指向无人监听的端口，发 1000 个 span 不等网络）。`komo doctor` 显示端点（或"未配置"）与最近一次 span 导出成功或失败的时间与原因：包了一层 span exporter 把每批结果记进 `TelemetryStatus`，`/healthz` 带出。指标导出的失败目前不进 doctor。

## 5. 本地仍要补的事实与视图

这些是产品自身要用的，不依赖遥测后端：

| 项 | 改法 |
|---|---|
| `message.assistant` | 追加可选字段 `reasoning_tokens`；旧行照常解码 |
| `memory.injected` | **已落地**：可选 `recall`（`RecallDiagnostics`）：mode、`degraded` + 原因、融合前 / 后候选数、`dropped_by_budget`、`reused_pin`（附录 B.3.5 的"为什么选中"）。只在换块时写事件；后面沿用的 Run 没有自己的事件，`run inspect` 标"沿用这一段钉下的那块" |
| `run inspect` 与 `/v1/runs/{id}` | 开始 / 结束时间、耗时、模型、失败原因（`runs.last_error`）、按轮汇总的 token（从 JSONL 的 `message.assistant` 求和）、压缩事件；修 `ended_at` 写死与 `memories` 恒空。**已落地**：`RunDetail` 加 `model` / `duration_ms` / `failure` / `tokens`（某项没人报则缺席，不是 0）/ `compactions` / `recall`，`ended_at` 取 `runs.ended_at`，`memories` 取这条 Run 生效的 `memory.injected`；`reasoning_tokens` 另算 |
| 投递 | 不加命令；投递失败进 `komo.delivery` span 与指标。真要逐条查再说 |

## 6. 日志（已实现）

- 只写一份：Gateway 自己写 `logs/gateway.log.<日期>`（`tracing-appender` 按天轮转，保留 7 个），纯文本无 ANSI；stderr 层只在 stderr 是终端时开（前台手动跑），launchd / systemd 下不再收一份重复的。文件建不出来时退回 stderr。launchd 仍把 stdout / stderr 收进 `gateway.out.log` / `gateway.err.log`，现在里面只剩 panic 之类绕过 tracing 的输出。
- 对账、恢复扫描这类周期任务，什么都没改时降到 DEBUG：对账汇总只在 resumed / blocked / reclaimed / closed / purged 有一项非零时 INFO；"恢复决定"对保持原状的 Run 降为 DEBUG。记忆队列与 Cron 的循环本来就只在有事时记。
- 日志仍是纯文本，给人排障用；任何统计都不从日志里解析。日志进 Loki 不在第一版（要多一个 `opentelemetry-appender-tracing`）。

## 7. 本地后端

用 Grafana 官方的 `grafana/otel-lgtm`：一个容器里有 OTLP 接收（4318）、Prometheus、Tempo、Loki、Grafana（3000）。定位是开发与个人使用，对 komo 合适。

```bash
docker run -d --name komo-lgtm --restart unless-stopped \
  -p 127.0.0.1:3000:3000 -p 127.0.0.1:4318:4318 \
  -v ~/.komo-lgtm:/data grafana/otel-lgtm:<版本>
```

- 端口只绑 127.0.0.1。镜像版本钉死，不用 `latest`；数据卷的容器内路径以该版本 README 为准，落地第 2 步核对重启后数据还在。
- NAS 上的实例在自己的 `config.toml` 里指向自己的后端。
- 仓库放一份 Grafana 看板 JSON（`docs/grafana/komo.json`）：模型 token 按用途 / 模型、缓存命中率、模型 p95 与错误率、Run 失败率与耗时、工具 p95 与失败率、记忆注入 / 采纳 / 重复交代、投递与 Cron 失败。

## 8. 不做

- `/metrics`（Prometheus 拉取）：OTLP 推送已经覆盖，不要第二个出口。
- komo 内的统计命令（`komo stats`）与遥测表（`llm_calls`、`memory_passes`）：汇总交给 Prometheus / Grafana，komo 不再存第二份。
- 价格表与费用字段：Grafana 记录规则里算。
- 内容采集：见原则 3。

## 9. 落地步骤

| 步 | 内容 | 验证 |
|---|---|---|
| 1 | 起后端，钉版本、挂数据卷 | curl 往 4318 发一条测试 span，Grafana 里查得到；`docker restart` 后还在 |
| 2 | 加依赖，`init_tracing` 接两层，`[telemetry]` 配置 | 冷编仍在预算内（`--timings`）；`cargo tree -d` 无新重复；不配端点时行为与文件日志不变 |
| 3 | `invoke_agent` / `komo.round` / `chat` / `execute_tool` 埋点与模型、工具指标 | 测试：内存 exporter 断言两轮 Run 的 span 树与 token 属性、一次失败重试是兄弟 `chat` span、服务商没报的 token 不出现为 0；本机跑一个 Run，Tempo 里看到整棵树 |
| 4 | 第 5 节的本地补齐 | 本机挑一个失败、一个完成的 Run，`run inspect` 逐项对得上 JSONL |
| 5 | 记忆、Cron、投递、MCP 埋点（记忆部分随 §9.10） | Grafana 里对应指标有数 |
| 6 | 日志单写、轮转、空闲降级 | 重启后只有一个文件在长；跨天出现轮转文件 |
| 7 | 看板 JSON 进仓库 | 导入后每个面板有数据 |

**落地后能回答的 §14 待验证项**：压缩值不值（`komo.compaction` + 缓存读写）、CJK token 估算偏差（估算 vs `gen_ai.usage.input_tokens`）、Anthropic 第 4 个缓存断点的命中（接上 Anthropic 后端后看缓存读写）。

## 10. 待定

- 链路采样：现在全采（个人用量很小），量上来再定。
- 记忆的采纳（`used`）依赖 §9.10 的提取调用返回 `used: [编号]`，那一条写进 §9.10 时一起定。
