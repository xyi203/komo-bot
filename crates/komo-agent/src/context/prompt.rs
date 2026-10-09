//! 系统提示正文的一处装配（`docs/agent.md` §10、§11）：主 Agent 与子代理共用一套 section，
//! 不再各写一份——重复的那几句（工作目录、工具名、`RULES`）漂了，模型真正照着做的那一条
//! 就是漂掉的那一条。
//!
//! 这里只管**正文本身**；`instructions`（最前）与 `memory`（最后）两段由
//! [`super::assemble`] 拼在外面（§10：分隔符与顺序都照抄改造之前那一份）。

use std::path::Path;

use komo_kernel::types::delegate::DelegateSpec;

use super::{InvocationContext, SkillCatalog};

/// 提示里每条 Run 都说的那几句行为约束。
///
/// **一处定义**：主对话与子代理两份提示各抄一份就会漂，而漂掉的那一条恰恰是模型真正照
/// 着做的那一条。
///
/// 学 pi（`system-prompt.ts`）：**规则跟着工具走**——没挂 `rg` 就不说"用 rg"，说了也
/// 用不上。工具名本身不再列：API 的 tool schema 里已经有一份。
fn rules(tools: &[String]) -> String {
    let mut lines = Vec::new();
    if tools.iter().any(|tool| tool == RG_TOOL) {
        lines.push("- 找代码和文字用 rg，别用 shell 拼 grep / find / ls。");
    }
    lines.push("- 危险操作会被拦下等人批准；被拒绝就当作结果，不要绕过。");
    lines.push("- 如实报告做了什么、证据是什么；没有证据就说没有。");
    lines.join("\n")
}

const RG_TOOL: &str = "rg";

/// 装配系统提示正文：身份 / 工作目录 / 行为约束，主 Agent 再加「komo 自己的状态怎么查」
/// 与 §5.6 的 skills 目录、子代理再加任务与结果契约。
pub(crate) fn build(
    workspace: &Path,
    tools: &[String],
    invocation: &InvocationContext,
    skills: Option<&SkillCatalog>,
) -> String {
    let rules = rules(tools);
    match invocation {
        InvocationContext::Main => {
            let has_shell = tools.iter().any(|tool| tool == SHELL_TOOL);
            main_prompt(workspace, &rules, skills, has_shell)
        }
        InvocationContext::Delegated(spec) => subagent_prompt(workspace, &rules, spec),
    }
}

/// 主 Agent 的系统提示：身份 / 工作目录 / 行为约束 / komo 自查 / §5.6 的 skills 目录。
fn main_prompt(
    workspace: &Path,
    rules: &str,
    skills: Option<&SkillCatalog>,
    has_shell: bool,
) -> String {
    let mut prompt = format!(
        "你是 komo，跑在用户自己机器上的助手。\n\
         工作目录：{}\n\
         {rules}",
        workspace.display(),
    );
    if has_shell {
        prompt.push_str("\n\n");
        prompt.push_str(SELF_BLOCK);
    }
    if let Some(block) = skills.and_then(SkillCatalog::prompt_block) {
        prompt.push_str("\n\n");
        prompt.push_str(&block);
    }
    prompt
}

/// 能跑 komo CLI 的那个工具；没挂它时 [`SELF_BLOCK`] 不出现——说了也跑不了。
const SHELL_TOOL: &str = "shell";

/// 「你就跑在 komo 里」：komo 自己的状态用 komo CLI 查（2026-09-24 的实际会话：问
/// "有哪些 cron job"，模型先读了同名的共享 skill、再翻系统 crontab / launchctl，第 8 轮才
/// 想到 `komo cron list`）。`komo` 靠 PATH 找到：服务单元写入了安装时的 PATH（§3）。
///
/// 学 pi 的 docs 段：只给入口（`komo --help`）和一个例子，子命令清单不抄进提示。
const SELF_BLOCK: &str = "komo 自己的状态（cron、会话 / Run、记忆、审批 / 介入、配置、skills、\
                          toolbox、Gateway）用 shell 跑 `komo <子命令>` 查，例如 `komo cron list`，\
                          其余看 `komo --help`；别去翻系统 crontab、launchctl 或同名 skill。";

/// 子代理的系统提示（§4）。
///
/// **它不共享父那份正文**：子代理拿不到父的对话历史、记忆与 Skills，只拿得到这一条任务。
/// 所以提示必须自己把三件事说清：这是被派出来的、干完要交什么、以及"任务里没写的东西
/// 就是没给你"——最后这句不是客套，它把"自包含"从一句设计口号变成对子代理可执行的要求。
fn subagent_prompt(workspace: &Path, rules: &str, spec: &DelegateSpec) -> String {
    let mut prompt = format!(
        "你是 komo 派出的子代理，只做下面这件事。你看不到主对话、记忆和 Skills：\
         任务里没写的就是没给，缺什么用工具查，查不到如实说。\n\
         任务：{}\n\
         工作目录：{}\n\
         {rules}",
        spec.task,
        workspace.display(),
    );
    prompt.push_str("\n\n最后一条回复只放结果，不调工具、不寒暄：");
    match &spec.contract {
        Some(contract) => {
            prompt.push_str(
                "\n一个满足下面 schema 的 JSON 对象（父侧用同一份校验，不合规会退回）：\n",
            );
            prompt.push_str(
                &serde_json::to_string_pretty(&contract.schema).unwrap_or_else(|_| "{}".into()),
            );
        }
        None => prompt.push_str("\n一段能独立读懂的结论（父侧只拿走这段文本）。"),
    }
    prompt
}

#[cfg(test)]
mod tests;
