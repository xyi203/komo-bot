//! 钩子的验收（`docs/storage-durable-d5.md` §4 W1b）。
//!
//! 直接对一台 Durable Gateway 的 `GatewayState` 问钩子（它们的输入只有已提交的 document 与当前
//! 配置），再加一个端到端用例：经 `DurableCore.harness.submit` 跑一整轮，看真 harness 上钩子给的
//! 系统段、工具表与模型。

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::durable::recall::{MEMORY_RECALL_ENTRY_KIND, MEMORY_REVOKE_ENTRY_KIND};
use komo_kernel::durable::{
    Content, ContextEdit, ContextView, ConversationId, EditAction, EntryId, EntryRecord, Message,
    Sections, SubmissionId, SystemMessage, TaskId, derive_context,
};
use komo_kernel::protocol::matter::{MatterActionRequest, MatterAddRequest};
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::delegate::DelegateContract;
use komo_kernel::types::ids::MemoryId;
use komo_kernel::types::matter::{Matter, MatterKind, MatterVerb};
use komo_kernel::types::memory::{
    Confirmation, ExtractionMetadata, MemoryItem, MemoryKind, MemoryScope, MemoryState,
    MemoryUsage, Provenance,
};
use komo_kernel::types::model::Effort;
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use komo_kernel::types::turn::MemoryUse;
use komo_runtime::durable::harness::orchestration::store_task_doc;
use komo_runtime::durable::harness::{
    AgentHooks, ContextRequest, HookError, OrchestrationHooks, SubmissionDraft, TaskDoc,
};
use komo_runtime::durable::{ConversationOwnership, EntryDraft};
use komo_runtime::matter::Caller;

use super::GatewayHooks;
use crate::service::durable::agent_doc::{self, AgentDoc};
use crate::service::durable::assemble::{self as assemble_slot, StateSlot};
use crate::service::durable::orchestration::{GatewayOrchestration, MonitorRunner};
use crate::service::test_support::TestGateway;
use crate::service::test_support::harness::{FakeLlm, call_round, text_round};

/// 带两个模型 alias 的配置；`extra` 接在 `[models]` 之后。
fn config_with(agent: &str, memory: &str) -> String {
    format!(
        r#"
default_agent = "assistant"

[agents.assistant]
{agent}

[model.main]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[model.fast]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "fast-test"
api_key_env = "KOMO_LLM_API_KEY"

[models]
default = "main"

{memory}
"#
    )
}

const NO_MEMORY: &str = "[memory]\nenabled = false\n";

struct World {
    gateway: TestGateway,
    slot: StateSlot,
}

impl World {
    async fn new(config: &str, llm: Arc<FakeLlm>) -> World {
        let gateway = TestGateway::with(config, llm).await;
        let slot = assemble_slot::new_slot();
        // 这个世界只测钩子，不装转后台：给一个用不上的空槽。
        assemble_slot::wire(&slot, gateway.state(), &Default::default());
        World { gateway, slot }
    }

    fn hooks(&self) -> Arc<GatewayHooks> {
        GatewayHooks::new(Arc::clone(&self.slot))
    }

    fn core(&self) -> &Arc<crate::service::durable::DurableCore> {
        self.gateway.state().core().expect("Durable")
    }

    /// 建一个对话 conversation（可带 `komo.agent`）。
    async fn conversation(&self, doc: Option<AgentDoc>) -> ConversationId {
        self.core()
            .engine
            .commit(move |tx| {
                Box::pin(async move {
                    let record = tx
                        .create_conversation(ConversationOwnership::Ownerless)
                        .await?;
                    if let Some(doc) = &doc {
                        agent_doc::write_in(tx, record.id, doc).await?;
                    }
                    Ok(record.id)
                })
            })
            .await
            .expect("提交得了")
    }

    /// 建一个任务 conversation：带 `komo.task`，没有 `komo.agent`。`fork_at` 给了就从父 fork。
    async fn task_conversation(
        &self,
        parent: ConversationId,
        fork_at: Option<EntryId>,
        build: impl FnOnce(TaskDoc) -> TaskDoc + Send + 'static,
    ) -> ConversationId {
        self.core()
            .engine
            .commit(move |tx| {
                Box::pin(async move {
                    let record = match fork_at {
                        Some(at) => {
                            tx.fork_conversation(parent, at, ConversationOwnership::Ownerless)
                                .await?
                        }
                        None => {
                            tx.create_conversation(ConversationOwnership::Ownerless)
                                .await?
                        }
                    };
                    let doc = build(TaskDoc {
                        parent,
                        anchor: TaskId::new(1),
                        title: "测试任务".into(),
                        fork: fork_at.is_some(),
                        model: None,
                        effort: None,
                        contracts: BTreeMap::new(),
                        last_run: None,
                        continued: None,
                    });
                    store_task_doc(tx, record.id, &doc).await?;
                    Ok(record.id)
                })
            })
            .await
            .expect("提交得了")
    }

    /// 往 conversation 里追加一条 entry，返回它的 ID。
    async fn entry(&self, conversation: ConversationId, kind: &str, message: Message) -> EntryId {
        let kind = kind.to_owned();
        self.core()
            .engine
            .commit(move |tx| {
                Box::pin(async move {
                    let draft =
                        komo_runtime::durable::EntryDraft::new(kind).with_model(vec![message]);
                    Ok(tx.append_entry(conversation, draft).await?.id)
                })
            })
            .await
            .expect("提交得了")
    }

    fn write_config_and_reload(&self, config: &str) {
        self.gateway.write_config(config);
        self.gateway
            .state()
            .config
            .reload()
            .expect("新配置校验得过");
    }
}

fn agent(name: &str) -> AgentDoc {
    AgentDoc::new(name)
}

fn system_message(sections: &[(&str, &str)]) -> Message {
    let mut patch = Sections::default();
    for (key, text) in sections {
        patch = patch.set(*key, *text);
    }
    Message::System(SystemMessage {
        content: Content::Text(String::new()),
        sections: Some(patch),
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
        extra: Default::default(),
    })
}

fn texts(sections: &[komo_runtime::durable::harness::PromptSection]) -> Vec<(String, String)> {
    sections
        .iter()
        .map(|section| (section.key.clone(), section.text.clone()))
        .collect()
}

fn names(definitions: Vec<ToolDefinition>) -> Vec<String> {
    definitions.into_iter().map(|tool| tool.name).collect()
}

fn finisher() -> Arc<FakeLlm> {
    FakeLlm::finisher("好")
}

// ---------------------------------------------------------------- setup

/// 任务会话：`komo.task` 给的模型名字按**当前**配置解析，热重载之后下一次就用新的；
/// 名字不在配置里就是没有可用的模型（`no_model`）。
#[tokio::test]
async fn setup_resolves_task_doc_model_names_against_current_config() {
    let world = World::new(&config_with("", NO_MEMORY), finisher()).await;
    let hooks = world.hooks();
    let parent = world.conversation(Some(agent("assistant"))).await;
    let task = world
        .task_conversation(parent, None, |mut doc| {
            doc.model = Some("fast".into());
            doc.effort = Some(Effort::new("low"));
            doc
        })
        .await;

    let setup = hooks.setup(task, &[]).await.expect("解析得出");
    assert_eq!(setup.model.model, "fast-test", "task 给的名字优先");
    assert_eq!(setup.model.effort, Some(Effort::new("low")));
    // 父没有 task 的覆盖：Agent 的默认模型。
    let parent_setup = hooks.setup(parent, &[]).await.unwrap();
    assert_eq!(parent_setup.model.model, "gpt-test");

    // 热重载：`fast` 指向另一个上游模型 id，下一次解析就用新的。
    world.write_config_and_reload(&config_with("", NO_MEMORY).replace("fast-test", "fast-test-v2"));
    let after = hooks.setup(task, &[]).await.unwrap();
    assert_eq!(after.model.model, "fast-test-v2");

    // alias 删掉了：没有可用的模型，说清是哪一个。
    world.write_config_and_reload(
        &config_with("", NO_MEMORY).replace("[model.fast]", "[model.other]"),
    );
    let Err(HookError::NoModel(message)) = hooks.setup(task, &[]).await else {
        panic!("该是 NoModel");
    };
    assert!(message.contains("fast"), "{message}");
}

/// `komo.agent` → 默认 Agent：没有 document 的 conversation 用 `default_agent`；document 指向已经
/// 删掉的 Agent 时退回默认 Agent，不让输入失败；`komo.agent.model` / `effort` / `workdir` 生效。
#[tokio::test]
async fn setup_falls_back_to_the_default_agent_and_honours_the_agent_doc() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = komo_runtime::tools::paths::real_root(dir.path());
    let world = World::new(
        &config_with("instructions = \"默认人格\"", NO_MEMORY),
        finisher(),
    )
    .await;
    let hooks = world.hooks();

    let bare = world.conversation(None).await;
    let setup = hooks.setup(bare, &[]).await.unwrap();
    assert_eq!(setup.model.model, "gpt-test");
    assert!(
        texts(&setup.sections)[0].1 == "默认人格",
        "默认 Agent 的指令在最前：{:?}",
        setup.sections
    );

    let gone = world
        .conversation(Some(AgentDoc {
            agent: "deleted-agent".into(),
            model: Some("fast".into()),
            effort: Some("high".into()),
            workdir: Some(workdir.display().to_string()),
            max_rounds: Some(3),
            ..Default::default()
        }))
        .await;
    let setup = hooks.setup(gone, &[]).await.unwrap();
    assert_eq!(setup.model.model, "fast-test");
    assert_eq!(setup.model.effort, Some(Effort::new("high")));
    assert_eq!(setup.budget.max_rounds, Some(3));
    assert_eq!(setup.tools.env.cwd, workdir);
    assert!(
        texts(&setup.sections)
            .iter()
            .any(|(key, text)| key == "base" && text.contains(&workdir.display().to_string())),
        "base 段里是这个工作目录"
    );
    assert_eq!(setup.cache_key.as_deref(), Some(&*format!("c{gone}")));
}

/// 每个阶段把探到的窗口补进模型；配置里写了的不被盖掉；没探到的保持 `None`。
/// （旧的"受理时补进快照"在新存储里就是每次 `setup` 现读 `windows`。）
#[tokio::test]
async fn the_probed_window_is_filled_into_the_model_only_when_config_has_none() {
    use crate::service::windows::{WindowState, key_of};

    let world = World::new(&config_with("", NO_MEMORY), finisher()).await;
    let hooks = world.hooks();
    let state = world.gateway.state();
    let configured = state.snapshot().model.clone();
    assert_eq!(configured.context_window, None);
    let conversation = world.conversation(None).await;

    // 没探到 / 探到但 provider 没说：None。
    let setup = hooks.setup(conversation, &[]).await.unwrap();
    assert_eq!(setup.model.context_window, None);
    state
        .windows
        .finish(key_of(&configured), WindowState::Missing);
    let setup = hooks.setup(conversation, &[]).await.unwrap();
    assert_eq!(setup.model.context_window, None);

    // 探到了：补进模型。
    state
        .windows
        .finish(key_of(&configured), WindowState::Found(500_000));
    let setup = hooks.setup(conversation, &[]).await.unwrap();
    assert_eq!(setup.model.context_window, Some(500_000));
}

/// fork 任务返回父的段（`replay_sections`）与父的缓存键，模型沿用父此刻的。
#[tokio::test]
async fn fork_task_reuses_parent_sections_and_cache_key() {
    let world = World::new(&config_with("", NO_MEMORY), finisher()).await;
    let hooks = world.hooks();
    let parent = world
        .conversation(Some(AgentDoc {
            model: Some("fast".into()),
            ..agent("assistant")
        }))
        .await;
    let system = system_message(&[
        ("base", "父的 base"),
        ("project", "父的规范"),
        ("memory", "父的记忆"),
    ]);
    let at = world.entry(parent, "komo.system", system.clone()).await;
    let fork = world.task_conversation(parent, Some(at), |doc| doc).await;

    let setup = hooks
        .setup(fork, std::slice::from_ref(&system))
        .await
        .unwrap();
    assert_eq!(
        texts(&setup.sections),
        vec![
            ("base".to_owned(), "父的 base".to_owned()),
            ("project".to_owned(), "父的规范".to_owned()),
            ("memory".to_owned(), "父的记忆".to_owned()),
        ],
        "段原样沿用，一个字不换"
    );
    assert_eq!(setup.cache_key.as_deref(), Some(&*format!("c{parent}")));
    assert_eq!(setup.model.model, "fast-test", "fork 沿用父的模型");
    // 而 fresh 任务有自己的缓存键与 Agent 默认模型。
    let fresh = world.task_conversation(parent, None, |doc| doc).await;
    let setup = hooks.setup(fresh, &[]).await.unwrap();
    assert_eq!(setup.cache_key.as_deref(), Some(&*format!("c{fresh}")));
    assert_eq!(setup.model.model, "gpt-test");
}

/// fresh 任务的 `base` 段带任务模式（中途没人能回答）；对话会话不带。
#[tokio::test]
async fn a_fresh_task_session_gets_the_task_mode_in_its_base() {
    let world = World::new(&config_with("", NO_MEMORY), finisher()).await;
    let hooks = world.hooks();
    let parent = world.conversation(Some(agent("assistant"))).await;
    let task = world.task_conversation(parent, None, |doc| doc).await;
    let base_of = |sections: &[komo_runtime::durable::harness::PromptSection]| {
        sections
            .iter()
            .find(|section| section.key == "base")
            .map(|section| section.text.clone())
            .expect("有 base 段")
    };

    let base = base_of(&hooks.setup(task, &[]).await.unwrap().sections);
    assert!(
        base.ends_with(komo_agent::context::TASK_MODE_BLOCK),
        "{base}"
    );
    let base = base_of(&hooks.setup(parent, &[]).await.unwrap().sections);
    assert!(
        !base.contains(komo_agent::context::TASK_MODE_BLOCK),
        "{base}"
    );
}

/// 任务会话交给模型的工具表里没有 `task` 与 `monitor`（fresh 任务）；对话会话有。
#[tokio::test]
async fn task_sessions_lack_task_and_monitor_in_schema() {
    let world = World::new(&config_with("", NO_MEMORY), finisher()).await;
    let hooks = world.hooks();
    let parent = world.conversation(Some(agent("assistant"))).await;
    let task = world.task_conversation(parent, None, |doc| doc).await;

    let main = hooks.tools(parent).await.unwrap();
    let declared = names(main.definitions());
    for name in ["task", "monitor", "monitor_stop", "read", "bash"] {
        assert!(
            declared.iter().any(|have| have == name),
            "{name} in {declared:?}"
        );
    }
    assert!(main.env.conversation, "对话会话");
    assert!(main.env.detach_after.is_some(), "对话会话里长命令转后台");

    let child = hooks.tools(task).await.unwrap();
    let declared = names(child.definitions());
    for name in ["task", "monitor", "monitor_stop"] {
        assert!(
            !declared.iter().any(|have| have == name),
            "{name} 不该在任务会话里：{declared:?}"
        );
        assert!(child.get(name).is_none(), "{name} 也不在能力面里");
    }
    assert!(declared.iter().any(|have| have == "read"));
    assert!(!child.env.conversation);
    assert!(child.env.detach_after.is_none(), "任务会话不转后台");
    // fork 任务保留父的能力面：声明与父逐字相同，提示缓存才共用。
    let system = system_message(&[("base", "父的 base")]);
    let at = world.entry(parent, "komo.system", system).await;
    let fork = world.task_conversation(parent, Some(at), |doc| doc).await;
    let forked = hooks.tools(fork).await.unwrap();
    assert_eq!(forked.definitions(), main.definitions());
    // `setup` 里的工具表同一个口径。
    let via_setup = hooks.setup(task, &[]).await.unwrap();
    assert_eq!(names(via_setup.tools.definitions()), declared);
}

// ---------------------------------------------------------------- 系统段

fn seeded_memory(id: &str, content: &str) -> MemoryItem {
    let now = time::OffsetDateTime::now_utc();
    MemoryItem {
        id: MemoryId::from_raw(id),
        revision: 1,
        content: content.into(),
        kind: MemoryKind::Preference,
        scope: MemoryScope::Personal,
        provenance: Provenance::UserStatement,
        confirmation: Confirmation::Unconfirmed,
        state: MemoryState::Active,
        evidence: vec![],
        observed_at: now,
        valid_until: None,
        created_at: now,
        updated_at: now,
        extraction: ExtractionMetadata::new("hooks-test", None, "v1"),
        usage: MemoryUsage::default(),
        supersedes: None,
        superseded_by: None,
        application: None,
        applies_when: None,
        belongs_to: None,
    }
}

const KEYWORD_MEMORY: &str = "[memory]\nenabled = true\n\n[memory.retrieval]\nmode = \"keyword\"\n";

/// 项目规范一旦展示就钉住，直到下一次 head（上下文里没有已展示的段）才换；召回的记忆不进系统段。
#[tokio::test]
async fn project_section_is_pinned_until_the_next_head() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = komo_runtime::tools::paths::real_root(dir.path());
    std::fs::write(workdir.join("AGENTS.md"), "规范一").unwrap();
    let world = World::new(&config_with("", KEYWORD_MEMORY), finisher()).await;
    world
        .gateway
        .state()
        .memory
        .put(seeded_memory("m-1", "用户喜欢深色主题"), None)
        .await
        .unwrap();
    let hooks = world.hooks();
    let conversation = world
        .conversation(Some(AgentDoc {
            workdir: Some(workdir.display().to_string()),
            ..agent("assistant")
        }))
        .await;
    let ask = [Message::user("深色主题", 1)];
    let find = |sections: &[(String, String)], key: &str| {
        sections
            .iter()
            .find(|(have, _)| have == key)
            .map(|(_, text)| text.clone())
    };

    let first = texts(&hooks.setup(conversation, &ask).await.unwrap().sections);
    assert!(find(&first, "project").is_some_and(|text| text.contains("规范一")));
    let keys: Vec<&str> = first.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, ["base", "project"], "记忆不是系统段");

    // 之后规范文件变了：已展示的那份原样不动。
    std::fs::write(workdir.join("AGENTS.md"), "规范二").unwrap();
    let shown = system_message(
        &first
            .iter()
            .map(|(key, text)| (key.as_str(), text.as_str()))
            .collect::<Vec<_>>(),
    );
    let context = [shown, Message::user("深色主题", 2)];
    let pinned = texts(&hooks.setup(conversation, &context).await.unwrap().sections);
    assert_eq!(pinned, first);

    // 升级前钉着的 `memory` 段不再交回：补丁把它去掉。
    let mut old = first.clone();
    old.push(("memory".into(), "[已知记忆]\n- 旧块".into()));
    let legacy = system_message(
        &old.iter()
            .map(|(key, text)| (key.as_str(), text.as_str()))
            .collect::<Vec<_>>(),
    );
    let context = [legacy, Message::user("深色主题", 3)];
    let upgraded = texts(&hooks.setup(conversation, &context).await.unwrap().sections);
    assert_eq!(upgraded, first, "旧的记忆段不在期望的段里");

    // 下一次 head：上下文里没有已展示的段，规范读新的。
    let after_head = texts(&hooks.setup(conversation, &ask).await.unwrap().sections);
    assert!(find(&after_head, "project").is_some_and(|text| text.contains("规范二")));
}

/// 按钩子交回的 draft 推进的一份上下文（不经 harness：ID 按顺序编）。
struct Transcript {
    conversation: ConversationId,
    entries: Vec<EntryRecord>,
}

impl Transcript {
    fn push(&mut self, draft: EntryDraft) {
        let id = EntryId::new(self.entries.last().map_or(1, |entry| entry.id.get() + 1));
        let mut record = EntryRecord::new(id, self.conversation, draft.kind);
        record.model = draft.model;
        record.data = draft.data;
        record.edits = draft.edits;
        self.entries.push(record);
    }

    fn input(&mut self, text: &str) {
        let mut draft = EntryDraft::new("cli.input");
        draft.model = Some(vec![Message::user(text, 1)]);
        self.push(draft);
    }

    fn view(&self) -> ContextView {
        derive_context(&self.entries, None)
    }

    async fn ask(&self, hooks: &GatewayHooks, run: u64) -> Vec<EntryDraft> {
        let view = self.view();
        hooks
            .context_entries(ContextRequest {
                conversation: self.conversation,
                run: Some(SubmissionId::new(run)),
                entries: &self.entries,
                view: &view,
                now: 1,
            })
            .await
            .unwrap()
    }
}

fn model_text(draft: &EntryDraft) -> String {
    match draft.model.as_deref() {
        Some([Message::User(user)]) => match &user.content {
            Content::Text(text) => text.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("召回块是一条 user 消息：{other:?}"),
    }
}

fn joined(view: &ContextView) -> String {
    format!("{:?}", view.messages)
}

/// 记忆按 Run 召回、作为一条宿主附上的 user 消息追加在输入之后（§9.4）：同一 Run 只召回一次；
/// 下一个 Run 只追加本段没展示过的条目，之前的消息逐字不动；遗忘用 `edits` 改写旧块；
/// 筛空的 Run 只落一条不进上下文的记录。
#[tokio::test]
async fn memory_recall_is_appended_once_per_run_and_forgetting_rewrites_it() {
    let world = World::new(&config_with("", KEYWORD_MEMORY), finisher()).await;
    let state = world.gateway.state();
    state
        .memory
        .put(seeded_memory("m-1", "用户喜欢深色主题"), None)
        .await
        .unwrap();
    let hooks = world.hooks();
    let conversation = world.conversation(Some(agent("assistant"))).await;
    let mut transcript = Transcript {
        conversation,
        entries: Vec::new(),
    };

    // Run 1：召回一块，带宿主标记，挂在 Run 上。
    transcript.input("深色主题");
    let drafts = transcript.ask(&hooks, 1).await;
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    let block = &drafts[0];
    assert_eq!(block.kind, MEMORY_RECALL_ENTRY_KIND);
    assert!(model_text(block).contains("用户喜欢深色主题"));
    assert!(block.model.as_ref().unwrap()[0].is_host_injected());
    assert_eq!(block.data.as_ref().unwrap()["submission"], 1);
    transcript.push(drafts.into_iter().next().unwrap());
    // 同一 Run 的后续轮次（重启后也一样：看的是已提交的 entry）不再召回。
    assert!(transcript.ask(&hooks, 1).await.is_empty());

    // Run 2：多了一条更相关的记忆。只追加没展示过的那条，之前的消息是新上下文的前缀。
    state
        .memory
        .put(seeded_memory("m-2", "用户的深色主题偏好是黑色"), None)
        .await
        .unwrap();
    let before = transcript.view().messages;
    transcript.input("深色主题偏好");
    let drafts = transcript.ask(&hooks, 2).await;
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    let text = model_text(&drafts[0]);
    assert!(text.contains("偏好是黑色"), "{text}");
    assert!(!text.contains("用户喜欢深色主题"), "本段已展示过：{text}");
    transcript.push(drafts.into_iter().next().unwrap());
    let after = transcript.view().messages;
    assert_eq!(after[..before.len()], before[..], "追加不动前缀");
    let first_block = transcript.entries[1].id;

    // 遗忘 m-1：同一 Run 的下一轮追加一条只带 edits 的 write entry，把第一块整块去掉。
    state
        .memories
        .forget(&MemoryId::from_raw("m-1"), 1)
        .await
        .unwrap();
    let drafts = transcript.ask(&hooks, 2).await;
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    let revoke = &drafts[0];
    assert_eq!(revoke.kind, MEMORY_REVOKE_ENTRY_KIND);
    assert!(revoke.model.is_none());
    assert_eq!(revoke.edits, vec![ContextEdit::omit(first_block)]);
    transcript.push(drafts.into_iter().next().unwrap());
    let view = transcript.view();
    assert!(!joined(&view).contains("用户喜欢深色主题"), "{view:?}");
    assert!(joined(&view).contains("偏好是黑色"), "别的块不动");
    // 改写过之后不再重复改写。
    assert!(transcript.ask(&hooks, 2).await.is_empty());

    // Run 3：门槛 / 关键词筛空：只落一条没有 model 的记录，上下文一个字不多。
    let before = transcript.view().messages;
    transcript.input("今天天气怎么样");
    let drafts = transcript.ask(&hooks, 3).await;
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    assert_eq!(drafts[0].kind, MEMORY_RECALL_ENTRY_KIND);
    assert!(drafts[0].model.is_none(), "{drafts:?}");
    transcript.push(drafts.into_iter().next().unwrap());
    assert_eq!(
        transcript.view().messages.len(),
        before.len() + 1,
        "只多了输入"
    );
    assert!(
        transcript.ask(&hooks, 3).await.is_empty(),
        "记录过就不再召回"
    );
}

/// 块里只有一部分条目失效时 `replace` 成去掉那几行的视图，别的行一个字不改。
#[tokio::test]
async fn forgetting_one_of_several_rewrites_the_block_without_that_line() {
    let world = World::new(&config_with("", KEYWORD_MEMORY), finisher()).await;
    let state = world.gateway.state();
    for (id, content) in [("m-1", "深色主题用黑色"), ("m-2", "深色主题字号 14")] {
        state
            .memory
            .put(seeded_memory(id, content), None)
            .await
            .unwrap();
    }
    let hooks = world.hooks();
    let conversation = world.conversation(Some(agent("assistant"))).await;
    let mut transcript = Transcript {
        conversation,
        entries: Vec::new(),
    };
    transcript.input("深色主题");
    let drafts = transcript.ask(&hooks, 1).await;
    let text = model_text(&drafts[0]);
    assert!(
        text.contains("用黑色") && text.contains("字号 14"),
        "{text}"
    );
    transcript.push(drafts.into_iter().next().unwrap());

    state
        .memories
        .forget(&MemoryId::from_raw("m-2"), 1)
        .await
        .unwrap();
    let drafts = transcript.ask(&hooks, 1).await;
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    let [edit] = drafts[0].edits.as_slice() else {
        panic!("{drafts:?}")
    };
    let EditAction::Replace { messages } = &edit.action else {
        panic!("{edit:?}")
    };
    let Message::User(user) = &messages[0] else {
        panic!("{messages:?}")
    };
    let Content::Text(left) = &user.content else {
        panic!()
    };
    assert!(
        left.contains("用黑色") && !left.contains("字号 14"),
        "{left}"
    );
    assert_eq!(user.host_origin(), Some("memory"), "改写后仍是宿主附上的");
    assert_eq!(
        left.as_str(),
        komo_agent::context::memory::without(
            &text,
            &[MemoryUse {
                memory: MemoryId::from_raw("m-2"),
                revision: 1,
            }]
        )
        .unwrap()
    );
}

const GATED_MEMORY: &str =
    "[memory]\nenabled = true\n\n[memory.retrieval]\nmode = \"keyword\"\ntop_k = 1\n";

fn seeded_convention(id: &str, content: &str, when: &str) -> MemoryItem {
    let mut item = seeded_memory(id, content);
    item.application = Some(komo_kernel::types::memory::MemoryApplication::Convention);
    item.applies_when = Some(when.into());
    item
}

/// 约定不过门槛、不占 `top_k`（§9.4「约定不过门槛」）：用一句和约定一个字都不重合的话要求提单，
/// 召回块里照样有约定；同时匹配这句话的线索仍然受 `top_k = 1` 约束，约定没有挤掉它。
#[tokio::test]
async fn a_convention_is_injected_for_a_paraphrase_and_takes_no_top_k_slot() {
    let world = World::new(&config_with("", GATED_MEMORY), finisher()).await;
    let state = world.gateway.state();
    for item in [
        seeded_convention("m-1", "发布工单 Platform 默认选 saas", "创建发布工单时"),
        seeded_memory("m-2", "深色主题字号 14"),
        seeded_memory("m-3", "深色主题用黑色"),
    ] {
        state.memory.put(item, None).await.unwrap();
    }
    let hooks = world.hooks();
    let conversation = world.conversation(Some(agent("assistant"))).await;
    let mut transcript = Transcript {
        conversation,
        entries: Vec::new(),
    };

    transcript.input("帮我上线一下 xx");
    let drafts = transcript.ask(&hooks, 1).await;
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    let text = model_text(&drafts[0]);
    assert!(text.contains("适用的长期约定"), "{text}");
    assert!(text.contains("Platform 默认选 saas"), "{text}");
    assert!(!text.contains("历史线索"), "没有线索命中：{text}");
    transcript.push(drafts.into_iter().next().unwrap());

    // 同一段里的第二个 Run 不重复带；门槛路上的线索仍只给 top_k 条。
    transcript.input("深色主题");
    let drafts = transcript.ask(&hooks, 2).await;
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    let text = model_text(&drafts[0]);
    assert!(!text.contains("saas"), "本段已展示过：{text}");
    assert_eq!(
        text.matches("深色主题").count(),
        1,
        "线索按 top_k = 1：{text}"
    );
}

/// 约定与它同时命中的线索各走各的路：约定组不占 `top_k` 名额，查询刚好命中约定时线索也还在。
#[tokio::test]
async fn a_matching_convention_does_not_displace_a_clue_from_top_k() {
    let world = World::new(&config_with("", GATED_MEMORY), finisher()).await;
    let state = world.gateway.state();
    for item in [
        seeded_convention("m-1", "深色主题优先用黑色", "改界面配色时"),
        seeded_memory("m-2", "深色主题字号 14"),
    ] {
        state.memory.put(item, None).await.unwrap();
    }
    let hooks = world.hooks();
    let conversation = world.conversation(Some(agent("assistant"))).await;
    let mut transcript = Transcript {
        conversation,
        entries: Vec::new(),
    };
    transcript.input("深色主题");
    let drafts = transcript.ask(&hooks, 1).await;
    let text = model_text(&drafts[0]);
    assert!(text.contains("优先用黑色"), "{text}");
    assert!(text.contains("字号 14"), "约定没有占掉线索的名额：{text}");
}

/// 新增的约定只在下一个 Run 补上；遗忘它之后旧块被改写；压缩开新段之后在新段第一个 Run 重新带上。
#[tokio::test]
async fn a_new_convention_is_added_a_forgotten_one_is_rewritten_and_a_new_segment_brings_it_again()
{
    let world = World::new(&config_with("", KEYWORD_MEMORY), finisher()).await;
    let state = world.gateway.state();
    state
        .memory
        .put(
            seeded_convention("m-1", "发布工单 Platform 默认选 saas", "创建发布工单时"),
            None,
        )
        .await
        .unwrap();
    let hooks = world.hooks();
    let conversation = world.conversation(Some(agent("assistant"))).await;
    let mut transcript = Transcript {
        conversation,
        entries: Vec::new(),
    };
    transcript.input("你好");
    let drafts = transcript.ask(&hooks, 1).await;
    assert!(model_text(&drafts[0]).contains("Platform 默认选 saas"));
    transcript.push(drafts.into_iter().next().unwrap());
    let first_block = transcript.entries[1].id;

    // 之后 `memory add` 的约定在下一个 Run 补上，旧的那条不重复。
    state
        .memory
        .put(
            seeded_convention("m-2", "查日志默认用 sls", "查日志时"),
            None,
        )
        .await
        .unwrap();
    transcript.input("你好");
    let drafts = transcript.ask(&hooks, 2).await;
    let text = model_text(&drafts[0]);
    assert!(
        text.contains("查日志默认用 sls") && !text.contains("saas"),
        "{text}"
    );
    transcript.push(drafts.into_iter().next().unwrap());

    // 遗忘 m-1：第一块整块去掉（只剩它一条），第二块不动。
    state
        .memories
        .forget(&MemoryId::from_raw("m-1"), 1)
        .await
        .unwrap();
    let drafts = transcript.ask(&hooks, 2).await;
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    assert_eq!(drafts[0].kind, MEMORY_REVOKE_ENTRY_KIND);
    assert_eq!(drafts[0].edits, vec![ContextEdit::omit(first_block)]);
    transcript.push(drafts.into_iter().next().unwrap());
    assert!(!joined(&transcript.view()).contains("saas"));

    // 新段：这一条 entry 把上下文的起点挪到自己，旧块不在视图里，约定重新带一次（只剩没被遗忘的）。
    transcript.input("接着聊");
    let mut head = transcript.entries.pop().unwrap();
    head.head = Some(head.id);
    transcript.entries.push(head);
    let drafts = transcript.ask(&hooks, 3).await;
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    let text = model_text(&drafts[0]);
    assert!(text.contains("查日志默认用 sls"), "新段重新带上：{text}");
    assert!(!text.contains("saas"), "被遗忘的不回来：{text}");
}

/// 预算放不下的约定从后（创建较晚的）往前截；截掉的回到门槛那条路，命中了仍渲染在约定组里，
/// 没命中就不带，并在召回记录里留下"约定超出预算"。
#[tokio::test]
async fn conventions_over_budget_fall_back_to_the_gated_path() {
    let config = "[memory]\nenabled = true\n\n[memory.retrieval]\nmode = \"keyword\"\nconvention_tokens = 170\n";
    let world = World::new(&config_with("", config), finisher()).await;
    let state = world.gateway.state();
    for item in [
        seeded_convention("m-1", "发布工单 Platform 默认选 saas", "创建发布工单时"),
        seeded_convention("m-2", "查日志默认用 sls", "查日志时"),
    ] {
        state.memory.put(item, None).await.unwrap();
    }
    let hooks = world.hooks();
    let conversation = world.conversation(Some(agent("assistant"))).await;

    // 没命中：m-2 被截掉，也不带。
    let mut transcript = Transcript {
        conversation,
        entries: Vec::new(),
    };
    transcript.input("你好");
    let drafts = transcript.ask(&hooks, 1).await;
    let text = model_text(&drafts[0]);
    assert!(
        text.contains("Platform 默认选 saas") && !text.contains("sls"),
        "{text}"
    );
    assert_eq!(
        drafts[0].data.as_ref().unwrap()["recall"]["conventions_over_budget"],
        1
    );

    // 命中：m-2 走门槛那条路，仍在约定组里。
    let mut transcript = Transcript {
        conversation,
        entries: Vec::new(),
    };
    transcript.input("查日志");
    let drafts = transcript.ask(&hooks, 1).await;
    let text = model_text(&drafts[0]);
    assert!(
        text.contains("Platform 默认选 saas") && text.contains("查日志默认用 sls"),
        "{text}"
    );
    let conventions = text.find("适用的长期约定").unwrap();
    assert!(
        !text.contains("历史线索") && conventions < text.find("sls").unwrap(),
        "{text}"
    );
}

/// `private` 只带本 Agent 自己作用域的约定；个人作用域的约定它看不见。
#[tokio::test]
async fn a_private_agent_gets_only_its_own_scopes_conventions() {
    let world = World::new(
        &config_with("memory = \"private\"", KEYWORD_MEMORY),
        finisher(),
    )
    .await;
    let state = world.gateway.state();
    let mut own = seeded_convention("m-own", "这个助手的口令是蓝色", "任何时候");
    own.scope = MemoryScope::Agent {
        agent_id: "assistant".into(),
    };
    let mut other = seeded_convention("m-other", "别的助手的口令是红色", "任何时候");
    other.scope = MemoryScope::Agent {
        agent_id: "helper".into(),
    };
    for item in [
        own,
        other,
        seeded_convention("m-personal", "个人的口令是绿色", "任何时候"),
    ] {
        state.memory.put(item, None).await.unwrap();
    }
    let hooks = world.hooks();
    let conversation = world.conversation(Some(agent("assistant"))).await;
    let mut transcript = Transcript {
        conversation,
        entries: Vec::new(),
    };
    transcript.input("你好");
    let drafts = transcript.ask(&hooks, 1).await;
    let text = model_text(&drafts[0]);
    assert!(text.contains("蓝色"), "{text}");
    assert!(!text.contains("红色") && !text.contains("绿色"), "{text}");
}

/// `memory = off`：一条约定也不带。
#[tokio::test]
async fn memory_off_brings_no_conventions() {
    let world = World::new(&config_with("memory = \"off\"", KEYWORD_MEMORY), finisher()).await;
    let state = world.gateway.state();
    state
        .memory
        .put(
            seeded_convention("m-1", "发布工单 Platform 默认选 saas", "创建发布工单时"),
            None,
        )
        .await
        .unwrap();
    let hooks = world.hooks();
    let conversation = world.conversation(Some(agent("assistant"))).await;
    let mut transcript = Transcript {
        conversation,
        entries: Vec::new(),
    };
    transcript.input("你好");
    assert!(transcript.ask(&hooks, 1).await.is_empty());
}

// ---------------------------------------------------------------- 事项组（`docs/goals.md` §5）

async fn add_goal(state: &crate::service::state::GatewayState, title: &str, next: &str) -> Matter {
    state
        .matters
        .add(
            &MatterAddRequest {
                kind: MatterKind::Goal,
                title: title.into(),
                next: Some(next.into()),
                due: None,
                standard: None,
                session: None,
                run: None,
                request_key: None,
            },
            &Caller::Operator,
        )
        .await
        .unwrap()
}

async fn act(
    state: &crate::service::state::GatewayState,
    matter: &Matter,
    verb: MatterVerb,
    request: MatterActionRequest,
) -> Matter {
    state
        .matters
        .apply(&matter.id, verb, &request, &Caller::Operator)
        .await
        .unwrap()
}

async fn fresh_transcript(world: &World) -> Transcript {
    Transcript {
        conversation: world.conversation(Some(agent("assistant"))).await,
        entries: Vec::new(),
    }
}

/// 进行中的目标第一个 Run 就带上（不过门槛，用户这句话与它一个字都不沾），同一段里第二个 Run
/// 不重复：只落一条没有 `model` 的记录。
#[tokio::test]
async fn a_matter_shows_on_the_first_run_and_is_not_repeated_in_the_segment() {
    let world = World::new(&config_with("", KEYWORD_MEMORY), finisher()).await;
    let state = world.gateway.state();
    let goal = add_goal(state, "把 owl 发布观察做完", "看 AB 组的 5xx").await;
    let hooks = world.hooks();
    let mut transcript = fresh_transcript(&world).await;

    transcript.input("你好");
    let drafts = transcript.ask(&hooks, 1).await;
    let text = model_text(&drafts[0]);
    assert!(text.contains("你在做和在管的事"), "{text}");
    assert!(
        text.contains(&format!("下一步：看 AB 组的 5xx · {}@1〕", goal.id)),
        "{text}"
    );
    assert_eq!(
        drafts[0].data.as_ref().unwrap()["matters"][0]["revision"],
        1
    );
    transcript.push(drafts.into_iter().next().unwrap());

    transcript.input("再聊一句");
    let drafts = transcript.ask(&hooks, 2).await;
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    assert!(drafts[0].model.is_none(), "{drafts:?}");
}

/// 搁置后旧块被改写（只剩它一条就整块 `omit`）；改了下一步则旧版本失效、新版本在下一个 Run 补上。
#[tokio::test]
async fn parking_rewrites_the_old_block_and_a_new_revision_arrives_on_the_next_run() {
    let world = World::new(&config_with("", KEYWORD_MEMORY), finisher()).await;
    let state = world.gateway.state();
    let first = add_goal(state, "把 owl 发布观察做完", "看 AB 组的 5xx").await;
    let second = add_goal(state, "整理周报模板", "先列大纲").await;
    let hooks = world.hooks();
    let mut transcript = fresh_transcript(&world).await;
    transcript.input("你好");
    let drafts = transcript.ask(&hooks, 1).await;
    transcript.push(drafts.into_iter().next().unwrap());
    let block = transcript.entries[1].id;

    // 搁置第二个、改第一个的下一步：旧块里两行都失效，整块去掉。
    act(state, &second, MatterVerb::Park, Default::default()).await;
    let moved = act(
        state,
        &first,
        MatterVerb::Next,
        MatterActionRequest {
            next: Some("对一下 CI 的耗时".into()),
            ..Default::default()
        },
    )
    .await;
    transcript.input("接着说");
    let drafts = transcript.ask(&hooks, 2).await;
    assert_eq!(drafts.len(), 2, "{drafts:?}");
    assert_eq!(drafts[0].kind, MEMORY_REVOKE_ENTRY_KIND);
    assert_eq!(drafts[0].edits, vec![ContextEdit::omit(block)]);
    let text = model_text(&drafts[1]);
    assert!(
        text.contains("对一下 CI 的耗时")
            && text.contains(&format!("{}@{}〕", moved.id, moved.revision)),
        "{text}"
    );
    assert!(!text.contains("整理周报模板"), "搁置的不带：{text}");
    transcript.push(drafts.into_iter().next().unwrap());
    assert!(!joined(&transcript.view()).contains("看 AB 组的 5xx"));
}

/// 只失效一条时旧块改写成少一行的视图，其余事项行不动；结案的事项同样掉出去。
#[tokio::test]
async fn closing_one_of_several_matters_rewrites_the_block_without_that_line() {
    let world = World::new(&config_with("", KEYWORD_MEMORY), finisher()).await;
    let state = world.gateway.state();
    let first = add_goal(state, "把 owl 发布观察做完", "看 AB 组的 5xx").await;
    add_goal(state, "整理周报模板", "先列大纲").await;
    let hooks = world.hooks();
    let mut transcript = fresh_transcript(&world).await;
    transcript.input("你好");
    let drafts = transcript.ask(&hooks, 1).await;
    transcript.push(drafts.into_iter().next().unwrap());

    act(state, &first, MatterVerb::Done, Default::default()).await;
    let drafts = transcript.ask(&hooks, 1).await;
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    assert_eq!(drafts[0].kind, MEMORY_REVOKE_ENTRY_KIND);
    transcript.push(drafts.into_iter().next().unwrap());
    let view = joined(&transcript.view());
    assert!(
        !view.contains("owl 发布观察") && view.contains("整理周报模板"),
        "{view}"
    );
}

/// 等待中的目标带"在等"；领域带"标准"；进行中的领域也带。
#[tokio::test]
async fn waiting_goals_and_areas_are_injected_with_their_own_fields() {
    let world = World::new(&config_with("", KEYWORD_MEMORY), finisher()).await;
    let state = world.gateway.state();
    let goal = add_goal(state, "owl 发布工单", "催审批").await;
    act(
        state,
        &goal,
        MatterVerb::Wait,
        MatterActionRequest {
            waiting_for: Some("SRE 审批".into()),
            ..Default::default()
        },
    )
    .await;
    state
        .matters
        .add(
            &MatterAddRequest {
                kind: MatterKind::Area,
                title: "告警值班轮换".into(),
                next: None,
                due: None,
                standard: Some("每周一前完成".into()),
                session: None,
                run: None,
                request_key: None,
            },
            &Caller::Operator,
        )
        .await
        .unwrap();
    let hooks = world.hooks();
    let mut transcript = fresh_transcript(&world).await;
    transcript.input("你好");
    let text = model_text(&transcript.ask(&hooks, 1).await[0]);
    assert!(
        text.contains("目标（等待中）：owl 发布工单〔在等：SRE 审批"),
        "{text}"
    );
    assert!(
        text.contains("领域：告警值班轮换〔标准：每周一前完成"),
        "{text}"
    );
}

/// `memory = private` 与 `off` 的会话不带事项；共享的会话带。
#[tokio::test]
async fn private_and_off_sessions_get_no_matters() {
    for mode in ["private", "off"] {
        let world = World::new(
            &config_with(&format!("memory = \"{mode}\""), KEYWORD_MEMORY),
            finisher(),
        )
        .await;
        add_goal(world.gateway.state(), "把 owl 发布观察做完", "看 5xx").await;
        let hooks = world.hooks();
        let mut transcript = fresh_transcript(&world).await;
        transcript.input("你好");
        let drafts = transcript.ask(&hooks, 1).await;
        assert!(
            drafts.iter().all(|draft| draft.model.is_none()),
            "{mode}：{drafts:?}"
        );
    }
}

/// 超出 `matter_tokens` 的按更新时间从旧到新截掉，数量记在召回记录里；截掉的不回落到检索。
#[tokio::test]
async fn matters_over_budget_are_cut_and_counted() {
    // 抬头约 47 个字符，一行（带 36 位的 id）约 50：120 只放得下一条。
    let config =
        "[memory]\nenabled = true\n\n[memory.retrieval]\nmode = \"keyword\"\nmatter_tokens = 120\n";
    let world = World::new(&config_with("", config), finisher()).await;
    let state = world.gateway.state();
    add_goal(state, "目标甲", "一").await;
    add_goal(state, "目标乙", "二").await;
    let hooks = world.hooks();
    let mut transcript = fresh_transcript(&world).await;
    transcript.input("你好");
    let drafts = transcript.ask(&hooks, 1).await;
    let text = model_text(&drafts[0]);
    assert_eq!(text.matches("- 目标：").count(), 1, "{text}");
    assert_eq!(
        drafts[0].data.as_ref().unwrap()["recall"]["matters_over_budget"],
        1
    );
}

/// skills 目录行同样钉住（W4b）：展示过之后注册表变了（多了 skill、重载），已展示的那份原样不动，
/// 直到下一次 head；"没有"也钉住。
#[tokio::test]
async fn skills_section_is_pinned_until_the_next_head() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    let write_skill = |name: &str, description: &str| {
        let one = skills.join(name);
        std::fs::create_dir_all(&one).unwrap();
        std::fs::write(
            one.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n正文\n"),
        )
        .unwrap();
    };
    write_skill("pr-review", "审 PR");
    let config = config_with("", NO_MEMORY)
        + &format!("\n[paths]\nskill_dirs = [\"{}\"]\n", skills.display());
    let world = World::new(&config, finisher()).await;
    let state = world.gateway.state();
    let hooks = world.hooks();
    let conversation = world.conversation(Some(agent("assistant"))).await;
    let ask = [Message::user("在吗", 1)];
    let find = |sections: &[(String, String)], key: &str| {
        sections
            .iter()
            .find(|(have, _)| have == key)
            .map(|(_, text)| text.clone())
    };
    let shown_of = |sections: &[(String, String)]| {
        system_message(
            &sections
                .iter()
                .map(|(key, text)| (key.as_str(), text.as_str()))
                .collect::<Vec<_>>(),
        )
    };

    let first = texts(&hooks.setup(conversation, &ask).await.unwrap().sections);
    assert!(find(&first, "skills").is_some_and(|text| text.contains("pr-review")));

    // 注册表里多了一个 skill、重载之后：已展示的原样不动；下一次 head 才带上它。
    write_skill("log-diagnosis", "查日志");
    crate::reload::reload(state).await.expect("新配置装得上");
    let context = [shown_of(&first), Message::user("在吗", 2)];
    let pinned = texts(&hooks.setup(conversation, &context).await.unwrap().sections);
    assert_eq!(find(&pinned, "skills"), find(&first, "skills"));
    assert!(!find(&pinned, "skills").unwrap().contains("log-diagnosis"));
    let after_head = texts(&hooks.setup(conversation, &ask).await.unwrap().sections);
    assert!(
        find(&after_head, "skills").is_some_and(|text| text.contains("log-diagnosis")),
        "{after_head:?}"
    );

    // "没有"也钉住：展示过的基线里没有 skills 段，之后再多出 skill 也不补。
    let bare: Vec<(String, String)> = first
        .iter()
        .filter(|(key, _)| key != "skills")
        .cloned()
        .collect();
    let context = [shown_of(&bare), Message::user("在吗", 3)];
    let still_bare = texts(&hooks.setup(conversation, &context).await.unwrap().sections);
    assert_eq!(find(&still_bare, "skills"), None, "{still_bare:?}");
}

/// 同一个 profile 下，段按 instructions → base → project → skills 的顺序连成系统提示（`komo-agent`
/// 里有纯函数的组合测试）。
#[tokio::test]
async fn the_system_sections_follow_the_prompt_order() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = komo_runtime::tools::paths::real_root(dir.path());
    std::fs::write(workdir.join("AGENTS.md"), "项目规范\n").unwrap();
    let skills = dir.path().join("skills/demo");
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(
        skills.join("SKILL.md"),
        "---\nname: demo\ndescription: 演示技能\n---\n正文\n",
    )
    .unwrap();
    let config = config_with(
        &format!(
            "instructions = \"  你是 coder。\\n\"\nworkspace = \"{}\"",
            workdir.display()
        ),
        NO_MEMORY,
    ) + &format!(
        "\n[paths]\nskill_dirs = [\"{}\"]\n",
        dir.path().join("skills").display()
    );
    let world = World::new(&config, finisher()).await;
    let conversation = world.conversation(Some(agent("assistant"))).await;
    let setup = world.hooks().setup(conversation, &[]).await.unwrap();

    let joined = setup
        .sections
        .iter()
        .map(|section| section.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    assert!(joined.starts_with("你是 coder。\n\n你是 komo"), "{joined}");
    assert!(joined.contains("项目规范"), "{joined}");
    assert!(joined.contains("演示技能"), "{joined}");
    let keys: Vec<&str> = setup.sections.iter().map(|s| s.key.as_str()).collect();
    assert_eq!(keys, ["instructions", "base", "project", "skills"]);
    assert!(joined.contains("演示技能"), "skills 目录行在里面");
}

// ---------------------------------------------------------------- MCP

/// 目录里装着、之后被撤掉的工具：名字还在能力面里，模型再调时 `prepare` 报"为什么没了"；
/// 它不进给模型的声明。
#[tokio::test]
async fn a_withdrawn_mcp_tool_fails_with_its_reason() {
    struct Echo;
    #[async_trait]
    impl Tool for Echo {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "mcp__demo__echo".into(),
                description: "回显".into(),
                parameters: serde_json::json!({ "type": "object" }),
                doc: None,
            }
        }
        async fn prepare(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ExecutionPlan, ToolError> {
            unreachable!("撤掉之后不会走到真工具")
        }
        async fn execute(
            &self,
            _plan: ExecutionPlan,
            _ctx: &ToolContext,
            _sink: &mut dyn OutputWriter,
        ) -> Result<ToolOutput, ToolError> {
            unreachable!()
        }
    }
    let world = World::new(&config_with("", NO_MEMORY), finisher()).await;
    let executor = &world.gateway.state().tool_executor;
    executor.install(vec![Arc::new(Echo)]);
    let hooks = world.hooks();
    let conversation = world.conversation(None).await;

    let before = hooks.tools(conversation).await.unwrap();
    assert!(names(before.definitions()).contains(&"mcp__demo__echo".to_owned()));

    executor.withdraw(vec![(
        "mcp__demo__echo".into(),
        "MCP 服务器 demo 已从配置移除".into(),
    )]);
    let after = hooks.tools(conversation).await.unwrap();
    assert!(
        !names(after.definitions()).contains(&"mcp__demo__echo".to_owned()),
        "撤掉的不再声明给模型"
    );
    let stub = after.get("mcp__demo__echo").expect("名字还在能力面里");
    let ctx = tool_context(&after);
    let error = stub
        .prepare(serde_json::json!({}), &ctx)
        .await
        .expect_err("桩只会报原因");
    assert!(error.to_string().contains("已从配置移除"), "{error}");
}

fn tool_context(set: &komo_runtime::durable::harness::ToolSet) -> ToolContext {
    use komo_kernel::types::ids::{AttemptId, RunId, ToolCallId};
    use komo_kernel::types::tool::{CancelToken, InterjectSignal};
    ToolContext {
        session: set.env.session.clone(),
        run: RunId::from_raw("r"),
        input_run: Some(RunId::from_raw("r")),
        call: ToolCallId::from_raw("c"),
        attempt: AttemptId::from_raw("1"),
        source: set.env.source.clone(),
        cwd: set.env.cwd.clone(),
        roots: set.env.roots.clone(),
        mounts: set.env.mounts.clone(),
        resumed: None,
        cancel: CancelToken::new(),
        call_timeout: set.env.call_timeout,
        detach_after: None,
        interjected: InterjectSignal::new(),
        conversation: true,
    }
}

/// 能力面里点名的 MCP 工具、目录里还没有：钩子有超时地等那台服务器；等到它起不来，工具以桩
/// 的形态留在能力面里、`prepare` 报连接失败的原因（而不是被当作"未知名字"悄悄丢掉）。
#[tokio::test]
async fn mcp_tools_in_surface_are_awaited() {
    let config = config_with("tools = [\"read\", \"mcp__broken__lookup\"]", NO_MEMORY)
        + "\n[mcp.servers.broken]\ncommand = \"/nonexistent/mcp-server\"\n";
    let world = World::new(&config, finisher()).await;
    let hooks = world.hooks();
    let conversation = world.conversation(None).await;

    let set = hooks.tools(conversation).await.unwrap();
    assert!(
        set.surface().allows("mcp__broken__lookup"),
        "点名的名字没被丢掉"
    );
    assert_eq!(names(set.definitions()), ["read"], "桩不进声明");
    let stub = set.get("mcp__broken__lookup").expect("桩在");
    let error = stub
        .prepare(serde_json::json!({}), &tool_context(&set))
        .await
        .expect_err("桩只会报原因");
    assert!(
        error.to_string().contains("broken") || error.to_string().contains("连"),
        "原因里说得清是哪台服务器、为什么：{error}"
    );
}

// ---------------------------------------------------------------- 压缩与 effort

#[tokio::test]
async fn compaction_setup_maps_config_and_komo_agent_prompt() {
    let config = config_with("", NO_MEMORY)
        + "\n[compaction]\nenabled = true\nkeep_recent_tokens = 5000\nmax_context_tokens = 90000\nbackground_tokens = 20000\nmemo_token_estimate = 700\n";
    let world = World::new(&config, finisher()).await;
    let hooks = world.hooks();
    let conversation = world
        .conversation(Some(AgentDoc {
            model: Some("fast".into()),
            ..agent("assistant")
        }))
        .await;

    let setup = hooks.compaction(conversation).await.unwrap();
    assert!(setup.enabled);
    assert_eq!(setup.settings.keep_recent_tokens, 5000);
    assert_eq!(setup.settings.max_context_tokens, 90_000);
    assert_eq!(setup.settings.memo_tokens, 700);
    assert_eq!(setup.settings.background_tokens, 20_000, "后台阈值来自新键");
    assert_eq!(setup.model.model, "fast-test", "摘要用这一轮的模型");
    assert_eq!(
        setup.prompt.system,
        komo_agent::context::compaction::SUMMARY_INSTRUCTIONS
    );
    assert_eq!(
        setup.cache_key.as_deref(),
        Some(&*format!("c{conversation}"))
    );

    // 热重载：关掉。
    world.write_config_and_reload(&config_with("", NO_MEMORY));
    let off = hooks.compaction(conversation).await.unwrap();
    assert!(!off.enabled);
    assert_eq!(off.settings.background_tokens, 0);
}

/// 只给 effort 时按 Agent 的默认模型校验档位；给了 model 就按它；接着做的任务按它上一次的模型。
#[tokio::test]
async fn check_effort_uses_agent_default_model() {
    // `main` 声明只支持 low / high；`fast` 只支持 medium。
    let config = config_with("model = \"fast\"", NO_MEMORY)
        .replace(
            "model = \"gpt-test\"",
            "model = \"gpt-test\"\nefforts = [\"low\", \"high\"]",
        )
        .replace(
            "model = \"fast-test\"",
            "model = \"fast-test\"\nefforts = [\"medium\"]",
        );
    let world = World::new(&config, finisher()).await;
    let orchestration = GatewayOrchestration::new(Arc::clone(&world.slot), None);
    let conversation = world.conversation(Some(agent("assistant"))).await;

    // Agent 的默认模型是 `fast`（Profile 的 alias）：只支持 medium。
    assert!(
        orchestration
            .check_effort(conversation, None, &Effort::new("medium"))
            .await
            .is_ok()
    );
    let problem = orchestration
        .check_effort(conversation, None, &Effort::new("high"))
        .await
        .expect_err("fast 不支持 high");
    assert!(problem.contains("high"), "{problem}");
    // 显式给了 model 就按它的档位。
    assert!(
        orchestration
            .check_effort(conversation, Some("main"), &Effort::new("high"))
            .await
            .is_ok()
    );
    let missing = orchestration
        .check_effort(conversation, Some("nope"), &Effort::new("low"))
        .await
        .expect_err("alias 不存在");
    assert!(missing.contains("nope"), "{missing}");
    // 接着做的任务：按它上一次的模型（komo.task 里记的 `main`）。
    let task = world
        .task_conversation(conversation, None, |mut doc| {
            doc.model = Some("main".into());
            doc
        })
        .await;
    assert!(
        orchestration
            .check_effort(task, None, &Effort::new("high"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn the_contract_block_is_komo_agents_and_monitor_fails_cleanly_when_unassembled() {
    let world = World::new(&config_with("", NO_MEMORY), finisher()).await;
    let orchestration = GatewayOrchestration::new(Arc::clone(&world.slot), None);
    let contract = DelegateContract {
        schema: serde_json::json!({ "type": "object", "required": ["ok"] }),
        mode: Default::default(),
    };
    assert_eq!(
        orchestration.contract_block(&contract),
        komo_agent::context::task_contract_block(&contract)
    );

    let launch = komo_runtime::durable::harness::MonitorLaunch {
        record: komo_runtime::durable::harness::MonitorRecord {
            id: 1,
            conversation: ConversationId::new(1),
            title: "t".into(),
            command: "true".into(),
            cwd: "/".into(),
            timeout_minutes: 1,
            resume_after_restart: false,
            started_at: 0,
            deadline: 60_000,
            state: komo_runtime::durable::harness::MonitorState::Starting,
        },
        mode: komo_runtime::durable::harness::LaunchMode::Fresh,
        remaining_ms: 60_000,
    };
    let error = orchestration.start_monitor(&launch).await.unwrap_err();
    assert!(error.to_string().contains("没有装配监听"), "{error}");
    assert!(orchestration.stop_monitor(1).await.is_err());

    // 装配了就委托。
    struct Recorder(std::sync::Mutex<Vec<String>>);
    #[async_trait]
    impl MonitorRunner for Recorder {
        async fn start(
            &self,
            launch: &komo_runtime::durable::harness::MonitorLaunch,
        ) -> Result<(), komo_runtime::durable::harness::MonitorError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("start {}", launch.record.id));
            Ok(())
        }
        async fn stop(&self, id: u64) -> Result<(), komo_runtime::durable::harness::MonitorError> {
            self.0.lock().unwrap().push(format!("stop {id}"));
            Ok(())
        }
    }
    let recorder = Arc::new(Recorder(Default::default()));
    let wired = GatewayOrchestration::new(
        Arc::clone(&world.slot),
        Some(Arc::clone(&recorder) as Arc<dyn MonitorRunner>),
    );
    wired.start_monitor(&launch).await.unwrap();
    wired.stop_monitor(1).await.unwrap();
    assert_eq!(*recorder.0.lock().unwrap(), ["start 1", "stop 1"]);
}

// ---------------------------------------------------------------- 端到端

/// 钩子在真 harness 上跑通：经 `DurableCore.harness.submit` 往写好 `komo.agent` 的 conversation 提交
/// 输入，FakeLlm 先回一轮工具调用再回答；断言发给模型的系统段、工具表与模型都是钩子给的，
/// 工具调用真的在 Agent 的工作目录里执行。
#[tokio::test]
async fn a_durable_turn_runs_on_the_hooks_prompt_tools_and_model() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = komo_runtime::tools::paths::real_root(dir.path());
    std::fs::write(workdir.join("notes.txt"), "钩子端到端").unwrap();
    std::fs::write(workdir.join("AGENTS.md"), "先读再答").unwrap();
    let llm = FakeLlm::new(vec![vec![
        call_round(
            1,
            "call-1",
            "read",
            serde_json::json!({ "path": workdir.join("notes.txt").display().to_string() }),
        ),
        text_round(2, "读到了"),
    ]]);
    let world = World::new(
        &config_with(
            "instructions = \"你是端到端测试助手。\"\ntools = [\"read\", \"rg\"]",
            NO_MEMORY,
        ),
        Arc::clone(&llm),
    )
    .await;
    let conversation = world
        .conversation(Some(AgentDoc {
            model: Some("fast".into()),
            workdir: Some(workdir.display().to_string()),
            ..agent("assistant")
        }))
        .await;

    let core = world.core();
    let submission = core
        .harness
        .submit(
            &core.engine,
            conversation,
            SubmissionDraft::input("读一下 notes.txt"),
        )
        .await
        .expect("受理");
    let record = core.wait_submission(submission).await.expect("结算");
    assert_eq!(
        record.status(),
        komo_kernel::durable::SubmissionStatus::Done,
        "{record:?}"
    );

    assert_eq!(llm.turns(), 1);
    assert_eq!(llm.models(), ["fast-test"], "komo.agent 里的模型 alias");
    {
        let requests = llm.requests.lock().unwrap();
        let request = &requests[0];
        assert!(
            request
                .system_prompt
                .starts_with("你是端到端测试助手。\n\n你是 komo"),
            "{}",
            request.system_prompt
        );
        assert!(
            request
                .system_prompt
                .contains(&workdir.display().to_string()),
            "工作目录进了 base 段"
        );
        assert!(
            request.system_prompt.contains("先读再答"),
            "项目规范进了 project 段"
        );
        let declared: Vec<&str> = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(declared, ["read", "rg"], "工具表就是 Agent 的能力面");
        let inputs = llm.inputs.lock().unwrap();
        let komo_kernel::types::turn::RoundInput::ToolResults { results, .. } = &inputs[1] else {
            panic!("{:?}", inputs[1]);
        };
        assert!(results[0].content.contains("钩子端到端"), "{results:?}");
        assert!(!results[0].is_error);
    }

    // 同一个 conversation 的下一条输入：系统段没变，不再写 `komo.system`（前缀不动）。
    let second = core
        .harness
        .submit(
            &core.engine,
            conversation,
            SubmissionDraft::input("再说一句"),
        )
        .await
        .unwrap();
    core.wait_submission(second).await.unwrap();
    let requests = llm.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].system_prompt, requests[0].system_prompt);
    assert_eq!(requests[1].tools, requests[0].tools);
}
