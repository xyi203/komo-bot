//! `sessions/<id>/events.jsonl`：追加写入、范围读、打开时的尾部校验（§8.3）。
//!
//! 一行一条事件，UTF-8，**每条完整记录以换行结束**——这就是识别写到一半的末尾的办法。
//! 写入器分配 `seq`（Session 内按追加顺序严格递增），写完整记录后清空用户态缓冲再
//! `sync_all`；同步失败就停在这一步，不能执行后续副作用或向客户端确认持久完成。
//!
//! 尾部恢复的三条规则，它们不是同一条（§8.3）：
//!
//! - **只有末尾不完整、且不被已提交引用覆盖的行**，才能先隔离原始字节再截断。
//! - **文件中间损坏、已提交范围缺失或哈希不匹配**：停止，报
//!   [`LedgerError::Corrupt`]，不跳过坏行也不退回旧检查点。
//! - **未知 `type`** 保留原始内容与序号（kernel 的 `EventPayload::Unknown` 负责）；
//!   **未知 `v`** 必须暂停恢复（kernel 的 `EventDecodeError::UnsupportedVersion`）。

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use komo_kernel::events::{EVENT_FORMAT_VERSION, Event, EventPayload};
use komo_kernel::traits::{LedgerError, StoreError};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{EventId, RunId, Seq, SessionId};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// 一个 Session 目录，以及在它内部解析引用的规则。
///
/// **路径一律相对于该 Session 目录解析**，不依赖进程工作目录；解析时禁止越出该目录
/// （§8.3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPaths {
    root: PathBuf,
}

impl SessionPaths {
    /// `<sessions_root>/<session_id>`。
    pub fn new(sessions_root: impl AsRef<Path>, session: &SessionId) -> Self {
        Self {
            root: sessions_root.as_ref().join(session.as_str()),
        }
    }

    /// 直接指向一个已知的 Session 目录。
    pub fn at(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn events(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }

    /// 尾部隔离文件。半行原样搬到这里，**可以用于诊断，不能当作完成结果**（§8.3）。
    pub fn quarantine(&self) -> PathBuf {
        self.root.join("events.jsonl.quarantine")
    }

    pub fn payloads(&self) -> PathBuf {
        self.root.join("payloads")
    }

    pub fn tool_output(&self) -> PathBuf {
        self.root.join("tool-output")
    }

    pub fn artifacts(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    /// 把一个 Session 内的相对引用解析成真实路径。
    ///
    /// 绝对路径、`..` 与符号意义上的越界一律拒绝——引用是受控路径，不是任意文件名。
    pub fn resolve(&self, relative: &str) -> Result<PathBuf, StoreError> {
        let candidate = Path::new(relative);
        if candidate.is_absolute() {
            return Err(StoreError::Corrupt(format!(
                "引用不能是绝对路径：{relative}"
            )));
        }
        let mut out = self.root.clone();
        for component in candidate.components() {
            match component {
                Component::Normal(part) => out.push(part),
                Component::CurDir => {}
                _ => {
                    return Err(StoreError::Corrupt(format!(
                        "引用越出了 Session 目录：{relative}"
                    )));
                }
            }
        }
        Ok(out)
    }

    /// 建出 Session 目录本身，并同步父目录。
    pub async fn ensure(&self) -> Result<(), StoreError> {
        create_dir_all_synced(&self.root).await
    }
}

/// 一条等待写入的事件——`seq` 由写入器分配，所以这里没有它。
#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub event_id: EventId,
    pub run: Option<RunId>,
    pub ts: OffsetDateTime,
    pub payload: EventPayload,
}

impl PendingEvent {
    pub fn new(
        event_id: EventId,
        run: Option<RunId>,
        ts: OffsetDateTime,
        payload: EventPayload,
    ) -> Self {
        Self {
            event_id,
            run,
            ts,
            payload,
        }
    }
}

/// 一条已经落盘并同步的事件，连同它在文件里的坐标——`session_log_index` 存的就是它。
#[derive(Debug, Clone, PartialEq)]
pub struct AppendedEvent {
    pub event: Event,
    pub byte_offset: u64,
    /// 记录长度，**含结尾换行**。
    pub byte_len: u64,
    /// 这一行（不含换行）的 SHA-256。
    pub digest: String,
}

impl AppendedEvent {
    pub fn seq(&self) -> Seq {
        self.event.seq
    }
}

/// 打开日志时对尾部的期待。
///
/// 两样东西合起来才说得出"这个半行能不能扔"：数据库已经应用到哪个 seq，以及已提交那
/// 些行的摘要。
#[derive(Debug, Clone, Default)]
pub struct TailExpectation {
    /// `sessions.applied_seq`：数据库已经引用到的最后一个连续事件。
    pub applied_seq: Seq,
    /// 已提交记录的摘要（`session_log_index.digest`），按 seq。**校验不符就是损坏**。
    pub digests: BTreeMap<Seq, String>,
}

/// 打开日志时对尾部做了什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailRepair {
    /// 文件末尾是完整记录，什么都没动。
    Clean,
    /// 末尾半行已隔离到 `events.jsonl.quarantine` 并截断。
    Quarantined { bytes: u64, offset: u64 },
}

/// 一个 Session 的 JSONL 写入器兼读取器。
///
/// **每个 Session 一个实例，串行**：模型、工具、审批审计与 Memory 来源标记都经过它，
/// 不能各自追加导致行内容交错（§8.3）。串行由 [`crate::coordinator::Coordinator`] 的
/// mutex 保证。
#[derive(Debug)]
pub struct SessionLog {
    paths: SessionPaths,
    session: SessionId,
    file: tokio::fs::File,
    /// 已写入的字节数 = 下一条记录的起始偏移。
    length: u64,
    next_seq: u64,
    /// 每条记录的坐标，按追加顺序。范围读靠它 seek，不必反复解析整个会话（§8.3）。
    index: Vec<LineLoc>,
    repair: TailRepair,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineLoc {
    seq: u64,
    offset: u64,
    len: u64,
}

impl SessionLog {
    /// 打开（必要时创建）一个 Session 的日志，并校验尾部。
    pub async fn open(
        paths: SessionPaths,
        session: SessionId,
        expectation: &TailExpectation,
    ) -> Result<SessionLog, LedgerError> {
        paths.ensure().await.map_err(persist)?;
        let path = paths.events();

        let existing = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(persist(StoreError::Io(format!(
                    "读 {} 失败：{e}",
                    path.display()
                ))));
            }
        };

        let scan = scan(&existing, &session, expectation)?;

        let repair = if scan.tail_bytes.is_empty() {
            TailRepair::Clean
        } else {
            // 末尾半行：先隔离保留原始字节，再截断（§8.3）。
            quarantine(&paths, scan.complete_len, &scan.tail_bytes).await?;
            tokio::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .await
                .map_err(|e| persist(StoreError::Io(format!("打开 {} 失败：{e}", path.display()))))?
                .set_len(scan.complete_len)
                .await
                .map_err(|e| {
                    persist(StoreError::Io(format!("截断 {} 失败：{e}", path.display())))
                })?;
            tracing::warn!(
                session = %session,
                bytes = scan.tail_bytes.len(),
                offset = scan.complete_len,
                "隔离了 JSONL 的末尾半行"
            );
            TailRepair::Quarantined {
                bytes: scan.tail_bytes.len() as u64,
                offset: scan.complete_len,
            }
        };

        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .await
            .map_err(|e| persist(StoreError::Io(format!("打开 {} 失败：{e}", path.display()))))?;
        // 新建文件时目录项本身也要落盘。
        sync_dir(paths.root()).await.map_err(persist)?;

        let next_seq = scan.index.last().map(|l| l.seq + 1).unwrap_or(1);
        Ok(SessionLog {
            paths,
            session,
            file,
            length: scan.complete_len,
            next_seq,
            index: scan.index,
            repair,
        })
    }

    pub fn paths(&self) -> &SessionPaths {
        &self.paths
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// 打开时对尾部做了什么。
    pub fn repair(&self) -> &TailRepair {
        &self.repair
    }

    /// 已经写到哪个 seq。空日志是 [`Seq::ZERO`]。
    pub fn last_seq(&self) -> Seq {
        Seq(self.next_seq.saturating_sub(1))
    }

    pub fn byte_len(&self) -> u64 {
        self.length
    }

    /// 追加一条事件：分配 seq → 写整行 → flush → `sync_all`。
    ///
    /// 返回之后调用方才能提交数据库事务（§8.5 的箭头顺序）。同步失败返回
    /// [`LedgerError::Persist`]，**此时不能执行后续副作用**。
    pub async fn append(&mut self, pending: PendingEvent) -> Result<AppendedEvent, LedgerError> {
        let event = Event {
            v: EVENT_FORMAT_VERSION,
            seq: Seq(self.next_seq),
            event_id: pending.event_id,
            session: self.session.clone(),
            run: pending.run,
            ts: pending.ts,
            payload: pending.payload,
        };
        let line = event
            .to_line()
            .map_err(|e| LedgerError::Persist(format!("事件序列化失败：{e}")))?;
        debug_assert!(!line.contains('\n'), "事件必须占一个物理行");

        let mut bytes = line.into_bytes();
        let digest = ContentHash::of_bytes(&bytes).as_str().to_string();
        bytes.push(b'\n');

        let offset = self.length;
        self.file
            .write_all(&bytes)
            .await
            .map_err(|e| LedgerError::Persist(format!("写 events.jsonl 失败：{e}")))?;
        self.file
            .flush()
            .await
            .map_err(|e| LedgerError::Persist(format!("清空缓冲失败：{e}")))?;
        self.file
            .sync_all()
            .await
            .map_err(|e| LedgerError::Persist(format!("同步 events.jsonl 失败：{e}")))?;

        let len = bytes.len() as u64;
        self.length += len;
        self.next_seq += 1;
        self.index.push(LineLoc {
            seq: event.seq.0,
            offset,
            len,
        });

        Ok(AppendedEvent {
            event,
            byte_offset: offset,
            byte_len: len,
            digest,
        })
    }

    /// 读 `from` **之后**的一页事件。
    ///
    /// 返回 `(事件, 后面还有没有)`。`limit == 0` 由实现决定一页多大。
    pub async fn read(&mut self, from: Seq, limit: u32) -> Result<(Vec<Event>, bool), LedgerError> {
        let page = if limit == 0 { DEFAULT_PAGE } else { limit } as usize;
        let start = self.index.partition_point(|l| l.seq <= from.0);
        let slice = &self.index[start..];
        let take = slice.len().min(page);
        if take == 0 {
            return Ok((Vec::new(), false));
        }

        let first = slice[0];
        let last = slice[take - 1];
        let span = (last.offset + last.len) - first.offset;

        let mut buffer = vec![0u8; span as usize];
        self.file
            .seek(std::io::SeekFrom::Start(first.offset))
            .await
            .map_err(|e| LedgerError::Corrupt(format!("定位 events.jsonl 失败：{e}")))?;
        self.file
            .read_exact(&mut buffer)
            .await
            .map_err(|e| LedgerError::Corrupt(format!("读 events.jsonl 失败：{e}")))?;

        let mut events = Vec::with_capacity(take);
        for line in buffer.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            events.push(parse_line(line, self.index[start + events.len()].offset)?);
        }
        let more = slice.len() > take;
        Ok((events, more))
    }
}

/// `limit == 0` 时一页多大。够小，能在测试里翻好几页；够大，一轮补读不会来回几十次。
const DEFAULT_PAGE: u32 = 256;

/// 不持有写入器时读一页——给"读另一个 Session 的历史"用。
pub async fn read_events(
    paths: &SessionPaths,
    session: &SessionId,
    from: Seq,
    limit: u32,
) -> Result<(Vec<Event>, bool), LedgerError> {
    let bytes = match tokio::fs::read(paths.events()).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), false)),
        Err(e) => {
            return Err(persist(StoreError::Io(format!(
                "读 events.jsonl 失败：{e}"
            ))));
        }
    };
    let scan = scan(&bytes, session, &TailExpectation::default())?;
    let page = if limit == 0 { DEFAULT_PAGE } else { limit } as usize;

    let mut events = Vec::new();
    let mut more = false;
    for loc in scan.index.iter().filter(|l| l.seq > from.0) {
        if events.len() == page {
            more = true;
            break;
        }
        let line = &bytes[loc.offset as usize..(loc.offset + loc.len - 1) as usize];
        events.push(parse_line(line, loc.offset)?);
    }
    Ok((events, more))
}

// ---------------------------------------------------------------- 扫描与校验

struct Scan {
    index: Vec<LineLoc>,
    /// 完整记录覆盖的字节数。
    complete_len: u64,
    /// 末尾那段没有以换行结束的字节。
    tail_bytes: Vec<u8>,
}

fn scan(
    bytes: &[u8],
    session: &SessionId,
    expectation: &TailExpectation,
) -> Result<Scan, LedgerError> {
    let mut index: Vec<LineLoc> = Vec::new();
    let mut offset: u64 = 0;
    let mut previous: u64 = 0;

    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let Some(relative) = bytes[cursor..].iter().position(|b| *b == b'\n') else {
            break;
        };
        let line = &bytes[cursor..cursor + relative];
        let len = (relative + 1) as u64;

        // 空行是中间损坏，不是"可以跳过的坏行"。
        if line.is_empty() {
            return Err(LedgerError::Corrupt(format!(
                "events.jsonl 偏移 {offset} 处是一个空行"
            )));
        }

        let event = parse_line(line, offset)?;
        if event.session != *session {
            return Err(LedgerError::Corrupt(format!(
                "events.jsonl 偏移 {offset} 处的 session_id 是 {}，不是 {session}",
                event.session
            )));
        }
        if event.seq.0 != previous + 1 {
            return Err(LedgerError::Corrupt(format!(
                "events.jsonl 的 seq 不连续：{} 之后是 {}",
                previous, event.seq
            )));
        }
        if let Some(expected) = expectation.digests.get(&event.seq) {
            let actual = ContentHash::of_bytes(line);
            if actual.as_str() != expected {
                return Err(LedgerError::Corrupt(format!(
                    "seq {} 的摘要与已提交索引不符",
                    event.seq
                )));
            }
        }

        previous = event.seq.0;
        index.push(LineLoc {
            seq: event.seq.0,
            offset,
            len,
        });
        offset += len;
        cursor += relative + 1;
    }

    let complete_len = offset;
    let tail_bytes = bytes[cursor..].to_vec();

    // 已提交范围缺失：数据库引用到的事件在文件里根本没有完整落盘（§8.3）。
    if expectation.applied_seq.0 > previous {
        return Err(LedgerError::Corrupt(format!(
            "已提交范围缺失：数据库应用到 seq {}，文件只有到 {previous}",
            expectation.applied_seq
        )));
    }

    Ok(Scan {
        index,
        complete_len,
        tail_bytes,
    })
}

fn parse_line(line: &[u8], offset: u64) -> Result<Event, LedgerError> {
    let text = std::str::from_utf8(line)
        .map_err(|e| LedgerError::Corrupt(format!("events.jsonl 偏移 {offset} 不是 UTF-8：{e}")))?;
    Event::from_line(text)
        .map_err(|e| LedgerError::Corrupt(format!("events.jsonl 偏移 {offset} 解析失败：{e}")))
}

async fn quarantine(paths: &SessionPaths, offset: u64, tail: &[u8]) -> Result<(), LedgerError> {
    let path = paths.quarantine();
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .map_err(|e| persist(StoreError::Io(format!("打开 {} 失败：{e}", path.display()))))?;
    // 原始字节前面记一行坐标，事后诊断要知道它是从哪儿掉下来的。
    let header = format!("# offset={offset} bytes={}\n", tail.len());
    file.write_all(header.as_bytes())
        .await
        .map_err(|e| persist(StoreError::Io(format!("写隔离文件失败：{e}"))))?;
    file.write_all(tail)
        .await
        .map_err(|e| persist(StoreError::Io(format!("写隔离文件失败：{e}"))))?;
    file.write_all(b"\n")
        .await
        .map_err(|e| persist(StoreError::Io(format!("写隔离文件失败：{e}"))))?;
    file.flush()
        .await
        .map_err(|e| persist(StoreError::Io(format!("清空隔离文件缓冲失败：{e}"))))?;
    file.sync_all()
        .await
        .map_err(|e| persist(StoreError::Io(format!("同步隔离文件失败：{e}"))))?;
    Ok(())
}

// ---------------------------------------------------------------- 文件工具

/// 建目录，并同步它的父目录——新建目录项本身也要落盘（§8.3）。
pub async fn create_dir_all_synced(dir: &Path) -> Result<(), StoreError> {
    if tokio::fs::metadata(dir).await.is_ok() {
        return Ok(());
    }
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| StoreError::Io(format!("建立 {} 失败：{e}", dir.display())))?;
    if let Some(parent) = dir.parent() {
        sync_dir(parent).await?;
    }
    Ok(())
}

/// 同步一个目录的目录项。
pub async fn sync_dir(dir: &Path) -> Result<(), StoreError> {
    let file = tokio::fs::File::open(dir)
        .await
        .map_err(|e| StoreError::Io(format!("打开目录 {} 失败：{e}", dir.display())))?;
    file.sync_all()
        .await
        .map_err(|e| StoreError::Io(format!("同步目录 {} 失败：{e}", dir.display())))
}

fn persist(error: StoreError) -> LedgerError {
    crate::db::store_to_ledger(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::events::{ConversationBoundary, MessageUser};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    fn user(text: &str) -> EventPayload {
        EventPayload::MessageUser(MessageUser {
            text: Some(text.to_string()),
            text_ref: None,
        })
    }

    async fn fresh() -> (tempfile::TempDir, SessionPaths, SessionId) {
        let dir = tempfile::tempdir().expect("临时目录");
        let session = SessionId::from_raw("sess-1");
        let paths = SessionPaths::new(dir.path(), &session);
        (dir, paths, session)
    }

    async fn open(paths: &SessionPaths, session: &SessionId) -> Result<SessionLog, LedgerError> {
        SessionLog::open(paths.clone(), session.clone(), &TailExpectation::default()).await
    }

    async fn write_three(paths: &SessionPaths, session: &SessionId) {
        let mut log = open(paths, session).await.unwrap();
        for index in 1..=3 {
            log.append(PendingEvent::new(
                EventId::from_raw(format!("evt-{index}")),
                None,
                NOW,
                user(&format!("第 {index} 条")),
            ))
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn seq_starts_at_one_and_never_skips() {
        let (_dir, paths, session) = fresh().await;
        let mut log = open(&paths, &session).await.unwrap();
        assert_eq!(log.last_seq(), Seq::ZERO);

        for expected in 1..=3u64 {
            let appended = log
                .append(PendingEvent::new(
                    EventId::from_raw(format!("evt-{expected}")),
                    None,
                    NOW,
                    user("x"),
                ))
                .await
                .unwrap();
            assert_eq!(appended.seq(), Seq(expected));
        }
        assert_eq!(log.last_seq(), Seq(3));
    }

    #[tokio::test]
    async fn every_record_ends_with_a_newline_and_occupies_one_line() {
        let (_dir, paths, session) = fresh().await;
        let mut log = open(&paths, &session).await.unwrap();
        log.append(PendingEvent::new(
            EventId::from_raw("evt-1"),
            None,
            NOW,
            user("第一行\n第二行"),
        ))
        .await
        .unwrap();

        let bytes = tokio::fs::read(paths.events()).await.unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'), "完整记录以换行结束");
        assert_eq!(
            bytes.iter().filter(|b| **b == b'\n').count(),
            1,
            "内容里的换行由 JSON 转义，不是物理换行"
        );
    }

    /// 验收 ⑦（前半）：末尾半行隔离后恢复。
    #[tokio::test]
    async fn a_half_written_tail_is_quarantined_and_then_the_log_keeps_going() {
        let (_dir, paths, session) = fresh().await;
        write_three(&paths, &session).await;

        // 模拟"写到一半就断电"：追加一段没有换行的字节。
        let half = br#"{"v":1,"seq":4,"event_id":"evt-4","session_i"#;
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(paths.events())
            .await
            .unwrap();
        file.write_all(half).await.unwrap();
        file.sync_all().await.unwrap();
        drop(file);

        let mut log = open(&paths, &session).await.unwrap();
        assert_eq!(
            log.repair(),
            &TailRepair::Quarantined {
                bytes: half.len() as u64,
                offset: log.byte_len(),
            }
        );
        assert_eq!(log.last_seq(), Seq(3), "三条完整记录都还在");

        // 原始字节留在隔离文件里，**可以用于诊断，不能当作完成结果**。
        let quarantined = tokio::fs::read(paths.quarantine()).await.unwrap();
        assert!(
            quarantined.windows(half.len()).any(|w| w == half),
            "原始字节原样保留"
        );

        // 截断之后还能继续写，而且 seq 接得上。
        let next = log
            .append(PendingEvent::new(
                EventId::from_raw("evt-4"),
                None,
                NOW,
                user("第 4 条"),
            ))
            .await
            .unwrap();
        assert_eq!(next.seq(), Seq(4));

        let reopened = open(&paths, &session).await.unwrap();
        assert_eq!(reopened.repair(), &TailRepair::Clean, "再打开时已经干净了");
        assert_eq!(reopened.last_seq(), Seq(4));
    }

    /// 验收 ⑦（后半）：文件**中间**损坏报 Corrupt，不跳过坏行。
    #[tokio::test]
    async fn corruption_in_the_middle_stops_the_read() {
        let (_dir, paths, session) = fresh().await;
        write_three(&paths, &session).await;

        let text = tokio::fs::read_to_string(paths.events()).await.unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines[1] = r#"{"v":1,"seq":2,"event_id":"evt-2","session_id":"sess-1","at":"2026-09-15T08:00:00Z","type":"tool.result","data":{"call_id":"only-half"}}"#;
        tokio::fs::write(paths.events(), format!("{}\n", lines.join("\n")))
            .await
            .unwrap();

        let error = open(&paths, &session).await.unwrap_err();
        assert!(
            matches!(&error, LedgerError::Corrupt(message) if message.contains("解析失败")),
            "{error}"
        );
    }

    /// 未知 `type` 保留原始内容与序号；未知 `v` 必须暂停恢复（§8.3）。
    #[tokio::test]
    async fn an_unknown_type_is_kept_but_an_unknown_version_stops_the_read() {
        let (_dir, paths, session) = fresh().await;
        let unknown_type = r#"{"v":1,"seq":1,"event_id":"e1","session_id":"sess-1","at":"2026-09-15T08:00:00Z","type":"memory.promoted","data":{"note":"未来的机制"}}"#;
        tokio::fs::create_dir_all(paths.root()).await.unwrap();
        tokio::fs::write(paths.events(), format!("{unknown_type}\n"))
            .await
            .unwrap();
        let mut log = open(&paths, &session).await.unwrap();
        let (events, _) = log.read(Seq::ZERO, 10).await.unwrap();
        assert_eq!(events[0].seq, Seq(1), "seq 不被重新编号");
        assert!(events[0].payload.is_unknown());

        let future_version = r#"{"v":2,"seq":1,"event_id":"e1","session_id":"sess-1","at":"2026-09-15T08:00:00Z","type":"run.queued","data":{"input_ref":"e0"}}"#;
        tokio::fs::write(paths.events(), format!("{future_version}\n"))
            .await
            .unwrap();
        let error = open(&paths, &session).await.unwrap_err();
        assert!(matches!(error, LedgerError::Corrupt(_)), "{error}");
    }

    /// 已提交范围缺失：数据库引用到的事件在文件里根本没有。
    #[tokio::test]
    async fn a_committed_range_that_is_not_in_the_file_is_corruption() {
        let (_dir, paths, session) = fresh().await;
        write_three(&paths, &session).await;

        let expectation = TailExpectation {
            applied_seq: Seq(7),
            digests: Default::default(),
        };
        let error = SessionLog::open(paths.clone(), session.clone(), &expectation)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, LedgerError::Corrupt(message) if message.contains("已提交范围缺失")),
            "{error}"
        );
    }

    /// 哈希不匹配也是损坏——索引说的和文件里的不是同一行。
    #[tokio::test]
    async fn a_digest_that_does_not_match_the_index_is_corruption() {
        let (_dir, paths, session) = fresh().await;
        write_three(&paths, &session).await;

        let mut digests = std::collections::BTreeMap::new();
        digests.insert(Seq(2), "0".repeat(64));
        let expectation = TailExpectation {
            applied_seq: Seq(3),
            digests,
        };
        let error = SessionLog::open(paths.clone(), session.clone(), &expectation)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, LedgerError::Corrupt(message) if message.contains("摘要")),
            "{error}"
        );
    }

    /// 一个完整但**不被索引覆盖**的末尾行不能被删掉（§8.3：完整有效行不能因为索引落后
    /// 被删除）。
    #[tokio::test]
    async fn a_complete_tail_the_index_has_not_caught_up_with_is_kept() {
        let (_dir, paths, session) = fresh().await;
        write_three(&paths, &session).await;

        let expectation = TailExpectation {
            applied_seq: Seq(1),
            digests: Default::default(),
        };
        let mut log = SessionLog::open(paths.clone(), session.clone(), &expectation)
            .await
            .unwrap();
        assert_eq!(log.repair(), &TailRepair::Clean);
        let (events, _) = log.read(Seq::ZERO, 10).await.unwrap();
        assert_eq!(events.len(), 3, "索引落后不是删行的理由");
    }

    #[tokio::test]
    async fn reading_pages_and_says_when_it_is_done() {
        let (_dir, paths, session) = fresh().await;
        write_three(&paths, &session).await;
        let mut log = open(&paths, &session).await.unwrap();

        let (page, more) = log.read(Seq::ZERO, 2).await.unwrap();
        assert_eq!(page.len(), 2);
        assert!(more);
        let (page, more) = log.read(page.last().unwrap().seq, 2).await.unwrap();
        assert_eq!(page.len(), 1);
        assert!(!more, "读到头了");
        let (page, more) = log.read(Seq(3), 2).await.unwrap();
        assert!(page.is_empty() && !more);
    }

    #[tokio::test]
    async fn a_boundary_is_just_another_appended_event() {
        let (_dir, paths, session) = fresh().await;
        let mut log = open(&paths, &session).await.unwrap();
        let appended = log
            .append(PendingEvent::new(
                EventId::from_raw("evt-b"),
                None,
                NOW,
                EventPayload::ConversationBoundary(ConversationBoundary { by: None }),
            ))
            .await
            .unwrap();
        assert_eq!(appended.event.type_name(), "conversation.boundary");
        assert_eq!(appended.seq(), Seq(1));
    }

    #[test]
    fn a_reference_cannot_escape_the_session_directory() {
        let paths = SessionPaths::at("/data/sessions/s1");
        assert!(paths.resolve("payloads/a.bin").is_ok());
        assert!(paths.resolve("../s2/payloads/a.bin").is_err());
        assert!(paths.resolve("/etc/passwd").is_err());
        assert_eq!(
            paths.resolve("./tool-output/r/c/a/output.json").unwrap(),
            std::path::Path::new("/data/sessions/s1/tool-output/r/c/a/output.json")
        );
    }

    /// 不持有写入器时也能读——"读另一个 Session 的历史"走的就是这条。
    #[tokio::test]
    async fn events_can_be_read_without_holding_the_writer() {
        let (_dir, paths, session) = fresh().await;
        write_three(&paths, &session).await;
        let (events, more) = read_events(&paths, &session, Seq::ZERO, 2).await.unwrap();
        assert_eq!(events.len(), 2);
        assert!(more);
        let (events, more) = read_events(&paths, &session, Seq(2), 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert!(!more);
    }
}
