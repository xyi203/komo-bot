//! 一个 Run 跑起来之后**谁在看**（§11.4）。
//!
//! 每个 Run 一个订阅者，按来源分流：
//!
//! ```text
//! 交互（聊天 / TUI / HTTP）  终态 → 回来源会话；等待审批 / 需要处理 → 来源会话 + home chat
//! Cron（一次触发）          终态 → 按 notify 投 home chat 并回写触发状态；等待审批 → home chat
//! ```
//!
//! **两种来源共用这一个循环**，因为它们的分别只有一条：谁是"来源会话"。§11.4 把这条
//! 写成了一句话——「审批请求的投递目标：Run 的来源会话，**加上** home chat（若不同）。
//! 来源是 Cron 或已断开的 TUI 时只有 home chat」——所以来源是 `Option<ChannelPeer>`，
//! 不是两条代码路径。
//!
//! **一个 Run 只有一个看的人**：`GatewayState::watching` 是那张登记表。没有它，Cron 的
//! 看客与交互的看客会各投一遍同一条审批，操作者收到两张一模一样的卡（去重键那一层管
//! 的是平台重投，管不到这个）。
//!
//! 它**不是**恢复机制：进程活着的时候订阅事件流，重启后靠 `deliveries` 表里那一行
//! pending 补发（§11.4），不靠这里再看一遍。

use std::sync::Arc;

use komo_kernel::events::EventPayload;
use komo_kernel::types::chat::{ChannelPeer, DeliveryTarget, Outbound};
use komo_kernel::types::ids::{ApprovalId, RunId, SessionId};

use super::cron_watch::Watched;
use super::state::GatewayState;

/// 这个 Run 是谁的。
#[derive(Debug, Clone)]
pub enum Watcher {
    /// 交互：来源会话。聊天渠道有，TUI / HTTP 没有——没有就只投 home chat。
    Interactive { peer: Option<ChannelPeer> },
    /// 一次 Cron 触发。终态要按 `notify` 过滤，并回写触发状态（§10）。
    Cron(Box<Watched>),
}

impl Watcher {
    /// 来源会话（§11.4 的"Run 的来源会话"那一半）。
    fn peer(&self) -> Option<&ChannelPeer> {
        match self {
            Watcher::Interactive { peer } => peer.as_ref(),
            // 「来源是 Cron 或已断开的 TUI 时只有 home chat」。
            Watcher::Cron(_) => None,
        }
    }

    fn label(&self) -> Option<&str> {
        match self {
            Watcher::Interactive { .. } => None,
            Watcher::Cron(watched) => Some(&watched.name),
        }
    }
}

/// 盯这个 Run，直到它有结论。**同一个 Run 只盯一次。**
pub fn watch(state: Arc<GatewayState>, session: SessionId, run: RunId, watcher: Watcher) {
    if !state.start_watching(&run) {
        tracing::debug!(%run, "这个 Run 已经有人在看了");
        return;
    }
    let mut events = state.hub.subscribe(&session);
    tokio::spawn(async move {
        loop {
            let frame = match events.recv().await {
                Ok(frame) => frame,
                // 落后了就接着收：内存通知丢了不丢数据（§13.1），状态还有周期扫描兜底。
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            let komo_kernel::protocol::sse::SseEvent::Event(event) = &frame.event else {
                continue;
            };
            if event.run.as_ref() != Some(&run) {
                continue;
            }
            if step(&state, &session, &run, &watcher, &event.payload).await == Step::Done {
                break;
            }
        }
        state.stop_watching(&run);
    });
}

/// 这一条事件之后还要不要接着看。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Keep,
    Done,
}

async fn step(
    state: &Arc<GatewayState>,
    session: &SessionId,
    run: &RunId,
    watcher: &Watcher,
    payload: &EventPayload,
) -> Step {
    match payload {
        // **等待审批**：先投出去，再回写触发状态。投递在前是因为这一条是给人看的，
        // 而状态是给清单看的。
        //
        // 「新增危险操作暂停等待审批，不能因无人值守而自动放行」（§10）——没有这一步，
        // Run 会停在等待上而**没有人知道它在等**。
        EventPayload::RunWaitingApproval(body) => {
            deliver_approval(state, watcher, &body.approval).await;
            if let Watcher::Cron(watched) = watcher {
                super::cron_watch::settle(
                    state,
                    watched,
                    komo_kernel::cron::FiringStatus::Waiting,
                    None,
                )
                .await;
            }
            // 不 return：批准之后这个 Run 会接着跑，终态还要记。
            Step::Keep
        }
        // 「需要操作者判断」（§8.6）与等待审批同一类：它在**问**，所以不受 `notify`
        // 约束，也一样投到来源会话 + home chat。
        EventPayload::RunNeedsAttention(body) => {
            let reason = match watcher.label() {
                Some(name) => format!("定时任务「{name}」：{}", body.reason),
                None => body.reason.clone(),
            };
            let message = Outbound::NeedsAttention {
                session: session.clone(),
                run: run.clone(),
                reason,
            };
            deliver_to_source_and_home(state, watcher, message).await;
            if let Watcher::Cron(watched) = watcher {
                super::cron_watch::settle(
                    state,
                    watched,
                    komo_kernel::cron::FiringStatus::Waiting,
                    Some(body.reason.clone()),
                )
                .await;
            }
            Step::Done
        }
        EventPayload::RunCompleted(body) => {
            let text = body
                .final_message
                .clone()
                .unwrap_or_else(|| "（这一轮没有文字回复）".to_string());
            finish(
                state,
                session,
                run,
                watcher,
                komo_kernel::cron::FiringStatus::Ok,
                None,
                text,
            )
            .await;
            Step::Done
        }
        EventPayload::RunFailed(body) => {
            finish(
                state,
                session,
                run,
                watcher,
                komo_kernel::cron::FiringStatus::Error,
                Some(body.reason.clone()),
                format!("任务失败：{}", body.reason),
            )
            .await;
            Step::Done
        }
        EventPayload::RunCancelled(_) => {
            finish(
                state,
                session,
                run,
                watcher,
                komo_kernel::cron::FiringStatus::Error,
                Some("已取消".to_string()),
                "任务已取消。".to_string(),
            )
            .await;
            Step::Done
        }
        _ => Step::Keep,
    }
}

/// 收场：回写触发状态（Cron），把结果发回去。
async fn finish(
    state: &Arc<GatewayState>,
    session: &SessionId,
    run: &RunId,
    watcher: &Watcher,
    status: komo_kernel::cron::FiringStatus,
    error: Option<String>,
    text: String,
) {
    match watcher {
        Watcher::Interactive { peer } => {
            let Some(peer) = peer else {
                // TUI / HTTP 自己在看事件流（SSE），不必再投一条（§13.1）。
                return;
            };
            if let Err(error) = state
                .notifier
                .log()
                .deliver(
                    &DeliveryTarget::to_peer(peer.clone()),
                    Outbound::Text { text },
                )
                .await
            {
                tracing::warn!(%error, %run, "回复投不出去");
            }
        }
        Watcher::Cron(watched) => {
            super::cron_watch::settle(state, watched, status, error).await;
            // 取消不报告：那是人自己按下的。
            if status != komo_kernel::cron::FiringStatus::Error || watched.notify.delivers(status) {
                super::cron_watch::notify(state, watched, status, session, run, &text).await;
            }
        }
    }
}

/// 把这条审批投到**来源会话 + home chat**（§11.4）。
///
/// 已经决定过的不再问一次；投不出去（一个 home_chat 都没配、来源会话也没有）**报到
/// 日志**，不静默丢弃——一个没人能回答的等待会让这个 Run 从此停在那里。
async fn deliver_approval(state: &Arc<GatewayState>, watcher: &Watcher, approval: &ApprovalId) {
    if !state.start_delivering_approval(approval) {
        tracing::debug!(%approval, "这条审批已经投过了");
        return;
    }
    let record = match state.approval_repo.get(approval).await {
        Ok(Some(record)) => record,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, %approval, "读不出这条审批，投不出去");
            return;
        }
    };
    if record.decision.is_some() {
        return; // 已经决定过了，不必再问一次。
    }
    if let Err(error) = state
        .notifier
        .deliver_approval(
            watcher.peer(),
            komo_runtime::approvals::presentation(&record),
        )
        .await
    {
        tracing::warn!(%error, %approval, "审批请求投不出去：没人能回答它");
    }
}

/// 来源会话 + home chat，两处都投（§11.4）。home chat 与来源会话是同一个时只投一次。
async fn deliver_to_source_and_home(
    state: &Arc<GatewayState>,
    watcher: &Watcher,
    message: Outbound,
) {
    let mut delivered_to_source = false;
    if let Some(peer) = watcher.peer() {
        let is_home = state
            .notifier
            .home_targets()
            .iter()
            .any(|target| &target.peer == peer);
        if !is_home
            && let Err(error) = state
                .notifier
                .log()
                .deliver(&DeliveryTarget::to_peer(peer.clone()), message.clone())
                .await
        {
            tracing::warn!(%error, "投不到来源会话");
        }
        delivered_to_source = !is_home;
    }
    if let Err(error) = state.notifier.deliver_home(message).await {
        if !delivered_to_source {
            tracing::warn!(%error, "没有 home chat 也没有来源会话，这条没人看得见");
        } else {
            tracing::debug!(%error, "没有 home chat，只投了来源会话");
        }
    }
}
