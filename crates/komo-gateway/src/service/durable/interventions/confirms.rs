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
    ConversationId, Cursor, ScanOrder, SessionLifecycle, SessionRecord, StorageWrite, TaskId,
    TaskRecord, TaskState, TaskStatus,
};
use komo_kernel::protocol::http::{
    InterventionAnswerResponse, InterventionDetail, InterventionKind, InterventionSummary,
    InterventionVerdict,
};
use komo_kernel::traits::GatewayError;
use komo_kernel::types::ids::{SessionId, ToolCallId};
use komo_runtime::durable::harness::{ConfirmAnswer, ConfirmVerdict, TOOL_KIND, confirm_address};
use serde_json::Value;

use super::super::DurableCore;
use super::super::sessions::{SessionError, time_of};
use super::{RunLookup, task_entry};

const SCAN_PAGE: usize = 512;

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
            state.tasks.insert(record.id.get(), record.clone());
        }
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

    pub(in crate::service::durable) async fn confirm_summary(
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

    pub(in crate::service::durable) async fn confirm_detail(
        &self,
        found: &Confirming,
    ) -> InterventionDetail {
        InterventionDetail::Confirm {
            summary: self.confirm_summary(found, &mut RunLookup::default()).await,
            tool: found.tool.clone(),
            arguments: found.arguments.clone(),
            deadline: time_of(found.deadline),
        }
    }

    /// 答复一张确认卡：写 `komo.confirm`（谁、什么时候、什么结论）。已经答过返回原结论。
    pub(in crate::service::durable) async fn answer_confirm(
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
}
