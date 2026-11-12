//! 会话服务层：建会话、列表、详情映射、主会话与按来源找会话、拒绝输入的判定
//! （`docs/storage-durable-d5.md` §4 W1c / §7）。
//!
//! 一个 komo Session = 一条 `sessions` 行 + 一组 conversation（根 + 会话内分支）+ `current_branch`
//! 指针。**建会话 = sessions 行 + 根 conversation + `komo.agent` document，一次提交**——没有
//! "行建了、根没建"的崩溃窗口。这里不接 HTTP：W2a 在上面接路由，渠道（W3a）、cron（W3b）直接调。
//!
//! 错误统一用 [`SessionError`]：调用方（HTTP 层）按变体映射状态码——`NotFound` 404、`Invalid` 400、
//! `NotOpen` / `Busy` / `Conflict` / `RequestKeyConflict` 409、`Internal` 500。

pub mod agent_doc;
pub mod copy;
pub mod lifecycle;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod run_tests;
pub mod runs;
pub mod search;
#[cfg(test)]
mod session_tests;
pub mod task_list;
pub mod tasks;
pub mod titles;
pub mod tree;

use komo_kernel::durable::storage::{EntryQuery, SessionQuery};
use komo_kernel::durable::{
    ConversationId, EntryId, ScanOrder, SessionCursor, SessionKind, SessionLifecycle,
    SessionRecord, Storage, StorageError, SubmissionId, legacy_ids,
};
use komo_kernel::protocol::http::SessionSummary;
use komo_kernel::types::chat::ChannelPeer;
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::status::{RunState, SessionState};
use komo_runtime::durable::harness::{InboxItem, SubmitError};
use komo_runtime::durable::{ConversationOwnership, DurableError, Tx};
use time::OffsetDateTime;

use super::DurableCore;
use agent_doc::AgentDoc;

/// 来源 `home`：主会话的 `source`（旧 komo 的 `HOME_ORIGIN`）。
pub const HOME_SOURCE: &str = "home";

/// 群的"线"（`docs/tasks.md` §4.13）：一个群一条、不跑模型的 `kind = main` 会话，承载该群的展示消息与分发任务。
/// `source` 是这个前缀加群的 `ChannelPeer`（不带话题）。
pub const GROUP_LINE_PREFIX: &str = "line:";

/// 群 `peer` 的线的 `source`。
pub fn group_line_source(peer: &ChannelPeer) -> String {
    format!(
        "{GROUP_LINE_PREFIX}{}",
        ChannelPeer::new(peer.platform.clone(), peer.chat_id.as_str())
    )
}

const SCAN_PAGE: usize = 256;

/// 会话服务层的错误。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// 找不到会话 / 条目。文字是"找不到什么"。
    #[error("找不到 {0}")]
    NotFound(String),
    /// 请求本身不对（目标不在这个会话里、没有可压缩的内容、摘要模型失败……）。
    #[error("{0}")]
    Invalid(String),
    /// 这个会话不接受新输入 / 新内容：`closing` / `deleted` / `purged`（§8.10）。
    #[error("这个会话当前状态是 {}", .0.as_str())]
    NotOpen(SessionState),
    /// 树操作被拒：有分支在忙（Run、排队的输入、压缩进行中）。
    #[error("{0}")]
    Busy(String),
    /// 同一个请求键已经用于另一条内容不同的输入。
    #[error("请求键 {0} 已经用于另一条内容不同的输入")]
    RequestKeyConflict(String),
    /// 操作期间会话被别处改动（乐观校验没过），重试即可。
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Internal(String),
}

impl From<DurableError> for SessionError {
    fn from(error: DurableError) -> Self {
        match error {
            DurableError::Rejected(message) => SessionError::Invalid(message),
            DurableError::Storage(StorageError::Request(message)) => SessionError::Invalid(message),
            other => SessionError::Internal(other.to_string()),
        }
    }
}

impl From<StorageError> for SessionError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::Request(message) => SessionError::Invalid(message),
            other => SessionError::Internal(other.to_string()),
        }
    }
}

impl From<SubmitError> for SessionError {
    fn from(error: SubmitError) -> Self {
        match error {
            SubmitError::Busy(conversation) => {
                SessionError::Busy(format!("conversation {conversation} 正忙"))
            }
            SubmitError::Invalid(message) => SessionError::Invalid(message),
            SubmitError::Durable(error) => error.into(),
        }
    }
}

impl From<std::io::Error> for SessionError {
    fn from(error: std::io::Error) -> Self {
        SessionError::Internal(error.to_string())
    }
}

/// `sessions.lifecycle` → 协议里的 [`SessionState`]。
pub fn state_of(lifecycle: SessionLifecycle) -> SessionState {
    match lifecycle {
        SessionLifecycle::Active => SessionState::Active,
        SessionLifecycle::Closing => SessionState::Closing,
        SessionLifecycle::Deleted => SessionState::Deleted,
        SessionLifecycle::Purged => SessionState::Purged,
    }
}

/// 会话不接受新输入就返回 [`SessionError::NotOpen`]（带那一种状态，调用方照旧文案写 409）。
pub(super) fn ensure_open(record: &SessionRecord) -> Result<(), SessionError> {
    let state = state_of(record.lifecycle);
    if state.accepts_input() {
        Ok(())
    } else {
        Err(SessionError::NotOpen(state))
    }
}

/// 毫秒时间戳 → `OffsetDateTime`（0 与越界落到 Unix 纪元）。
pub fn time_of(ms: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// `SessionKind` 在协议里的词：`main` / `normal` / `task`。cron 触发建的会话对客户端就是普通会话。
pub fn kind_name(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Main => "main",
        SessionKind::Normal | SessionKind::Cron => "normal",
        SessionKind::Task => "task",
    }
}

/// 建会话要的东西。
#[derive(Debug, Clone)]
pub struct NewSession {
    /// 预先分配的会话 ID（cron 先在 `aux.db` 写 firing 行再建会话要用）；没有就现铸 UUIDv7。
    pub id: Option<String>,
    pub kind: SessionKind,
    pub title: Option<String>,
    /// 标题还是临时的（取自第一条消息，等 `titles` 换成模型起的）。
    pub title_provisional: bool,
    pub parent_session: Option<String>,
    /// 创建请求的幂等键。
    pub request_key: Option<String>,
    pub request_hash: Option<String>,
    /// 根 conversation 上的 `komo.agent`：Agent、模型 / effort 覆盖、工作目录、`maxRounds`、来源。
    /// `sessions.agent` / `source` / `cwd` 取自它，免得两处漂移。
    pub doc: AgentDoc,
}

impl NewSession {
    pub fn new(agent: impl Into<String>) -> Self {
        NewSession {
            id: None,
            kind: SessionKind::Normal,
            title: None,
            title_provisional: false,
            parent_session: None,
            request_key: None,
            request_hash: None,
            doc: AgentDoc::new(agent),
        }
    }

    pub fn kind(mut self, kind: SessionKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn workdir(mut self, workdir: impl Into<String>) -> Self {
        self.doc.workdir = Some(workdir.into());
        self
    }

    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.doc.source = Some(source.into());
        self
    }

    pub fn request_key(mut self, key: impl Into<String>) -> Self {
        self.request_key = Some(key.into());
        self
    }
}

/// 在**调用方自己的提交**里建会话：sessions 行 + 根 conversation + 根上的 `komo.agent`。
///
/// W3b（cron）要在同一次提交里接着 [`Harness::admit_in_tx`](komo_runtime::durable::harness::Harness::admit_in_tx)
/// 受理第一条输入，所以这一步留成 `Tx` 级函数：它只做表写与 document 写，之后调用方再受理、
/// 不要在它之后做表读。`id` 已存在返回 `Rejected`（`put_session` 会整条替换，不能靠它兜底）。
pub async fn create_in(
    tx: &mut Tx,
    spec: &NewSession,
    id: &str,
) -> Result<SessionRecord, DurableError> {
    if tx.session(id).await?.is_some() {
        return Err(DurableError::rejected(format!("会话 {id} 已经存在")));
    }
    let root = tx
        .create_conversation_in(id, ConversationOwnership::Ownerless)
        .await?;
    let now = tx.now();
    let mut record = SessionRecord::new(id, root.id);
    record.kind = spec.kind;
    record.title = spec.title.clone();
    record.title_provisional = spec.title_provisional;
    record.parent_session = spec.parent_session.clone();
    record.request_key = spec.request_key.clone();
    record.request_hash = spec.request_hash.clone();
    record.agent = Some(spec.doc.agent.clone());
    record.source = spec.doc.source.clone();
    record.cwd = spec.doc.workdir.clone();
    record.created_at = now;
    record.changed_at = now;
    agent_doc::write_in(tx, root.id, &spec.doc).await?;
    tx.put_session(record.clone());
    Ok(record)
}

/// 过 `query` 的会话里第一条也满足 `matches` 的（按 `query.order`）。
async fn first_session(
    storage: &dyn Storage,
    query: &SessionQuery,
    matches: impl Fn(&SessionRecord) -> bool,
) -> Result<Option<SessionRecord>, StorageError> {
    let mut cursor: Option<SessionCursor> = None;
    loop {
        let page = storage.scan_sessions(query, 16, cursor.as_ref()).await?;
        if let Some(found) = page.items.into_iter().find(|record| matches(record)) {
            return Ok(Some(found));
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(None),
        }
    }
}

/// 会话列表要的生命周期：默认 `active` 与 `closing`；`all` 再带上 `deleted`。`purged` 两种都不列
/// （只在显式查看单个会话时可见，§8.10）。
fn listed_lifecycles(all: bool) -> Vec<SessionLifecycle> {
    let mut wanted = vec![SessionLifecycle::Active, SessionLifecycle::Closing];
    if all {
        wanted.push(SessionLifecycle::Deleted);
    }
    wanted
}

/// 会话列表的查询：最新的在前（id 是 UUIDv7，降序就是创建序倒排，且 id 唯一，次序完全确定）。
fn list_query(all: bool) -> SessionQuery {
    SessionQuery {
        lifecycles: listed_lifecycles(all),
        order: Some(ScanOrder::Descending),
        ..SessionQuery::default()
    }
}

/// 会话列表的一页：`after` 是上一页的续扫位置（不透明，见 [`SessionPageCursor`]）。
pub struct SessionListPage {
    pub records: Vec<SessionRecord>,
    pub next: Option<SessionPageCursor>,
}

/// 会话列表对外的续扫位置。里面是上一页最后一条的 id（降序续扫 `id < after`）：之后新建的会话 id
/// 更大，只会出现在第一页之前，不会挤进后面的页造成重复或漏项；翻页期间被删的只是不再出现。
/// 编码成十六进制，客户端只原样带回，不解读。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPageCursor(SessionCursor);

impl SessionPageCursor {
    pub fn encode(&self) -> String {
        self.0
            .after()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// 解不出来（不是偶数位十六进制、不是 UTF-8、空）是 [`SessionError::Invalid`]。
    pub fn decode(text: &str) -> Result<Self, SessionError> {
        let invalid = || SessionError::Invalid("cursor 无效".into());
        if text.is_empty() || text.len() % 2 != 0 {
            return Err(invalid());
        }
        let bytes = (0..text.len())
            .step_by(2)
            .map(|at| {
                text.get(at..at + 2)
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok())
            })
            .collect::<Option<Vec<u8>>>()
            .ok_or_else(invalid)?;
        let after = String::from_utf8(bytes).map_err(|_| invalid())?;
        Ok(Self(SessionCursor::new(after, ScanOrder::Descending)))
    }
}

impl DurableCore {
    fn fresh_session_id(&self) -> String {
        SessionId::new_at(self.clock.now()).to_string()
    }

    /// 建会话（一次提交）。
    pub async fn create_session(&self, spec: NewSession) -> Result<SessionRecord, SessionError> {
        let id = spec.id.clone().unwrap_or_else(|| self.fresh_session_id());
        Ok(self
            .engine
            .commit(move |tx| Box::pin(async move { create_in(tx, &spec, &id).await }))
            .await?)
    }

    /// 同一提交线内按请求键查找或创建，返回行和是否去重。
    pub async fn create_session_idempotent(
        &self,
        mut spec: NewSession,
        key: Option<String>,
        body_hash: String,
    ) -> Result<(SessionRecord, bool), SessionError> {
        let Some(key) = key else {
            return Ok((self.create_session(spec).await?, false));
        };
        let storage = self.storage.clone();
        let id = spec.id.clone().unwrap_or_else(|| self.fresh_session_id());
        spec.request_key = Some(key.clone());
        spec.request_hash = Some(body_hash.clone());
        let result = self
            .engine
            .commit(move |tx| {
                Box::pin(async move {
                    if let Some(record) = storage.session_by_request(&key).await? {
                        return Ok(
                            if record.request_hash.as_deref() == Some(body_hash.as_str()) {
                                Ok((record, true))
                            } else {
                                Err(SessionError::RequestKeyConflict(key))
                            },
                        );
                    }
                    Ok(Ok((create_in(tx, &spec, &id).await?, false)))
                })
            })
            .await?;
        result
    }

    /// 一个会话的行；不在是 [`SessionError::NotFound`]。
    pub async fn session(&self, id: &str) -> Result<SessionRecord, SessionError> {
        self.storage
            .session(id)
            .await?
            .ok_or_else(|| SessionError::NotFound(format!("会话 {id}")))
    }

    /// 会话列表（最新的在前）。默认只列 `active` 与 `closing`；`all` 把 `deleted` 也带上。
    /// `purged` 两种都不列（只在显式查看单个会话时可见，§8.10）。全部读完：内部调用方（找回、
    /// 任务清单、监听）要的是全量，不受列表分页影响。
    pub async fn list_sessions(&self, all: bool) -> Result<Vec<SessionRecord>, SessionError> {
        let query = list_query(all);
        let mut records = Vec::new();
        let mut cursor: Option<SessionCursor> = None;
        loop {
            let page = self
                .storage
                .scan_sessions(&query, SCAN_PAGE, cursor.as_ref())
                .await?;
            records.extend(page.items);
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(records),
            }
        }
    }

    /// 会话列表的一页（次序与 [`Self::list_sessions`] 相同）。条件下推到存储：每种生命周期各走一次
    /// `(lifecycle, id)` 索引，一页的读取量只和 `limit` 有关。
    pub async fn list_sessions_page(
        &self,
        all: bool,
        limit: usize,
        after: Option<&SessionPageCursor>,
    ) -> Result<SessionListPage, SessionError> {
        let page = self
            .storage
            .scan_sessions(&list_query(all), limit, after.map(|cursor| &cursor.0))
            .await?;
        Ok(SessionListPage {
            records: page.items,
            next: page.next.map(SessionPageCursor),
        })
    }

    /// 这个会话现在接不接受新输入，以及不接受时那**一种**状态（§8.10）。会话不存在是 `NotFound`。
    pub async fn input_refusal(&self, id: &str) -> Result<Option<SessionState>, SessionError> {
        let record = self.session(id).await?;
        let state = state_of(record.lifecycle);
        Ok((!state.accepts_input()).then_some(state))
    }

    /// 某个 Agent 的**主会话**（渠道 peer 的 home session）：不存在就建（`kind = main`、
    /// `source = home`）。找与建在同一次提交里做，两个入口同时来也只会有一个。返回 `(行, 是否刚建)`。
    ///
    /// 只认还收输入的会话；同一个 Agent 有多条时取最早的那条。
    pub async fn main_session(&self, agent: &str) -> Result<(SessionRecord, bool), SessionError> {
        let spec = NewSession::new(agent)
            .kind(SessionKind::Main)
            .source(HOME_SOURCE);
        // 带上 `source`：群的线也是 `kind = main`，不能被当成操作者的主会话。
        let query = SessionQuery {
            kind: Some(SessionKind::Main),
            agent: Some(agent.to_owned()),
            source: Some(HOME_SOURCE.to_owned()),
            ..SessionQuery::default()
        };
        self.ensure_session(query, |_| true, spec).await
    }

    /// 群 `peer` 的线：不存在就建（`kind = main`，`source = line:…`，见 [`GROUP_LINE_PREFIX`]）。
    /// 只认还收输入的会话；同一个群有多条时取最早的。
    pub async fn group_line(
        &self,
        agent: &str,
        peer: &ChannelPeer,
    ) -> Result<(SessionRecord, bool), SessionError> {
        let source = group_line_source(peer);
        let spec = NewSession::new(agent)
            .kind(SessionKind::Main)
            .source(&source);
        let query = SessionQuery {
            kind: Some(SessionKind::Main),
            source: Some(source),
            ..SessionQuery::default()
        };
        self.ensure_session(query, |_| true, spec).await
    }

    /// 按旧办法开出来的话题会话（`kind = normal`，`source = chat:{peer}`）：有就返回、没有不建。
    /// 它们保持可用——话题里再来消息仍进它自己答，不迁移。
    pub async fn legacy_chat_session(
        &self,
        peer: &ChannelPeer,
    ) -> Result<Option<SessionRecord>, SessionError> {
        let query = SessionQuery {
            source: Some(crate::dispatcher::chat_origin(peer)),
            lifecycle: Some(SessionLifecycle::Active),
            order: Some(ScanOrder::Ascending),
            ..SessionQuery::default()
        };
        Ok(first_session(&*self.storage, &query, |record| {
            record.kind != SessionKind::Main
        })
        .await?)
    }

    /// 一个来源（`chat:telegram:123` 这类群 / 话题 / 带话题的私聊）对应的会话：不存在就建一个
    /// 普通会话。只认还收输入的会话（被删或正在关闭的不再接消息，下一条另开新的）；多条取最早的。
    /// 返回 `(行, 是否刚建)`。
    pub async fn session_for_source(
        &self,
        source: &str,
        agent: &str,
    ) -> Result<(SessionRecord, bool), SessionError> {
        let spec = NewSession::new(agent).source(source);
        let query = SessionQuery {
            source: Some(source.to_owned()),
            ..SessionQuery::default()
        };
        self.ensure_session(query, |record| record.kind != SessionKind::Main, spec)
            .await
    }

    /// 找到第一个（创建序最早的）还收输入、过 `query` 且 `matches` 的会话；没有就按 `spec` 建。
    /// 同一次提交，所以找与建之间不会有第二个入口插进来。`query` 下推成带索引的条件，提交线里
    /// 只读命中的那几行，不扫全表。
    async fn ensure_session(
        &self,
        query: SessionQuery,
        matches: impl Fn(&SessionRecord) -> bool + Send + 'static,
        spec: NewSession,
    ) -> Result<(SessionRecord, bool), SessionError> {
        let storage = self.storage.clone();
        let id = self.fresh_session_id();
        let query = SessionQuery {
            lifecycle: Some(SessionLifecycle::Active),
            order: Some(ScanOrder::Ascending),
            ..query
        };
        Ok(self
            .engine
            .commit(move |tx| {
                Box::pin(async move {
                    // 先读后写：扫描读的是已提交状态，必须排在这次提交的第一次表写之前。
                    let found = first_session(&*storage, &query, matches).await?;
                    match found {
                        Some(record) => Ok((record, false)),
                        None => Ok((create_in(tx, &spec, &id).await?, true)),
                    }
                })
            })
            .await?)
    }

    /// 一个会话的概览（[`SessionSummary`]）：`current_run` 是当前分支上在跑 / 排着队的那条 run
    /// （`RunId` = 首个输入 submission 的十进制 ID）；`current_wait` 只会是 `Retry`（§1）。
    pub async fn session_summary(
        &self,
        record: &SessionRecord,
    ) -> Result<SessionSummary, SessionError> {
        use komo_runtime::durable::harness::Harness;
        if record.lifecycle == SessionLifecycle::Purged {
            // 墓碑：conversation 行已经 purge 了（W4a），没有 run、没有 entry，只剩"它曾经存在"。
            return Ok(self.build_summary(
                record,
                None,
                None,
                None,
                komo_kernel::types::ids::Seq::ZERO,
            ));
        }
        let live = Harness::live(&*self.storage, record.current_branch).await?;
        let (current_run, current_state, current_wait) = match &live.run {
            Some(run) => {
                let first = run.inputs.first().copied();
                match live.generation.as_ref().and_then(runs::retry_wait) {
                    Some(wait) => (first, Some(RunState::Waiting), Some(wait)),
                    None => (first, Some(RunState::Running), None),
                }
            }
            None => {
                // 没有在跑的 run：收件箱里排着的第一条输入就是下一条 run。
                let inbox = Harness::inbox(&*self.storage, record.current_branch).await?;
                let queued = inbox.items.iter().find_map(|item| match item {
                    InboxItem::Steer { id, .. } | InboxItem::FollowUp { id, .. } => Some(*id),
                    InboxItem::Write { .. } => None,
                });
                (queued, queued.map(|_| RunState::Queued), None)
            }
        };
        let applied_seq = match self.tail_entry(record.current_branch).await? {
            Some(entry) => legacy_ids::seq(entry, 0),
            None => komo_kernel::types::ids::Seq::ZERO,
        };
        Ok(self.build_summary(
            record,
            current_run,
            current_state,
            current_wait,
            applied_seq,
        ))
    }

    fn build_summary(
        &self,
        record: &SessionRecord,
        current_run: Option<SubmissionId>,
        current_state: Option<RunState>,
        current_wait: Option<komo_kernel::types::status::WaitReason>,
        applied_seq: komo_kernel::types::ids::Seq,
    ) -> SessionSummary {
        let created = time_of(record.created_at);
        SessionSummary {
            session: SessionId::from_raw(record.id.clone()),
            title: record.title.clone().unwrap_or_default(),
            state: state_of(record.lifecycle),
            workdir: record.cwd.clone(),
            current_run: current_run.map(run_of),
            current_state,
            current_wait,
            applied_seq,
            created_at: created,
            updated_at: if record.changed_at > 0 {
                time_of(record.changed_at)
            } else {
                created
            },
            kind: kind_name(record.kind).to_owned(),
            home: (record.kind == SessionKind::Task)
                .then(|| record.parent_session.clone().map(SessionId::from_raw))
                .flatten(),
            task: (record.kind == SessionKind::Task)
                .then(|| komo_kernel::types::handle::short_handle(record.root_conversation.get())),
        }
    }

    /// 一个 conversation 沿 fork 祖先拼出来的可见历史里最新的那条 entry。
    pub(super) async fn tail_entry(
        &self,
        conversation: ConversationId,
    ) -> Result<Option<EntryId>, SessionError> {
        let mut query = EntryQuery::new(conversation);
        query.order = Some(ScanOrder::Descending);
        let page = self.storage.scan_entries(&query, 1, None).await?;
        Ok(page.items.first().map(|entry| entry.id))
    }
}

fn run_of(first_input: SubmissionId) -> RunId {
    legacy_ids::run_id(first_input)
}
