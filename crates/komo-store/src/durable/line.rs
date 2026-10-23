//! 提交线（§3.1、pi-durable spec §4 / §10.1 / §11.2）。
//!
//! 一个 tokio 任务持有**唯一的写连接**，按到达顺序一次处理一个提交：`BEGIN` → 逐条写 →
//! 更新 `last_seq` → `COMMIT`，成功才回复调用方。一次提交就是一个 SQL 事务、一次落盘。
//!
//! 三条纪律：
//!
//! - **存储错误让整条线失败**（spec §10.1）：任何 SQL / 提交错误都先 `ROLLBACK`，再把线置为
//!   失败，此后所有 `commit` 返回**同一个**错误。提交了没有是未知的，重开是唯一干净的判断方式。
//!   提交前的检查（空提交、Progress 混入别的写）是调用方的错，不让线失败。
//! - **提交里没有外部 await**：线只收已准备好的 [`Write`]（或一个同步闭包，见
//!   [`Line::commit_with`]），形状上就做不到在事务里等模型、子进程、文件同步或用户。
//! - **seq 严格递增**：每次成功提交 +1，随事务写进 `durable_meta.last_seq`，重开后接着走。
//!   失败的提交不占号（线随即失败，号也无处可用）。
//!
//! 落盘档位（§3.6）：`Structural` 提交前把 `synchronous` 设为 FULL，`Progress` 设为 OFF；
//! 只在档位变化时才发 pragma。

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use turso::{Connection, Database, Value};

use super::ids::{self, IdMint};
use super::schema::{self, META_LAST_SEQ};
use super::write::{self, CommitKind, Write};

/// 提交序号：从 1 起，严格递增，重开后接续。
pub type Seq = u64;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum LineError {
    /// 提交前被拒（调用方的错）。线不受影响。
    #[error("提交被拒：{0}")]
    Rejected(String),
    /// 存储错误。线已失败，之后所有提交都返回这同一条。
    #[error("存储错误，提交线已失败：{0}")]
    Failed(String),
    /// 线已关闭（任务已退出）。
    #[error("提交线已关闭")]
    Closed,
    /// 打开库文件失败。
    #[error("打开新库失败：{0}")]
    Open(String),
    /// 库是更新的二进制写的（§8：旧二进制要能识别并拒绝）。
    #[error("库的格式版本是 {found}，本二进制只认识到 {supported}")]
    UnsupportedFormat { found: i64, supported: i64 },
}

type Build = Box<dyn for<'a> FnOnce(&mut IdMint<'a>) -> Vec<Write> + Send>;

struct Request {
    kind: CommitKind,
    build: Build,
    reply: oneshot::Sender<Result<Seq, LineError>>,
}

struct Shared {
    database: Database,
    path: PathBuf,
    /// 已分配的最大 ID。
    ids: AtomicI64,
    /// 线的失败原因；一旦置上就不再改。
    failed: Mutex<Option<LineError>>,
}

impl Shared {
    fn failure(&self) -> Option<LineError> {
        self.failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// 线的句柄；克隆很便宜，所有克隆共用同一条线。最后一个句柄丢掉后线任务退出、写连接关闭。
#[derive(Clone)]
pub struct Line {
    tx: mpsc::Sender<Request>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Line")
            .field("path", &self.shared.path)
            .finish()
    }
}

impl Line {
    /// 打开 `<data_dir>/durable.db`（不存在就新建）并启动线任务。须在 tokio 运行时内调用。
    pub async fn open(data_dir: &Path) -> Result<Line, LineError> {
        Line::open_file(&schema::durable_db_path(data_dir)).await
    }

    pub async fn open_file(path: &Path) -> Result<Line, LineError> {
        let (database, conn) = schema::open(path).await?;
        let max_id = max_id(&conn).await?;
        let last_seq = schema::query_one_text(
            &conn,
            &format!("SELECT value FROM durable_meta WHERE name = '{META_LAST_SEQ}'"),
        )
        .await?
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| LineError::Open("durable_meta 缺少 last_seq".into()))?;

        let shared = Arc::new(Shared {
            database,
            path: path.to_path_buf(),
            ids: AtomicI64::new(max_id),
            failed: Mutex::new(None),
        });
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(run(
            rx,
            Writer {
                conn,
                seq: last_seq,
                sync: CommitKind::Structural, // `schema::open` 刚设成 FULL
            },
            shared.clone(),
        ));
        Ok(Line { tx, shared })
    }

    /// 提交一批已准备好的写。返回即已按 `kind` 的档位落盘。
    pub async fn commit(&self, kind: CommitKind, writes: Vec<Write>) -> Result<Seq, LineError> {
        self.send(kind, Box::new(move |_| writes)).await
    }

    /// 与 [`Line::commit`] 相同，但写在**线内**由同步闭包产出，闭包里可以取号
    /// （[`IdMint`]）：取号序就是提交序。闭包不能 `await`。
    pub async fn commit_with<F>(&self, kind: CommitKind, build: F) -> Result<Seq, LineError>
    where
        F: for<'a> FnOnce(&mut IdMint<'a>) -> Vec<Write> + Send + 'static,
    {
        self.send(kind, Box::new(build)).await
    }

    /// 先取号再组写入。并发调用方之间取号序与提交序可以不同；要严格次序用 [`Line::commit_with`]。
    pub fn mint_id(&self) -> i64 {
        ids::next(&self.shared.ids)
    }

    /// 一条只读连接（同一 `turso::Database`）。WAL 下读不阻塞写，也不被写阻塞；
    /// 读到的是某个已提交状态。
    pub fn read_connection(&self) -> Result<Connection, LineError> {
        self.shared
            .database
            .connect()
            .map_err(|e| LineError::Open(e.to_string()))
    }

    /// 线的失败原因；没失败是 `None`。
    pub fn failure(&self) -> Option<LineError> {
        self.shared.failure()
    }

    pub fn path(&self) -> &Path {
        &self.shared.path
    }

    async fn send(&self, kind: CommitKind, build: Build) -> Result<Seq, LineError> {
        if let Some(error) = self.shared.failure() {
            return Err(error);
        }
        let (reply, wait) = oneshot::channel();
        self.tx
            .send(Request { kind, build, reply })
            .await
            .map_err(|_| LineError::Closed)?;
        wait.await.map_err(|_| LineError::Closed)?
    }
}

struct Writer {
    conn: Connection,
    seq: Seq,
    /// 连接上当前生效的同步档位（Structural = FULL，Progress = OFF）。
    sync: CommitKind,
}

async fn run(mut rx: mpsc::Receiver<Request>, mut writer: Writer, shared: Arc<Shared>) {
    while let Some(request) = rx.recv().await {
        let result = match shared.failure() {
            Some(error) => Err(error),
            None => writer.process(&shared, request.kind, request.build).await,
        };
        // 调用方已经不等了也无妨：提交已经发生或已经失败，结果只是没人收。
        let _ = request.reply.send(result);
    }
}

impl Writer {
    async fn process(
        &mut self,
        shared: &Shared,
        kind: CommitKind,
        build: Build,
    ) -> Result<Seq, LineError> {
        let writes = build(&mut IdMint::new(&shared.ids));
        write::validate(kind, &writes).map_err(LineError::Rejected)?;

        match self.commit(kind, &writes).await {
            Ok(seq) => {
                self.seq = seq;
                Ok(seq)
            }
            Err(message) => {
                // 先回滚；回滚本身失败也无所谓——线马上就是失败态，重开才是判断。
                let _ = self.conn.execute("ROLLBACK", ()).await;
                let error = LineError::Failed(message);
                tracing::error!(%error, "提交线失败");
                *shared.failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
                Err(error)
            }
        }
    }

    async fn commit(&mut self, kind: CommitKind, writes: &[Write]) -> Result<Seq, String> {
        if self.sync != kind {
            let pragma = match kind {
                CommitKind::Structural => "PRAGMA synchronous = FULL",
                CommitKind::Progress => "PRAGMA synchronous = OFF",
            };
            self.conn
                .execute(pragma, ())
                .await
                .map_err(|e| format!("{pragma}：{e}"))?;
            self.sync = kind;
        }
        let seq = self.seq + 1;
        self.conn
            .execute("BEGIN", ())
            .await
            .map_err(|e| format!("BEGIN：{e}"))?;
        for write in writes {
            write.apply(&self.conn, seq as i64).await?;
        }
        self.conn
            .execute(
                "UPDATE durable_meta SET value = ?1 WHERE name = 'last_seq'",
                [Value::Text(seq.to_string())],
            )
            .await
            .map_err(|e| format!("last_seq：{e}"))?;
        self.conn
            .execute("COMMIT", ())
            .await
            .map_err(|e| format!("COMMIT：{e}"))?;
        Ok(seq)
    }
}

/// 五张实体表里最大的 ID（没有就是 0）。
async fn max_id(conn: &Connection) -> Result<i64, LineError> {
    let sql = "SELECT COALESCE(MAX(m), 0) FROM (\
        SELECT MAX(id) AS m FROM conversations UNION ALL \
        SELECT MAX(id) FROM entries UNION ALL \
        SELECT MAX(id) FROM tasks UNION ALL \
        SELECT MAX(id) FROM submissions UNION ALL \
        SELECT MAX(id) FROM documents)";
    let mut rows = conn
        .query(sql, ())
        .await
        .map_err(|e| LineError::Open(e.to_string()))?;
    let mut max = 0;
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| LineError::Open(e.to_string()))?
    {
        if let Value::Integer(n) = row
            .get_value(0)
            .map_err(|e| LineError::Open(e.to_string()))?
        {
            max = n;
        }
    }
    Ok(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, conversation_id: i64) -> Write {
        Write::InsertEntry {
            id,
            conversation_id,
            head: Some(id),
            record: format!("{{\"n\":{id}}}"),
        }
    }

    fn doc(id: i64, value: &str) -> Write {
        Write::CreateDocument {
            id,
            kind: "komo.live".into(),
            scope_kind: "conversation".into(),
            owner_id: 1,
            key_value: String::new(),
            value: value.into(),
        }
    }

    async fn count(line: &Line, sql: &str) -> i64 {
        let conn = line.read_connection().unwrap();
        let mut rows = conn.query(sql, ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        match row.get_value(0).unwrap() {
            Value::Integer(n) => n,
            other => panic!("不是整数：{other:?}"),
        }
    }

    #[tokio::test]
    async fn commit_seq_is_strictly_increasing_and_continues_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let line = Line::open(dir.path()).await.unwrap();
        let a = line
            .commit(CommitKind::Structural, vec![entry(1, 1)])
            .await
            .unwrap();
        let b = line
            .commit(CommitKind::Structural, vec![entry(2, 1)])
            .await
            .unwrap();
        let c = line.commit(CommitKind::Progress, vec![]).await;
        assert_eq!((a, b), (1, 2));
        assert!(matches!(c, Err(LineError::Rejected(_))));
        drop(line);
        tokio::task::yield_now().await;

        let line = Line::open(dir.path()).await.unwrap();
        let next = line
            .commit(CommitKind::Structural, vec![entry(3, 1)])
            .await
            .unwrap();
        assert_eq!(next, 3);
    }

    #[tokio::test]
    async fn ids_continue_from_the_largest_stored_id_across_all_entity_tables() {
        let dir = tempfile::tempdir().unwrap();
        let line = Line::open(dir.path()).await.unwrap();
        assert_eq!(line.mint_id(), 1);
        line.commit(CommitKind::Structural, vec![entry(5, 1), doc(9, "{}")])
            .await
            .unwrap();
        line.commit(
            CommitKind::Structural,
            vec![Write::InsertSubmission {
                id: 12,
                conversation_id: 1,
                request_id: Some("r1".into()),
                status: "queued".into(),
                record: "{}".into(),
            }],
        )
        .await
        .unwrap();
        drop(line);
        tokio::task::yield_now().await;

        let line = Line::open(dir.path()).await.unwrap();
        assert_eq!(line.mint_id(), 13);
    }

    #[tokio::test]
    async fn commit_with_mints_ids_in_commit_order_even_when_callers_race() {
        let dir = tempfile::tempdir().unwrap();
        let line = Line::open(dir.path()).await.unwrap();
        let mut joins = Vec::new();
        for _ in 0..8 {
            let line = line.clone();
            joins.push(tokio::spawn(async move {
                line.commit_with(CommitKind::Structural, |mint| vec![entry(mint.mint(), 1)])
                    .await
                    .unwrap()
            }));
        }
        let mut seqs = Vec::new();
        for join in joins {
            seqs.push(join.await.unwrap());
        }
        // 提交序（seq）与 ID 序一致：第 k 次提交的 entry 恰好是 ID k。
        let conn = line.read_connection().unwrap();
        let mut rows = conn
            .query("SELECT id, commit_seq FROM entries ORDER BY id", ())
            .await
            .unwrap();
        let mut n = 0;
        while let Some(row) = rows.next().await.unwrap() {
            n += 1;
            let (Value::Integer(id), Value::Integer(seq)) =
                (row.get_value(0).unwrap(), row.get_value(1).unwrap())
            else {
                panic!("列类型不对")
            };
            assert_eq!(id, n);
            assert_eq!(seq, n);
        }
        assert_eq!(n, 8);
    }

    #[tokio::test]
    async fn a_progress_commit_with_any_other_write_is_rejected_without_failing_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let line = Line::open(dir.path()).await.unwrap();
        line.commit(CommitKind::Structural, vec![doc(1, "{}")])
            .await
            .unwrap();
        let error = line
            .commit(
                CommitKind::Progress,
                vec![
                    Write::ReplaceDocument {
                        id: 1,
                        value: "{\"p\":1}".into(),
                    },
                    entry(2, 1),
                ],
            )
            .await
            .unwrap_err();
        assert!(matches!(error, LineError::Rejected(_)), "{error:?}");
        assert_eq!(line.failure(), None);
        assert_eq!(count(&line, "SELECT COUNT(*) FROM entries").await, 0);
        // 线照常工作；被拒的提交没有占号。
        let seq = line
            .commit(
                CommitKind::Progress,
                vec![Write::ReplaceDocument {
                    id: 1,
                    value: "{\"p\":2}".into(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(seq, 2);
    }

    #[tokio::test]
    async fn a_failing_transaction_rolls_back_and_fails_the_line_with_the_same_error() {
        let dir = tempfile::tempdir().unwrap();
        let line = Line::open(dir.path()).await.unwrap();
        line.commit(CommitKind::Structural, vec![entry(1, 1)])
            .await
            .unwrap();

        // 第二条写主键冲突：第一条（entry 2）必须随事务回滚。
        let first = line
            .commit(CommitKind::Structural, vec![entry(2, 1), entry(1, 1)])
            .await
            .unwrap_err();
        assert!(matches!(first, LineError::Failed(_)), "{first:?}");
        assert_eq!(count(&line, "SELECT COUNT(*) FROM entries").await, 1);

        // 此后所有提交——哪怕本身没问题——都返回同一个错误。
        let again = line
            .commit(CommitKind::Structural, vec![entry(3, 1)])
            .await
            .unwrap_err();
        let progress = line
            .commit(
                CommitKind::Progress,
                vec![Write::ReplaceDocument {
                    id: 1,
                    value: "{}".into(),
                }],
            )
            .await
            .unwrap_err();
        assert_eq!(again, first);
        assert_eq!(progress, first);
        assert_eq!(line.failure(), Some(first));
        assert_eq!(count(&line, "SELECT COUNT(*) FROM entries").await, 1);

        // 重开：失败前的提交都在，seq 没被失败的提交占用。
        drop(line);
        tokio::task::yield_now().await;
        let line = Line::open(dir.path()).await.unwrap();
        assert_eq!(count(&line, "SELECT COUNT(*) FROM entries").await, 1);
        assert_eq!(
            line.commit(CommitKind::Structural, vec![entry(2, 1)])
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn updating_a_missing_row_fails_the_commit_instead_of_silently_doing_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let line = Line::open(dir.path()).await.unwrap();
        let error = line
            .commit(
                CommitKind::Structural,
                vec![Write::UpdateSubmission {
                    id: 7,
                    status: "done".into(),
                    record: "{}".into(),
                }],
            )
            .await
            .unwrap_err();
        assert!(matches!(error, LineError::Failed(_)), "{error:?}");
    }

    #[tokio::test]
    async fn every_write_kind_lands_in_its_table() {
        let dir = tempfile::tempdir().unwrap();
        let line = Line::open(dir.path()).await.unwrap();
        line.commit(
            CommitKind::Structural,
            vec![
                Write::InsertConversation {
                    id: 1,
                    owner_conversation: None,
                    owner_task: None,
                    parent_conversation: None,
                    parent_at: None,
                    session_id: Some("s1".into()),
                    record: "{}".into(),
                },
                Write::InsertConversation {
                    id: 2,
                    owner_conversation: None,
                    owner_task: None,
                    parent_conversation: Some(1),
                    parent_at: Some(3),
                    session_id: Some("s1".into()),
                    record: "{}".into(),
                },
                entry(3, 1),
                Write::UpsertTask {
                    id: 4,
                    conversation_id: 1,
                    kind: "generate".into(),
                    status: "pending".into(),
                    abort_requested: false,
                    background: false,
                    record: "{\"v\":1}".into(),
                },
                Write::InsertSubmission {
                    id: 5,
                    conversation_id: 1,
                    request_id: Some("req".into()),
                    status: "queued".into(),
                    record: "{}".into(),
                },
                doc(6, "{\"p\":0}"),
                Write::UpsertSession {
                    id: "01900000-0000-7000-8000-000000000000".into(),
                    lifecycle: "active".into(),
                    current_branch: Some(1),
                    root_conversation: 1,
                    record: "{}".into(),
                },
            ],
        )
        .await
        .unwrap();
        line.commit(
            CommitKind::Structural,
            vec![
                Write::UpsertTask {
                    id: 4,
                    conversation_id: 1,
                    kind: "generate".into(),
                    status: "terminal".into(),
                    abort_requested: true,
                    background: false,
                    record: "{\"v\":2}".into(),
                },
                Write::UpdateSubmission {
                    id: 5,
                    status: "done".into(),
                    record: "{\"a\":1}".into(),
                },
                Write::UpsertSession {
                    id: "01900000-0000-7000-8000-000000000000".into(),
                    lifecycle: "archived".into(),
                    current_branch: Some(2),
                    root_conversation: 1,
                    record: "{}".into(),
                },
            ],
        )
        .await
        .unwrap();
        line.commit(
            CommitKind::Progress,
            vec![Write::ReplaceDocument {
                id: 6,
                value: "{\"p\":1}".into(),
            }],
        )
        .await
        .unwrap();
        line.commit(
            CommitKind::Structural,
            vec![Write::RetireDocument { id: 6 }],
        )
        .await
        .unwrap();

        assert_eq!(
            count(
                &line,
                "SELECT COUNT(*) FROM conversations WHERE parent_conversation = 1"
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &line,
                "SELECT COUNT(*) FROM tasks WHERE status = 'terminal' AND abort_requested = 1"
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &line,
                "SELECT COUNT(*) FROM submissions WHERE status = 'done'"
            )
            .await,
            1
        );
        assert_eq!(
            count(&line, "SELECT version FROM documents WHERE id = 6").await,
            2
        );
        assert_eq!(
            count(&line, "SELECT retired_seq FROM documents WHERE id = 6").await,
            4
        );
        assert_eq!(
            count(
                &line,
                "SELECT COUNT(*) FROM sessions WHERE lifecycle = 'archived' AND current_branch = 2"
            )
            .await,
            1
        );
        // 退役后不能再替换。
        let error = line
            .commit(
                CommitKind::Progress,
                vec![Write::ReplaceDocument {
                    id: 6,
                    value: "{}".into(),
                }],
            )
            .await
            .unwrap_err();
        assert!(matches!(error, LineError::Failed(_)));
    }

    #[tokio::test]
    async fn readers_are_not_blocked_by_and_do_not_block_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let line = Line::open(dir.path()).await.unwrap();
        let reader = line.read_connection().unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let task = tokio::spawn(async move {
            let (mut ok, mut err) = (0u32, 0u32);
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                let result: Result<(), turso::Error> = async {
                    let mut rows = reader
                        .query("SELECT id FROM entries ORDER BY id DESC LIMIT 50", ())
                        .await?;
                    while rows.next().await?.is_some() {}
                    Ok(())
                }
                .await;
                if result.is_ok() {
                    ok += 1
                } else {
                    err += 1
                }
                tokio::task::yield_now().await;
            }
            (ok, err)
        });
        for i in 1..=30 {
            line.commit(CommitKind::Structural, vec![entry(i, 1)])
                .await
                .unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let (ok, err) = task.await.unwrap();
        assert_eq!(
            err, 0,
            "WAL 下并发读不该报错（D0：MVCC 下是 database is locked）"
        );
        assert!(ok > 0);
    }

    /// 真盘上的提交延迟，对照 D0（Structural 约 13 ms、Progress 约 0.13 ms）。
    /// `cargo test -p komo-store durable::line::tests::bench -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "基准：要真盘，跑在 $HOME/.cache/komo-d2-bench"]
    async fn bench_commit_latency_on_a_real_disk() {
        let home = std::env::var("HOME").expect("HOME");
        let dir = PathBuf::from(home).join(".cache/komo-d2-bench");
        let _ = std::fs::remove_dir_all(&dir);
        let line = Line::open(&dir).await.unwrap();
        line.commit(CommitKind::Structural, vec![doc(1, "{}")])
            .await
            .unwrap();
        let payload =
            |n: usize, salt: usize| format!("{{\"s\":{salt},\"x\":\"{}\"}}", "a".repeat(n));

        let mut structural = Vec::new();
        let mut progress = Vec::new();
        let wall = std::time::Instant::now();
        // 与 D0 同样的 9:1 交错；每次写的值都不同（同值 UPDATE 会被 WAL 跳过）。
        for i in 0..1000usize {
            let t = std::time::Instant::now();
            if i % 10 == 0 {
                line.commit(CommitKind::Structural, vec![entry(10 + i as i64, 1)])
                    .await
                    .unwrap();
                structural.push(t.elapsed());
            } else {
                line.commit(
                    CommitKind::Progress,
                    vec![Write::ReplaceDocument {
                        id: 1,
                        value: payload(2048, i),
                    }],
                )
                .await
                .unwrap();
                progress.push(t.elapsed());
            }
        }
        let wall = wall.elapsed();
        let pure: Vec<_> = {
            let mut v = Vec::new();
            for i in 0..300usize {
                let t = std::time::Instant::now();
                line.commit(CommitKind::Structural, vec![entry(5000 + i as i64, 1)])
                    .await
                    .unwrap();
                v.push(t.elapsed());
            }
            v
        };
        let pct = |v: &mut Vec<std::time::Duration>, p: f64| {
            v.sort();
            v[((v.len() as f64 - 1.0) * p) as usize].as_secs_f64() * 1000.0
        };
        let (mut s, mut p, mut u) = (structural, progress, pure);
        println!(
            "interleaved 9:1 wall={:.2}s | structural n={} p50={:.2}ms p99={:.2}ms | progress n={} p50={:.3}ms p99={:.3}ms",
            wall.as_secs_f64(),
            s.len(),
            pct(&mut s, 0.5),
            pct(&mut s, 0.99),
            p.len(),
            pct(&mut p, 0.5),
            pct(&mut p, 0.99)
        );
        println!(
            "pure structural n={} p50={:.2}ms p99={:.2}ms",
            u.len(),
            pct(&mut u, 0.5),
            pct(&mut u, 0.99)
        );
        drop(line);
        tokio::task::yield_now().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
