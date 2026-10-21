//! 系统提示正文的一处装配（`docs/agent.md` §10、§11）：主 Agent 与子代理共用一套 section，
//! 不再各写一份——重复的那几句（工作目录、工具名、`RULES`）漂了，模型真正照着做的那一条
//! 就是漂掉的那一条。
//!
//! 这里只管**正文本身**；`instructions`（最前）与 `memory`（最后）两段由
//! [`super::assemble`] 拼在外面（§10：分隔符与顺序都照抄改造之前那一份）。

use std::path::Path;

use komo_kernel::types::agent::ProjectFile;
use komo_kernel::types::delegate::{DelegateContract, DelegateSpec};
use komo_kernel::types::tool::ToolNamespace;

use super::InvocationContext;

/// 提示里每条 Run 都说的那几句行为约束。
///
/// **一处定义**：主对话与子代理两份提示各抄一份就会漂，而漂掉的那一条恰恰是模型真正照
/// 着做的那一条。
///
/// 学 pi（`system-prompt.ts`）：**规则跟着工具走**——没挂 `rg` 就不说"用 rg"，说了也
/// 用不上。工具名本身不再列：API 的 tool schema 里已经有一份。
fn rules(tools: &[String]) -> String {
    let has = |name: &str| tools.iter().any(|tool| tool == name);
    let mut lines = Vec::new();
    if has(RG_TOOL) {
        lines.push("- 找代码、文字和列文件用 rg（列文件设 list_files=true），别用 bash 拼 rg / grep / find / ls。");
    }
    // 2026-10 的统计：364 次 shell 里 126 次是 cat / head / ls 这类纯只读命令。
    if has(READ_TOOL) && has(BASH_TOOL) {
        lines.push("- 看文件用 read，别用 bash 跑 cat / head / tail / sed。");
    }
    // 2026-10-15 的统计：Opus 88% 的回合只发一个调用，执行器早就能让同一轮的读同时跑。
    // 照 Claude Code：互不依赖的调用同一轮一起发。
    if has(READ_TOOL) || has(RG_TOOL) {
        lines.push(
            "- 互不依赖的调用（读几个文件、搜几个词）放在同一轮一起发，读和搜会同时执行；\
             只有后一个要用前一个的结果时才分轮。",
        );
    }
    // 2026-10-09 的统计：codemode 挂上了也几乎没人用，模型一轮一轮地 read → rg → read。
    // 多处读改由并行调用承担，codemode 留给要在脚本里筛、拼输出的情形。
    if has(CODEMODE_TOOL) {
        lines.push(
            "- 只想从大段输出里挑几行、或要把好几处的结果合起来处理时，\
             用 codemode 写一段脚本一次做完。",
        );
    }
    // 2026-10-15 的统计：edit 一次只改一处，约四分之一的改动被模型改用 bash 跑 python
    // 批量替换，绕过了版本核对。2026-10-21 起 edit 一次能改多处（`edits[]`，照 pi），
    // 每处都对原文件匹配，一次绑一个版本，不再连发。
    if has(EDIT_TOOL) {
        lines.push(
            "- 同一个文件要改好几处，用一次 edit 带多个 edits[]：每一处都对原文件匹配、\
             互不重叠，挨得近的改动合成一处；别连发几个 edit，也别用 bash 跑脚本替换。",
        );
    }
    lines.push("- 调用被拒绝就当作结果，不要换个工具绕过。");
    lines.push("- 如实报告做了什么、证据是什么；没有证据就说没有。");
    lines.join("\n")
}

const RG_TOOL: &str = "rg";
const READ_TOOL: &str = "read";
const CODEMODE_TOOL: &str = "codemode";
const EDIT_TOOL: &str = "edit";

/// 装配系统提示正文：身份 / 工作目录 / 行为约束，主 Agent 再加「komo 自己的状态怎么查」、
/// 项目规范与 §5.6 的 skills 目录，子代理再加任务与结果契约。
pub(crate) fn build(
    workspace: &Path,
    tools: &[String],
    mcp_servers: &[ToolNamespace],
    invocation: &InvocationContext,
    project_files: &[ProjectFile],
    skills: Option<&str>,
) -> String {
    let mut rules = rules(tools);
    if let Some(block) = servers_block(mcp_servers) {
        rules.push_str("\n\n");
        rules.push_str(&block);
    }
    match invocation {
        InvocationContext::Main => {
            let has_shell = tools.iter().any(|tool| tool == BASH_TOOL);
            main_prompt(workspace, &rules, project_files, skills, has_shell)
        }
        InvocationContext::Delegated(spec) => subagent_prompt(workspace, &rules, spec),
    }
}

/// 服务器段里一台服务器的说明最多这么多字（pi 的 `MAX_SERVER_DESCRIPTION_CHARS`）。
const MAX_SERVER_DESCRIPTION_CHARS: usize = 250;

/// 整段的字数上限（pi 的 `MAX_SERVERS_SECTION_CHARS`）：说明先缩短；只列名字都放不下时，
/// 后面的服务器不列，最后一行说还有几台。
const MAX_SERVERS_SECTION_CHARS: usize = 4096;

const SERVERS_INTRO: &str = "MCP 服务器（工具没有声明给你，在 codemode 脚本里调用；用 search_tools / \
                             describe_tool / describe_namespace 找工具、看参数与服务器的说明）：";

/// 只在 codemode 里调的工具所属的服务器，每台一行（照 pi 的 `renderServersSection`）。
/// 一台都没有就不出这一段。
fn servers_block(servers: &[ToolNamespace]) -> Option<String> {
    if servers.is_empty() {
        return None;
    }
    let mut listed: Vec<&ToolNamespace> = servers.iter().collect();
    listed.sort_by(|a, b| a.name.cmp(&b.name));
    let heads: Vec<String> = listed
        .iter()
        .map(|server| format!("- {}", server.name))
        .collect();
    let omitted = |count: usize| -> Vec<String> {
        match count {
            0 => Vec::new(),
            n => vec![format!("- …还有 {n} 台，用 search_tools() 找它们的工具")],
        }
    };
    let size = |kept: usize| -> usize {
        std::iter::once(SERVERS_INTRO.to_string())
            .chain(heads[..kept].iter().cloned())
            .chain(omitted(listed.len() - kept))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .count()
    };
    let mut kept = listed.len();
    while kept > 0 && size(kept) > MAX_SERVERS_SECTION_CHARS {
        kept -= 1;
    }
    // 每条说明前还有一个「：」。
    let per_server = match kept {
        0 => 0,
        _ => MAX_SERVER_DESCRIPTION_CHARS
            .min((MAX_SERVERS_SECTION_CHARS.saturating_sub(size(kept)) / kept).saturating_sub(1)),
    };
    let mut lines = vec![SERVERS_INTRO.to_string()];
    for (server, head) in listed.iter().zip(&heads).take(kept) {
        let summary = truncate(&server.summary(), per_server);
        lines.push(match summary.is_empty() {
            true => head.clone(),
            false => format!("{head}：{summary}"),
        });
    }
    lines.extend(omitted(listed.len() - kept));
    Some(lines.join("\n"))
}

/// 截到 `max` 个字，截了就以「…」结尾。
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max <= 1 {
        return String::new();
    }
    let head: String = text.chars().take(max - 1).collect();
    format!("{}…", head.trim_end())
}

/// 主 Agent 的系统提示：身份 / 工作目录 / 行为约束 / komo 自查 / 项目规范 / §5.6 的
/// skills 目录。
fn main_prompt(
    workspace: &Path,
    rules: &str,
    project_files: &[ProjectFile],
    skills: Option<&str>,
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
    if let Some(block) = project_block(project_files) {
        prompt.push_str("\n\n");
        prompt.push_str(&block);
    }
    if let Some(block) = skills {
        prompt.push_str("\n\n");
        prompt.push_str(block);
    }
    prompt
}

/// 能跑 komo CLI 的那个工具；没挂它时 [`SELF_BLOCK`] 不出现——说了也跑不了。
const BASH_TOOL: &str = "bash";

/// 「你就跑在 komo 里」：komo 自己的状态用 komo CLI 查（2026-09-24 的实际会话：问
/// "有哪些 cron job"，模型先读了同名的共享 skill、再翻系统 crontab / launchctl，第 8 轮才
/// 想到 `komo cron list`）。`komo` 靠 PATH 找到：服务单元写入了安装时的 PATH（§3）。
///
/// 学 pi 的 docs 段：只给入口（`komo --help`）和一个例子，子命令清单不抄进提示。
///
/// 「记住」那句（2026-10-20）：记忆原本只能等后台提取，用户说"记住这个习惯"时只能回一句
/// "等它出现在列表里再确认"。
///
/// 末句（2026-10-15）：会话里 6 次在 Run 里重启 Gateway，8 个调用因此记成 uncertain，
/// 最长的一条两个多小时后才收尾。
const SELF_BLOCK: &str = "komo 自己的状态（cron、会话 / Run、记忆、介入、配置、skills、\
                          Gateway）用 bash 跑 `komo <子命令>` 查，例如 `komo cron list`，\
                          其余看 `komo --help`；别去翻系统 crontab、launchctl 或同名 skill。\
                          用户要你记住什么（偏好、习惯、约定），当场用 `komo memory add \"一句陈述\"` \
                          记下，它记成用户原话、未确认，告诉用户可以用 `komo memory confirm` 确认；\
                          别替用户 confirm。\
                          你自己就跑在 Gateway 里：`komo gateway restart|stop` 和会重启 \
                          Gateway 的安装脚本会打断你正在做的这件事，要跑就放在最后一步，\
                          或者交给用户自己跑。";

/// 项目规范（`AGENTS.md` / `CLAUDE.md`）那一块：开头一句说清顺序与冲突怎么办，每份文件
/// 原样包在 `<project_instructions path="…">` 里（标签照 pi，`system-prompt.ts`）。
/// 正文全是空白的那份不出现；一份都不剩就整块不出现（不留空标题）。
fn project_block(files: &[ProjectFile]) -> Option<String> {
    let rendered: Vec<String> = files
        .iter()
        .filter(|file| !file.content.trim().is_empty())
        .map(|file| {
            format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                file.path,
                file.content.trim_end()
            )
        })
        .collect();
    if rendered.is_empty() {
        return None;
    }
    let mut block = String::from(
        "项目规范（从工作目录往上找到的 AGENTS.md / CLAUDE.md，外层在前；冲突时以靠后的为准）：",
    );
    for file in rendered {
        block.push_str("\n\n");
        block.push_str(&file);
    }
    Some(block)
}

/// 会话里规范改过之后补在任务后面的那一块：系统提示里那份是本会话早先的版本，冲突时以
/// 这里为准。一份都不剩就明说删掉了，不然模型会一直照着系统提示里的旧规范做。
pub(crate) fn project_update_block(files: &[ProjectFile]) -> String {
    let head =
        "（项目规范在本会话中改过。系统提示里的是早先的版本，下面是现在的；冲突时以这里为准。）";
    match project_block(files) {
        Some(block) => format!("{head}\n\n{block}"),
        None => format!("{head}\n\n项目规范文件现在都不在了，系统提示里那份不再适用。"),
    }
}

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
    prompt.push_str("\n\n");
    prompt.push_str(FINAL_REPLY);
    match &spec.contract {
        Some(contract) => {
            prompt.push_str(
                "\n一个满足下面 schema 的 JSON 对象（父侧用同一份校验，不合规会退回）：\n",
            );
            prompt.push_str(&contract_schema(contract));
        }
        None => prompt.push_str("\n一段能独立读懂的结论（父侧只拿走这段文本）。"),
    }
    prompt
}

/// 结果契约的开头一句：子代理的系统提示与 `task` 的契约段共用。
const FINAL_REPLY: &str = "最后一条回复只放结果，不调工具、不寒暄：";

fn contract_schema(contract: &DelegateContract) -> String {
    serde_json::to_string_pretty(&contract.schema).unwrap_or_else(|_| "{}".into())
}

/// `task` 带契约时补在任务后面的那一段（`docs/komo_bot.md` §4）：与子代理的 Result Contract
/// 同一套说法，但交回不退回——复验结论随结果一起交回派它的会话。
pub(crate) fn task_contract_block(contract: &DelegateContract) -> String {
    format!(
        "（结果契约）{FINAL_REPLY}\n一个满足下面 schema 的 JSON 对象（交回之前用同一份校验复验，\
         复验结论随结果一起交回）：\n{}",
        contract_schema(contract)
    )
}

#[cfg(test)]
mod tests;
