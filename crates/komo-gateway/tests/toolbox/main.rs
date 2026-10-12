//! W6 toolbox 验收：docs/komo_bot.md §14 阶段 6 的验证列，加上 §5.5 的 Memos 那一行。
//!
//! | §14 验证列 | 测试 |
//! |---|---|
//! | 调用使用已测试版本 | `lifecycle::a_candidate_becomes_callable_only_after_it_is_tested`、`komo-runtime` 的 `tools::python::tests::a_call_binds_the_enabled_module_version` |
//! | 测完又改的候选装不上 | `lifecycle::a_candidate_edited_after_its_test_does_not_ride_in_on_the_old_test` |
//!
//! 启用 / 停用直接生效（§7，2026-10-11 删掉审批）：原先"启用的审批在微信里能看到版本
//! 差异与测试结果"那一行随审批一起没了。
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

mod lifecycle;
mod memos;
