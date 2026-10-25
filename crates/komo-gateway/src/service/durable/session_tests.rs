//! W1c 验收：会话与树服务层（不接 HTTP）。
//!
//! 直接开 `DurableCore`（临时目录里的 Turso 库）+ 一份测试钩子 + `FakeLlm` 当 `ModelStream`：
//! W1b 的真钩子还没装配，这里的钩子只给一个朴素的 Agent（没有工具），并记下每次 `setup` 时
//! 看到的 `komo.agent.model`，好断言 sticky 覆盖真的到了钩子手里。历史用 `seed` 经提交线直接
//! 写 entry——只有它们的 ID 是铸出来的，所以断言一律用返回的 ID。

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use komo_kernel::durable::compaction::wrap_summary;
use komo_kernel::durable::storage::{SubmissionQuery, TaskQuery};
use komo_kernel::durable::{
    ContentBlock, ConversationId, EditAction, EntryId, EntryRecord, Message, SessionLifecycle,
    StopReason, Storage, SubmissionStatus, derive_context,
};
use komo_kernel::projection::{DEFAULT_MODEL_RESULT_BYTES, ProjectionContext};
use komo_kernel::test_support::MemOutputStore;
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::resource::ResourceMounts;
use komo_kernel::types::surface::AgentSurface;
use komo_runtime::durable::EntryDraft;
use komo_runtime::durable::harness::{
    AgentHooks, AgentSetup, CompactionSettings, CompactionSetup, HookError, NoOrchestration,
    PromptSection, RetryPolicy, RunBudget, SubmissionDraft, SummaryPrompt, ToolEnv, ToolSet,
    WhenBusy,
};
use komo_runtime::llm::{ModelRequest, ModelStream};
use serde_json::json;
use tokio::sync::Semaphore;

use super::DurableCore;
use super::agent_doc;
use super::copy::ForkRequest;
use super::sessions::{NewSession, SessionError};
use super::submit::{InputRequest, ModelOverride};
use super::tree::{
    BEFORE_FIRST, CONTEXT_EDIT_ENTRY, LABEL_ENTRY, MODEL_CHANGE_ENTRY, NavigateRequest,
    SESSION_INFO_ENTRY, Summarizer, TreeData,
};
use crate::service::state::SystemClock;
use crate::service::test_support::harness::FakeLlm;
use crate::service::test_support::sample_snapshot;

// ------------------------------------------------------------------ 脚手架

struct TestHooks {
    storage: OnceLock<Arc<dyn Storage>>,
    /// 每次 `setup` 时 conversation 的 `komo.agent.model`。
    seen_models: Mutex<Vec<Option<String>>>,
}

impl TestHooks {
    fn new() -> Arc<Self> {
        Arc::new(TestHooks {
            storage: OnceLock::new(),
            seen_models: Mutex::new(Vec::new()),
        })
    }

    fn env() -> ToolEnv {
        let session = SessionId::from_raw("test");
        ToolEnv {
            session: session.clone(),
            source: PlanSource::Interactive { session },
            cwd: std::env::temp_dir(),
            roots: vec![],
            mounts: ResourceMounts::default(),
            projection: ProjectionContext {
                model_result_bytes: DEFAULT_MODEL_RESULT_BYTES,
                decay: None,
            },
            call_timeout: std::time::Duration::from_secs(60),
            detach_after: None,
            conversation: true,
            outputs: Arc::new(MemOutputStore::default()),
            images: None,
        }
    }
}

#[async_trait]
impl AgentHooks for TestHooks {
    async fn setup(
        &self,
        conversation: ConversationId,
        _context: &[Message],
    ) -> Result<AgentSetup, HookError> {
        if let Some(storage) = self.storage.get() {
            let doc = agent_doc::read(&**storage, conversation)
                .await
                .ok()
                .flatten();
            self.seen_models
                .lock()
                .unwrap()
                .push(doc.and_then(|doc| doc.model));
        }
        Ok(AgentSetup {
            sections: vec![PromptSection::new("base", "You are a test agent.")],
            tools: ToolSet::new(
                vec![],
                AgentSurface::new(Vec::<String>::new()),
                TestHooks::env(),
            ),
            model: sample_snapshot().model,
            budget: RunBudget::default(),
            cache_key: None,
            max_output_tokens: None,
        })
    }

    async fn compaction(
        &self,
        _conversation: ConversationId,
    ) -> Result<CompactionSetup, HookError> {
        Ok(CompactionSetup {
            enabled: false,
            settings: CompactionSettings {
                keep_recent_tokens: 1,
                ..CompactionSettings::default()
            },
            model: sample_snapshot().model,
            prompt: SummaryPrompt::default(),
            retry: RetryPolicy::default(),
            cache_key: None,
        })
    }
}

/// 模型要等放行才回答：让一条 run 停在"进行中"。
struct Gate {
    inner: Arc<FakeLlm>,
    permits: Arc<Semaphore>,
}

#[async_trait]
impl ModelStream for Gate {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        progress: &mut (dyn for<'m> FnMut(&'m komo_kernel::durable::AssistantMessage) + Send),
    ) -> Result<komo_kernel::durable::AssistantMessage, komo_kernel::types::turn::LlmError> {
        self.permits.acquire().await.expect("闸门").forget();
        self.inner.stream(request, progress).await
    }
}

async fn open_at(home: &Path, stream: Arc<dyn ModelStream>) -> (Arc<DurableCore>, Arc<TestHooks>) {
    let hooks = TestHooks::new();
    let core = DurableCore::open(
        home,
        Arc::new(SystemClock),
        stream,
        hooks.clone(),
        Arc::new(NoOrchestration),
        None,
    )
    .await
    .expect("打开 DurableCore");
    hooks.storage.set(core.storage.clone()).ok();
    core.resume();
    (core, hooks)
}

struct Fixture {
    dir: tempfile::TempDir,
    core: Arc<DurableCore>,
    hooks: Arc<TestHooks>,
    llm: Arc<FakeLlm>,
}

impl Fixture {
    async fn new(llm: Arc<FakeLlm>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let (core, hooks) = open_at(dir.path(), llm.clone()).await;
        Fixture {
            dir,
            core,
            hooks,
            llm,
        }
    }

    /// 一个新会话，上面经提交线直接写了几条历史（`(kind, message)` 按序）。
    async fn session_with(&self, history: &[(&str, Message)]) -> (String, Vec<EntryId>) {
        let record = self
            .core
            .create_session(NewSession::new("assistant").workdir("/work"))
            .await
            .unwrap();
        let ids = seed(&self.core, record.root_conversation, history).await;
        (record.id, ids)
    }

    async fn tree(&self, session: &str) -> TreeData {
        self.core.read_tree(session).await.unwrap()
    }

    /// 提交一条输入并等它收尾。
    async fn say(&self, session: &str, request: InputRequest) -> super::submit::Accepted {
        let accepted = self.core.submit_input(session, request).await.unwrap();
        self.core
            .wait_submission(accepted.submission)
            .await
            .unwrap();
        accepted
    }
}

fn user(text: &str) -> (&'static str, Message) {
    ("komo.user", Message::user(text, 1))
}

fn assistant(text: &str) -> (&'static str, Message) {
    (
        "komo.assistant",
        Message::assistant(vec![ContentBlock::text(text)], StopReason::Stop, 1),
    )
}

async fn seed(
    core: &DurableCore,
    conversation: ConversationId,
    history: &[(&str, Message)],
) -> Vec<EntryId> {
    let history: Vec<(String, Message)> = history
        .iter()
        .map(|(kind, message)| ((*kind).to_owned(), message.clone()))
        .collect();
    core.engine
        .commit(move |tx| {
            Box::pin(async move {
                let mut ids = Vec::new();
                for (kind, message) in history {
                    let entry = tx
                        .append_entry(
                            conversation,
                            EntryDraft::new(kind).with_model(vec![message]),
                        )
                        .await?;
                    ids.push(entry.id);
                }
                Ok(ids)
            })
        })
        .await
        .unwrap()
}

fn text_of(message: &Message) -> String {
    match message {
        Message::User(user) => match &user.content {
            komo_kernel::durable::Content::Text(text) => text.clone(),
            other => format!("{other:?}"),
        },
        other => format!("{other:?}"),
    }
}

/// 路径到 `leaf` 的模型上下文里所有文字（拼成一串，断言"在 / 不在"用）。
fn context_text(tree: &TreeData, leaf: EntryId) -> String {
    let path: Vec<EntryRecord> = tree.path(leaf).into_iter().cloned().collect();
    serde_json::to_string(&derive_context(&path, None).messages).unwrap()
}

fn kinds(tree: &TreeData, conversation: ConversationId) -> Vec<String> {
    tree.branch(conversation)
        .unwrap()
        .entries
        .iter()
        .map(|entry| entry.kind.clone())
        .collect()
}

// ------------------------------------------------------------------ 会话

#[tokio::test]
async fn creating_a_session_writes_its_row_root_and_agent_in_one_commit() {
    let fx = Fixture::new(FakeLlm::finisher("好")).await;
    let record = fx
        .core
        .create_session(
            NewSession::new("assistant")
                .title("t")
                .workdir("/work")
                .source("cli")
                .request_key("k"),
        )
        .await
        .unwrap();
    assert_eq!(record.lifecycle, SessionLifecycle::Active);
    assert_eq!(record.current_branch, record.root_conversation);
    assert_eq!(record.agent.as_deref(), Some("assistant"));
    assert!(record.created_at > 0 && record.created_at == record.changed_at);

    let stored = fx.core.session(&record.id).await.unwrap();
    assert_eq!(stored, record);
    let root = fx
        .core
        .storage
        .conversation(record.root_conversation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root.session.as_deref(), Some(record.id.as_str()));
    let doc = agent_doc::read(&*fx.core.storage, record.root_conversation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(doc.agent, "assistant");
    assert_eq!(doc.workdir.as_deref(), Some("/work"));
    assert_eq!(doc.source.as_deref(), Some("cli"));

    // 同一个 ID 不能建两次（`put_session` 会整条替换，不能靠它兜底）。
    let mut again = NewSession::new("assistant");
    again.id = Some(record.id.clone());
    assert!(fx.core.create_session(again).await.is_err());

    let summary = fx.core.session_summary(&record).await.unwrap();
    assert_eq!(summary.session.as_str(), record.id);
    assert_eq!(summary.kind, "normal");
    assert_eq!(summary.current_run, None);
    assert_eq!(summary.workdir.as_deref(), Some("/work"));
}

#[tokio::test]
async fn the_main_session_and_source_sessions_are_found_before_they_are_created() {
    let fx = Fixture::new(FakeLlm::finisher("好")).await;
    let (main, created) = fx.core.main_session("assistant").await.unwrap();
    assert!(created);
    let (again, created) = fx.core.main_session("assistant").await.unwrap();
    assert!(!created);
    assert_eq!(again.id, main.id);
    let (other, created) = fx.core.main_session("coder").await.unwrap();
    assert!(created && other.id != main.id, "每个 Agent 一份主会话");

    let (group, created) = fx
        .core
        .session_for_source("chat:telegram:123", "assistant")
        .await
        .unwrap();
    assert!(created);
    assert_eq!(group.source.as_deref(), Some("chat:telegram:123"));
    let (same, created) = fx
        .core
        .session_for_source("chat:telegram:123", "assistant")
        .await
        .unwrap();
    assert!(!created && same.id == group.id);

    let listed = fx.core.list_sessions(false).await.unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].id, other.id.max(group.id.clone()), "最新的在前");
    assert!(fx.core.input_refusal(&main.id).await.unwrap().is_none());
    assert!(matches!(
        fx.core.input_refusal("nope").await,
        Err(SessionError::NotFound(_))
    ));
}

// ------------------------------------------------------------------ 导航与分支

#[tokio::test]
async fn editing_an_input_forks_at_its_parent_on_submit() {
    let fx = Fixture::new(FakeLlm::finisher("model answer")).await;
    let (session, ids) = fx
        .session_with(&[
            user("shared task"),
            assistant("shared answer"),
            user("abandoned direction"),
            assistant("abandoned answer"),
        ])
        .await;
    let (u2, a1) = (ids[2], ids[1]);

    // 选中输入：落在它的上一条，原文给编辑框。
    let outcome = fx
        .core
        .navigate(&session, NavigateRequest::to(u2))
        .await
        .unwrap();
    assert_eq!(outcome.leaf, Some(a1));
    assert_eq!(outcome.editor_text.as_deref(), Some("abandoned direction"));

    // 提交：同一次提交里从 a1 fork，输入落在新分支，current_branch 改过去、leaf 清掉。
    let accepted = fx
        .say(
            &session,
            InputRequest::new("replacement direction").request_id("branch-input"),
        )
        .await;
    let tree = fx.tree(&session).await;
    assert_eq!(tree.branches.len(), 2, "根 + 一个 fork");
    let root = tree.session.root_conversation;
    let child = tree.branch(accepted.conversation).unwrap();
    assert_ne!(accepted.conversation, root);
    let fork = child.conversation.parent.unwrap();
    assert_eq!((fork.conversation, fork.at), (root, a1));
    assert_eq!(
        child.conversation.session.as_deref(),
        Some(session.as_str())
    );
    assert_eq!(tree.session.current_branch, accepted.conversation);
    assert_eq!(tree.session.leaf, None);
    assert!(
        child.entries[0].kind == "cli.input",
        "{:?}",
        kinds(&tree, accepted.conversation)
    );
    assert_eq!(
        tree.branch(root).unwrap().entries.len(),
        4,
        "老分支原封不动"
    );

    // 只有新路径进模型。
    let requests = fx.llm.requests.lock().unwrap();
    let sent = serde_json::to_string(&requests[0].messages).unwrap();
    drop(requests);
    assert!(
        sent.contains("shared task") && sent.contains("shared answer"),
        "{sent}"
    );
    assert!(sent.contains("replacement direction"), "{sent}");
    assert!(!sent.contains("abandoned"), "{sent}");

    // 两个分支都在树里；当前 leaf 是新分支的末端。
    assert_eq!(tree.leaf, tree.current_tail());
    assert!(tree.owner_of(ids[3]) == Some(root));
}

#[tokio::test]
async fn navigation_alone_appends_nothing_and_persists_the_leaf() {
    let llm = FakeLlm::finisher("好");
    let dir = tempfile::tempdir().unwrap();
    let (core, _) = open_at(dir.path(), llm.clone()).await;
    let record = core
        .create_session(NewSession::new("assistant"))
        .await
        .unwrap();
    let ids = seed(
        &core,
        record.root_conversation,
        &[user("q1"), assistant("a1"), user("q2"), assistant("a2")],
    )
    .await;
    let before = core.read_tree(&record.id).await.unwrap();
    assert_eq!(before.leaf, Some(ids[3]), "没导航过：跟着末端");

    let outcome = core
        .navigate(&record.id, NavigateRequest::to(ids[1]))
        .await
        .unwrap();
    assert_eq!((outcome.leaf, outcome.editor_text), (Some(ids[1]), None));
    let after = core.read_tree(&record.id).await.unwrap();
    assert_eq!(after.session.leaf, Some(ids[1]));
    assert_eq!(after.leaf, Some(ids[1]));
    assert_eq!(
        after
            .branch(record.root_conversation)
            .unwrap()
            .entries
            .len(),
        4,
        "导航不追加 entry"
    );
    assert_eq!(after.branches.len(), 1, "也不新建 conversation");

    // 重开之后位置还在。
    core.close().await.unwrap();
    let (core, _) = open_at(dir.path(), llm).await;
    let reopened = core.read_tree(&record.id).await.unwrap();
    assert_eq!(reopened.leaf, Some(ids[1]));
    assert_eq!(reopened.session.leaf, Some(ids[1]));

    // 导航回末端：记成"跟着末端"。
    core.navigate(&record.id, NavigateRequest::to(ids[3]))
        .await
        .unwrap();
    assert_eq!(core.session(&record.id).await.unwrap().leaf, None);

    // 选中第一条输入：leaf 落在"第一条之前"，提交时建新根 conversation。
    let outcome = core
        .navigate(&record.id, NavigateRequest::to(ids[0]))
        .await
        .unwrap();
    assert_eq!(outcome.leaf, None);
    assert_eq!(outcome.editor_text.as_deref(), Some("q1"));
    let tree = core.read_tree(&record.id).await.unwrap();
    assert_eq!(tree.session.leaf, Some(BEFORE_FIRST));
    assert_eq!(tree.leaf, None);
    let accepted = core
        .submit_input(&record.id, InputRequest::new("fresh start"))
        .await
        .unwrap();
    core.wait_submission(accepted.submission).await.unwrap();
    let tree = core.read_tree(&record.id).await.unwrap();
    assert_eq!(tree.branches.len(), 2);
    let new_root = tree.branch(accepted.conversation).unwrap();
    assert!(new_root.conversation.parent.is_none(), "新根没有 fork 点");
    assert_eq!(tree.session.current_branch, accepted.conversation);
    assert_eq!(tree.session.leaf, None);
    assert!(
        agent_doc::read(&*core.storage, accepted.conversation)
            .await
            .unwrap()
            .is_some()
    );
    core.close().await.unwrap();
}

#[tokio::test]
async fn navigating_to_the_end_of_another_branch_continues_that_branch_without_forking() {
    let fx = Fixture::new(FakeLlm::finisher("好")).await;
    let (session, ids) = fx
        .session_with(&[user("q1"), assistant("a1"), user("q2"), assistant("a2")])
        .await;
    // 在 a1 处分出第二个分支。
    fx.core
        .navigate(&session, NavigateRequest::to(ids[2]))
        .await
        .unwrap();
    let side = fx.say(&session, InputRequest::new("side question")).await;
    // 回到老分支的末端 a2 再提交：接在老分支后面，不再 fork。
    fx.core
        .navigate(&session, NavigateRequest::to(ids[3]))
        .await
        .unwrap();
    let back = fx.say(&session, InputRequest::new("main follow-up")).await;
    let tree = fx.tree(&session).await;
    assert_eq!(tree.branches.len(), 2);
    assert_eq!(back.conversation, tree.session.root_conversation);
    assert_ne!(back.conversation, side.conversation);
    assert_eq!(tree.session.current_branch, tree.session.root_conversation);
    assert_eq!(tree.session.leaf, None);
}

#[tokio::test]
async fn fork_copies_komo_agent_into_the_branch() {
    let fx = Fixture::new(FakeLlm::finisher("好")).await;
    let (session, ids) = fx
        .session_with(&[user("q1"), assistant("a1"), user("q2"), assistant("a2")])
        .await;
    let root = fx.core.session(&session).await.unwrap().root_conversation;
    let before = agent_doc::read(&*fx.core.storage, root)
        .await
        .unwrap()
        .unwrap();

    // 编辑成分支，同一提交复制 komo.agent；带 model 覆盖时 sticky 写在新分支上。
    fx.core
        .navigate(&session, NavigateRequest::to(ids[2]))
        .await
        .unwrap();
    let accepted = fx
        .say(
            &session,
            InputRequest::new("again").model(ModelOverride {
                alias: Some("job".into()),
                provider: Some("responses".into()),
                model_id: Some("job-model".into()),
                effort: Some("high".into()),
            }),
        )
        .await;
    let child = agent_doc::read(&*fx.core.storage, accepted.conversation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.agent, before.agent);
    assert_eq!(child.workdir, before.workdir);
    assert_eq!(child.model.as_deref(), Some("job"), "覆盖是 sticky 的");
    assert_eq!(child.effort.as_deref(), Some("high"));
    let root_doc = agent_doc::read(&*fx.core.storage, root)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root_doc, before, "老分支的 komo.agent 不动");
    assert_eq!(
        fx.hooks
            .seen_models
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .as_deref(),
        Some("job")
    );

    let tree = fx.tree(&session).await;
    let change = tree
        .branch(accepted.conversation)
        .unwrap()
        .entries
        .iter()
        .find(|entry| entry.kind == MODEL_CHANGE_ENTRY)
        .expect("model.change 展示 entry");
    assert_eq!(change.data.as_ref().unwrap()["alias"], "job");
    assert!(change.model.is_none(), "不进上下文");

    // 同样的覆盖再来一次：值没变，什么都不写。
    let again = fx
        .say(
            &session,
            InputRequest::new("third").model(ModelOverride {
                alias: Some("job".into()),
                effort: Some("high".into()),
                ..ModelOverride::default()
            }),
        )
        .await;
    let tree = fx.tree(&session).await;
    let changes = tree
        .branch(again.conversation)
        .unwrap()
        .entries
        .iter()
        .filter(|entry| entry.kind == MODEL_CHANGE_ENTRY)
        .count();
    assert_eq!(changes, 1);
}

#[tokio::test]
async fn the_same_request_key_is_one_submission_even_after_the_branch_moved() {
    let fx = Fixture::new(FakeLlm::finisher("好")).await;
    let (session, ids) = fx
        .session_with(&[user("q1"), assistant("a1"), user("q2"), assistant("a2")])
        .await;
    let first = fx
        .say(&session, InputRequest::new("hello").request_id("k1"))
        .await;
    assert!(!first.deduplicated);

    let replay = fx
        .core
        .submit_input(&session, InputRequest::new("hello").request_id("k1"))
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.submission, first.submission);

    // 内容不同：409 级的冲突，什么都没写。
    let conflict = fx
        .core
        .submit_input(
            &session,
            InputRequest::new("something else").request_id("k1"),
        )
        .await;
    assert!(
        matches!(conflict, Err(SessionError::RequestKeyConflict(_))),
        "{conflict:?}"
    );

    // 导航之后输入会去另一个 conversation，同一个键重发仍然认回原来那条。
    fx.core
        .navigate(&session, NavigateRequest::to(ids[2]))
        .await
        .unwrap();
    let replay = fx
        .core
        .submit_input(&session, InputRequest::new("hello").request_id("k1"))
        .await
        .unwrap();
    assert!(replay.deduplicated && replay.submission == first.submission);
    let tree = fx.tree(&session).await;
    assert_eq!(tree.branches.len(), 1, "重发没有造出分支");
    assert_eq!(tree.session.leaf, Some(ids[1]), "也没有动导航位置");
}

#[tokio::test]
async fn pi_style_entry_ids_parse_with_or_without_a_suffix() {
    use super::tree::parse_entry_id;
    assert_eq!(parse_entry_id("12").unwrap(), EntryId::new(12));
    assert_eq!(parse_entry_id("12:1").unwrap(), EntryId::new(12));
    for bad in ["", "0", "012", "x", "-1", ":1"] {
        assert!(parse_entry_id(bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn the_first_message_becomes_a_provisional_title() {
    let fx = Fixture::new(FakeLlm::finisher("好")).await;
    let record = fx
        .core
        .create_session(NewSession::new("assistant"))
        .await
        .unwrap();
    fx.say(&record.id, InputRequest::new("帮我看看这个报错\n第二行"))
        .await;
    let record = fx.core.session(&record.id).await.unwrap();
    assert_eq!(record.title.as_deref(), Some("帮我看看这个报错"));
    assert!(record.title_provisional);
    assert!(record.changed_at >= record.created_at);
}

// ------------------------------------------------------------------ 分支摘要、label、name、edit

#[tokio::test]
async fn a_branch_summary_forks_at_the_target_and_records_where_it_came_from() {
    let fx = Fixture::new(FakeLlm::finisher("SUMMARY OF LEFT")).await;
    let (session, ids) = fx
        .session_with(&[user("task"), assistant("answer"), user("left branch")])
        .await;
    let model = sample_snapshot().model;
    let outcome = fx
        .core
        .navigate(
            &session,
            NavigateRequest {
                target: ids[1],
                summarize: Some(Summarizer::new(fx.llm.clone(), model)),
                instructions: Some("keep decisions".into()),
                label: Some("checkpoint".into()),
            },
        )
        .await
        .unwrap();
    let tree = fx.tree(&session).await;
    assert_eq!(tree.branches.len(), 2);
    let child = tree.session.current_branch;
    assert_ne!(child, tree.session.root_conversation);
    let entries = &tree.branch(child).unwrap().entries;
    assert_eq!(
        entries.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        [BRANCH_SUMMARY, LABEL_ENTRY]
    );
    let summary = &entries[0];
    assert_eq!(
        summary.data.as_ref().unwrap()["fromId"],
        ids[2].get().to_string()
    );
    assert_eq!(summary.data.as_ref().unwrap()["summary"], "SUMMARY OF LEFT");
    assert!(text_of(&summary.model.as_ref().unwrap()[0]).contains("SUMMARY OF LEFT"));
    let label = &entries[1];
    assert_eq!(
        label.data.as_ref().unwrap()["targetId"],
        summary.id.get().to_string()
    );
    assert_eq!(label.data.as_ref().unwrap()["label"], "checkpoint");
    assert_eq!(outcome.leaf, Some(label.id));
    assert_eq!(tree.session.leaf, None);
    assert_eq!(tree.leaf, Some(label.id));

    // 摘要的输入是序列化过的被放弃那一段，不是整条历史。
    let requests = fx.llm.requests.lock().unwrap();
    let input = requests[0].messages[0].text.as_deref().unwrap();
    assert!(input.starts_with("<conversation>"), "{input}");
    assert!(
        input.contains("[User]: left branch") && !input.contains("[User]: task"),
        "{input}"
    );
    assert!(requests[0].system_prompt.contains("keep decisions"));
    drop(requests);

    // 摘要进上下文，被放弃的原话不进。
    let context = context_text(&tree, label.id);
    assert!(
        context.contains("SUMMARY OF LEFT") && !context.contains("left branch"),
        "{context}"
    );
}

const BRANCH_SUMMARY: &str = super::tree::BRANCH_SUMMARY_ENTRY;

#[tokio::test]
async fn labels_names_and_context_edits_are_entries_that_leave_history_alone() {
    let fx = Fixture::new(FakeLlm::finisher("好")).await;
    let (session, ids) = fx
        .session_with(&[user("original"), assistant("answer")])
        .await;
    let (u, a) = (ids[0], ids[1]);

    fx.core
        .context_edit(&session, u, Some(json!({"content": "replacement"})))
        .await
        .unwrap();
    fx.core
        .label(&session, a, Some("checkpoint".into()))
        .await
        .unwrap();
    let named = fx.core.name(&session, "tree session").await.unwrap();

    let tree = fx.tree(&session).await;
    let root = tree.session.root_conversation;
    assert_eq!(tree.branches.len(), 1, "leaf 在末端，不分叉");
    assert_eq!(
        kinds(&tree, root),
        [
            "komo.user",
            "komo.assistant",
            CONTEXT_EDIT_ENTRY,
            LABEL_ENTRY,
            SESSION_INFO_ENTRY
        ]
    );
    assert_eq!(named.leaf, tree.current_tail());
    assert_eq!(
        tree.session.title.as_deref(),
        Some("tree session"),
        "name 同提交改标题"
    );
    assert!(!tree.session.title_provisional);
    let edit = &tree.branch(root).unwrap().entries[2];
    assert_eq!(edit.edits.len(), 1);
    assert_eq!(edit.edits[0].target, u);
    assert!(matches!(edit.edits[0].action, EditAction::Replace { .. }));
    // 原始历史没动，最新的路径上用的是替换。
    assert_eq!(
        text_of(&tree.entry(u).unwrap().model.as_ref().unwrap()[0]),
        "original"
    );
    assert!(context_text(&tree, tree.leaf.unwrap()).contains("replacement"));

    // 导航回编辑之前的位置：上下文里又是原文，编辑只在选中的路径上生效。
    fx.core
        .navigate(&session, NavigateRequest::to(a))
        .await
        .unwrap();
    let tree = fx.tree(&session).await;
    let context = context_text(&tree, tree.leaf.unwrap());
    assert!(
        context.contains("original") && !context.contains("replacement"),
        "{context}"
    );

    // 在 a 处打标签：leaf 不在末端，先落分支（label 只是展示，不进上下文）。
    fx.core
        .label(&session, u, Some("first".into()))
        .await
        .unwrap();
    let tree = fx.tree(&session).await;
    assert_eq!(tree.branches.len(), 2);
    assert_eq!(kinds(&tree, tree.session.current_branch), [LABEL_ENTRY]);
    assert_eq!(tree.session.leaf, None);

    // 不是消息的条目不能编辑；不存在的目标是 NotFound。
    let label_id = tree.branch(tree.session.current_branch).unwrap().entries[0].id;
    assert!(matches!(
        fx.core.context_edit(&session, label_id, None).await,
        Err(SessionError::Invalid(_))
    ));
    assert!(matches!(
        fx.core.label(&session, EntryId::new(999_999), None).await,
        Err(SessionError::NotFound(_))
    ));
}

// ------------------------------------------------------------------ 复制

#[tokio::test]
async fn clone_copies_entries_and_blobs_and_survives_source_purge() {
    let fx = Fixture::new(FakeLlm::finisher("好")).await;
    let (session, ids) = fx.session_with(&[user("task"), assistant("result")]).await;
    // 源会话的 blob：工具输出与 artifact。
    let source_dir = fx.dir.path().join("sessions").join(&session);
    std::fs::create_dir_all(source_dir.join("artifacts/r")).unwrap();
    std::fs::write(source_dir.join("artifacts/r/report.txt"), "report evidence").unwrap();
    std::fs::create_dir_all(source_dir.join("tool-output/7/9/1")).unwrap();
    std::fs::write(source_dir.join("tool-output/7/9/1/stdout"), "out").unwrap();

    let clone = fx
        .core
        .fork_session(
            &session,
            ForkRequest {
                target: None,
                clone: true,
            },
        )
        .await
        .unwrap();
    assert_ne!(clone.id, session);
    assert_eq!(clone.kind, komo_kernel::durable::SessionKind::Normal);
    assert_eq!(clone.parent_session, None);

    // 逐条复制了 entry（新 ID），不复制 task / submission。
    let copied = fx.tree(&clone.id).await;
    assert_eq!(copied.branches.len(), 1);
    let entries = &copied.branch(clone.root_conversation).unwrap().entries;
    assert_eq!(entries.len(), 2);
    assert!(
        entries.iter().all(|entry| !ids.contains(&entry.id)),
        "新铸的 ID"
    );
    assert_eq!(text_of(&entries[0].model.as_ref().unwrap()[0]), "task");
    assert_eq!(copied.leaf, Some(entries[1].id));
    let storage = &fx.core.storage;
    let submissions = storage
        .scan_submissions(
            &SubmissionQuery {
                conversation: Some(clone.root_conversation),
                ..SubmissionQuery::default()
            },
            10,
            None,
        )
        .await
        .unwrap();
    assert!(submissions.items.is_empty());
    let tasks = storage
        .scan_tasks(
            &TaskQuery {
                conversation: Some(clone.root_conversation),
                ..TaskQuery::default()
            },
            10,
            None,
        )
        .await
        .unwrap();
    assert!(tasks.items.is_empty());
    // komo.agent 复制了，但来源不跟着走。
    let doc = agent_doc::read(&**storage, clone.root_conversation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(doc.agent, "assistant");
    assert_eq!(doc.workdir.as_deref(), Some("/work"));

    // 源会话被 purge（行、全部 conversation、目录）之后，副本照样完整。
    let source = fx.tree(&session).await;
    let conversations: Vec<ConversationId> =
        source.branches.iter().map(|b| b.conversation.id).collect();
    fx.core
        .engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.purge_conversations(conversations);
                Ok(())
            })
        })
        .await
        .unwrap();
    std::fs::remove_dir_all(&source_dir).unwrap();
    let survivor = fx.tree(&clone.id).await;
    assert_eq!(
        survivor
            .branch(clone.root_conversation)
            .unwrap()
            .entries
            .len(),
        2
    );
    let clone_dir = fx.dir.path().join("sessions").join(&clone.id);
    assert_eq!(
        std::fs::read_to_string(clone_dir.join("artifacts/r/report.txt")).unwrap(),
        "report evidence"
    );
    assert_eq!(
        std::fs::read_to_string(clone_dir.join("tool-output/7/9/1/stdout")).unwrap(),
        "out"
    );
    assert!(
        !fx.dir
            .path()
            .join("sessions")
            .join(format!(".{}.fork", clone.id))
            .exists(),
        "暂存目录不留"
    );
    // 副本是活的：能接着聊。
    fx.say(&clone.id, InputRequest::new("continue")).await;
}

#[tokio::test]
async fn fork_stops_before_the_selected_input_and_clone_keeps_it() {
    let fx = Fixture::new(FakeLlm::finisher("好")).await;
    let (session, ids) = fx
        .session_with(&[user("q1"), assistant("a1"), user("q2"), assistant("a2")])
        .await;
    let forked = fx
        .core
        .fork_session(
            &session,
            ForkRequest {
                target: Some(ids[2]),
                clone: false,
            },
        )
        .await
        .unwrap();
    let tree = fx.tree(&forked.id).await;
    assert_eq!(
        kinds(&tree, forked.root_conversation),
        ["komo.user", "komo.assistant"]
    );
    let cloned = fx
        .core
        .fork_session(
            &session,
            ForkRequest {
                target: Some(ids[2]),
                clone: true,
            },
        )
        .await
        .unwrap();
    let tree = fx.tree(&cloned.id).await;
    assert_eq!(
        kinds(&tree, cloned.root_conversation),
        ["komo.user", "komo.assistant", "komo.user"]
    );
    // 默认目标是当前 leaf（导航过就是导航到的位置）。
    fx.core
        .navigate(&session, NavigateRequest::to(ids[1]))
        .await
        .unwrap();
    let at_leaf = fx
        .core
        .fork_session(&session, ForkRequest::default())
        .await
        .unwrap();
    let tree = fx.tree(&at_leaf.id).await;
    assert_eq!(
        tree.branch(at_leaf.root_conversation)
            .unwrap()
            .entries
            .len(),
        2
    );
    // 第一条输入之前没有东西可复制：fork 出一个空会话，能接着聊。
    let empty = fx
        .core
        .fork_session(
            &session,
            ForkRequest {
                target: Some(ids[0]),
                clone: false,
            },
        )
        .await
        .unwrap();
    assert!(
        fx.tree(&empty.id)
            .await
            .branch(empty.root_conversation)
            .unwrap()
            .entries
            .is_empty()
    );
    let blank = fx
        .core
        .create_session(NewSession::new("assistant"))
        .await
        .unwrap();
    assert!(matches!(
        fx.core
            .fork_session(&blank.id, ForkRequest::default())
            .await,
        Err(SessionError::Invalid(_))
    ));
}

#[tokio::test]
async fn copies_rewrite_heads_edit_targets_and_the_ids_gateway_entries_refer_to() {
    let fx = Fixture::new(FakeLlm::finisher("好")).await;
    let record = fx
        .core
        .create_session(NewSession::new("assistant"))
        .await
        .unwrap();
    let root = record.root_conversation;
    // reset（head 指自己）、一条输入、一条回答，再是指着输入的 context.edit 与 label。
    let (reset, input, answer) = fx
        .core
        .engine
        .commit(move |tx| {
            Box::pin(async move {
                let reset = tx
                    .append_entry(
                        root,
                        EntryDraft::new("komo.reset")
                            .with_head(komo_runtime::durable::DraftHead::SelfHead),
                    )
                    .await?;
                let (kind, message) = user("question");
                let input = tx
                    .append_entry(root, EntryDraft::new(kind).with_model(vec![message]))
                    .await?;
                let (kind, message) = assistant("answer");
                let answer = tx
                    .append_entry(root, EntryDraft::new(kind).with_model(vec![message]))
                    .await?;
                let mut edit = EntryDraft::new(CONTEXT_EDIT_ENTRY);
                edit.edits = vec![komo_kernel::durable::ContextEdit::omit(input.id)];
                tx.append_entry(root, edit).await?;
                tx.append_entry(
                    root,
                    EntryDraft::new(LABEL_ENTRY)
                        .with_data(json!({"targetId": answer.id.get().to_string(), "label": "x"})),
                )
                .await?;
                Ok((reset.id, input.id, answer.id))
            })
        })
        .await
        .unwrap();
    let clone = fx
        .core
        .fork_session(
            &record.id,
            ForkRequest {
                target: None,
                clone: true,
            },
        )
        .await
        .unwrap();
    let tree = fx.tree(&clone.id).await;
    let entries = &tree.branch(clone.root_conversation).unwrap().entries;
    assert_eq!(entries.len(), 5);
    assert_ne!(entries[0].id, reset);
    assert_eq!(entries[0].head, Some(entries[0].id), "reset 仍然指自己");
    assert_eq!(entries[3].edits[0].target, entries[1].id, "edit 指向新 ID");
    assert_ne!(entries[1].id, input);
    let label = entries[4].data.as_ref().unwrap();
    assert_eq!(label["targetId"], entries[2].id.get().to_string());
    assert_ne!(entries[2].id, answer);
}

// ------------------------------------------------------------------ 忙与压缩

#[tokio::test]
async fn tree_ops_are_refused_while_any_branch_is_busy() {
    let permits = Arc::new(Semaphore::new(0));
    let llm = FakeLlm::finisher("好");
    let dir = tempfile::tempdir().unwrap();
    let (core, _) = open_at(
        dir.path(),
        Arc::new(Gate {
            inner: llm.clone(),
            permits: permits.clone(),
        }),
    )
    .await;
    let record = core
        .create_session(NewSession::new("assistant"))
        .await
        .unwrap();
    let ids = seed(
        &core,
        record.root_conversation,
        &[user("q1"), assistant("a1")],
    )
    .await;

    // 一条 run 停在进行中。
    let accepted = core
        .submit_input(&record.id, InputRequest::new("work"))
        .await
        .unwrap();
    assert_eq!(accepted.status, SubmissionStatus::Placed);
    let summary = core
        .session_summary(&core.session(&record.id).await.unwrap())
        .await
        .unwrap();
    assert_eq!(summary.current_run, Some(accepted.run.clone()));
    assert_eq!(
        summary.current_state,
        Some(komo_kernel::types::status::RunState::Running)
    );

    let busy = |result: Result<(), SessionError>| match result {
        Err(SessionError::Busy(_)) => {}
        other => panic!("忙时应该是 Busy：{other:?}"),
    };
    busy(
        core.navigate(&record.id, NavigateRequest::to(ids[0]))
            .await
            .map(drop),
    );
    busy(
        core.label(&record.id, ids[0], Some("x".into()))
            .await
            .map(drop),
    );
    busy(core.name(&record.id, "x").await.map(drop));
    busy(core.context_edit(&record.id, ids[0], None).await.map(drop));
    busy(core.compact(&record.id, None).await.map(drop));
    busy(
        core.fork_session(
            &record.id,
            ForkRequest {
                target: None,
                clone: true,
            },
        )
        .await
        .map(drop),
    );
    // 忙时提交：默认排队；`Reject` 被拒且什么都没写。
    let queued = core
        .submit_input(&record.id, InputRequest::new("later"))
        .await
        .unwrap();
    assert_eq!(queued.status, SubmissionStatus::Queued);
    let before = core.read_tree(&record.id).await.unwrap();
    busy(
        core.submit_input(
            &record.id,
            InputRequest::new("no").when_busy(WhenBusy::Reject),
        )
        .await
        .map(drop),
    );
    let after = core.read_tree(&record.id).await.unwrap();
    assert_eq!(
        after
            .branch(record.root_conversation)
            .unwrap()
            .entries
            .len(),
        before
            .branch(record.root_conversation)
            .unwrap()
            .entries
            .len()
    );

    // 放行：run 与排队的输入都跑完，树操作恢复。
    permits.add_permits(100);
    core.wait_submission(accepted.submission).await.unwrap();
    core.wait_submission(queued.submission).await.unwrap();
    core.name(&record.id, "idle again").await.unwrap();

    core.close().await.unwrap();
}

#[tokio::test]
async fn a_busy_branch_that_is_not_current_still_blocks_tree_ops() {
    let permits = Arc::new(Semaphore::new(100));
    let llm = FakeLlm::finisher("好");
    let dir = tempfile::tempdir().unwrap();
    let (core, _) = open_at(
        dir.path(),
        Arc::new(Gate {
            inner: llm,
            permits: permits.clone(),
        }),
    )
    .await;
    let record = core
        .create_session(NewSession::new("assistant"))
        .await
        .unwrap();
    let ids = seed(
        &core,
        record.root_conversation,
        &[user("q1"), assistant("a1"), user("q2"), assistant("a2")],
    )
    .await;
    core.navigate(&record.id, NavigateRequest::to(ids[2]))
        .await
        .unwrap();
    let side = core
        .submit_input(&record.id, InputRequest::new("side"))
        .await
        .unwrap();
    core.wait_submission(side.submission).await.unwrap();
    let current = core.session(&record.id).await.unwrap().current_branch;
    assert_ne!(current, record.root_conversation);

    // 当前分支空闲，老分支（根）上有一条 run 停着。
    permits.forget_permits(100);
    core.harness
        .submit(
            &core.engine,
            record.root_conversation,
            SubmissionDraft::input("held"),
        )
        .await
        .unwrap();
    assert!(matches!(
        core.name(&record.id, "x").await,
        Err(SessionError::Busy(_))
    ));
    assert!(matches!(
        core.navigate(&record.id, NavigateRequest::to(ids[0])).await,
        Err(SessionError::Busy(_))
    ));
    permits.add_permits(100);
    core.engine.wait_for_idle(None).await.unwrap();
    core.name(&record.id, "idle again").await.unwrap();
    core.close().await.unwrap();
}

#[tokio::test]
async fn compact_runs_on_the_current_branch_and_waits_for_the_summary() {
    let fx = Fixture::new(FakeLlm::finisher("MANUAL SUMMARY")).await;
    let (session, _) = fx
        .session_with(&[
            user("first question"),
            assistant("first answer"),
            user("second question"),
            assistant("second answer"),
        ])
        .await;
    let outcome = fx
        .core
        .compact(&session, Some("keep decisions".into()))
        .await
        .unwrap();
    let summary = outcome.summary.expect("摘要 entry");
    // 等回来的时候摘要已经落在当前分支上。
    let tree = fx.tree(&session).await;
    let root = tree.session.root_conversation;
    assert_eq!(tree.branches.len(), 1);
    let entry = tree.entry(summary).unwrap();
    assert_eq!(entry.kind, "komo.compaction");
    assert_eq!(entry.data.as_ref().unwrap()["reason"], "manual");
    assert_eq!(
        text_of(&entry.model.as_ref().unwrap()[0]),
        wrap_summary("MANUAL SUMMARY")
    );
    assert_eq!(tree.owner_of(summary), Some(root));
    assert_eq!(tree.leaf, Some(summary), "摘要是新的末端");
    assert_eq!(fx.llm.turns(), 1);
    let context = context_text(&tree, summary);
    assert!(context.contains("MANUAL SUMMARY"), "{context}");

    // 空会话没有可压缩的内容。
    let blank = fx
        .core
        .create_session(NewSession::new("assistant"))
        .await
        .unwrap();
    assert!(matches!(
        fx.core.compact(&blank.id, None).await,
        Err(SessionError::Invalid(_))
    ));
}

#[tokio::test]
async fn compact_after_navigation_works_on_a_fresh_branch() {
    let fx = Fixture::new(FakeLlm::finisher("MANUAL SUMMARY")).await;
    let (session, ids) = fx
        .session_with(&[
            user("first question"),
            assistant("first answer"),
            user("second question"),
            assistant("second answer"),
        ])
        .await;
    fx.core
        .navigate(&session, NavigateRequest::to(ids[1]))
        .await
        .unwrap();
    let outcome = fx.core.compact(&session, None).await.unwrap();
    let tree = fx.tree(&session).await;
    assert_eq!(tree.branches.len(), 2, "leaf 不在末端先 fork");
    let summary = outcome.summary.unwrap();
    assert_eq!(tree.owner_of(summary), Some(tree.session.current_branch));
    assert_ne!(tree.session.current_branch, tree.session.root_conversation);
    assert_eq!(tree.session.leaf, None);
    let context = context_text(&tree, summary);
    assert!(!context.contains("second"), "{context}");
}

// ------------------------------------------------------------------ 其余

#[tokio::test]
async fn a_closed_session_refuses_input_and_tree_changes() {
    let fx = Fixture::new(FakeLlm::finisher("好")).await;
    let (session, ids) = fx.session_with(&[user("q"), assistant("a")]).await;
    let mut record = fx.core.session(&session).await.unwrap();
    record.lifecycle = SessionLifecycle::Closing;
    fx.core
        .engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.put_session(record);
                Ok(())
            })
        })
        .await
        .unwrap();
    use komo_kernel::types::status::SessionState;
    assert_eq!(
        fx.core.input_refusal(&session).await.unwrap(),
        Some(SessionState::Closing)
    );
    assert!(matches!(
        fx.core.submit_input(&session, InputRequest::new("x")).await,
        Err(SessionError::NotOpen(SessionState::Closing))
    ));
    assert!(matches!(
        fx.core
            .navigate(&session, NavigateRequest::to(ids[0]))
            .await,
        Err(SessionError::NotOpen(SessionState::Closing))
    ));
    assert!(matches!(
        fx.core.boundary(&session, None).await,
        Err(SessionError::NotOpen(_))
    ));
    // 默认列表还列 closing；all 才带 deleted。
    assert_eq!(fx.core.list_sessions(false).await.unwrap().len(), 1);
}

#[tokio::test]
async fn boundary_and_cancel_act_on_the_current_branch() {
    let fx = Fixture::new(FakeLlm::finisher("好")).await;
    let (session, _) = fx.session_with(&[user("q"), assistant("a")]).await;
    let id = fx
        .core
        .boundary(&session, Some("handoff".into()))
        .await
        .unwrap();
    let record = fx.core.wait_submission(id).await.unwrap();
    assert_eq!(record.status(), SubmissionStatus::Done);
    let tree = fx.tree(&session).await;
    let reset = tree
        .branch(tree.session.root_conversation)
        .unwrap()
        .entries
        .last()
        .unwrap();
    assert_eq!(reset.kind, "komo.reset");
    assert_eq!(reset.head, Some(reset.id));
    assert!(
        !fx.core.cancel_run(&session).await.unwrap(),
        "空闲时没有 run 可取消"
    );
    // reset 没动 komo.agent。
    assert!(
        agent_doc::read(&*fx.core.storage, tree.session.root_conversation)
            .await
            .unwrap()
            .is_some()
    );
}
