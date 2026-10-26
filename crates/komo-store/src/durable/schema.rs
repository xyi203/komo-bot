//! 新库文件的表结构、建表与补列（§3.2、§3.7、§8）。
//!
//! 规则沿用 `db.rs`：**只加不改**。新建时一次事务建全表并写下格式版本；已存在时先核对版本
//! （比本二进制新就拒绝，§8"旧二进制要能识别并拒绝"），再对每张表比对列清单、缺的用
//! `ALTER TABLE ADD COLUMN` 补上（新列必须可空或 `NOT NULL DEFAULT`）、缺的索引补建。
//!
//! 没有 toasty 模型，所以"DDL 常量 = 实际建出的 schema"由本文件的测试担保：新建一个库，
//! 读 `sqlite_schema` 与 `pragma_table_info`，与常量和 [`TABLES`] 逐项比对。
//!
//! 记录正文列（`record` / `value`）存 JSON 文本；D1 的记录类型来了直接 serde。能被索引过滤的
//! 字段（状态、归属、请求键……）单独成列，与 pi 的 `migrations.ts` 同形。

use std::path::{Path, PathBuf};

use turso::{Builder, Connection, Database, Value};

use super::line::LineError;

/// 数据目录下的新库文件名（§3.7：老 `state.db` 原样留着做回滚）。
pub const DURABLE_DB_FILE: &str = "durable.db";

/// 本二进制写入、也只认识到的格式版本。库里的版本更大就拒绝打开。
pub const FORMAT_VERSION: i64 = 1;

/// `<data_dir>/durable.db`。
pub fn durable_db_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DURABLE_DB_FILE)
}

/// `durable_meta` 里的键。
pub(crate) const META_FORMAT_VERSION: &str = "format_version";
pub(crate) const META_LAST_SEQ: &str = "last_seq";
/// 已分配过的最大 ID 的高水位（设计 §6.1：ID 不复用）。随分配它的那次提交同事务推进；
/// purge 删光末尾的行之后重开，取号器仍从它起，不会重铸已发出的 ID。
pub(crate) const META_MAX_ID: &str = "max_id";

// 每张表的 DDL 常量。`STRICT` + `json_valid`：坏 JSON 在写入点就被挡住，而不是等 serde 读到。

/// 格式版本与提交 seq 的落点。键值表而不是 pi 的单行表：加一项不用加列。
pub const DURABLE_META_TABLE_DDL: &str = "CREATE TABLE durable_meta (
    name TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT";

/// 一个分支 = 一个 conversation；`parent_*` 是 fork 的来源，`owner_*` 是嵌套调用的归属。
pub const CONVERSATIONS_TABLE_DDL: &str = "CREATE TABLE conversations (
    id INTEGER PRIMARY KEY NOT NULL,
    owner_conversation INTEGER,
    owner_task INTEGER,
    parent_conversation INTEGER,
    parent_at INTEGER,
    session_id TEXT,
    commit_seq INTEGER NOT NULL,
    record TEXT NOT NULL CHECK (json_valid(record))
) STRICT";

pub const ENTRIES_TABLE_DDL: &str = "CREATE TABLE entries (
    id INTEGER PRIMARY KEY NOT NULL,
    conversation_id INTEGER NOT NULL,
    head INTEGER,
    commit_seq INTEGER NOT NULL,
    record TEXT NOT NULL CHECK (json_valid(record))
) STRICT";

pub const TASKS_TABLE_DDL: &str = "CREATE TABLE tasks (
    id INTEGER PRIMARY KEY NOT NULL,
    conversation_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    abort_requested INTEGER NOT NULL CHECK (abort_requested IN (0, 1)),
    background INTEGER NOT NULL CHECK (background IN (0, 1)),
    record TEXT NOT NULL CHECK (json_valid(record))
) STRICT";

pub const SUBMISSIONS_TABLE_DDL: &str = "CREATE TABLE submissions (
    id INTEGER PRIMARY KEY NOT NULL,
    conversation_id INTEGER NOT NULL,
    request_id TEXT,
    status TEXT NOT NULL,
    record TEXT NOT NULL CHECK (json_valid(record))
) STRICT";

/// 整值，不做 delta 表（§3.3）。`version` 是调用方的**定义版本**（D1 的 `DocumentRecord::version`），
/// 不是自增计数。`key_value` 由调用方编码，使 `None` 与 `Some("")` 分得开（见 `storage.rs`）。
/// 值没变的 UPDATE，WAL 认出页面没变就不写盘（D0 的坑）——语义上没有东西要持久化，所以是对的。
pub const DOCUMENTS_TABLE_DDL: &str = "CREATE TABLE documents (
    id INTEGER PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    owner_id INTEGER NOT NULL,
    key_value TEXT NOT NULL,
    version INTEGER NOT NULL,
    created_seq INTEGER NOT NULL,
    changed_seq INTEGER NOT NULL,
    retired_seq INTEGER,
    value TEXT NOT NULL CHECK (json_valid(value))
) STRICT";

/// komo Session：对外 UUIDv7、生命周期（§8.10）、当前分支与根 conversation（§4.2）。
pub const SESSIONS_TABLE_DDL: &str = "CREATE TABLE sessions (
    id TEXT PRIMARY KEY NOT NULL,
    lifecycle TEXT NOT NULL,
    current_branch INTEGER,
    root_conversation INTEGER NOT NULL,
    record TEXT NOT NULL CHECK (json_valid(record))
) STRICT";

/// 一列的补列材料：`ALTER TABLE ADD COLUMN` 的子句（类型 + 约束，须可空或带默认值）。
pub struct ColumnSpec {
    pub name: &'static str,
    pub clause: &'static str,
}

pub struct TableSpec {
    pub name: &'static str,
    pub ddl: &'static str,
    /// 按 DDL 里的顺序。
    pub columns: &'static [ColumnSpec],
}

const fn col(name: &'static str, clause: &'static str) -> ColumnSpec {
    ColumnSpec { name, clause }
}

pub const TABLES: &[TableSpec] = &[
    TableSpec {
        name: "durable_meta",
        ddl: DURABLE_META_TABLE_DDL,
        columns: &[
            col("name", "TEXT"),
            col("value", "TEXT NOT NULL DEFAULT ''"),
        ],
    },
    TableSpec {
        name: "conversations",
        ddl: CONVERSATIONS_TABLE_DDL,
        columns: &[
            col("id", "INTEGER"),
            col("owner_conversation", "INTEGER"),
            col("owner_task", "INTEGER"),
            col("parent_conversation", "INTEGER"),
            col("parent_at", "INTEGER"),
            col("session_id", "TEXT"),
            col("commit_seq", "INTEGER NOT NULL DEFAULT 0"),
            col("record", "TEXT NOT NULL DEFAULT '{}'"),
        ],
    },
    TableSpec {
        name: "entries",
        ddl: ENTRIES_TABLE_DDL,
        columns: &[
            col("id", "INTEGER"),
            col("conversation_id", "INTEGER NOT NULL DEFAULT 0"),
            col("head", "INTEGER"),
            col("commit_seq", "INTEGER NOT NULL DEFAULT 0"),
            col("record", "TEXT NOT NULL DEFAULT '{}'"),
        ],
    },
    TableSpec {
        name: "tasks",
        ddl: TASKS_TABLE_DDL,
        columns: &[
            col("id", "INTEGER"),
            col("conversation_id", "INTEGER NOT NULL DEFAULT 0"),
            col("kind", "TEXT NOT NULL DEFAULT ''"),
            col("status", "TEXT NOT NULL DEFAULT ''"),
            col("abort_requested", "INTEGER NOT NULL DEFAULT 0"),
            col("background", "INTEGER NOT NULL DEFAULT 0"),
            col("record", "TEXT NOT NULL DEFAULT '{}'"),
        ],
    },
    TableSpec {
        name: "submissions",
        ddl: SUBMISSIONS_TABLE_DDL,
        columns: &[
            col("id", "INTEGER"),
            col("conversation_id", "INTEGER NOT NULL DEFAULT 0"),
            col("request_id", "TEXT"),
            col("status", "TEXT NOT NULL DEFAULT ''"),
            col("record", "TEXT NOT NULL DEFAULT '{}'"),
        ],
    },
    TableSpec {
        name: "documents",
        ddl: DOCUMENTS_TABLE_DDL,
        columns: &[
            col("id", "INTEGER"),
            col("kind", "TEXT NOT NULL DEFAULT ''"),
            col("scope_kind", "TEXT NOT NULL DEFAULT ''"),
            col("owner_id", "INTEGER NOT NULL DEFAULT 0"),
            col("key_value", "TEXT NOT NULL DEFAULT ''"),
            col("version", "INTEGER NOT NULL DEFAULT 0"),
            col("created_seq", "INTEGER NOT NULL DEFAULT 0"),
            col("changed_seq", "INTEGER NOT NULL DEFAULT 0"),
            col("retired_seq", "INTEGER"),
            col("value", "TEXT NOT NULL DEFAULT '{}'"),
        ],
    },
    TableSpec {
        name: "sessions",
        ddl: SESSIONS_TABLE_DDL,
        columns: &[
            col("id", "TEXT"),
            col("lifecycle", "TEXT NOT NULL DEFAULT ''"),
            col("current_branch", "INTEGER"),
            col("root_conversation", "INTEGER NOT NULL DEFAULT 0"),
            col("record", "TEXT NOT NULL DEFAULT '{}'"),
        ],
    },
];

/// 索引（名字 + 语句）。参照 pi 的 `migrations.ts`；`conversations_by_parent` 是 komo 加的（§4.2
/// 的 `/tree` 视图要按父找子）。已存在时补建的判据是**索引名**，不比语句文本。
pub const INDEXES: &[(&str, &str)] = &[
    (
        "conversations_by_owner_conversation",
        "CREATE INDEX conversations_by_owner_conversation ON conversations (owner_conversation, id)",
    ),
    (
        "conversations_by_owner_task",
        "CREATE INDEX conversations_by_owner_task ON conversations (owner_task, id)",
    ),
    (
        "conversations_by_parent",
        "CREATE INDEX conversations_by_parent ON conversations (parent_conversation, id)",
    ),
    (
        "conversations_by_session",
        "CREATE INDEX conversations_by_session ON conversations (session_id, id)",
    ),
    (
        "entries_by_conversation",
        "CREATE INDEX entries_by_conversation ON entries (conversation_id, id DESC)",
    ),
    (
        "entry_heads_by_conversation",
        "CREATE INDEX entry_heads_by_conversation ON entries (conversation_id, id DESC) WHERE head IS NOT NULL",
    ),
    (
        "tasks_by_status",
        "CREATE INDEX tasks_by_status ON tasks (status, id)",
    ),
    (
        "tasks_by_conversation",
        "CREATE INDEX tasks_by_conversation ON tasks (conversation_id, id)",
    ),
    (
        "tasks_by_kind",
        "CREATE INDEX tasks_by_kind ON tasks (kind, id)",
    ),
    (
        "tasks_by_abort_requested",
        "CREATE INDEX tasks_by_abort_requested ON tasks (abort_requested, id)",
    ),
    (
        "tasks_by_background",
        "CREATE INDEX tasks_by_background ON tasks (background, id)",
    ),
    (
        "submissions_by_request",
        "CREATE INDEX submissions_by_request ON submissions (conversation_id, request_id)",
    ),
    (
        "submissions_by_conversation",
        "CREATE INDEX submissions_by_conversation ON submissions (conversation_id, id)",
    ),
    (
        "submissions_by_status",
        "CREATE INDEX submissions_by_status ON submissions (status, id)",
    ),
    (
        "documents_by_address",
        "CREATE INDEX documents_by_address ON documents (kind, scope_kind, owner_id, key_value, created_seq DESC, retired_seq)",
    ),
    (
        "documents_by_scope",
        "CREATE INDEX documents_by_scope ON documents (scope_kind, owner_id, id)",
    ),
    (
        "documents_by_scope_kind",
        "CREATE INDEX documents_by_scope_kind ON documents (scope_kind, owner_id, kind, id)",
    ),
    (
        "sessions_by_lifecycle",
        "CREATE INDEX sessions_by_lifecycle ON sessions (lifecycle, id)",
    ),
];

fn db_err(error: turso::Error) -> LineError {
    LineError::Open(error.to_string())
}

/// 打开（必要时建出 / 补齐）库文件，返回库与已配置好的写连接。
///
/// 连接配置：WAL（turso 默认的 journal 模式，开库后回读核对）、`synchronous=FULL`、
/// `data_sync_retry=1`——默认 `false` 时 fsync 出错是 `panic!` 而不是 `Err`（`db.rs` 同一条理由）。
/// 写连接还启用 `fullfsync`，让 Apple 平台的 FULL 提交冲刷设备写缓存。
pub(crate) async fn open(path: &Path) -> Result<(Database, Connection), LineError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| LineError::Open(format!("建立 {} 失败：{e}", parent.display())))?;
    }
    let database = Builder::new_local(&path.to_string_lossy())
        .build()
        .await
        .map_err(db_err)?;
    let conn = database.connect().map_err(db_err)?;
    conn.execute("PRAGMA data_sync_retry = 1", ())
        .await
        .map_err(db_err)?;
    conn.execute("PRAGMA fullfsync = ON", ())
        .await
        .map_err(db_err)?;
    conn.execute("PRAGMA synchronous = FULL", ())
        .await
        .map_err(db_err)?;

    let mode = pragma_text(&conn, "journal_mode").await?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(LineError::Open(format!(
            "{} 的 journal_mode 是 {mode}，不是 wal；新库只用 WAL（§3.7）",
            path.display()
        )));
    }

    let tables = column_one(&conn, "SELECT name FROM sqlite_schema WHERE type = 'table'").await?;
    if tables.iter().any(|t| t == "durable_meta") {
        check_version(&conn, path).await?;
        add_missing(&conn, &tables).await?;
    } else {
        create_all(&conn).await?;
    }
    Ok((database, conn))
}

/// 一次事务建全表 + 索引 + 元数据；元数据最后写，所以半建的库不会被当成已建好的。
async fn create_all(conn: &Connection) -> Result<(), LineError> {
    conn.execute("BEGIN", ()).await.map_err(db_err)?;
    let result: Result<(), turso::Error> = async {
        for table in TABLES {
            if table.name == "durable_meta" {
                continue;
            }
            let ddl = table
                .ddl
                .replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1);
            conn.execute(&ddl, ()).await?;
        }
        for (_, sql) in INDEXES {
            let sql = sql.replacen("CREATE INDEX ", "CREATE INDEX IF NOT EXISTS ", 1);
            conn.execute(&sql, ()).await?;
        }
        conn.execute(DURABLE_META_TABLE_DDL, ()).await?;
        conn.execute(
            "INSERT INTO durable_meta (name, value) VALUES (?1, ?2)",
            [Value::Text(META_LAST_SEQ.into()), Value::Text("0".into())],
        )
        .await?;
        conn.execute(
            "INSERT INTO durable_meta (name, value) VALUES (?1, ?2)",
            [
                Value::Text(META_FORMAT_VERSION.into()),
                Value::Text(FORMAT_VERSION.to_string()),
            ],
        )
        .await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => conn.execute("COMMIT", ()).await.map(|_| ()).map_err(db_err),
        Err(error) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(db_err(error))
        }
    }
}

async fn check_version(conn: &Connection, path: &Path) -> Result<(), LineError> {
    let found = query_one_text(
        conn,
        "SELECT value FROM durable_meta WHERE name = 'format_version'",
    )
    .await?;
    let found: i64 = found
        .as_deref()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| {
            LineError::Open(format!("{} 缺少或损坏格式版本，拒绝打开", path.display()))
        })?;
    if found > FORMAT_VERSION {
        return Err(LineError::UnsupportedFormat {
            found,
            supported: FORMAT_VERSION,
        });
    }
    Ok(())
}

/// 已存在的库：缺表建表、缺列 `ADD COLUMN`、缺索引补建。
///
/// **先一次读清、再改**：turso 的读游标拖着读事务，同一连接上"边读边改 schema"会 panic
/// （`db.rs` 的 `migrate_file` 注释，实测 2026-09-20），所以这里的读都收进 `Vec` 后再动 DDL。
async fn add_missing(conn: &Connection, tables: &[String]) -> Result<(), LineError> {
    let mut present: Vec<(&TableSpec, Option<Vec<String>>)> = Vec::new();
    for table in TABLES {
        let columns = if tables.iter().any(|t| t == table.name) {
            Some(
                column_one(
                    conn,
                    &format!("SELECT name FROM pragma_table_info('{}')", table.name),
                )
                .await?,
            )
        } else {
            None
        };
        present.push((table, columns));
    }
    let indexes = column_one(conn, "SELECT name FROM sqlite_schema WHERE type = 'index'").await?;

    for (table, columns) in present {
        let Some(columns) = columns else {
            conn.execute(
                &table
                    .ddl
                    .replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1),
                (),
            )
            .await
            .map_err(db_err)?;
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
            .map_err(db_err)?;
        }
    }
    for (name, sql) in INDEXES {
        if !indexes.iter().any(|existing| existing == name) {
            conn.execute(*sql, ()).await.map_err(db_err)?;
        }
    }
    Ok(())
}

/// 第一列的所有文本值；游标在函数内收干净。
pub(crate) async fn column_one(conn: &Connection, sql: &str) -> Result<Vec<String>, LineError> {
    let mut rows = conn.query(sql, ()).await.map_err(db_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(db_err)? {
        if let Value::Text(text) = row.get_value(0).map_err(db_err)? {
            out.push(text);
        }
    }
    Ok(out)
}

pub(crate) async fn query_one_text(
    conn: &Connection,
    sql: &str,
) -> Result<Option<String>, LineError> {
    Ok(column_one(conn, sql).await?.into_iter().next())
}

async fn pragma_text(conn: &Connection, name: &str) -> Result<String, LineError> {
    let mut rows = conn
        .query(&format!("PRAGMA {name}"), ())
        .await
        .map_err(db_err)?;
    let mut out = String::new();
    while let Some(row) = rows.next().await.map_err(db_err)? {
        out = match row.get_value(0).map_err(db_err)? {
            Value::Text(text) => text,
            Value::Integer(n) => n.to_string(),
            other => format!("{other:?}"),
        };
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh(dir: &Path) -> (Database, Connection) {
        open(&durable_db_path(dir)).await.expect("打开新库")
    }

    /// 与 `db.rs` 的 DDL 字节对等测试同一个意思：常量就是实际建出的 schema。
    #[tokio::test]
    async fn a_fresh_database_has_exactly_the_ddl_constants() {
        let dir = tempfile::tempdir().unwrap();
        let (_db, conn) = fresh(dir.path()).await;
        for table in TABLES {
            let sql = query_one_text(
                &conn,
                &format!(
                    "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = '{}'",
                    table.name
                ),
            )
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("缺表 {}", table.name));
            // turso 存的 SQL 会把括号内外的空白与引号规整一下，比对时一并抹平。
            let normalize = |s: &str| {
                s.replace('"', "")
                    .replace('(', " ( ")
                    .replace(')', " ) ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            assert_eq!(
                normalize(&sql),
                normalize(table.ddl),
                "{} 的 DDL",
                table.name
            );

            let columns = column_one(
                &conn,
                &format!("SELECT name FROM pragma_table_info('{}')", table.name),
            )
            .await
            .unwrap();
            let expected: Vec<_> = table.columns.iter().map(|c| c.name.to_string()).collect();
            assert_eq!(columns, expected, "{} 的列", table.name);
        }
        let indexes = column_one(
            &conn,
            "SELECT name FROM sqlite_schema WHERE type = 'index' AND name NOT LIKE 'sqlite_%'",
        )
        .await
        .unwrap();
        for (name, _) in INDEXES {
            assert!(indexes.iter().any(|i| i == name), "缺索引 {name}");
        }
        assert_eq!(indexes.len(), INDEXES.len(), "多出了未登记的索引");
    }

    #[tokio::test]
    async fn the_database_is_in_wal_mode_with_full_sync_and_data_sync_retry() {
        let dir = tempfile::tempdir().unwrap();
        let (_db, conn) = fresh(dir.path()).await;
        assert_eq!(pragma_text(&conn, "journal_mode").await.unwrap(), "wal");
        // 2 = FULL
        assert_eq!(pragma_text(&conn, "synchronous").await.unwrap(), "2");
        assert_eq!(pragma_text(&conn, "data_sync_retry").await.unwrap(), "1");
        #[cfg(target_vendor = "apple")]
        assert_eq!(pragma_text(&conn, "fullfsync").await.unwrap(), "1");
    }

    /// 真盘微基准：每次改变正文，避免 WAL 跳过相同页面造成虚假的吞吐。
    #[tokio::test]
    #[ignore = "manual disk sync benchmark; run with --ignored --nocapture"]
    async fn measure_sync_modes() {
        use std::time::Instant;
        for (label, sync, fullfsync) in [
            ("FULL", "FULL", "OFF"),
            ("FULL+fullfsync", "FULL", "ON"),
            ("OFF", "OFF", "ON"),
        ] {
            let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
            let path = durable_db_path(dir.path());
            let (_db, conn) = open(&path).await.unwrap();
            conn.execute(&format!("PRAGMA fullfsync = {fullfsync}"), ())
                .await
                .unwrap();
            conn.execute(&format!("PRAGMA synchronous = {sync}"), ())
                .await
                .unwrap();
            let mut timings = Vec::new();
            let mut wal_peak = 0;
            let mut wal_resets = 0;
            let mut previous_wal = 0;
            let start = Instant::now();
            for n in 0..1000 {
                let value = format!("{n}:{}", "x".repeat(2048));
                let commit_start = Instant::now();
                conn.execute("BEGIN", ()).await.unwrap();
                conn.execute(
                    "INSERT OR REPLACE INTO durable_meta (name, value) VALUES ('benchmark', ?1)",
                    [value],
                )
                .await
                .unwrap();
                conn.execute("COMMIT", ()).await.unwrap();
                timings.push(commit_start.elapsed().as_secs_f64() * 1000.0);
                let wal = std::fs::metadata(format!("{}-wal", path.display()))
                    .map(|m| m.len())
                    .unwrap_or(0);
                wal_peak = wal_peak.max(wal);
                wal_resets += usize::from(wal < previous_wal);
                previous_wal = wal;
            }
            let elapsed = start.elapsed();
            timings.sort_by(f64::total_cmp);
            println!(
                "{label}: 1000 commits, {:.0}/s, p50 {:.3} ms, p95 {:.3} ms, p99 {:.3} ms, WAL peak {wal_peak} bytes, file shrinks {wal_resets}",
                1000.0 / elapsed.as_secs_f64(),
                timings[500],
                timings[950],
                timings[990]
            );
        }
    }

    #[tokio::test]
    async fn a_database_with_a_newer_format_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (_db, conn) = fresh(dir.path()).await;
            conn.execute(
                "UPDATE durable_meta SET value = ?1 WHERE name = 'format_version'",
                [Value::Text((FORMAT_VERSION + 1).to_string())],
            )
            .await
            .unwrap();
        }
        let error = open(&durable_db_path(dir.path()))
            .await
            .err()
            .expect("应拒绝");
        assert!(
            matches!(error, LineError::UnsupportedFormat { found, supported }
                if found == FORMAT_VERSION + 1 && supported == FORMAT_VERSION),
            "{error:?}"
        );
    }

    /// 旧形状：少一列、少一个索引。重开后补齐，且已有行保留。
    #[tokio::test]
    async fn reopening_an_older_shape_adds_the_missing_column_and_index() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (_db, conn) = fresh(dir.path()).await;
            conn.execute("INSERT INTO sessions (id, lifecycle, root_conversation, record) VALUES ('s1', 'active', 1, '{}')", ())
                .await
                .unwrap();
            conn.execute("DROP INDEX sessions_by_lifecycle", ())
                .await
                .unwrap();
            conn.execute("ALTER TABLE sessions DROP COLUMN current_branch", ())
                .await
                .unwrap();
        }
        let (_db, conn) = fresh(dir.path()).await;
        let columns = column_one(&conn, "SELECT name FROM pragma_table_info('sessions')")
            .await
            .unwrap();
        assert!(columns.iter().any(|c| c == "current_branch"), "{columns:?}");
        let indexes = column_one(&conn, "SELECT name FROM sqlite_schema WHERE type = 'index'")
            .await
            .unwrap();
        assert!(indexes.iter().any(|i| i == "sessions_by_lifecycle"));
        let kept = column_one(&conn, "SELECT id FROM sessions").await.unwrap();
        assert_eq!(kept, vec!["s1".to_string()]);
    }
}
