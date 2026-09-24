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
use komo_kernel::traits::Ledger;
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

/// 重启后把未终态、且来自聊天的交互 Run 重新挂上看客（`docs/home-dispatcher.md` §8
/// Fix 1）。
///
/// 看客只活在内存里：`GatewayState::watching` 与这里的 `tokio::spawn` 都在进程重启时
/// 清空，而账本上这条 Run 该往哪投（`runs.peer`）一直都在。少了这一步，一条跨重启还在
/// 跑的交互 Run 跑完之后没有人订阅它的终态帧，最终回复就再也投不出去——审批有周期兜底
/// 补投（[`GatewayState::sweep_unseen_interventions`]），最终回复原来没有，这里补上。
///
/// **必须在调度器起来之前调用**：调度器一起，这些 Run 可能立刻继续往下跑，慢一步挂上
/// 看客就会真的错过它的终态帧。
pub async fn reattach_unfinished(state: &Arc<GatewayState>) {
    let unfinished = match komo_store::repos::runs::unfinished(&state.db).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "读不出未终态 Run，重启后的看客补挂没做成");
            return;
        }
    };
    let mut reattached = 0usize;
    for record in unfinished {
        // 没有 peer = TUI / HTTP，或者是委派的子 Run（永远 `peer = None`）：那条路本来
        // 就不靠这里补——TUI / HTTP 自己在看事件流，子 Run 的结果回到父 Run 自己的收尾。
        let Some(raw) = record.peer.as_deref() else {
            continue;
        };
        let Some(peer) = ChannelPeer::parse(raw) else {
            tracing::warn!(
                run = %record.run,
                peer = raw,
                "runs.peer 解析不出来，这条 Run 的看客补不上"
            );
            continue;
        };
        let watcher = Watcher::Interactive { peer: Some(peer) };
        // 订阅之前先核对一遍权威状态：这条 Run 有没有可能在 `unfinished()` 这份名单读
        // 出来之后、真的订阅之前，就已经落了终态——启动阶段调度器还没起来，这一步理论
        // 上总是"还没有"，但核对本身几乎不花时间，换来的是不必靠"调用次序恰好对"这件
        // 事成立（本函数的文档：必须在调度器起来之前调用）。
        if deliver_if_already_finished(state, &record.session, &record.run, &watcher).await {
            continue;
        }
        watch(Arc::clone(state), record.session, record.run, watcher);
        reattached += 1;
    }
    if reattached > 0 {
        tracing::info!(reattached, "重启后补挂了聊天来源的看客");
    }
}

/// [`reattach_unfinished`] 补挂之前的核对：这条 Run 是不是已经落了终态。命中就直接按
/// 落盘的终态收尾（复用 [`finish`]，与"跑完那一刻"同一份口径）并返回 `true`；调用方就
/// 不必再订阅一次——那一帧广播可能已经错过了，订阅了也是白等。
///
/// 走的是 [`Ledger::run_end`]，不是重新扫一遍 JSONL：它已经把外置的大正文按引用读回来了
/// （§8.3），返回 `None` 就是这条 Run 确实还没到终态。
async fn deliver_if_already_finished(
    state: &Arc<GatewayState>,
    session: &SessionId,
    run: &RunId,
    watcher: &Watcher,
) -> bool {
    use komo_kernel::cron::FiringStatus;
    use komo_kernel::types::status::RunEnd;

    let end = match state.routed.run_end(run).await {
        Ok(Some(end)) => end,
        Ok(None) => return false,
        Err(error) => {
            tracing::warn!(%error, %run, "读不出这条 Run 的终态，补挂照常订阅");
            return false;
        }
    };
    let (status, error, text) = match end {
        RunEnd::Completed { final_message, .. } => (
            FiringStatus::Ok,
            None,
            final_message.unwrap_or_else(|| "（这一轮没有文字回复）".to_string()),
        ),
        RunEnd::Failed { reason } => (
            FiringStatus::Error,
            Some(reason.clone()),
            format!("任务失败：{reason}"),
        ),
        RunEnd::Cancelled { .. } => (
            FiringStatus::Error,
            Some("已取消".to_string()),
            "任务已取消。".to_string(),
        ),
        RunEnd::Abandoned { reason, .. } => {
            let why = reason.unwrap_or_else(|| "操作者放弃".to_string());
            (
                FiringStatus::Error,
                Some(why.clone()),
                format!("任务已放弃（abandoned）：{why}"),
            )
        }
    };
    finish(state, session, run, watcher, status, error, text, true).await;
    true
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
        // **停在一个外部条件上**（§8.4）：一个事件（`run.waiting`）加一个理由。投不投、
        // 投到哪儿，由理由决定——"需要人判断"的那两类要投（§11.4），等时钟与等前一条
        // Run 不投（那不是问人）。
        //
        // 「新增危险操作暂停等待审批，不能因无人值守而自动放行」（§10）——少了投递这一步，
        // Run 会停在等待上而**没有人知道它在等**。
        EventPayload::RunWaiting(body) => {
            match &body.reason {
                komo_kernel::types::status::WaitReason::Approval { approval } => {
                    deliver_approval(state, watcher, approval).await;
                }
                // 结果不明 / 前提没了（§7.5 的另外两类）：同一句话问人，投**来源会话 +
                // home chat**。"需要人判断"不该因为种类不同而有不同的到达率（§11.4）。
                komo_kernel::types::status::WaitReason::Intervention { .. } => {
                    let question = question_for(state, run, watcher).await;
                    deliver_to_source_and_home(
                        state,
                        watcher,
                        Outbound::NeedsAttention {
                            session: session.clone(),
                            run: run.clone(),
                            reason: question,
                        },
                    )
                    .await;
                }
                // 等时钟（`retry`）与等另一条 Run（`dependency`）：不是在等人，不投。
                komo_kernel::types::status::WaitReason::Retry { .. }
                | komo_kernel::types::status::WaitReason::Dependency { .. } => {}
            }
            // **让出名额之后立刻再看一眼队列。** 一条 Run 停在等待上就不再占并发位，
            // 而队列里可能有本来领不走的东西：同会话次序守卫挡着的下一条，或者一条刚被
            // 受理的子 Run——挡住它的正是它自己的父，而父此刻刚刚停下（§4、§8.4）。
            // 不叫这一声，那些要一直等到下一次周期扫描。
            state.waker().wake();
            if let Watcher::Cron(watched) = watcher {
                super::cron_watch::settle(
                    state,
                    watched,
                    komo_kernel::cron::FiringStatus::Waiting,
                    None,
                )
                .await;
            }
            // 不 return：答复 / 到点 / 前一条终态之后这个 Run 会接着跑，终态还要记。
            Step::Keep
        }
        EventPayload::RunCompleted(body) => {
            // 命令 Job（§10）：ok / error 与投递正文从那次 `shell` 调用的落盘结果现读，
            // 不信 `final_message`——`CommandDriver` 收尾用的是投影过的正文，不是干净
            // 的 stdout，也分不出退出码是不是 0（它永远正常收尾，见 `command_driver`
            // 模块文档「工具失败是结果，驱动失败是终止」）。
            if let Watcher::Cron(watched) = watcher
                && watched.is_command
            {
                let (status, text, deliver) = command_outcome(state, session, run).await;
                finish(state, session, run, watcher, status, None, text, deliver).await;
                return Step::Done;
            }
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
                true,
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
                true,
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
                true,
            )
            .await;
            Step::Done
        }
        // 操作者在清单上放弃（§7.5）：**与取消分得开**——不是用户不想跑了，而是这件事
        // 不会再有下文。话也要说得不一样，否则那个 Run 看起来像是被人取消的。
        EventPayload::RunAbandoned(body) => {
            let why = body
                .reason
                .clone()
                .unwrap_or_else(|| "操作者放弃".to_string());
            finish(
                state,
                session,
                run,
                watcher,
                komo_kernel::cron::FiringStatus::Error,
                Some(why.clone()),
                format!("任务已放弃（abandoned）：{why}"),
                true,
            )
            .await;
            Step::Done
        }
        _ => Step::Keep,
    }
}

/// 命令 Job 的 firing 结果：`(状态, 投递正文, 要不要投递)`。
///
/// - 退出 0、stdout 非空：`Ok`，正文是 trim 过的 stdout **原样**，要投递。
/// - 退出 0、stdout 为空：`Ok`，不投递——看门狗"没事不说话"（§10）。
/// - 非 0 退出：`Error`，正文是退出码 + stderr 末尾若干行，要投递（受 `notify` 过滤）。
/// - 结果读不出来（落盘的东西对不上、还没写完）：留一句说得清出处的话，仍然投递——
///   静默好过一句假话，但也不该真的什么都不说。
async fn command_outcome(
    state: &Arc<GatewayState>,
    session: &SessionId,
    run: &RunId,
) -> (komo_kernel::cron::FiringStatus, String, bool) {
    use komo_kernel::cron::FiringStatus;
    let Some(result) = shell_result_of(state, session, run).await else {
        return (
            FiringStatus::Error,
            "命令的执行结果读不出来，去原 Session 看细节（§10）".to_string(),
            true,
        );
    };
    if result.exit_code == Some(0) {
        let stdout = result.stdout_tail.trim();
        if stdout.is_empty() {
            (FiringStatus::Ok, String::new(), false)
        } else {
            (FiringStatus::Ok, stdout.to_string(), true)
        }
    } else {
        let code = result
            .exit_code
            .map_or_else(|| "?".to_string(), |code| code.to_string());
        let stderr = tail_lines(&result.stderr_tail, 20);
        (FiringStatus::Error, format!("exit={code}\n{stderr}"), true)
    }
}

/// 这个 Run 唯一那次 `shell` 调用的落盘结果（§10：命令 Job 全程只发那一个调用）。
///
/// 从这一段的 Surface 里找那条工具结果的 `output` 引用，再按引用读回 `output.json`——
/// 与 `service::segment` 的 `replay` 读法同一条路（§8.3），只是这里要的是**结构化**的
/// `ShellResult`（`body.result`），不是投影过的模型可读正文。
async fn shell_result_of(
    state: &Arc<GatewayState>,
    session: &SessionId,
    run: &RunId,
) -> Option<komo_runtime::tools::shell::ShellResult> {
    let mut events = Vec::new();
    let mut from = komo_kernel::types::ids::Seq::ZERO;
    loop {
        let batch = state.routed.read(session, from, 0).await.ok()?;
        if batch.events.is_empty() {
            break;
        }
        for event in batch.events {
            from = from.max(event.seq);
            events.push(event);
        }
        if batch.next.is_none() {
            break;
        }
    }
    let surface = komo_kernel::fold::fold(&events);
    let output = surface.messages.iter().find_map(|message| {
        if message.run.as_ref() != Some(run) {
            return None;
        }
        message
            .tool_results
            .first()
            .map(|result| result.output.clone())
    })?;
    let verified = state.outputs.open(&output).await.ok()?;
    serde_json::from_value(verified.body.result).ok()
}

/// 正文的最后 `n` 行——错误摘要用它，别把一屏 stderr 整段糊给操作者。
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// 一条"需要人判断"的通知正文（§11.4）。
///
/// 问题本身**不在事件里**：§7.5 第 1 条把清单钉成派生视图，`kind` 与问题都由权威当场判出
/// （"有没有一条 `uncertain` 调用"、"会话还在不在服务范围里"）。所以这里按句柄回清单取
/// 那一条——照事件编一句话，只会和权威漂移。取不到（这一条刚刚被答掉）就退回一句说得清
/// 出处的话。
async fn question_for(state: &Arc<GatewayState>, run: &RunId, watcher: &Watcher) -> String {
    let question = match state.intervention(run.as_str()).await {
        Ok(Some(detail)) => super::interventions::question_of(&detail),
        Ok(None) => {
            "这条 Run 停在一次需要你判断的答复上（去 `GET /v1/interventions` 看这一条）".to_string()
        }
        Err(error) => {
            tracing::warn!(%error, %run, "读不出这条 Intervention 的问题正文");
            "这条 Run 停在一次需要你判断的答复上（清单这一次没读出来）".to_string()
        }
    };
    match watcher.label() {
        Some(name) => format!("定时任务「{name}」：{question}"),
        None => question,
    }
}

/// 收场：回写触发状态（Cron），把结果发回去。
///
/// `deliver = false` 只在命令 Job 退出 0、stdout 为空时用（§10：看门狗"没事不说话"）——
/// 触发状态仍然回写为 `Ok`，只是不经过 `notify` 的那次投递，不能让一个空结果的 Job
/// 看起来像是没跑过。
async fn finish(
    state: &Arc<GatewayState>,
    session: &SessionId,
    run: &RunId,
    watcher: &Watcher,
    status: komo_kernel::cron::FiringStatus,
    error: Option<String>,
    text: String,
    deliver: bool,
) {
    // **它进终态了，后面等着的那些可以走了**（§8.4 的次序规则）：一条条件 UPDATE 把
    // `waiting + dependency` 且前置已终态的放回 `queued`。对账每一拍也会做（§8.9），但
    // "同会话的下一条白等一分钟"是操作者看得见的延迟。
    state.release_dependents().await;
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
            // 取消不报告：那是人自己按下的。空输出的命令 Job（`deliver = false`）同样
            // 不报告，但理由不同——那是看门狗，不是取消。
            if deliver
                && (status != komo_kernel::cron::FiringStatus::Error
                    || watched.notify.delivers(status))
            {
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
    // **屏幕前有人在看的时候不投 chat 那份。** TUI / HTTP 来源的 Run 没有 chat 对端
    // （`peer` 是 `None`），照 §11.4 那份卡片就只剩 home chat 一个出口——而它是一次
    // 网络往返（实测几秒到几十秒，渠道不通时更久），人在看着这一条的时候它只会晚到，
    // 顺带多一份噪音（另外它还会在原地回写一条"已批准 · 短ID"，人刚在弹窗里答过）。
    //
    // 弹窗走的是 SSE 那一帧（`approval.requested` 一到就推，TUI 见 `run.waiting` 就自己
    // 去问清单），所以"有人在看"= 这条审批已经有人看见了。
    //
    // **没人在看就一定投**：Cron、`komo run` 脚本、TUI 关掉之后都落在那一支上——
    // §10 的底线是"不能因为无人值守就没人知道它在等"。TUI 关上而审批还挂着的那种，
    // 由周期兜底（[`GatewayState::sweep_unseen_interventions`]）在下一拍补投。
    //
    // 这一条**不占** `start_delivering_intervention` 那个"投过一次"的名额：看见的人走了
    // 之后兜底还要能补投。
    if watcher.peer().is_none() && state.hub.viewers(&record.session) > 0 {
        tracing::debug!(%approval, session = %record.session, "有人在看着这个会话，审批只弹在他面前");
        return;
    }
    if !state.start_delivering_intervention(record.short_id.as_str()) {
        tracing::debug!(%approval, "这条审批已经投过了");
        return;
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
