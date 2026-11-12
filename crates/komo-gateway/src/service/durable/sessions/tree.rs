//! 会话树服务层：读树、name、忙时守卫、落点（`docs/storage-durable-d5.md` §7；旧实现的行为参照是
//! `http/session_tree.rs`）。导航、label、context-edit、分支摘要与 compact 的用户入口已在 2026-11-12 删除
//! （`docs/tasks.md` §5，D5）；没有了写入方，`record.leaf` 只会留在更早的会话行里。
//!
//! # 树长什么样
//!
//! 一个 Session 是一组 conversation：根、加上会话内分支 fork 出来的子 conversation（继承父的
//! session，`conversations.session_id` 列）。`sessions.current_branch` 是下一条输入落在哪个分支，
//! `sessions.record.leaf` 是旧导航留下的位置（决定 11）。**没有 `leaf` 就是当前分支的末端。**
//!
//! # 落点（只为读到旧 `leaf` 的会话保留）
//!
//! - `leaf` 为 `None` 表示"跟着末端"；落在"第一条之前"没有 entry 可指，用 [`BEFORE_FIRST`]
//!   （entry ID 从 2 起铸，0 永远不是真 entry）。
//! - 下一次追加（提交输入、name）时，如果 `leaf` 不在末端，**同一次提交**里先造出落点
//!   （[`Placement`]）：`leaf` 就是它所在 conversation 的最后一条自己的 entry 时直接接在那个
//!   conversation 后面（不用 fork）；否则 `fork_conversation(含 leaf 的 conversation, at = leaf)`；
//!   在第一条之前就建新根 conversation。新 conversation 同一提交复制 `komo.agent`（决定 10）；
//!   `current_branch` 改过去、`leaf` 清掉。
//! - 任一分支忙（`komo.live.run`、收件箱非空、压缩进行中）时树操作（name、fork）返回
//!   [`SessionError::Busy`]（409）。
//!
//! # 公开的读口
//!
//! [`DurableCore::read_tree`] 给 W2a（`/tree`）与 W4c（导出）：取一个 Session 的全部 conversation
//! 及各自**自己的** entry（不含 fork 祖先那一段），加上解出来的当前 leaf。pi 投影（W1a 的
//! `pi_tree`）在它上面做，这里不碰。

use std::collections::HashMap;

use komo_kernel::durable::fork::{fork_chain, visible_entries};
use komo_kernel::durable::storage::{ConversationQuery, EntryQuery};
use komo_kernel::durable::{
    Content, ContentBlock, ConversationId, ConversationRecord, Cursor, EntryId, EntryRecord,
    Message, ScanOrder, SessionRecord, Storage,
};
use komo_runtime::durable::harness::Harness;
use komo_runtime::durable::{ConversationOwnership, DurableError, EntryDraft, Tx};
use serde_json::json;

use super::super::DurableCore;
use super::agent_doc;
use super::{SessionError, ensure_open};

/// gateway 写的展示 entry 的 kind（`docs/storage-durable-d5.md` §2）。
// entry kind 只有一份定义：kernel 的 `pi_tree` 按它们投影，这里写入用同一组常量。
pub use komo_kernel::durable::pi_tree::{
    BRANCH_SUMMARY_KIND as BRANCH_SUMMARY_ENTRY, LABEL_KIND as LABEL_ENTRY,
    MODEL_CHANGE_KIND as MODEL_CHANGE_ENTRY, SESSION_INFO_KIND as SESSION_INFO_ENTRY,
};

/// `leaf` 落在"第一条 entry 之前"的哨兵（真 entry 的 ID 从 2 起，见 `ROOT_CONVERSATION_ID`）。
pub const BEFORE_FIRST: EntryId = EntryId::new(0);

const SCAN_PAGE: usize = 512;

// ------------------------------------------------------------------- 读树

/// 一个 conversation 与它**自己的** entry（升序；fork 祖先那一段不在里面）。
#[derive(Debug, Clone)]
pub struct BranchEntries {
    pub conversation: ConversationRecord,
    pub entries: Vec<EntryRecord>,
}

/// 一个 Session 的全部 conversation 与 entry，加上当前 leaf。
#[derive(Debug, Clone)]
pub struct TreeData {
    pub session: SessionRecord,
    /// 按 conversation ID 升序（根在最前）。
    pub branches: Vec<BranchEntries>,
    /// **解出来的** leaf（`/tree` 的 `leaf_id`）：导航过就是那个位置，否则是当前分支的末端；
    /// 会话还没有任何 entry、或导航到了"第一条之前"时是 `None`。
    ///
    /// 交给 `pi_tree(.., leaf)` 的应该是**原始的** `session.leaf`（不是这个解出来的值）：
    /// `None` = 跟着末端，`Some(BEFORE_FIRST)` 在树里找不到所以 `leaf_id` 是 `None`。
    pub leaf: Option<EntryId>,
}

/// `sessions.record.leaf` 的三种读法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafPos {
    /// 没导航过：当前分支的末端。
    Tail,
    /// 第一条 entry 之前。
    BeforeFirst,
    At(EntryId),
}

impl LeafPos {
    fn of(leaf: Option<EntryId>) -> Self {
        match leaf {
            None => LeafPos::Tail,
            Some(id) if id == BEFORE_FIRST => LeafPos::BeforeFirst,
            Some(id) => LeafPos::At(id),
        }
    }
}

impl TreeData {
    pub fn branch(&self, conversation: ConversationId) -> Option<&BranchEntries> {
        self.branches
            .iter()
            .find(|branch| branch.conversation.id == conversation)
    }

    /// 一条 entry 属于哪个 conversation（只在这个 Session 里找）。
    pub fn owner_of(&self, entry: EntryId) -> Option<ConversationId> {
        self.branches
            .iter()
            .find(|branch| {
                branch
                    .entries
                    .binary_search_by_key(&entry, |record| record.id)
                    .is_ok()
            })
            .map(|branch| branch.conversation.id)
    }

    pub fn entry(&self, id: EntryId) -> Option<&EntryRecord> {
        self.branches.iter().find_map(|branch| {
            branch
                .entries
                .binary_search_by_key(&id, |record| record.id)
                .ok()
                .map(|index| &branch.entries[index])
        })
    }

    /// 一个 conversation 自己的最后一条 entry。
    pub fn last_own(&self, conversation: ConversationId) -> Option<EntryId> {
        self.branch(conversation)?.entries.last().map(|e| e.id)
    }

    /// 一个 conversation 的末端：自己的最后一条；还没有自己的就是 fork 点。
    pub fn tail_of(&self, conversation: ConversationId) -> Option<EntryId> {
        let branch = self.branch(conversation)?;
        branch
            .entries
            .last()
            .map(|entry| entry.id)
            .or_else(|| branch.conversation.parent.map(|parent| parent.at))
    }

    pub fn current_tail(&self) -> Option<EntryId> {
        self.tail_of(self.session.current_branch)
    }

    pub fn leaf_pos(&self) -> LeafPos {
        LeafPos::of(self.session.leaf)
    }

    /// 到 `leaf`（含）为止的路径：根在前，沿 fork 祖先拼。`leaf` 不在这个 Session 里是空。
    pub fn path(&self, leaf: EntryId) -> Vec<&EntryRecord> {
        let Some(owner) = self.owner_of(leaf) else {
            return Vec::new();
        };
        let records: HashMap<ConversationId, &ConversationRecord> = self
            .branches
            .iter()
            .map(|branch| (branch.conversation.id, &branch.conversation))
            .collect();
        let Ok(chain) = fork_chain(owner, |id| records.get(&id).copied(), None, Some(leaf)) else {
            return Vec::new();
        };
        visible_entries(
            &chain,
            |id| {
                self.branch(id)
                    .map_or(&[][..], |branch| branch.entries.as_slice())
            },
            None,
            Some(leaf),
            ScanOrder::Ascending,
        )
    }

    /// 路径上 `entry` 的上一条：同一个 conversation 的上一条自己的 entry；没有就是 fork 点；
    /// 根的第一条之前是 `None`。
    pub fn prev_of(&self, entry: EntryId) -> Option<EntryId> {
        let branch = self.branch(self.owner_of(entry)?)?;
        let index = branch
            .entries
            .binary_search_by_key(&entry, |record| record.id)
            .ok()?;
        match index.checked_sub(1) {
            Some(before) => Some(branch.entries[before].id),
            None => branch.conversation.parent.map(|parent| parent.at),
        }
    }
}

/// 读一个 Session 的全部 conversation 与 entry（已提交状态）。
pub async fn load_tree(storage: &dyn Storage, id: &str) -> Result<TreeData, SessionError> {
    let session = storage
        .session(id)
        .await?
        .ok_or_else(|| SessionError::NotFound(format!("会话 {id}")))?;
    let query = ConversationQuery {
        session: Some(id.to_owned()),
        order: Some(ScanOrder::Ascending),
        ..ConversationQuery::default()
    };
    let mut conversations = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let page = storage
            .scan_conversations(&query, SCAN_PAGE, cursor.as_ref())
            .await?;
        conversations.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    let mut branches = Vec::with_capacity(conversations.len());
    for conversation in conversations {
        // fork 子 conversation 的可见历史带着祖先那一段；自己的 entry 都在 fork 点之后。
        let mut query = EntryQuery::new(conversation.id);
        query.order = Some(ScanOrder::Ascending);
        query.min = conversation
            .parent
            .map(|parent| EntryId::new(parent.at.get() + 1));
        let mut entries = Vec::new();
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = storage
                .scan_entries(&query, SCAN_PAGE, cursor.as_ref())
                .await?;
            entries.extend(page.items);
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        branches.push(BranchEntries {
            conversation,
            entries,
        });
    }
    let mut tree = TreeData {
        session,
        branches,
        leaf: None,
    };
    tree.leaf = match tree.leaf_pos() {
        LeafPos::Tail => tree.current_tail(),
        LeafPos::BeforeFirst => None,
        LeafPos::At(id) => Some(id),
    };
    Ok(tree)
}

// ------------------------------------------------------------------- 忙时守卫

/// 这个 conversation 为什么忙；空闲是 `None`。忙 = 有 run、收件箱非空、压缩进行中。
pub(in crate::service::durable) async fn busy_reason(
    storage: &dyn Storage,
    conversation: ConversationId,
) -> Result<Option<&'static str>, SessionError> {
    let live = Harness::live(storage, conversation).await?;
    if live.run.is_some() {
        return Ok(Some("有 Run 在跑"));
    }
    if live.compactions.as_ref().is_some_and(|c| !c.is_empty()) {
        return Ok(Some("正在压缩"));
    }
    if !Harness::inbox(storage, conversation)
        .await?
        .items
        .is_empty()
    {
        return Ok(Some("收件箱里还有排队的输入"));
    }
    Ok(None)
}

/// 任一分支忙就是 [`SessionError::Busy`]（409）。
pub(in crate::service::durable) async fn ensure_idle(
    storage: &dyn Storage,
    tree: &TreeData,
) -> Result<(), SessionError> {
    for branch in &tree.branches {
        let id = branch.conversation.id;
        if let Some(why) = busy_reason(storage, id).await? {
            return Err(SessionError::Busy(format!(
                "这个会话有分支在忙（conversation {id}：{why}），等它停下再改树"
            )));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------- 落点

/// 一次追加落在哪个 conversation。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// 接在已有 conversation 的末端（不新建）。
    Existing(ConversationId),
    /// 在 `from` 的 `at` 处 fork 一个子 conversation。
    Fork { from: ConversationId, at: EntryId },
    /// `leaf` 在第一条之前：建新根 conversation。
    NewRoot,
}

/// `pos` 处追加该落在哪。
pub fn placement(tree: &TreeData, pos: LeafPos) -> Placement {
    let current = tree.session.current_branch;
    match pos {
        LeafPos::Tail => Placement::Existing(current),
        LeafPos::BeforeFirst => Placement::NewRoot,
        LeafPos::At(at) => {
            if Some(at) == tree.tail_of(current) {
                return Placement::Existing(current);
            }
            let Some(owner) = tree.owner_of(at) else {
                return Placement::Existing(current);
            };
            if tree.last_own(owner) == Some(at) {
                Placement::Existing(owner)
            } else {
                Placement::Fork { from: owner, at }
            }
        }
    }
}

/// 在提交里造出落点（表写：新 conversation；document 写：`komo.agent` 复制）。必须排在这次提交
/// 的任何表读之前。返回要追加到的 conversation。
pub(in crate::service::durable) async fn realize(
    tx: &mut Tx,
    session: &str,
    current: ConversationId,
    placement: Placement,
) -> Result<ConversationId, DurableError> {
    match placement {
        Placement::Existing(conversation) => Ok(conversation),
        Placement::Fork { from, at } => {
            let child = tx
                .fork_conversation(from, at, ConversationOwnership::Ownerless)
                .await?;
            agent_doc::copy_in(tx, current, child.id).await?;
            Ok(child.id)
        }
        Placement::NewRoot => {
            let root = tx
                .create_conversation_in(session, ConversationOwnership::Ownerless)
                .await?;
            agent_doc::copy_in(tx, current, root.id).await?;
            Ok(root.id)
        }
    }
}

/// 提交开始前的校验：会话还开着、结构没被别处改过（`current_branch` 与 `leaf` 和操作开始时读到的
/// 一致）、所有分支空闲。在提交闭包里用——线上串行，读到的就是提交那一刻的状态。
pub(in crate::service::durable) async fn verify(
    storage: &dyn Storage,
    id: &str,
    expect: &Expect,
) -> Result<TreeData, SessionError> {
    let tree = load_tree(storage, id).await?;
    ensure_open(&tree.session)?;
    if tree.session.current_branch != expect.current_branch || tree.session.leaf != expect.leaf {
        return Err(SessionError::Conflict(
            "会话在操作期间被改动了，请重试".to_owned(),
        ));
    }
    ensure_idle(storage, &tree).await?;
    Ok(tree)
}

/// 操作开始时读到的结构，提交时核对。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::service::durable) struct Expect {
    pub current_branch: ConversationId,
    pub leaf: Option<EntryId>,
}

impl Expect {
    pub(in crate::service::durable) fn of(tree: &TreeData) -> Self {
        Expect {
            current_branch: tree.session.current_branch,
            leaf: tree.session.leaf,
        }
    }
}

/// 追加的结果。
#[derive(Debug, Clone)]
pub struct Appended {
    /// 追加落到的 conversation（现在的 `current_branch`）。
    pub conversation: ConversationId,
    pub entries: Vec<EntryRecord>,
}

/// name 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigateOutcome {
    /// 操作之后的 leaf（`/tree` 的 `leaf_id`）。
    pub leaf: Option<EntryId>,
}

// ------------------------------------------------------------------- 帮手

/// 一个 entry 若是"输入"（用户说的话，不是摘要或 reset 的交接），它的原文。
///
/// 输入 = `komo.user`，或不占 `komo.` 前缀、`model` 里是 user 消息的 entry（`cli.input`、
/// `chat.input`、`cron.input`、`task.result`、`monitor.*`……），但分支摘要不算。
pub fn input_text(entry: &EntryRecord) -> Option<String> {
    let kind = entry.kind.as_str();
    let is_input =
        kind == "komo.user" || (!kind.starts_with("komo.") && kind != BRANCH_SUMMARY_ENTRY);
    if !is_input {
        return None;
    }
    match entry.model.as_ref()?.first()? {
        Message::User(user) => Some(content_text(&user.content)),
        _ => None,
    }
}

pub(in crate::service::durable) fn content_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// `EntryId` 的十进制字符串：HTTP 与 pi 投影里的 entry id 都是它。
pub fn entry_id_text(id: EntryId) -> String {
    id.get().to_string()
}

/// 客户端给的 entry id → `EntryId`。认 `"12"` 与 `"12:1"`（pi 投影里一条 durable entry 展开成
/// 多条时是 `"{id}:{k}"`，见 kernel `pi_tree`）；不是规范写法 / 不存在是 `NotFound`。
pub fn parse_entry_id(text: &str) -> Result<EntryId, SessionError> {
    let base = text.split_once(':').map_or(text, |(base, _)| base);
    let value: u64 = base
        .parse()
        .map_err(|_| SessionError::NotFound(format!("条目 {text}")))?;
    (value.to_string() == base && value > 0)
        .then(|| EntryId::new(value))
        .ok_or_else(|| SessionError::NotFound(format!("条目 {text}")))
}

// ------------------------------------------------------------------- 操作

impl DurableCore {
    /// 一个 Session 的全部 conversation 与 entry（`/tree` 与导出用）。读已提交状态，不进提交线。
    pub async fn read_tree(&self, session: &str) -> Result<TreeData, SessionError> {
        load_tree(&*self.storage, session).await
    }

    /// 读树并要求会话开着、所有分支空闲（树操作的前置）。
    pub(in crate::service::durable) async fn editable_tree(
        &self,
        session: &str,
    ) -> Result<TreeData, SessionError> {
        let tree = load_tree(&*self.storage, session).await?;
        ensure_open(&tree.session)?;
        ensure_idle(&*self.storage, &tree).await?;
        Ok(tree)
    }

    pub(in crate::service::durable) fn entry_in<'a>(
        &self,
        tree: &'a TreeData,
        id: EntryId,
    ) -> Result<&'a EntryRecord, SessionError> {
        tree.entry(id)
            .ok_or_else(|| SessionError::NotFound(format!("条目 {id}")))
    }

    /// 在当前位置追加展示 entry（name 用）：leaf 不在末端先落分支，
    /// `current_branch` 改到落点、`leaf` 清掉；`rename` 同提交更新 `sessions.title`。
    pub(in crate::service::durable) async fn append_at_leaf(
        &self,
        session: &str,
        expect: Expect,
        drafts: Vec<EntryDraft>,
        rename: Option<String>,
    ) -> Result<Appended, SessionError> {
        let storage = self.storage.clone();
        let id = session.to_owned();
        self.engine
            .commit(move |tx| {
                Box::pin(async move {
                    let tree = match verify(&*storage, &id, &expect).await {
                        Ok(tree) => tree,
                        Err(error) => return Ok(Err(error)),
                    };
                    let place = placement(&tree, tree.leaf_pos());
                    let conversation = realize(tx, &id, tree.session.current_branch, place).await?;
                    let mut entries = Vec::with_capacity(drafts.len());
                    for draft in drafts {
                        entries.push(tx.append_entry(conversation, draft).await?);
                    }
                    let mut record = tree.session.clone();
                    record.current_branch = conversation;
                    record.leaf = None;
                    record.changed_at = tx.now();
                    if let Some(title) = rename {
                        record.title = Some(title);
                        record.title_provisional = false;
                    }
                    tx.put_session(record);
                    Ok(Ok(Appended {
                        conversation,
                        entries,
                    }))
                })
            })
            .await?
    }

    fn after_append(appended: &Appended) -> NavigateOutcome {
        NavigateOutcome {
            leaf: appended.entries.last().map(|entry| entry.id),
        }
    }

    /// 改会话名：空闲时追加 `session.info {name}`，同一次提交更新 `sessions.title`；
    /// 有分支在忙（Run、排队、压缩）时不动树，只改 `sessions.title`（同自动起标题）——
    /// 标题是给人看的，不该等 Run 停下。
    pub async fn name(&self, session: &str, name: &str) -> Result<NavigateOutcome, SessionError> {
        match self.name_on_tree(session, name).await {
            Err(SessionError::Busy(_)) => self.retitle(session, name).await,
            other => other,
        }
    }

    /// 忙时改名：只改 `sessions` 行的标题，不追加 entry、不动 leaf。
    async fn retitle(&self, session: &str, name: &str) -> Result<NavigateOutcome, SessionError> {
        let (id, title) = (session.to_owned(), name.to_owned());
        self.engine
            .commit(move |tx| {
                Box::pin(async move {
                    let Some(mut record) = tx.session(&id).await? else {
                        return Ok(Err(SessionError::NotFound(format!("会话 {id}"))));
                    };
                    if let Err(error) = ensure_open(&record) {
                        return Ok(Err(error));
                    }
                    record.title = Some(title);
                    record.title_provisional = false;
                    tx.put_session(record);
                    Ok(Ok(()))
                })
            })
            .await??;
        let tree = load_tree(&*self.storage, session).await?;
        Ok(NavigateOutcome { leaf: tree.leaf })
    }

    async fn name_on_tree(
        &self,
        session: &str,
        name: &str,
    ) -> Result<NavigateOutcome, SessionError> {
        let tree = self.editable_tree(session).await?;
        let draft = EntryDraft::new(SESSION_INFO_ENTRY).with_data(json!({"name": name}));
        let appended = self
            .append_at_leaf(
                session,
                Expect::of(&tree),
                vec![draft],
                Some(name.to_owned()),
            )
            .await?;
        Ok(Self::after_append(&appended))
    }
}
