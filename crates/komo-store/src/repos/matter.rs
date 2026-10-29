//! `MatterRepo`：目标与领域（`docs/goals.md` §3）。
//!
//! 版本由调用方算（状态机在 kernel 的 `matter::transition`），这里只守一件事：
//! **带预期版本的写，读到的那一行版本对不上就什么都不写**——读和比在同一个事务里，
//! 并发的两次推进只有一次落库，另一次拿到版本冲突。

use async_trait::async_trait;
use komo_kernel::traits::{MatterRepo, RepoError, StoreError};
use komo_kernel::types::ids::MatterId;
use komo_kernel::types::matter::{Matter, MatterKind, MatterState, MatterTrigger, parse_due};
use komo_kernel::types::memory::Evidence;

use crate::db::{
    BoxFuture, Db, decode, encode, from_ts, from_ts_opt, map_toasty, to_ts, to_ts_opt,
};
use crate::models::MatterRow;

/// Turso 上的 [`MatterRepo`]。
#[derive(Debug, Clone)]
pub struct TursoMatterRepo {
    db: Db,
}

impl TursoMatterRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

#[async_trait]
impl MatterRepo for TursoMatterRepo {
    async fn get(&self, id: &MatterId) -> Result<Option<Matter>, RepoError> {
        let id = id.to_string();
        self.db
            .read(move |ex| {
                let id = id.clone();
                Box::pin(async move {
                    match MatterRow::filter_by_id(&id)
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?
                    {
                        Some(row) => Ok(Some(matter_from_row(&row)?)),
                        None => Ok(None),
                    }
                }) as BoxFuture<'_, Result<Option<Matter>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn list(
        &self,
        kind: Option<MatterKind>,
        include_closed: bool,
    ) -> Result<Vec<Matter>, RepoError> {
        self.db
            .read(move |ex| {
                Box::pin(async move {
                    // 清单本来就该短（§5「预算」）：整表读出来在内存里筛，不为它开索引。
                    let mut rows = MatterRow::all().exec(ex).await.map_err(map_toasty)?;
                    rows.sort_by(|a, b| a.id.cmp(&b.id));
                    let mut out = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let matter = matter_from_row(row)?;
                        if kind.is_some_and(|kind| kind != matter.kind) {
                            continue;
                        }
                        if !include_closed && matter.state.is_closed() {
                            continue;
                        }
                        out.push(matter);
                    }
                    Ok(out)
                }) as BoxFuture<'_, Result<Vec<Matter>, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }

    async fn put(
        &self,
        matter: Matter,
        expected_revision: Option<u32>,
    ) -> Result<Matter, RepoError> {
        self.db
            .with_write_retry(move |ex| {
                let matter = matter.clone();
                Box::pin(async move {
                    let existing = MatterRow::filter_by_id(matter.id.as_str())
                        .first()
                        .exec(ex)
                        .await
                        .map_err(map_toasty)?;
                    let evidence = encode(&matter.evidence)?;
                    let triggers = encode(&matter.triggers)?;
                    let due = matter.due.map(|due| due.to_string());
                    match (existing, expected_revision) {
                        (None, None) => {
                            toasty::create!(MatterRow {
                                id: matter.id.as_str(),
                                revision: i64::from(matter.revision),
                                kind: matter.kind.as_str(),
                                title: matter.title.clone(),
                                state: matter.state.as_str(),
                                next: matter.next.clone(),
                                waiting_for: matter.waiting_for.clone(),
                                due: due,
                                standard: matter.standard.clone(),
                                evidence: evidence,
                                created_at: to_ts(matter.created_at),
                                updated_at: to_ts(matter.updated_at),
                                closed_at: to_ts_opt(matter.closed_at),
                                triggers: triggers,
                            })
                            .exec(ex)
                            .await
                            .map_err(map_toasty)?;
                        }
                        (Some(_), None) => {
                            return Err(StoreError::Other(format!("事项 {} 已经存在", matter.id)));
                        }
                        (None, Some(_)) => {
                            return Err(StoreError::NotFound {
                                what: format!("事项 {}", matter.id),
                            });
                        }
                        (Some(mut row), Some(expected)) => {
                            let actual = u32::try_from(row.revision).unwrap_or(u32::MAX);
                            if actual != expected {
                                return Err(StoreError::VersionConflict { expected, actual });
                            }
                            row.update()
                                .revision(i64::from(matter.revision))
                                .title(matter.title.clone())
                                .state(matter.state.as_str())
                                .next(matter.next.clone())
                                .waiting_for(matter.waiting_for.clone())
                                .due(due)
                                .standard(matter.standard.clone())
                                .evidence(evidence)
                                .updated_at(to_ts(matter.updated_at))
                                .closed_at(to_ts_opt(matter.closed_at))
                                .triggers(triggers)
                                .exec(ex)
                                .await
                                .map_err(map_toasty)?;
                        }
                    }
                    Ok(matter)
                }) as BoxFuture<'_, Result<Matter, StoreError>>
            })
            .await
            .map_err(RepoError::from)
    }
}

fn matter_from_row(row: &MatterRow) -> Result<Matter, StoreError> {
    let corrupt = |what: String| StoreError::Corrupt(format!("事项 {}：{what}", row.id));
    Ok(Matter {
        id: MatterId::from_raw(row.id.clone()),
        revision: u32::try_from(row.revision).map_err(|_| corrupt("revision 越界".into()))?,
        kind: row.kind.parse::<MatterKind>().map_err(corrupt)?,
        title: row.title.clone(),
        state: row.state.parse::<MatterState>().map_err(corrupt)?,
        next: row.next.clone(),
        waiting_for: row.waiting_for.clone(),
        due: row
            .due
            .as_deref()
            .map(parse_due)
            .transpose()
            .map_err(corrupt)?,
        standard: row.standard.clone(),
        evidence: decode::<Vec<Evidence>>(&row.evidence, "事项证据")?,
        triggers: decode::<Vec<MatterTrigger>>(&row.triggers, "事项触发器")?,
        created_at: from_ts(row.created_at),
        updated_at: from_ts(row.updated_at),
        closed_at: from_ts_opt(row.closed_at),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::ids::{CronJobId, EventId, RunId, Seq, SessionId};
    use komo_kernel::types::matter::{MatterChange, NewMatter, transition};
    use komo_kernel::types::memory::{EvidenceRef, Provenance};
    use time::OffsetDateTime;
    use time::macros::{date, datetime};

    const T0: OffsetDateTime = datetime!(2026-10-29 08:00:00.123456789 UTC);

    async fn temp() -> (TursoMatterRepo, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("aux.db"))
            .await
            .expect("打开库");
        (TursoMatterRepo::new(db), dir)
    }

    fn goal(id: &str) -> Matter {
        let mut matter = NewMatter {
            kind: MatterKind::Goal,
            title: "这周把 owl 的发布观察做完".into(),
            next: Some("看 AB 组的 5xx".into()),
            due: Some(date!(2026 - 11 - 01)),
            standard: None,
        }
        .build(MatterId::from_raw(id), T0)
        .unwrap();
        matter.evidence.push(Evidence {
            reference: EvidenceRef::Event {
                session: SessionId::from_raw("sess-1"),
                event: EventId::from_raw("evt-1"),
                seq: Seq(1),
            },
            provenance: Provenance::UserStatement,
            observed_at: T0,
            extracted_from_run: Some(RunId::from_raw("run-1")),
        });
        matter
    }

    fn area(id: &str) -> Matter {
        NewMatter {
            kind: MatterKind::Area,
            title: "告警值班轮换".into(),
            next: None,
            due: None,
            standard: Some("每周一前完成".into()),
        }
        .build(MatterId::from_raw(id), T0)
        .unwrap()
    }

    /// 存进去什么读出来就是什么：截止日期、证据、纳秒时间、结案时间都不丢。
    #[tokio::test]
    async fn a_matter_round_trips_through_the_row() {
        let (repo, _dir) = temp().await;
        let stored = repo.put(goal("g-1"), None).await.unwrap();
        assert_eq!(repo.get(&stored.id).await.unwrap(), Some(stored.clone()));

        let waiting = transition(&stored, &MatterChange::Wait("SRE 审批".into()), T0)
            .unwrap()
            .unwrap();
        repo.put(waiting.clone(), Some(1)).await.unwrap();
        assert_eq!(repo.get(&stored.id).await.unwrap(), Some(waiting.clone()));

        // 挂着的触发器按挂上的先后存、原样读回。
        let linked = transition(
            &waiting,
            &MatterChange::Link(MatterTrigger::cron(CronJobId::from_raw("job-1"))),
            T0,
        )
        .unwrap()
        .unwrap();
        let linked = transition(
            &linked,
            &MatterChange::Link(MatterTrigger::parse_monitor("sess-1:#3f2a").unwrap()),
            T0,
        )
        .unwrap()
        .unwrap();
        repo.put(linked.clone(), Some(2)).await.unwrap();
        let read = repo.get(&stored.id).await.unwrap().unwrap();
        assert_eq!(read, linked);
        assert_eq!(read.triggers.len(), 2);

        let done = transition(&linked, &MatterChange::Done, T0)
            .unwrap()
            .unwrap();
        repo.put(done.clone(), Some(4)).await.unwrap();
        let read = repo.get(&stored.id).await.unwrap().unwrap();
        assert_eq!(read, done);
        assert_eq!(read.closed_at, Some(T0));
        assert_eq!(repo.get(&MatterId::from_raw("nope")).await.unwrap(), None);
    }

    /// 预期版本对不上：报冲突，什么都不写。新建撞上已有的 id、改一条不存在的也报错。
    #[tokio::test]
    async fn a_stale_revision_writes_nothing() {
        let (repo, _dir) = temp().await;
        let stored = repo.put(goal("g-1"), None).await.unwrap();
        let mut edited = stored.clone();
        edited.title = "换个标题".into();
        edited.revision = 2;
        assert_eq!(
            repo.put(edited.clone(), Some(7)).await,
            Err(RepoError::VersionConflict {
                expected: 7,
                actual: 1
            })
        );
        assert_eq!(repo.get(&stored.id).await.unwrap(), Some(stored.clone()));
        assert!(repo.put(goal("g-1"), None).await.is_err());
        assert!(matches!(
            repo.put(goal("g-2"), Some(1)).await,
            Err(RepoError::NotFound { .. })
        ));
    }

    /// 清单默认只给未结案的，按创建先后；可按种类筛。
    #[tokio::test]
    async fn the_list_hides_closed_matters_unless_asked() {
        let (repo, _dir) = temp().await;
        let g1 = repo.put(goal("g-1"), None).await.unwrap();
        repo.put(area("g-2"), None).await.unwrap();
        repo.put(goal("g-3"), None).await.unwrap();
        let dropped = transition(&g1, &MatterChange::Drop, T0).unwrap().unwrap();
        repo.put(dropped, Some(1)).await.unwrap();

        let ids = |items: Vec<Matter>| {
            items
                .into_iter()
                .map(|m| m.id.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(repo.list(None, false).await.unwrap()), ["g-2", "g-3"]);
        assert_eq!(
            ids(repo.list(None, true).await.unwrap()),
            ["g-1", "g-2", "g-3"]
        );
        assert_eq!(
            ids(repo.list(Some(MatterKind::Goal), true).await.unwrap()),
            ["g-1", "g-3"]
        );
        assert_eq!(
            ids(repo.list(Some(MatterKind::Area), false).await.unwrap()),
            ["g-2"]
        );
    }
}
