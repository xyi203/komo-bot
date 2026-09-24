//! `GatewayTaskSpawner`：`TaskSpawner` 的生产实现（`docs/home-dispatcher.md` §4.2）。
//!
//! 复用已有原语，一步都不新造：`SessionId::new_at` → `session::ensure_owned`（写死
//! `kind = Task`、`agent_id = worker`）→ `ledgers.open`（落目录、`events.jsonl`）→
//! `session::set_title_if_empty`（标题先写死，`accept_input` 的"首行当标题"规则才不会
//! 盖掉它）→ `GatewayState::submit`（冻结身份、受理输入、挂看客）。
//!
//! **它自己需要一份 `Arc<GatewayState>`**（建会话、`submit` 都要），所以只能在那份状态
//! 造出来**之后**才能接上——与 `Dispatcher` / `GatewayState::inbound` 同一个"先有整份
//! 状态才能自己引用自己"的接法（见 `GatewayState::wire_task_spawner`）。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::traits::{SpawnError, TaskSpawner};
use komo_kernel::types::chat::ChannelPeer;
use komo_kernel::types::ids::{RequestKey, RunId, SessionId};
use komo_kernel::types::status::RunState;
use komo_kernel::types::task::{self, FollowOutcome, TaskHandle, TaskSpec};
use komo_store::models::SessionKind;
use komo_store::repos::session;

use super::state::GatewayState;

/// 任务会话的 `origin`：`task:{home_session}`。看板（Phase 3）与 `follow` 的解析都按它
/// 过滤——同一个 home 名下的任务会话不需要新列，查询按这个字符串前缀（准确说是相等）
/// 就够了。
pub fn task_origin(home_session: &SessionId) -> String {
    format!("task:{home_session}")
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
    async fn parent_of(
        &self,
        from: &RunId,
    ) -> Result<(SessionId, Option<ChannelPeer>), SpawnError> {
        let record = komo_store::repos::runs::get(&self.state.db, from)
            .await
            .map_err(|error| SpawnError::Failed(format!("读不到派它的那条 Run：{error}")))?
            .ok_or_else(|| SpawnError::Failed(format!("派它的那条 Run {from} 不在账本里")))?;
        let peer = record.peer.as_deref().and_then(ChannelPeer::parse);
        Ok((record.session, peer))
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
        let (home_session, origin_peer) = self.parent_of(from).await?;
        let now = self.state.clock.now();
        let task_session = SessionId::new_at(now);
        let origin = task_origin(&home_session);
        let jsonl_path = format!("sessions/{task_session}/events.jsonl");
        let snapshot = self.state.snapshot();
        let worker = snapshot
            .home
            .worker_or(&snapshot.agent.default_agent)
            .to_string();

        // 会话行**写死**在受理任何输入之前：`kind = Task`、`agent_id = worker`
        // （§3：任务会话建会话时写死）；幂等——同一个 id 重放只会原样返回已有的那一行。
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
        let (home_session, origin_peer) = self.parent_of(from).await?;
        let origin = task_origin(&home_session);

        // 同一个 home 名下、还没被逻辑删除的任务会话；短号在这个集合里撞了才算歧义
        // （§4.3）。全量的"进行中 + 最近 N 条"任务看板是 Phase 3 的事，这里只解析。
        let records = session::list(&self.state.db, false)
            .await
            .map_err(|error| SpawnError::Failed(format!("任务会话列不出来：{error}")))?;
        let mut candidates: Vec<_> = records
            .into_iter()
            .filter(|record| {
                record.kind == SessionKind::Task
                    && record.origin == origin
                    && task::matches_short_id(&record.session, task_id)
            })
            .collect();

        if candidates.is_empty() {
            return Err(SpawnError::UnknownTask(format!(
                "#{task_id} 不是这个 home 名下正在跑或最近的任务"
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
