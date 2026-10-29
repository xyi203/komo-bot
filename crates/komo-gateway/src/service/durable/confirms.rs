//! 待确认的对外调用（`docs/confirm.md` §6）：无人值守的 Run 调对外工具时，工具任务停在 `confirm`
//! 阶段。它们和 faulted 任务一起出现在 Intervention 清单里（`kind = confirm`，句柄是工具任务 ID），
//! 结论是 `approve` / `reject`。
//!
//! 清单同样是派生视图：[`ConfirmIndex`] 开库后第一次用时扫一遍活任务，之后靠提交订阅维护（任务
//! 离开 `confirm` 阶段就移出）。答复只写 conversation 级 document `komo.confirm`（谁、什么时候、
//! 答了什么，任务结束后留着可查），工具任务每拍读它；写过答复的不再列出。
//!
//! 投递同 faulted 任务：来源对端 + home chat，逻辑键 `intervention:confirm:{任务}:…` 幂等，启动时
//! 回放一遍。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use komo_kernel::durable::storage::TaskQuery;
use komo_kernel::durable::{
    ConversationId, Cursor, ScanOrder, SessionKind, SessionLifecycle, SessionRecord, StorageWrite,
    TaskId, TaskRecord, TaskState, TaskStatus,
};
use komo_kernel::protocol::http::{
    InterventionAnswerResponse, InterventionDetail, InterventionKind, InterventionSummary,
    InterventionVerdict,
};
use komo_kernel::traits::{GatewayError, Shutdown};
use komo_kernel::types::chat::Outbound;
use komo_kernel::types::ids::{RunId, SessionId, ToolCallId};
use komo_runtime::durable::harness::{ConfirmAnswer, ConfirmVerdict, TOOL_KIND, confirm_address};
use serde_json::Value;
use tokio::sync::mpsc;

use super::DurableCore;
use super::interventions::{RunLookup, deliver_attention, task_entry};
use super::sessions::{SessionError, time_of};
use crate::service::state::GatewayState;

const SCAN_PAGE: usize = 512;
/// 卡里的参数最多印这么多字节。
const ARGUMENTS_BYTES: usize = 1024;

/// 工具任务此刻停在 `confirm` 吗。
fn confirming(record: &TaskRecord) -> bool {
    record.kind == TOOL_KIND
        && match &record.state {
            TaskState::Pending { checkpoint } | TaskState::Running { checkpoint } => {
                checkpoint.get("phase").and_then(Value::as_str) == Some("confirm")
            }
            _ => false,
        }
}

fn checkpoint(record: &TaskRecord) -> Option<&Value> {
    match &record.state {
        TaskState::Pending { checkpoint } | TaskState::Running { checkpoint } => Some(checkpoint),
        _ => None,
    }
}

// ------------------------------------------------------------------- 索引

#[derive(Default)]
pub struct ConfirmIndex {
    state: Mutex<IndexState>,
    loading: tokio::sync::Mutex<()>,
    announce: Mutex<Option<mpsc::UnboundedSender<TaskRecord>>>,
}

#[derive(Default)]
struct IndexState {
    loaded: bool,
    tasks: BTreeMap<u64, TaskRecord>,
}

impl ConfirmIndex {
    /// 提交线上的订阅回调：同步、要轻。
    pub fn observe(&self, writes: &[StorageWrite]) {
        for write in writes {
            let StorageWrite::Task(record) = write else {
                continue;
            };
            if record.kind != TOOL_KIND {
                continue;
            }
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if !confirming(record) {
                state.tasks.remove(&record.id.get());
                continue;
            }
            let fresh = state
                .tasks
                .insert(record.id.get(), record.clone())
                .is_none();
            drop(state);
            if fresh
                && let Some(sender) = self
                    .announce
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
            {
                let _ = sender.send(record.clone());
            }
        }
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<TaskRecord> {
        let (sender, receiver) = mpsc::unbounded_channel();
        *self.announce.lock().unwrap_or_else(PoisonError::into_inner) = Some(sender);
        receiver
    }

    async fn ensure_loaded(
        &self,
        storage: &dyn komo_kernel::durable::Storage,
    ) -> Result<(), SessionError> {
        if self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .loaded
        {
            return Ok(());
        }
        let _loading = self.loading.lock().await;
        if self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .loaded
        {
            return Ok(());
        }
        let mut found = Vec::new();
        // 重启后任务先回到 pending；正在等的是 running。
        for status in [TaskStatus::Pending, TaskStatus::Running] {
            let query = TaskQuery {
                kind: Some(TOOL_KIND.to_owned()),
                status: Some(status),
                order: Some(ScanOrder::Ascending),
                ..TaskQuery::default()
            };
            let mut cursor: Option<Cursor> = None;
            loop {
                let page = storage
                    .scan_tasks(&query, SCAN_PAGE, cursor.as_ref())
                    .await?;
                found.extend(page.items.into_iter().filter(confirming));
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for record in found {
            state.tasks.entry(record.id.get()).or_insert(record);
        }
        state.loaded = true;
        Ok(())
    }

    fn all(&self) -> Vec<TaskRecord> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tasks
            .values()
            .cloned()
            .collect()
    }

    fn get(&self, id: u64) -> Option<TaskRecord> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tasks
            .get(&id)
            .cloned()
    }

    /// 这些 conversation 被 purge 了：它们的任务不再列。
    pub fn forget(&self, conversations: &[ConversationId]) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tasks
            .retain(|_, record| !conversations.contains(&record.conversation));
    }
}

// ------------------------------------------------------------------- 清单

/// 一个等确认的工具调用，连同它所在的 Session。
#[derive(Debug, Clone)]
pub struct Confirming {
    pub task: TaskRecord,
    pub session: SessionRecord,
    pub tool: String,
    pub arguments: Value,
    pub asked_at: i64,
    pub deadline: i64,
}

impl Confirming {
    fn of(task: TaskRecord, session: SessionRecord) -> Option<Self> {
        let checkpoint = checkpoint(&task)?;
        let tool = checkpoint.pointer("/plan/tool")?.as_str()?.to_owned();
        let arguments = checkpoint.get("arguments").cloned().unwrap_or(Value::Null);
        let asked_at = checkpoint.get("askedAt")?.as_i64()?;
        let deadline = checkpoint.get("deadline")?.as_i64()?;
        Some(Self {
            task,
            session,
            tool,
            arguments,
            asked_at,
            deadline,
        })
    }

    pub fn handle(&self) -> String {
        self.task.id.to_string()
    }
}

/// 卡里的参数：紧凑 JSON，截到 [`ARGUMENTS_BYTES`]。
fn arguments_text(arguments: &Value) -> String {
    let text = arguments.to_string();
    if text.len() <= ARGUMENTS_BYTES {
        return text;
    }
    let mut end = ARGUMENTS_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…（共 {} 字节）", &text[..end], text.len())
}

impl DurableCore {
    pub fn confirms(&self) -> &Arc<ConfirmIndex> {
        &self.confirms
    }

    /// 还没答的待确认调用（升序）。`only` 给了就只取那个 Session 的。
    pub async fn confirming(&self, only: Option<&str>) -> Result<Vec<Confirming>, SessionError> {
        self.confirms.ensure_loaded(&*self.storage).await?;
        let mut out = Vec::new();
        for task in self.confirms.all() {
            let Some(session) = self.session_of_conversation(task.conversation).await? else {
                continue;
            };
            if only.is_some_and(|only| only != session.id)
                || !matches!(
                    session.lifecycle,
                    SessionLifecycle::Active | SessionLifecycle::Closing
                )
            {
                continue;
            }
            if self
                .confirm_answer(task.conversation, task.id)
                .await?
                .is_some()
            {
                continue;
            }
            if let Some(found) = Confirming::of(task, session) {
                out.push(found);
            }
        }
        Ok(out)
    }

    /// 句柄 → 正在等确认的任务（含已答、还没被任务读走的）。
    pub async fn find_confirming(&self, handle: &str) -> Result<Option<Confirming>, SessionError> {
        let Ok(id) = handle.trim().parse::<u64>() else {
            return Ok(None);
        };
        self.confirms.ensure_loaded(&*self.storage).await?;
        let Some(task) = self.confirms.get(id) else {
            return Ok(None);
        };
        let Some(session) = self.session_of_conversation(task.conversation).await? else {
            return Ok(None);
        };
        Ok(Confirming::of(task, session))
    }

    async fn confirm_answer(
        &self,
        conversation: ConversationId,
        task: TaskId,
    ) -> Result<Option<ConfirmAnswer>, SessionError> {
        let Some(record) = self
            .storage
            .find_document(&confirm_address(conversation, task))
            .await?
        else {
            return Ok(None);
        };
        Ok(serde_json::from_value(record.value).ok())
    }

    pub(super) async fn confirm_summary(
        &self,
        found: &Confirming,
        runs: &mut RunLookup,
    ) -> InterventionSummary {
        let call = found
            .task
            .input
            .get("callId")
            .and_then(Value::as_str)
            .map(ToolCallId::from_raw);
        let run = match task_entry(&found.task) {
            Some(entry) => runs.run_of(self, &found.session.id, entry).await,
            None => None,
        };
        InterventionSummary {
            handle: found.handle(),
            kind: InterventionKind::Confirm,
            session: SessionId::from_raw(found.session.id.clone()),
            run,
            call,
            question: format!("无人值守的 Run 要调对外工具 {}", found.tool),
            verdicts: InterventionKind::Confirm.verdicts(),
            created_at: time_of(found.asked_at),
        }
    }

    pub(super) async fn confirm_detail(&self, found: &Confirming) -> InterventionDetail {
        InterventionDetail::Confirm {
            summary: self.confirm_summary(found, &mut RunLookup::default()).await,
            tool: found.tool.clone(),
            arguments: found.arguments.clone(),
            deadline: time_of(found.deadline),
        }
    }

    /// 答复一张确认卡：写 `komo.confirm`（谁、什么时候、什么结论）。已经答过返回原结论。
    pub(super) async fn answer_confirm(
        &self,
        found: &Confirming,
        verdict: InterventionVerdict,
        by: String,
    ) -> Result<InterventionAnswerResponse, GatewayError> {
        let handle = found.handle();
        let verdict = match verdict {
            InterventionVerdict::Approve => ConfirmVerdict::Approve,
            InterventionVerdict::Reject => ConfirmVerdict::Reject,
            other => {
                return Err(GatewayError::InvalidRequest(format!(
                    "这条是 confirm（{} 等你确认），答不了 `{}`；它能答的是：`approve` / `reject`",
                    found.tool,
                    other.as_str()
                )));
            }
        };
        let address = confirm_address(found.task.conversation, found.task.id);
        let answer = ConfirmAnswer {
            verdict,
            by,
            at: (self.clock.now().unix_timestamp_nanos() / 1_000_000) as i64,
        };
        let value = serde_json::to_value(&answer).expect("ConfirmAnswer 可序列化");
        let previous = self
            .engine
            .commit(move |tx| {
                Box::pin(async move {
                    if let Some((_, value)) = tx.document(&address).await? {
                        return Ok(serde_json::from_value::<ConfirmAnswer>(value).ok());
                    }
                    tx.put_document(address, 1, value).await?;
                    Ok(None)
                })
            })
            .await
            .map_err(|error| GatewayError::Internal(error.to_string()))?;
        let (verdict, already) = match previous {
            Some(previous) => (previous.verdict, true),
            None => (answer.verdict, false),
        };
        let note = match (verdict, already) {
            (ConfirmVerdict::Approve, false) => format!("已批准：{} 马上执行。", found.tool),
            (ConfirmVerdict::Reject, false) => {
                format!("已拒绝：{} 不会执行，模型会收到说明。", found.tool)
            }
            (ConfirmVerdict::Approve, true) => format!("{handle} 已经批准过了，这里是原结论。"),
            (ConfirmVerdict::Reject, true) => format!("{handle} 已经拒绝过了，这里是原结论。"),
        };
        Ok(InterventionAnswerResponse {
            handle,
            kind: InterventionKind::Confirm,
            verdict: match verdict {
                ConfirmVerdict::Approve => InterventionVerdict::Approve,
                ConfirmVerdict::Reject => InterventionVerdict::Reject,
            },
            run_state: None,
            note,
            already_answered: already,
        })
    }

    /// 卡从哪来（定时任务、后台任务、会话里的后台事件）。
    async fn confirm_origin(&self, session: &SessionRecord) -> String {
        match session.kind {
            SessionKind::Cron => match session.title.as_deref() {
                Some(name) => format!("定时任务「{name}」"),
                None => "定时任务".into(),
            },
            SessionKind::Task => self
                .task_label(&session.id)
                .await
                .unwrap_or_else(|| "后台任务".into()),
            _ => match session.title.as_deref() {
                Some(title) => format!("会话「{title}」里的后台事件"),
                None => "会话里的后台事件".into(),
            },
        }
    }

    /// 卡的正文（渲染时前面加"需要你判断"）。
    async fn confirm_card(&self, found: &Confirming) -> String {
        let hours = (found.deadline - found.asked_at) as f64 / 3_600_000.0;
        let wait = if hours >= 1.0 && hours.fract() == 0.0 {
            format!("{} 小时", hours as i64)
        } else {
            format!("{} 分钟", (found.deadline - found.asked_at) / 60_000)
        };
        let handle = found.handle();
        format!(
            "对外动作等你确认\n来源：{}\n调用：{}\n参数：{}\n{wait}内没回复就不执行。\n\
             回复 /answer {handle} approve 执行，/answer {handle} reject 不执行。",
            self.confirm_origin(&found.session).await,
            found.tool,
            arguments_text(&found.arguments),
        )
    }
}

// ------------------------------------------------------------------- 投递

/// 起确认卡的投递（与 [`super::interventions::spawn_attention`] 同时起，在 `core.resume()` 之前）。
pub fn spawn_confirm_attention(state: &Arc<GatewayState>, shutdown: &Shutdown) {
    let Ok(core) = state.core() else { return };
    let mut receiver = core.confirms.subscribe();
    let weak = Arc::downgrade(state);
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        if let Some(state) = weak.upgrade()
            && let Ok(core) = state.core()
        {
            match core.confirming(None).await {
                Ok(all) => {
                    for found in all {
                        announce(&state, &found).await;
                    }
                }
                Err(error) => tracing::warn!(%error, "读不出待确认的调用，启动时的补投没做成"),
            }
        }
        loop {
            let received =
                tokio::time::timeout(super::interventions::SHUTDOWN_POLL, receiver.recv()).await;
            if shutdown.is_cancelled() {
                return;
            }
            let record = match received {
                Err(_) => continue,
                Ok(Some(record)) => record,
                Ok(None) => return,
            };
            let Some(state) = weak.upgrade() else { return };
            let Ok(core) = state.core() else { return };
            match core.find_confirming(&record.id.to_string()).await {
                Ok(Some(found))
                    if matches!(
                        found.session.lifecycle,
                        SessionLifecycle::Active | SessionLifecycle::Closing
                    ) =>
                {
                    announce(&state, &found).await;
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "确认卡没投出去"),
            }
        }
    });
}

async fn announce(state: &Arc<GatewayState>, found: &Confirming) {
    let Ok(core) = state.core() else { return };
    let summary = core.confirm_summary(found, &mut RunLookup::default()).await;
    let key = crate::notifier::intervention_key(&summary);
    let message = Outbound::NeedsAttention {
        session: SessionId::from_raw(found.session.id.clone()),
        run: summary
            .run
            .clone()
            .unwrap_or_else(|| RunId::from_raw(found.handle())),
        reason: core.confirm_card(found).await,
    };
    deliver_attention(state, &found.session, message, &key, &found.handle()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_arguments_are_cut_on_a_char_boundary() {
        let short = serde_json::json!({ "to": "x" });
        assert_eq!(arguments_text(&short), r#"{"to":"x"}"#);
        let long = serde_json::json!({ "text": "中".repeat(1000) });
        let text = arguments_text(&long);
        assert!(text.contains("…（共 "), "{text}");
        assert!(text.len() < ARGUMENTS_BYTES + 40);
    }
}
