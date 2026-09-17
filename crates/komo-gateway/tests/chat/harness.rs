//! 这一组测试要的那一台 Gateway。
//!
//! 共用件在 `komo_gateway::service::test_support::harness`（`test-support` feature 下编
//! 进来）：真数据目录、真 `service::start`、内存发送口、脚本化模型。四个集成测试目标原
//! 先各抄了一份，现在只留各自特有的那几个助手——chat 这一组连特有的都没有，所以这里只
//! 有一行 `pub use`。

#![allow(unused_imports)]

pub use komo_gateway::service::test_support::harness::*;
