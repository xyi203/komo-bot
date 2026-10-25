//! 收件箱、boundary 与受理（`docs/storage-durable.md` §4.3；pi-durable spec §6，
//! `harness/{inbox,submissions}.ts`）。
//!
//! 排队的 submission 在 conversation 级的 `komo.inbox` document 里，一个按 ID 序的列表：
//!
//! ```json
//! { "items": [
//!   { "id": 5, "mode": "steer",    "content": "换个思路", "entry": { "kind": "channel.msg", "data": {…} } },
//!   { "id": 6, "mode": "followUp", "content": "再看看 B" },
//!   { "id": 7, "mode": "write",    "entry": { "kind": "komo.reset", "head": "self" } }
//! ] }
//! ```
//!
//! 受理（[`admit`]）与 boundary（[`apply_boundary`]）都是**一次提交里的闭包**：先做表读
//! （请求键、最新的 head 标记），再写。
//!
//! | 动作 | submission | 其他写入 |
//! |---|---|---|
//! | 输入，空闲且收件箱空 | `placed`（带 user entry） | 建 run 与生成任务 |
//! | 只用于展示的 write（无 `model` / `head` / `edits`），没有排队的 reset | `done` | 追加 entry，**忙闲都立刻落** |
//! | 空闲且收件箱空的其他 write | `done`；head 指到当前范围之前则 `unanswered{stale}` | 追加 entry |
//! | 输入或别的 write，忙或收件箱非空 | `queued` | 往 `komo.inbox` 加一项 |
//! | boundary 放置 write / 用户项 | `done` / `placed` | 追加 entry，用户项加进当前或后继 run |
//! | run 回答 | 输入 `done` | 移除 `run`，或交给后继 |
//! | run 失败 / 它的任务 abort（含 `Harness::abort_run`） | 输入 `unanswered`（原因） | 移除 `run`；**同一次提交**按 final boundary 放全部排队项（先 write，再内部与用户输入，按 ID 序），放了输入就起后继 run |
//! | `Conversation.abort()`（`abort_conversation`） | 排队的用户输入 `unanswered{aborted}` | 先从收件箱移除用户输入；write 与内部输入留着，随后由 run 的收尾放置 |
//! | 撤回排队项（显式，单条） | `unanswered{aborted}` | 移除这一项（内部输入也行） |
//!
//! 每结算一个**输入**（上表里所有 `done` / `unanswered`，含排队中被撤回的），[`settle`] 同一次提交
//! 追加一条只展示的 `komo.settled`（见 [`entries`]），write 不追加。
//!
//! **非正常收尾不挡后面的消息**（D5 决定 8，恢复旧 komo 的行为）：run 因取消、模型失败、预算、
//! fault、orphan 收尾时，收件箱里排着的东西——用户的 steer / followUp、内部输入、write——都在收尾的
//! 同一次提交里按 final boundary 放置，有输入放置就起后继 run；不再等"下一条用户输入"才顺带放。
//! 队列模式照旧（`OneAtATime` 时每种取第一项，余下的留给后继 run 自己的 boundary），所以每个后继
//! run 至少消耗一个排队的输入，链条有限、不会死循环。放置在收尾那次提交里做（boundary 的表读在
//! 这次提交的第一次表写之前），没有"run 结束了、后继没起"的崩溃窗口。
//!
//! 谁来撤回排队的输入是调用方的选择：[`Harness::abort_run`](super::Harness::abort_run)（`/cancel`、
//! Esc）只停当前这一轮、**不撤**收件箱，排着的消息接着跑；`abort_conversation`（删除会话等）先撤用户
//! 输入再 abort，内部输入仍然保留。单条排队项用 [`withdraw`]。
//!
//! **内部输入**（`internal: true`）是系统往 conversation 里递的 steer / followUp：后台任务交回的
//! 结果（`task.result`）、监听的事件与结束消息（`monitor.event` / `monitor.end`）。它们不是用户
//! 说的话，所以 `abort_conversation` 不撤它们——结果丢了就没人转告用户。这是对 pi 的有意偏离，
//! 见 `docs/storage-durable.md` §13.2。
//!
//! boundary 的选择是按 ID 序的确定规则：`postTools` 放全部 write 与 steer，`final` 再加
//! followUp；每种模式按设置取第一项或全部。先放 write 再放用户项，所以排在 reset 前的用户项
//! 落在 reset 之后、在新上下文里跑。

use komo_kernel::durable::{
    Content, ConversationId, DocumentAddress, DocumentScope, EntryId, InputState, SubmissionId,
    SubmissionKind, SubmissionRecord, SubmissionSettlement, WriteState,
};
use komo_kernel::durable::{ContextEdit, Message};
use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::hooks::QueueMode;
use super::live::{self, LiveState, RunControl};
use super::{GENERATION_KIND, entries};
use crate::durable::error::DurableError;
use crate::durable::kind::TaskKind;
use crate::durable::tx::{DraftHead, EntryDraft, Ownership, TaskOptions, Tx};

pub const INBOX_KIND: &str = "komo.inbox";
pub const INBOX_VERSION: u32 = 1;

pub fn inbox_address(conversation: ConversationId) -> DocumentAddress {
    DocumentAddress {
        kind: INBOX_KIND.to_owned(),
        scope: DocumentScope::Conversation { conversation },
        key: None,
    }
}

/// 输入放置成的 entry 的 kind 与 data（不是 `komo.user` 时）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputEntry {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl InputEntry {
    pub fn new(kind: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            kind: kind.into(),
            data,
        }
    }
}

/// write 的 `head`：指自己（`"self"`，"自此重新开始"）或某条已有 entry。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteHead {
    SelfHead,
    At(EntryId),
}

impl Serialize for WriteHead {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            WriteHead::SelfHead => serializer.serialize_str("self"),
            WriteHead::At(id) => id.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for WriteHead {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(text) if text == "self" => Ok(WriteHead::SelfHead),
            Value::Number(number) => number
                .as_u64()
                .map(|id| WriteHead::At(EntryId::new(id)))
                .ok_or_else(|| de::Error::custom("head must be a non-negative integer")),
            other => Err(de::Error::custom(format!(
                "head must be \"self\" or an entry ID, got {other}"
            ))),
        }
    }
}

/// 一次 write submission 要追加的 entry（存成普通 JSON）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteDraft {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Vec<Message>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<WriteHead>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<Vec<ContextEdit>>,
}

impl WriteDraft {
    /// 只带 `kind` 与 `data` 的展示用 entry（`label`、`session_info`）。
    pub fn display(kind: impl Into<String>, data: Value) -> Self {
        Self {
            kind: kind.into(),
            model: None,
            data: Some(data),
            head: None,
            edits: None,
        }
    }

    /// `/new`：`komo.reset`，`head: "self"`，可带交接文字。
    pub fn reset(handoff: Option<Message>) -> Self {
        Self {
            kind: entries::RESET.to_owned(),
            model: handoff.map(|message| vec![message]),
            data: None,
            head: Some(WriteHead::SelfHead),
            edits: None,
        }
    }

    /// 没有 `model` / `head` / `edits`（给了空的也算有）：不改任何模型上下文，不需要 boundary。
    pub fn is_display_only(&self) -> bool {
        self.model.is_none() && self.head.is_none() && self.edits.is_none()
    }

    fn is_reset(&self) -> bool {
        self.head == Some(WriteHead::SelfHead)
    }

    pub(crate) fn to_entry_draft(&self) -> EntryDraft {
        EntryDraft {
            kind: self.kind.clone(),
            model: self.model.clone(),
            data: self.data.clone(),
            head: self.head.map(|head| match head {
                WriteHead::SelfHead => DraftHead::SelfHead,
                WriteHead::At(id) => DraftHead::At(id),
            }),
            edits: self.edits.clone().unwrap_or_default(),
        }
    }
}

/// 一项排队的 submission。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum InboxItem {
    Steer {
        id: SubmissionId,
        content: Content,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<InputEntry>,
        /// 系统递的内部输入（见模块文档）。旧数据没有这个字段，按用户输入读。
        #[serde(default, skip_serializing_if = "is_false")]
        internal: bool,
    },
    FollowUp {
        id: SubmissionId,
        content: Content,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<InputEntry>,
        #[serde(default, skip_serializing_if = "is_false")]
        internal: bool,
    },
    Write {
        id: SubmissionId,
        entry: WriteDraft,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl InboxItem {
    /// 是不是系统递的内部输入（write 不算：它不是输入）。
    pub fn is_internal(&self) -> bool {
        matches!(
            self,
            InboxItem::Steer { internal: true, .. } | InboxItem::FollowUp { internal: true, .. }
        )
    }

    pub fn id(&self) -> SubmissionId {
        match self {
            InboxItem::Steer { id, .. }
            | InboxItem::FollowUp { id, .. }
            | InboxItem::Write { id, .. } => *id,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InboxState {
    pub items: Vec<InboxItem>,
}

impl InboxState {
    /// 有没有排着队的 reset：它落下之前追加的 entry 会落在它开始的上下文之外。
    pub fn has_queued_reset(&self) -> bool {
        queued_reset(&self.items)
    }
}

fn queued_reset(items: &[InboxItem]) -> bool {
    items
        .iter()
        .any(|item| matches!(item, InboxItem::Write { entry, .. } if entry.is_reset()))
}

pub async fn load(tx: &mut Tx, conversation: ConversationId) -> Result<InboxState, DurableError> {
    match tx.document(&inbox_address(conversation)).await? {
        None => Ok(InboxState::default()),
        Some((_, value)) => serde_json::from_value(value)
            .map_err(|e| DurableError::rejected(format!("komo.inbox is malformed: {e}"))),
    }
}

pub async fn store(
    tx: &mut Tx,
    conversation: ConversationId,
    state: &InboxState,
) -> Result<(), DurableError> {
    let address = inbox_address(conversation);
    if state.items.is_empty() && tx.document(&address).await?.is_none() {
        return Ok(());
    }
    let value = serde_json::to_value(state).expect("InboxState 可序列化");
    tx.put_document(address, INBOX_VERSION, value).await
}

// ---------------------------------------------------------------- 提交什么

/// 忙时来的输入怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhenBusy {
    /// 排在后面，final boundary 时作为下一条输入开跑。
    #[default]
    FollowUp,
    /// 插话：postTools boundary 并进当前 run。
    Steer,
    /// 忙就拒绝，什么都不写。
    Reject,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubmissionDraft {
    Input {
        content: Content,
        entry: Option<InputEntry>,
        when_busy: WhenBusy,
        request_id: Option<String>,
        /// 内部输入：由调用方（后台任务交回、监听消息）声明，不靠 entry kind 猜。排队后
        /// `Conversation.abort()` 不撤它，run 非正常收尾后用它起后继 run（见模块文档）。
        internal: bool,
    },
    Write {
        entry: WriteDraft,
        request_id: Option<String>,
    },
}

impl SubmissionDraft {
    pub fn input(content: impl Into<String>) -> Self {
        SubmissionDraft::Input {
            content: Content::Text(content.into()),
            entry: None,
            when_busy: WhenBusy::FollowUp,
            request_id: None,
            internal: false,
        }
    }

    pub fn write(entry: WriteDraft) -> Self {
        SubmissionDraft::Write {
            entry,
            request_id: None,
        }
    }

    pub fn steer(mut self) -> Self {
        if let SubmissionDraft::Input { when_busy, .. } = &mut self {
            *when_busy = WhenBusy::Steer;
        }
        self
    }

    /// 标成内部输入（只对输入有意义）。
    pub fn internal(mut self) -> Self {
        if let SubmissionDraft::Input { internal, .. } = &mut self {
            *internal = true;
        }
        self
    }

    pub fn reject_when_busy(mut self) -> Self {
        if let SubmissionDraft::Input { when_busy, .. } = &mut self {
            *when_busy = WhenBusy::Reject;
        }
        self
    }

    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        match &mut self {
            SubmissionDraft::Input { request_id, .. }
            | SubmissionDraft::Write { request_id, .. } => *request_id = Some(id.into()),
        }
        self
    }

    pub fn with_entry(mut self, input_entry: InputEntry) -> Self {
        if let SubmissionDraft::Input { entry, .. } = &mut self {
            *entry = Some(input_entry);
        }
        self
    }

    pub(crate) fn request_id(&self) -> Option<&str> {
        match self {
            SubmissionDraft::Input { request_id, .. }
            | SubmissionDraft::Write { request_id, .. } => request_id.as_deref(),
        }
    }

    /// 输入 entry 的 kind 必须是非空字符串且不占 `komo.` 前缀（那是 Harness 自己的）。
    pub fn check(&self) -> Result<(), String> {
        if let SubmissionDraft::Input {
            entry: Some(entry), ..
        } = self
            && (entry.kind.is_empty() || entry.kind.starts_with("komo."))
        {
            return Err(format!(
                "An input entry's kind must be a non-empty string not starting with \"komo.\", got {:?}",
                entry.kind
            ));
        }
        Ok(())
    }
}

/// 受理的结果：已有或新建的 submission，或忙时被 `Reject` 拒掉（什么都没写）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admitted {
    Id(SubmissionId),
    Busy,
}

// ---------------------------------------------------------------- boundary

/// 队列模式（每个 boundary 现读）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueModes {
    pub steering: QueueMode,
    pub follow_up: QueueMode,
}

/// boundary 的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At {
    PostTools,
    /// run 收尾（回答，或取消、失败、预算、fault、orphan 的非正常收尾）：全部 write，steer 与
    /// followUp 各按设置取第一项或全部。非正常收尾不挡后面的消息，同样用它放置后继 run 的输入。
    Final,
}

/// boundary 在第一次表写之前读的东西，与它见到的最新 head。
pub struct Boundary {
    pub conversation: ConversationId,
    pub inbox: InboxState,
    pub modes: QueueModes,
    /// 当前范围的起点：最新 head 标记的 `head`；本次提交里放置的 head write 会往前推。
    pub head: Option<EntryId>,
}

/// 选中的用户项（按 ID 序）与是否放置了 reset。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoundaryResult {
    pub users: Vec<SubmissionId>,
    pub reset: bool,
}

/// 表读必须先于本次提交的第一次表写，所以调用方在闭包开头准备 boundary。
pub async fn prepare_boundary(
    tx: &mut Tx,
    conversation: ConversationId,
    modes: QueueModes,
) -> Result<Boundary, DurableError> {
    let head = tx
        .latest_head_marker(conversation)
        .await?
        .and_then(|marker| marker.head);
    let inbox = load(tx, conversation).await?;
    Ok(Boundary {
        conversation,
        inbox,
        modes,
        head,
    })
}

/// head 写入指到当前范围起点之前：放置它会把被截掉的历史带回来。`head: "self"` 永远不过期。
fn is_stale(boundary: &Boundary, entry: &WriteDraft) -> bool {
    matches!(
        (entry.head, boundary.head),
        (Some(WriteHead::At(target)), Some(start)) if target < start
    )
}

/// 放置 boundary 选中的项（spec §6）：全部 write、第一个或全部 steer、`final` 时再加
/// followUp；选中的 reset 把 `postTools` 变成 `final`。先 write 后用户项，各按 ID 序。
/// 选中的与过期的项按位置移除，剩下的次序不变；收件箱写回。
pub async fn apply_boundary(
    tx: &mut Tx,
    boundary: &mut Boundary,
    at: At,
    now: i64,
) -> Result<BoundaryResult, DurableError> {
    let conversation = boundary.conversation;
    let items = std::mem::take(&mut boundary.inbox.items);
    let reset = queued_reset(&items);
    let final_boundary = at == At::Final || reset;

    let pick = |wanted: fn(&InboxItem) -> bool, mode: QueueMode| -> Vec<usize> {
        let indexes: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| wanted(item))
            .map(|(index, _)| index)
            .collect();
        match mode {
            QueueMode::All => indexes,
            QueueMode::OneAtATime => indexes.into_iter().take(1).collect(),
        }
    };
    let writes: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| matches!(item, InboxItem::Write { .. }))
        .map(|(index, _)| index)
        .collect();
    let mut users = pick(
        |item| matches!(item, InboxItem::Steer { .. }),
        boundary.modes.steering,
    );
    if final_boundary {
        users.extend(pick(
            |item| matches!(item, InboxItem::FollowUp { .. }),
            boundary.modes.follow_up,
        ));
    }
    users.sort_unstable();

    for &index in &writes {
        let InboxItem::Write { id, entry } = &items[index] else {
            continue;
        };
        if is_stale(boundary, entry) {
            settle(
                tx,
                *id,
                &SubmissionSettlement::Unanswered {
                    reason: "stale".to_owned(),
                    detail: None,
                },
            )
            .await?;
            continue;
        }
        let record = tx
            .append_entry(conversation, entry.to_entry_draft())
            .await?;
        if let Some(head) = entry.head {
            boundary.head = Some(match head {
                WriteHead::SelfHead => record.id,
                WriteHead::At(target) => target,
            });
        }
        place(tx, *id, record.id).await?;
    }
    let mut placed = Vec::new();
    for &index in &users {
        let (id, content, entry) = match &items[index] {
            InboxItem::Steer {
                id, content, entry, ..
            }
            | InboxItem::FollowUp {
                id, content, entry, ..
            } => (*id, content, entry.as_ref()),
            InboxItem::Write { .. } => continue,
        };
        let record = tx
            .append_entry(conversation, entries::input_draft(content, entry, now))
            .await?;
        place(tx, id, record.id).await?;
        placed.push(id);
    }
    let mut removed: Vec<usize> = writes.into_iter().chain(users).collect();
    removed.sort_unstable_by(|a, b| b.cmp(a));
    let mut remaining = items;
    for index in removed {
        remaining.remove(index);
    }
    boundary.inbox.items = remaining;
    store(tx, conversation, &boundary.inbox).await?;
    Ok(BoundaryResult {
        users: placed,
        reset,
    })
}

// ---------------------------------------------------------------- submission 状态

/// boundary 放置：输入 `queued → placed`，write `queued → done`。
pub async fn place(tx: &mut Tx, id: SubmissionId, entry: EntryId) -> Result<(), DurableError> {
    let Some(mut record) = tx.submission(id).await? else {
        return Err(DurableError::rejected(format!(
            "Submission {id} does not exist"
        )));
    };
    record.kind = match record.kind {
        SubmissionKind::Input {
            state: InputState::Queued,
        } => SubmissionKind::Input {
            state: InputState::Placed { entry },
        },
        SubmissionKind::Write {
            state: WriteState::Queued,
        } => SubmissionKind::Write {
            state: WriteState::Done { entry },
        },
        other => {
            return Err(DurableError::rejected(format!(
                "Submission {id} is not queued: {other:?}"
            )));
        }
    };
    tx.put_submission(record);
    Ok(())
}

/// 按事务里当前的记录 settle 一个输入 / write（候选优先，退到已提交，所以第一次表写之后也
/// 能用）。已经 settle 过的不动：它的 run 收尾时发现输入已被别的东西 settle 了也照常收尾。
///
/// 每结算一个**输入**，同一次提交里往它的 conversation 追加一条 [`entries::SETTLED`]；顺序上排在
/// 这次提交里先追加的 entry（run 的回答）之后、后继放置的输入 entry 之前。
pub async fn settle(
    tx: &mut Tx,
    id: SubmissionId,
    settlement: &SubmissionSettlement,
) -> Result<(), DurableError> {
    let Some(mut record) = tx.submission(id).await? else {
        return Ok(());
    };
    let next = match (&record.kind, settlement) {
        (
            SubmissionKind::Input {
                state: InputState::Placed { entry },
            },
            SubmissionSettlement::Done { answer },
        ) => SubmissionKind::Input {
            state: InputState::Done {
                entry: *entry,
                answer: *answer,
            },
        },
        (
            SubmissionKind::Input {
                state: InputState::Placed { entry },
            },
            SubmissionSettlement::Unanswered { reason, detail },
        ) => SubmissionKind::Input {
            state: InputState::Unanswered {
                entry: Some(*entry),
                reason: reason.clone(),
                detail: detail.clone(),
            },
        },
        (
            SubmissionKind::Input {
                state: InputState::Queued,
            },
            SubmissionSettlement::Unanswered { reason, detail },
        ) => SubmissionKind::Input {
            state: InputState::Unanswered {
                entry: None,
                reason: reason.clone(),
                detail: detail.clone(),
            },
        },
        (
            SubmissionKind::Write {
                state: WriteState::Queued,
            },
            SubmissionSettlement::Unanswered { reason, detail },
        ) => SubmissionKind::Write {
            state: WriteState::Unanswered {
                reason: reason.clone(),
                detail: detail.clone(),
            },
        },
        // 已经 done / unanswered，或不合法的组合（排队的输入不能 done）：不动。
        _ => return Ok(()),
    };
    // 输入结算时同一次提交里留一条只展示的 `komo.settled`（gateway 的投影锚点）；write 不留。
    let anchor = match &next {
        SubmissionKind::Input { state } => entries::settled_draft(id, state),
        SubmissionKind::Write { .. } => None,
    };
    let conversation = record.conversation;
    record.kind = next;
    tx.put_submission(record);
    if let Some(anchor) = anchor {
        tx.append_entry(conversation, anchor).await?;
    }
    Ok(())
}

/// 往 `live` 里开一条 run：建一个 conversation 所有的生成任务，`run` 指向它。
pub async fn start_run(
    tx: &mut Tx,
    generation: &dyn TaskKind,
    conversation: ConversationId,
    live: &mut LiveState,
    inputs: Vec<SubmissionId>,
) -> Result<komo_kernel::durable::TaskId, DurableError> {
    let task = create_generation(tx, generation, conversation).await?;
    live.run = Some(RunControl {
        task,
        inputs,
        rounds: 0,
        tokens: 0,
    });
    Ok(task)
}

/// 一个 conversation 所有的生成任务。
pub async fn create_generation(
    tx: &mut Tx,
    generation: &dyn TaskKind,
    conversation: ConversationId,
) -> Result<komo_kernel::durable::TaskId, DurableError> {
    debug_assert_eq!(generation.name(), GENERATION_KIND);
    tx.create_task(
        generation,
        json!({}),
        Ownership::Conversation,
        TaskOptions {
            conversation: Some(conversation),
            ..TaskOptions::default()
        },
    )
    .await
}

// ---------------------------------------------------------------- 受理

fn submission_type(record: &SubmissionRecord) -> &'static str {
    match record.kind {
        SubmissionKind::Input { .. } => "input",
        SubmissionKind::Write { .. } => "write",
    }
}

/// 在一次提交里受理一个 submission（spec §6）。已知的请求键原样返回已有的那条，什么都不写。
pub async fn admit(
    tx: &mut Tx,
    generation: &dyn TaskKind,
    conversation: ConversationId,
    draft: &SubmissionDraft,
    modes: QueueModes,
) -> Result<Admitted, DurableError> {
    let now = tx.now();
    let request_id = draft.request_id();
    if let Some(request) = request_id
        && let Some(existing) = tx.submission_by_request(conversation, request).await?
    {
        let wanted = match draft {
            SubmissionDraft::Input { .. } => "input",
            SubmissionDraft::Write { .. } => "write",
        };
        if submission_type(&existing) != wanted {
            return Err(DurableError::rejected(format!(
                "Request {request} already identifies a submission of type {}",
                submission_type(&existing)
            )));
        }
        return Ok(Admitted::Id(existing.id));
    }

    // 只用于展示的 write：忙闲都立刻落；只有排着 reset 时才排队，免得落在 reset 之前被切出上下文。
    if let SubmissionDraft::Write { entry, .. } = draft
        && entry.is_display_only()
        && !load(tx, conversation).await?.has_queued_reset()
    {
        let record = tx
            .append_entry(conversation, entry.to_entry_draft())
            .await?;
        let submission = tx
            .create_submission(
                conversation,
                request_id,
                SubmissionKind::Write {
                    state: WriteState::Done { entry: record.id },
                },
            )
            .await?;
        return Ok(Admitted::Id(submission.id));
    }

    let mut live = live::load(tx, conversation).await?;
    let busy = live.is_busy();
    if busy
        && matches!(
            draft,
            SubmissionDraft::Input {
                when_busy: WhenBusy::Reject,
                ..
            }
        )
    {
        return Ok(Admitted::Busy);
    }
    // boundary 要读表，所以先于第一次表写准备；忙时不需要。
    let mut boundary = if busy {
        None
    } else {
        Some(prepare_boundary(tx, conversation, modes).await?)
    };

    if boundary
        .as_ref()
        .is_none_or(|boundary| !boundary.inbox.items.is_empty())
    {
        // 排队：忙，或收件箱里还有等着的项（失败的 run 留下的）。后到的排在它们后面。
        let kind = match draft {
            SubmissionDraft::Input { .. } => SubmissionKind::Input {
                state: InputState::Queued,
            },
            SubmissionDraft::Write { .. } => SubmissionKind::Write {
                state: WriteState::Queued,
            },
        };
        let submission = tx.create_submission(conversation, request_id, kind).await?;
        let item = match draft {
            SubmissionDraft::Write { entry, .. } => InboxItem::Write {
                id: submission.id,
                entry: entry.clone(),
            },
            SubmissionDraft::Input {
                content,
                entry,
                when_busy,
                internal,
                ..
            } => match when_busy {
                WhenBusy::Steer => InboxItem::Steer {
                    id: submission.id,
                    content: content.clone(),
                    entry: entry.clone(),
                    internal: *internal,
                },
                _ => InboxItem::FollowUp {
                    id: submission.id,
                    content: content.clone(),
                    entry: entry.clone(),
                    internal: *internal,
                },
            },
        };
        let Some(mut boundary) = boundary else {
            let mut inbox = load(tx, conversation).await?;
            inbox.items.push(item);
            store(tx, conversation, &inbox).await?;
            return Ok(Admitted::Id(submission.id));
        };
        boundary.inbox.items.push(item);
        let result = apply_boundary(tx, &mut boundary, At::Final, now).await?;
        if !result.users.is_empty() {
            start_run(tx, generation, conversation, &mut live, result.users).await?;
            live::store(tx, conversation, &live).await?;
        }
        return Ok(Admitted::Id(submission.id));
    }

    let boundary = boundary.take().expect("空闲时准备过");
    match draft {
        SubmissionDraft::Write { entry, .. } => {
            if is_stale(&boundary, entry) {
                let submission = tx
                    .create_submission(
                        conversation,
                        request_id,
                        SubmissionKind::Write {
                            state: WriteState::Unanswered {
                                reason: "stale".to_owned(),
                                detail: None,
                            },
                        },
                    )
                    .await?;
                return Ok(Admitted::Id(submission.id));
            }
            let record = tx
                .append_entry(conversation, entry.to_entry_draft())
                .await?;
            let submission = tx
                .create_submission(
                    conversation,
                    request_id,
                    SubmissionKind::Write {
                        state: WriteState::Done { entry: record.id },
                    },
                )
                .await?;
            Ok(Admitted::Id(submission.id))
        }
        SubmissionDraft::Input { content, entry, .. } => {
            let id = place_idle_input(
                tx,
                generation,
                conversation,
                &mut live,
                content,
                entry.as_ref(),
                request_id,
                now,
            )
            .await?;
            live::store(tx, conversation, &live).await?;
            Ok(Admitted::Id(id))
        }
    }
}

/// 空闲、收件箱为空的 conversation 直接放置一条输入：追加 user entry、建 `placed` 的
/// submission、起 run。`admit` 的最后一支，也是 `orchestration` 在刚建出来的子 conversation 里
/// 放第一条输入的路径（那里没有请求键可查、没有 head 可读，也不许有表读）。调用方随后把
/// `live` 写回。
#[allow(clippy::too_many_arguments)]
pub async fn place_idle_input(
    tx: &mut Tx,
    generation: &dyn TaskKind,
    conversation: ConversationId,
    live: &mut LiveState,
    content: &Content,
    entry: Option<&InputEntry>,
    request_id: Option<&str>,
    now: i64,
) -> Result<SubmissionId, DurableError> {
    let record = tx
        .append_entry(conversation, entries::input_draft(content, entry, now))
        .await?;
    let submission = tx
        .create_submission(
            conversation,
            request_id,
            SubmissionKind::Input {
                state: InputState::Placed { entry: record.id },
            },
        )
        .await?;
    start_run(tx, generation, conversation, live, vec![submission.id]).await?;
    Ok(submission.id)
}

/// 撤回一个还在排队的 submission：从收件箱移除，settle `unanswered{aborted}`。
pub enum Withdrawn {
    Aborted,
    AlreadyPlaced,
    Settled,
    NotFound,
}

pub async fn withdraw(
    tx: &mut Tx,
    id: SubmissionId,
    conversation: Option<ConversationId>,
) -> Result<Withdrawn, DurableError> {
    let Some(record) = tx.submission(id).await? else {
        return Ok(Withdrawn::NotFound);
    };
    if conversation.is_some_and(|conversation| record.conversation != conversation) {
        return Ok(Withdrawn::NotFound);
    }
    match record.status() {
        komo_kernel::durable::SubmissionStatus::Queued => {
            settle(
                tx,
                id,
                &SubmissionSettlement::Unanswered {
                    reason: "aborted".to_owned(),
                    detail: None,
                },
            )
            .await?;
            let mut inbox = load(tx, record.conversation).await?;
            inbox.items.retain(|item| item.id() != id);
            store(tx, record.conversation, &inbox).await?;
            Ok(Withdrawn::Aborted)
        }
        komo_kernel::durable::SubmissionStatus::Placed => Ok(Withdrawn::AlreadyPlaced),
        _ => Ok(Withdrawn::Settled),
    }
}

/// `Conversation.abort()` 的一半：撤回排队的**用户** steer / followUp（`unanswered{aborted}`），
/// 排队的 write 与内部输入留着——内部输入是后台任务的结果、监听消息，丢了没人转告用户；它们在
/// run 的 abort handler 里随收尾的 final boundary 放置、起后继 run。显式 [`withdraw`] 单条撤回
/// 对内部输入仍然有效。
pub async fn withdraw_queued_inputs(
    tx: &mut Tx,
    conversation: ConversationId,
) -> Result<(), DurableError> {
    let mut inbox = load(tx, conversation).await?;
    let mut kept = Vec::new();
    for item in std::mem::take(&mut inbox.items) {
        if matches!(item, InboxItem::Write { .. }) || item.is_internal() {
            kept.push(item);
            continue;
        }
        settle(
            tx,
            item.id(),
            &SubmissionSettlement::Unanswered {
                reason: "aborted".to_owned(),
                detail: None,
            },
        )
        .await?;
    }
    inbox.items = kept;
    store(tx, conversation, &inbox).await
}
