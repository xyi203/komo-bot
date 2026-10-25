//! 提交输入（`docs/storage-durable-d5.md` §4 W1c / §7）：HTTP、渠道、cron 共用的那一段。
//!
//! 输入路由到 `current_branch`。`leaf` 不在末端（导航过）时，**同一次提交**里：
//!
//! 1. 造落点（[`Placement`](super::tree::Placement)）：`fork_conversation(含 leaf 的 conversation,
//!    at = leaf)`，或在第一条之前建新根 conversation；
//! 2. `komo.agent` 复制给新 conversation（决定 10）；
//! 3. 改 `current_branch`、清 `leaf`；
//! 4. [`Harness::admit_in_tx`](komo_runtime::durable::harness::Harness::admit_in_tx) 受理输入。
//!
//! 所以没有"conversation 建了、输入没进"的崩溃窗口。`leaf` 就是末端（或没导航过）时只是
//! 受理 + 更新会话行的 `changed_at`。
//!
//! # 请求键
//!
//! `request_id` 在 conversation 内去重（`Harness` 的规则），但导航之后输入会路由到另一个
//! conversation，同一个键重发就不该再开一条 run：所以先在 Session 的全部 conversation 里找这个键。
//! 命中且内容相同返回原 submission（`deduplicated`）；内容不同是
//! [`SessionError::RequestKeyConflict`]。内容靠输入 entry 的 `data.inputHash`（原文的 SHA-256）比，
//! 它由这里写进去，调用方给的 `data`（`chat.input` 的 `peer` / `principal` 等）原样保留。
//!
//! # model / effort 覆盖（决定 15）
//!
//! 覆盖是 sticky 的：写进目标 conversation 的 `komo.agent`（之后每一轮钩子都读到它），并追加一条
//! `model.change` 展示 entry。值没变就什么都不写。`model.change` 排在输入 entry **之后**
//! （同一次提交里受理要先于它：受理读已提交状态，不能排在这个 conversation 的表写之后）；它没有
//! `model`，不进上下文，所以次序只影响展示。

use komo_kernel::durable::{
    Content, ConversationId, InputState, Message, SubmissionId, SubmissionKind, SubmissionRecord,
    SubmissionStatus, legacy_ids,
};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::RunId;
use komo_runtime::durable::EntryDraft;
use komo_runtime::durable::harness::inbox::Admitted;
use komo_runtime::durable::harness::{Harness, InboxItem, InputEntry, SubmissionDraft, WhenBusy};
use serde_json::{Value, json};

use super::DurableCore;
use super::agent_doc::{self, AgentDoc};
use super::sessions::{SessionError, ensure_open};
use super::tree::{MODEL_CHANGE_ENTRY, busy_reason, load_tree, placement, realize};

/// 输入没指明来源时放置成的 entry kind（§2：gateway 写的 `cli.input`）。
pub const DEFAULT_INPUT_KIND: &str = "cli.input";

/// 本次提交带的模型 / effort 覆盖（调用方已经按当前配置解析并校验过）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelOverride {
    /// 模型 alias（`komo.agent.model`，之后按**当前**配置解析）。
    pub alias: Option<String>,
    /// 给 `model.change` 展示用：解析出来的 provider 与模型名。
    pub provider: Option<String>,
    pub model_id: Option<String>,
    pub effort: Option<String>,
}

/// 一条输入。
#[derive(Debug, Clone)]
pub struct InputRequest {
    pub text: String,
    /// 幂等键（请求键 / 平台消息键）。
    pub request_id: Option<String>,
    /// 放置成的 entry 的 kind 与 data；没有就是 `cli.input`。
    pub entry: Option<InputEntry>,
    /// 忙时怎么办：排队（默认）、插话、拒绝。
    pub when_busy: WhenBusy,
    pub model: Option<ModelOverride>,
    /// 规范文件（AGENTS.md 一类）变了时的 `project_update_block`：补在输入正文**后面**，
    /// 展示的是同一条 entry。
    pub project_update: Option<String>,
}

impl InputRequest {
    pub fn new(text: impl Into<String>) -> Self {
        InputRequest {
            text: text.into(),
            request_id: None,
            entry: None,
            when_busy: WhenBusy::FollowUp,
            model: None,
            project_update: None,
        }
    }

    pub fn request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }

    pub fn entry(mut self, entry: InputEntry) -> Self {
        self.entry = Some(entry);
        self
    }

    pub fn when_busy(mut self, when_busy: WhenBusy) -> Self {
        self.when_busy = when_busy;
        self
    }

    pub fn model(mut self, model: ModelOverride) -> Self {
        self.model = Some(model);
        self
    }

    pub fn project_update(mut self, block: impl Into<String>) -> Self {
        self.project_update = Some(block.into());
        self
    }
}

/// 受理的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub session: String,
    /// 输入落进的 conversation（可能是刚 fork 出来的）。
    pub conversation: ConversationId,
    pub submission: SubmissionId,
    /// 旧协议的 `RunId`：这条 submission 的十进制 ID（它开新 run 时就是这条 run 的 ID）。
    pub run: RunId,
    /// 命中了同一请求键的原 submission：什么都没写。
    pub deduplicated: bool,
    /// 受理之后的状态：`placed`（开跑了）、`queued`（排队）、`done` / `unanswered`。
    pub status: SubmissionStatus,
}

struct Landed {
    conversation: ConversationId,
    submission: SubmissionId,
    deduplicated: bool,
}

/// 把 `inputHash` 并进输入 entry 的 `data`（`data` 不是对象就包一层）。
fn with_hash(data: Option<Value>, hash: &str) -> Value {
    match data {
        None | Some(Value::Null) => json!({ "inputHash": hash }),
        Some(Value::Object(mut map)) => {
            map.insert("inputHash".to_owned(), json!(hash));
            Value::Object(map)
        }
        Some(other) => json!({ "inputHash": hash, "value": other }),
    }
}

fn hash_of(data: Option<&Value>) -> Option<String> {
    data?.get("inputHash")?.as_str().map(str::to_owned)
}

/// 命中的已有 submission 与这次的内容是不是同一条。认不出内容（排队中被撤回、旧数据没有
/// `inputHash`）就当同一条。
async fn same_input(
    storage: &dyn komo_kernel::durable::Storage,
    existing: &SubmissionRecord,
    hash: &str,
    key: &str,
) -> Result<(), SessionError> {
    let SubmissionKind::Input { state } = &existing.kind else {
        return Err(SessionError::RequestKeyConflict(key.to_owned()));
    };
    let stored = match state {
        InputState::Placed { entry }
        | InputState::Done { entry, .. }
        | InputState::Unanswered {
            entry: Some(entry), ..
        } => storage
            .entry(*entry)
            .await?
            .and_then(|stored| hash_of(stored.entry.data.as_ref())),
        InputState::Queued => {
            let inbox = Harness::inbox(storage, existing.conversation).await?;
            inbox.items.iter().find_map(|item| match item {
                InboxItem::Steer { id, entry, .. } | InboxItem::FollowUp { id, entry, .. }
                    if *id == existing.id =>
                {
                    hash_of(entry.as_ref().and_then(|entry| entry.data.as_ref()))
                }
                _ => None,
            })
        }
        InputState::Unanswered { entry: None, .. } => None,
    };
    match stored {
        Some(stored) if stored != hash => Err(SessionError::RequestKeyConflict(key.to_owned())),
        _ => Ok(()),
    }
}

impl DurableCore {
    /// 提交一条输入：路由到 `current_branch`（`leaf` 不在末端时同一提交先 fork / 建新根），
    /// 受理，更新会话行（`current_branch`、清 `leaf`、`changed_at`、标题为空时取首行）。
    ///
    /// 不接受新输入的会话（`closing` / `deleted` / `purged`）是 [`SessionError::NotOpen`]；
    /// `WhenBusy::Reject` 遇忙是 [`SessionError::Busy`]，什么都不写。
    pub async fn submit_input(
        &self,
        session: &str,
        request: InputRequest,
    ) -> Result<Accepted, SessionError> {
        let hash = ContentHash::of_str(&request.text).to_string();
        let mut entry = request
            .entry
            .clone()
            .unwrap_or_else(|| InputEntry::new(DEFAULT_INPUT_KIND, None));
        entry.data = Some(with_hash(entry.data.take(), &hash));
        let body = match request.project_update.as_deref() {
            Some(update) if !update.trim().is_empty() => {
                format!("{}\n\n{update}", request.text)
            }
            _ => request.text.clone(),
        };
        let draft = SubmissionDraft::Input {
            content: Content::Text(body),
            entry: Some(entry),
            when_busy: request.when_busy,
            request_id: request.request_id.clone(),
            internal: false,
        };

        let storage = self.storage.clone();
        let harness = self.harness.clone();
        let id = session.to_owned();
        let text = request.text.clone();
        let request_id = request.request_id.clone();
        let when_busy = request.when_busy;
        let model = request.model.clone();
        self.engine.resume();
        let landed = self
            .engine
            .commit(move |tx| {
                Box::pin(async move {
                    // 先读后写：下面读的都是已提交状态，在这次提交的第一次表写之前。
                    let tree = match load_tree(&*storage, &id).await {
                        Ok(tree) => tree,
                        Err(error) => return Ok(Err(error)),
                    };
                    if let Err(error) = ensure_open(&tree.session) {
                        return Ok(Err(error));
                    }
                    if let Some(key) = &request_id {
                        for branch in &tree.branches {
                            let found = tx
                                .submission_by_request(branch.conversation.id, key)
                                .await?;
                            if let Some(existing) = found {
                                if let Err(error) =
                                    same_input(&*storage, &existing, &hash, key).await
                                {
                                    return Ok(Err(error));
                                }
                                return Ok(Ok(Landed {
                                    conversation: existing.conversation,
                                    submission: existing.id,
                                    deduplicated: true,
                                }));
                            }
                        }
                    }
                    let place = placement(&tree, tree.leaf_pos());
                    if when_busy == WhenBusy::Reject
                        && let super::tree::Placement::Existing(conversation) = place
                    {
                        match busy_reason(&*storage, conversation).await {
                            Ok(Some(why)) => {
                                return Ok(Err(SessionError::Busy(format!(
                                    "这个会话正忙（{why}），输入被拒绝"
                                ))));
                            }
                            Ok(None) => {}
                            Err(error) => return Ok(Err(error)),
                        }
                    }

                    let current = tree.session.current_branch;
                    let conversation = realize(tx, &id, current, place).await?;

                    // sticky 的 model / effort 覆盖：写进 komo.agent，值变了才留 model.change。
                    let mut change: Option<Value> = None;
                    if let Some(over) = &model {
                        let mut doc = match agent_doc::read_in(tx, conversation).await? {
                            Some(doc) => doc,
                            None => AgentDoc::new(tree.session.agent.clone().unwrap_or_default()),
                        };
                        let mut changed = false;
                        if let Some(alias) = &over.alias
                            && doc.model.as_ref() != Some(alias)
                        {
                            doc.model = Some(alias.clone());
                            changed = true;
                        }
                        if let Some(effort) = &over.effort
                            && doc.effort.as_ref() != Some(effort)
                        {
                            doc.effort = Some(effort.clone());
                            changed = true;
                        }
                        if changed {
                            agent_doc::write_in(tx, conversation, &doc).await?;
                            let mut data = serde_json::Map::new();
                            for (key, value) in [
                                ("alias", &over.alias),
                                ("provider", &over.provider),
                                ("modelId", &over.model_id),
                                ("effort", &over.effort),
                            ] {
                                if let Some(value) = value {
                                    data.insert(key.to_owned(), json!(value));
                                }
                            }
                            change = Some(Value::Object(data));
                        }
                    }

                    let submission = match harness.admit_in_tx(tx, conversation, &draft).await? {
                        Admitted::Id(submission) => submission,
                        Admitted::Busy => {
                            // `Reject` 的忙已经在上面挡过（同一个线上状态），到这里不该发生。
                            return Err(komo_runtime::durable::DurableError::rejected(
                                "conversation 在受理时忙了，输入被拒绝",
                            ));
                        }
                    };
                    if let Some(data) = change {
                        tx.append_entry(
                            conversation,
                            EntryDraft::new(MODEL_CHANGE_ENTRY).with_data(data),
                        )
                        .await?;
                    }

                    let mut record = tree.session.clone();
                    record.current_branch = conversation;
                    record.leaf = None;
                    record.changed_at = tx.now();
                    if record
                        .title
                        .as_deref()
                        .is_none_or(|title| title.trim().is_empty())
                        && let Some(title) = komo_store::coordinator::title_for(&text)
                    {
                        record.title = Some(title);
                        record.title_provisional = true;
                    }
                    tx.put_session(record);
                    Ok(Ok(Landed {
                        conversation,
                        submission,
                        deduplicated: false,
                    }))
                })
            })
            .await??;

        let status = self
            .storage
            .submission(landed.submission)
            .await?
            .map(|record| record.status())
            .unwrap_or(SubmissionStatus::Queued);
        Ok(Accepted {
            session: session.to_owned(),
            conversation: landed.conversation,
            submission: landed.submission,
            run: legacy_ids::run_id(landed.submission),
            deduplicated: landed.deduplicated,
            status,
        })
    }

    /// `/new`：在当前分支追加 `komo.reset`（`Harness::reset`），可带交接文字。
    pub async fn boundary(
        &self,
        session: &str,
        handoff: Option<String>,
    ) -> Result<SubmissionId, SessionError> {
        let record = self.session(session).await?;
        ensure_open(&record)?;
        let handoff = handoff.map(|text| {
            Message::user(
                text,
                (self.clock.now().unix_timestamp_nanos() / 1_000_000) as i64,
            )
        });
        Ok(self
            .harness
            .reset(&self.engine, record.current_branch, handoff)
            .await?)
    }

    /// `/cancel`、Esc：只停当前分支这一轮（决定 8：`Harness::abort_run`，不撤回收件箱）。返回有没有
    /// run 被取消。
    pub async fn cancel_run(&self, session: &str) -> Result<bool, SessionError> {
        let record = self.session(session).await?;
        Ok(self
            .harness
            .abort_run(&self.engine, record.current_branch)
            .await?)
    }
}
