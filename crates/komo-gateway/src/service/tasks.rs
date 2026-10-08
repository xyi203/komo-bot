//! 后台任务（`docs/background-tasks.md`）：`TaskSpawner` 的生产实现，以及任务收尾时把
//! 结果交回派它的那个会话。
//!
//! 建任务复用已有原语，一步都不新造：`SessionId::new_at` → `session::ensure_owned`（写死
//! `kind = Task`、`agent_id` = 派它的会话的 Agent）→ `ledgers.open`（落目录、`events.jsonl`）
//! → `session::set_title_if_empty`（标题先写死，`accept_input` 的"首行当标题"规则才不会
//! 盖掉它）→ `GatewayState::submit`（冻结身份、受理输入、挂看客）。
//!
//! **它自己需要一份 `Arc<GatewayState>`**（建会话、`submit` 都要），所以只能在那份状态
//! 造出来**之后**才能接上——与 `Dispatcher` / `GatewayState::inbound` 同一个"先有整份
//! 状态才能自己引用自己"的接法（见 `GatewayState::wire_task_spawner`）。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::traits::{SpawnError, TaskSpawner};
use komo_kernel::types::chat::ChannelPeer;
use komo_kernel::types::chat::{DeliveryTarget, Outbound};
use komo_kernel::types::ids::{RequestKey, RunId, SessionId};
use komo_kernel::types::status::RunState;
use komo_kernel::types::task::{self, FollowOutcome, TaskHandle, TaskSpec};
use komo_store::models::SessionKind;
use komo_store::repos::session;

use super::state::GatewayState;

/// 任务会话的 `origin`：`task:{parent_session}`。`follow` 的解析与收尾时"结果交回谁"
/// 都按它——任务属于哪个会话不需要新列。
pub fn task_origin(parent: &SessionId) -> String {
    format!("task:{parent}")
}

/// [`task_origin`] 的反解：一个会话的 `origin` 是不是 `task:{parent}` 的样子，是就给出
/// `parent`。`komo session list` 与 [`report_to_parent`] 都用它。
pub fn home_of(origin: &str) -> Option<SessionId> {
    origin.strip_prefix("task:").map(SessionId::from_raw)
}

/// 同一个会话名下、短号能匹配上 `task_id` 的任务会话。
async fn candidates_for(
    db: &komo_store::Db,
    parent: &SessionId,
    task_id: &str,
) -> Result<Vec<komo_store::repos::session::SessionRecord>, komo_kernel::traits::StoreError> {
    let origin = task_origin(parent);
    let records = session::list(db, false).await?;
    Ok(records
        .into_iter()
        .filter(|record| {
            record.kind == SessionKind::Task
                && record.origin == origin
                && task::matches_short_id(&record.session, task_id)
        })
        .collect())
}

pub struct GatewayTaskSpawner {
    state: Arc<GatewayState>,
}

impl GatewayTaskSpawner {
    pub fn new(state: Arc<GatewayState>) -> Arc<Self> {
        Arc::new(Self { state })
    }

    /// 派它的那条 Run 属于哪个会话（任务会话的 parent）、该投给哪个渠道对端
    /// （`runs.peer`，`ChannelPeer::parse` 是它的反解）。
    ///
    /// **任务会话里不能再派任务**：结果只交回一层，再往下派就成了一棵没人看得见的树。
    async fn parent_of(
        &self,
        from: &RunId,
    ) -> Result<(session::SessionRecord, Option<ChannelPeer>), SpawnError> {
        let run = komo_store::repos::runs::get(&self.state.db, from)
            .await
            .map_err(|error| SpawnError::Failed(format!("读不到派它的那条 Run：{error}")))?
            .ok_or_else(|| SpawnError::Failed(format!("派它的那条 Run {from} 不在账本里")))?;
        let parent = session::get(&self.state.db, &run.session)
            .await
            .map_err(|error| SpawnError::Failed(format!("读不到派它的那个会话：{error}")))?
            .ok_or_else(|| SpawnError::Failed(format!("派它的会话 {} 不在库里", run.session)))?;
        if parent.kind == SessionKind::Task {
            return Err(SpawnError::Failed(
                "这里已经是一个后台任务了，不能再派任务：自己把这件事做完".into(),
            ));
        }
        let peer = run.peer.as_deref().and_then(ChannelPeer::parse);
        Ok((parent, peer))
    }
}

#[async_trait]
impl TaskSpawner for GatewayTaskSpawner {
    async fn spawn(
        &self,
        from: &RunId,
        request_key: RequestKey,
        spec: TaskSpec,
    ) -> Result<TaskHandle, SpawnError> {
        let (parent, origin_peer) = self.parent_of(from).await?;
        let now = self.state.clock.now();
        let task_session = SessionId::new_at(now);
        let origin = task_origin(&parent.session);
        let jsonl_path = format!("sessions/{task_session}/events.jsonl");
        let worker = self
            .state
            .agent_of_session(&parent.session)
            .await
            .map_err(|error| {
                SpawnError::Failed(format!("读不出派它的会话归哪个 Agent：{error}"))
            })?;

        // 会话行**写死**在受理任何输入之前：`kind = Task`、`agent_id` 与派它的会话相同；
        // 幂等——同一个 id 重放只会原样返回已有的那一行。
        session::ensure_owned(
            &self.state.db,
            &task_session,
            &origin,
            &jsonl_path,
            &worker,
            SessionKind::Task,
        )
        .await
        .map_err(|error| SpawnError::Failed(format!("任务会话建不出来：{error}")))?;

        // 目录 + `events.jsonl`：`ledgers.open` 内部的 `ensure_in` 是幂等的，行已经在
        // 就原样返回，不会把上面写好的 `kind` / `agent_id` 改回默认值。
        self.state
            .ledgers
            .open(&task_session, &origin)
            .await
            .map_err(|error| SpawnError::Failed(format!("任务会话目录开不出来：{error}")))?;

        // 标题在第一条输入**之前**写死：`accept_input` 的"首行当标题"只填空的会话
        // （`docs/komo_bot.md` §8.5），先写上就不会被那条规则盖掉。
        session::set_title_if_empty(&self.state.db, &task_session, &spec.title)
            .await
            .map_err(|error| SpawnError::Failed(format!("标题记不上：{error}")))?;

        let response = self
            .state
            .submit(&task_session, request_key, spec.task, origin_peer, None)
            .await
            .map_err(|error| SpawnError::Failed(format!("提交第一条输入失败：{error}")))?;

        Ok(TaskHandle {
            session: response.session,
            short_id: task::short_id(&task_session),
            title: spec.title,
        })
    }

    async fn follow(
        &self,
        from: &RunId,
        request_key: RequestKey,
        task_id: &str,
        text: &str,
    ) -> Result<FollowOutcome, SpawnError> {
        let (parent, origin_peer) = self.parent_of(from).await?;

        // 同一个会话名下按短号匹配的任务会话；短号在这个集合里撞了才算歧义。
        let mut candidates = candidates_for(&self.state.db, &parent.session, task_id)
            .await
            .map_err(|error| SpawnError::Failed(format!("任务会话列不出来：{error}")))?;

        if candidates.is_empty() {
            return Err(SpawnError::UnknownTask(format!(
                "#{task_id} 不是这个会话派出去的任务"
            )));
        }
        if candidates.len() > 1 {
            let listed = candidates
                .iter()
                .map(|record| format!("#{}", task::extended_short_id(&record.session)))
                .collect::<Vec<_>>()
                .join("、");
            return Err(SpawnError::Ambiguous(format!(
                "#{task_id} 有歧义，候选：{listed}"
            )));
        }
        let target = candidates.remove(0);

        let response = self
            .state
            .submit(
                &target.session,
                request_key,
                text.to_string(),
                origin_peer,
                None,
            )
            .await
            .map_err(|error| SpawnError::Failed(format!("提交失败：{error}")))?;

        Ok(FollowOutcome {
            session: response.session,
            short_id: task::short_id(&target.session),
            queued_behind: response.state == RunState::Waiting,
        })
    }
}

/// 一条任务 Run 收尾时怎么样了（[`report_to_parent`] 的标题那一半）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutcome {
    Completed,
    NotCompleted,
}

/// 任务会话里一条 Run 收尾了：把结果当作**一条内部输入**交回派它的会话，由那边的模型
/// 转告用户（hermes 的 async delegation 同一个做法）。
///
/// 结果不直接投给渠道：派任务的那个会话要看得见结果，用户再问"进度怎么样"时它才答得上，
/// 而不是只看得见一句"已派出"。请求键 `task-result:{run}` 让同一条 Run 的结果只交回
/// 一次——看客与重启补挂各收尾一遍也只受理一条。
///
/// 返回 `false` = 这不是任务会话，调用方照常把结果投给来源。交回失败（派它的会话已经
/// 删掉了、受理出错）时退回直接投给来源，结果不能因此丢掉。
pub async fn report_to_parent(
    state: &Arc<GatewayState>,
    task_session: &SessionId,
    run: &RunId,
    peer: Option<&ChannelPeer>,
    outcome: TaskOutcome,
    text: &str,
) -> bool {
    let record = match session::get(&state.db, task_session).await {
        Ok(Some(record)) if record.kind == SessionKind::Task => record,
        Ok(_) => return false,
        Err(error) => {
            tracing::warn!(%error, session = %task_session, "读不出这个会话，结果照常投给来源");
            return false;
        }
    };
    let Some(parent) = home_of(&record.origin) else {
        return false;
    };
    let headline = match outcome {
        TaskOutcome::Completed => "已完成",
        TaskOutcome::NotCompleted => "没有完成",
    };
    let input = format!(
        "[后台任务 #{short}「{title}」{headline}]\n\n{text}\n\n\
         （这是后台任务交回的结果，不是用户发来的消息；用户看不到上面这段，请把结论转告用户。）",
        short = task::short_id(task_session),
        title = record.title,
    );
    let key = RequestKey::new(format!("task-result:{run}"));
    match state.submit(&parent, key, input, peer.cloned(), None).await {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(%error, %run, %parent, "后台任务的结果交不回派它的会话，直接投给来源");
            if let Some(peer) = peer
                && let Err(error) = state
                    .notifier
                    .log()
                    .deliver(
                        &DeliveryTarget::to_peer(peer.clone()),
                        Outbound::RunFinished {
                            session: task_session.clone(),
                            run: run.clone(),
                            summary: text.to_string(),
                        },
                    )
                    .await
            {
                tracing::warn!(%error, %run, "回复投不出去");
            }
            true
        }
    }
}

/// 任务会话里那些"需要你判断"的消息前面挂的标签：`后台任务 #2a9c「查空调」`。不是任务
/// 会话就是 `None`。操作者在聊天里同时看着几个任务时，靠它分清是哪一个在问。
pub async fn label_of(state: &GatewayState, task_session: &SessionId) -> Option<String> {
    let record = session::get(&state.db, task_session).await.ok()??;
    if record.kind != SessionKind::Task {
        return None;
    }
    Some(format!(
        "后台任务 #{}「{}」",
        task::short_id(task_session),
        record.title
    ))
}
