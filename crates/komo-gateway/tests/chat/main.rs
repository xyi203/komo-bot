//! W5 端到端验收：docs/komo_bot.md §14 阶段 4「聊天入口」那一行的验证列，**一句一个
//! 测试**，走真 Gateway（真数据目录、真 state.db、真 Dispatcher、真 `HomeNotifier`）与
//! 真渠道代码（`TelegramChannel` / `WeChatChannel` 对着 loopback 上的假平台服务端
//! `serve`；飞书见 `feishu.rs` 的模块注释）。
//!
//! 验证列逐句 → 测试：
//!
//! | 验证列 | 测试 |
//! |---|---|
//! | 同一 `update_id` 重发只产生一个 Run | `telegram::a_replayed_update_produces_one_run` |
//! | 同一 `event_id` 重发只产生一个 Run | `feishu::a_replayed_event_produces_one_run` |
//! | 同一 `client_id` 重发只产生一个 Run | `wechat::a_replayed_client_id_produces_one_run` |
//! | 不在 `allow_from` 的被拒且不留记录 | `cross::a_stranger_is_refused_and_leaves_no_trace`、`feishu::a_feishu_stranger_leaves_no_delivery`、`wechat::a_wechat_stranger_is_refused_but_can_still_ask_for_its_id` |
//! | `/id` 对被拒者仍可用 | 同上（每条的第二段） |
//! | 加进 `allow_from` 保存后下一条即进 Run | `cross::a_reloaded_allow_list_decides_the_next_message` |
//! | 来源会话与 home chat 都收到介入通知 | `cross::an_intervention_reaches_both_the_source_and_home`、`cross::a_waiting_run_sends_its_intervention_to_the_chat_and_home` |
//! | 第二个答复得到「已经答复过了」 | `cross::an_intervention_reaches_both_the_source_and_home`（后半） |
//! | `/approve`、`y` 不是命令 | `telegram::the_command_table_answers_every_command`（`/approve` 答帮助）；`y` 见 `service::tests` |
//! | Gateway 重启后 pending 投递补发一次 | `cross::a_pending_delivery_is_resent_once_after_a_restart` |
//! | 飞书 ws 断线重连不丢事件也不重跑 Run | `feishu::a_reconnect_loses_nothing_and_reruns_nothing` |
//! | 微信在用户未发消息前 `Deferred` | `wechat::a_delivery_before_the_user_speaks_is_deferred` |
//! | 发消息后先收到积压的投递 | `wechat::the_backlog_is_flushed_before_the_new_message` |
//!
//! 审批（短 ID、按钮、`/approve` 范围、批量答复、决定后原地改卡）随 Policy 一起删掉了
//! （§7，2026-10-11），那几行验证列与它们的测试一起删。介入要停住一个 Run 时用测试工具
//! `unsure`（结果不明 → `verify`）或 `gate`（一直等到放行）。
//!
//! 外加 §11.2 / §11.3 的会话与命令表：home session 的归一
//! （`cross::every_private_chat_lands_in_one_home_session`）、群会话按
//! `{platform}:{chat_id}`（`cross::every_group_gets_its_own_session`）、`/new`
//! 不切 Session、`/status` `/pending` `/cancel` 的回执、`allow_from` 为空的渠道
//! "只出不进"、没有 `home_chat` 时 `deliver_home` 报错而不是静默丢弃，以及 §3 的
//! "校验不过的配置永远不会被装上"。
//!
//! 三个假平台（假 Bot API / 假开放平台 / 假 iLink）不在这里另写一份：用的是渠道自己带的
//! 那一套（`komo_gateway::channels::*::fake`，`test-support` feature 下编进来）——同一套
//! 假平台两份实现迟早会各说各的。
//!
//! `smoke.rs` 是 `komo` 二进制的冒烟（真进程、`/healthz`、401、`komo session list`、
//! 第二台拿不到锁），默认 `#[ignore]`——它要先 `cargo build -p komo`。
//!
//! **失败即缺陷**：与文档不符的地方留成 `#[ignore]` + `// BUG(n):`，
//! `cargo test -p komo-gateway --test chat -- --ignored` 一次全部复现。

mod harness;

mod cross;
mod durable;
mod feishu;
mod skills;
mod smoke;
mod telegram;
mod wechat;
