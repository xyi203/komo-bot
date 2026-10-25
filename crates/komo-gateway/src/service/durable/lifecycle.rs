//! Session 生命周期与 reconcile 在新存储上（§8.10；`docs/storage-durable-d5.md` §4 W4a）。
//!
//! 状态阶梯 `active → closing → deleted → purged`，行在 `sessions.lifecycle`，每一步都是
//! 提交里的比较并换（CAS）：
//!
//! - **delete**：先 `closing`（不再收新输入，`ensure_open` 拒），然后撤回这个 Session 所有 conversation
//!   里排队的输入——**含内部输入**（`abort_conversation` 为了"后台结果不丢"保留内部输入，而删除要的
//!   正是不再起后继 run）。`now = true` 再对每个 conversation 打 abort 标记（穿过后台边界，名下的后台
//!   任务一起停），等被标记的任务终结，之后进 `deleted`。abort 的 run 收尾时仍会放置收尾那一刻才到的内部输入、
//!   起后继 run，所以"撤回 → abort → 看还剩什么"循环到什么都不剩（有上限，剩下的由后台补做接着收）。
//!   `now = false`：撤回排队的，其余让在跑的 run 跑完，空了才 `deleted`（后台补做按判定推进，不是时钟）。
//! - **purge**：要求已 `deleted`（旧 komo 会自动先删；新存储不——purge 是破坏性的，不替人做上一步）。
//!   先算阻挡项（未结束的工作 + `aux.db` 里的记忆证据 / 投递 / cron 触发），有就 409 并列出；没有就**一次
//!   提交**里 `PurgeConversations` + 退役 `komo.interventions.ack` + 墓碑 `purged`，**提交之后**才删
//!   Session 目录（先账后内容，§8.10 第 3 条）。已 `purged` 再来一次是幂等的：只把还留着的目录收掉。
//! - **启动补做**：[`boot`] 在引擎恢复之前查工具任务的 blob 引用、收掉墓碑已落而目录还在的会话；
//!   [`spawn`] 之后一直（启动一次 + 每 `CLOSING_TICK`）把停在 `closing` 的推进到 `deleted`。
//!
//! # reconcile（`POST /v1/reconcile`、启动）
//!
//! 新存储的恢复由引擎开库时的 reconciliation 做，gateway 的 reconcile 只剩**blob 引用**一件事：还没
//! 终结的工具任务（checkpoint 在 `execute` / `probe`）的尝试目录里有 `output.json`，却解析不了、或它引用的
//! `stdout.txt` / `stderr.txt` 不在了 / 大小对不上——这份"已发布的输出"不可信，而恢复会把它当成"没有结果"，
//! 然后重跑或记成 uncertain，悄悄换掉一次已经完成的尝试。这里改成让那个工具任务 `faulted`
//! （`Engine::fault_idle_task`，在跑的不碰），它就出现在 intervention 清单里等人处理（"不能静默"）。
//! 只查引用，不读内容、不重算哈希。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use komo_kernel::durable::storage::{SessionQuery, TaskQuery};
use komo_kernel::durable::{
    ConversationId, ConversationRecord, SessionCursor, SessionLifecycle, SessionRecord, TaskState,
    TaskStatus,
};
use komo_kernel::protocol::http::{PurgeBlocker, ReconcileResponse};
use komo_kernel::traits::Shutdown;
use komo_kernel::types::ids::{RunId, SessionId};
use komo_runtime::durable::harness::orchestration::monitor;
use komo_runtime::durable::harness::{Harness, InboxItem, TOOL_KIND, WithdrawResult};
use komo_store::SessionPaths;
use serde_json::Value;

use super::DurableCore;
use super::runs::session_conversations;
use super::sessions::SessionError;
use crate::service::state::GatewayState;

/// 补做 `closing → deleted` 的间隔。
const CLOSING_TICK: Duration = Duration::from_secs(5);
/// "撤回 → abort → 看还剩什么"最多转几圈。
const DRAIN_ROUNDS: usize = 8;
const SCAN_PAGE: usize = 256;

/// delete 的结果。
#[derive(Debug, Clone)]
pub struct Deleted {
    pub lifecycle: SessionLifecycle,
    /// 行上最近一次改动的时刻（毫秒）。
    pub changed_at: i64,
    /// `now` 时被取消的未完成 run。
    pub cancelled: Vec<RunId>,
}

/// purge 的结果。
#[derive(Debug, Clone, Copy)]
pub struct Purged {
    pub removed_bytes: u64,
}

/// 一次 reconcile 判定了什么（对应 [`ReconcileResponse`]）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// 看了几个在 `execute` / `probe` 的工具任务。
    pub checked: u32,
    /// 因 blob 引用读不到而 `faulted` 的工具任务。
    pub blocked: u32,
    /// `closing → deleted` 推进的会话数。
    pub closed: u32,
    /// 墓碑已落、目录还在，这次收掉的会话数。
    pub purged: u32,
}

impl DurableCore {
    /// 比较并换：`lifecycle` 在 `from` 里才改成 `to`（并记 `changed_at`），返回改后的行与这次有没有改。
    async fn advance(
        &self,
        id: &str,
        from: &'static [SessionLifecycle],
        to: SessionLifecycle,
    ) -> Result<(SessionRecord, bool), SessionError> {
        let id = id.to_owned();
        Ok(self
            .engine
            .commit(move |tx| {
                Box::pin(async move {
                    let mut record = tx.session(&id).await?.ok_or_else(|| {
                        komo_runtime::durable::DurableError::rejected(format!("会话 {id} 不存在"))
                    })?;
                    if !from.contains(&record.lifecycle) {
                        return Ok((record, false));
                    }
                    record.lifecycle = to;
                    record.changed_at = tx.now();
                    tx.put_session(record.clone());
                    Ok((record, true))
                })
            })
            .await?)
    }

    /// 撤回这个 Session 全部 conversation 里排队的 steer / followUp，**含内部输入**。返回撤回了几条。
    /// 排队的 write（展示用）留着：它们随 run 收尾的 boundary 落下，不起 run。
    async fn withdraw_queued(
        &self,
        conversations: &[ConversationRecord],
    ) -> Result<usize, SessionError> {
        let mut withdrawn = 0;
        for conversation in conversations {
            let inbox = Harness::inbox(&*self.storage, conversation.id).await?;
            for item in inbox.items {
                if matches!(item, InboxItem::Write { .. }) {
                    continue;
                }
                let result = self
                    .harness
                    .withdraw(&self.engine, item.id(), Some(conversation.id))
                    .await?;
                if result == WithdrawResult::Aborted {
                    withdrawn += 1;
                }
            }
        }
        Ok(withdrawn)
    }

    /// 这个 Session 还有什么没做完：在跑的 run、排队的输入、活着的任务。每条一个阻挡项。
    pub async fn work_remaining(&self, session: &str) -> Result<Vec<PurgeBlocker>, SessionError> {
        let mut blockers = Vec::new();
        for conversation in session_conversations(&*self.storage, session).await? {
            let live = Harness::live(&*self.storage, conversation.id).await?;
            if let Some(run) = &live.run {
                blockers.push(PurgeBlocker {
                    what: "unfinished_run".into(),
                    detail: format!(
                        "conversation {} 上还有一条在跑的 run（输入 {}）；先让它跑完或取消",
                        conversation.id,
                        run.inputs
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }
            let inbox = Harness::inbox(&*self.storage, conversation.id).await?;
            let queued = inbox
                .items
                .iter()
                .filter(|item| !matches!(item, InboxItem::Write { .. }))
                .count();
            if queued > 0 {
                blockers.push(PurgeBlocker {
                    what: "queued_input".into(),
                    detail: format!(
                        "conversation {} 的收件箱里还排着 {queued} 条输入",
                        conversation.id
                    ),
                });
            }
            let mut live_tasks = 0;
            for status in [
                TaskStatus::Pending,
                TaskStatus::Running,
                TaskStatus::Waiting,
                TaskStatus::Completing,
            ] {
                let page = self
                    .storage
                    .scan_tasks(
                        &TaskQuery {
                            conversation: Some(conversation.id),
                            status: Some(status),
                            ..TaskQuery::default()
                        },
                        SCAN_PAGE,
                        None,
                    )
                    .await?;
                live_tasks += page.items.len();
            }
            if live_tasks > 0 {
                blockers.push(PurgeBlocker {
                    what: "task".into(),
                    detail: format!(
                        "conversation {} 上还有 {live_tasks} 个没终结的任务",
                        conversation.id
                    ),
                });
            }
        }
        Ok(blockers)
    }

    /// 这个 Session 名下还在跑的监听（purge 的阻挡项；delete 不替它们收尾——结束消息会再起 run）。
    async fn live_monitors(
        &self,
        conversations: &[ConversationRecord],
    ) -> Result<Vec<PurgeBlocker>, SessionError> {
        let ids: Vec<ConversationId> = conversations.iter().map(|c| c.id).collect();
        Ok(monitor::list(&*self.storage)
            .await?
            .into_iter()
            .filter(|record| record.is_live() && ids.contains(&record.conversation))
            .map(|record| PurgeBlocker {
                what: "monitor".into(),
                detail: format!(
                    "监听 #{}「{}」还在跑；先 monitor_stop 或等它结束",
                    record.id, record.title
                ),
            })
            .collect())
    }

    /// 逻辑删除（§8.10）。已经 `deleted` / `purged` 的再删一次是幂等的：返回它现在的状态、`cancelled` 为空。
    pub async fn delete_session(&self, id: &str, now: bool) -> Result<Deleted, SessionError> {
        let record = self.session(id).await?;
        if matches!(
            record.lifecycle,
            SessionLifecycle::Deleted | SessionLifecycle::Purged
        ) {
            return Ok(Deleted {
                lifecycle: record.lifecycle,
                changed_at: record.changed_at,
                cancelled: Vec::new(),
            });
        }
        // 先取"将被取消"的 run 清单：abort 之后它们就不再是未结束的了。
        let unfinished: Vec<RunId> = if now {
            self.unfinished_runs(id)
                .await?
                .into_iter()
                .map(|summary| summary.run)
                .collect()
        } else {
            Vec::new()
        };
        let (mut record, _) = self
            .advance(id, &[SessionLifecycle::Active], SessionLifecycle::Closing)
            .await?;
        let conversations = session_conversations(&*self.storage, id).await?;
        if now {
            for _ in 0..DRAIN_ROUNDS {
                self.withdraw_queued(&conversations).await?;
                for conversation in &conversations {
                    // 穿过后台边界：后台任务（`task` 工具派出的、监听的锚点）一起停。
                    self.engine
                        .abort_conversation(conversation.id, true)
                        .await?;
                }
                if self.work_remaining(id).await?.is_empty() {
                    break;
                }
            }
        } else {
            self.withdraw_queued(&conversations).await?;
        }
        if self.work_remaining(id).await?.is_empty() {
            record = self
                .advance(
                    id,
                    &[SessionLifecycle::Active, SessionLifecycle::Closing],
                    SessionLifecycle::Deleted,
                )
                .await?
                .0;
        }
        Ok(Deleted {
            lifecycle: record.lifecycle,
            changed_at: record.changed_at,
            cancelled: unfinished,
        })
    }

    /// 把停在 `closing` 的会话推进到 `deleted`：撤回新到的排队输入，工作都做完了就推进。返回推进了几个。
    pub async fn finish_closing(&self) -> Result<u32, SessionError> {
        let query = SessionQuery {
            lifecycle: Some(SessionLifecycle::Closing),
            order: None,
        };
        let mut closing = Vec::new();
        let mut cursor: Option<SessionCursor> = None;
        loop {
            let page = self
                .storage
                .scan_sessions(&query, SCAN_PAGE, cursor.as_ref())
                .await?;
            closing.extend(page.items);
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let mut closed = 0;
        for record in closing {
            let conversations = session_conversations(&*self.storage, &record.id).await?;
            self.withdraw_queued(&conversations).await?;
            if self.work_remaining(&record.id).await?.is_empty() {
                let (_, changed) = self
                    .advance(
                        &record.id,
                        &[SessionLifecycle::Closing],
                        SessionLifecycle::Deleted,
                    )
                    .await?;
                if changed {
                    closed += 1;
                }
            }
        }
        Ok(closed)
    }

    /// 还在跑的工具任务里 blob 引用读不到的，写 `faulted`（见模块文档）。
    pub async fn check_blobs(&self, sessions_dir: &Path) -> Result<(u32, u32), SessionError> {
        let (mut checked, mut faulted) = (0, 0);
        for status in [TaskStatus::Pending, TaskStatus::Running] {
            let query = TaskQuery {
                kind: Some(TOOL_KIND.to_owned()),
                status: Some(status),
                ..TaskQuery::default()
            };
            let mut cursor = None;
            loop {
                let page = self
                    .storage
                    .scan_tasks(&query, SCAN_PAGE, cursor.as_ref())
                    .await?;
                for task in page.items {
                    let checkpoint = match &task.state {
                        TaskState::Pending { checkpoint } | TaskState::Running { checkpoint } => {
                            checkpoint
                        }
                        _ => continue,
                    };
                    let phase = checkpoint.get("phase").and_then(Value::as_str);
                    let attempt = checkpoint.get("attempt").and_then(Value::as_u64);
                    let (Some("execute" | "probe"), Some(attempt)) = (phase, attempt) else {
                        continue;
                    };
                    let Some(session) = self
                        .storage
                        .conversation(task.conversation)
                        .await?
                        .and_then(|conversation| conversation.session)
                    else {
                        continue;
                    };
                    checked += 1;
                    let paths = SessionPaths::new(sessions_dir, &SessionId::from_raw(session));
                    let dir = paths.root().join(format!(
                        "tool-output/{}/{}/{attempt}",
                        task.conversation, task.id
                    ));
                    let Some(why) = broken_output(&paths, &dir).await else {
                        continue;
                    };
                    if self
                        .engine
                        .fault_idle_task(task.id, format!("引用的 blob 读不到：{why}"))
                        .await?
                    {
                        faulted += 1;
                    }
                }
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
        }
        Ok((checked, faulted))
    }
}

/// 一次尝试目录里的 `output.json` 不可信的原因；没有 `output.json`（还没发布）或一切正常是 `None`。
async fn broken_output(paths: &SessionPaths, dir: &Path) -> Option<String> {
    let bytes = match tokio::fs::read(dir.join("output.json")).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => return Some(format!("读 output.json 失败：{error}")),
    };
    let Ok(document) = serde_json::from_slice::<Value>(&bytes) else {
        return Some("output.json 不是合法的 JSON".to_owned());
    };
    for name in ["stdout", "stderr"] {
        let Some(reference) = document.get(name).filter(|value| value.is_object()) else {
            continue;
        };
        let (Some(path), Some(size)) = (
            reference.get("path").and_then(Value::as_str),
            reference.get("size").and_then(Value::as_u64),
        ) else {
            return Some(format!("output.json 里 {name} 的引用不完整"));
        };
        let resolved = match paths.resolve(path) {
            Ok(resolved) => resolved,
            Err(error) => return Some(format!("{name} 的引用 {path} 不合法：{error}")),
        };
        match tokio::fs::metadata(&resolved).await {
            Ok(meta) if meta.len() == size => {}
            Ok(meta) => {
                return Some(format!("{path} 大小是 {}，引用写的是 {size}", meta.len()));
            }
            Err(_) => return Some(format!("{path} 不在了")),
        }
    }
    None
}

/// 墓碑已落（`purged`）、目录还在的会话：把目录收掉。返回收掉了几个。
async fn finish_purged(core: &DurableCore, sessions_dir: &Path) -> Result<u32, SessionError> {
    let query = SessionQuery {
        lifecycle: Some(SessionLifecycle::Purged),
        order: None,
    };
    let mut purged = Vec::new();
    let mut cursor: Option<SessionCursor> = None;
    loop {
        let page = core
            .storage
            .scan_sessions(&query, SCAN_PAGE, cursor.as_ref())
            .await?;
        purged.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    let mut done = 0;
    for record in purged {
        let paths = SessionPaths::new(sessions_dir, &SessionId::from_raw(record.id.clone()));
        if !paths.root().exists() {
            continue;
        }
        match komo_store::purge::remove_session_content(&paths).await {
            Ok(_) => done += 1,
            // 删不掉就说清楚、下一次再试（§8.8），不假装成功。
            Err(error) => {
                tracing::warn!(%error, session = %record.id, "墓碑已落但目录删不掉，下次再试")
            }
        }
    }
    Ok(done)
}

/// purge（§8.10 第 3 条）。`Err(blockers)` 是引用检查没过（HTTP 409，列出要先处置什么）。
pub async fn purge(
    state: &Arc<GatewayState>,
    id: &str,
) -> Result<Result<Purged, Vec<PurgeBlocker>>, SessionError> {
    let core = state
        .core()
        .map_err(|error| SessionError::Internal(error.to_string()))?;
    let record = core.session(id).await?;
    let sessions_dir = state.snapshot().paths.sessions_dir.clone();
    let paths = SessionPaths::new(&sessions_dir, &SessionId::from_raw(id));
    match record.lifecycle {
        SessionLifecycle::Active | SessionLifecycle::Closing => {
            return Err(SessionError::Conflict(format!(
                "会话 {id} 现在是 {}：先 delete（purge 只从 `deleted` 起步，不替人做上一步）",
                super::sessions::state_of(record.lifecycle).as_str()
            )));
        }
        SessionLifecycle::Purged => {
            // 幂等：只把还留着的目录收掉。
            let removed_bytes = remove_content(&paths).await?;
            return Ok(Ok(Purged { removed_bytes }));
        }
        SessionLifecycle::Deleted => {}
    }
    let conversations = session_conversations(&*core.storage, id).await?;
    let mut blockers = core.work_remaining(id).await?;
    blockers.extend(core.live_monitors(&conversations).await?);
    blockers.extend(
        komo_store::purge::purge_blockers(&state.db, &SessionId::from_raw(id))
            .await
            .map_err(|error| SessionError::Internal(error.to_string()))?,
    );
    if !blockers.is_empty() {
        return Ok(Err(blockers));
    }
    let ids: Vec<ConversationId> = conversations.iter().map(|c| c.id).collect();
    let session = id.to_owned();
    let purged_ids = ids.clone();
    core.engine
        .commit(move |tx| {
            Box::pin(async move {
                // 先读后写：表读必须排在这次提交的第一次表写（`purge_conversations`）之前。
                let mut record = tx.session(&session).await?.ok_or_else(|| {
                    komo_runtime::durable::DurableError::rejected(format!("会话 {session} 不存在"))
                })?;
                if record.lifecycle != SessionLifecycle::Deleted {
                    return Err(komo_runtime::durable::DurableError::rejected(format!(
                        "会话 {session} 不再是 deleted，purge 取消"
                    )));
                }
                tx.retire_document(&super::interventions::ack_address(&session))
                    .await?;
                tx.purge_conversations(purged_ids);
                record.lifecycle = SessionLifecycle::Purged;
                record.changed_at = tx.now();
                tx.put_session(record);
                Ok(())
            })
        })
        .await?;
    core.faults().forget(&ids);
    // 账先落、内容后删：这里被杀，下次启动的 `boot` 收尾。
    let removed_bytes = remove_content(&paths).await?;
    Ok(Ok(Purged { removed_bytes }))
}

async fn remove_content(paths: &SessionPaths) -> Result<u64, SessionError> {
    komo_store::purge::remove_session_content(paths)
        .await
        .map_err(|error| SessionError::Internal(error.to_string()))
}

/// 引擎恢复**之前**做的事（`service::start`）：工具任务的 blob 引用、收掉墓碑已落的目录。
/// 必须在 `core.resume()` 之前——恢复一开始，`execute` 阶段的任务就会把读不到的输出当成"没有结果"重判。
pub async fn boot(state: &Arc<GatewayState>) {
    if let Err(error) = reconcile(state).await {
        tracing::warn!(%error, "启动时的 reconcile 没做成");
    }
}

/// 一次 reconcile（`POST /v1/reconcile` 与启动共用）。幂等。
pub async fn reconcile(state: &Arc<GatewayState>) -> Result<ReconcileReport, SessionError> {
    let core = state
        .core()
        .map_err(|error| SessionError::Internal(error.to_string()))?;
    let sessions_dir = state.snapshot().paths.sessions_dir.clone();
    let (checked, blocked) = core.check_blobs(&sessions_dir).await?;
    let closed = core.finish_closing().await?;
    let purged = finish_purged(core, &sessions_dir).await?;
    let report = ReconcileReport {
        checked,
        blocked,
        closed,
        purged,
    };
    if blocked + closed + purged > 0 {
        tracing::info!(?report, "reconcile 判定了");
    }
    Ok(report)
}

impl ReconcileReport {
    pub fn response(self, finished_at: time::OffsetDateTime) -> ReconcileResponse {
        ReconcileResponse {
            checked: self.checked,
            resumed: 0,
            blocked: self.blocked,
            reclaimed: 0,
            closed: self.closed,
            purged: self.purged,
            finished_at,
        }
    }
}

/// 后台：启动一次、之后每 [`CLOSING_TICK`] 把停在 `closing` 的会话推进到 `deleted`（W4a）。
pub fn spawn(state: &Arc<GatewayState>, shutdown: &Shutdown) {
    let weak = Arc::downgrade(state);
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        loop {
            let Some(state) = weak.upgrade() else { return };
            if let Ok(core) = state.core()
                && let Err(error) = core.finish_closing().await
            {
                tracing::warn!(%error, "补做 closing → deleted 没做成");
            }
            drop(state);
            let deadline = tokio::time::Instant::now() + CLOSING_TICK;
            while tokio::time::Instant::now() < deadline {
                if shutdown.is_cancelled() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    });
}
