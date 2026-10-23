//! 新存储（docs/storage-durable.md §3）的 store 侧底座：一个新库文件、一条串行提交线、
//! blob 写入顺序。**新旧并存**：这里不碰 `state.db`、不碰 toasty 模型，旧代码照常运行；
//! D1 的记录类型与 `Storage` trait 落地后，在这些零件上实现它。
//!
//! # 对 AGENTS.md / §8.2 的有意偏离
//!
//! AGENTS.md 与 §8.2 规定"裸 SQL 只许出现在三个模块（db / queue / memory）"，且所有写都走
//! toasty 的连接池。本目录**整体**违反这两条，理由只有一个：线要在**逐次提交之间切
//! `PRAGMA synchronous`**（结构性提交 FULL、只写进度的提交 OFF，§3.6），那是每连接状态，
//! toasty 的池把连接借出去又收回去，没有"我独占这一条连接、并且上次把它设成了什么"这个概念。
//! 所以线持有一条裸 `turso::Connection`；表也不建 toasty 模型——读写都经过这条裸连接
//! 与同库的只读连接，没有第二套 schema 来源。代价由 [`schema`] 里的
//! "DDL 常量 = `sqlite_schema` 里实际建出的 SQL"测试补上。裸 SQL 因此集中在三个文件：
//! [`schema`]（建表 / 补列）、[`write`]（每种写的那一条语句）、[`line`]（事务与 pragma）。
//!
//! ```text
//! Line（可克隆句柄）── mpsc ──▶ 线任务 ── 唯一写连接（WAL，synchronous 逐次切换）
//!   │                                         │
//!   └─ read_connection() ─▶ 同一 turso::Database 的只读连接（WAL 下读不阻塞写）
//! blob::publish_blob      先写 blob（同步、改名、同步目录），**再**提交引用它的 entry
//! ```

pub mod blob;
pub mod ids;
pub mod line;
pub mod schema;
pub mod storage;
pub mod write;

pub use blob::{PublishedBlob, publish_blob};
pub use ids::IdMint;
pub use line::{Line, LineError, Seq};
pub use schema::{DURABLE_DB_FILE, FORMAT_VERSION, durable_db_path};
pub use storage::TursoStorage;
pub use write::{CommitKind, Write};

#[cfg(test)]
mod crash_tests;
