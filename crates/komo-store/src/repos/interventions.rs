//! §7.5 Intervention：一切"需要人判断"的同一个入口。
//!
//! 审批、结果不明、阻塞今天本该是三套互不相干的出口——审批有短 ID、有清单，`needs_attention`
//! 只有一条事件与一句投递。操作者因此撞上最难解释的现象：**会话停着不动，而 `komo approval
//! list` 是空的**。这里把它们合成一条**派生清单**：
//!
//! - **清单不是第四张表**（第 1 条）。权威仍是 `runs` 与 `approval_requests`，`kind` 由
//!   "有没有一条 `uncertain` 调用"当场判出。多一张 `interventions` 表就多一处会与权威漂移
//!   的状态，而这次改造的全部理由就是不要那个（§8.9）。
//! - **挡队的每一条都在清单里，而且是同一次判定**（第 2 条）。[`waits_for_human`] 回答
//!   "进清单"，[`blocks_queue`] 回答"挡不挡队"，两者都在这里，由同一份状态派生；§8.7 的
//!   领取语句与 reconcile 的 `closing → deleted` 用的是同一份判定。
//! - **结论只走一条路：核对 → 按 §8.4 行事**（第 3 条）。这个模块只回答"有什么在等人"，
//!   答复的副作用由 runtime 落地。

use std::collections::{BTreeMap, BTreeSet};

use komo_kernel::protocol::http::{
    InterventionDetail, InterventionKind, InterventionListQuery, InterventionSummary,
};
use komo_kernel::traits::StoreError;
use komo_kernel::types::ids::{RunId, SessionId, ShortId, ToolCallId};
use komo_kernel::types::plan::PlanHash;
use komo_kernel::types::status::{RunState, WaitReason};
use time::OffsetDateTime;
use toasty::Executor;

use crate::db::{BoxFuture, Db, from_ts, map_toasty};
use crate::models::{ApprovalRequestRow, RunRow, ToolCallRow};
use crate::repos::approvals::record_from_row;
use crate::repos::runs::{state_of, wait_of};

/// §7.5 第 2 条：这条 Run **停在人身上**吗——清单列的就是它，不需要第二个条件。
///
/// 等时钟（`wait_kind = 'retry'`）不算：它在等时钟，不是等人。等依赖
/// （`wait_kind = 'dependency'`）也不算：它在等前一条 Run 跑完，那件事不需要人回答。
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

/// 待处理清单（§7.5）：`approval_requests` 与"停在人身上的 Run"的并集查询。
///
/// `kind` 当场判：有 `uncertain` 调用 → `Verify`；否则 → `Blocked`。**审批类由权威行表达**
/// ——`wait_kind = 'approval'` 的 Run，如果 `approval_requests` 里有待处理行，那一条以短 ID
/// 出现在清单里（Run 那一侧跳过，所以同一条 Run 不会既以短 ID 又以 Run ID 出现两次）；
/// 权威行缺失时**照样列出来**（它确实挡着队），但记成 `Blocked` 并写清"没有这条审批请求"
/// ——§7.5 第 3 条那句"没有'我说它发生了'"的另一面就是"没有权威行就不能造一条答复出来"。
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
            let approvals = pending_approvals_in(ex).await?;
            let blocked = blocked_runs_in(ex).await?;

            let mut out = Vec::with_capacity(approvals.len() + blocked.len());
            // 已经被审批那一条表达过的 Run：正文与句柄都在审批行上（短 ID 才是它的句柄），
            // 所以 Run 那一侧不再重复列。
            let mut expressed: BTreeSet<String> = BTreeSet::new();
            for row in &approvals {
                if let Some(run) = &row.run_id {
                    expressed.insert(run.clone());
                }
                out.push(approval_summary(row));
            }
            for entry in &blocked {
                if expressed.contains(entry.run.as_str()) {
                    continue;
                }
                out.push(run_summary(entry));
            }

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

/// 按句柄取一条：先按短 ID 找**待处理**审批（§11.3），再按 Run ID 找 `verify` / `blocked`。
///
/// `None` = 这个句柄此刻没有任何东西在等人（已经答过的审批、已经跑完的 Run 都是这样）。
pub async fn find(db: &Db, handle: &str) -> Result<Option<InterventionDetail>, StoreError> {
    let handle = handle.to_string();
    db.read(move |ex| {
        let handle = handle.clone();
        Box::pin(async move {
            if let Some(short) = ShortId::parse(&handle) {
                let rows = ApprovalRequestRow::filter(
                    ApprovalRequestRow::fields()
                        .decided()
                        .eq(false)
                        .and(ApprovalRequestRow::fields().short_id().eq(short.as_str())),
                )
                .exec(ex)
                .await
                .map_err(map_toasty)?;
                if let Some(row) = rows.first() {
                    return Ok(Some(InterventionDetail::Approval(Box::new(
                        record_from_row(row)?,
                    ))));
                }
            }

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
            // 等审批的 Run 用 Run ID 来找也一样：**权威是审批行**，有那一条就把它交出去
            // （句柄仍是短 ID）。少了这一步，同一个问题会按句柄给出两种答案——短 ID 说
            // "批这份计划"，Run ID 却说"没有那条审批请求"。
            if matches!(wait, WaitReason::Approval { .. })
                && let Some(approval) = pending_approval_for_run_in(ex, &row.id).await?
            {
                return Ok(Some(InterventionDetail::Approval(Box::new(
                    record_from_row(&approval)?,
                ))));
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

async fn pending_approvals_in(
    ex: &mut dyn Executor,
) -> Result<Vec<ApprovalRequestRow>, StoreError> {
    let mut rows = ApprovalRequestRow::filter(ApprovalRequestRow::fields().decided().eq(false))
        .exec(ex)
        .await
        .map_err(map_toasty)?;
    rows.sort_by(|a, b| a.requested_at.cmp(&b.requested_at).then(a.id.cmp(&b.id)));
    Ok(rows)
}

/// 这条 Run 上**待处理**的那条审批（权威行）。`None` = 没有——那是损坏的形状。
async fn pending_approval_for_run_in(
    ex: &mut dyn Executor,
    run: &str,
) -> Result<Option<ApprovalRequestRow>, StoreError> {
    let mut rows = ApprovalRequestRow::filter(
        ApprovalRequestRow::fields()
            .decided()
            .eq(false)
            .and(ApprovalRequestRow::fields().run_id().eq(run)),
    )
    .exec(ex)
    .await
    .map_err(map_toasty)?;
    rows.sort_by(|a, b| a.requested_at.cmp(&b.requested_at).then(a.id.cmp(&b.id)));
    Ok(rows.into_iter().next())
}

/// 审批那一类的清单项：句柄是短 ID，问题是这份计划要放行什么（§7.5）。
fn approval_summary(row: &ApprovalRequestRow) -> InterventionSummary {
    InterventionSummary {
        handle: row.short_id.clone(),
        kind: InterventionKind::Approval,
        session: SessionId::from_raw(row.session_id.clone()),
        run: row.run_id.clone().map(RunId::from_raw),
        call: row.call_id.clone().map(ToolCallId::from_raw),
        question: row.reason.clone(),
        verdicts: InterventionKind::Approval.verdicts(),
        created_at: from_ts(row.requested_at),
    }
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
        // 状态说在等审批，而权威行不在：**它确实挡着队**，所以列出来；但答复没有着落，
        // 只能记成阻塞并说清缺了什么（§7.5 第 3 条）。
        WaitReason::Approval { approval } => (
            InterventionKind::Blocked,
            format!(
                "状态说这条 Run 在等审批，但数据库里没有待处理的那条审批请求（{}）：\
                 先查它为什么丢了，再决定怎么处置",
                approval
            ),
        ),
        // 构造这个类型时已经按 `needs_a_person` 收过一遍，这两种到不了；真出现了也只能
        // 当阻塞报出去——不 pretend 它不是问题。
        WaitReason::Retry { .. } | WaitReason::Dependency { .. } => {
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

    /// 一条停在审批上的 Run（`waiting + approval`，权威行另建）。
    async fn waiting_on_approval(db: &Db, session: &str, run: &str, approval: &str) {
        run_row(
            db,
            session,
            run,
            RunState::Waiting,
            Some(WaitReason::Approval {
                approval: komo_kernel::types::ids::ApprovalId::from_raw(approval),
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

    /// 一条待处理审批：句柄是短 ID。正文是**一份真的执行计划**——`find` 要把整份
    /// `ApprovalRecord` 交给界面（§7.2），计划解不出来就是损坏。
    async fn pending_approval(db: &Db, id: &str, short: &str, session: &str, run: &str) {
        let plan = serde_json::to_string(&komo_kernel::test_support::sample_plan(
            "shell",
            &SessionId::from_raw(session),
        ))
        .expect("计划序列化得出来");
        let (id, short, session, run) = (
            id.to_string(),
            short.to_string(),
            session.to_string(),
            run.to_string(),
        );
        write(db, move |ex| {
            let (id, short, session, run, plan) = (
                id.clone(),
                short.clone(),
                session.clone(),
                run.clone(),
                plan.clone(),
            );
            Box::pin(async move {
                toasty::create!(ApprovalRequestRow {
                    id,
                    short_id: short,
                    session_id: session,
                    run_id: Some(run),
                    call_id: None as Option<String>,
                    plan_hash: "ph",
                    plan,
                    reason: "要放行一条 shell 命令",
                    changes: None as Option<String>,
                    evidence: None as Option<String>,
                    scopes: "[]",
                    requested_at: 1_i64,
                    valid_until: 0_i64,
                    decided: false,
                    decision: None as Option<String>,
                })
                .exec(ex)
                .await
                .map_err(map_toasty)?;
                Ok(())
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
    /// 一条等审批的（由待处理审批行表达，句柄是短 ID）、一条停在人身上的（Run ID 做句柄）、
    /// 一条同 Session 的 `queued`——正好两条在清单里，而且"挡队"与"进清单"同源：那个被
    /// 挡住的 `queued` Run 领不走，挡住它的那两条都在清单里。
    #[tokio::test]
    async fn every_run_stopped_on_a_human_appears_exactly_once() {
        let (db, _dir) = temp().await;
        waiting_on_approval(&db, "sess-1", "run-0001", "appr-1").await;
        waiting_on_a_person(&db, "sess-1", "run-0002").await;
        set_last_error(&db, "run-0002", "引用的计划文件已经是另一份了").await;
        run_row(&db, "sess-1", "run-0003", RunState::Queued, None).await;
        pending_approval(&db, "appr-1", "AAAA", "sess-1", "run-0001").await;

        let pending = list_now(&db).await;
        assert_eq!(pending.len(), 2, "审批一条 + 停滞一条：{pending:#?}");

        let approval = pending
            .iter()
            .find(|entry| entry.kind == InterventionKind::Approval)
            .expect("审批那一条在");
        assert_eq!(approval.handle, "AAAA", "审批的句柄是短 ID");
        assert_eq!(approval.run, Some(RunId::from_raw("run-0001")));
        assert_eq!(approval.question, "要放行一条 shell 命令");

        let blocked = pending
            .iter()
            .find(|entry| entry.kind == InterventionKind::Blocked)
            .expect("阻塞那一条在");
        assert_eq!(blocked.handle, "run-0002", "另外两类的句柄是 Run ID");
        assert_eq!(blocked.question, "引用的计划文件已经是另一份了");
        assert_eq!(
            blocked.verdicts,
            InterventionKind::Blocked.verdicts(),
            "清单项自带答案菜单"
        );
        assert!(blocked.verdicts.contains(&InterventionVerdict::Resolve));

        // 每一条 Run 在清单里恰好出现一次（`run-0003` 是排队中，不在清单里）。
        for run in ["run-0001", "run-0002"] {
            let seen = pending
                .iter()
                .filter(|entry| entry.run.as_ref().map(RunId::as_str) == Some(run))
                .count();
            assert_eq!(seen, 1, "{run} 恰好出现一次");
        }

        // 挡队与进清单同源：`run-0003` 领不走，而挡住它的两条都在清单里。
        let queue = queue::TursoRunQueue::new(db.clone());
        assert!(
            queue
                .claim(&ExecutorId::from_raw("exec-1"))
                .await
                .unwrap()
                .is_none(),
            "前面的 Run 停止等人，后面的不许领"
        );
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

    /// §7.5 第 3 条：**审批的权威是审批行。** 状态说在等审批而权威行不在时，仍然列出来
    /// （它挡着队），但记成 `blocked`、句柄是 Run ID——不许凭空造一条答复。
    #[tokio::test]
    async fn an_approval_wait_without_the_authority_row_is_blocked_not_approved() {
        let (db, _dir) = temp().await;
        waiting_on_approval(&db, "sess-1", "run-0001", "appr-1").await;

        let pending = list_now(&db).await;
        assert_eq!(pending.len(), 1, "缺权威行也要列出来：它挡着队");
        assert_eq!(pending[0].kind, InterventionKind::Blocked);
        assert_eq!(pending[0].handle, "run-0001");
        assert!(
            pending[0].question.contains("没有待处理的那条审批请求"),
            "说清缺了什么：{}",
            pending[0].question
        );
        assert!(
            !pending[0].verdicts.contains(&InterventionVerdict::Approve),
            "没有权威行就没有可批准的对象"
        );
        // 按 Run ID 取也一样：没有权威行，绝不编一条"审批"出来。
        match find(&db, "run-0001").await.unwrap() {
            Some(InterventionDetail::Blocked { reason, .. }) => {
                assert!(reason.contains("没有待处理的那条审批请求"), "{reason}");
            }
            other => panic!("该是 blocked：{other:?}"),
        }
    }

    /// 按短 ID 取详情拿到整份审批记录（§7.2 要界面显示的全部内容）。
    #[tokio::test]
    async fn finding_an_approval_by_short_id_returns_the_record() {
        let (db, _dir) = temp().await;
        waiting_on_approval(&db, "sess-1", "run-0001", "appr-1").await;
        pending_approval(&db, "appr-1", "AAAA", "sess-1", "run-0001").await;

        match find(&db, "aaaa").await.unwrap() {
            Some(InterventionDetail::Approval(record)) => {
                assert_eq!(record.short_id, ShortId::parse("AAAA").unwrap());
                assert_eq!(record.session, SessionId::from_raw("sess-1"));
                assert_eq!(record.run, Some(RunId::from_raw("run-0001")));
                assert_eq!(record.reason, "要放行一条 shell 命令");
            }
            other => panic!("该是审批：{other:?}"),
        }
        // **两个句柄指向同一条**：按 Run ID 找回来的还是那份权威审批，而不是"缺行"。
        match find(&db, "run-0001").await.unwrap() {
            Some(InterventionDetail::Approval(record)) => {
                assert_eq!(record.short_id, ShortId::parse("AAAA").unwrap());
            }
            other => panic!("按 Run ID 也该拿到那次审批：{other:?}"),
        }
        // 句柄不是审批也不在等人 → 没有这一条。
        assert!(find(&db, "ZZZZ").await.unwrap().is_none());
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
        assert!(waits_for_human(&WaitReason::Approval {
            approval: komo_kernel::types::ids::ApprovalId::from_raw("ap-1"),
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

        let only_approval = list(
            &db,
            &InterventionListQuery {
                kind: Some(InterventionKind::Approval),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(only_approval.is_empty());
    }
}
