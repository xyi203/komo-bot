# Session 树（pi 对齐）：实现说明与待验证项

设计决策在 `docs/komo_bot.md` §8.11；本文记录**已经实现了什么、怎么验证、还差什么**。对照基线是 pi coding-agent 1.4.0（`core/session-manager`、`AgentSession.navigateTree`）。状态截至 2026-10-23，分支 `feat/session-tree`。

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

## 2. 待验证与未完成

### 2.1 真机端到端（未做）

用真实 Gateway 与 TUI 走一遍，期望都成立才算验证：

1. 发两轮对话，`/tree` 选第一条用户消息：输入框回填原文，提交后出现兄弟分支；旧分支仍在树里；模型请求里没有旧分支内容。
2. 切回旧分支继续对话，两条线互不串上下文。
3. 一条 Run 运行中重启 Gateway：恢复后按该 Run 自己的路径继续，工具不重放。
4. `/clone`、`/fork` 后对源会话 `delete` + `purge`：新会话仍能读历史、工具输出和产物。
5. 运行中、压缩中执行 `/tree`：被拒绝并给出原因。

### 2.2 手动分支摘要与 `/compact`（未做）

`session_tree.rs::summarize` 把原始 entry 的 JSON 逐行当文本发给模型。应改为经 `compaction::assemble_tree_context` 取已投影的上下文，再用 `context::compaction::summary_request` 构造请求，使摘要输入与模型实际看到的一致（含工具结果投影与图片）。验收：摘要请求里不出现 entry 的 JSON 字段名，工具结果按投影视图出现。

### 2.3 旧会话迁移（仅单元覆盖）

旧记录（无 entry）读取时由 `TreeState` 现算 entry。需要用真实旧目录验证：

1. 复制一份有工具调用、插话、压缩、已 `approval.*` 事件的旧会话。
2. 只读浏览（`GET …/tree`、`events`）不改写文件。
3. 在其上继续对话后，旧部分与新部分在同一棵树里，`resume` 正常。
4. 旧二进制打开含新记录的会话时明确报错，而不是静默读出线性历史。

### 2.4 设计文档里的旧规则（未改）

`docs/komo_bot.md` §8.3“历史 Run 只发布两件事…”及“压缩只处理当前 Run”相关段落正文还在，与 §1.2 冲突；§14 也没有对应验收行。需要：在这些段落标注“已被 §8.11 取代”并改写，在 §14 加入 2.1–2.3 的验收条目和结果。

## 3. 已知取舍

- 纯导航（未追加 entry 就退出）不保存浏览位置，与 pi 一致。
- 工具结果的 pi message 内容是投影后的文本；完整输出仍由 `tool-output/` 引用承载，fork 时整目录复制。
- 压缩不再钉住开头任务，长单任务依赖摘要保留任务原文要点。
