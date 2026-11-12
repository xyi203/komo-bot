//! 卡死检测（`docs/tasks.md` §4.9 B4、B5）：按"没有进展"判，不按总时长。
//!
//! 对象是分发派出的任务（`komo.task.reply` 在）一轮正在跑。**最后一次进展的时刻**取自 `komo.live.activityAt`
//! （runtime 在每次写 `komo.live` 时盖章：模型调用开始 / 完成、工具开始 / 结束、节流的进度提交都经过那里），
//! 判断只用这份已提交的事实与当前时钟，不靠进程内的计时器：重启后下一次扫描照样判。唯一的进程内量是
//! `started_at`——这一代 Gateway 起来的时刻，当作进展的下限，免得停机很久之后一起来就把所有在跑的任务停掉。
//!
//! 阈值是 `[execution] stall_model_secs`（两次模型调用之间，默认 450）与 `stall_tool_secs`（有工具在执行，
//! 默认 1200），各自 0 = 不检测。两段处理：
//!
//! - 到阈值的 80%：往任务里插一条提醒（[`TASK_STALL_ENTRY`]，内部输入，`steer`——下一个边界并进当前一轮），
//!   让它尽快收尾并回复。请求键带着当时的 `activityAt`，所以同一段无进展里重复扫描不会重复插；
//! - 到阈值仍无进展：落一句普通的话（线上的 `main.reply`，投给来源对端），再用 [`DurableCore::stop_task`]
//!   停掉。先说后停：每一步幂等（请求键同上），崩在两者之间下次扫描补完。
//!
//! 不算卡死：会话在等操作者（有未答的介入或 `confirm`，同任务清单的"等你"）、已被停下、没有在跑的一轮。
//! 提交里再核对一遍"还在跑、`activityAt` 没变"，扫描与提交之间任务有了进展就什么都不做。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use komo_kernel::durable::storage::{SessionCursor, SessionQuery};
use komo_kernel::durable::{Content, ScanOrder, SessionKind, SessionLifecycle, SubmissionId};
use komo_kernel::protocol::http::InterventionListQuery;
use komo_kernel::traits::Shutdown;
use komo_runtime::durable::harness::inbox::{Admitted, InputEntry};
use komo_runtime::durable::harness::orchestration::TASK_STALL_ENTRY;
use komo_runtime::durable::harness::{
    Harness, ReplyTo, SlotStatus, SubmissionDraft, WhenBusy, live, load_task_doc, task_doc,
};
use komo_runtime::durable::{DurableError, Tx};

use super::{DispatchInput, say};
use crate::service::durable::DurableCore;
use crate::service::durable::sessions::SessionError;
use crate::service::state::GatewayState;

/// 扫描间隔。阈值以分钟计，半分钟的粒度足够。
const TICK: Duration = Duration::from_secs(30);

/// 到阈值的这个比例（百分数）先提醒。
const REMIND_PCT: i64 = 80;

/// 卡死后往来源对端说的话。
const STALLED_REPLY: &str = "这件事卡住了，我先停下。要不要再试一次？";

/// 提醒的正文：任务会话里的模型读到的。
const REMINDER: &str = "（这条是 komo 自动发的，不是用户说的。）你已经很久没有新的进展了。请尽快收尾：\
手上的活做完就直接给出最后的回复；做不下去就说明卡在哪、缺什么，末行写「状态：受阻：原因」。\
再没有动静这件事会被停掉。";

/// 起扫描循环（`service::start`，在引擎恢复之后）。
pub fn spawn(state: &Arc<GatewayState>, shutdown: &Shutdown) {
    let weak = Arc::downgrade(state);
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        loop {
            let deadline = tokio::time::Instant::now() + TICK;
            while tokio::time::Instant::now() < deadline {
                if shutdown.is_cancelled() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            let Some(state) = weak.upgrade() else { return };
            if let Err(error) = sweep(&state).await {
                tracing::warn!(%error, "卡死检测这一轮没做成");
            }
        }
    });
}

fn millis(time: time::OffsetDateTime) -> i64 {
    (time.unix_timestamp_nanos() / 1_000_000) as i64
}

/// 把所有在跑的分发任务过一遍。
pub(crate) async fn sweep(state: &Arc<GatewayState>) -> Result<(), SessionError> {
    let execution = state.snapshot().execution.clone();
    if execution.stall_model_secs == 0 && execution.stall_tool_secs == 0 {
        return Ok(());
    }
    let core = state
        .core()
        .map_err(|error| SessionError::Internal(error.to_string()))?;
    let now = millis(state.clock.now());
    let floor = millis(state.started_at);
    // 在等操作者的任务不算卡死（与任务清单的"等你"同一个口径）。
    let asking: HashSet<String> = core
        .interventions(&InterventionListQuery::default())
        .await?
        .into_iter()
        .map(|summary| summary.session.as_str().to_owned())
        .collect();

    let query = SessionQuery {
        kind: Some(SessionKind::Task),
        lifecycle: Some(SessionLifecycle::Active),
        order: Some(ScanOrder::Descending),
        ..SessionQuery::default()
    };
    let mut cursor: Option<SessionCursor> = None;
    loop {
        let page = core
            .storage
            .scan_sessions(&query, 256, cursor.as_ref())
            .await?;
        for record in page.items {
            if asking.contains(&record.id) || record.parent_session.is_none() {
                continue;
            }
            let Some(doc) = task_doc(&*core.storage, record.root_conversation).await? else {
                continue;
            };
            if doc.reply.is_none() || doc.stopped {
                continue;
            }
            let live = Harness::live(&*core.storage, record.current_branch)
                .await
                .map_err(|error| SessionError::Internal(error.to_string()))?;
            let Some(run) = &live.run else { continue };
            // 有工具在执行（或排着马上执行）用工具的阈值，否则是两次模型调用之间。
            let in_tool = live
                .tools
                .iter()
                .flatten()
                .any(|slot| matches!(slot.status, SlotStatus::Pending | SlotStatus::Running));
            let limit_secs = if in_tool {
                execution.stall_tool_secs
            } else {
                execution.stall_model_secs
            };
            if limit_secs == 0 {
                continue;
            }
            // 旧 document 没有 `activityAt`：退到这一轮开始的时刻。
            let seen = live.activity_at.unwrap_or(run.started_at);
            let last = seen.max(floor);
            let idle = now - last;
            let limit = i64::try_from(limit_secs)
                .unwrap_or(i64::MAX)
                .saturating_mul(1000);
            if idle >= limit {
                stop(core, &record, seen).await?;
            } else if idle >= limit / 100 * REMIND_PCT {
                remind(core, &record, seen).await?;
            }
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(()),
        }
    }
}

/// 提交里的核对：这个会话还在跑、`activityAt` 还是扫描时看到的、没被停。不满足就什么都不做。
async fn still_stalled(
    tx: &mut Tx,
    conversation: komo_kernel::durable::ConversationId,
    root: komo_kernel::durable::ConversationId,
    seen: i64,
) -> Result<Option<(ReplyTo, komo_kernel::durable::ConversationId, SubmissionId)>, DurableError> {
    let live = live::load(tx, conversation).await?;
    let Some(run) = &live.run else {
        return Ok(None);
    };
    if live.activity_at.unwrap_or(run.started_at) != seen {
        return Ok(None);
    }
    let Some(first) = run.inputs.first().copied() else {
        return Ok(None);
    };
    let Some(doc) = load_task_doc(tx, root).await? else {
        return Ok(None);
    };
    if doc.stopped {
        return Ok(None);
    }
    Ok(doc.reply.map(|reply| (reply, doc.parent, first)))
}

async fn remind(
    core: &DurableCore,
    record: &komo_kernel::durable::SessionRecord,
    seen: i64,
) -> Result<(), SessionError> {
    let harness = core.harness.clone();
    let (conversation, root) = (record.current_branch, record.root_conversation);
    let sent = core
        .engine
        .commit(move |tx| {
            let harness = harness.clone();
            Box::pin(async move {
                if still_stalled(tx, conversation, root, seen).await?.is_none() {
                    return Ok(false);
                }
                let draft = SubmissionDraft::Input {
                    content: Content::Text(REMINDER.to_owned()),
                    entry: Some(InputEntry::new(TASK_STALL_ENTRY, None)),
                    when_busy: WhenBusy::Steer,
                    // 同一段无进展里同一个键：重复扫描不重复插。
                    request_id: Some(format!("stall-remind:{root}:{seen}")),
                    internal: true,
                };
                Ok(!matches!(
                    harness.admit_in_tx(tx, conversation, &draft).await?,
                    Admitted::Busy
                ))
            })
        })
        .await?;
    if sent {
        tracing::info!(task = %root, "任务很久没有进展，已提醒它尽快收尾");
        core.resume();
    }
    Ok(())
}

async fn stop(
    core: &DurableCore,
    record: &komo_kernel::durable::SessionRecord,
    seen: i64,
) -> Result<(), SessionError> {
    let harness = core.harness.clone();
    let (conversation, root) = (record.current_branch, record.root_conversation);
    let parent_session = record.parent_session.clone();
    let spoken = core
        .engine
        .commit(move |tx| {
            let harness = harness.clone();
            let parent_session = parent_session.clone();
            Box::pin(async move {
                // 表读先于第一次表写：父会话已经在删就不往它的线上说话（停还是要停）。
                let parent_alive = match &parent_session {
                    Some(id) => tx
                        .session(id)
                        .await?
                        .is_some_and(|parent| parent.lifecycle == SessionLifecycle::Active),
                    None => false,
                };
                let Some((reply, parent, first)) =
                    still_stalled(tx, conversation, root, seen).await?
                else {
                    return Ok(false);
                };
                if let (true, Some(session)) = (parent_alive, parent_session) {
                    let dispatch = DispatchInput {
                        session,
                        conversation: parent,
                        submission: reply.origin_submission,
                        peer: reply.peer.clone(),
                    };
                    say(
                        tx,
                        &harness,
                        &dispatch,
                        &reply,
                        format!("stall-stop:{root}:{first}"),
                        STALLED_REPLY,
                    )
                    .await?;
                }
                Ok(true)
            })
        })
        .await?;
    if spoken {
        tracing::warn!(task = %root, "任务一直没有进展，判为卡死，已停下");
        core.stop_task(root).await?;
        core.resume();
    }
    Ok(())
}
