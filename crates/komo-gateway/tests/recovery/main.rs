//! W5：§14「恢复故障注入验收」逐行一个测试，外加「首个端到端验收」与「目录与引用验收」。
//!
//! 每个测试都跑一台**真** Gateway（`service::start`：真 config.toml、真 state.db、真
//! TcpListener、真调度器、真账本），在指定的一步强制中断，然后用**同一个数据目录**重启，
//! 断言 §14 那张表右列的事真的发生了。断言的是副作用次数、Run / ToolCall 身份、授权消费、
//! 预算与事件配对——「仅检查"恢复后状态变成 running"不算通过」（§14）。
//!
//! **失败的测试是有意留着的**：它们是实现与文档不符的地方，不是脚手架坏了。
//!
//! 两条注入路见 `harness`：故障账本装饰器（`Fault`），以及停机之后直接篡改磁盘。

mod e2e;
mod harness;
mod home_dispatcher;
mod reconcile;
mod rows;
mod verdicts;
