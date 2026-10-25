//! 持久记录（docs/storage-durable.md §3.2；pi-durable spec §2、§5.1、§6）。
//!
//! 纯值类型。不变量尽量用类型表达：submission 的 input / write 两类各有自己合法的
//! 状态，非法组合写不出来；任务状态把 checkpoint / outcome 放进各自的变体。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use super::message::Message;

// ---------------------------------------------------------------- conversation

/// fork 的来源：子会话读父会话到 `at`（含）为止的 entry。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkPoint {
    pub conversation: ConversationId,
    pub at: EntryId,
}

/// 拥有者：谁的哪个任务建了这个会话。只管归属与自底向上的 abort，不是访问控制。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationOwner {
    pub conversation: ConversationId,
    pub task: TaskId,
}

/// 创建后不可变；任务终态之后 `owner` 也留着。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationRecord {
    pub id: ConversationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ForkPoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<ConversationOwner>,
    /// 这个 conversation 属于哪个 komo Session（`SessionRecord::id`）。创建后不可变；写进
    /// `conversations.session_id` 列，按它建了索引（`ConversationQuery::session`）。fork 出来的
    /// 子 conversation 继承父的 session；嵌套工具与压缩用的 conversation 没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

impl ConversationRecord {
    pub fn root(id: ConversationId) -> Self {
        Self {
            id,
            parent: None,
            owner: None,
            session: None,
        }
    }

    pub fn fork(id: ConversationId, parent: ConversationId, at: EntryId) -> Self {
        Self {
            id,
            parent: Some(ForkPoint {
                conversation: parent,
                at,
            }),
            owner: None,
            session: None,
        }
    }

    pub fn in_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }
}

// ----------------------------------------------------------------------- entry

/// 对某条 entry 的上下文编辑：同一目标取最新一条。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextEdit {
    pub target: EntryId,
    #[serde(flatten)]
    pub action: EditAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum EditAction {
    /// 目标不再贡献任何模型消息。
    Omit,
    /// 目标改为贡献这些消息。
    Replace { messages: Vec<Message> },
}

impl ContextEdit {
    pub fn omit(target: EntryId) -> Self {
        Self {
            target,
            action: EditAction::Omit,
        }
    }

    pub fn replace(target: EntryId, messages: Vec<Message>) -> Self {
        Self {
            target,
            action: EditAction::Replace { messages },
        }
    }
}

/// 不可变的 transcript 记录；ID 在整个库里全局、有序、不复用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryRecord {
    pub id: EntryId,
    pub conversation: ConversationId,
    pub kind: String,
    /// 进模型上下文的消息；没有 `model` 的 entry 只是记录，不进请求。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Vec<Message>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// 后续上下文从这条 entry 起算（`head == id` 即"自此重新开始"）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<EntryId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<ContextEdit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_task: Option<TaskId>,
}

impl EntryRecord {
    pub fn new(id: EntryId, conversation: ConversationId, kind: impl Into<String>) -> Self {
        Self {
            id,
            conversation,
            kind: kind.into(),
            model: None,
            data: None,
            head: None,
            edits: Vec::new(),
            by_task: None,
        }
    }

    pub fn with_model(mut self, model: Vec<Message>) -> Self {
        self.model = Some(model);
        self
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn with_head(mut self, head: EntryId) -> Self {
        self.head = Some(head);
        self
    }

    pub fn with_edits(mut self, edits: Vec<ContextEdit>) -> Self {
        self.edits = edits;
        self
    }
}

// ------------------------------------------------------------------------ task

/// 任务的终局原因。`result` 是任务自己的返回值，用 JSON。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum TaskOutcome {
    Completed {
        result: Value,
    },
    Failed {
        error: TaskOutcomeError,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    /// 显式取消，由任务自己的 abort 协议处理。
    Aborted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    /// 重启后被放弃，或所有者已终结而它还没跑完。
    Orphaned {
        reason: String,
    },
    /// 任务代码本身出错（phase 抛错、迁移失败），只让这个单元失败。
    Faulted {
        error: TaskOutcomeError,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOutcomeError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

/// 等待的汇合策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JoinPolicy {
    FailFast,
    AllSettled,
}

/// 任务的完整执行状态。`running` 只是"某次调用正在跑"，重启后一律回 `pending`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum TaskState {
    Pending {
        checkpoint: Value,
    },
    Running {
        checkpoint: Value,
    },
    /// 挂起、不占调用，等 `on` 里的任务都终结后从 `checkpoint` 继续。
    Waiting {
        checkpoint: Value,
        on: Vec<TaskId>,
        policy: JoinPolicy,
    },
    /// 结局已定，等自己名下还活着的任务收尾后才算终结；不再跑代码。
    Completing {
        outcome: TaskOutcome,
    },
    Terminal {
        outcome: TaskOutcome,
    },
}

/// `TaskState` 的判别，用于查询。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskStatus {
    Pending,
    Running,
    Waiting,
    Completing,
    Terminal,
}

impl TaskState {
    pub fn status(&self) -> TaskStatus {
        match self {
            TaskState::Pending { .. } => TaskStatus::Pending,
            TaskState::Running { .. } => TaskStatus::Running,
            TaskState::Waiting { .. } => TaskStatus::Waiting,
            TaskState::Completing { .. } => TaskStatus::Completing,
            TaskState::Terminal { .. } => TaskStatus::Terminal,
        }
    }
}

/// 重启放弃（而非用户请求 abort）时记在 abort 标记上的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AbortReason {
    Restart,
}

/// 整条任务记录；每次状态变化整条替换。
///
/// `memos` 只在 pending / running / waiting 时有意义（先写者赢的幂等备忘），
/// completing / terminal 时为 `None`。这条由调度器保证，类型不强制。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: TaskId,
    pub conversation: ConversationId,
    pub kind: String,
    pub version: u32,
    pub input: Value,
    /// 拥有它的任务；顶层任务没有。不可变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<TaskId>,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub abort_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_reason: Option<AbortReason>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub abandon_on_restart: bool,
    /// 第一次进入 running 的时刻（毫秒），等待与恢复期间保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memos: Option<BTreeMap<String, Value>>,
}

impl TaskRecord {
    /// 一个刚建好的 pending 任务。
    pub fn pending(
        id: TaskId,
        conversation: ConversationId,
        kind: impl Into<String>,
        input: Value,
        checkpoint: Value,
    ) -> Self {
        Self {
            id,
            conversation,
            kind: kind.into(),
            version: 1,
            input,
            owner: None,
            background: false,
            abort_requested: false,
            abort_reason: None,
            abandon_on_restart: false,
            started_at: None,
            ended_at: None,
            state: TaskState::Pending { checkpoint },
            memos: None,
        }
    }
}

// ------------------------------------------------------------------ submission

/// input 提交的生命周期：`queued → placed → done`，或在任意处 `unanswered`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum InputState {
    Queued,
    /// 输入 entry 已落进 transcript，run 在跑。
    Placed {
        entry: EntryId,
    },
    Done {
        entry: EntryId,
        answer: EntryId,
    },
    Unanswered {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<EntryId>,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Value>,
    },
}

/// write 提交只是被动写入一条 entry：没有 `placed`，也没有 `answer`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum WriteState {
    Queued,
    Done {
        entry: EntryId,
    },
    Unanswered {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SubmissionKind {
    Input {
        #[serde(flatten)]
        state: InputState,
    },
    Write {
        #[serde(flatten)]
        state: WriteState,
    },
}

/// 收件箱里的一次提交；`request_id` 在 conversation 内去重。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionRecord {
    pub id: SubmissionId,
    pub conversation: ConversationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(flatten)]
    pub kind: SubmissionKind,
}

/// `SubmissionKind` 里状态的判别，用于查询（input 与 write 共用同一组名字）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubmissionStatus {
    Queued,
    Placed,
    Done,
    Unanswered,
}

/// 一次提交的结算结果（run 收尾时给 input 提交盖的章）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum SubmissionSettlement {
    Done {
        answer: EntryId,
    },
    Unanswered {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Value>,
    },
}

impl SubmissionRecord {
    pub fn queued_input(
        id: SubmissionId,
        conversation: ConversationId,
        request_id: Option<&str>,
    ) -> Self {
        Self {
            id,
            conversation,
            request_id: request_id.map(str::to_owned),
            kind: SubmissionKind::Input {
                state: InputState::Queued,
            },
        }
    }

    pub fn queued_write(
        id: SubmissionId,
        conversation: ConversationId,
        request_id: Option<&str>,
    ) -> Self {
        Self {
            id,
            conversation,
            request_id: request_id.map(str::to_owned),
            kind: SubmissionKind::Write {
                state: WriteState::Queued,
            },
        }
    }

    pub fn status(&self) -> SubmissionStatus {
        match &self.kind {
            SubmissionKind::Input { state } => match state {
                InputState::Queued => SubmissionStatus::Queued,
                InputState::Placed { .. } => SubmissionStatus::Placed,
                InputState::Done { .. } => SubmissionStatus::Done,
                InputState::Unanswered { .. } => SubmissionStatus::Unanswered,
            },
            SubmissionKind::Write { state } => match state {
                WriteState::Queued => SubmissionStatus::Queued,
                WriteState::Done { .. } => SubmissionStatus::Done,
                WriteState::Unanswered { .. } => SubmissionStatus::Unanswered,
            },
        }
    }
}

// -------------------------------------------------------------------- document

/// document 的作用域。conversation 级的不随 fork 拷贝，由 fork 提交自己决定要不要建。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DocumentScope {
    Session,
    Conversation { conversation: ConversationId },
    Task { task: TaskId },
}

/// 一个 document 的逻辑地址：`key` 为空是单例，否则是 family 里的一员。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocumentAddress {
    pub kind: String,
    pub scope: DocumentScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// document 的一次生命周期（create → retire）及其当前整值。
///
/// 只存整值，每次 change 整个替换；没有 delta、没有历史回看（§2、§3.3）。
/// `version` 是定义版本，迁移由调用方在读到旧版本时自己做。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentRecord {
    pub id: DocumentId,
    pub address: DocumentAddress,
    pub version: u32,
    pub value: Value,
    /// 创建它的提交，由存储盖章。
    pub created_at: Seq,
    /// 最近一次 change 的提交，由存储盖章。
    pub changed_at: Seq,
    /// 退役它的提交；活着时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<Seq>,
}

impl DocumentRecord {
    pub fn is_alive(&self) -> bool {
        self.retired_at.is_none()
    }
}

// --------------------------------------------------------------------- session

/// komo Session 的生命周期（§8.10）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionLifecycle {
    Active,
    Closing,
    Deleted,
    Purged,
}

/// komo Session 的种类（D5 契约，`docs/storage-durable-d5.md` §2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionKind {
    /// 渠道 peer 的主会话。
    Main,
    /// 普通会话（TUI / CLI 新建、群与话题）。
    #[default]
    Normal,
    /// 后台任务的会话（`task` 工具派出的子 conversation 那一个）。
    Task,
    /// cron 触发建出来的会话。
    Cron,
}

/// 对外的 komo Session：id 是 UUIDv7 字符串（不在数字命名空间里），整条替换。
///
/// 一个 Session 是一组 conversation 加一个"当前分支"指针（§4.2）。D5 起带上 gateway 要的展示与
/// 路由信息，全部只放在 `record` JSON 里（不改 DDL）；新字段都有默认值，旧记录照样读得出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub id: String,
    pub lifecycle: SessionLifecycle,
    /// 下一条输入落在哪个分支（导航只改它，不追加 entry）。
    pub current_branch: ConversationId,
    pub root_conversation: ConversationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 来源（channel / cli / cron……），调用方自己的约定；渠道会话如 `chat:telegram:123`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub kind: SessionKind,
    /// 会话用的 Agent 名（`komo.agent` document 里也有，这里供列表与按 Agent 找主会话用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// 派生它的会话（任务会话指向父会话）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// 会话树里导航到的位置；没有就是当前分支的末端（导航只改它，不追加 entry）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf: Option<EntryId>,
    /// 标题还是临时的（取自第一条消息，等 `titles` 换成模型起的）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub title_provisional: bool,
    /// 创建与最近一次改动的时刻（毫秒）；旧记录没有，读成 0。
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub changed_at: i64,
    /// 创建请求的幂等键（同一键同一内容是同一个会话）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<String>,
}

impl SessionRecord {
    /// 一个刚建的、活着的会话：当前分支就是根 conversation，其余字段取默认。
    pub fn new(id: impl Into<String>, root_conversation: ConversationId) -> Self {
        Self {
            id: id.into(),
            lifecycle: SessionLifecycle::Active,
            current_branch: root_conversation,
            root_conversation,
            title: None,
            source: None,
            cwd: None,
            kind: SessionKind::default(),
            agent: None,
            parent_session: None,
            leaf: None,
            title_provisional: false,
            created_at: 0,
            changed_at: 0,
            request_key: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn submission_records_round_trip_in_a_flat_shape() {
        let placed = SubmissionRecord {
            id: SubmissionId::new(5),
            conversation: ConversationId::new(1),
            request_id: Some("r".into()),
            kind: SubmissionKind::Input {
                state: InputState::Placed {
                    entry: EntryId::new(4),
                },
            },
        };
        let expected = json!({
            "id": 5, "conversation": 1, "requestId": "r",
            "type": "input", "status": "placed", "entry": 4,
        });
        assert_eq!(serde_json::to_value(&placed).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<SubmissionRecord>(expected).unwrap(),
            placed
        );
        assert_eq!(placed.status(), SubmissionStatus::Placed);
    }

    #[test]
    fn write_submissions_have_no_placed_or_answer() {
        let placed =
            json!({"id": 5, "conversation": 1, "type": "write", "status": "placed", "entry": 4});
        assert!(serde_json::from_value::<SubmissionRecord>(placed).is_err());
        let done =
            json!({"id": 5, "conversation": 1, "type": "write", "status": "done", "entry": 4});
        let record: SubmissionRecord = serde_json::from_value(done).unwrap();
        assert_eq!(record.status(), SubmissionStatus::Done);
    }

    #[test]
    fn task_state_and_outcome_round_trip() {
        let mut task = TaskRecord::pending(
            TaskId::new(3),
            ConversationId::new(1),
            "komo.tool",
            json!({"call": "c1"}),
            json!({"phase": "call"}),
        );
        task.state = TaskState::Waiting {
            checkpoint: json!({"phase": "tools"}),
            on: vec![TaskId::new(4)],
            policy: JoinPolicy::AllSettled,
        };
        let text = serde_json::to_string(&task).unwrap();
        assert_eq!(serde_json::from_str::<TaskRecord>(&text).unwrap(), task);
        task.state = TaskState::Terminal {
            outcome: TaskOutcome::Failed {
                error: TaskOutcomeError {
                    message: "boom".into(),
                    detail: None,
                },
                result: None,
            },
        };
        let text = serde_json::to_string(&task).unwrap();
        assert_eq!(serde_json::from_str::<TaskRecord>(&text).unwrap(), task);
        assert_eq!(task.state.status(), TaskStatus::Terminal);
    }

    #[test]
    fn session_records_gain_fields_without_breaking_old_json() {
        // D4 的记录没有 kind / leaf / 时间戳等字段：照样读得出，读成默认值。
        let old = json!({
            "id": "s1", "lifecycle": "active", "currentBranch": 7, "rootConversation": 2,
            "title": "t", "source": "cli", "cwd": "/w",
        });
        let record: SessionRecord = serde_json::from_value(old).unwrap();
        assert_eq!(record.kind, SessionKind::Normal);
        assert_eq!(record.leaf, None);
        assert_eq!((record.created_at, record.changed_at), (0, 0));
        assert!(!record.title_provisional);

        let mut full = SessionRecord::new("s2", ConversationId::new(2));
        full.kind = SessionKind::Task;
        full.agent = Some("coder".into());
        full.parent_session = Some("s1".into());
        full.leaf = Some(EntryId::new(9));
        full.title_provisional = true;
        full.created_at = 10;
        full.changed_at = 20;
        full.request_key = Some("k".into());
        let value = serde_json::to_value(&full).unwrap();
        assert_eq!(value["kind"], "task");
        assert_eq!(value["parentSession"], "s1");
        assert_eq!(value["leaf"], 9);
        assert_eq!(
            serde_json::from_value::<SessionRecord>(value).unwrap(),
            full
        );
    }

    #[test]
    fn conversation_records_keep_their_session_and_old_json_has_none() {
        let record = ConversationRecord::root(ConversationId::new(2)).in_session("s1");
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["session"], "s1");
        assert_eq!(
            serde_json::from_value::<ConversationRecord>(value).unwrap(),
            record
        );
        let old: ConversationRecord = serde_json::from_value(json!({"id": 2})).unwrap();
        assert_eq!(old.session, None);
    }

    #[test]
    fn entry_with_edits_round_trips() {
        let entry = EntryRecord::new(EntryId::new(9), ConversationId::new(1), "komo.user")
            .with_model(vec![Message::user("hi", 1)])
            .with_head(EntryId::new(9))
            .with_edits(vec![
                ContextEdit::omit(EntryId::new(2)),
                ContextEdit::replace(EntryId::new(3), vec![Message::user("short", 2)]),
            ]);
        let text = serde_json::to_string(&entry).unwrap();
        assert_eq!(serde_json::from_str::<EntryRecord>(&text).unwrap(), entry);
        assert!(text.contains(r#""action":"omit""#));
    }
}
