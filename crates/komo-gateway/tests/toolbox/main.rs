//! W6 toolbox 验收：docs/komo_bot.md §14 阶段 6 的验证列，加上 §5.5 的 Memos 那一行。
//!
//! | §14 验证列 | 测试 |
//! |---|---|
//! | 调用使用已批准且已测试版本 | `lifecycle::a_candidate_becomes_callable_only_after_it_is_tested_and_approved`、`komo-runtime` 的 `tools::python::tests::a_call_binds_the_enabled_module_version` |
//! | 模块更新使旧授权失效 | `komo-runtime` 的 `tools::python::tests::updating_the_module_invalidates_a_grant_bound_to_the_old_version` |
//! | toolbox 启用的审批在微信里能看到版本差异与测试结果 | `approval::the_wechat_message_shows_the_version_diff_and_the_test_result`（另两个渠道各一条） |
//!
//! | §14 Memory 覆盖 | 测试 |
//! |---|---|
//! | 写入 Memos 后返回 ID / 链接，查询回到原文 | `memos::the_four_things_the_acceptance_asks_of_memos` |
//! | 删除或修改原文后更新关联摘要 | 同上（模块这一侧：`update` 返回新内容与同一 ID） |
//! | 写入未知时先核对 | 同上（`verify` 的三种结论） |
//!
//! §7.3 的执行边界（`code` 模式 import 不到候选）在 `komo-runtime` 的
//! `python_runtime::tests` 里——那里有一个真解释器，而这里的断言会退化成"HTTP 返回
//! 了什么"。

mod harness;

mod approval;
mod lifecycle;
mod memos;
