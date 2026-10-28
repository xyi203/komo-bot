//! 系统段装配（`docs/agent.md` §5）：`ContextInput` → 按段的系统提示。
//!
//! "对于这个 Agent、这个时刻，系统提示里有什么"的答案只有一处，就是 [`system_sections`]。它是
//! 纯函数：**相同 `ContextInput` 产出相同的段**，没有 I/O、没有失败路径。Gateway 负责把需要
//! I/O 才能获得的事实凑齐，传进来的必须已经是**读到的正文**，不是"去哪读"的指针。
//! 回放消息不在这里：新存储的上下文由 kernel 的 `durable` 从 entry 推出来。

pub mod compaction;
pub mod memory;
mod prompt;

use std::path::PathBuf;

use komo_kernel::types::agent::ProjectFile;
use komo_kernel::types::delegate::{DelegateContract, DelegateSpec};
use komo_kernel::types::tool::ToolNamespace;

pub use prompt::{
    PROJECT_UPDATE_HEAD, SECTION_BASE, SECTION_INSTRUCTIONS, SECTION_MEMORY, SECTION_PROJECT,
    SECTION_SKILLS, TASK_MODE_BLOCK,
};

/// 一次装配所需的**全部事实**（`docs/agent.md` §5）。
///
/// 身份里的模型、能力面、记忆作用域不进来：它们决定的是请求的 model、tool definitions 和召回
/// 范围，都在 I/O 阶段用掉了，装配用不到。
pub struct ContextInput {
    /// 身份指令正文（已从冻结快照指向的 payload 读回）。
    pub instructions: Option<String>,
    /// 项目规范文件（`AGENTS.md` 等，已从冻结快照指向的 payload 读回），外层在前。
    /// 只渲染进主 Agent 的提示；空 = 没有（或者是子代理，调用方不给）。
    pub project_files: Vec<ProjectFile>,
    /// 解析过的真实工作目录。
    pub workspace: PathBuf,
    /// 这一段真正挂上的工具名，按 Schema 的顺序。RULES 按它挑规则（挂了 `rg` 才说"用
    /// rg"），skills 门控也按它。
    /// 名字本身不进提示：API 的 tool schema 里已经有一份。
    pub tools: Vec<String>,
    /// 只在 codemode 里调的那些工具所属的组（MCP 服务器，`docs/mcp.md` §3）：工具没进 schema，
    /// 模型从系统提示的这一段知道它们在。空 = 不出这一段。
    pub mcp_servers: Vec<ToolNamespace>,
    /// 这一段钉住的记忆注入段；`None` = 不注入（子代理、记忆关闭、没召回到）。
    /// 正文由 [`memory::render`] 渲染、由 `MemoryManager` 按段钉住，这里只决定它放在哪。
    pub memory: Option<String>,
    /// 渲染好的 skills 目录块（[`SkillCatalog::prompt_block`]）；只有主 Agent 有。是文本
    /// 而不是目录值：这条 Run 受理时冻结下来的就是这段正文，续跑按它逐字装配。
    ///
    /// [`SkillCatalog::prompt_block`]: crate::skills::SkillCatalog::prompt_block
    pub skills: Option<String>,
    pub invocation: InvocationContext,
    /// 这是 `task` 派出的 fresh 任务会话：`base` 段末尾补 [`TASK_MODE_BLOCK`]（中途没人能回答）。
    /// fork 任务不走这里，它沿用父的段。
    pub background_task: bool,
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

/// 系统提示按段交回 `(key, 正文)`（新存储的 `komo.system` entry 按段名补丁，
/// `docs/storage-durable-d5.md` §6）：`instructions` / `base` / `project` / `skills` /
/// `memory`，空的段不出现，key 常量见 [`SECTION_INSTRUCTIONS`] 等；用 `"\n\n"` 连起来就是
/// 完整的系统提示（身份指令在最前，记忆在最后）。只看 `input` 里拼系统提示的那几样事实（身份、
/// 项目规范、工作目录、工具名、服务器、记忆、skills、调用方式）。
pub fn system_sections(input: &ContextInput) -> Vec<(String, String)> {
    prompt::sections(
        input.instructions.as_deref(),
        &input.workspace,
        &input.tools,
        &input.mcp_servers,
        &input.invocation,
        input.background_task,
        &input.project_files,
        input.skills.as_deref(),
        input.memory.as_deref(),
    )
}

/// `task` 带契约时补在任务后面的那一段（与子代理的 Result Contract 同一套说法）。
/// 新 harness 的 `OrchestrationHooks::contract_block` 直接调它，两处文字不会漂。
pub fn task_contract_block(contract: &DelegateContract) -> String {
    prompt::task_contract_block(contract)
}

/// 这些项目规范文件渲染成的系统段正文（`project` 段）；一份都没有是 `None`。网关拿它与已展示的
/// `project` 段比，看规范文件是不是在会话中改过（改过就补 [`project_update_block`]）。
pub fn project_section(files: &[ProjectFile]) -> Option<String> {
    prompt::project_block(files)
}

/// 会话里规范改过之后补在任务后面的那一块。
pub fn project_update_block(files: &[ProjectFile]) -> String {
    prompt::project_update_block(files)
}

#[cfg(test)]
mod tests;
