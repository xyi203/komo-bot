//! 系统段（`AgentSetup.sections`）：模型每次请求前看到的系统提示的来源。
//!
//! 新存储的系统提示是 `komo.system` entry 回放出来的**补丁**（`harness/system.rs`）：钩子每轮
//! 把"期望的段"交回去，runtime 与已展示的比，没变就什么都不写（前缀字节不动、提示缓存照样
//! 命中）。段的文字全部来自 `komo_agent::context::system_sections`；key 与次序：
//!
//! `instructions`（Profile 的身份指令）→ `base`（身份 / 工作目录 / RULES / komo 自查 / MCP 服务器）
//! → `project`（`AGENTS.md` 等）→ `skills`（目录行）。
//!
//! 召回的记忆不在系统提示里：每个 Run 一条召回块追加在输入之后（`recall.rs`，`docs/memory.md`
//! §9.4）。升级前的会话里钉着的 `memory` 段，下一次请求时由补丁去掉。
//!
//! # 已展示的段钉住
//!
//! 已经展示过一份基线（`replay_sections(context)` 不空）时，`project` 与 `skills` **用已展示的
//! 那份**，不再现读磁盘、不再重算目录——"没有"也钉住：规范文件变了、注册表里多了 / 少了 skill，
//! 都要等下一次 head（`/new`、压缩）才换，否则每次变化都打掉整段前缀缓存（退避重发的请求也不会
//! 带上期间新增的 skill，W4b）。规范文件变了的事由 `submit.rs` 把 `project_update_block` 补进下一条
//! 输入（W1c）。其余的段（`base` / `instructions`）每次按当前事实重渲，文字没变就不写。
//!
//! **fork 任务**返回 `replay_sections(context)`：fork 的 conversation 一开头就带着父的 `komo.system`
//! entry，段原样沿用——首请求与父的上一次请求逐字节同前缀。

use std::sync::Arc;

use komo_agent::context::{
    ContextInput, InvocationContext, SECTION_BASE, SECTION_INSTRUCTIONS, SECTION_PROJECT,
    SECTION_SKILLS, system_sections,
};
use komo_kernel::durable::Message;
use komo_kernel::types::agent::MemoryMode;
use komo_kernel::types::memory::MemoryScope;
use komo_runtime::durable::harness::system::replay_sections;
use komo_runtime::durable::harness::{HookError, PromptSection, ToolSet};

use super::setup::Resolved;
use crate::service::state::GatewayState;

pub(super) fn build(
    state: &Arc<GatewayState>,
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
        workspace: resolved.workspace.clone(),
        tools: tool_names,
        mcp_servers: executor.namespaces_for(surface),
        skills,
        invocation: InvocationContext::Main,
        background_task: resolved.is_task(),
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
    ] {
        let keep_shown = pinned && (key == SECTION_PROJECT || key == SECTION_SKILLS);
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

pub(super) fn recall_scopes(resolved: &Resolved) -> Option<Vec<MemoryScope>> {
    scopes_for(
        resolved.profile.memory,
        &resolved.agent_id,
        resolved.profile.memory_scope.as_ref(),
    )
}

/// 这一次召回放行哪些作用域（§9.2）；`None` = 这一段不召回。
///
/// `memory = "off"` 不召回；`"private"` 只放行这个 Agent 自己的 `agent:<id>`——**不带
/// Personal**，那是操作者的个人记忆，私有记忆的 Profile 不该碰；`"shared"` 看 Profile
/// 的 `memory_scope`：没写 = **不按作用域过滤**（`RecallQuery::scopes` 空就是"全部"），
/// 写了就只召回那一个，外加 [`MemoryScope::Personal`]——用户资料是**显式共享**的那一份，
/// 每个 Agent 都该看得见。
fn scopes_for(
    mode: MemoryMode,
    agent_id: &str,
    scope: Option<&MemoryScope>,
) -> Option<Vec<MemoryScope>> {
    match mode {
        MemoryMode::Off => None,
        MemoryMode::Private => Some(vec![MemoryScope::Agent {
            agent_id: agent_id.to_string(),
        }]),
        MemoryMode::Shared => {
            let Some(scope) = scope else {
                return Some(Vec::new());
            };
            let mut scopes = vec![scope.clone()];
            if *scope != MemoryScope::Personal {
                scopes.push(MemoryScope::Personal);
            }
            Some(scopes)
        }
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    fn agent(id: &str) -> MemoryScope {
        MemoryScope::Agent {
            agent_id: id.into(),
        }
    }

    #[test]
    fn recall_scopes_follow_the_memory_mode() {
        assert_eq!(scopes_for(MemoryMode::Off, "helper", None), None);
        // 私有：只有它自己的作用域，Profile 的 `memory_scope` 不参与、Personal 不带。
        assert_eq!(
            scopes_for(MemoryMode::Private, "helper", Some(&MemoryScope::Personal)),
            Some(vec![agent("helper")])
        );
        // 共享：没写 scope = 不按作用域过滤。
        assert_eq!(
            scopes_for(MemoryMode::Shared, "assistant", None),
            Some(vec![])
        );
        let project = MemoryScope::Project {
            project_id: "komo".into(),
        };
        assert_eq!(
            scopes_for(MemoryMode::Shared, "assistant", Some(&project)),
            Some(vec![project, MemoryScope::Personal])
        );
    }
}
