# Session 树（pi 对齐）：实现说明与待验证项

设计决策在 `docs/komo_bot.md` §8.11；本文记录**已经实现了什么、怎么验证、还差什么**。对照基线是 pi coding-agent 1.4.0（`core/session-manager`、`AgentSession.navigateTree`）。状态截至 2026-10-23。

## 1. 已实现

### 1.1 模块与职责

| 位置 | 内容 |
|---|---|
| `komo-kernel/src/session/` | 纯消息树：`SessionEntry` / `EntryKind`（pi v3 的 11 种 entry）、`SessionTree`（append、branch、path、context_entries、project、extract、label、name）。无 I/O。 |
| `komo-kernel/src/session_events.rs` | `TreeState`：从执行事件重建树；排队输入的锚点（`input-<run>`）、插话并入位置、每个 Run 的 head。 |
| `komo-kernel/src/events/tree_codec.rs` | 记录编码：pi entry 加 `komo` 执行元数据，`v: 2`；正文只在 entry 里存一份。 |
| `komo-store` | `SessionLog` 写 pi header（`version: 3`），追加时算出 entry；`Coordinator` 的会话级锁、`ensure_idle`、`append_session_entry`、`index_imported`。 |
| `komo-agent/src/context/tree.rs` | 沿当前分支选路径、跨 Run 压缩、`context_edit`、system/tool 检查点、半轮调用修复。 |
| `komo-gateway/src/http/session_tree.rs` | HTTP 路由（见 1.3）。 |
| `komo-client` | 树浏览器（`tree_picker.rs`、`app/tree.rs`）与斜杠命令。 |

### 1.2 与旧行为的差异

- 历史 Run 的工具往返随当前分支回放，直到压缩或 `context_edit` 收掉。
- 压缩覆盖整条分支，不再单独保留开头那句任务；摘要取代切点之前的一切（与 pi 一致）。
- 每个 Run 固定自己的路径（`TreeState::head_for`）；UI 的 `leaf` 不影响恢复。
- fork 任务的冻结前缀 `seq` 清零，不进入本会话的压缩切点。
- 新记录旧二进制读不了（`v: 2`），不会把兄弟分支拼成线性历史。

### 1.3 接口与命令

| 路由 | TUI 命令 |
|---|---|
| `GET /v1/sessions/{id}/tree` | `/tree` |
| `POST …/navigate` | `/tree <id> [--summarize]` |
| `POST …/fork`（`clone: true` 为复制当前路径） | `/fork [id]`、`/clone` |
| `POST …/context-edit` | 无直接命令（客户端 API 已有） |
| `POST …/label` | `/label <id> [标签]` |
| `POST …/name` | `/name <名称>` |
| `POST …/compact` | `/compact [指示]` |

导航、编辑、标签、命名、压缩、fork 都先取会话锁并要求没有未结束的 Run。

### 1.4 已有的自动化验证

`cargo test --workspace --no-fail-fast` 全部通过。与本特性直接相关：

- kernel：`session::tests`（压缩、retain-none、context_edit、分支、环）、`session_events::tests`（排队锚点、插话、导航不改恢复路径、完成事件不复制正文）。
- agent：`context::tree::tests`。
- gateway：`http::session_tree::tests`（分支只让新路径进模型、编辑/标签/命名、clone 在源会话清除后仍可读、摘要与压缩落成 entry）。
- client：311 项，含 API 线格式与 TUI 状态。

## 2. 验证结果与剩余缺口

### 2.1 端到端场景（已用集成测试覆盖，2026-10-23）

测试在 `komo-gateway/src/http/session_tree/tests.rs`，走真实 `service::start`、真实 HTTP、真实 state.db 与 JSONL，模型是脚本化替身。

| 场景 | 测试 |
|---|---|
| 编辑历史输入形成兄弟分支，模型只看新路径 | `navigating_and_editing_an_input_creates_a_sibling_that_alone_reaches_the_model` |
| 重启后分支与位置不变，之后的请求仍只带当前路径 | `a_branch_and_its_current_position_survive_a_gateway_restart` |
| 有未结束 Run 时导航、压缩、命名、fork 被拒（409） | `tree_changes_are_refused_while_a_run_is_unfinished` |
| clone 后清除源会话，新会话仍可读且无可执行 Run | `cloned_messages_and_artifacts_survive_source_purge_without_copying_executable_runs` |
| 标签、命名、`context_edit` 只改模型视图，原记录保留 | `context_edits_and_labels_preserve_raw_history_and_apply_only_on_the_selected_path` |
| 分支摘要与压缩落成 entry | `summaries_and_compaction_are_entries_on_the_new_path` |
| 执行恢复（重启接续、调用不重放） | 既有 `tests/recovery`、`tests/agents` 等，树格式下全部通过 |

### 2.2 手动分支摘要与 `/compact`（已完成）

摘要输入改为 pi 的 `serializeConversation` 形状（`komo_agent::context::compaction::serialize_conversation`）：`[User]` / `[Assistant]` / `[Assistant tool calls]` / `[Tool result]`，思考块不进，工具结果截到 2000 字符；系统提示用 pi 的结构化摘要格式（`TREE_SUMMARY_INSTRUCTIONS`）。分支摘要只序列化被放弃的那一段（重新挂根后投影，不带共享祖先）；`/compact` 序列化整条当前分支的投影。

### 2.3 旧会话（已用手写旧格式夹具验证）

`a_pre_tree_session_is_browsable_without_rewriting_and_continues_in_the_same_tree`：只读浏览不改文件；继续对话后旧字节是前缀、旧上下文进模型；新记录 `v:2` 且无 `seq`，旧二进制按线性日志解析会失败。

### 2.4 设计文档（已完成）

`docs/komo_bot.md` §8.3 的回放规则已改写为按分支路径，压缩段落标注了新范围，§8.11 写明状态，§14 增加了“Session 树验收”。

### 2.5 仍未验证

- 真实模型 provider（只用了脚本化替身）。
- 真人在交互式 TUI 里的操作：客户端只有单元与线格式测试（311 项），没有驱动真实终端。
- 线上积累的真实旧会话目录：只有按旧格式手写的夹具。
- 被取消 / 失败留下的半轮调用在完整回放下不触发 provider 400：只有 `repair_pending` 的单元测试，没有真实 provider 验证。

## 3. 已知取舍

- 纯导航（未追加 entry 就退出）不保存浏览位置，与 pi 一致。
- 工具结果的 pi message 内容是投影后的文本；完整输出仍由 `tool-output/` 引用承载，fork 时整目录复制。
- 压缩不再钉住开头任务，长单任务依赖摘要保留任务原文要点。
