# Session 树（pi 对齐）：用 fork 表达，实现说明与待验证项

> 状态：已实施的方案记录（2026-10-25 换成 fork 后的实现、验证与待验证项）；用户行为的规格在 `docs/komo_bot.md` §8.11。**2026-11-12：会话树的用户入口已删除**（导航、`--summarize` 分支摘要、`/label`、`/context-edit`、`/fork`、`/clone`、`/compact`，随 TUI 一起；见 `docs/tasks.md` §5、D5）。下文凡描述这些功能的段落都带"已删除"标记，保留作为存储层（`fork_conversation`、`pi_tree`、导出）的由来记录。

用户行为的设计决策在 `docs/komo_bot.md` §8.11；本文记录**新存储（pi-durable 提交模型，`docs/storage-durable.md`）下已经实现了什么、怎么验证、还差什么**。对照基线是 pi coding-agent 1.4.0（`core/session-manager`、`AgentSession.navigateTree`）。状态截至 2026-10-25。

2026-10-23 的第一版把树存成 events.jsonl 里带 `parentId` 的 pi v3 entry（`SessionEntry` 记录加 `komo` 执行元数据、`TreeState`、`tree_codec`）；2026-10-25 随存储重写**整体换成 fork**，用户行为、HTTP 路由与 TUI 命令不变（这些路由与命令现已大半删除，见上），旧的 JSONL 树实现已在 v0.8 存储重写中移除，也不读旧会话。

## 1. 模型

pi 的"一个文件里多分支"在 pi-durable 里没有原生对应，对应物是 **fork**：

| pi / komo 概念 | 新存储里 |
|---|---|
| Session | `sessions` 行（对外 UUIDv7、标题、来源、生命周期）+ 一组 conversation（根 + 各分支）；`root_conversation`、`current_branch`、`record.leaf` |
| 分支 | 一个从父 conversation 的某条 entry 处 `fork(at = E)` 出来的子 conversation；子沿 `parent.at` 读父的 entry，不复制 |
| 在 entry E 之后分叉（编辑历史输入 = 在那条输入的父 entry 处分叉） | `fork_conversation(含 leaf 的 conversation, at = leaf)`，`current_branch` 指向它，编辑后的输入作为它的第一条 submission |
| `leafId`（当前位置） | **持久化在 `sessions` 行的 `record.leaf`**（偏离 pi：pi 只在内存里、重载时取文件最后一条）。`None` = 跟着当前分支的末端；"第一条之前"用哨兵 `BEFORE_FIRST = EntryId(0)`（真 entry 的 ID 从 2 起铸） |
| `GET …/tree` | 按 session 找全部 conversation，交给 kernel 的 `pi_tree`（`komo-kernel/src/durable/pi_tree.rs`），把各 fork 挂在各自的 `parent.at` 上，拼出 pi 的树；entry id = 十进制 entry ID（一条 durable entry 可展开成 `"{id}"`、`"{id}:{n}"`） |
| `label` / `session_info` / `branch_summary` / `context_edit` | write entry（`label`、`session.info`、`branch.summary`、`context.edit`）；只展示的 write 不改上下文、忙时也立刻落。**现在只有 `session.info`（`name`）还有写入方**；其余三种已无用户入口，旧会话里已有的条目照常被 `pi_tree`、导出与上下文推导读出 |
| 压缩 | `komo.compaction` entry，`head` = 第一条保留的 entry；`/new` 是 `komo.reset`（`head: "self"`） |
| 网页会话菜单的"分叉"（`POST …/fork`；`/fork [id]`、`/clone` 已删除） | **复制**：新 `sessions` 行 + 新根 conversation，可见 entry 逐条复制（`head` / `edits` 目标按 ID 重写）、blob 先复制并同步再提交；不复制 task、submission、live。原会话被回收后新会话仍可读，所以跨 Session 永远没有 `parent.at` 共享，purge 的判据只有一条 |

`komo_kernel::session` 里的 pi v3 `SessionEntry` / `SessionTree` 词汇留着，作为 `pi_tree` 的输出形状与导出（`komo session export`，`docs/komo_bot.md` §3）的读回验证用；存储与上下文推导不再经过它。**上下文推导**是 kernel 的纯函数（`komo-kernel/src/durable/context.rs`，pi-durable spec §2.1 规则：最新 head 起、edits 后者赢、结果挪到调用后、缺结果补错误、去掉 aborted / error），从当前分支的 conversation 沿 fork 链读。

## 2. 用户行为

> **已删除（2026-11-12）**：导航、编辑历史输入重发、分支摘要、`/compact`（下面"导航""提交"里的 leaf 分叉、"分支摘要""`/compact`"各条）。仍在的是提交落在当前分支、忙时守卫（现在只管 `name` 与 `fork`）和 `fork`。下面的原文保留，说明存储层为什么长成这样。`record.leaf` 没有写入方了，只在更早的会话行里可能有值；提交与改名对它的落点逻辑还在，导出与 `/tree` 照读它。

- **导航不追加 entry**：只改 `sessions` 行（`record.leaf`）。选中 user / custom message 时 leaf = 它的上一条、`editor_text` = 原文；提交编辑后的输入时才 fork。重启后位置还在（偏离 pi 的"纯导航不保存位置"，D5 决定 11）。
- **提交**：输入路由到 `current_branch`。leaf 不在末端 → 同一次提交里 fork（leaf 恰是某 conversation 自己的最后一条时直接接在它后面，不 fork；leaf 在第一条之前就建新根 conversation）+ 复制 `komo.agent` + 清 `leaf` + 受理输入（`admit_in_tx`）；fork 只复制 `komo.agent`，不复制 live / inbox / task。
- **忙时守卫**：任一分支忙（`komo.live.run` 在、收件箱非空、压缩进行中）时导航、编辑、compact、fork 都 409（`SessionError::Busy`；首版保留，决定 6）。命名例外（2026-11-03）：忙时不追加 `session.info`、不动树，只在一次提交里改 `sessions.title`（同自动起标题）。标题是给人看的，不该等 Run 停下；空闲时照旧落 `session.info`。Run 固定自己所在的 conversation，导航换的是"下一条输入去哪个分支"。
- **分支摘要**：沿旧路径找共同祖先，摘要（辅助模型 `LlmClient`，pi 的 `serializeConversation` 形状）作为 `branch.summary` entry 接到目标位置，在目标处 fork。
- **`/compact`**：`Harness::compact(current_branch, instructions)` + 等压缩任务；leaf 不在末端先 fork。摘要是 write submission，`compact` 从任务结果的 `submissionId` 反查摘要 entry。
- **与旧行为的差异**（相对 2026-10-23 之前）：历史 Run 的工具往返随当前分支回放，直到压缩或 `context_edit` 收掉；压缩覆盖整条分支，不再单独保留开头那句任务；每个 Run 固定自己的 conversation；fork 里没有 `seq` 冻结前缀。

## 3. 接口与命令

现存的路由（调用方都在网页或 CLI）：

| 路由 | 调用方 |
|---|---|
| `GET /v1/sessions/{id}/tree` | 网页右栏的上下文构成（沿 `leafId` 读，`web.md` §6）、导出 |
| `POST …/fork`（无请求体：从当前位置复制整条路径；末条是用户输入时复制到它的上一条） | 网页会话菜单"分叉" |
| `POST …/name` | 网页顶栏改名 |
| `GET …/export` | `komo session export`（pi v3 JSONL，leaf 落在文件末尾） |

**已删除（2026-11-12）**：`POST …/navigate`（`/tree <id> [--summarize]`）、`POST …/context-edit`、`POST …/label`（`/label`）、`POST …/compact`（`/compact`）、`/fork [id]` 与 `/clone`（`fork` 路由的 `target_id` / `clone` 字段）、`POST …/resume`、`POST …/boundary`（渠道的 `/new` 走服务层，不经 HTTP）。这些路由的调用方只有 `komo-client` 的 TUI，网页 `packages/api` 没有。

实现：`komo-gateway/src/service/durable/sessions/{mod,tree,copy}.rs`、`submit/mod.rs`（服务层，`impl DurableCore`）与 `http/v2/{sessions,tree,runs,export}.rs`；`SessionError` → `ApiFailure` 在 `http/error.rs`（404 / 400 / 409）。

## 4. 自动化验证

`komo-gateway/src/http/v2/tree_tests.rs`（走真实 `service::start`、真实 HTTP、真实 `durable.db`，模型是脚本化替身；`seed` 经 `DurableCore` 提交并返回真实 entry ID）：

| 场景 | 测试 |
|---|---|
| 命名是一条 `session.info` entry，原记录保留 | `naming_a_session_is_an_entry_that_leaves_history_alone` |
| 有未结束 Run 时 fork 被拒（409），命名不等 Run | `tree_changes_are_refused_while_a_run_is_unfinished` |
| fork 后回收源会话，新会话仍可读且没有 task / submission | `forked_messages_and_artifacts_survive_source_purge_without_copying_executable_runs` |

**已删除（2026-11-12，随功能一起）**：编辑历史输入形成兄弟分支（`navigating_and_editing_an_input_creates_a_sibling_that_alone_reaches_the_model`）、重启后分支与导航位置不变（`a_branch_and_its_current_position_survive_a_gateway_restart`）、标签 / `context_edit` 只改模型视图（`context_edits_and_labels_preserve_raw_history_and_apply_only_on_the_selected_path`，命名那一半留下了）、分支摘要与压缩落成 entry（`summaries_and_compaction_are_entries_on_the_new_path`）。

服务层：`session_tests.rs` 的 `a_leaf_left_in_the_middle_forks_at_its_parent_on_submit`、`a_stored_leaf_survives_reopen_and_lands_the_next_submit`、`a_leaf_at_the_end_of_another_branch_continues_that_branch_without_forking`、`the_same_request_key_is_one_submission_even_after_the_leaf_moved`、`landing_without_the_tree_matches_landing_with_it_over_random_sequences`（以上几条没有了导航入口，测试用 `leave_leaf_at` 直接写 `record.leaf`，覆盖的是"更早的会话行里留着 leaf"时的落点）、`fork_copies_komo_agent_into_the_branch`、`fork_copies_to_the_leaf_and_stops_before_a_trailing_input`、`fork_copies_entries_and_blobs_and_survives_source_purge`、`copies_rewrite_heads_edit_targets_and_the_ids_gateway_entries_refer_to`、`tree_ops_are_refused_while_any_branch_is_busy`、`naming_appends_session_info_and_renames_in_one_commit`（删除的：`a_branch_summary_forks_at_the_target_and_records_where_it_came_from`、`labels_names_and_context_edits_are_entries_that_leave_history_alone`、`compact_runs_on_the_current_branch_and_waits_for_the_summary`、`compact_after_navigation_works_on_a_fresh_branch`、`fork_stops_before_the_selected_input_and_clone_keeps_it`）；kernel：`pi_tree_round_trips_through_session_tree` 与导出的四个往返用例（`export_round_trips_through_session_tree_with_branches_and_leaf` 等：分支数、leaf、标签、名字与 leaf 处推导出的上下文都与 `/tree` 一致；分支、标签、上下文编辑、分支摘要、压缩条目现在经提交线直接写入）。

## 5. 仍未验证

- 真实模型 provider（只用了脚本化替身）；
- 导出文件用 pi 的 `SessionManager.open` 真读：本机没有 node，只对着 `session-manager.ts`（8efd314fa）逐点核对过（`docs/storage-durable.md` §9 D6）。
- 被取消 / 失败留下的半轮调用在完整回放下不触发 provider 400：上下文推导"缺结果补错误"只有单元与 conformance 用例，没有真实 provider 验证。

## 6. 已知取舍

- 导航持久化（`record.leaf`）偏离 pi（导航已删，字段只在旧会话行里有值）；导出时按 pi 的"末条即 leaf"约定安排（`docs/storage-durable.md` §9 D6）。
- 工具结果的 pi message 内容是存下来的模型视图；完整输出仍由 `tool-output/` 引用承载，fork 复制整个相关目录。
- 压缩不再钉住开头任务，长单任务依赖摘要保留任务原文要点。
- 会话列表与按 origin 查会话目前是全表扫描（TODO）。
