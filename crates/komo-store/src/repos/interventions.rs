//! §7.5 Intervention：一切"需要人判断"的同一个入口。
//!
//! 结果不明与阻塞合成一条**派生清单**（审批在 2026-10-11 删掉了，只剩这两类）：
//!
//! - **清单不是第三张表**（第 1 条）。权威仍是 `runs` 与 `tool_calls`，`kind` 由
//!   "有没有一条 `uncertain` 调用"当场判出。多一张 `interventions` 表就多一处会与权威漂移
//!   的状态，而这次改造的全部理由就是不要那个（§8.9）。
//! - **挡队的每一条都在清单里，而且是同一次判定**（第 2 条）。[`waits_for_human`] 回答
//!   "进清单"，[`blocks_queue`] 回答"挡不挡队"，两者都在这里，由同一份状态派生；§8.7 的
//!   领取语句与 reconcile 的 `closing → deleted` 用的是同一份判定。
//! - **结论只走一条路：核对 → 按 §8.4 行事**（第 3 条）。这个模块只回答"有什么在等人"，
//!   答复的副作用由 runtime 落地。

use std::collections::BTreeMap;

use komo_kernel::protocol::http::{
    InterventionDetail, InterventionKind, InterventionListQuery, InterventionSummary,
};
use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::{RunId, SessionId, ToolCallId};
use komo_kernel::types::plan::PlanHash;
use komo_kernel::types::status::{RunState, WaitReason};
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, from_ts, map_toasty};
use crate::models::{RunRow, ToolCallRow};
use crate::repos::runs::{state_of, wait_of};

/// §7.5 第 2 条：这条 Run **停在人身上**吗——清单列的就是它，不需要第二个条件。
///
/// 等时钟（`wait_kind = 'retry'`）不算：它在等时钟，不是等人。等依赖
/// （`wait_kind = 'dependency'`）也不算：它在等前一条 Run 跑完，那件事不需要人回答。
/// 删审批之前停在审批上的（`wait_kind = 'approval'`，D2）也不算：Gateway 启动时把它们
/// 续跑（[`crate::repos::recovery::RecoveryStore::legacy_approval_waits`]），不问人。
pub fn waits_for_human(wait: &WaitReason) -> bool {
    wait.needs_a_person()
}

/// §7.5 第 2 条：这条 Run 挡不挡它所在 Session 的队。
///
/// 与 §8.7 领取语句里那句 `earlier.state NOT IN ('completed','failed','cancelled',
/// 'abandoned')` 是同一份判定，也是 reconcile 判 `closing → deleted` 时问的那一句
/// （"还有谁没跑完"）。**停着等人的 Run 一定挡队**——两处判定分家就会出现"卡住但清单为空"。
pub fn blocks_queue(state: RunState) -> bool {
    state.is_unfinished()
}

/// 一条 Run 上那条结果不明的调用（§8.6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncertainCall {
    pub call: ToolCallId,
    pub tool: String,
    pub plan_hash: Option<PlanHash>,
}

/// 一条停在人身上的 Run。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedRun {
    pub run: RunId,
    pub session: SessionId,
    pub state: RunState,
    /// 在等什么。构造这个类型的路径已经保证它**要人**（[`waits_for_human`]）。
    pub wait: WaitReason,
    /// `runs.last_error`——`blocked` 的问题正文（§7.5 那张表）。
    pub reason: Option<String>,
    /// 这条 Run 上那条 `uncertain` 的调用：有它就是 `verify`（§8.6）。
    pub uncertain: Option<UncertainCall>,
    pub created_at: OffsetDateTime,
}

/// 停在人身上的 Run，按创建序。每一条 Run 至多出现一次。
pub async fn blocked_runs(db: &Db) -> Result<Vec<BlockedRun>, StoreError> {
    db.read(move |ex| Box::pin(async move { blocked_runs_in(ex).await }))
        .await
}

/// 待处理清单（§7.5）：停在人身上的 Run。
///
/// `kind` 当场判：有 `uncertain` 调用 → `Verify`；否则 → `Blocked`。句柄是 Run ID。
/// 删审批之前停在审批上的 Run 不在这里（[`waits_for_human`]）。
///
/// 按 `created_at` 排序。
pub async fn list(
    db: &Db,
    query: &InterventionListQuery,
) -> Result<Vec<InterventionSummary>, StoreError> {
    let query = query.clone();
    db.read(move |ex| {
        let query = query.clone();
        Box::pin(async move {
            let mut out: Vec<InterventionSummary> =
                blocked_runs_in(ex).await?.iter().map(run_summary).collect();
            out.retain(|summary| {
                query
                    .session
                    .as_ref()
                    .is_none_or(|session| &summary.session == session)
                    && query
                        .run
                        .as_ref()
                        .is_none_or(|run| summary.run.as_ref() == Some(run))
                    && query.kind.is_none_or(|kind| summary.kind == kind)
            });
            out.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.handle.cmp(&b.handle))
            });
            Ok(out)
        }) as BoxFuture<'_, Result<Vec<InterventionSummary>, StoreError>>
    })
    .await
}

/// 按句柄（Run ID）取一条 `verify` / `blocked`。
///
/// `None` = 这个句柄此刻没有任何东西在等人（已经跑完的 Run、等时钟 / 依赖的 Run、
/// 删审批之前停在审批上的 Run 都是这样）。
pub async fn find(db: &Db, handle: &str) -> Result<Option<InterventionDetail>, StoreError> {
    let handle = handle.to_string();
    db.read(move |ex| {
        let handle = handle.clone();
        Box::pin(async move {
            let Some(row) = RunRow::filter_by_id(&handle)
                .first()
                .exec(ex)
                .await
                .map_err(map_toasty)?
            else {
                return Ok(None);
            };
            let state = state_of(&row)?;
            let Some(wait) = wait_of(&row)? else {
                return Ok(None);
            };
            if !waits_for_human(&wait) {
                return Ok(None);
            }
            let uncertain = uncertain_by_run(ex).await?.remove(&row.id);
            let entry = BlockedRun {
                run: RunId::from_raw(row.id.clone()),
                session: SessionId::from_raw(row.session_id.clone()),
                state,
                wait,
                reason: row.last_error.clone(),
                uncertain,
                created_at: from_ts(row.created_at),
            };
            Ok(Some(detail_of(&entry)))
        }) as BoxFuture<'_, Result<Option<InterventionDetail>, StoreError>>
    })
    .await
}

/// 停在人身上的 Run（调用方已经在一个读连接 / 事务里）。
async fn blocked_runs_in(ex: &mut dyn Executor) -> Result<Vec<BlockedRun>, StoreError> {
    let rows = RunRow::all().exec(ex).await.map_err(map_toasty)?;
    let mut uncertain = uncertain_by_run(ex).await?;
    let mut out = Vec::new();
    for row in &rows {
        let state = state_of(row)?;
        let Some(wait) = wait_of(row)? else {
            continue;
        };
        if !waits_for_human(&wait) {
            continue;
        }
        out.push(BlockedRun {
            run: RunId::from_raw(row.id.clone()),
            session: SessionId::from_raw(row.session_id.clone()),
            state,
            wait,
            reason: row.last_error.clone(),
            uncertain: uncertain.remove(&row.id),
            created_at: from_ts(row.created_at),
        });
    }
    out.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.run.as_str().cmp(b.run.as_str()))
    });
    Ok(out)
}

/// `uncertain` 的调用，按 `run_id` 归组。
///
/// 一条 Run 上可能不止一条（两次调用都停在核对上），取 id 最小的那条做句柄：它稳定、
/// 可重复，界面不会因为查询顺序换一条调用问同一个问题。
async fn uncertain_by_run(
    ex: &mut dyn Executor,
) -> Result<BTreeMap<String, UncertainCall>, StoreError> {
    let mut rows = ToolCallRow::filter(ToolCallRow::fields().state().eq("uncertain"))
        .exec(ex)
        .await
        .map_err(map_toasty)?;
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    let mut out = BTreeMap::new();
    for row in &rows {
        out.entry(row.run_id.clone()).or_insert(UncertainCall {
            call: ToolCallId::from_raw(row.id.clone()),
            tool: row.tool.clone(),
            plan_hash: row.plan_hash.clone().map(PlanHash::from_raw),
        });
    }
    Ok(out)
}

/// `verify` / `blocked` 那一类的清单项：句柄是 Run ID。
fn run_summary(entry: &BlockedRun) -> InterventionSummary {
    let (kind, question) = classify(entry);
    summary_of(entry, kind, &question)
}

fn summary_of(entry: &BlockedRun, kind: InterventionKind, question: &str) -> InterventionSummary {
    InterventionSummary {
        handle: entry.run.to_string(),
        kind,
        session: entry.session.clone(),
        run: Some(entry.run.clone()),
        call: entry.uncertain.as_ref().map(|call| call.call.clone()),
        question: question.to_string(),
        verdicts: kind.verdicts(),
        created_at: entry.created_at,
    }
}

/// 按句柄取到的那一条的完整形状（§7.5 的 `InterventionDetail`）。
///
/// 种类与问题正文都由 [`classify`] 判一次——列表项（`/pending`）与详情（`komo
/// intervention show`）不许对同一条问题各说各的（§7.5 第 2 条的那句"同一个判定"）。
fn detail_of(entry: &BlockedRun) -> InterventionDetail {
    let (kind, question) = classify(entry);
    let summary = summary_of(entry, kind, &question);
    match (kind, &entry.uncertain) {
        (InterventionKind::Verify, Some(call)) => InterventionDetail::Verify {
            summary,
            tool: call.tool.clone(),
            plan_hash: call.plan_hash.clone(),
            reason: question,
        },
        _ => InterventionDetail::Blocked {
            summary,
            reason: question,
        },
    }
}

/// `kind` 当场判（§7.5 第 1 条）：有 `uncertain` 调用 → `verify`；否则 → `blocked`。
fn classify(entry: &BlockedRun) -> (InterventionKind, String) {
    match &entry.wait {
        WaitReason::Intervention { .. } => match &entry.uncertain {
            Some(call) => (InterventionKind::Verify, verify_reason(call)),
            None => (InterventionKind::Blocked, blocked_reason(entry)),
        },
        // 构造这个类型时已经按 `needs_a_person` 收过一遍，这三种到不了；真出现了也只能
        // 当阻塞报出去——不 pretend 它不是问题。
        WaitReason::Approval { .. } | WaitReason::Retry { .. } | WaitReason::Dependency { .. } => {
            (InterventionKind::Blocked, blocked_reason(entry))
        }
    }
}

/// `verify` 的问题：**哪次调用的结果不明**（§8.6）——操作者要核对的是那个动作。
fn verify_reason(call: &UncertainCall) -> String {
    format!(
        "{}（调用 {}）的结果不明：副作用可能已经发生，先核对目标状态再决定怎么继续",
        call.tool, call.call
    )
}

/// `blocked` 的问题：`runs.last_error` 写下的那句。没有就老实说没有——不要编一个。
fn blocked_reason(entry: &BlockedRun) -> String {
    entry.reason.clone().unwrap_or_else(|| {
        format!(
            "这条 Run 停在 {} 上等人判断，但没有写下原因（runs.last_error 为空）",
            entry.wait.kind()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ToolCallRow;
    use crate::repos::{queue, runs, session};
    use komo_kernel::protocol::http::InterventionVerdict;
    use komo_kernel::traits::RunQueue;
    use komo_kernel::types::ids::{ExecutorId, InterventionId};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-15 08:00:00 UTC);

    async fn temp() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("state.db"))
            .await
            .expect("打开库");
        (db, dir)
    }

    async fn write<F>(db: &Db, op: F)
    where
        F: for<'a> Fn(&'a mut dyn Executor) -> BoxFuture<'a, Result<(), StoreError>>
            + Send
            + Sync
            + Clone
            + 'static,
    {
        db.with_write_retry(move |ex| op(ex)).await.unwrap();
    }

    /// 建一个会话（`active`）+ 一条 Run。
    async fn run_row(db: &Db, session: &str, run: &str, state: RunState, wait: Option<WaitReason>) {
        let (session, run) = (session.to_string(), run.to_string());
        let (kind, reference, wake_at) = match &wait {
            Some(wait) => (
                Some(wait.kind().to_string()),
                runs::wait_ref_str(wait),
                wait.wake_at().map(crate::db::to_ts).unwrap_or(0),
            ),
            None => (None, None, 0),
        };
        write(db, move |ex| {
            let (session, run) = (session.clone(), run.clone());
            let (kind, reference) = (kind.clone(), reference.clone());
            Box::pin(async move {
                session::ensure_in(
                    ex,
                    &SessionId::from_raw(session.clone()),
                    "api",
                    &format!("sessions/{session}/events.jsonl"),
                    NOW,
                )
                .await?;
                toasty::create!(RunRow {
                    id: run,
                    session_id: session,
                    request_key: "k",
                    input_hash: "h",
                    input_event: None as Option<String>,
                    input_seq: 0_i64,
                    final_event: None as Option<String>,
                    parent_run_id: None as Option<String>,
                    delegate: None as Option<String>,
                    status: String::new(),
                    state: state.as_str(),
                    wait_kind: kind,
                    wait_ref: reference,
                    wake_at,
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
        .await;
    }

    /// 删审批之前停在审批上的 Run（`waiting + approval`，D2）。
    async fn waiting_on_legacy_approval(db: &Db, session: &str, run: &str, approval: &str) {
        run_row(
            db,
            session,
            run,
            RunState::Waiting,
            Some(WaitReason::Approval {
                approval: approval.to_string(),
            }),
        )
        .await;
    }

    /// 一条停在人身上的 Run（`waiting + intervention`，句柄是它自己的 Run ID）。
    async fn waiting_on_a_person(db: &Db, session: &str, run: &str) {
        run_row(
            db,
            session,
            run,
            RunState::Waiting,
            Some(WaitReason::Intervention {
                intervention: InterventionId::for_run(&RunId::from_raw(run)),
            }),
        )
        .await;
    }

    async fn set_last_error(db: &Db, run: &str, error: &str) {
        let (run, error) = (run.to_string(), error.to_string());
        write(db, move |ex| {
            let (run, error) = (run.clone(), error.clone());
            Box::pin(async move {
                let mut row = runs::get_in(ex, &RunId::from_raw(run))
                    .await?
                    .expect("run 在");
                row.update()
                    .last_error(Some(error))
                    .exec(ex)
                    .await
                    .map_err(map_toasty)
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await;
    }

    /// 一条结果不明的调用（§8.6）。
    async fn uncertain_call(db: &Db, id: &str, session: &str, run: &str, tool: &str) {
        let (id, session, run, tool) = (
            id.to_string(),
            session.to_string(),
            run.to_string(),
            tool.to_string(),
        );
        write(db, move |ex| {
            let (id, session, run, tool) = (id.clone(), session.clone(), run.clone(), tool.clone());
            Box::pin(async move {
                toasty::create!(ToolCallRow {
                    id,
                    session_id: session,
                    run_id: run,
                    tool,
                    provider_call_id: "pc",
                    round: 0_i64,
                    state: "uncertain",
                    args_event: None as Option<String>,
                    plan_event: None as Option<String>,
                    result_event: None as Option<String>,
                    plan_hash: Some("ph".to_string()),
                    recovery: "{}",
                    idempotency_key: None as Option<String>,
                    attempts: 1_i64,
                    output_ref: None as Option<String>,
                    preview: None as Option<String>,
                    created_at: 0_i64,
                    updated_at: 0_i64,
                })
                .exec(ex)
                .await
                .map_err(map_toasty)?;
                Ok(())
            }) as BoxFuture<'_, Result<(), StoreError>>
        })
        .await;
    }

    async fn list_now(db: &Db) -> Vec<InterventionSummary> {
        list(db, &InterventionListQuery::default()).await.unwrap()
    }

    /// 验收 ⑨（关键）：**停在人身上的每一条 Run 恰好出现一次**。
    ///
    /// 一条停在人身上的（Run ID 做句柄）、一条删审批之前停在审批上的（D2：不进清单，
    /// Gateway 启动时续跑它）、一条同 Session 的 `queued`——清单里正好一条；那个
    /// `queued` Run 照样领不走：前面的 Run 还没跑完。
    #[tokio::test]
    async fn every_run_stopped_on_a_human_appears_exactly_once() {
        let (db, _dir) = temp().await;
        waiting_on_legacy_approval(&db, "sess-1", "run-0001", "appr-1").await;
        waiting_on_a_person(&db, "sess-1", "run-0002").await;
        set_last_error(&db, "run-0002", "引用的计划文件已经是另一份了").await;
        run_row(&db, "sess-1", "run-0003", RunState::Queued, None).await;

        let pending = list_now(&db).await;
        assert_eq!(pending.len(), 1, "只有停滞那一条：{pending:#?}");

        let blocked = &pending[0];
        assert_eq!(blocked.kind, InterventionKind::Blocked);
        assert_eq!(blocked.handle, "run-0002", "句柄是 Run ID");
        assert_eq!(blocked.question, "引用的计划文件已经是另一份了");
        assert_eq!(
            blocked.verdicts,
            InterventionKind::Blocked.verdicts(),
            "清单项自带答案菜单"
        );
        assert!(blocked.verdicts.contains(&InterventionVerdict::Resolve));

        // 挡队与进清单是两个问题：`run-0003` 领不走（前面两条都没跑完）。
        let queue = queue::TursoRunQueue::new(db.clone());
        assert!(
            queue
                .claim(&ExecutorId::from_raw("exec-1"))
                .await
                .unwrap()
                .is_none(),
            "前面的 Run 没跑完，后面的不许领"
        );
    }

    /// D2：删审批之前停在审批上的 Run 不要人——清单里没有它，按 Run ID 也取不到。
    #[tokio::test]
    async fn a_legacy_approval_wait_is_not_an_intervention() {
        let (db, _dir) = temp().await;
        waiting_on_legacy_approval(&db, "sess-1", "run-0001", "appr-1").await;

        assert!(list_now(&db).await.is_empty());
        assert!(find(&db, "run-0001").await.unwrap().is_none());
        assert!(blocked_runs(&db).await.unwrap().is_empty());
    }

    /// 验收 ⑨：`waiting + intervention` 上带一条 `uncertain` 调用 → `verify`，`call` 对得上。
    #[tokio::test]
    async fn an_uncertain_call_turns_an_intervention_wait_into_a_verify() {
        let (db, _dir) = temp().await;
        waiting_on_a_person(&db, "sess-1", "run-0001").await;
        set_last_error(&db, "run-0001", "副作用可能已经发生").await;
        uncertain_call(&db, "call-1", "sess-1", "run-0001", "shell").await;

        let pending = list_now(&db).await;
        assert_eq!(pending.len(), 1);
        let entry = &pending[0];
        assert_eq!(entry.kind, InterventionKind::Verify);
        assert_eq!(entry.handle, "run-0001");
        assert_eq!(entry.call, Some(ToolCallId::from_raw("call-1")));
        assert!(
            entry.question.contains("shell") && entry.question.contains("call-1"),
            "问题要说清是哪次调用：{}",
            entry.question
        );
        assert_eq!(entry.verdicts, InterventionKind::Verify.verdicts());
        assert!(entry.verdicts.contains(&InterventionVerdict::Satisfied));
        assert!(!entry.verdicts.contains(&InterventionVerdict::Resolve));

        // 按 Run ID 取详情：同一条，带工具与计划哈希。
        match find(&db, "run-0001").await.unwrap() {
            Some(InterventionDetail::Verify {
                tool,
                plan_hash,
                summary,
                ..
            }) => {
                assert_eq!(tool, "shell");
                assert_eq!(plan_hash, Some(PlanHash::from_raw("ph")));
                assert_eq!(summary.call, Some(ToolCallId::from_raw("call-1")));
            }
            other => panic!("该是 verify：{other:?}"),
        }
    }

    /// §7.5 第 2 条：等时钟（`retry`）与等依赖（`dependency`）**不进清单**，但照样挡队
    /// ——这就是"挡队"与"进清单"的分界。终态的 Run 两样都不是。
    #[tokio::test]
    async fn waits_on_clocks_or_dependencies_stay_off_the_list_but_still_block() {
        let (db, _dir) = temp().await;
        run_row(
            &db,
            "sess-1",
            "run-0001",
            RunState::Waiting,
            Some(WaitReason::Retry {
                attempts: 1,
                not_before: NOW + time::Duration::seconds(30),
                cause: komo_kernel::types::status::RetryCause::RateLimited,
            }),
        )
        .await;
        run_row(
            &db,
            "sess-1",
            "run-0002",
            RunState::Waiting,
            Some(WaitReason::Dependency {
                run: RunId::from_raw("run-0001"),
            }),
        )
        .await;
        run_row(&db, "sess-2", "run-0003", RunState::Completed, None).await;

        assert!(list_now(&db).await.is_empty(), "没有人在等人判断");
        assert!(blocks_queue(RunState::Waiting), "等时钟的照样挡队");
        assert!(blocks_queue(RunState::Queued));
        assert!(!blocks_queue(RunState::Completed));
        assert!(!blocks_queue(RunState::Abandoned));
        assert!(!waits_for_human(&WaitReason::Approval {
            approval: "ap-1".into(),
        }));
        assert!(!waits_for_human(&WaitReason::Dependency {
            run: RunId::from_raw("run-0001"),
        }));
    }

    /// 过滤器按会话 / Run / 种类各筛各的。
    #[tokio::test]
    async fn the_list_can_be_filtered_by_session_run_and_kind() {
        let (db, _dir) = temp().await;
        waiting_on_a_person(&db, "sess-1", "run-0001").await;
        waiting_on_a_person(&db, "sess-2", "run-0002").await;

        let only_sess2 = list(
            &db,
            &InterventionListQuery {
                session: Some(SessionId::from_raw("sess-2")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(only_sess2.len(), 1);
        assert_eq!(only_sess2[0].handle, "run-0002");

        let only_run = list(
            &db,
            &InterventionListQuery {
                run: Some(RunId::from_raw("run-0001")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(only_run.len(), 1);

        let only_verify = list(
            &db,
            &InterventionListQuery {
                kind: Some(InterventionKind::Verify),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(only_verify.is_empty());
    }
}
