//! D4b 验收：脚本化的 `ModelStream` 与测试工具跑在 `MemoryStorage` 上，时间用 tokio 的暂停时钟。
//! 每条测试旁注明对应 pi-durable 的哪个测试，或设计 §5 恢复表的哪一行。

mod support;

mod generation;
mod inbox;
mod probe;
mod recovery;
mod tools;
