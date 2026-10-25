//! 引擎验收：跑在 D1 的 `MemoryStorage` 上，时间用 tokio 的暂停时钟。
//! 每条测试旁注明对应 pi-durable 的哪个测试 / 示例。

pub(super) mod support;

mod abort;
mod examples;
mod lifecycle;
mod ownership;
mod phases;
mod recovery;
mod seams;
mod waiting;
