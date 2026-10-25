//! 任务会话的展示（`docs/storage-durable-d5.md` §4 W2c；决定 5）：列表与查进度用的格式。
//!
//! 派任务、接着做、查进度、交回结果都在 runtime（`harness/orchestration/task.rs`）；任务会话在
//! `sessions` 里的行（`kind = task`、UUIDv7、`parent_session`、标题）也是 runtime 派出那次提交里
//! 写的。这里只给 gateway 要**说给人听**的那几句：
//!
//! - **短号统一是 `#<子 conversation ID>`**（[`handle`] / [`parse_handle`]）：模型在回执里看到的
//!   `已派出 #12`、`task_id: "12"`、监听的 `#id` 同一种写法，不再有旧的"UUID 后四位"。任务会话的根
//!   conversation 就是那个子 conversation（`SessionRecord::root_conversation`）。
//! - [`TaskLine`]：一个任务会话现在的样子（标题、父会话、跑着 / 排队 / 上一次怎么收的尾），
//!   [`TaskLine::render`] 是一行给操作者看的话；`/status`、`/pending` 一类渠道命令（W3a）直接用。
//! - [`DurableCore::task_label`]：任务会话里那些"需要你判断"的消息前面挂的标签
//!   `后台任务 #12「查空调」`，操作者在聊天里同时看着几个任务时靠它分清是哪一个在问。
//!
//! 已知差异：`komo session list` 的"任务·#xxxx"标注由 `komo-client` 按会话 ID 的后四位算（协议冻结，
//! 客户端不动），所以在 Durable 下它与这里的 `#<conversation>` 对不上；以聊天与回执里的为准，
//! 客户端侧留给 D6 之后。

use komo_kernel::durable::{ConversationId, SessionKind, SessionLifecycle, SessionRecord};
use komo_runtime::durable::harness::{Harness, task_doc};

use super::DurableCore;
use super::sessions::SessionError;

/// 任务短号：`#<conversation>`。
pub fn handle(conversation: ConversationId) -> String {
    format!("#{conversation}")
}

/// 解析模型 / 操作者写的任务短号：`12`、`#12`、` #12 `。
pub fn parse_handle(raw: &str) -> Option<ConversationId> {
    raw.trim()
        .trim_start_matches('#')
        .parse::<u64>()
        .ok()
        .map(ConversationId::new)
}

/// 一个任务现在跑到哪了（一句话的口径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskPhase {
    /// 有一条 run 在跑，已经跑了这么多轮。
    Running { rounds: u32 },
    /// 没有在跑的 run，收件箱里排着输入。
    Queued,
    /// 上一条 run 的结局：`completed` / `failed` / `cancelled`。
    Finished(String),
    /// 还没有 run（刚派出）。
    Starting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLine {
    /// 子 conversation，句柄 `#id`。
    pub conversation: ConversationId,
    /// 任务会话的 Session ID。
    pub session: String,
    pub title: String,
    /// 派它的会话。
    pub parent_session: Option<String>,
    /// 会话已被删除 / 正在关闭。
    pub lifecycle: SessionLifecycle,
    pub phase: TaskPhase,
}

impl TaskLine {
    pub fn handle(&self) -> String {
        handle(self.conversation)
    }

    /// `#12「查空调」· 运行中（第 2 轮）`。
    pub fn render(&self) -> String {
        let phase = match &self.phase {
            TaskPhase::Running { rounds } => format!("运行中（已 {rounds} 轮）"),
            TaskPhase::Queued => "排队中".to_owned(),
            TaskPhase::Starting => "刚派出".to_owned(),
            TaskPhase::Finished(state) => match state.as_str() {
                "completed" => "已完成".to_owned(),
                "failed" => "失败".to_owned(),
                "cancelled" => "已取消".to_owned(),
                other => format!("已结束（{other}）"),
            },
        };
        let gone = match self.lifecycle {
            SessionLifecycle::Active => "",
            SessionLifecycle::Closing => "（正在删除）",
            SessionLifecycle::Deleted | SessionLifecycle::Purged => "（已删除）",
        };
        format!("{}「{}」· {phase}{gone}", self.handle(), self.title)
    }
}

impl DurableCore {
    /// 一个 sessions 行的任务视图；不是任务会话是 `None`。
    pub async fn task_line(
        &self,
        record: &SessionRecord,
    ) -> Result<Option<TaskLine>, SessionError> {
        if record.kind != SessionKind::Task {
            return Ok(None);
        }
        let conversation = record.root_conversation;
        let live = Harness::live(&*self.storage, conversation).await?;
        let phase = if let Some(run) = &live.run {
            TaskPhase::Running { rounds: run.rounds }
        } else if !Harness::inbox(&*self.storage, conversation)
            .await?
            .items
            .is_empty()
        {
            TaskPhase::Queued
        } else {
            match task_doc(&*self.storage, conversation)
                .await?
                .and_then(|doc| doc.last_run)
            {
                Some(last) => TaskPhase::Finished(last.state),
                None => TaskPhase::Starting,
            }
        };
        Ok(Some(TaskLine {
            conversation,
            session: record.id.clone(),
            title: record.title.clone().unwrap_or_default(),
            parent_session: record.parent_session.clone(),
            lifecycle: record.lifecycle,
            phase,
        }))
    }

    /// 任务会话列表（最新的在前）；`parent` 给了就只列那个会话派出的。不列已删除的。
    pub async fn task_sessions(&self, parent: Option<&str>) -> Result<Vec<TaskLine>, SessionError> {
        let mut lines = Vec::new();
        for record in self.list_sessions(false).await? {
            if parent.is_some_and(|parent| record.parent_session.as_deref() != Some(parent)) {
                continue;
            }
            if let Some(line) = self.task_line(&record).await? {
                lines.push(line);
            }
        }
        Ok(lines)
    }

    /// 任务会话里那些"需要你判断"的消息前面挂的标签：`后台任务 #12「查空调」`。不是任务会话
    /// （或不存在）就是 `None`。
    pub async fn task_label(&self, session: &str) -> Option<String> {
        let record = self.storage.session(session).await.ok()??;
        if record.kind != SessionKind::Task {
            return None;
        }
        Some(format!(
            "后台任务 {}「{}」",
            handle(record.root_conversation),
            record.title.unwrap_or_default()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_is_the_conversation_number_with_a_hash_and_parses_back_loosely() {
        let conversation = ConversationId::new(12);
        assert_eq!(handle(conversation), "#12");
        for raw in ["12", "#12", "  #12 "] {
            assert_eq!(parse_handle(raw), Some(conversation), "{raw}");
        }
        for raw in ["", "#", "zzzz", "-1", "1a2b"] {
            assert_eq!(parse_handle(raw), None, "{raw}");
        }
    }

    #[test]
    fn a_task_line_reads_as_one_sentence() {
        let mut line = TaskLine {
            conversation: ConversationId::new(12),
            session: "s".into(),
            title: "查空调".into(),
            parent_session: Some("p".into()),
            lifecycle: SessionLifecycle::Active,
            phase: TaskPhase::Running { rounds: 2 },
        };
        assert_eq!(line.render(), "#12「查空调」· 运行中（已 2 轮）");
        line.phase = TaskPhase::Finished("completed".into());
        assert_eq!(line.render(), "#12「查空调」· 已完成");
        line.phase = TaskPhase::Queued;
        line.lifecycle = SessionLifecycle::Deleted;
        assert_eq!(line.render(), "#12「查空调」· 排队中（已删除）");
    }
}
