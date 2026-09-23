//! W6 Cron 验收：docs/komo_bot.md §14 阶段 7 那一行的验证列，**一句一个测试**，
//! 加上 §10 里逐条写死的默认行为。走真 Gateway（真数据目录、真 state.db、真监听、
//! 真 `HomeNotifier`、真 Policy）。
//!
//! | §14 阶段 7 验证列 | 测试 |
//! |---|---|
//! | 重启不重复创建同次触发 | `firing::a_restart_does_not_create_the_same_firing_twice` |
//! | 新危险操作等待审批 | `approval::a_dangerous_action_waits_and_the_request_reaches_home_chat` |
//! | Cron 的等待在聊天里 `/approve` 后按 Cron 权限继续，不升权 | `approval::approving_once_does_not_hand_the_job_the_operators_powers`、`approval::approving_with_cron_scope_binds_the_job_and_its_version` |
//!
//! §10 的默认行为逐条：
//!
//! | §10 | 测试 |
//! |---|---|
//! | 唯一键 `job_id + scheduled_at_utc` | `firing::a_restart_does_not_create_the_same_firing_twice` |
//! | 上一次未结束时跳过并**记录原因** | `firing::an_overlapping_slot_is_skipped_and_leaves_a_trace` |
//! | 错过的触发不集中补跑 | `firing::a_slot_missed_by_days_is_not_made_up`（单元测在 `komo-runtime`） |
//! | `@at` 是一次性，claim 时完成 | `definition::a_one_shot_is_done_after_it_fires_and_refuses_to_run_again` |
//! | 手动 run 用独立幂等键，不写触发记录 | `definition::a_manual_run_writes_no_firing_and_does_not_advance_the_slot` |
//! | Job 改了重新匹配授权 | `definition::changing_the_definition_bumps_the_version_but_pausing_does_not` |
//! | 时区明确保存 | `definition::an_unknown_timezone_is_refused_with_what_to_write`（到期时刻的断言在 `komo-runtime` 的真 tzdb 测试里） |
//! | 模型 / effort 覆盖按完整配置解析 | `definition::a_model_override_is_a_whole_config_and_leaves_memory_alone` |
//! | `notify` | `notify::on_error_keeps_quiet_about_a_good_run_but_never_about_a_wait` |
//! | 结果去原 Session 查看 | `firing::the_list_shows_the_last_firing_and_the_next_slot` |
//! | 命令直跑：零模型请求、add 即授权、ok / error / 空输出语义 | `command::*` |
//!
//! **失败即缺陷**：与文档不符的地方留成 `#[ignore]` + `// BUG(n):`。

mod harness;

mod approval;
mod command;
mod definition;
mod firing;
mod notify;
