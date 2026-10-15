//! `codemode`：一段在操作系统沙箱里跑的 Python 脚本（`docs/codemode.md`）。
//!
//! 与 [`super::dispatch`] 同一个模式：这里只有定义与 `prepare`，执行在 executor
//! （`Operation::Codemode` 分流到 `run_codemode`）——脚本里的工具调用要回到执行器
//! 判定，工具自己够不到它。

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanVersions, RecoveryMode, Replay};
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::{normalized, parse_args, plan_time};

pub const CODEMODE_TOOL: &str = "codemode";

/// 脚本里调不了的：本地命令与写入（执行器只放行读文件与 MCP）、编排操作、计划与脚本自己。
///
/// 执行器用它把这些名字挡在 `TOOLS` 与子调用之外；`prepare` 用它在脚本跑起来**之前**
/// 就拒——实测模型常把 `tools.shell("git status")` 当成只读调用写进脚本，等跑到那一行
/// 才被拒，整段脚本作废，前面的 MCP 写也可能已经做了。
pub const NOT_IN_SCRIPTS: &[&str] = &[
    "shell",
    "python",
    "edit",
    "write",
    "codemode",
    "delegate",
    "dispatch",
    "follow",
    "monitor",
    "monitor_stop",
    "update_plan",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodeArgs {
    pub code: String,
}

#[derive(Debug, Default)]
pub struct CodemodeTool;

impl CodemodeTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for CodemodeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: CODEMODE_TOOL.into(),
            description: "写一段 Python 脚本，在脚本里调其他工具、在本地过滤和汇总，只有脚本的\
                          输出回到你这里。适合：一次读很多文件或搜很多处、从大段结果里挑出要的\
                          那几行、把几处结果拼成一张表。只读一个文件、只搜一次，直接调 read / rg \
                          更省。\n\
                          例子（读两个文件，只留函数签名）：\n\
                          for p in [\"src/a.rs\", \"src/b.rs\"]:\n\
                          \x20   lines = tools.read(path=p)[\"text\"].splitlines()\n\
                          \x20   text([l for l in lines if \"fn \" in l])\n\
                          脚本跑在沙箱里：不能写文件、不能联网、不能起子进程，只能用标准库\
                          （和受管理环境里装好的库）。\n\
                          - tools.<name>(**args)：调工具，返回 {\"status\", \"text\", \"result\"}；\
                          text 是工具的正文输出（read 是文件内容，rg 是匹配行），result 是结构化\
                          结果。工具名里的 - 写成 _。能调 read、rg 与 MCP 工具（MCP 含写操作）；\
                          shell、python、edit、write 与编排工具（delegate、dispatch、monitor 等）\
                          只能在脚本外直接调，脚本里写了 tools.shell(...) 会在运行前被拒。\
                          子调用逐次持久化，unsafe 中断不重跑；恢复核对\
                          已记录调用及参数，禁止新增调用，分歧或结果不明会终止整段脚本。\n\
                          - text(value)：加一段输出（非字符串转 JSON）。print 的内容附在最后。\n\
                          - TOOLS：能调的工具名。\n\
                          - ALL_TOOLS（[{name, description}]）、search_tools(query, limit=8, \
                          namespace=None)、describe_tool(name)（说明与参数 schema）、\
                          describe_namespace(name)（一台 MCP 服务器的说明、instructions 与工具名）：\
                          找没声明给你的工具，比如 MCP 工具。\n\
                          一段脚本最多调 256 次工具；完整输出会落盘，给模型的视图统一受结果预算限制。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Python 源码（模块级代码）" }
                },
                "required": ["code"],
                "additionalProperties": false
            }),
        }
    }

    /// 宿主 journal 保证已交付的子结果复用；unsafe 子调用中断与分歧停止恢复。
    fn replay(&self) -> Replay {
        Replay::Safe
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let args: CodemodeArgs = parse_args(args, CODEMODE_TOOL)?;
        if args.code.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                message: "code 不能是空的".into(),
            });
        }
        let refused = refused_calls(&args.code);
        if !refused.is_empty() {
            return Err(ToolError::InvalidArguments {
                message: format!(
                    "脚本里调不了 {}：脚本没有运行。脚本里只能调 read、rg 与 MCP 工具；\
                     这几个请在脚本外直接调，读和汇总的部分再放进脚本",
                    refused.join("、")
                ),
            });
        }
        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: CODEMODE_TOOL.into(),
            operation: Operation::Codemode {
                code: args.code.clone(),
            },
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![],
            versions: PlanVersions::default(),
            // Safe 表示可重新进入 journal 协议，不表示可重做全部子调用。
            recovery: RecoveryMode::SafeReread,
            then_run: None,
        })
    }

    async fn execute(
        &self,
        _plan: komo_kernel::types::plan::ExecutionPlan,
        _ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed {
            message: "codemode 由 executor 执行：这台 Gateway 没有接上沙箱".into(),
        })
    }
}

/// 脚本正文里出现的 `tools.<名字>(`，名字在 [`NOT_IN_SCRIPTS`] 里的那些（去重、按出现顺序）。
///
/// 只认字面写法，不解析 Python：`getattr(tools, "shell")` 这种绕法认不出，执行器照样会在
/// 那一次子调用上拒。前面紧挨着标识符字符的不算（`mytools.shell(` 不是我们的 `tools`）。
fn refused_calls(code: &str) -> Vec<&'static str> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out: Vec<&'static str> = Vec::new();
    for (at, _) in code.match_indices("tools.") {
        if code[..at].chars().next_back().is_some_and(is_ident) {
            continue;
        }
        let rest = &code[at + "tools.".len()..];
        let name_len = rest.find(|c: char| !is_ident(c)).unwrap_or(rest.len());
        let (name, after) = rest.split_at(name_len);
        if !after.trim_start().starts_with('(') {
            continue;
        }
        if let Some(hit) = NOT_IN_SCRIPTS.iter().find(|n| **n == name)
            && !out.contains(hit)
        {
            out.push(hit);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_commands_and_writes_in_a_script_are_found_before_it_runs() {
        let code = "r = tools.rg(pattern='x')\nw = tools.shell (command='git status')\n\
                    tools.edit(path='a', match_text='b', replace_text='c')\ntools.shell(command='ls')";
        assert_eq!(refused_calls(code), vec!["shell", "edit"]);
    }

    #[test]
    fn reads_mcp_calls_and_lookalikes_are_not_refused() {
        let code = "tools.read(path='a')\ntools.mcp__devops_mcp_prod__issue_get_info(key='GD-1')\n\
                    mytools.shell(command='x')\nprint(tools.shell_history)\nTOOLS";
        assert!(refused_calls(code).is_empty());
    }
}
