//! 拼 Router 的输入（`docs/tasks.md` §4.11 "输入"）：候选任务来自任务清单投影，最近消息来自主线。
//!
//! 只读已提交状态。判断重做时（重启后从检查点续）拼出来的可能与第一次不同——没关系，判断没有副作用，
//! 而被采用的那一份会写进检查点（`Decision`），执行不再重读。

use komo_agent::router::{
    CandidateState, CandidateTask, RecentMessage, RouteScope, RouterInput, RouterLimits, Speaker,
};
use komo_kernel::durable::storage::EntryQuery;
use komo_kernel::durable::{ContentBlock, EntryId, Message, ScanOrder, SessionRecord, StopReason};
use komo_kernel::protocol::http::{TaskSource, TaskState};
use komo_kernel::types::task::{TASK_STATUS_BLOCKED, TASK_STATUS_DONE};

use super::super::DurableCore;
use super::super::sessions::SessionError;
use super::{Handles, MAIN_REPLY_ENTRY, MAIN_USER_ENTRY};

/// 候选只看最近这么多个任务会话（任务清单按创建序降序）：已结束的任务会一直累积，每条消息都扫全部
/// 不划算；更早的任务 Router 本来也只取最近几个已结束的。
const SCAN_TASKS: usize = 200;
/// 找最近消息时在主线上最多往回看多少条 entry（主线上还有结算锚点等不是消息的 entry）。
const SCAN_ENTRIES: usize = 200;
/// "最后一次往外说的话"往回找多少条 entry。
const SCAN_SAID: usize = 64;

pub(super) struct Built {
    pub input: RouterInput,
    /// 候选的短号 → 任务会话的 conversation。
    pub handles: Handles,
}

pub(super) async fn build(
    core: &DurableCore,
    main: &SessionRecord,
    message: EntryId,
    text: &str,
) -> Result<Built, SessionError> {
    let limits = RouterLimits::default();
    let page = core.task_page(false, Some(SCAN_TASKS), None).await?;
    // 只要这个主会话派出的任务：分发派出的、父是它。`task` 工具在任务会话里派出的子任务、定时任务、
    // 群与话题的会话都不是私聊的候选。
    let mut live = Vec::new();
    let mut finished = Vec::new();
    for summary in page.tasks {
        if summary.source != TaskSource::Dispatched
            || summary.dispatched_by.as_ref().map(|s| s.as_str()) != Some(main.id.as_str())
        {
            continue;
        }
        match summary.state {
            TaskState::Running | TaskState::Idle => live.push((summary, CandidateState::Running)),
            TaskState::Waiting => live.push((summary, CandidateState::Waiting)),
            TaskState::Returned => finished.push((summary, CandidateState::Finished)),
            TaskState::Archived => {}
        }
    }
    // 最近结束的在前，只留要用的个数（`RouterInput::new` 也会截，这里先截省得多读）。
    finished.sort_by(|a, b| b.0.ended_at.cmp(&a.0.ended_at));
    finished.truncate(limits.finished_tasks);

    let mut tasks = Vec::new();
    let mut handles = Handles::new();
    for (summary, state) in live.into_iter().chain(finished) {
        let record = core.session(summary.session.as_str()).await?;
        handles.insert(summary.handle.clone(), record.root_conversation);
        tasks.push(CandidateTask {
            id: summary.handle,
            title: summary.title,
            state,
            last_said: last_said(core, &record).await?,
        });
    }
    let recent = recent_messages(core, main, message).await?;
    Ok(Built {
        input: RouterInput::new(RouteScope::Private, text, tasks, recent, limits),
        handles,
    })
}

/// 任务最近一次对外的回复：最后一条以 `stop` 收尾的 assistant 回复，去掉末行的状态行。没有就是空。
async fn last_said(core: &DurableCore, task: &SessionRecord) -> Result<String, SessionError> {
    let query = EntryQuery {
        order: Some(ScanOrder::Descending),
        ..EntryQuery::new(task.current_branch)
    };
    let page = core.storage.scan_entries(&query, SCAN_SAID, None).await?;
    for entry in page.items {
        for message in entry.model.iter().flatten() {
            let Message::Assistant(message) = message else {
                continue;
            };
            if message.stop_reason != StopReason::Stop {
                continue;
            }
            let text = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let said = strip_status(&text);
            if !said.is_empty() {
                return Ok(said);
            }
        }
    }
    Ok(String::new())
}

/// 去掉末行的状态行（给程序看的，Router 不需要）。
fn strip_status(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(last) = lines.iter().rposition(|line| !line.trim().is_empty()) else {
        return String::new();
    };
    let line: String = lines[last]
        .chars()
        .filter(|c| !matches!(c, '*' | '`' | '_'))
        .collect();
    let line = line.trim().trim_start_matches(['>', '-', '#', ' ']);
    if line.starts_with(TASK_STATUS_DONE) || line.starts_with(TASK_STATUS_BLOCKED) {
        lines[..last].join("\n").trim().to_owned()
    } else {
        text.trim().to_owned()
    }
}

/// 主线上最近的消息（旧的在前）：操作者发的与投回主线的回复，不含正在分发的这一条。
async fn recent_messages(
    core: &DurableCore,
    main: &SessionRecord,
    current: EntryId,
) -> Result<Vec<RecentMessage>, SessionError> {
    let query = EntryQuery {
        max: Some(EntryId::new(current.get().saturating_sub(1))),
        order: Some(ScanOrder::Descending),
        ..EntryQuery::new(main.current_branch)
    };
    let page = core
        .storage
        .scan_entries(&query, SCAN_ENTRIES, None)
        .await?;
    let want = RouterLimits::default().recent_messages;
    let mut out = Vec::new();
    for entry in page.items {
        let speaker = match entry.kind.as_str() {
            MAIN_USER_ENTRY => Speaker::Operator,
            MAIN_REPLY_ENTRY => Speaker::Agent,
            _ => continue,
        };
        let text = entry
            .data
            .as_ref()
            .and_then(|data| data.get("text"))
            .and_then(|text| text.as_str())
            .unwrap_or_default()
            .trim();
        if text.is_empty() {
            continue;
        }
        out.push(RecentMessage {
            speaker,
            text: text.to_owned(),
        });
        if out.len() >= want {
            break;
        }
    }
    out.reverse();
    Ok(out)
}

pub(super) fn speaker(speaker: Speaker) -> &'static str {
    match speaker {
        Speaker::Operator => "用户",
        Speaker::Agent => "你",
        Speaker::Member => "群友",
    }
}

/// `list` 的清单：一段自然的话，用标题不带任务号，分"正在做的"和"等你回话的"；都没有就说手上没有事。
pub(super) fn render_list(tasks: &[CandidateTask]) -> String {
    let titled = |state: CandidateState| -> Vec<&str> {
        tasks
            .iter()
            .filter(|task| task.state == state)
            .map(|task| task.title.as_str())
            .collect()
    };
    let (running, waiting) = (
        titled(CandidateState::Running),
        titled(CandidateState::Waiting),
    );
    if running.is_empty() && waiting.is_empty() {
        return "现在手上没有事。".to_owned();
    }
    let mut parts = Vec::new();
    for (head, titles) in [("正在做的：", running), ("等你回话的：", waiting)] {
        if titles.is_empty() {
            continue;
        }
        let lines: Vec<String> = titles.iter().map(|title| format!("· {title}")).collect();
        parts.push(format!("{head}\n{}", lines.join("\n")));
    }
    parts.join("\n\n")
}
