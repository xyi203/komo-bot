//! 系统段（`AgentSetup.sections`）：模型每次请求前看到的系统提示的来源。
//!
//! 新存储的系统提示是 `komo.system` entry 回放出来的**补丁**（`harness/system.rs`）：钩子每轮
//! 把"期望的段"交回去，runtime 与已展示的比，没变就什么都不写（前缀字节不动、提示缓存照样
//! 命中）。段的文字全部来自 `komo_agent::context::system_sections`（与旧 `assemble` 逐字相同，
//! `golden_system_sections_match_legacy_prompt` 锁着）；key 与次序：
//!
//! `instructions`（Profile 的身份指令）→ `base`（身份 / 工作目录 / RULES / komo 自查 / MCP 服务器）
//! → `project`（`AGENTS.md` 等）→ `skills`（目录行）→ `memory`（召回的记忆，最后）。
//!
//! # 已展示的段钉住
//!
//! 已经展示过一份基线（`replay_sections(context)` 不空）时，`project` 与 `memory` **用已展示的那份**，
//! 不再现读磁盘、不再召回——"没有"也钉住：规范文件变了或这一段对话又召回出别的记忆，都要等
//! 下一次 head（`/new`、压缩）才换，否则每次变化都打掉整段前缀缓存。规范文件变了的事由
//! `submit.rs` 把 `project_update_block` 补进下一条输入（W1c）；记忆条目被遗忘则立刻作废这一块
//! （核对 `komo.memory` document 里记的 uses）。其余的段（`base` / `instructions` / `skills`）
//! 每次按当前事实重渲，文字没变就不写。
//!
//! **fork 任务**返回 `replay_sections(context)`：fork 的 conversation 一开头就带着父的 `komo.system`
//! entry，段原样沿用——首请求与父的上一次请求逐字节同前缀。

use std::sync::Arc;

use komo_agent::context::{
    ContextInput, InvocationContext, SECTION_BASE, SECTION_INSTRUCTIONS, SECTION_MEMORY,
    SECTION_PROJECT, SECTION_SKILLS, system_sections,
};
use komo_kernel::durable::{Content, ContentBlock, ConversationId, Message};
use komo_kernel::types::memory::MemoryScope;
use komo_runtime::durable::harness::system::replay_sections;
use komo_runtime::durable::harness::{HookError, PromptSection, ToolSet};

use super::setup::Resolved;
use crate::service::durable::DurableCore;
use crate::service::state::GatewayState;

// `komo.memory` document 的定义在 runtime（提取与 run 详情也读它；W3c 加了 `recall` / `at` 两格）。
pub use komo_runtime::memory::durable::{MEMORY_DOC_VERSION, MemoryDoc, memory_address};

pub(super) async fn build(
    state: &Arc<GatewayState>,
    core: &Arc<DurableCore>,
    resolved: &Resolved,
    tools: &ToolSet,
    context: &[Message],
) -> Result<Vec<PromptSection>, HookError> {
    let shown = replay_sections(context);
    // fork 任务：父的段原样沿用。
    if resolved.is_fork() && !shown.is_empty() {
        return Ok(shown
            .into_iter()
            .map(|(key, text)| PromptSection::new(key, text))
            .collect());
    }
    let pinned = !shown.is_empty();
    let shown_text = |key: &str| {
        shown
            .iter()
            .find(|(have, _)| have == key)
            .map(|(_, text)| text.clone())
    };

    let project_files = if pinned || !resolved.profile.project_files {
        Vec::new()
    } else {
        crate::service::project_files::discover(&resolved.workspace)
    };
    let memory = if pinned {
        None
    } else {
        recall(state, core, resolved, context).await
    };
    // 已展示的记忆被遗忘 / 失效了：这一块作废，按当前事实重召回（`forget` 立刻生效，§9.2）。
    let memory_dead = match (pinned, shown_text(SECTION_MEMORY)) {
        (true, Some(_)) => !pinned_memory_valid(state, core, resolved).await,
        _ => false,
    };
    let memory = if memory_dead {
        recall(state, core, resolved, context).await
    } else {
        memory
    };

    let config = state.snapshot();
    let surface = tools.surface();
    let executor = &state.tool_executor;
    let tool_names: Vec<String> = tools
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    let skills = crate::service::state::skills_block(
        &state.skills.read().expect("skills 注册表"),
        surface.names(),
    );
    let input = ContextInput {
        instructions: resolved.profile.instructions.clone(),
        project_files,
        project_update: None,
        contract: None,
        workspace: resolved.workspace.clone(),
        tools: tool_names,
        mcp_servers: executor.namespaces_for(surface),
        history: Vec::new(),
        memory,
        skills,
        invocation: InvocationContext::Main,
        projection: config.execution.projection(),
        decayed: Default::default(),
        fork: None,
    };
    let fresh = system_sections(&input);
    let fresh_text = |key: &str| {
        fresh
            .iter()
            .find(|(have, _)| have == key)
            .map(|(_, text)| text.clone())
    };

    let mut out = Vec::new();
    for key in [
        SECTION_INSTRUCTIONS,
        SECTION_BASE,
        SECTION_PROJECT,
        SECTION_SKILLS,
        SECTION_MEMORY,
    ] {
        // 钉住的两段用已展示的那份；记忆块作废了就用刚重召回的。
        let keep_shown = pinned && is_pinned_key(key) && !(key == SECTION_MEMORY && memory_dead);
        let text = if keep_shown {
            shown_text(key)
        } else {
            fresh_text(key)
        };
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            out.push(PromptSection::new(key, text));
        }
    }
    Ok(out)
}

fn is_pinned_key(key: &str) -> bool {
    key == SECTION_PROJECT || key == SECTION_MEMORY
}

/// 召回这一段的记忆（只在没有已展示的 `memory` 段时问；召回失败不拦这一轮）。
///
/// 查询文本是上下文里最后一条用户消息（§9.4）。召回出来的 uses 同时记进 `komo.memory`
/// document（最佳努力：记不下来只留一条 warn，代价是遗忘核对退化成"不核对"）。
async fn recall(
    state: &Arc<GatewayState>,
    core: &Arc<DurableCore>,
    resolved: &Resolved,
    context: &[Message],
) -> Option<String> {
    if !state.memories.enabled() {
        return None;
    }
    let scopes = recall_scopes(resolved)?;
    let text = latest_user_text(context).unwrap_or_default();
    let run =
        komo_kernel::types::ids::RunId::from_raw(format!("conversation-{}", resolved.conversation));
    let injection = state.memories.prepare(&run, &text, &[], &scopes).await;
    let conversation = resolved.conversation;
    let doc = MemoryDoc {
        uses: injection.uses.clone(),
        recall: injection.diagnostics.clone(),
        at: (state.clock.now().unix_timestamp_nanos() / 1_000_000) as i64,
    };
    if let Err(error) = write_memory_doc(core, conversation, &doc).await {
        tracing::warn!(%error, %conversation, "这一块记忆注入的 uses 没记进 komo.memory");
    }
    injection.text.filter(|text| !text.is_empty())
}

fn recall_scopes(resolved: &Resolved) -> Option<Vec<MemoryScope>> {
    crate::service::context_sources::recall_scopes(
        resolved.profile.memory,
        &resolved.agent_id,
        resolved.profile.memory_scope.as_ref(),
    )
}

async fn write_memory_doc(
    core: &DurableCore,
    conversation: ConversationId,
    doc: &MemoryDoc,
) -> Result<(), komo_runtime::durable::DurableError> {
    let value = serde_json::to_value(doc).expect("MemoryDoc 可序列化");
    core.engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.put_document(memory_address(conversation), MEMORY_DOC_VERSION, value)
                    .await
            })
        })
        .await
}

/// 钉住的那块记忆里的条目是不是都还在、都还能召回（只看 `komo.memory` 里记的 uses；没记 = 视为
/// 有效，没有证据说它失效）。
async fn pinned_memory_valid(
    state: &Arc<GatewayState>,
    core: &Arc<DurableCore>,
    resolved: &Resolved,
) -> bool {
    let record = match core
        .storage
        .find_document(&memory_address(resolved.conversation))
        .await
    {
        Ok(Some(record)) => record,
        _ => return true,
    };
    let Ok(doc) = serde_json::from_value::<MemoryDoc>(record.value) else {
        return true;
    };
    if doc.uses.is_empty() {
        return true;
    }
    let alive = state
        .memories
        .revalidate(&doc.uses, state.clock.now())
        .await;
    alive.len() == doc.uses.len()
}

fn latest_user_text(context: &[Message]) -> Option<String> {
    context.iter().rev().find_map(|message| match message {
        Message::User(user) => Some(match &user.content {
            Content::Text(text) => text.clone(),
            Content::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }),
        _ => None,
    })
}
