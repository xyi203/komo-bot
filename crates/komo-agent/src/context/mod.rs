//! Agent Context Boundary（`docs/agent.md` §5）：`ContextInput` → `AgentContext`。
//!
//! 这是这一次重构最核心的边界——"对于这个 Agent、这个 Run、这个时刻，模型应该看到什么"
//! 的答案只有一处，就是 [`assemble`]。它是纯函数：**相同 `ContextInput` 产出相同
//! `AgentContext`**，没有 I/O、没有失败路径。Gateway / Runtime 负责把需要 I/O 才能获得
//! 的事实凑齐（`context_sources.rs`），传进来的必须已经是**读到的正文**，不是"去哪读"的
//! 指针——`MemoryManager`、`Store`、`PayloadStore`、`ToolOutputStore`、`Coordinator` 都不
//! 允许出现在这个模块里。

pub mod history;
pub mod memory;
mod prompt;
pub mod tasks;

use std::path::PathBuf;

use komo_kernel::types::delegate::DelegateSpec;
use komo_kernel::types::turn::ReplayMessage;

pub use history::{
    Entry, EntryKind, ReplayScope, ResolvedMessage, StoredOutput, entries, latest_user_text,
};
pub use tasks::TaskBoard;

use crate::skills::SkillCatalog;

/// 一次装配所需的**全部事实**（`docs/agent.md` §5）。
///
/// 身份里的模型、能力面、记忆作用域不进来：它们决定的是 `TurnRequest.model`、tool
/// definitions 和召回范围，都在 I/O 阶段用掉了，assembler 用不到。
pub struct ContextInput<'s> {
    /// 身份指令正文（已从冻结快照指向的 payload 读回）。
    pub instructions: Option<String>,
    /// 解析过的真实工作目录。
    pub workspace: PathBuf,
    /// 这一段真正挂上的工具名，按 Schema 的顺序。系统提示要列它们，skills 门控也按它，
    /// 所以 tool definitions 必须在 [`assemble`] 之前算好。
    pub tools: Vec<String>,
    /// 已解析正文的回放条目。借用 `Surface`，不复制消息。
    pub history: Vec<ResolvedMessage<'s>>,
    /// 这一段钉住的记忆注入段；`None` = 不注入（子代理、记忆关闭、没召回到）。
    /// 正文由 [`memory::render`] 渲染、由 `MemoryManager` 按段钉住，这里只决定它放在哪。
    pub memory: Option<String>,
    /// 已按 `OfferContext` 门控的 skills 目录；只有主 Agent 有。
    pub skills: Option<SkillCatalog>,
    /// 分发器的任务看板（`docs/home-dispatcher.md` §5、§9 Phase 3）；`None` = 这一段不是
    /// 分发器 Run，不渲染这一节——**其余场景的提示因此逐字不变**（golden 不动）。渲染
    /// 位置在 [`assemble`] 里：skills 之后、memory 之前。
    pub tasks: Option<TaskBoard>,
    pub invocation: InvocationContext,
    /// 工具结果投影的正文预算（`[execution] model_result_bytes`）。
    /// 与 `CallEnv` 用同一个值，否则"刚跑完"和"回放"渲染出来不一样。
    pub model_result_bytes: usize,
}

/// 这一段是主 Agent 的对话，还是一次委派（`docs/agent.md` §11）。
///
/// 直接带 `DelegateSpec`：结果契约的 schema 要原样渲染进提示，只放 `task` 与
/// `parent_run_id` 是不够的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvocationContext {
    Main,
    Delegated(DelegateSpec),
}

/// 装配出来的东西：模型看到的系统提示与回放消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<ReplayMessage>,
}

/// 唯一的 Context Assembly 入口（`docs/agent.md` §5）。
///
/// 顺序（§10）：Instructions（最前）→ Identity / Task / 工作目录 / 工具 / RULES → komo 自查
/// （仅主 Agent 且挂了 `shell`）→ Skills
/// （仅主 Agent）→ Result Contract（仅子代理）→ **任务看板**（仅分发器 Run，
/// `docs/home-dispatcher.md` §5）→ Memory（最后，`\n\n` + 正文）。
///
/// `tasks: None` 时不多一段——非分发器场景的提示因此逐字不变（golden 不受影响）。
pub fn assemble(input: ContextInput<'_>) -> AgentContext {
    let mut prompt = prompt::build(
        &input.workspace,
        &input.tools,
        &input.invocation,
        input.skills.as_ref(),
    );
    prompt = with_instructions(input.instructions.as_deref(), prompt);
    if let Some(board) = &input.tasks {
        prompt.push_str("\n\n");
        prompt.push_str(&tasks::prompt_block(board));
    }
    if let Some(memory) = &input.memory {
        prompt.push_str("\n\n");
        prompt.push_str(memory);
    }
    let messages = history::to_replay_messages(input.history, input.model_result_bytes);
    AgentContext {
        system_prompt: prompt,
        messages,
    }
}

/// 把身份指令放在系统提示**最前面**，基座提示排在它后面。
fn with_instructions(instructions: Option<&str>, prompt: String) -> String {
    match instructions.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("{text}\n\n{prompt}"),
        None => prompt,
    }
}

#[cfg(test)]
mod tests;
