//! 任务清单：把会话投影成面向人的任务列表（`GET /v1/tasks`，`docs/tasks.md` §3）。
//!
//! 只读、不落库、不新建表：哪些会话算任务、状态是什么，都从现有的 `sessions` 行、`komo.live`、
//! `komo.task`、监听登记与待处理清单里读出来。
//!
//! # 哪些会话算任务
//!
//! `kind` 是 `task`（`task` 工具派出的）、`normal`（群与话题、直接开的会话）、`cron`（定时任务触发的）。
//! `main` 不算。（`docs/tasks.md` §3.1 的表把 cron 排除在外，以任务说明为准。）
//!
//! # 状态的优先级
//!
//! 同时满足几条时，从上到下取第一条：
//!
//! 1. **已归档**——会话 `closing` / `deleted`。会话已经在删了，别的都不再有意义。
//! 2. **等你（未答的介入）**——有待处理的介入或 `confirm`。哪怕 Run 还在跑（停在 `confirm` 上），
//!    用户该做的事也是去答复，不是"进行中"。
//! 3. **进行中**——`komo.live.run` 在，或收件箱里排着输入（马上要开跑）。新 Run 一开，上一次的
//!    "受阻"就过期了，所以它排在受阻之前。
//! 4. **等你（受阻）**——最近一次交回的状态是 `blocked`。
//! 5. **已交回**——`komo.task.lastRun` 有终态。`outcome` 区分完成 / 没说做完 / 没有完成；旧
//!    document 没有 `status` 时按"没说做完"。
//! 6. **空闲**——其余：没有派出方的会话、刚派出还没有 Run 的任务、跑完了的定时任务会话。
//!
//! # 分页
//!
//! 与会话列表同一个口径（`limit` / 不透明 `cursor`，不带 `limit` 就是全部）：按会话 id 降序扫
//! `sessions`，挑出任务会话，凑够 `limit + 1` 条就停（多出来的那条只用来判断还有没有下一页）。
//! 续扫位置是这一页最后一条的 id，所以扫过但被滤掉的 `main` 会话不会让页变短，也不会重复。

use std::collections::HashMap;

use komo_kernel::durable::storage::{SessionCursor, SessionQuery};
use komo_kernel::durable::{ScanOrder, SessionKind, SessionLifecycle, SessionRecord};
use komo_kernel::protocol::http::{
    InterventionListQuery, TaskOutcome, TaskSource, TaskState, TaskSummary,
};
use komo_kernel::types::handle::short_handle;
use komo_kernel::types::ids::SessionId;
use komo_runtime::durable::harness::orchestration::monitor;
use komo_runtime::durable::harness::{Harness, task_doc};

use super::super::DurableCore;
use super::{SCAN_PAGE, SessionError, SessionPageCursor, time_of};

/// 任务清单的一页。
pub struct TaskPage {
    pub tasks: Vec<TaskSummary>,
    pub next: Option<SessionPageCursor>,
}

/// 默认只要活着的；`all` 把归档的（`closing` / `deleted`）带上。`purged` 都不列。
fn task_query(all: bool) -> SessionQuery {
    let mut lifecycles = vec![SessionLifecycle::Active];
    if all {
        lifecycles.extend([SessionLifecycle::Closing, SessionLifecycle::Deleted]);
    }
    SessionQuery {
        lifecycles,
        order: Some(ScanOrder::Descending),
        ..SessionQuery::default()
    }
}

fn source_of(kind: SessionKind) -> Option<TaskSource> {
    match kind {
        SessionKind::Main => None,
        SessionKind::Normal => Some(TaskSource::Conversation),
        SessionKind::Task => Some(TaskSource::Dispatched),
        SessionKind::Cron => Some(TaskSource::Cron),
    }
}

impl DurableCore {
    /// 任务清单的一页（最新的在前）。`limit` 为空 = 全部。
    pub async fn task_page(
        &self,
        all: bool,
        limit: Option<usize>,
        after: Option<&SessionPageCursor>,
    ) -> Result<TaskPage, SessionError> {
        let query = task_query(all);
        let mut cursor: Option<SessionCursor> = after.map(|cursor| cursor.0.clone());
        let mut records: Vec<SessionRecord> = Vec::new();
        'scan: loop {
            let page = self
                .storage
                .scan_sessions(&query, SCAN_PAGE, cursor.as_ref())
                .await?;
            for record in page.items {
                if source_of(record.kind).is_none() {
                    continue;
                }
                records.push(record);
                if limit.is_some_and(|limit| records.len() > limit) {
                    break 'scan;
                }
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let mut next = None;
        if let Some(limit) = limit
            && records.len() > limit
        {
            records.truncate(limit);
            next = records.last().map(|last| {
                SessionPageCursor(SessionCursor::new(last.id.clone(), ScanOrder::Descending))
            });
        }

        // 待处理的介入与监听：整张清单各读一次，按会话 / conversation 归并，比每行各查一遍省。
        let waiting: std::collections::HashSet<String> = self
            .interventions(&InterventionListQuery::default())
            .await?
            .into_iter()
            .map(|summary| summary.session.as_str().to_owned())
            .collect();
        let mut monitors: HashMap<u64, u32> = HashMap::new();
        for record in monitor::list(&*self.storage).await? {
            if record.is_live() {
                *monitors.entry(record.conversation.get()).or_default() += 1;
            }
        }

        let mut tasks = Vec::with_capacity(records.len());
        for record in &records {
            let asking = waiting.contains(&record.id);
            let live_monitors = monitors
                .get(&record.current_branch.get())
                .copied()
                .unwrap_or(0);
            tasks.push(self.task_summary(record, asking, live_monitors).await?);
        }
        Ok(TaskPage { tasks, next })
    }

    async fn task_summary(
        &self,
        record: &SessionRecord,
        asking: bool,
        monitors: u32,
    ) -> Result<TaskSummary, SessionError> {
        let source = source_of(record.kind).expect("调用方已滤掉 main");
        let live = Harness::live(&*self.storage, record.current_branch).await?;
        let queued = live.run.is_none()
            && !Harness::inbox(&*self.storage, record.current_branch)
                .await?
                .items
                .is_empty();
        // `komo.task` 只有派出的任务有；它在子 conversation（= 根）上。
        let last = match record.kind {
            SessionKind::Task => task_doc(&*self.storage, record.root_conversation)
                .await?
                .and_then(|doc| doc.last_run),
            _ => None,
        };
        let outcome = last.as_ref().map(|last| match last.state.as_str() {
            "completed" => match last.status.as_deref() {
                Some("done") => TaskOutcome::Done,
                Some("blocked") => TaskOutcome::Blocked,
                // 没写状态行；旧 document 没有这一格也是这个。
                _ => TaskOutcome::Unstated,
            },
            _ => TaskOutcome::NotCompleted,
        });

        let archived = record.lifecycle != SessionLifecycle::Active;
        let running = live.run.is_some() || queued;
        let state = if archived {
            TaskState::Archived
        } else if asking {
            TaskState::Waiting
        } else if running {
            TaskState::Running
        } else if outcome == Some(TaskOutcome::Blocked) {
            TaskState::Waiting
        } else if outcome.is_some() {
            TaskState::Returned
        } else {
            TaskState::Idle
        };

        Ok(TaskSummary {
            session: SessionId::from_raw(record.id.clone()),
            handle: short_handle(record.root_conversation.get()),
            title: record.title.clone().unwrap_or_default(),
            state,
            // 在跑时上一次的结局已经过期，不带。
            outcome: if live.run.is_some() { None } else { outcome },
            source,
            dispatched_by: (record.kind == SessionKind::Task)
                .then(|| record.parent_session.clone().map(SessionId::from_raw))
                .flatten(),
            started_at: time_of(record.created_at),
            ended_at: if live.run.is_some() {
                None
            } else {
                last.as_ref().map(|last| time_of(last.ended_at))
            },
            rounds: match &live.run {
                Some(run) => Some(run.rounds),
                None => last.as_ref().and_then(|last| last.rounds),
            },
            monitors,
        })
    }
}
