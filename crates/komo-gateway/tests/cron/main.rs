//! W6 Cron 验收：docs/komo_bot.md §14 阶段 7 那一行的验证列，**一句一个测试**，
//! 加上 §10 里逐条写死的默认行为。走真 Gateway（真数据目录、真 state.db、真监听、
//! 真 `HomeNotifier`）。
//!
//! | §14 阶段 7 验证列 | 测试 |
//! |---|---|
//! | 重启不重复创建同次触发 | `firing::a_restart_does_not_create_the_same_firing_twice` |
//! | 要人处理的等待投到 home chat（审批 2026-10-11 删除，§7） | `notify::never_still_lets_a_wait_through` |
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
//! | 改定义版本 +1、暂停不动版本 | `definition::changing_the_definition_bumps_the_version_but_pausing_does_not` |
//! | 时区明确保存 | `definition::an_unknown_timezone_is_refused_with_what_to_write`（到期时刻的断言在 `komo-runtime` 的真 tzdb 测试里） |
//! | 模型 / effort 覆盖按完整配置解析 | `definition::a_model_override_is_a_whole_config_and_leaves_memory_alone` |
//! | `notify` | `notify::on_error_keeps_quiet_about_a_good_run_but_never_about_a_wait` |
//! | 结果去原 Session 查看 | `firing::the_list_shows_the_last_firing_and_the_next_slot` |
//! | 命令直跑：零模型请求、ok / error / 空输出语义 | `command::*` |
//!
//! **失败即缺陷**：与文档不符的地方留成 `#[ignore]` + `// BUG(n):`。

mod harness;

mod command;
mod definition;
mod durable;
mod firing;
mod notify;
