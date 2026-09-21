//! Turso 连接、写重试与 schema 演进（§8.2）。
//!
//! 三条来自 spike 实测（`.scratch/komo-v08-rewrite/spikes/store.md`）的硬规则，违反就
//! 是并发 bug：
//!
//! 1. **每个写操作、包括只有一条语句的，都放进 `db.transaction()`，再整个包进
//!    [`Db::with_write_retry`]。** toasty 只在显式事务上发 `BEGIN CONCURRENT`；
//!    autocommit 单语句走普通写事务，而驱动不设 `busy_timeout`——实测 4 个写入器并发写
//!    200 行不同主键，autocommit 只成 6 次，包进事务 200 次全成。
//! 2. 重试条件是 [`toasty::Error::is_serialization_failure`]，**不匹配字符串**。闭包只
//!    依赖入参与事务内读到的状态，里面不 `await` 模型、子进程、文件同步或用户；回滚后
//!    干净重跑，绝不双重应用。重试超限报 [`StoreError::Contended`]。
//! 3. 连接建立时 `PRAGMA data_sync_retry = 1`：Turso 默认 `false`，此时 fsync **出错是
//!    `panic!` 而不是返回 `Err`**（`turso_core` 的 `storage/pager.rs:4330`）。
//!
//! raw SQL 只走 `toasty::sql::{statement, query}`，**没有第二个 `turso::Database`
//! 句柄**；raw SQL 只出现在三个模块：这里（schema / 连接）、
//! [`crate::repos::queue`]（§8.7 的四条领取语句）和 [`crate::repos::memory`]
//! （关键词臂的 `instr`）。
//!
//! 唯一的例外是[建池之前的 schema 迁移](migrate_file)：它**必须**用一条普通（非 MVCC）
//! turso 连接，因为 MVCC 下 DDL 不落盘（实测 2026-09-20）。那条连接在池铺开之前就已经关了，
//! 与池不并存——所以"只有一个句柄"这条纪律在并发意义上仍然成立。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use komo_kernel::traits::{LedgerError, StoreError};
use time::OffsetDateTime;
use toasty::Executor;
use toasty_driver_turso::Turso;

use crate::models;

/// 一个借用了执行器的异步块。
///
/// 闭包写成 `|tx| Box::pin(async move { … })`：`async` 闭包还不能在 trait 约束里表达
/// 一个借用了参数的返回 future，装箱是现在唯一写得出来的形状。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 写重试的退避参数。
///
/// spike 没有标定过它：4 个竞争者下走事务的失败率约 66%，收敛是确定的，具体档位要压测。
/// 这里给一组保守默认——指数退避，上限 `max_delay`——并留出旋钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryConfig {
    /// 首次冲突后的等待。
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// 除首次外还能重试几次。超限报 [`StoreError::Contended`]。
    pub max_retries: u32,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            base_delay: Duration::from_millis(2),
            max_delay: Duration::from_millis(200),
            max_retries: 12,
        }
    }
}

impl RetryConfig {
    fn delay_for(&self, attempt: u32) -> Duration {
        let shift = attempt.min(16);
        let scaled = self
            .base_delay
            .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX));
        scaled.min(self.max_delay)
    }
}

/// 打开数据库的选项。
#[derive(Debug, Clone)]
pub struct DbOptions {
    pub retry: RetryConfig,
    /// 连接池大小。**每个池槽都要单独设 `data_sync_retry`**（那是每连接状态），所以
    /// 这个数在打开时就要知道。
    pub pool_size: usize,
}

impl Default for DbOptions {
    fn default() -> Self {
        Self {
            retry: RetryConfig::default(),
            pool_size: 8,
        }
    }
}

/// state.db 的句柄。
///
/// Turso 对 db 文件持**进程独占锁**，所以进程里只有一个 `Db`，而 Gateway 是唯一打开它
/// 的进程（§8.2、§3）。克隆很便宜——内部共享同一个连接池。
#[derive(Debug, Clone)]
pub struct Db {
    inner: toasty::Db,
    path: Option<PathBuf>,
    retry: RetryConfig,
}

impl Db {
    /// 打开 `path` 上的 state.db。
    pub async fn connect(path: impl AsRef<Path>) -> Result<Db, StoreError> {
        Db::connect_with(path, DbOptions::default()).await
    }

    /// 打开 `path`，带选项。
    ///
    /// **新文件让 toasty 建表**（`push_schema` 只对新文件执行，且不幂等，§8.2）；已存在
    /// 的文件走建池**之前**的 [`Db::migrate_file`]：用普通（非 MVCC）连接逐表
    /// `CREATE TABLE IF NOT EXISTS`、逐列 `ALTER TABLE ADD COLUMN`、补建缺失索引——MVCC
    /// 连接上的 DDL 不落盘，所以这一步必须在 `toasty::Db::builder` 之前。建池之后再跑一次
    /// [`Db::ensure_schema`]：它现在只**核对**（文件库缺列就报错）并给内存库补列。
    /// "新建之后 schema 与常量一致"正是 DDL 字节对齐测试所断言的那件事。
    pub async fn connect_with(path: impl AsRef<Path>, opts: DbOptions) -> Result<Db, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| StoreError::Io(format!("建立 {} 失败：{e}", parent.display())))?;
        }
        let fresh = !path.exists();
        let driver = Turso::file(&path).concurrent_writes();
        let db = Db::build(driver, Some(path), opts, fresh).await?;
        Ok(db)
    }

    /// 打开一个只存在于内存里的库。给测试与"没有数据目录也要能起来"的诊断用。
    pub async fn open_memory() -> Result<Db, StoreError> {
        Db::open_memory_with(DbOptions::default()).await
    }

    /// 在一个临时目录里打开一个库，返回它和那个目录的守卫。
    ///
    /// 目录守卫一被丢弃目录就没了，所以**别让它比 `Db` 先走**。上层 crate 的测试一般
    /// 要的是 [`crate::test_support::TempStore`]，它连 `sessions/` 一起给。
    #[cfg(feature = "test-support")]
    pub async fn open_temp() -> Result<(Db, tempfile::TempDir), StoreError> {
        let dir =
            tempfile::tempdir().map_err(|e| StoreError::Io(format!("建立临时目录失败：{e}")))?;
        let db = Db::connect(dir.path().join("state.db")).await?;
        Ok((db, dir))
    }

    /// 打开一个内存库，带选项。
    pub async fn open_memory_with(opts: DbOptions) -> Result<Db, StoreError> {
        let driver = Turso::in_memory().concurrent_writes();
        Db::build(driver, None, opts, true).await
    }

    async fn build(
        driver: Turso,
        path: Option<PathBuf>,
        opts: DbOptions,
        fresh: bool,
    ) -> Result<Db, StoreError> {
        // **迁移要在 MVCC 连接建起来之前做完**（[`migrate_file`] 的注释里有实测依据）：
        // DDL 一旦走 MVCC 那条路就会静默丢掉，而补列是升级路径上唯一让旧库能用的事。
        // 新文件不走这条——它交给下面的 `push_schema`。
        if let Some(path) = path.as_ref()
            && !fresh
        {
            Self::migrate_file(path).await?;
        }

        let inner = toasty::Db::builder()
            .models(models::model_set())
            .max_pool_size(opts.pool_size)
            .build(driver)
            .await
            .map_err(map_toasty)?;

        // **顺序要紧**：schema 先做，连接池后铺。
        //
        // MVCC 下 DDL 要求这个库上没有别的连接开着——实测（同一台机器、同一份依赖）：
        // `max_pool_size(1)` 时 `CREATE TABLE` / `CREATE INDEX` / `ALTER TABLE` 全部
        // `Ok`；`max_pool_size(2)` 起就变成 `database is locked`，而且 autocommit、
        // `BEGIN`、`BEGIN CONCURRENT` 三条路都一样（`BEGIN CONCURRENT` 另有一句明确的
        // 「DDL statements require an exclusive transaction」）。deadpool 是**按需**创建
        // 连接的，所以只要在铺开连接之前把 schema 做完，全程就只有一条连接。
        if fresh {
            inner.push_schema().await.map_err(map_toasty)?;
        }
        let db = Db {
            inner,
            path,
            retry: opts.retry,
        };
        db.ensure_schema().await?;

        // 每连接状态：fsync 出错要能报告，而不是把整个进程带走（§8.2）。
        prime_connections(&db.inner, opts.pool_size).await?;
        Ok(db)
    }

    /// state.db 的路径；内存库没有。
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn retry_config(&self) -> RetryConfig {
        self.retry
    }

    /// 一个可以直接 `exec` 的 toasty 句柄（克隆自同一个池）。
    ///
    /// 只给**读**用。写一律走 [`Db::with_write_retry`]。
    pub fn handle(&self) -> toasty::Db {
        self.inner.clone()
    }

    /// 在**一条**池连接上跑一段只读逻辑。
    ///
    /// 不开事务：读不抢写锁，而 §14 的「长读事务与并发写提交的快照语义」还没核实，开一
    /// 个长读事务正是那条待验证项警告的形状。同一闭包里的多条语句共用一条连接，所以它
    /// 们至少看到同一个连接上的连续状态。
    pub async fn read<T, F>(&self, mut op: F) -> Result<T, StoreError>
    where
        F: for<'a> FnMut(&'a mut dyn Executor) -> BoxFuture<'a, Result<T, StoreError>> + Send,
        T: Send,
    {
        let mut conn = self.inner.connection().await.map_err(map_toasty)?;
        op(&mut conn).await
    }

    /// 在一个 `BEGIN CONCURRENT` 事务里跑一段写逻辑，冲突就整段重跑。
    ///
    /// 闭包可能被调用多次，所以它**只能依赖入参和事务内读到的状态**——回滚后干净重跑，
    /// 绝不双重应用（§8.2）。里面不要 `await` 模型、子进程、文件同步或用户。
    pub async fn with_write_retry<T, F>(&self, mut op: F) -> Result<T, StoreError>
    where
        F: for<'a> FnMut(&'a mut dyn Executor) -> BoxFuture<'a, Result<T, StoreError>> + Send,
        T: Send,
    {
        let mut attempt = 0u32;
        loop {
            let mut handle = self.inner.clone();
            let outcome: Result<T, StoreError> = async {
                let mut tx = handle.transaction().await.map_err(map_toasty)?;
                let value = op(&mut tx).await?;
                tx.commit().await.map_err(map_toasty)?;
                Ok(value)
            }
            .await;

            match outcome {
                Ok(value) => return Ok(value),
                // 竞争失败的第一手信号是错误而不是 0 行（§8.7）。
                Err(StoreError::Contended) if attempt < self.retry.max_retries => {
                    let delay = self.retry.delay_for(attempt);
                    attempt += 1;
                    tracing::debug!(attempt, ?delay, "写入争用，退避重试");
                    tokio::time::sleep(delay).await;
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// 逐表 `CREATE TABLE IF NOT EXISTS`、逐列 `ALTER TABLE ADD COLUMN`，再建索引。
    ///
    /// 幂等：重复调用什么都不做。**没有迁移脚本目录**——每张表的 DDL 常量就放在模型
    /// 旁边，一个测试断言它与 toasty 为空库生成的 DDL 字节相等（§8.2）。
    /// 在**一条普通（非 MVCC）连接**上，把已存在的文件库补到当前 schema。
    ///
    /// **为什么不顺着 [`Db::ensure_schema`] 那条池连接补**：MVCC 下 DDL 不落盘。实测
    /// （2026-09-20，真实旧库 + 委派那两列）：同一条 `ALTER TABLE … ADD COLUMN` 在普通
    /// 连接上重开可见；在 `concurrent_writes()` 的池连接上返回 `Ok`、日志照打"补一列"，
    /// 但重开就没了——同进程随后每条用到新列的语句都 `no such column`，重启也一样。
    /// 于是"schema 只增不改、连上时补列"（§8.2）在旧库上等于没有，而旧库升级恰恰只能靠它。
    /// 建池之后 [`Db::ensure_schema`] 只核对：文件库还缺列就报错，不再假装补上。
    ///
    /// 只处理**已存在**的文件；新文件交给 `push_schema`，内存库没有文件、由
    /// `ensure_schema` 在池上补。全程幂等。
    async fn migrate_file(path: &Path) -> Result<(), StoreError> {
        let database = turso::Builder::new_local(&path.to_string_lossy())
            .build()
            .await
            .map_err(|error| StoreError::Io(format!("打开 {} 失败：{error}", path.display())))?;
        let conn = database
            .connect()
            .map_err(|error| StoreError::Io(format!("连上 {} 失败：{error}", path.display())))?;

        // 分两段：**先一次读清**（表清单、每张表的列、索引名），把读游标连同连接一起丢掉，
        // 再在**一条只写的连接**上改。
        //
        // 必须这么分：turso 的读游标拖着一条读事务，DDL 要的是写事务，同一条连接上"边读
        // 边改"会 panic 在 `vdbe/execute.rs` 的 `SetCookie`（`invalid transaction state for
        // SetCookie: TransactionState::Read, should be write`，实测 2026-09-20）。`ensure_schema`
        // 那条路没炸，推测是它每句都走 toasty（`sql::query` / `sql::statement`）把语句收得
        // 更干净；不管原因是什么，**别在裸连接上照它那写法来**。
        let mut existing: Vec<String> = Vec::new();
        let mut present: Vec<(&str, Vec<String>)> = Vec::new();
        let mut indexes: Vec<String> = Vec::new();
        {
            let mut rows = conn
                .query("SELECT name FROM sqlite_master WHERE type = 'table'", ())
                .await
                .map_err(|error| StoreError::Other(error.to_string()))?;
            while let Some(row) = rows
                .next()
                .await
                .map_err(|error| StoreError::Other(error.to_string()))?
            {
                if let Some(turso::Value::Text(name)) = row
                    .get_value(0)
                    .map_err(|error| StoreError::Other(error.to_string()))?
                    .into()
                {
                    existing.push(name);
                }
            }
        }
        for table in models::TABLES {
            if !existing.iter().any(|name| name == table.name) {
                continue;
            }
            let mut columns: Vec<String> = Vec::new();
            {
                let mut rows = conn
                    .query(
                        &format!("SELECT name FROM pragma_table_info('{}')", table.name),
                        (),
                    )
                    .await
                    .map_err(|error| StoreError::Other(error.to_string()))?;
                while let Some(row) = rows
                    .next()
                    .await
                    .map_err(|error| StoreError::Other(error.to_string()))?
                {
                    if let Some(turso::Value::Text(name)) = row
                        .get_value(0)
                        .map_err(|error| StoreError::Other(error.to_string()))?
                        .into()
                    {
                        columns.push(name);
                    }
                }
            }
            present.push((table.name, columns));
        }
        {
            let mut rows = conn
                .query("SELECT name FROM sqlite_master WHERE type = 'index'", ())
                .await
                .map_err(|error| StoreError::Other(error.to_string()))?;
            while let Some(row) = rows
                .next()
                .await
                .map_err(|error| StoreError::Other(error.to_string()))?
            {
                if let turso::Value::Text(name) = row
                    .get_value(0)
                    .map_err(|error| StoreError::Other(error.to_string()))?
                {
                    indexes.push(name);
                }
            }
        }
        drop(conn);
        drop(database);

        let database = turso::Builder::new_local(&path.to_string_lossy())
            .build()
            .await
            .map_err(|error| StoreError::Io(format!("打开 {} 失败：{error}", path.display())))?;
        let conn = database
            .connect()
            .map_err(|error| StoreError::Io(format!("连上 {} 失败：{error}", path.display())))?;

        for table in models::TABLES {
            let Some((_, columns)) = present.iter().find(|(name, _)| *name == table.name) else {
                let create = table
                    .ddl
                    .replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1);
                conn.execute(&create, ())
                    .await
                    .map_err(|error| StoreError::Other(error.to_string()))?;
                continue;
            };
            for column in table.columns {
                if columns.iter().any(|name| name == column.name) {
                    continue;
                }
                tracing::info!(table = table.name, column = column.name, "补一列");
                conn.execute(
                    &format!(
                        "ALTER TABLE \"{}\" ADD COLUMN \"{}\" {}",
                        table.name, column.name, column.clause
                    ),
                    (),
                )
                .await
                .map_err(|error| StoreError::Other(error.to_string()))?;
                if let Some(backfill) = column.backfill {
                    // 补完之后**已有的行**是空的，默认值答不出它们该是什么——回填就写在
                    // 列旁边（`ColumnSpec::backfill`），这条路径与 `ensure_schema` 用同一份。
                    tracing::info!(table = table.name, column = column.name, "回填新列");
                    conn.execute(backfill, ())
                        .await
                        .map_err(|error| StoreError::Other(error.to_string()))?;
                }
            }
        }
        for index in models::INDEXES {
            // 与 `ensure_schema` 同一份判据：按**索引名**比对（`CREATE INDEX` 的语句文本
            // 各版本可能不同，拿整句去比会重复建，而已存在时再建一次是错）。
            if indexes
                .iter()
                .any(|name| index.contains(&format!("\"{name}\"")))
            {
                continue;
            }
            conn.execute(index, ())
                .await
                .map_err(|error| StoreError::Other(error.to_string()))?;
        }
        Ok(())
    }

    pub async fn ensure_schema(&self) -> Result<(), StoreError> {
        // DDL 不进 `BEGIN CONCURRENT`：建表 / 加列是连接期的单线程动作，没有竞争者，而
        // MVCC 对 DDL 的事务语义不在 spike 的实测范围内——不拿一个没验过的东西去承担
        // 一个本来就不需要的保证。
        let mut conn = self.inner.connection().await.map_err(map_toasty)?;

        for table in models::TABLES {
            let existing = table_columns(&mut conn, table.name).await?;
            match existing {
                None => {
                    let create =
                        table
                            .ddl
                            .replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1);
                    toasty::sql::statement(create)
                        .exec(&mut conn)
                        .await
                        .map_err(map_toasty)?;
                }
                Some(present) => {
                    for column in table.columns {
                        if present.iter().any(|name| name == column.name) {
                            continue;
                        }
                        // 文件库走到这里说明[建池之前那一次迁移](migrate_file)没做或做漏了。
                        // **宁可报错也不假装补上**：在这条 MVCC 连接上 `ALTER` 会返回 Ok、
                        // 然后重开就没了（实测 2026-09-20），静默丢掉比起不来更坏。
                        if self.path.is_some() {
                            return Err(StoreError::Io(format!(
                                "{} 缺一列 {}：文件库的补列必须走建池之前的迁移（Db::connect），\
                                 在已建池的连接上补会静默丢掉",
                                table.name, column.name
                            )));
                        }
                        tracing::info!(table = table.name, column = column.name, "补一列");
                        toasty::sql::statement(format!(
                            "ALTER TABLE \"{}\" ADD COLUMN \"{}\" {}",
                            table.name, column.name, column.clause
                        ))
                        .exec(&mut conn)
                        .await
                        .map_err(map_toasty)?;
                        if let Some(backfill) = column.backfill {
                            // 与 [`Db::migrate_file`] 同一份材料：列旁边写着回填，两条升级
                            // 路径就不会有一条漏掉（这条是内存库那条）。
                            tracing::info!(table = table.name, column = column.name, "回填新列");
                            toasty::sql::statement(backfill)
                                .exec(&mut conn)
                                .await
                                .map_err(map_toasty)?;
                        }
                    }
                }
            }
        }

        let present = index_names(&mut conn).await?;
        for index in models::INDEXES {
            if present
                .iter()
                .any(|name| index.contains(&format!("\"{name}\"")))
            {
                continue;
            }
            toasty::sql::statement(*index)
                .exec(&mut conn)
                .await
                .map_err(map_toasty)?;
        }
        Ok(())
    }

    /// 库里每张表当前的 `CREATE TABLE` 原文，按表名排序。DDL 字节对齐测试读它。
    pub async fn table_ddl(&self) -> Result<Vec<(String, String)>, StoreError> {
        let mut conn = self.inner.connection().await.map_err(map_toasty)?;
        let rows = toasty::sql::query(
            "SELECT name, sql FROM sqlite_master WHERE type = 'table' ORDER BY name",
        )
        .exec(&mut conn)
        .await
        .map_err(map_toasty)?;

        let mut out = Vec::new();
        for row in &rows {
            let name = column_string(row, 0);
            let sql = column_string(row, 1);
            // `sqlite_*` 是自动索引的影子，`__turso_internal_*` 是 MVCC 自己的元数据表
            // ——两者都不是 komo 的表，不该出现在 DDL 对齐里。
            if let (Some(name), Some(sql)) = (name, sql)
                && !name.starts_with("sqlite_")
                && !name.starts_with("__")
            {
                out.push((name, sql));
            }
        }
        Ok(out)
    }
}

/// 把 `PRAGMA data_sync_retry = 1` 打到**每一个池槽**上。
///
/// 它是每连接状态（`turso_core` 的 `connection.rs:452`），而 toasty 的驱动没有留连接
/// 建立钩子——所以这里同时握住 `pool_size` 个连接，逼 deadpool 把每个槽都创建出来，逐
/// 个设完再一起还回去。
///
/// TODO(decide: 池槽被健康检查淘汰后重建的那一个不会再被设到。彻底的做法是给
/// `vendor/toasty-driver-turso` 的 `Driver::connect` 加一个 pragma 钩子，但那会把 vendor
/// 从「只改 manifest」变成「改代码」——先记在这里，等编排者拍板。)
async fn prime_connections(db: &toasty::Db, pool_size: usize) -> Result<(), StoreError> {
    let mut held = Vec::with_capacity(pool_size);
    for _ in 0..pool_size.max(1) {
        let mut conn = db.connection().await.map_err(map_toasty)?;
        toasty::sql::statement("PRAGMA data_sync_retry = 1")
            .exec(&mut conn)
            .await
            .map_err(map_toasty)?;
        held.push(conn);
    }
    drop(held);
    Ok(())
}

/// 一张表现有的列名；表不存在时 `None`。
///
/// 列名从 `pragma_table_info` 读，**不从 `sqlite_master.sql` 里找子串**：turso 的
/// `ALTER TABLE … DROP COLUMN` 会把存下来的 DDL 原文重写成不带引号的形状，对着它做
/// 子串匹配会把每一列都判成"缺"，然后 `ADD COLUMN` 报 duplicate column name——一条
/// 在真实升级路径上才会踩到的路。
/// 库里已有的索引名。
async fn index_names(conn: &mut dyn Executor) -> Result<Vec<String>, StoreError> {
    let rows = toasty::sql::query("SELECT name FROM sqlite_master WHERE type = 'index'")
        .exec(conn)
        .await
        .map_err(map_toasty)?;
    Ok(rows
        .iter()
        .filter_map(|row| column_string(row, 0))
        .collect())
}

async fn table_columns(
    conn: &mut dyn Executor,
    name: &str,
) -> Result<Option<Vec<String>>, StoreError> {
    let exists =
        toasty::sql::query("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1")
            .bind(name.to_string())
            .exec(&mut *conn)
            .await
            .map_err(map_toasty)?;
    if exists.is_empty() {
        return Ok(None);
    }
    let rows = toasty::sql::query("SELECT name FROM pragma_table_info(?1)")
        .bind(name.to_string())
        .exec(conn)
        .await
        .map_err(map_toasty)?;
    Ok(Some(
        rows.iter()
            .filter_map(|row| column_string(row, 0))
            .collect(),
    ))
}

// ---------------------------------------------------------------- raw SQL 取值

/// 取 raw 查询结果里的第 `index` 列（文本）。
pub fn column_string(row: &toasty::stmt::Value, index: usize) -> Option<String> {
    match column(row, index)? {
        toasty::stmt::Value::String(text) => Some(text.to_string()),
        _ => None,
    }
}

/// 取第 `index` 列（整数）。SQLite 的整数宽度不固定，所以窄类型也收。
pub fn column_i64(row: &toasty::stmt::Value, index: usize) -> Option<i64> {
    match column(row, index)? {
        toasty::stmt::Value::I64(n) => Some(*n),
        toasty::stmt::Value::I32(n) => Some(i64::from(*n)),
        toasty::stmt::Value::I16(n) => Some(i64::from(*n)),
        toasty::stmt::Value::I8(n) => Some(i64::from(*n)),
        toasty::stmt::Value::U64(n) => i64::try_from(*n).ok(),
        toasty::stmt::Value::U32(n) => Some(i64::from(*n)),
        toasty::stmt::Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

/// 取第 `index` 列（BLOB）。
pub fn column_bytes(row: &toasty::stmt::Value, index: usize) -> Option<Vec<u8>> {
    match column(row, index)? {
        toasty::stmt::Value::Bytes(bytes) => Some(bytes.clone()),
        _ => None,
    }
}

fn column(row: &toasty::stmt::Value, index: usize) -> Option<&toasty::stmt::Value> {
    match row {
        toasty::stmt::Value::Record(record) => record.get(index),
        // 单列查询有的驱动直接给标量。
        other if index == 0 => Some(other),
        _ => None,
    }
}

// ---------------------------------------------------------------- 时间

/// `OffsetDateTime` → 列里存的 i64（unix **纳秒**）。
///
/// 秒精度会把 `MemoryItem` 这种带时间戳的值类型在一次 put / get 之后改掉——存进去什么
/// 读出来就该是什么，所以存全精度。i64 纳秒覆盖 1970 ± 292 年。
pub fn to_ts(at: OffsetDateTime) -> i64 {
    i64::try_from(at.unix_timestamp_nanos()).unwrap_or(i64::MAX)
}

/// 列里的 i64 → `OffsetDateTime`。
pub fn from_ts(raw: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(raw)).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// 可选时间 → i64，`None` 记作哨兵 `0`。
///
/// **`0` 就是 1970-01-01T00:00:00Z**，而那个时刻在 komo 里没有意义（§8 的每个时间都是
/// 运行期产生的），所以用它当"未设置"比多开一列可空时间省事。
pub fn to_ts_opt(at: Option<OffsetDateTime>) -> i64 {
    at.map(to_ts).unwrap_or(0)
}

/// i64 → 可选时间，哨兵 `0` 读作 `None`。
pub fn from_ts_opt(raw: i64) -> Option<OffsetDateTime> {
    (raw != 0).then(|| from_ts(raw))
}

// ---------------------------------------------------------------- 错误

/// toasty 错误 → [`StoreError`]。
///
/// **序列化失败（MVCC 写写冲突 / `database is locked`）映射成
/// [`StoreError::Contended`]**，这就是 [`Db::with_write_retry`] 认出"该重试"的方式：
/// 谓词是 `is_serialization_failure()`，不是字符串匹配（§8.2）。
pub fn map_toasty(error: toasty::Error) -> StoreError {
    if error.is_serialization_failure() {
        StoreError::Contended
    } else {
        StoreError::Other(error.to_string())
    }
}

/// [`StoreError`] → [`LedgerError`]。
///
/// `StoreError` → `RepoError` 的方向**不在这里**：kernel 有
/// `impl From<StoreError> for RepoError`，两个领域性变体在那儿逐个对上，仓储直接
/// `?` 或 `.map_err(RepoError::from)` 就行。
///
/// 账本这边没有对应的变体：预期 revision 不符、授权覆盖不到这份计划，对
/// [`LedgerError`] 来说都是"这个状态下做不了这件事"，所以并进 [`LedgerError::Conflict`]
/// ——**并进去的是文本，不是沉默**。
pub fn store_to_ledger(error: StoreError) -> LedgerError {
    match error {
        StoreError::NotFound { what } => LedgerError::NotFound { what },
        StoreError::Contended => LedgerError::Contended,
        StoreError::Corrupt(message) => LedgerError::Corrupt(message),
        StoreError::VersionConflict { expected, actual } => {
            LedgerError::Conflict(format!("版本冲突：预期 {expected}，当前 {actual}"))
        }
        StoreError::GrantMismatch(message) => {
            LedgerError::Conflict(format!("授权不匹配：{message}"))
        }
        StoreError::Io(message) => LedgerError::Persist(message),
        StoreError::Other(message) => LedgerError::Conflict(message),
    }
}

/// JSON 编码一个要进 TEXT 列的值。
pub fn encode<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|e| StoreError::Other(format!("列编码失败：{e}")))
}

/// 解码一个 TEXT 列里的 JSON。
pub fn decode<T: serde::de::DeserializeOwned>(raw: &str, what: &str) -> Result<T, StoreError> {
    serde_json::from_str(raw).map_err(|e| StoreError::Corrupt(format!("{what} 解析失败：{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{SessionKind, SessionRow};
    use komo_kernel::types::ids::SessionId;

    async fn temp() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("state.db"))
            .await
            .expect("打开库");
        (db, dir)
    }

    /// 在**普通（非 MVCC）连接**上跑一句 SQL。
    ///
    /// 测试里造"旧形状"必须用它：在池连接上做 DDL 会静默丢掉，那样的测试两边都在空转
    /// （`a_missing_column_is_added_in_place` 原来就是这样，真机上"旧库升级起不来"才漏过去）。
    async fn plain_exec(path: &Path, sql: &str) {
        let db = turso::Builder::new_local(&path.to_string_lossy())
            .build()
            .await
            .expect("打开普通连接");
        let conn = db.connect().expect("连接");
        conn.execute(sql, ()).await.expect("执行");
    }

    /// 用**另一条新连接**问一次列清单——形状是不是真的落盘了，只有它能回答。
    async fn plain_columns(path: &Path, table: &str) -> Vec<String> {
        let db = turso::Builder::new_local(&path.to_string_lossy())
            .build()
            .await
            .expect("打开普通连接");
        let conn = db.connect().expect("连接");
        let mut rows = conn
            .query(
                &format!("SELECT name FROM pragma_table_info('{table}')"),
                (),
            )
            .await
            .expect("查列");
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.expect("下一行") {
            if let turso::Value::Text(name) = row.get_value(0).expect("取值") {
                out.push(name);
            }
        }
        out
    }

    /// 用**普通连接**把整张 schema 按"旧形状"摆出来——老库就是上一个版本这样建出来的。
    ///
    /// `legacy` 给"这张表用哪份 DDL"（比如去掉新列的 runs）。不先开池是必要的：池的连接
    /// 释放得慢，普通连接上去改会 `database is locked`；而"先建池再改"正是空转测试的写法。
    async fn plain_build_old(path: &Path, legacy: Option<(&str, String)>) {
        for table in crate::models::TABLES {
            let ddl = match &legacy {
                Some((name, ddl)) if *name == table.name => ddl.clone(),
                _ => table.ddl.to_string(),
            };
            plain_exec(
                path,
                &ddl.replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1),
            )
            .await;
        }
        for index in crate::models::INDEXES {
            plain_exec(path, index).await;
        }
    }

    async fn insert(db: &Db, id: String) -> Result<(), StoreError> {
        db.with_write_retry(move |ex| {
            let id = id.clone();
            Box::pin(async move {
                toasty::create!(SessionRow {
                    id,
                    title: String::new(),
                    origin: "test",
                    agent_id: String::new(),
                    kind: "normal",
                    workdir: None as Option<String>,
                    current_run: None as Option<String>,
                    jsonl_path: String::new(),
                    applied_seq: 0_i64,
                    applied_bytes: 0_i64,
                    created_at: 0_i64,
                    updated_at: 0_i64,
                    state: "active",
                    state_changed_at: 0_i64,
                })
                .exec(ex)
                .await
                .map_err(map_toasty)?;
                Ok(())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
    }

    /// 验收 ③：两个写入器对**不同** Session 并发提交不互相阻塞。
    ///
    /// spike 实测 autocommit 下 200 次只成 6 次，包进事务 200/200 全成——这条测试锁住
    /// 的就是"每个写都进 `db.transaction()`"这条规则（§8.2）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_writers_on_different_sessions_never_block_each_other() {
        const ROUNDS: usize = 60;
        let (db, _dir) = temp().await;

        let mut handles = Vec::new();
        for writer in 0..2 {
            let db = db.clone();
            handles.push(tokio::spawn(async move {
                let mut failures = Vec::new();
                for round in 0..ROUNDS {
                    if let Err(error) = insert(&db, format!("w{writer}-{round}")).await {
                        failures.push(error);
                    }
                }
                failures
            }));
        }

        let mut failures = Vec::new();
        for handle in handles {
            failures.extend(handle.await.expect("写入任务没有 panic"));
        }
        assert!(failures.is_empty(), "并发写不同行不该失败：{failures:?}");

        let count = db
            .read(|ex| {
                Box::pin(
                    async move { SessionRow::all().count().exec(ex).await.map_err(map_toasty) },
                ) as BoxFuture<'_, Result<u64, StoreError>>
            })
            .await
            .unwrap();
        assert_eq!(count, (ROUNDS * 2) as u64);
    }

    /// 验收 ④：同一行冲突时，一方重试成功，而且**只应用一次**。
    ///
    /// 两个写入器都读同一行、各自 +1、再写回。读-改-写在一个 `BEGIN CONCURRENT` 事务
    /// 里，所以败者拿到 serialization failure、整段干净重跑，绝不双重应用——最终计数正
    /// 好是写入次数。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_conflicted_write_is_retried_and_applied_exactly_once() {
        const WRITERS: i64 = 4;
        const EACH: i64 = 15;
        let (db, _dir) = temp().await;
        insert(&db, "counter".to_string()).await.unwrap();

        let mut handles = Vec::new();
        for _ in 0..WRITERS {
            let db = db.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..EACH {
                    db.with_write_retry(|ex| {
                        Box::pin(async move {
                            let mut row = SessionRow::filter_by_id("counter")
                                .get(&mut *ex)
                                .await
                                .map_err(map_toasty)?;
                            let next = row.applied_seq + 1;
                            row.update()
                                .applied_seq(next)
                                .exec(&mut *ex)
                                .await
                                .map_err(map_toasty)?;
                            Ok(())
                        }) as BoxFuture<'_, Result<(), StoreError>>
                    })
                    .await
                    .expect("重试之后应当成功");
                }
            }));
        }
        for handle in handles {
            handle.await.expect("写入任务没有 panic");
        }

        let final_value = db
            .read(|ex| {
                Box::pin(async move {
                    SessionRow::filter_by_id("counter")
                        .get(ex)
                        .await
                        .map_err(map_toasty)
                        .map(|row| row.applied_seq)
                }) as BoxFuture<'_, Result<i64, StoreError>>
            })
            .await
            .unwrap();
        assert_eq!(
            final_value,
            WRITERS * EACH,
            "每次自增只能应用一次——少了是丢写，多了是重跑时双重应用"
        );
    }

    /// 验收 ⑫：关掉再打开，`ensure_schema` 幂等，数据读得回来。
    #[tokio::test]
    async fn reopening_an_existing_file_keeps_the_schema_and_the_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        let db = Db::connect(&path).await.unwrap();
        insert(&db, "kept".to_string()).await.unwrap();
        let before = db.table_ddl().await.unwrap();
        drop(db);

        let db = Db::connect(&path).await.unwrap();
        // 幂等：再跑一次什么都不做。
        db.ensure_schema().await.unwrap();
        db.ensure_schema().await.unwrap();
        let after = db.table_ddl().await.unwrap();
        assert_eq!(before, after, "重开之后 schema 不该变");

        let found = db
            .read(|ex| {
                Box::pin(async move {
                    SessionRow::filter_by_id("kept")
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)
                }) as BoxFuture<'_, Result<Option<SessionRow>, StoreError>>
            })
            .await
            .unwrap();
        assert!(found.is_some(), "重开之后行还在");
    }

    /// 旧库缺一列时，**重开**（建池之前的迁移）补得上，而且补完能写能读。
    ///
    /// 造"旧形状"与"确认它真的落盘"都必须在**普通连接**上做：在池连接上做 DDL 会静默丢掉，
    /// 那样的测试两边都在空转——这一条原来就是那样写的，所以真机上"旧库升级起不来"才漏
    /// 过去了。`plain_columns` 用**另一条新连接**问同一件事，就是为了不让这条再空转。
    #[tokio::test]
    async fn a_missing_column_is_added_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        // 旧形状：sessions 少一列。
        let legacy = crate::models::session::DDL.replace(r#""origin" TEXT NOT NULL, "#, "");
        assert!(!legacy.contains("origin"), "旧形状里不该有 origin");
        plain_build_old(&path, Some(("sessions", legacy))).await;

        let before = plain_columns(&path, "sessions").await;
        assert!(
            !before.iter().any(|name| name == "origin"),
            "旧形状没落盘，后面的断言就没有意义：{before:?}"
        );

        // 重开：迁移在建池之前把它补回来，而且**落盘**。
        let db = Db::connect(&path).await.unwrap();
        let after = plain_columns(&path, "sessions").await;
        assert!(
            after.iter().any(|name| name == "origin"),
            "补回来了，而且落盘：{after:?}"
        );

        insert(&db, "after-alter".to_string()).await.unwrap();
    }

    /// 委派那两列在**旧形状的 runs 表**上补得回来（§8.2 的加列规则，也是升级路径）。
    ///
    /// 同上：旧形状在普通连接上造、在另一条新连接上确认，然后才重开验迁移。
    #[tokio::test]
    async fn the_delegate_columns_are_added_to_an_existing_runs_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        let legacy = crate::models::run::DDL
            .replace(r#""parent_run_id" TEXT, "#, "")
            .replace(r#""delegate" TEXT, "#, "");
        assert!(
            !legacy.contains("parent_run_id") && !legacy.contains("delegate"),
            "旧形状里不该有这两列"
        );
        plain_build_old(&path, Some(("runs", legacy))).await;

        let before = plain_columns(&path, "runs").await;
        assert!(
            !before.iter().any(|name| name == "parent_run_id"),
            "旧形状没落盘：{before:?}"
        );

        let db = Db::connect(&path).await.unwrap();
        let after = plain_columns(&path, "runs").await;
        for column in ["parent_run_id", "delegate"] {
            assert!(
                after.iter().any(|name| name == column),
                "{column} 补回来了，而且落盘：{after:?}"
            );
        }
        assert!(
            crate::repos::runs::get(&db, &komo_kernel::types::ids::RunId::from_raw("缺的"))
                .await
                .unwrap()
                .is_none(),
            "补完之后按模型读一遍不报错：SELECT 的列清单在这张表上成立"
        );
    }

    /// 把 `sessions` 退回"还没有 Agent 归属"的旧形状：**先丢掉引用了那两列的索引，再删列**
    /// ——老库里既没有那条索引，也没有这两列。真机上老库就是这么长出来的。
    async fn plain_roll_back_agent_columns(path: &Path) {
        for sql in [
            r#"DROP INDEX IF EXISTS "sessions_main_per_agent""#,
            r#"ALTER TABLE "sessions" DROP COLUMN "kind""#,
            r#"ALTER TABLE "sessions" DROP COLUMN "agent_id""#,
        ] {
            plain_exec(path, sql).await;
        }
    }

    /// 升级之前就存在的那两行：操作者的全局主会话（`origin = home`）与一个平台会话。
    async fn plain_insert_legacy_sessions(path: &Path) {
        plain_exec(
            path,
            r#"INSERT INTO "sessions" ("id", "title", "origin", "workdir", "current_run", "jsonl_path", "applied_seq", "applied_bytes", "created_at", "updated_at", "state", "state_changed_at") VALUES ('h', '', 'home', NULL, NULL, 'p', 0, 0, 1, 1, 'active', 1), ('a', '', 'feishu:1', NULL, NULL, 'p', 0, 0, 2, 2, 'active', 2)"#,
        )
        .await;
    }

    /// 老库（没有 `agent_id` / `kind`）打开：两列补得上、能起，而且**已有的行**回填了
    /// ——`origin = 'home'` 的那条是主会话，其余是普通会话；`agent_id` 留空串
    /// （`docs/bot.md` §4.2）。
    ///
    /// 旧形状照旧在普通连接上造、在另一条新连接上确认（见
    /// `a_missing_column_is_added_on_reopen`），重开才走 [`Db::migrate_file`]。
    #[tokio::test]
    async fn the_agent_columns_are_added_and_backfilled_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        plain_build_old(&path, None).await;
        plain_roll_back_agent_columns(&path).await;
        plain_insert_legacy_sessions(&path).await;
        let before = plain_columns(&path, "sessions").await;
        assert!(
            !before
                .iter()
                .any(|name| name == "agent_id" || name == "kind"),
            "旧形状没落盘，后面的断言就没有意义：{before:?}"
        );

        let db = Db::connect(&path).await.unwrap();
        let after = plain_columns(&path, "sessions").await;
        for column in ["agent_id", "kind"] {
            assert!(
                after.iter().any(|name| name == column),
                "{column} 补回来了，而且落盘：{after:?}"
            );
        }

        let home = crate::repos::session::get(&db, &SessionId::from_raw("h"))
            .await
            .unwrap()
            .expect("旧行读得回来");
        assert_eq!(
            home.kind,
            SessionKind::Main,
            "origin = home 的那条按回填规则是主会话"
        );
        assert_eq!(home.agent_id, "", "空串 = 还没有归属，迁移不替路由认主");
        let other = crate::repos::session::get(&db, &SessionId::from_raw("a"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(other.kind, SessionKind::Normal, "非 home 的是普通会话");
        assert_eq!(other.agent_id, "");

        // 补完之后两列能用：那条老的主会话可以被认领一次（认领 = `set_agent_in`），
        // 认完就是 `main_session(assistant)`。
        assert!(
            crate::repos::session::set_agent(&db, &SessionId::from_raw("h"), "assistant")
                .await
                .unwrap(),
            "升级前的主会话还没有归属，第一次认领要写进去"
        );
        let main = crate::repos::session::main_for_agent(&db, "assistant")
            .await
            .unwrap()
            .expect("认领之后它就是这个 Agent 的主会话");
        assert_eq!(main.row.id, "h");
        assert!(main.duplicates.is_empty());
    }

    /// 时间列是 unix 纳秒，`0` 是"未设置"。
    #[test]
    fn a_timestamp_round_trips_at_full_precision() {
        let at = time::macros::datetime!(2026-09-15 08:00:00.123456789 UTC);
        assert_eq!(from_ts(to_ts(at)), at);
        assert_eq!(to_ts_opt(None), 0);
        assert_eq!(from_ts_opt(0), None);
        assert_eq!(from_ts_opt(to_ts(at)), Some(at));
    }

    #[test]
    fn the_backoff_grows_and_then_stops_growing() {
        let retry = RetryConfig::default();
        assert!(retry.delay_for(0) < retry.delay_for(3));
        assert_eq!(retry.delay_for(30), retry.max_delay);
    }

    /// `models::INDEXES` 真的建出来了。
    ///
    /// 这一条不是形式主义：MVCC 下 DDL 只在"这个库上没有别的连接"时才成功，而失败
    /// 的形状是 `database is locked` —— 如果 `ensure_schema` 挪到连接池铺开之后，索引
    /// 会**静默地**一个都建不出来（见 `build` 的注释）。
    #[tokio::test]
    async fn the_declared_indexes_exist_after_connect() {
        let (db, _dir) = temp().await;
        let names = db
            .read(|ex| {
                Box::pin(async move {
                    let rows =
                        toasty::sql::query("SELECT name FROM sqlite_master WHERE type = 'index'")
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                    Ok(rows
                        .iter()
                        .filter_map(|row| column_string(row, 0))
                        .collect::<Vec<_>>())
                }) as BoxFuture<'_, Result<Vec<String>, StoreError>>
            })
            .await
            .unwrap();
        for declared in crate::models::INDEXES {
            let name = declared
                .split('"')
                .nth(1)
                .expect("每条 CREATE INDEX 都写着索引名");
            assert!(names.iter().any(|n| n == name), "缺索引 {name}：{names:?}");
        }
    }

    /// `data_sync_retry` 真的打到了池里的连接上——默认是 `false`，那时 fsync 出错是
    /// `panic!` 而不是 `Err`（§8.2）。
    #[tokio::test]
    async fn every_pooled_connection_reports_data_sync_retry_on() {
        let (db, _dir) = temp().await;
        for _ in 0..8 {
            let value = db
                .read(|ex| {
                    Box::pin(async move {
                        let rows = toasty::sql::query("PRAGMA data_sync_retry")
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                        Ok(rows.first().and_then(|row| column_i64(row, 0)))
                    }) as BoxFuture<'_, Result<Option<i64>, StoreError>>
                })
                .await
                .unwrap();
            assert_eq!(value, Some(1), "fsync 出错要能报告，而不是把进程带走");
        }
    }
}
