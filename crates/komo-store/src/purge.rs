//! 回收 Session 内容（§8.10 第 3 条）：`purge` 之前**先算引用**，再删目录。
//!
//! - [`purge_blockers`]：`aux.db` 里还有哪些东西引用这个会话（记忆证据、未送出的投递、cron 触发
//!   记录），一条一类、说人话；空数组 = 可以删内容。非空 = 网关回 409 并列出这些，不假装成功
//!   （§8.8）。会话自己的未完成工作在 `durable.db` 里，由 gateway 另算。
//! - [`remove_session_content`]：删 Session 目录，**目录不在 = `Ok(0)`**——墓碑先落、内容后删，
//!   所以"上一次删了一半就被杀掉"用同一个调用收尾，重跑与跑一遍结果相同。

use std::path::Path;

use komo_kernel::protocol::http::PurgeBlocker;
use komo_kernel::traits::StoreError;
use komo_kernel::types::chat::Outbound;
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::memory::EvidenceRef;

use crate::db::{BoxFuture, Db, map_toasty};
use crate::models::{CronFiringRow, DeliveryRow, MemoryEvidenceRow};
use crate::repos::deliveries;
use crate::session_dir::SessionPaths;

/// `purge` 之前的引用检查：**还有多少条要处置**。
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

            // `deliveries` 没有 `session_id` 列，归属只能从 `outbound` 里读：带会话的几种直接比。
            // 认不出的一条不算这个会话的——它是别人的投递，删内容不该把它算进来。
            let rows = DeliveryRow::all().exec(ex).await.map_err(map_toasty)?;
            let deliveries = rows
                .iter()
                .filter(|row| !deliveries::is_settled_state(&row.state))
                .filter(|row| {
                    matches!(
                        Outbound::from_stored(&row.outbound),
                        Ok(Some(
                            Outbound::RunFinished { session: ref s, .. }
                                | Outbound::NeedsAttention { session: ref s, .. }
                                | Outbound::RunAbsorbed { session: ref s, .. }
                        )) if s.as_str() == id
                    )
                })
                .count();
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

            Ok(blockers)
        }) as BoxFuture<'_, Result<Vec<PurgeBlocker>, StoreError>>
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
    use crate::models::{CronFiringRow, DeliveryRow, MemoryEvidenceRow};
    use komo_kernel::types::ids::{RunId, Seq};

    struct Fixture {
        /// 目录守卫：把它放走，`aux.db` 一起没了。
        _dir: tempfile::TempDir,
        db: Db,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("aux.db"))
            .await
            .expect("打开库");
        Fixture { _dir: dir, db }
    }

    impl Fixture {
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

    /// §8.10 第 3 条：`purge` 之前**先算引用**，一类一条，说人话。
    #[tokio::test]
    async fn purging_waits_until_every_reference_is_settled() {
        let f = fixture().await;
        let session = SessionId::from_raw("sess-1");
        f.evidence("ev-1", &session).await;
        f.pending_delivery("dl-1", &session).await;
        f.cron_firing("cf-1", &session).await;
        // 别的会话的东西不该被算进来。
        let other = SessionId::from_raw("sess-2");
        f.evidence("ev-2", &other).await;
        f.cron_firing("cf-2", &other).await;

        let blockers = purge_blockers(&f.db, &session).await.unwrap();
        let kinds: Vec<&str> = blockers.iter().map(|b| b.what.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["memory_evidence", "delivery", "cron_firing"],
            "三类各一条：{blockers:#?}"
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
                toasty::sql::statement("UPDATE deliveries SET state = 'sent' WHERE id = 'dl-1'")
                    .exec(ex)
                    .await
                    .map_err(map_toasty)?;
                toasty::sql::statement("DELETE FROM cron_firings WHERE id = 'cf-1'")
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

    /// 删内容：返回字节数，再删一次是 0——删一半被杀之后重跑收尾靠的就是这一条。
    #[tokio::test]
    async fn removing_content_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(root.path(), &SessionId::from_raw("s1"));
        paths.ensure().await.unwrap();
        tokio::fs::write(paths.root().join("note.txt"), b"hello")
            .await
            .unwrap();
        assert_eq!(remove_session_content(&paths).await.unwrap(), 5);
        assert_eq!(remove_session_content(&paths).await.unwrap(), 0);
        assert!(!paths.root().exists());
    }
}
