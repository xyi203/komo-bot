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
const RULES: &str = "找代码和文字用 rg 工具；不要用 shell 里的 grep / find / ls 拼搜索。\n\
                     危险操作会被拦下来等人批准；被拒绝就把它当作结果，不要绕过。\n\
                     做完之后如实报告做了什么、有什么证据；没有证据就说没有。";

/// 装配系统提示正文：身份 / 任务 / 工作目录 / 挂着的工具 / 三条行为约束，主 Agent 再加
/// 「komo 自己的状态怎么查」与 §5.6 的 skills 目录、子代理再加结果契约。
pub(crate) fn build(
    workspace: &Path,
    tools: &[String],
    invocation: &InvocationContext,
    skills: Option<&SkillCatalog>,
    komo_exe: Option<&Path>,
) -> String {
    let names: Vec<&str> = tools.iter().map(String::as_str).collect();
    let tool_names = if names.is_empty() {
        "（这一段没有工具）".to_string()
    } else {
        names.join("、")
    };

    match invocation {
        InvocationContext::Main => {
            let komo = komo_exe.filter(|_| tools.iter().any(|tool| tool == SHELL_TOOL));
            main_prompt(workspace, &tool_names, skills, komo)
        }
        InvocationContext::Delegated(spec) => subagent_prompt(workspace, &tool_names, spec),
    }
}

/// 主 Agent 的系统提示：身份 / 工作目录 / 挂着的工具 / 三条行为约束 / komo 自查 / §5.6 的
/// skills 目录。
fn main_prompt(
    workspace: &Path,
    tool_names: &str,
    skills: Option<&SkillCatalog>,
    komo_exe: Option<&Path>,
) -> String {
    let mut prompt = format!(
        "你是 komo，一个在用户自己机器上运行的助手。\n\
         工作目录：{}\n\
         可用工具：{tool_names}\n\
         {RULES}",
        workspace.display(),
    );
    if let Some(exe) = komo_exe {
        prompt.push_str("\n\n");
        prompt.push_str(&self_block(exe));
    }
    if let Some(block) = skills.and_then(SkillCatalog::prompt_block) {
        prompt.push_str("\n\n");
        prompt.push_str(&block);
    }
    prompt
}

/// 能跑 komo CLI 的那个工具；没挂它时 [`self_block`] 不出现——说了也跑不了。
const SHELL_TOOL: &str = "shell";

/// 「你就跑在 komo 里」：komo 自己的状态用 komo CLI 查（2026-09-24 的实际会话：问
/// "有哪些 cron job"，模型先读了同名的共享 skill、再翻系统 crontab / launchctl，第 8 轮才
/// 想到 `komo cron list`）。路径由 Gateway 传进来（进程内不变），PATH 里没有 komo 也跑得通。
fn self_block(exe: &Path) -> String {
    let exe = exe.display();
    format!(
        "你运行在 komo 里。cron 任务、会话与 Run、记忆、待处理的审批 / 介入、配置、skills、\
         toolbox、Gateway 这些都是 komo 自己的状态：问到它们就用 shell 直接跑 komo 命令查，\
         不要去翻系统 crontab、launchctl 或同名的 skill。\n\
         komo 可执行文件：{exe}（PATH 里不一定有 komo，用这个绝对路径）\n\
         常用子命令：cron list、session list、run inspect <RUN_ID>、memory search <查询词>、\
         intervention list、config check、skills list、toolbox list、gateway status、doctor；\
         其余看 `{exe} --help`。"
    )
}

/// 子代理的系统提示（§4）。
///
/// **它不共享父那份正文**：子代理拿不到父的对话历史、记忆与 Skills，只拿得到这一条任务。
/// 所以提示必须自己把三件事说清：这是被派出来的、干完要交什么、以及"任务里没写的东西
/// 就是没给你"——最后这句不是客套，它把"自包含"从一句设计口号变成对子代理可执行的要求。
fn subagent_prompt(workspace: &Path, tool_names: &str, spec: &DelegateSpec) -> String {
    let mut prompt = format!(
        "你是 komo 派出去的子代理，只负责下面这一件事。你看不到主对话、记忆与 Skills——\
         任务里没写的上下文就是没有给你，需要什么就用工具自己查，查不到就如实说。\n\
         任务：{}\n\
         工作目录：{}\n\
         可用工具：{tool_names}\n\
         {RULES}",
        spec.task,
        workspace.display(),
    );
    prompt.push_str("\n\n最后一条回复要**只有结果本身**（不要在那里调工具、不要寒暄）：");
    match &spec.contract {
        Some(contract) => {
            prompt.push_str(
                "\n一个 JSON 对象，满足下面这份 schema——父侧会用**同一份**校验，\
                 不合规会被退回来让你改：\n",
            );
            prompt.push_str(
                &serde_json::to_string_pretty(&contract.schema).unwrap_or_else(|_| "{}".into()),
            );
        }
        None => prompt.push_str("\n一段能独立读懂的结论（父侧只会把这段文本拿走）。"),
    }
    prompt
}

#[cfg(test)]
mod tests;
