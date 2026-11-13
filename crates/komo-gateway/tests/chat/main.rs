//! 聊天组：走真 Gateway（真数据目录、真 durable.db、真 HTTP 监听）与脚本化模型。聊天渠道删除后
//! （`docs/redesign.md` P17）消息一律经 `POST /v1/sessions/{id}/runs` 提交。
//!
//! `smoke.rs` 是 `komo` 二进制的冒烟（真进程、`/healthz`、401、`komo session list`、
//! 第二台拿不到锁），默认 `#[ignore]`——它要先 `cargo build -p komo`。
//!
//! **失败即缺陷**：与文档不符的地方留成 `#[ignore]` + `// BUG(n):`，
//! `cargo test -p komo-gateway --test chat -- --ignored` 一次全部复现。

mod harness;

mod cross;
mod skills;
mod smoke;
