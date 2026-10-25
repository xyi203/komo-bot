# D5 计划：把新存储与新 harness 接到 gateway

> 状态：**执行中（2026-10-25）**。本文是 D5 的工作包拆分与已拍板的决定；`docs/storage-durable.md` 是设计，冲突时以本文 §9 的决定为准（D7 回写设计文档）。每个工作包完成时在 §10 记一行。

## 1. 切换策略

- `ServiceOptions` 加 `storage: StorageMode { Legacy, Durable }` 与 `stream: Option<Arc<dyn ModelStream>>`。生产默认 `Legacy`，**W5 翻成 `Durable`**。测试有 `TestGateway::durable(..)` / builder `.durable()`，各包把自己负责的套件迁过去。
- Durable 模式启动：打开 `<data_dir>/durable.db`（`TursoStorage::open`）；cron / 投递 / memory / 幂等键等辅助表开**新的** toasty 库 `<data_dir>/aux.db`；**不打开旧 `state.db`**，不跑旧 scheduler / reconcile / audit outbox / `reattach_unfinished`。
- 新代码放新文件：HTTP 走 `http/v2/`（没改的路由——memories、config、models、mcp、healthz——直接复用旧 handler）；渠道 `Dispatcher::handle` 开头按模式分到 `dispatcher_v2.rs`；后台循环分到 `service/durable/background.rs`。D7 整文件删旧代码。
- 每波合并后 `cargo test --workspace` 必须全绿：Legacy 套件照旧，已迁移的跑 Durable。
- **协议冻结**：`komo_kernel::protocol::{http,session,sse}` 不改，`komo-client` 不动（D6 的 export 调用是唯一加法）。

| 项 | D5 处理 |
|---|---|
| `SseEvent::Event(Event)` | gateway 从 entry / submission 投影合成旧事件词汇（§5） |
| `RunId` | 这条 run **首个输入 submission 的十进制 ID** |
| `Seq`（SSE `id:`、`Event.seq`、`EventPage.next`、`SubmitRunResponse.seq`） | `(记录 ID << 12) \| k`，记录 ID 是 entry 或 submission 的全局单调 ID，`k` 是同一记录产出的第几帧 |
| `SessionSummary.current_wait` | 只会出现 `Retry` |
| `InterventionSummary` | 只列 faulted 任务，映射成 `Blocked`，可答结论只有 `abandon` |
| `POST /v1/sessions/{id}/resume`、`POST /v1/reconcile` | 保留路由；resume 只返回未结束的 run，reconcile 只做 blob 引用检查 |

## 2. 共用契约（各包不得各自发明）

- **SessionRecord 扩展**（kernel `durable/records.rs`，只在 `record` JSON 里加，不改 DDL）：`kind`（`main` / `normal` / `task` / `cron`）、`agent`、`source`（origin，如 `chat:telegram:123`）、`parent_session`、`leaf: Option<EntryId>`、`title_provisional`、`created_at` / `changed_at`（毫秒）、`request_key`。
- **ConversationRecord** 加 `session: Option<String>`，写进 `conversations.session_id` 列；fork 出来的子 conversation 继承父的 session。komo 的每个 Session 用 `create_conversation(Ownerless)` 建根（不用固定占 ID 1 的 `create_root_conversation`）。
- **`komo.agent` document**（conversation 级，gateway 写）：`{ agent, model?, effort?, workdir?, maxRounds?, source? }`。会话内分支 fork 时同一提交复制给子 conversation；任务子 conversation 没有它，钩子沿 `komo.task.parent` 回退到父的。
- **gateway 写的 entry kind**（`komo.` 前缀留给 harness）：`chat.input`（`data {peer, principal}`）、`cli.input`、`cron.input`（`data {job, slot}`）、`shell.command`（带 `model` 的 write）、`label`、`session.info`、`branch.summary`（带 `model`，`data.fromId`）、`context.edit`（带 `edits`）、`model.change`。
- **run 结算锚点**：`inbox::settle` 每结算一个**输入** submission，同提交追加一条只展示的 entry `komo.settled`，`data {submission, status: "done"|"unanswered", reason?, answer?}`。
- **ID 编解码**在 kernel `durable/legacy_ids.rs`：`run_id(SubmissionId) -> RunId`、`submission_of(&RunId)`、`seq(id, k)`、`record_of(Seq)`。
- **`DurableCore`**（`service/durable/core.rs`）：`{ engine, harness, storage: Arc<dyn Storage>, clock, home }`；`open(home, clock, stream, hooks, orchestration, codemode)`、`close()`、`wait_submission(id)`；挂在 `GatewayState.durable: Option<Arc<DurableCore>>`；钩子与 `GatewayState` 的环用 `OnceLock<Weak<GatewayState>>` 断开（照 `wire_task_spawner`）。
- **受保护路径**：`durable.db*`、`aux.db*` 加进 `write` / `edit` 拒写与 codemode 不可读名单（`state::protected_paths` / `codemode_unreadable_paths`）。
- **blob 目录**：沿用 `<data_dir>/sessions/{uuid}/`；工具输出 `tool-output/{conversation}/{tool_task}/{attempt}/`。

## 3. 波次

| 波 | 包（并行） | 依赖 |
|---|---|---|
| 0 | **W0a** 存储接缝 ∥ **W0b** gateway 骨架与测试脚手架 | — |
| 1 | **W1a** kernel 投影 ∥ **W1b** gateway 钩子 ∥ **W1c** 会话与树服务层 | W0 |
| 2 | **W2a** HTTP v2 会话 / run / 树 ∥ **W2b** 提交订阅与 SSE ∥ **W2c** 后台任务与监听 | W1 |
| 3 | **W3a** 渠道、分发、看客、投递 ∥ **W3b** cron ∥ **W3c** memory | W2 |
| 4 | **W4a** 生命周期、intervention、恢复 ∥ **W4b** 剩余套件迁移 ∥ **W4c** D6 导出 | W3（W4c 只依赖 W1a + W1c + W2a） |
| 5 | **W5** 翻默认、收尾 | 全部 |

每包只跑自己相关的测试；`cargo test --workspace` 由主控在合并时跑。每个 worktree 用自己的 `target/`，`CARGO_BUILD_JOBS` 按主控指定。

## 4. 工作包

### W0a 存储接缝（kernel / store / runtime）

- `komo-kernel/src/durable/records.rs`：SessionRecord 扩展、`ConversationRecord.session`。
- `komo-kernel/src/durable/storage.rs`：`ConversationQuery` 加 `session`、`parent_conversation` 过滤；`StorageWrite::PurgeConversations { conversations }`（删这些 conversation 的 entry / task / submission / document 行与 conversation 行）。`memory.rs` 跟上；`conformance.rs` 新增 `conversations_scan_by_session_and_parent`、`purge_removes_every_row_of_the_listed_conversations`、`a_conversation_records_its_session`。
- 新文件 `komo-kernel/src/durable/legacy_ids.rs`（RunId / Seq 编解码）。
- `komo-store/src/durable/{storage,write}.rs`：写 `session_id` 列；新查询走已有索引 `conversations_by_session` / `conversations_by_parent`；Purge 的 SQL。
- `komo-runtime/src/durable/tx.rs`：`put_session`、`session(id)`、`create_conversation_in(session, ownership)`、`purge_conversations`；fork 继承 session。
- `harness/inbox.rs`：`settle` 追加 `komo.settled`（kind 常量进 `harness/entries.rs`）。
- `harness/submit.rs`：`pub async fn admit_in_tx(&self, tx, conversation, draft) -> Result<Admitted>`，让 gateway 能在一次提交里"建 / fork conversation + 受理"。
- `harness/orchestration/task.rs`：派出的同一提交里给子 conversation 写 `sessions` 行（`kind=task`、UUIDv7、`parent_session` = 父 conversation 的 session、title、root 与 current_branch 都是子 conversation）。
- **决定 8（取消只停当前一轮）**：`Harness::abort_run(engine, conversation)`——只 abort 当前 run 的任务，**不撤回**收件箱；同时把非正常收尾（`report::end_run_abnormal`：取消、模型失败、预算、fault、orphan）的后继 boundary 改成放置**全部**排队输入（先 write，再内部与用户输入按 ID 序），恢复旧 komo"前一轮怎么结束都不挡后面的消息"。`Conversation.abort()`（`abort_conversation`）语义不变：先撤用户输入、保留内部输入。相应更新 `harness/tests/inbox.rs` 里"失败后用户 followUp 留在收件箱"的断言（理由写进提交说明）与 `docs/storage-durable.md` §13.2 那条偏离。
- 验收：conformance 内存与 Turso 都过；runtime `durable::` 全过（数 entry 的断言因 `komo.settled` 逐个改）；新增 `every_input_settlement_leaves_one_settled_entry`、`a_spawned_task_has_its_own_session_row`、`admit_in_tx_forks_and_places_in_one_commit`、`abort_run_keeps_queued_user_inputs_and_runs_them_next`、`a_failed_run_is_followed_by_the_queued_user_input`。

### W0b gateway 骨架与测试脚手架（只依赖 §2 的类型名）

- 新建 `service/durable/{mod,core,agent_doc,assemble,background}.rs`（`assemble::build_hooks` 先返回占位钩子，W1b 换掉；`agent_doc` 定义 `komo.agent` 读写与 fork 复制）；`http/v2/mod.rs`（合并各子模块 `routes()`，子模块先给空 `Router` 桩）。
- 改 `service/mod.rs`（`ServiceOptions.storage/stream`；`start()` 的 Durable 分支；`Running::stop` 里 `core.close()`）、`service/state.rs`（`durable` 字段；受保护路径）、`http/mod.rs`（按模式挑路由）、`service/test_support/{mod,harness}.rs`、`crates/komo/src/main.rs`（字面量补字段）。
- `FakeLlm` 同时实现 `ModelStream`：最后一条非 system 消息是用户输入就开新脚本、`turns += 1`、记一份 `TurnRequest` 形状的 `requests`；是工具结果就取当前脚本下一轮、记 `inputs`——大多数测试的 `requests[..].messages/system_prompt/tools` 与 `turns()` 断言不用改。`TestGateway::wait_run(&RunId)` 轮询 submission 状态。
- 验收：workspace 全绿（Legacy）；新增 `opens_and_closes_durable_db_beside_aux_db`、`durable_mode_never_opens_state_db`、`write_refuses_the_durable_databases`、`fake_llm_as_model_stream_keeps_turn_request_observables`。

### W1a kernel 投影（纯函数）

- `komo-kernel/src/durable/pi_tree.rs`：输入一个 Session 的各 conversation、各自的 entry、leaf；输出 `Vec<session::SessionEntry>` 与 `leaf_id`。entry id = 十进制 entry ID；`parentId` = 同 conversation 上一条，fork 首条指向 `parent.at`，新根 `null`。映射：user / assistant / tool-result → `Message`；`komo.compaction` → `Compaction`（`firstKeptEntryId` 取 head）；`komo.decay`、`context.edit` → `ContextEdit`；`label` → `Label`；`session.info` → `SessionInfo`；`branch.summary` → `BranchSummary`；`model.change` → `ModelChange`；`komo.system`、`komo.settled` 及其他 → `Custom`。
- `komo-kernel/src/durable/legacy_events.rs`：输入 entry + submission，输出 `Vec<Event>`，seq 按 §1 编码；每个 conversation 顺序折，输入开了 run 就把之后的 assistant / tool entry 归给它，直到它的 `komo.settled`。映射见 §5。
- 验收：`pi_tree_round_trips_through_session_tree`、`legacy_events_backfill_equals_live_projection`、`seq_is_strictly_increasing_per_session`、`a_withdrawn_queued_input_projects_as_cancelled`、`a_steer_projects_as_absorbed_into_the_current_run`。

### W1b gateway 钩子

- 新建 `service/durable/hooks/{mod,setup,tools,env,sections,compaction}.rs`、`service/durable/orchestration.rs`（`GatewayOrchestration`；`start/stop_monitor` 委托 `Option<Arc<dyn MonitorRunner>>`，没装配时干净失败）。改 `service/durable/assemble.rs`；`komo-agent/src/context/{mod,prompt}.rs` 公开 `system_sections(input) -> Vec<(key, text)>` 与 `task_contract_block`；`komo-runtime/src/config/file.rs` 加 `[compaction] background_tokens`（更新 `docs/komo_bot.md` §13.3 示例）。
- 从旧文件**复制、不改原文件**：`state.rs::freeze_run` 的 profile / 能力面 / 工作目录 / 模型解析，`segment.rs` 的 roots 与 `ResourceMounts`，`context_sources.rs` 的 identity 与 recall，`skills_block`、`project_files::discover`、`windows`。映射见 §6。
- 验收：`setup_resolves_task_doc_model_names_against_current_config`、`fork_task_reuses_parent_sections_and_cache_key`、`task_sessions_lack_task_and_monitor_in_schema`、`memory_section_is_pinned_until_the_next_head`、`a_withdrawn_mcp_tool_fails_with_its_reason`、`mcp_tools_in_surface_are_awaited`、`compaction_setup_maps_config_and_komo_agent_prompt`、`check_effort_uses_agent_default_model`、`golden_system_sections_match_legacy_prompt`（同一 profile 下系统段与旧 `assemble` 逐字相同）。

### W1c 会话与树服务层（不接 HTTP）

- 新建 `service/durable/sessions.rs`（建会话 = sessions 行 + 根 conversation + `komo.agent` 一次提交；列表；详情映射 `SessionSummary`；主会话与按 peer 找会话；input refusal）、`submit.rs`（输入路由到当前分支；leaf 不在末端时惰性 fork；model / effort 覆盖写成 sticky `komo.agent` + `model.change`；`project_update` 补在输入正文后）、`tree.rs`（navigate、label、name、context-edit、branch summary、compact = `Harness::compact` + `wait_for_task`、忙时守卫）、`copy.rs`（`/fork`、`/clone` 复制 entry——head 与 edits 目标按 ID 映射重写——与 blob，先复制同步再提交）。树映射见 §7。
- 验收：`editing_an_input_forks_at_its_parent_on_submit`、`navigation_alone_appends_nothing_and_persists_the_leaf`、`clone_copies_entries_and_blobs_and_survives_source_purge`、`fork_copies_komo_agent_into_the_branch`、`tree_ops_are_refused_while_any_branch_is_busy`、`compact_runs_on_the_current_branch_and_waits_for_the_summary`。

### W2a HTTP v2：会话、run、树

- 新建 `http/v2/{sessions,tree,runs}.rs`（create / list / detail / submit / boundary = `Harness::reset` / resume；tree / navigate / fork / context-edit / label / name / compact；run detail 由 submission + entry + `komo.live` 拼；cancel = `Harness::abort_run`，排队中的那条 = `Harness::withdraw`）、`service/durable/shell.rs`（`!` 命令直跑，决定 9）、`service/durable/runs.rs`（RunSummary / RunDetail / RunState 映射）。
- 改 `http/session_tree/tests.rs`（口径见 §7）。
- 验收：`session_tree/tests.rs` 6 个在 Durable 下过，删 `a_pre_tree_session_is_browsable_without_rewriting_and_continues_in_the_same_tree`（§8 不迁移）；`service/tests.rs` 迁到 Durable：`the_session_list_shows_the_first_message_as_its_title`、`the_same_request_key_with_other_content_is_a_conflict`、`slash_new_appends_a_boundary_to_the_same_session`、`a_bang_command_runs_without_the_model_and_lands_in_the_history`、`an_empty_bang_command_is_refused`、`a_completed_run_reports_end_time_duration_model_and_token_totals`、`a_run_without_reported_usage_leaves_token_totals_absent`、`a_failed_run_reports_why`、`everything_but_health_needs_the_token`；`tests/sessions` 迁到 Durable。

### W2b 提交订阅与 SSE

- 新建 `service/durable/feed.rs`、`http/v2/events.rs`（JSON 分页 + SSE 补读 + 直播）；`sse.rs` 加"非 Event 帧不按号去重"的入口（只加法）。`start()` 里 `feed::spawn` 一行由主控合并时补。
- 验收：`live_frames_equal_backfill_frames`、`assistant_delta_is_the_suffix_of_the_throttled_partial`、`frames_of_other_sessions_are_filtered`、"20 个并发补读不拖慢提交"冒烟；合并后 Durable 下过 `sse_replays_history_from_a_cursor_then_goes_live`、`an_end_to_end_turn_completes_and_reaches_the_event_stream`、`sse.rs` 现有 9 个单测。

### W2c 后台任务与监听（gateway 一半）

- 新建 `service/durable/monitor_runner.rs`（把 `monitors.rs` 的进程、泵、合批、时限、进程组、遗留进程核对搬成 `MonitorRunner`；事件 `Harness::submit(monitor::event_draft)`，结束 `finish_monitor`，启动时 `reconcile_monitors`）、`service/durable/tasks.rs`（任务会话展示，短号 `#<conversation>`）。改 `assemble.rs`（装 runner）、`service/monitors/detached.rs`（`bash` 转后台在 Durable 下交回 Harness，模式分支，不改 Legacy）。
- 验收：`tests/monitor` 8 个、`tests/detach` 8 个迁到 Durable（`a_restart_reports_an_unfinished_detached_command_as_unknown` 按新语义改写）；`tests/agents` 的 `a_task_session_cannot_spawn_another_task`、`a_fork_starts_from_the_parents_conversation_and_shares_its_prefix`、`a_fork_resumed_after_a_restart_sends_the_same_prefix`、`a_status_query_shows_what_a_running_task_is_doing`、`a_status_query_for_an_unknown_task_fails_cleanly`。

### W3a 渠道、分发、看客、投递

- 新建 `dispatcher_v2.rs`、`service/durable/watch.rs`（`run_watch` 的 Durable 版，重启后从未结算的输入 submission 重挂看客）、`service/durable/titles.rs`；改 `dispatcher.rs`（`handle` 开头按模式分支）、`channels/mod.rs`（只在直接读 `state.db` 处分支）。交回输入的来源对端按决定 6；`/new`、`/cancel`（= `abort_run`）、`/status`、`/pending` 走新存储；同一 peer 主会话忙时 `WhenBusy::Steer`。
- 验收：`tests/chat` 42 个迁到 Durable；`service/tests.rs` 渠道相关（`a_replayed_message_produces_one_run`、`a_plain_chat_message_is_accepted_as_an_interjection`、`thread_context_is_prepended_…`、`a_stranger_is_refused_…`、`a_deferred_delivery_is_flushed_…`）；`tests/agents` 的 `a_background_task_reports_back_through_home`、`a_chat_message_is_absorbed_into_a_running_home_and_its_channel_is_told`、`a_task_result_is_absorbed_into_a_running_home`、`a_follow_up_absorbed_into_a_busy_task_reports_back_once`；`dispatcher.rs`、`deliveries.rs` 单测不变。

### W3b cron

- `komo-runtime/src/scheduler/cron.rs`：`CronScheduler` 依赖的 `Arc<dyn Ledger>` 换成 runtime 里的新 trait `CronTarget`（`fire(spec) -> FiredRef`、`run_now`）。新建 `service/durable/cron.rs`（先在 `aux.db` 写 firing 行并预分配 session ID，再一次 engine 提交建会话 `kind=cron`——`komo.agent` 带模型覆盖与 `maxRounds`——并受理 `request_id=cron:{job}:{slot}`；session 已存在就跳过；命令型 Job 走 `shell.rs`；tick 的 spawn）、`http/v2/cron.rs`、`service/durable/cron_watch.rs`（按 `komo.settled` 决定投不投、回写 firing 状态）。
- 验收：`tests/cron` 25 个在 Durable 下过（`on_error_keeps_quiet_about_a_good_run_but_never_about_a_wait` 的 wait 改为失败，理由写进提交）。

### W3c memory

- 新建 `komo-runtime/src/memory/durable.rs`（`DurableSessionEvents` 实现 `SessionEvents`，内部调 W1a 的 `legacy_events`；`DurableMemoryWork` 实现 `MemoryWorkLog`，游标在 conversation 级 document `komo.memory_cursor`，`finish` 走 engine 提交）、`service/durable/memory.rs`（装配与 `MEMORY_TICK`；钉住那一块的 uses 记在 conversation 级 document `komo.memory`，由 W1b 的段写入时一并记）。改 `http/v2/runs.rs` 的 `memories` / `recall` 字段。
- 验收：`tests/memory` 27 个在 Durable 下过，含 `recall::the_memory_block_survives_a_restart_within_one_segment`。

### W4a 生命周期、intervention、恢复

- 新建 `service/durable/lifecycle.rs`（delete：`closing` → 撤回排队输入 → 对该 Session 每个 conversation `abort_conversation` → `deleted`；purge：要求已 `deleted`，`PurgeConversations` 提交后删目录，墓碑 `purged` 幂等；启动时做完停在 `closing` 的）、`service/durable/interventions.rs`（列 faulted 任务；`abandon` 记在 Session 级 document `komo.interventions.ack`）、`http/v2/interventions.rs`（含 reconcile 路由，只查 blob 引用）。
- **ID 不复用**（W0a 合并时发现，必须修）：Turso 重开后取号器从剩余最大 ID 起，purge 掉末尾 ID 后会重铸，违反设计 §6.1，且会让旧的 SSE 游标、memory 证据引用（`Seq` / `RunId`）指向新数据。把已分配过的最大 ID 持久化（如 `durable_meta` 的高水位，随分配同提交推进），开库时取号器从它起；加 conformance / Turso 用例`purged_ids_are_never_reissued_after_reopen`。
- 删除会话时 `abort_conversation` 保留内部输入，abort 之后仍会起后继 run：删除流程先 `withdraw` 该 Session 所有排队的内部输入（或在 `closing` 状态下拒绝起后继 run），并加测试。
- **重启时的订阅顺序**（W3a 合并时发现）：`start()` 里 `feed::spawn` 在 `background::spawn`（含 `core.resume()` 之后的 `watch::reattach`）之后，引擎恢复后到 feed 订阅之间产生的终态帧会丢；W3a 用"重启后把最近 64 条已结算输入再收尾一遍"兜住。把 feed 订阅提到引擎恢复之前，保留对"已结算、投递前崩溃"的补投（`deliver_once` 幂等），并决定 64 条窗口要不要换成按未投递标记查。
- 新增 `tests/recovery_durable/`：`the_first_end_to_end_acceptance`、`a_chat_run_in_flight_across_a_restart_still_gets_its_reply_delivered_once`、`a_background_task_in_flight_across_a_restart_still_reports_back`、`a_killed_bash_is_handed_to_the_model_as_interrupted`、`a_missing_blob_faults_the_tool_task_and_lists_it`。`service/tests.rs` 生命周期一组迁到 Durable。旧 `tests/recovery/{rows,verdicts,reconcile}.rs` 标记只跑 Legacy、D7 删。

### W4b 剩余套件迁移

- **skills 段冻结**（W3a 发现）：Durable 每次请求按当前注册表重算 skills 段，退避重发的请求会带上期间新增的 skill，前缀缓存作废。像 `project` / `memory` 一样钉住已展示的段到下一次 head，把 `a_resumed_segment_keeps_the_skills_catalog_it_was_accepted_with` 改回 `assert_eq!(resumed, first)`。另外决定 `tests/chat` 里 4 个建立在旧 `waiting + intervention` 上、W3a 没迁的测试（`an_intervention_reaches_both_the_source_and_home` 等）改写成 faulted 场景还是删除。
- **记忆提取按哪份配置**（W3c 发现）：新存储没有受理快照，提取时的记忆模式与 Agent 取提取那一刻的配置（`MemoryWorkItem.frozen`）。决定要不要把记忆模式冻进 `komo.agent`（受理时写），让提取按受理时的 Agent 配置；领取每拍全扫 Session，加待处理索引或留 TODO。
- `tests/{compaction,codemode,mcp,observation,resources}` 与 `tests/agents` 剩下的 profile / 身份测试迁到 Durable；"冻结身份"改成"每阶段按名字解析，已开始的请求用 checkpoint 里冻结的"；compaction 套件改用新的 `[compaction]` 与溢出救援；`home_answers_while_a_task_waits_on_a_person` 改成 faulted 场景或删除；`service/tests.rs` 的委派 / 审批残留标为只跑 Legacy。每个改写或删除的测试在提交说明里写理由。

### W4c D6 导出

- `http/v2/export.rs`：`GET /v1/sessions/{id}/export`，输出 pi v3 header + W1a `pi_tree` 结果，JSONL，带 blob 引用；`komo-client/src/api.rs` 一个调用；`crates/komo/src/commands.rs` 的 `komo session export`。
- 验收：`export_round_trips_through_session_tree_with_branches_and_leaf`；用 `/home/xyi203/code/pi` 的 session-manager 读一份导出文件，结果写回 `docs/storage-durable.md`。

### W5 翻默认、收尾

- `service/mod.rs` 默认 `Durable`；`TestGateway::with` 默认 Durable；Legacy 专用测试 `#[ignore = "legacy, D7"]` 或留给 D7 删。更新 `docs/storage-durable.md` §9 / §13、`docs/komo_bot.md` §14。
- 验收：`cargo test --workspace` 全绿、没有套件还跑 Legacy；`cargo build --timings` 记进文档；`install-local.sh` 冒烟（TUI 新建会话、`/tree`、编辑分支、重启后接着跑）。

## 5. SSE 投影

- `engine.line().subscribe_commits` 回调只把 `Publication` 送进 tokio mpsc（回调在线上同步执行，必须轻）。feed 任务维护 conversation → session、document ID → address 两张缓存。
- Entry 写与新建的 Submission 写 → `legacy_events` → `EventHub::publish_event`，`RunStatus` 照旧派生；`komo.live` 的 `DocumentChange` → 与上次发出的部分回复比较 → `AssistantDelta { run, round: rounds + 1, text: 后缀 }`，前缀对不上就从头发。

| 记录 | 产出 |
|---|---|
| submission S 新建 | `RunAccepted`（S,0），排队时再加 `RunQueued`（S,1） |
| 输入 entry E 放置 | 开新 run：`RunStarted`（E,0）+ `MessageUser`（E,1）；steer 并进：`RunAbsorbed{into}`（E,0）+ `MessageUser`（E,1） |
| assistant entry A | `MessageAssistant`（A,0）；第 i 个调用 `ToolPlanned` / `ToolStarted`（A,1+2i / A,2+2i） |
| 工具结果 entry T | `ToolResult`（T,0） |
| `komo.settled` X | `RunCompleted` / `RunFailed` / `RunCancelled`（X,0） |
| `komo.reset` | `ConversationBoundary` |
| `komo.compaction` / `komo.decay` | `ContextCompacted` |
| `model.change` | `ConfigChanged` |

- 补读：Session 全部 conversation，取各自 ID 大于 `record_of(cursor)` 的 entry 与 submission，合并排序后投影，丢掉 seq ≤ cursor 的帧，再接直播；补读与直播同一纯函数。`AssistantDelta` 与 `RunStatus` 不按号去重。

## 6. 钩子实现映射

| 钩子 | 靠哪段现有代码 | 要点 |
|---|---|---|
| `AgentHooks::setup` | `state.rs::freeze_run`、`agent_model` / `default_model_of`、`windows.rs`、`komo_agent::surface_of` | `komo.agent` → `komo.task`（任务会话）→ 默认 Agent；每次按当前配置解析；`task` 给的名字优先 |
| 系统段 | `komo_agent::context::prompt::build`（公开为 `system_sections`）、`skills_block`、`project_files::discover`、`context_sources::recall_for` | key：`instructions` / `base` / `project` / `skills` / `memory`；fork 任务返回 `replay_sections(context)`；`project` 与 `memory` 优先用已展示的那份；规范文件变了由 `submit.rs` 把 `project_update_block` 补进下一条输入 |
| `tools` / `ToolSet` | `ToolExecutor::tools()`、`refresh_mcp` / `tool_names`、`await_mcp` / `withdrawn_reason`（放宽为 pub）、`segment.rs` 的 `run_roots` / `ResourceMounts`、`config.execution.projection()` | MCP 工具有超时地等连上；撤掉的工具放 `WithdrawnTool` 桩，`prepare` 报原因；任务会话去掉 `task` 与 `monitor`；`ToolEnv.outputs` = Session 目录上的 `FileToolOutputStore`；`detach_after` 只给对话会话 |
| 模型 / 预算 | `config.model_catalog`、`max_rounds` / `max_retries`、cron Job 的 `maxRounds` | `cache_key` = 会话 ID，fork 任务用父的 |
| `AgentHooks::compaction` | `[compaction]`、`komo_agent::context::compaction` 的摘要提示 | 新增 `background_tokens` 键 |
| `check_effort` | `default_model_of` + `EffortCapabilities` | 只给 effort 时用 Agent 默认模型或任务上一次的模型 |
| `contract_block` | `komo_agent` 的 `task_contract_block` | 直接调用 |
| `start/stop_monitor` | `service/monitors.rs`、`monitors/detached.rs` | W2c；进程身份文件留在 `runtime/monitors/children/` |

## 7. 会话树映射与 `session_tree/tests.rs`

| 行为 | 落在哪里 |
|---|---|
| Session | `sessions` 行，`root_conversation`、`current_branch`、`record.leaf` |
| `/tree` | 按 session 找全部 conversation，交给 `pi_tree`；`leaf_id` = `leaf` 或当前分支末端 |
| 导航 | 只改 sessions 行，不追加 entry；选中 user 输入时 leaf = 它的上一条、`editor_text` = 原文；任一分支忙（`komo.live.run`、收件箱非空、压缩进行中）就 409 |
| 编辑成分支 | 提交时 leaf 不在末端：同一提交 `fork_conversation(含 leaf 的 conversation, at = leaf)`、复制 `komo.agent`、改 `current_branch` 并清 `leaf`，再 `admit_in_tx`；leaf 在第一条之前就建新根 conversation |
| `/fork`、`/clone` | 都**复制**：新 sessions 行 + 新根 conversation；可见 entry 逐条 `append_entry`（head / edits 目标按 ID 重写）；blob 先复制同步再提交；不复制 task、submission、live |
| label / name | 只展示 write：`label {targetId, label}`、`session.info {name}`；name 同提交更新 `sessions.title` |
| 分支摘要 | 沿用 `session_tree.rs` 的 `summarize`（aux `LlmClient`），在目标位置 fork，追加带 `model` 的 `branch.summary` |
| context-edit | 当前分支上一条 `context.edit` write |
| `/compact` | `Harness::compact(current_branch, instructions)` + `wait_for_task`；leaf 不在末端先 fork |

`session_tree/tests.rs` 口径（决定 1）：HTTP 调用与行为断言不变，只改帮手——`seed` 经 `DurableCore` 提交并返回真实 entry ID（`"u1"` 等换成返回值）；`finished` / `wait_done` 换 `TestGateway::wait_run`；`navigating_and_editing…` 末段改成重开 `DurableCore` 读 `/tree` 比 `leaf_id`；`cloned_messages_and_artifacts…` 的 `runs::list_for_session(..).is_empty()` 改为"新会话没有 task 与 submission"、`scan_records(..).len() == 2` 改为 `/tree` 里 message 有 2 条；`a_branch_and_its_current_position_survive_a_gateway_restart` 字面量补 `storage: Durable, stream: Some(llm)`；`Stalled` 补 `ModelStream` 实现；删 `a_pre_tree_session_is_browsable…`。

## 8. 渠道、投递、cron、intervention、生命周期、memory

| 领域 | 新存储下 |
|---|---|
| 渠道输入 | `dispatcher_v2`：认人不变；主会话按 `kind=main` + agent，群与话题按 `source`；`request_id` = 平台消息键；kind `chat.input`，`data.peer`；同一 peer 主会话忙时 `WhenBusy::Steer`。`channels/*` 收发、渲染、去重不变 |
| 回复与投递 | `watch.rs` 订阅 hub 投影帧，`RunCompleted` 时取首个输入的 `data.peer` 投回；`deliveries` 在 `aux.db`，先写后发，不变；重启时对未结算且带 peer 的输入重挂看客 |
| 交回输入的来源对端 | 决定 6 |
| cron | W3b；`cron_jobs` / `cron_firings` repo 不变（换到 `aux.db`） |
| intervention | faulted 任务列表 → `Blocked`、问题 = fault 原因、可答 `abandon`；`WaitReason` 不再产生 |
| 生命周期 | W4a |
| memory | 条目 / 证据 / 索引仍在 `aux.db`；提取输入经 `DurableSessionEvents`；注入是系统段 `memory`，uses 记在 `komo.memory` document；证据引用仍是 `Seq` / `RunId`（新编码） |
| 标题 | `titles.rs` 逻辑不变，读写 sessions 行的 `title` / `title_provisional` |

## 9. 已拍板的决定（2026-10-25）

| # | 问题 | 决定 |
|---|---|---|
| 1 | §8.11 端到端测试"原样通过"的口径 | HTTP 调用与行为断言不变，帮手函数可改；删 `a_pre_tree_session_is_browsable…` |
| 2 | 协议 | D5 冻结，gateway 投影合成旧事件；`RunId` / `Seq` 编码见 §1 |
| 3 | 结算锚点 | `inbox::settle` 追加 `komo.settled` |
| 4 | cron / 投递 / memory 的表 | 新 toasty 库 `aux.db`，从空开始；跨库靠预分配 ID 与请求键幂等；"同库同提交"推迟到 D7 之后 |
| 5 | 任务会话 sessions 行与短号 | runtime 在派出提交里原子写 sessions 行；短号统一 `#<conversation ID>` |
| 6 | 交回输入的来源对端 | 当前路径上最近一条带 peer 的用户输入的 peer，没有就退回 home chat |
| 7 | memory 注入形态 | 系统段 `memory`，uses 记在 `komo.memory` document |
| 8 | `/cancel` / Esc | **只停当前一轮**（用户确认）：`Harness::abort_run` 不撤回收件箱；任何非正常收尾后，排队的输入（write、内部、用户）都接着放置开跑——恢复旧 komo 行为。排队中的单条用 `Harness::withdraw` |
| 9 | `!` 命令与命令型 cron | gateway 直接用 bash 工具跑，不建 harness run；输出落 blob，提交一条 `shell.command` write；不做崩溃重放 |
| 10 | fork 时 document | 会话内分支复制 `komo.agent`，其余不带；`/fork` `/clone` 复制 `komo.agent`，不复制 live / inbox / task |
| 11 | 导航位置 | 持久化进 `sessions.record.leaf`（偏离 §8.11，D7 回写文档） |
| 12 | blob 根 | 沿用 `sessions/{uuid}/` |
| 13 | intervention 协议 | 不改；faulted → `Blocked`，只答 `abandon` |
| 14 | 辅助模型调用 | D5 继续 `LlmClient`；只有 harness 走 `ModelStream` |
| 15 | submit 带的 model / effort | sticky 的 `komo.agent` + `model.change` |
| 16 | 旧 `tests/recovery` | gateway 只保留 W4a 的 5 个端到端重启测试，其余由 runtime 测试覆盖，D7 删 |

风险：`TursoStorage` 读只有一条连接（W2b 冒烟，不够就 W5 前换连接池）；会话列表与按 origin 查会话是全表扫描（留 TODO）；FakeLlm 的轮次判定可能需要个别测试改脚本结构。

## 10. 进度

| 包 | 分支 | 合入 `d4c` | 备注 |
|---|---|---|---|
| W0a 存储接缝 | `d5-w0a` | ✓ 2026-10-25 | kernel：`SessionRecord` 扩展（`SessionKind` 与 `kind` / `agent` / `parent_session` / `leaf` / `title_provisional` / `created_at` / `changed_at` / `request_key`，全带默认值）、`ConversationRecord.session`、`ConversationQuery` 的 `session` / `parent_conversation`、`StorageWrite::PurgeConversations`、`legacy_ids`；store 写 `session_id` 列、Purge SQL；runtime `Tx::{put_session, session, create_conversation_in, fork_conversation_in, purge_conversations}`（fork 默认继承 session）、`komo.settled`（`inbox::settle`）、`Harness::admit_in_tx`、任务会话 `sessions` 行、`Harness::abort_run` 与非正常收尾放置排队输入（决定 8；队列模式照旧，`OneAtATime` 时每种取第一项、余下留给后继 run 自己的 boundary）。`komo.settled` 的 `data` 在 §2 的字段之外多一个可选 `detail`（失败详情），排队中被撤回的输入也各留一条（`unanswered` / `aborted`）。`Tx::latest_head_marker` / `submission_by_request` 认"这次提交才建的 conversation" |
| W0b gateway 骨架与测试脚手架 | `d5-w0b` | ✓ 2026-10-25 | `StorageMode` / `ServiceOptions.stream`；`service/durable/{mod,core,agent_doc,assemble,background}.rs`、`http/v2/*` 桩；`FakeLlm: ModelStream`、`TestGateway::durable` / `GatewayBuilder::durable` / `wait_run`。偏离见提交说明：Durable 启动对 `aux.db` 做一次旧记忆队列复位（首次写，同进程重开不再 `database is locked`） |
| W1a kernel 投影 | `d5-w1a` | ✓ 2026-10-25 | `durable/pi_tree.rs`（`pi_tree(conversations, current_branch, leaf) -> PiTree`、`pi_id` / `entry_of`；一条 entry 可展开成几条 pi entry，ID 为 `"{id}"`、`"{id}:{n}"`；`head: self` → 自引用 `Compaction` + omit 它自己的 `ContextEdit`；`komo.system` / `komo.settled` / 未知 kind → `Custom`）、`durable/legacy_events.rs`（`Projection::project(&[Record]) -> Vec<Event>` 增量与 `legacy_events(session, entries, submissions)` 补读同一个函数）。偏离 §5：`RunQueued` 对每个输入 submission 都出（补读只见最终状态，按"是否排过队"出会让两边的帧不同）；`RunAccepted` 不带正文 / peer。`generation::answer` 先放置后继输入、后追加 `komo.settled`，所以投影靠"回答 / 失败 / reset 之后放置的输入开新 run"而不是靠结算；未知 / 展示类 entry 不出帧 |
| W1b gateway 钩子 | `d5-w1b` | ✓ 2026-10-25 | `service/durable/hooks/{mod,setup,tools,env,sections,compaction}.rs` + `orchestration.rs`（`GatewayHooks` / `GatewayOrchestration`，`MonitorRunner` trait 由 W2c 实现并在 `assemble::build_orchestration` 装上）；`komo_agent::context::{system_sections, task_contract_block, project_update_block, SECTION_*}`；`[compaction] background_tokens`；`ToolExecutor::{withdrawn, withdrawn_reason}` 放宽、`segment::run_roots` 改 `pub(crate)`。偏离：fork 任务保留父的能力面（含 `task` 与 `monitor`），只有 fresh 任务去掉 `task` 与 `monitor`——否则工具声明变了整段缓存作废；Session ID 在 W0a 合并前是替身 `c{根 conversation}`（见 `hooks/setup.rs::session_of`，`// W0a 合并后:`）。 |
| W1c 会话与树服务层 | `d5-w1c` | ✓ 2026-10-25 | `service/durable/{sessions,submit,tree,copy}.rs`（`impl DurableCore`，不接 HTTP）：`create_session` / `create_in`（tx 级，给 cron 同提交受理）、`list_sessions`、`main_session`、`session_for_source`、`session_summary`、`input_refusal`；`submit_input`（路由到 `current_branch`；`leaf` 不在末端同一提交 fork / 建新根 + 复制 `komo.agent` + 清 `leaf` + `admit_in_tx`；请求键跨分支去重，内容靠 `data.inputHash` 比；model / effort sticky 写 `komo.agent` + `model.change`）、`boundary`、`cancel_run`；`read_tree` / `navigate`（带摘要 / 标签）/ `label` / `name` / `context_edit` / `compact`、任一分支忙 → `SessionError::Busy`；`fork_session`（`/fork` `/clone`，entry 逐条复制、head / edits / label / branch.summary 引用按 ID 映射重写，blob 先同步再提交）。**偏离 / 约定**：①"第一条之前"的 leaf 用哨兵 `BEFORE_FIRST = EntryId(0)` 存进 `record.leaf`（ID 从 2 起铸），`pi_tree` 要传原始 `session.leaf`，找不到就 `leaf_id = None`；②`leaf` 恰是某 conversation 自己的最后一条时直接接在那个 conversation 后面，不 fork；③`model.change` 排在输入 entry 之后（`admit_in_tx` 之后不能再对该 conversation 表读，所以展示 entry 只能后写）；④手动压缩的摘要是 write submission，`compact` 从任务结果的 `submissionId` 反查摘要 entry；⑤复制保留 `komo.settled`（其 `data.submission` 指向没复制的 submission，W1a 投影需容忍） |
| W2a HTTP v2：会话、run、树 | `d5-w2a` | ✓ 2026-10-25 | `http/v2/{sessions,tree,runs}.rs`（create / list / detail / submit / boundary / resume、`/v1/home-session`、tree / navigate / fork / context-edit / label / name / compact、run detail / cancel；`SessionError` → `ApiFailure` 在 `http/error.rs`：404 / 400 / 409）、`service/durable/runs.rs`（`unfinished_runs` / `run_detail` / `cancel_run_id` / `submission_state`：run = 一个 conversation 的 entry + submission 经 `legacy_events` 投影再 `fold`，状态 `Waiting` 只来自 `komo.live.generation.retry`；只认有 submission 记录的 RunId，复制会话里指向没复制 submission 的 `komo.settled` 不算 run）、`service/durable/shell.rs`（`run_command` 给 `!` 与 W3b 命令型 cron 共用：bash 工具直跑、输出落 `tool-output/{conversation}/shell-{uuid}/1/`，再提交 `shell.command` write，RunId = 那条 write submission 的 ID）。**偏离 / 约定**：①W0a 的 `place_idle_input` 先铸 entry 后铸 submission，导致 `legacy_events` 按 ID 序折叠时认不出输入（没有 `RunStarted`、后面的 assistant / tool entry 全无 run）；改成**先铸 submission、后铸 entry**（submission ID 恒小于放置它的 entry ID，排队输入本来就是这样）；②`submit` 的 `project_update` 没有人算（W1b / W1c 都留了口子），HTTP 提交暂不带；③`detail.pending` / `resume.pending` 恒空（W4a 接 faulted 任务）；`memories` / `recall` 恒空（W3c）；④HTTP 的 delete / purge / reconcile 没接（W4a）；`session_tree/tests.rs` 里源会话的回收暂用服务层的 `PurgeConversations` + 删目录代替，W4a 合入后换回两次 HTTP 调用；⑤`slash_new_…` 的聊天版（dispatcher）留作 `_through_chat_legacy`，W3a 迁到 `dispatcher_v2` 后并回；⑥手动压缩在 Durable 下只收 `keep_recent_tokens` 之外的旧内容，`session_tree` 测试配置里把它调到 1 |
| W2c 后台任务与监听 | `d5-w2c` | ✓ 2026-10-25 | `service/durable/monitor_runner.rs`（`GatewayMonitorRunner: MonitorRunner`：脚本进程、stdout 泵、合批、时限、进程组、`runtime/monitors/children/` 的进程身份与遗留进程核对；事件 `Harness::submit(monitor::event_draft)`，结束 `finish_monitor`；"在飞"= 输入已结算或并进了别的输入开的 run）、`monitor_runner/detached.rs`（`bash` 转后台：`Detacher` 接手后 `Harness::register_monitor` 登记进 `komo.monitors`，之后与监听同一套）、`tasks.rs`（`handle` / `parse_handle` / `TaskLine` / `DurableCore::{task_sessions, task_label}`，短号 `#<conversation>`）。**runtime 小改**（加法）：`OrchestrationHooks::{boot, shutdown, detacher}`（默认空；boot = 启动时对账前核对遗留进程，shutdown = `DurableCore::close` 里引擎关闭前终止进程组、登记原样留给下一代，detacher = `bash` 转后台的接手方）、`Harness::register_monitor`、`ToolEnv.interjected`（用户插话旗；`DurableCore::{interjection, lower_interjection, interject}`，`InputRequest::interject(true)` 在输入排在忙会话后面时举旗——W2a 的 HTTP v2 `interject: true` 要接到它上）、查进度补上 running 调用的参数摘要（`komo_kernel::fold::summarize_args`）。接线：`assemble::wire` 把接手方装进 `bash` 的 `DetachSlot`（`start()` 不改：`build_tools` 里 `park_detach_slot(instance_id, slot)` 一行，`wire` 取走）；`background::spawn` 启动对账。测试 `tests/{monitor,detach,agents}/durable.rs` 保留原名，Legacy 版本原样留在原处；`test_support::driver_stream::DriverStream` 把旧 `LlmClient` 脚本演成 `ModelStream`。偏离：句柄是工具任务 / conversation 的十进制 ID；`monitor_stop` 停的转后台命令不再交回"已被停止"；重启对账的"已中断"文案由 runtime 写（转后台命令例外，仍是"结果不明"）；`komo session list` 的"任务·#xxxx"标注仍是会话 ID 后四位（客户端冻结），与 `#<conversation>` 对不上 |
| W2b 提交订阅与 SSE | `d5-w2b` | ✓ 2026-10-25 | `service/durable/feed.rs`（`spawn(core, hub, shutdown).await -> bool`，对同一个 core 幂等；`backfill(storage, session, after)` / `read_session_records` 给 HTTP 补读与懒建共用；feed 任务维护 conversation→Session、`komo.live` document→conversation 两张缓存与每个 Session 一个 `Projection`，`AssistantDelta` 取与上次部分回复比较的后缀）、`http/v2/events.rs`（JSON 分页 + SSE，补读后接直播，hub 缓冲溢出时当场从存储补读）、`sse.rs::already_sent`。**偏离 / 约定**：①懒建 Session 投影时存储状态可能已含后续批次的记录，所以只推"记录 ID 大于 feed 见过的最大 ID"的帧（`floor`，起点是 spawn 时铸的 ID；靠 ID 全局单调且都在提交线上铸）；②补读每次全量投影再丢 `seq <= cursor`（W1a 的限制），长会话的重连是 O(历史)；③`spawn` 是 async（先铸 `floor` 再订阅），接线要 `.await`；④`AssistantDelta` 的 SSE `id:` 取该 Session 最近一帧的 seq；⑤补读先读 entry 再读 submission，不能反过来；⑥并发补读经 `READ_PERMITS = 2` 的闸门读存储（`TursoStorage` 读口只有一条连接，不设限时 20 路并发补读让提交慢近 30 倍，限流后约 3 倍；换连接池后可放宽）；⑦分支带着 `0119ba4a`（W2a：先铸 submission 再铸输入 entry），否则空闲直放的输入没有 `RunStarted` |
| W3a 渠道、分发、看客、投递 | `d5-w3a` | ✓ 2026-10-25 | `dispatcher_v2.rs`（`Dispatcher::handle` 开头按 `durable.is_some()` 分支；认人 / 命令解析 / 渲染复用 Legacy 的纯函数，改成 `pub(crate)`）：私聊无话题 → 默认 Agent 主会话，群 / 话题 / 带话题私聊 → `source = chat:{peer}` 的普通会话；输入 entry `chat.input`、`data {peer, principal}`，`request_id` = 平台消息键（持久去重）；宿主 run 是同一个 peer 起的才 `WhenBusy::Steer`，别的 peer 排在后面；`/new` = `boundary`，`/cancel` = `cancel_run_id`（在跑的只停这一轮，排队的 `withdraw`，等 ≤2 秒报真实状态），`/status` 带当前 run 与本会话派出的后台任务（`TaskLine::render`）；`/pending` `/answer` 读的 intervention 清单仍是 Legacy 的（`aux.db` 里空表），W4a 接 faulted 任务。`service/durable/watch.rs`：**一个 Session 一个看客**（`ensure`，订阅 hub 帧；dispatcher 与 HTTP v2 submit 在提交前调），终态帧按这条 run 首个输入定去向——`chat.input` 回 `data.peer`；`task.result` / `monitor.*` 按决定 6 取路径上最近带 peer 的用户输入的 peer，没有则只有**主会话**退回 home chat（TUI / HTTP 开的会话不投）；并进别人 run 的聊天输入收到 `RunAbsorbed`；投递用逻辑键 `run-finished:{run}` / `run-absorbed:{run}` 的 `deliver_once`。`reattach`（`background::spawn` 末尾，W3a 段）：每个非任务会话挂看客 + 每个未结算输入一个 `wait_submission` 小任务 + 把最近 64 条已结算输入再收尾一遍（补上终态帧落在 feed 起来之前的）。`titles.rs`：同 Legacy，读写 sessions 行的 `title` / `title_provisional`。`channels/mod.rs` 没有直接读 `state.db` 的地方，未改。测试：`tests/chat/durable/`（37 个，`start_durable` / `durable_origins` / `durable_session_of` / `durable_inputs` / `durable_soft_delete` 帮手）、`service/tests.rs::durable_chat`（5 个）与 `slash_new_…` 并回聊天版、`tests/agents/durable.rs` 4 个、`watch_tests.rs` 3 个。**偏离 / 约定**：①`tests/chat` 里 4 个建立在旧 `waiting + intervention` 上的没迁（`an_intervention_reaches_both_the_source_and_home`、`a_waiting_run_sends_its_intervention_to_the_chat_and_home`、`an_http_run_sends_its_intervention_to_home_only`、`a_round_stopped_at_its_first_call_still_finishes_the_rest`），`the_command_table_is_operator_only` 与 `a_message_behind_a_waiting_run_waits_instead_of_overtaking` 改成忙在 `gate` 上；②Durable 的 skills 段每次请求按当前注册表重算，`a_resumed_segment_keeps_the_skills_catalog_it_was_accepted_with` 记下了现状（重发的请求带上期间新增的 skill，会让前缀缓存作废），冻结归 W4b；③NeedsAttention 投递没做（W4a） |
| W3b cron | `d5-w3b` | ✓ 2026-10-25 | `komo-runtime` `scheduler/cron.rs`：`CronScheduler` 改依赖 `CronTarget`（`fire(FireSpec) -> FiredRef`、`run_now`、`has_unfinished(jobs, job)`；`FireSpec {job, session, request_key, model, slot, at}`），Legacy 侧 `LedgerCronTarget` 包旧 `Ledger`（行为不变）；调度器现在**先分配 Session ID 并随 `claim_firing` 写进触发记录**，目标报 `CronError::Target` 时那一槽记 `error` 并推进（`SkipReason::Failed`），不拖住同拍的别的 Job。gateway：`service/durable/cron.rs`（`DurableCronTarget`：一次提交 `sessions::create_in`（`kind=cron`、`komo.agent` 带模型 alias / effort / `maxRounds` / workdir、`source=cron:{job}`）+ `admit_in_tx`（`cron.input`，`data {job, slot}`，`request_id=cron:{job}:{slot}`）；会话已存在就只取回 Run；命令型 Job = 提交建会话 → `shell::run_command` → `shell.command` write 落账，不重放；`recover` 启动补做 queued 的触发记录：有 Run 重挂看客、有 Session 无 Run 补投、都没有记 error；`spawn` 在 `background::spawn` 末尾）、`cron_watch.rs`（`wait_submission` + `feed::backfill` 取终态帧，回写状态 / 按 `notify` 经旧 `notifier.deliver_home` 投；命令型从 `shell.command` 的 `data.output` 读 `BashResult`）、`http/v2/cron.rs`（复用旧 handler——`state.cron` 在 Durable 下就是 `aux.db` 的 repo；`resolve_model` 加一条：Durable 下 Job 的模型覆盖必须对得上 `model_catalog` 的 alias，否则 400）。`GatewayState::cron_scheduler` 改 `self: &Arc<Self>` 并按模式挑目标，`watch_cron_run` 按模式挑看客。**偏离 / 约定**：①重叠判定不能用 `has_unfinished_firing`（它连 `runs` 表，Durable 下查不到 Run 会永远判未结束），由目标按 submission 结算状态判；②Job 的 `skills` 在 Legacy 也没有人读，Durable 同样不带；③cron 会话标题 = Job 名；④看客不订阅 hub，等 submission 结算再读终态帧，重启靠 `recover` 重挂。测试 `tests/cron/durable/`（25 个同名 + 2 个 Durable 独有）；改过断言的：重叠三例把 `unsure`（停在等人核对）换成 `gate`（门不开 = run 没结束）；`on_error…` 后半与 `never_still_lets_a_wait_through` 的 wait 换成模型失败（新语义没有 `waiting + intervention`；后者只验 `never` 下失败不投、状态记 error）；重启例在停机前读触发记录（`aux.db` 独占锁）；两处 `runs::get` 换成 `run_detail`。 |
| W3c memory | `d5-w3c` | ✓ 2026-10-25 | `komo-runtime/src/memory/durable.rs`（`DurableSessionEvents: SessionEvents`：按 conversation 读 entry / submission 经 `legacy_events` 投影，再适配两处——run 第一条 `MessageUser` 的正文并回 `RunAccepted.text`（旧提取读 `RunAccepted`），`komo.memory` 合成一条 `MemoryInjected` 挂在触发召回的 run 上；`DurableMemoryWork: MemoryWorkLog`：工作 = 投影里已进终态的 run（Completed / Failed / Cancelled），会话全部输入结算且最后一条 run 结束已过 `extract_idle` 才领，游标在 conversation 级 document `komo.memory_cursor`（`{runs: {RunId: {work, cursor}}}`，`finish` 走 `engine.commit` 读改写，游标只进不退），领取只在进程内，`requeue_stuck` 清空领取表）、`service/durable/memory.rs`（`seams` 装配 + `spawn_tick`，接线在 `background::spawn` 末尾）、`state.rs::assemble` 按 `durable` 选事件 / 队列接缝、`http/v2/runs` 的 `memories` / `recall` 读 `komo.memory`（`service/durable/runs.rs`）。测试 `tests/memory/durable/` 保留 27 个原名。**小改**：`MemoryWorkItem.frozen`（记忆模式与 Agent 由领取方按**提取时**配置给，旧路径 `None` = 读 `run.accepted` 快照；`run_batch` 里一行 `unwrap_or_else`）；`MemoryDoc`（定义搬到 runtime，`hooks/sections.rs` 改成 `pub use`，加 `recall` 诊断与 `at` 毫秒时刻）。偏离：①新存储没有受理快照，记忆模式按提取时的 Agent 配置，不是受理时冻结；②`komo.memory` 只留最后一份，换段重召回后更早的 run 详情看不到当时那块；`at` 认触发 run = 开始时刻不晚于它的最后一条；③`aux.db` 的首次写仍是 `start()` 里的 `requeue_memory_work()`（未动）；④领取每拍扫全部 Session（`quiet` 表让无新结算输入的会话只扫一次 submission，TODO 索引）；⑤新请求里没有 `TurnRequest.memories`，测试改看系统提示与 run 详情 |
| W4c D6 导出 | `d5-w4c` | | `http/v2/export.rs`（`GET /v1/sessions/{id}/export`：pi v3 header + `pi_tree`，JSONL；header 多 `komoBlobRoot`，blob 保持引用不内联；leaf 安排在文件末尾——末条原样 / 无后代的挪到最后 / 其余追加 `custom` `komo.leaf` 标记）、`KomoClient::export_session`、`komo session export SESSION_ID [-o FILE]`。测试 `export_round_trips_through_session_tree_with_branches_and_leaf` 等 7 个。**偏离**：没能用 pi 真读（本机无 node，不全局安装），改为对着 `session-manager.ts` 逐点核对，记在 `storage-durable.md` §9 D6；导出只在 Durable 下有路由 |
