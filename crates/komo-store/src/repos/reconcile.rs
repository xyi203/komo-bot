//! §8.9 reconcile 与 §8.10 的 Session 生命周期查询。
//!
//! reconcile 是一次**只读观察 + 只写状态**的对账（§8.9）：它不调用工具、不发送外部请求、
//! 不消费授权、不改写任何内容。这个模块只提供它要的**观察与判定**，以及 §8.10 那三步里
//! 落库与删内容的两步：
//!
//! - 观察：`sessions.state` 与内容读不读得出来（[`session_observation`]）；
//! - 判定：`closing → deleted` 只在"再没有非终态 Run"时推进（[`closing_sessions_to_close`]）；
//! - 收尾：`purged` 而内容还在（[`purged_sessions_with_content`]）——墓碑先落、内容后删，
//!   所以重跑必须幂等（[`remove_session_content`] 目录不在就是 `Ok(0)`）。
//!
//! `purge` 之前**先算引用**（§8.10 第 3 条）：[`purge_blockers`] 说清楚还有多少条要处置，
//! 而不是假装删除成功（§8.8）。

use std::collections::BTreeSet;
use std::path::Path;

use komo_kernel::protocol::http::PurgeBlocker;
use komo_kernel::recovery::SessionObservation;
use komo_kernel::traits::StoreError;
use komo_kernel::types::chat::Outbound;
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::memory::EvidenceRef;
use komo_kernel::types::status::SessionState;
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, map_toasty};
use crate::models::{
    ApprovalRequestRow, CheckpointRow, CronFiringRow, DeliveryRow, MemoryEvidenceRow, RunRow,
    SessionRow,
};
use crate::repos::interventions::blocks_queue;
use crate::repos::{runs, session};
use crate::session_log::SessionPaths;

/// 一条会话此刻的观察（§8.9 的第一问）：数据库说什么状态，内容读不读得出来。
///
/// **行不在 = 这个会话不在服务范围里。** 那一行被删掉了（或从来没写过），这里按
/// `deleted` 读回去——它是这个情形下诚实的读法：墓碑不再是活的，内容按实际观察给。
/// 值认不出则是**损坏**（[`session::state`] 会报错），不在这里降级。
pub async fn session_observation(
    db: &Db,
    sessions_root: impl AsRef<Path>,
    session: &SessionId,
) -> Result<SessionObservation, StoreError> {
    let state = session::state(db, session).await?;
    Ok(SessionObservation {
        state: state.unwrap_or(SessionState::Deleted),
        content_available: content_available(&sessions_root, session).await,
    })
}

/// 目录在、`events.jsonl` 打得开吗。
///
/// 未完成的 Run 一定有事件已落盘（输入先写 JSONL 再进账本，§8.5），所以"目录在而
/// `events.jsonl` 不在"也是一种读不出内容：不能按空上下文把这条 Run 续上一轮。
///
/// **这是"内容在不在"的唯一权威来源**（§8.9 的第二个问题），别的入口只转发到它——
/// gateway 的 `RecoveryIndex::session_content` 就长在它上面。它**只观察，不创建**
/// （§8.9：观察不改写）：`SessionPaths::ensure` / `SessionLog::open` 那一类会建目录的
/// 入口一个都不在这里用。
pub async fn content_available(sessions_root: impl AsRef<Path>, session: &SessionId) -> bool {
    let paths = SessionPaths::new(sessions_root.as_ref(), session);
    if !paths.root().is_dir() {
        return false;
    }
    tokio::fs::File::open(paths.events()).await.is_ok()
}

/// `closing → deleted` 的判定（§8.10 第 2 条）：**这个会话再没有非终态 Run** 才推进。
///
/// 不是时钟。在那之前它一直是 `closing`，而"还有谁没跑完"正好是 §7.5 清单答得出的
/// ——两处用的是同一个 [`blocks_queue`]。
pub async fn closing_sessions_to_close(db: &Db) -> Result<Vec<SessionId>, StoreError> {
    db.read(move |ex| {
        Box::pin(async move {
            let sessions = SessionRow::all().exec(ex).await.map_err(map_toasty)?;
            let runs = RunRow::all().exec(ex).await.map_err(map_toasty)?;
            let mut busy: BTreeSet<String> = BTreeSet::new();
            for row in &runs {
                if blocks_queue(runs::state_of(row)?) {
                    busy.insert(row.session_id.clone());
                }
            }

            let mut out = Vec::new();
            for row in &sessions {
                if session::state_of_row(row)? == SessionState::Closing && !busy.contains(&row.id) {
                    out.push(SessionId::from_raw(row.id.clone()));
                }
            }
            out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            Ok(out)
        }) as BoxFuture<'_, Result<Vec<SessionId>, StoreError>>
    })
    .await
}

/// 墓碑已经把 `purged` 落下了、内容却还在的会话——reconcile 顺手收尾（幂等）。
pub async fn purged_sessions_with_content(
    db: &Db,
    sessions_root: impl AsRef<Path>,
) -> Result<Vec<SessionId>, StoreError> {
    let sessions_root = sessions_root.as_ref().to_path_buf();
    let purged = db
        .read(move |ex| {
            Box::pin(async move {
                let rows = SessionRow::all().exec(ex).await.map_err(map_toasty)?;
                let mut out = Vec::new();
                for row in &rows {
                    if session::state_of_row(row)? == SessionState::Purged {
                        out.push(SessionId::from_raw(row.id.clone()));
                    }
                }
                Ok(out)
            }) as BoxFuture<'_, Result<Vec<SessionId>, StoreError>>
        })
        .await?;

    let mut out = Vec::new();
    for session in purged {
        let paths = SessionPaths::new(&sessions_root, &session);
        if paths.root().exists() {
            out.push(session);
        }
    }
    out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(out)
}

/// `purge` 之前的引用检查（§8.10 第 3 条）：**还有多少条要处置**，一条一类，说人话。
///
/// 空数组 = 可以删内容。非空 = 网关回 409 并在正文里列出这些（`PurgeBlocked`），
/// 不假装成功（§8.8）。
pub async fn purge_blockers(db: &Db, session: &SessionId) -> Result<Vec<PurgeBlocker>, StoreError> {
    let id = session.to_string();
    db.read(move |ex| {
        let id = id.clone();
        Box::pin(async move {
            let mut blockers = Vec::new();

            let evidence = MemoryEvidenceRow::all().exec(ex).await.map_err(map_toasty)?;
            let evidence: usize = evidence
                .iter()
                .filter(|row| {
                    matches!(
                        serde_json::from_str::<EvidenceRef>(&row.reference),
                        Ok(EvidenceRef::Event { session, .. }) if session.as_str() == id
                    )
                })
                .count();
            if evidence > 0 {
                blockers.push(PurgeBlocker {
                    what: "memory_evidence".into(),
                    detail: format!(
                        "还有 {evidence} 条记忆证据指向这个会话的事件；先停用或删除这些记忆条目（§8.10）"
                    ),
                });
            }

            let checkpoints = CheckpointRow::filter(
                CheckpointRow::fields().session_id().eq(id.as_str()),
            )
            .exec(ex)
            .await
            .map_err(map_toasty)?;
            if !checkpoints.is_empty() {
                blockers.push(PurgeBlocker {
                    what: "checkpoint".into(),
                    detail: format!(
                        "还有 {} 个检查点引用这个会话；回收内容之前先把它们处置掉",
                        checkpoints.len()
                    ),
                });
            }

            let deliveries = deliveries_blocking(ex, &id).await?;
            if deliveries > 0 {
                blockers.push(PurgeBlocker {
                    what: "delivery".into(),
                    detail: format!("还有 {deliveries} 条未送出的投递挂在这个会话上；先送出或明确终止"),
                });
            }

            let firings = CronFiringRow::filter(CronFiringRow::fields().session_id().eq(id.as_str()))
                .exec(ex)
                .await
                .map_err(map_toasty)?;
            if !firings.is_empty() {
                blockers.push(PurgeBlocker {
                    what: "cron_firing".into(),
                    detail: format!(
                        "还有 {} 条 cron 触发记录引用这个会话；先处置这些触发（§10）",
                        firings.len()
                    ),
                });
            }

            let rows = RunRow::filter(RunRow::fields().session_id().eq(id.as_str()))
                .exec(ex)
                .await
                .map_err(map_toasty)?;
            let mut unfinished = Vec::new();
            for row in &rows {
                if blocks_queue(runs::state_of(row)?) {
                    unfinished.push(row.id.clone());
                }
            }
            if !unfinished.is_empty() {
                unfinished.sort();
                blockers.push(PurgeBlocker {
                    what: "unfinished_run".into(),
                    detail: format!(
                        "还有 {} 条未完成的 Run（{}）；先让它们跑完或明确取消",
                        unfinished.len(),
                        unfinished.join(", ")
                    ),
                });
            }

            Ok(blockers)
        }) as BoxFuture<'_, Result<Vec<PurgeBlocker>, StoreError>>
    })
    .await
}

/// 这个会话名下、**不是 `sent`** 的投递条数。
///
/// `deliveries` 没有 `session_id` 列，归属只能从 `outbound` 里读：两种带会话的（Run 结束、
/// 需要判断）直接比；审批类按审批 ID 归到会话；纯文本按目标 peer 等于会话来源判断。认不出
/// 的一条不算这个会话的——它是别人的投递，删内容不该把它算进来。
async fn deliveries_blocking(ex: &mut dyn Executor, session: &str) -> Result<usize, StoreError> {
    let origin = SessionRow::filter_by_id(session)
        .first()
        .exec(ex)
        .await
        .map_err(map_toasty)?
        .map(|row| row.origin);
    let approvals: BTreeSet<String> =
        ApprovalRequestRow::filter(ApprovalRequestRow::fields().session_id().eq(session))
            .exec(ex)
            .await
            .map_err(map_toasty)?
            .into_iter()
            .map(|row| row.id)
            .collect();

    let rows = DeliveryRow::all().exec(ex).await.map_err(map_toasty)?;
    Ok(rows
        .iter()
        .filter(|row| row.state != "sent")
        .filter(|row| {
            let outbound = serde_json::from_str::<Outbound>(&row.outbound);
            match outbound {
                Ok(Outbound::RunFinished { session: s, .. })
                | Ok(Outbound::NeedsAttention { session: s, .. }) => s.as_str() == session,
                Ok(Outbound::ApprovalRequest(presentation)) => {
                    approvals.contains(presentation.approval.as_str())
                }
                Ok(Outbound::ApprovalSettled { approval, .. }) => {
                    approvals.contains(approval.as_str())
                }
                Ok(Outbound::Text { .. }) => origin.as_deref().is_some_and(|origin| {
                    origin.split_once(':').is_some_and(|(platform, chat)| {
                        platform == row.platform && chat == row.chat_id
                    })
                }),
                Err(_) => false,
            }
        })
        .count())
}

/// 落 `purged` 墓碑（§8.10 第 3 条）：**数据库先提交，内容后删**。
///
/// CAS 从 `deleted` 到 `purged`：重跑一次返回 `false`，不会把已经回收的会话又"回收"一遍。
/// 调用方负责先走完 `closing → deleted` 与引用检查。
pub async fn mark_purged(
    db: &Db,
    session: &SessionId,
    now: OffsetDateTime,
) -> Result<bool, StoreError> {
    let session = session.clone();
    db.with_write_retry(move |ex| {
        let session = session.clone();
        Box::pin(async move {
            session::set_state_in(
                ex,
                &session,
                SessionState::Deleted,
                SessionState::Purged,
                now,
            )
            .await
        }) as BoxFuture<'_, Result<bool, StoreError>>
    })
    .await
}

/// 删掉一个 Session 目录，返回删掉的字节数。
///
/// **目录不在 = `Ok(0)`**：墓碑先落、内容后删，所以"上一次删了一半就被杀掉"用同一个调用
/// 收尾，重跑与跑一遍结果相同（§8.10 第 3 条）。删不掉报错，不返回 0 假装成功（§8.8）。
pub async fn remove_session_content(paths: &SessionPaths) -> Result<u64, StoreError> {
    let root = paths.root().to_path_buf();
    if !root.exists() {
        return Ok(0);
    }
    let bytes = dir_bytes(&root).await?;
    tokio::fs::remove_dir_all(&root).await.map_err(|error| {
        StoreError::Io(format!("删除会话目录 {} 失败：{error}", root.display()))
    })?;
    // 删掉的名字要真的从父目录里消失：父目录自己的改动也要落盘，否则崩溃后它可能又
    // 出现在别人眼里（§8.5 对"内容"的要求同样适用于删除）。
    if let Some(parent) = root.parent()
        && let Ok(dir) = tokio::fs::File::open(parent).await
    {
        let _ = dir.sync_all().await;
    }
    Ok(bytes)
}

async fn dir_bytes(root: &Path) -> Result<u64, StoreError> {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .map_err(|error| StoreError::Io(format!("读目录 {} 失败：{error}", dir.display())))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| StoreError::Io(format!("读目录 {} 失败：{error}", dir.display())))?
        {
            let meta = entry.metadata().await.map_err(|error| {
                StoreError::Io(format!("读 {} 失败：{error}", entry.path().display()))
            })?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CheckpointRow, CronFiringRow, DeliveryRow, MemoryEvidenceRow, RunRow};
    use crate::repos::runs;
    use komo_kernel::types::ids::{RunId, Seq};
    use komo_kernel::types::status::RunState;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    struct Fixture {
        /// 目录守卫：把它放走，`state.db` 与 `sessions/` 一起没了。
        _dir: tempfile::TempDir,
        db: Db,
        sessions_root: std::path::PathBuf,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("临时目录");
        let sessions_root = dir.path().join("sessions");
        let db = Db::connect(dir.path().join("state.db"))
            .await
            .expect("打开库");
        Fixture {
            _dir: dir,
            db,
            sessions_root,
        }
    }

    impl Fixture {
        fn paths(&self, session: &SessionId) -> SessionPaths {
            SessionPaths::new(&self.sessions_root, session)
        }

        /// 建会话行**并把目录与 `events.jsonl` 一起建出来**——内容可读是默认状态。
        async fn session(&self, id: &str) -> SessionId {
            let session = SessionId::from_raw(id);
            let paths = self.paths(&session);
            paths.ensure().await.unwrap();
            tokio::fs::write(paths.events(), b"{\"v\":1}\n")
                .await
                .unwrap();
            let id = id.to_string();
            self.db
                .with_write_retry(move |ex| {
                    let id = id.clone();
                    Box::pin(async move {
                        crate::repos::session::ensure_in(
                            ex,
                            &SessionId::from_raw(id),
                            "api",
                            "sessions/x/events.jsonl",
                            NOW,
                        )
                        .await
                        .map(|_| ())
                    }) as BoxFuture<'_, Result<(), StoreError>>
                })
                .await
                .unwrap();
            session
        }

        async fn set_state(&self, session: &SessionId, from: SessionState, to: SessionState) {
            let session = session.clone();
            self.db
                .with_write_retry(move |ex| {
                    let session = session.clone();
                    Box::pin(async move {
                        crate::repos::session::set_state_in(ex, &session, from, to, NOW).await
                    }) as BoxFuture<'_, Result<bool, StoreError>>
                })
                .await
                .unwrap();
        }

        /// 建一条 Run；只有 `state` 由调用方指定，其他列给能读回的最小值。
        async fn run(&self, session: &SessionId, id: &str, state: RunState) {
            let (session, id) = (session.to_string(), id.to_string());
            self.db
                .with_write_retry(move |ex| {
                    let (session, id) = (session.clone(), id.clone());
                    Box::pin(async move {
                        toasty::create!(RunRow {
                            id,
                            session_id: session,
                            request_key: "k",
                            input_hash: "h",
                            input_event: None as Option<String>,
                            input_seq: 0_i64,
                            final_event: None as Option<String>,
                            status: String::new(),
                            state: state.as_str(),
                            wait_kind: None as Option<String>,
                            wait_ref: None as Option<String>,
                            wake_at: 0_i64,
                            lease_until: 0_i64,
                            source: r#"{"kind":"interactive","session":"x"}"#,
                            peer: None as Option<String>,
                            claimed_by: None as Option<String>,
                            claim_generation: 0_i64,
                            claimed_at: 0_i64,
                            next_retry_at: 0_i64,
                            retry_attempts: 0_i64,
                            rounds: 0_i64,
                            max_rounds: 0_i64,
                            valid_until: 0_i64,
                            model_snapshot: r#"{"provider":"test","base_url":"memory://test","model":"m","api_key_env":"K","timeout_secs":30}"#,
                            effort: None as Option<String>,
                            grants: "[]",
                            memory_work: "pending",
                            memory_cursor: 0_i64,
                            last_error: None as Option<String>,
                            created_at: 0_i64,
                            updated_at: 0_i64,
                            ended_at: 0_i64,
                        })
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                        Ok(())
                    }) as BoxFuture<'_, Result<(), StoreError>>
                })
                .await
                .unwrap();
        }

        /// 一条指向某会话事件的记忆证据（§9.2）。
        async fn evidence(&self, id: &str, session: &SessionId) {
            let reference = serde_json::to_string(&EvidenceRef::Event {
                session: session.clone(),
                event: komo_kernel::types::ids::EventId::from_raw("evt-1"),
                seq: Seq(3),
            })
            .unwrap();
            let id = id.to_string();
            self.db
                .with_write_retry(move |ex| {
                    let id = id.clone();
                    let reference = reference.clone();
                    Box::pin(async move {
                        toasty::create!(MemoryEvidenceRow {
                            id,
                            memory_id: "mem-1",
                            revision: 1_i64,
                            ordinal: 0_i64,
                            reference,
                            provenance: "tool_observation",
                            observed_at: 0_i64,
                            extracted_from_run: None as Option<String>,
                        })
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                        Ok(())
                    }) as BoxFuture<'_, Result<(), StoreError>>
                })
                .await
                .unwrap();
        }

        async fn checkpoint(&self, id: &str, session: &SessionId) {
            let (id, session) = (id.to_string(), session.to_string());
            self.db
                .with_write_retry(move |ex| {
                    let (id, session) = (id.clone(), session.clone());
                    Box::pin(async move {
                        toasty::create!(CheckpointRow {
                            id,
                            session_id: session,
                            run_id: None as Option<String>,
                            covers_from: 0_i64,
                            covers_to: 3_i64,
                            byte_offset: 0_i64,
                            format_version: 1_i64,
                            memories: "[]",
                            retrieval_config_version: None as Option<String>,
                            cursor: 3_i64,
                            created_at: 0_i64,
                        })
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                        Ok(())
                    }) as BoxFuture<'_, Result<(), StoreError>>
                })
                .await
                .unwrap();
        }

        /// 一条**还没送出**的投递：正文说得出它属于哪个会话（§11.4）。
        async fn pending_delivery(&self, id: &str, session: &SessionId) {
            let outbound = serde_json::to_string(&Outbound::RunFinished {
                session: session.clone(),
                run: RunId::from_raw("run-1"),
                summary: "跑完了".into(),
            })
            .unwrap();
            let id = id.to_string();
            self.db
                .with_write_retry(move |ex| {
                    let (id, outbound) = (id.clone(), outbound.clone());
                    Box::pin(async move {
                        toasty::create!(DeliveryRow {
                            id,
                            platform: "feishu",
                            chat_id: "oc_1",
                            is_home: false,
                            outbound,
                            approval_id: None as Option<String>,
                            state: "pending",
                            attempts: 0_i64,
                            last_error: None as Option<String>,
                            created_at: 0_i64,
                            updated_at: 0_i64,
                        })
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                        Ok(())
                    }) as BoxFuture<'_, Result<(), StoreError>>
                })
                .await
                .unwrap();
        }

        async fn cron_firing(&self, id: &str, session: &SessionId) {
            let (id, session) = (id.to_string(), session.to_string());
            self.db
                .with_write_retry(move |ex| {
                    let (id, session) = (id.clone(), session.clone());
                    Box::pin(async move {
                        toasty::create!(CronFiringRow {
                            id,
                            job_id: "job-1",
                            job_version: 1_i64,
                            scheduled_at: 0_i64,
                            prompt: "报个天气",
                            session_id: Some(session),
                            run_id: None as Option<String>,
                            status: "ok",
                            error: None as Option<String>,
                            created_at: 0_i64,
                        })
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                        Ok(())
                    }) as BoxFuture<'_, Result<(), StoreError>>
                })
                .await
                .unwrap();
        }
    }

    /// §8.9 第一问：会话还在服务范围里吗、内容读得出来吗。
    #[tokio::test]
    async fn the_observation_says_whether_the_session_still_serves_and_has_content() {
        let f = fixture().await;
        let session = f.session("sess-1").await;

        let active = session_observation(&f.db, &f.sessions_root, &session)
            .await
            .unwrap();
        assert_eq!(active.state, SessionState::Active);
        assert!(active.serves(), "目录与 JSONL 都在");

        // 目录被手工删掉（§8.9 的那个现象）：**不许按空上下文继续**。
        tokio::fs::remove_dir_all(f.paths(&session).root())
            .await
            .unwrap();
        let gone = session_observation(&f.db, &f.sessions_root, &session)
            .await
            .unwrap();
        assert!(!gone.serves());
        assert!(gone.blocked_reason().unwrap().contains("内容读不出来"));

        // 逻辑删除之后连内容也不需要看：状态本身就不服务。
        f.set_state(&session, SessionState::Active, SessionState::Closing)
            .await;
        f.set_state(&session, SessionState::Closing, SessionState::Deleted)
            .await;
        let deleted = session_observation(&f.db, &f.sessions_root, &session)
            .await
            .unwrap();
        assert!(!deleted.serves(), "deleted 不服务");
        assert!(deleted.blocked_reason().unwrap().contains("逻辑删除"));

        // 会话行不在：也不服务（这是"内容与账本对不上"里最彻底的一种）。
        let orphan = SessionId::from_raw("sess-nowhere");
        let missing = session_observation(&f.db, &f.sessions_root, &orphan)
            .await
            .unwrap();
        assert!(!missing.serves());
    }

    /// 「内容在不在」是**只读**的一问（§8.9：观察不改写）：目录不在就是不在，问多少次
    /// 都不会把它建出来。而这正是 §8.10 判一条 Run 能不能领的依据——问一问就把它变成
    /// "在"，那条判断就永远答"能领"。
    #[tokio::test]
    async fn asking_whether_content_is_available_never_creates_it() {
        let f = fixture().await;
        let session = f.session("sess-1").await;
        assert!(content_available(&f.sessions_root, &session).await);

        // 目录在、`events.jsonl` 不在：未完成的 Run 一定有事件落盘（§8.5），所以这也算
        // 读不出内容——不能按空上下文把这条 Run 续上一轮。
        tokio::fs::remove_file(f.paths(&session).events())
            .await
            .unwrap();
        assert!(!content_available(&f.sessions_root, &session).await);

        // 目录也没了：观察之后它必须还是不在。
        tokio::fs::remove_dir_all(f.paths(&session).root())
            .await
            .unwrap();
        assert!(!content_available(&f.sessions_root, &session).await);
        assert!(!content_available(&f.sessions_root, &session).await);
        assert!(
            !f.paths(&session).root().exists(),
            "问一次不等于建一次（§8.9：观察不改写）"
        );
    }

    /// 认不出的状态是损坏，不是"当一个活会话读"（§8.10）。
    #[tokio::test]
    async fn an_unknown_session_state_stops_the_observation() {
        let f = fixture().await;
        let session = f.session("sess-1").await;
        f.db.with_write_retry(|ex| {
            Box::pin(async move {
                toasty::sql::statement("UPDATE sessions SET state = 'zombie' WHERE id = 'sess-1'")
                    .exec(ex)
                    .await
                    .map(|_| ())
                    .map_err(map_toasty)
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();

        let error = session_observation(&f.db, &f.sessions_root, &session)
            .await
            .expect_err("认不出的状态要报损坏");
        assert!(matches!(&error, StoreError::Corrupt(why) if why.contains("zombie")));
    }

    /// §8.10 第 2 条：`closing → deleted` **不是时钟**——还有非终态 Run 就不推进。
    #[tokio::test]
    async fn closing_becomes_deletable_only_when_nothing_is_unfinished() {
        let f = fixture().await;
        let busy = f.session("sess-busy").await;
        let done = f.session("sess-done").await;
        let active = f.session("sess-active").await;
        for session in [&busy, &done] {
            f.set_state(session, SessionState::Active, SessionState::Closing)
                .await;
        }
        f.run(&busy, "run-waiting", RunState::Waiting).await;
        f.run(&done, "run-done", RunState::Completed).await;
        let _ = &active;

        assert_eq!(
            closing_sessions_to_close(&f.db).await.unwrap(),
            vec![done.clone()],
            "只有那个一条未完成 Run 都不剩的能推进"
        );

        // 最后一条也终态了，下一个就轮到它。
        f.db.with_write_retry(|ex| {
            Box::pin(async move {
                let mut row = runs::get_in(ex, &RunId::from_raw("run-waiting"))
                    .await?
                    .unwrap();
                row.update()
                    .state(RunState::Cancelled.as_str())
                    .wait_kind(None as Option<String>)
                    .wait_ref(None as Option<String>)
                    .wake_at(0_i64)
                    .exec(ex)
                    .await
                    .map_err(map_toasty)
                    .map(|_| ())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
        assert_eq!(
            closing_sessions_to_close(&f.db).await.unwrap(),
            vec![busy.clone(), done.clone()]
        );
    }

    /// 墓碑已落、内容还在的会话——reconcile 顺手收尾（幂等）。
    #[tokio::test]
    async fn a_purged_tombstone_whose_content_survived_is_reported() {
        let f = fixture().await;
        let session = f.session("sess-1").await;
        f.set_state(&session, SessionState::Active, SessionState::Closing)
            .await;
        f.set_state(&session, SessionState::Closing, SessionState::Deleted)
            .await;
        assert!(
            mark_purged(&f.db, &session, NOW).await.unwrap(),
            "deleted → purged 推得动"
        );
        assert!(
            !mark_purged(&f.db, &session, NOW).await.unwrap(),
            "重跑一次什么都不改（幂等）"
        );

        assert_eq!(
            purged_sessions_with_content(&f.db, &f.sessions_root)
                .await
                .unwrap(),
            vec![session.clone()],
            "墓碑落了，目录还在"
        );

        // 删内容：返回字节数，再删一次是 0——删一半被杀之后重跑收尾靠的就是这一条。
        let paths = f.paths(&session);
        let removed = remove_session_content(&paths).await.unwrap();
        assert!(removed > 0, "有内容才删得出字节数");
        assert_eq!(remove_session_content(&paths).await.unwrap(), 0);
        assert!(!paths.root().exists());
        assert!(
            purged_sessions_with_content(&f.db, &f.sessions_root)
                .await
                .unwrap()
                .is_empty(),
            "收尾之后不再报它"
        );
    }

    /// §8.10 第 3 条：`purge` 之前**先算引用**，一类一条，说人话。
    #[tokio::test]
    async fn purging_waits_until_every_reference_is_settled() {
        let f = fixture().await;
        let session = f.session("sess-1").await;
        f.evidence("ev-1", &session).await;
        f.checkpoint("ck-1", &session).await;
        f.pending_delivery("dl-1", &session).await;
        f.cron_firing("cf-1", &session).await;
        f.run(&session, "run-1", RunState::Waiting).await;
        // 别的会话的东西不该被算进来。
        let other = f.session("sess-2").await;
        f.evidence("ev-2", &other).await;
        f.checkpoint("ck-2", &other).await;

        let blockers = purge_blockers(&f.db, &session).await.unwrap();
        let kinds: Vec<&str> = blockers.iter().map(|b| b.what.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "memory_evidence",
                "checkpoint",
                "delivery",
                "cron_firing",
                "unfinished_run"
            ],
            "五类各一条：{blockers:#?}"
        );
        for blocker in &blockers {
            assert!(
                blocker.detail.contains('1'),
                "{} 的正文要说清还有多少条：{}",
                blocker.what,
                blocker.detail
            );
        }
        let evidence = blockers
            .iter()
            .find(|b| b.what == "memory_evidence")
            .unwrap();
        assert!(
            evidence.detail.contains("记忆") && evidence.detail.contains("停用"),
            "记忆那一条要说人话、说清先处置什么：{}",
            evidence.detail
        );

        // 引用都处置掉之后才轮得到删内容。
        f.db.with_write_retry(|ex| {
            Box::pin(async move {
                toasty::sql::statement("DELETE FROM memory_evidence WHERE id = 'ev-1'")
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                toasty::sql::statement("DELETE FROM checkpoints WHERE id = 'ck-1'")
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                toasty::sql::statement("UPDATE deliveries SET state = 'sent' WHERE id = 'dl-1'")
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                toasty::sql::statement("DELETE FROM cron_firings WHERE id = 'cf-1'")
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                toasty::sql::statement("DELETE FROM runs WHERE id = 'run-1'")
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                Ok(())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await
        .unwrap();
        assert!(
            purge_blockers(&f.db, &session).await.unwrap().is_empty(),
            "引用都处置完了，可以删内容"
        );
        // 别的会话的东西一条没动。
        assert_eq!(purge_blockers(&f.db, &other).await.unwrap().len(), 2);
    }
}
