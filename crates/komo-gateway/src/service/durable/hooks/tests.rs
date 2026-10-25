//! 钩子的验收（`docs/storage-durable-d5.md` §4 W1b）。
//!
//! 直接对一台 Durable Gateway 的 `GatewayState` 问钩子（它们的输入只有已提交的 document 与当前
//! 配置），再加一个端到端用例：经 `DurableCore.harness.submit` 跑一整轮，看真 harness 上钩子给的
//! 系统段、工具表与模型。

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use komo_agent::context::{ContextInput, InvocationContext, assemble};
use komo_kernel::durable::{
    Content, ConversationId, EntryId, Message, Sections, SystemMessage, TaskId,
};
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::delegate::DelegateContract;
use komo_kernel::types::ids::MemoryId;
use komo_kernel::types::memory::{
    Confirmation, ExtractionMetadata, MemoryItem, MemoryKind, MemoryScope, MemoryState,
    MemoryUsage, Provenance,
};
use komo_kernel::types::model::Effort;
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use komo_runtime::durable::ConversationOwnership;
use komo_runtime::durable::harness::orchestration::store_task_doc;
use komo_runtime::durable::harness::{
    AgentHooks, HookError, OrchestrationHooks, SubmissionDraft, TaskDoc,
};

use super::{GatewayHooks, memory_address_for_tests};
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
    }
}

const KEYWORD_MEMORY: &str = "[memory]\nenabled = true\n\n[memory.retrieval]\nmode = \"keyword\"\n";

/// 召回的记忆与项目规范一旦展示就钉住，直到下一次 head（上下文里没有已展示的段）才换；
/// 被遗忘的记忆立刻作废那一块。
#[tokio::test]
async fn memory_section_is_pinned_until_the_next_head() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = komo_runtime::tools::paths::real_root(dir.path());
    std::fs::write(workdir.join("AGENTS.md"), "规范一").unwrap();
    let world = World::new(&config_with("", KEYWORD_MEMORY), finisher()).await;
    let state = world.gateway.state();
    state
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

    // 第一次：没有已展示的段，召回并读规范。
    let first = hooks.setup(conversation, &ask).await.unwrap();
    let first = texts(&first.sections);
    let find = |sections: &[(String, String)], key: &str| {
        sections
            .iter()
            .find(|(have, _)| have == key)
            .map(|(_, text)| text.clone())
    };
    assert!(
        find(&first, "memory").is_some_and(|text| text.contains("深色主题")),
        "{first:?}"
    );
    assert!(find(&first, "project").is_some_and(|text| text.contains("规范一")));
    let keys: Vec<&str> = first.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, ["base", "project", "memory"], "段的次序");

    // 之后：规范文件变了、记忆库里多了更相关的条目，已展示的两段原样不动。
    std::fs::write(workdir.join("AGENTS.md"), "规范二").unwrap();
    state
        .memory
        .put(seeded_memory("m-2", "用户的深色主题偏好是黑色"), None)
        .await
        .unwrap();
    let shown = Message::System(
        match system_message(
            &first
                .iter()
                .map(|(key, text)| (key.as_str(), text.as_str()))
                .collect::<Vec<_>>(),
        ) {
            Message::System(system) => system,
            _ => unreachable!(),
        },
    );
    let context = [shown.clone(), Message::user("深色主题", 2)];
    let pinned = texts(&hooks.setup(conversation, &context).await.unwrap().sections);
    assert_eq!(find(&pinned, "memory"), find(&first, "memory"));
    assert_eq!(find(&pinned, "project"), find(&first, "project"));
    assert_eq!(find(&pinned, "base"), find(&first, "base"));

    // 下一次 head：上下文里没有已展示的段，规范读新的、记忆重召回。
    let after_head = texts(&hooks.setup(conversation, &ask).await.unwrap().sections);
    assert!(find(&after_head, "project").is_some_and(|text| text.contains("规范二")));
    assert_ne!(find(&after_head, "memory"), find(&first, "memory"));

    // 被遗忘的条目立刻让钉住的那一块作废（`komo.memory` 里记着 uses）。
    let with_uses = hooks.setup(conversation, &ask).await.unwrap();
    let with_uses = texts(&with_uses.sections);
    let shown_now = system_message(
        &with_uses
            .iter()
            .map(|(key, text)| (key.as_str(), text.as_str()))
            .collect::<Vec<_>>(),
    );
    for id in ["m-1", "m-2"] {
        state
            .memories
            .forget(&MemoryId::from_raw(id), 1)
            .await
            .unwrap();
    }
    let context = [shown_now, Message::user("深色主题", 3)];
    let forgotten = texts(&hooks.setup(conversation, &context).await.unwrap().sections);
    assert_eq!(
        find(&forgotten, "memory"),
        None,
        "遗忘的记忆不会再被注入：{forgotten:?}"
    );
    assert!(find(&forgotten, "project").is_some(), "规范仍钉着");
    let _ = memory_address_for_tests(conversation);
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

/// 同一个 profile 下，段连起来与旧 `assemble` 的系统提示逐字相同（`komo-agent` 里还有一份
/// 纯函数的组合黄金测试）。
#[tokio::test]
async fn golden_system_sections_match_legacy_prompt() {
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
    let state = world.gateway.state();
    let conversation = world.conversation(Some(agent("assistant"))).await;
    let setup = world.hooks().setup(conversation, &[]).await.unwrap();

    // 旧路径的装配：同一份事实，经 `assemble`。
    let snapshot = state.snapshot();
    let profile = snapshot.agent.get("assistant").unwrap();
    let surface = setup.tools.surface().clone();
    let executor = &state.tool_executor;
    let legacy = assemble(ContextInput {
        instructions: profile.instructions.clone(),
        project_files: crate::service::project_files::discover(&workdir),
        project_update: None,
        contract: None,
        workspace: workdir.clone(),
        tools: executor
            .definitions_for(&surface)
            .into_iter()
            .map(|tool| tool.name)
            .collect(),
        mcp_servers: executor.namespaces_for(&surface),
        history: Vec::new(),
        memory: None,
        skills: crate::service::state::skills_block(&state.skills.read().unwrap(), surface.names()),
        invocation: InvocationContext::Main,
        projection: snapshot.execution.projection(),
        decayed: Default::default(),
        fork: None,
    })
    .system_prompt;
    let joined = setup
        .sections
        .iter()
        .map(|section| section.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    assert_eq!(joined, legacy);
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
