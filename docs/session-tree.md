# Session 树（pi 对齐）：用 fork 表达，实现说明与待验证项

用户行为的设计决策在 `docs/komo_bot.md` §8.11；本文记录**新存储（pi-durable 提交模型，`docs/storage-durable.md`）下已经实现了什么、怎么验证、还差什么**。对照基线是 pi coding-agent 1.4.0（`core/session-manager`、`AgentSession.navigateTree`）。状态截至 2026-10-25。

2026-10-23 的第一版把树存成 events.jsonl 里带 `parentId` 的 pi v3 entry（`SessionEntry` 记录加 `komo` 执行元数据、`TreeState`、`tree_codec`）；2026-10-25 随存储重写**整体换成 fork**，用户行为、HTTP 路由与 TUI 命令不变，旧的 JSONL 树实现已在 v0.8 存储重写中移除，也不读旧会话。

## 1. 模型

pi 的"一个文件里多分支"在 pi-durable 里没有原生对应，对应物是 **fork**：

| pi / komo 概念 | 新存储里 |
|---|---|
| Session | `sessions` 行（对外 UUIDv7、标题、来源、生命周期）+ 一组 conversation（根 + 各分支）；`root_conversation`、`current_branch`、`record.leaf` |
| 分支 | 一个从父 conversation 的某条 entry 处 `fork(at = E)` 出来的子 conversation；子沿 `parent.at` 读父的 entry，不复制 |
| 在 entry E 之后分叉（编辑历史输入 = 在那条输入的父 entry 处分叉） | `fork_conversation(含 leaf 的 conversation, at = leaf)`，`current_branch` 指向它，编辑后的输入作为它的第一条 submission |
| `leafId`（当前位置） | **持久化在 `sessions` 行的 `record.leaf`**（偏离 pi：pi 只在内存里、重载时取文件最后一条）。`None` = 跟着当前分支的末端；"第一条之前"用哨兵 `BEFORE_FIRST = EntryId(0)`（真 entry 的 ID 从 2 起铸） |
| `/tree` | 按 session 找全部 conversation，交给 kernel 的 `pi_tree`（`komo-kernel/src/durable/pi_tree.rs`），把各 fork 挂在各自的 `parent.at` 上，拼出 pi 的树；entry id = 十进制 entry ID（一条 durable entry 可展开成 `"{id}"`、`"{id}:{n}"`） |
| `label` / `session_info` / `branch_summary` / `context_edit` | write entry（`label`、`session.info`、`branch.summary`、`context.edit`）；只展示的 write 不改上下文、忙时也立刻落 |
| 压缩 | `komo.compaction` entry，`head` = 第一条保留的 entry；`/new` 是 `komo.reset`（`head: "self"`） |
| `/fork`、`/clone` | **复制**：新 `sessions` 行 + 新根 conversation，可见 entry 逐条复制（`head` / `edits` 目标按 ID 重写）、blob 先复制并同步再提交；不复制 task、submission、live。原会话被回收后新会话仍可读，所以跨 Session 永远没有 `parent.at` 共享，purge 的判据只有一条 |

`komo_kernel::session` 里的 pi v3 `SessionEntry` / `SessionTree` 词汇留着，作为 `pi_tree` 的输出形状与导出（`komo session export`，`docs/komo_bot.md` §3）的读回验证用；存储与上下文推导不再经过它。**上下文推导**是 kernel 的纯函数（`komo-kernel/src/durable/context.rs`，pi-durable spec §2.1 规则：最新 head 起、edits 后者赢、结果挪到调用后、缺结果补错误、去掉 aborted / error），从当前分支的 conversation 沿 fork 链读。

## 2. 用户行为

- **导航不追加 entry**：只改 `sessions` 行（`record.leaf`）。选中 user / custom message 时 leaf = 它的上一条、`editor_text` = 原文；提交编辑后的输入时才 fork。重启后位置还在（偏离 pi 的"纯导航不保存位置"，D5 决定 11）。
- **提交**：输入路由到 `current_branch`。leaf 不在末端 → 同一次提交里 fork（leaf 恰是某 conversation 自己的最后一条时直接接在它后面，不 fork；leaf 在第一条之前就建新根 conversation）+ 复制 `komo.agent` + 清 `leaf` + 受理输入（`admit_in_tx`）；fork 只复制 `komo.agent`，不复制 live / inbox / task。
- **忙时守卫**：任一分支忙（`komo.live.run` 在、收件箱非空、压缩进行中）时导航、编辑、命名、compact、fork 都 409（`SessionError::Busy`；首版保留，决定 6）。Run 固定自己所在的 conversation，导航换的是"下一条输入去哪个分支"。
- **分支摘要**：沿旧路径找共同祖先，摘要（辅助模型 `LlmClient`，pi 的 `serializeConversation` 形状）作为 `branch.summary` entry 接到目标位置，在目标处 fork。
- **`/compact`**：`Harness::compact(current_branch, instructions)` + 等压缩任务；leaf 不在末端先 fork。摘要是 write submission，`compact` 从任务结果的 `submissionId` 反查摘要 entry。
- **与旧行为的差异**（相对 2026-10-23 之前）：历史 Run 的工具往返随当前分支回放，直到压缩或 `context_edit` 收掉；压缩覆盖整条分支，不再单独保留开头那句任务；每个 Run 固定自己的 conversation；fork 里没有 `seq` 冻结前缀。

## 3. 接口与命令

| 路由 | TUI 命令 |
|---|---|
| `GET /v1/sessions/{id}/tree` | `/tree` |
| `POST …/navigate` | `/tree <id> [--summarize]` |
| `POST …/fork`（`clone: true` 为复制当前路径） | `/fork [id]`、`/clone` |
| `POST …/context-edit` | 无直接命令（客户端 API 已有） |
| `POST …/label` | `/label <id> [标签]` |
| `POST …/name` | `/name <名称>` |
| `POST …/compact` | `/compact [指示]` |
| `GET …/export` | `komo session export`（pi v3 JSONL，leaf 落在文件末尾） |

实现：`komo-gateway/src/service/durable/{sessions,submit,tree,copy}.rs`（服务层，`impl DurableCore`）与 `http/v2/{sessions,tree,runs,export}.rs`；`SessionError` → `ApiFailure` 在 `http/error.rs`（404 / 400 / 409）。

## 4. 自动化验证

`komo-gateway/src/http/session_tree/tests.rs`（走真实 `service::start`、真实 HTTP、真实 `durable.db`，模型是脚本化替身；`seed` 经 `DurableCore` 提交并返回真实 entry ID）：

| 场景 | 测试 |
|---|---|
| 编辑历史输入形成兄弟分支，模型只看新路径 | `navigating_and_editing_an_input_creates_a_sibling_that_alone_reaches_the_model` |
| 重启后分支与位置不变，之后的请求仍只带当前路径 | `a_branch_and_its_current_position_survive_a_gateway_restart` |
| 有未结束 Run 时导航、压缩、命名、fork 被拒（409） | `tree_changes_are_refused_while_a_run_is_unfinished` |
| clone 后回收源会话，新会话仍可读且没有 task / submission | `cloned_messages_and_artifacts_survive_source_purge_without_copying_executable_runs` |
| 标签、命名、`context_edit` 只改模型视图，原记录保留 | `context_edits_and_labels_preserve_raw_history_and_apply_only_on_the_selected_path` |
| 分支摘要与压缩落成 entry | `summaries_and_compaction_are_entries_on_the_new_path` |

服务层：`session_tests.rs` 的 `editing_an_input_forks_at_its_parent_on_submit`、`navigation_alone_appends_nothing_and_persists_the_leaf`、`clone_copies_entries_and_blobs_and_survives_source_purge`、`fork_copies_komo_agent_into_the_branch`、`tree_ops_are_refused_while_any_branch_is_busy`、`compact_runs_on_the_current_branch_and_waits_for_the_summary`；kernel：`pi_tree_round_trips_through_session_tree` 与导出的四个往返用例（`export_round_trips_through_session_tree_with_branches_and_leaf` 等：分支数、leaf、标签、名字与 leaf 处推导出的上下文都与 `/tree` 一致）。

## 5. 仍未验证

- 真实模型 provider（只用了脚本化替身）；真人在交互式 TUI 里的操作（客户端只有单元与线格式测试）。
- 导出文件用 pi 的 `SessionManager.open` 真读：本机没有 node，只对着 `session-manager.ts`（8efd314fa）逐点核对过（`docs/storage-durable.md` §9 D6）。
- 被取消 / 失败留下的半轮调用在完整回放下不触发 provider 400：上下文推导"缺结果补错误"只有单元与 conformance 用例，没有真实 provider 验证。

## 6. 已知取舍

- 导航持久化（`record.leaf`）偏离 pi；导出时按 pi 的"末条即 leaf"约定安排（`docs/storage-durable.md` §9 D6）。
- 工具结果的 pi message 内容是存下来的模型视图；完整输出仍由 `tool-output/` 引用承载，`/fork` `/clone` 复制整个相关目录。
- 压缩不再钉住开头任务，长单任务依赖摘要保留任务原文要点。
- 会话列表与按 origin 查会话目前是全表扫描（TODO）。
