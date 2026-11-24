//! `codemode`：一段在 djs（JavaScript / TypeScript 沙箱）里跑的脚本（`docs/codemode.md`）。
//!
//! 与 [`super::task`] 同一个模式：这里只有定义与 `prepare`（含字面编排调用拒绝与编译检查），
//! 执行在 harness（`Operation::Codemode` 分流到 `durable::harness::codemode`）——脚本里的
//! 工具调用要回到宿主判定并建嵌套任务，工具自己够不到它。

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanVersions, RecoveryMode, Replay};
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::{normalized, parse_args, plan_time};

pub const CODEMODE_TOOL: &str = "codemode";

/// 脚本里调不了的：编排操作与脚本自己。
///
/// 执行器用它把这些名字挡在 `TOOLS` 与子调用之外；`prepare` 在脚本跑起来之前拒绝
/// 字面调用，避免执行到一半才发现不支持、前面的副作用却已经发生。
pub const NOT_IN_SCRIPTS: &[&str] = &["codemode", "task", "monitor", "monitor_stop"];

/// `tool://codemode/doc` 的长说明：不进工具定义（`ToolDefinition::doc`）。
const CODEMODE_DOC: &str = "\
- 工具发现：ALL_TOOLS（数组，[{name, description}]）、\
searchTools(query, limit=8, namespace=null)、describeTool(name)（说明与参数 schema）、\
describeNamespace(name)（一台 MCP 服务器的说明、instructions 与工具名）；也兼容 snake_case 别名 \
search_tools / describe_tool / describe_namespace。\n\
- read、rg、edit、write、bash 与 MCP 可在同一脚本里混合调用；所有 tools.<name>(args) 均为 async 函数\
（返回 Promise），顶层可直接 await。\n\
- 并发：用原生 Promise.all([...]) 或 Promise.allSettled([...]) 并发调用彼此独立的工具；\
支持多跳链式并发（先完成的链立刻继续下一跳，不被慢调用阻塞）。不要并发改同一个文件：后一个会得到版本冲突。\n\
- 同一文件改多处用一次 await tools.edit({ path, edits: [{ match_text, replace_text }] })；\
write 覆盖已有文件时，用 read 或上次写入回执的 result.version 作为 expected_version。\n\
- 每个子调用独立提交，已完成的修改不会因后续失败回滚；需要继续处理其他项时用 Promise.allSettled 或 \
try { ... } catch (e) { if (e instanceof ToolError) ... }。\
脚本里不支持 then_run，编辑成功后单独 await tools.bash({ command })。\n\
- 子调用逐次持久化，unsafe 中断不重跑；中断后重跑脚本，已记录的调用与交付序原样回放，\
之后的调用照常执行，分歧或结果不明会终止整段脚本。\n\
- bash 的 text 受结果预算限制，放不下时只给开头预览并写明裁了多少，完整输出按 artifact:// 引用用 read 取回；\
非零退出正常返回 status='failed' 与 result.exit_code。\n\
- 外层墙钟包含等待子调用的时间；脚本里的 bash 不转后台，长命令可直接在脚本外调用。\n\
- 一段脚本最多调 256 次工具；完整输出会落盘，给模型的视图统一受结果预算限制。";

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
            description: "写一段 TypeScript / JavaScript 脚本（async 函数体，顶层可直接 await 与 return），\
                          在脚本里并发调其他工具、在本地过滤和汇总，只有脚本的输出回到你这里。\
                          适合：并发调用多个 MCP 或文件工具、多跳链式查询、从大段结果里筛出要的内容、把多处结果拼成表。\n\
                          例子（并发读两个文件，只留函数签名）：\n\
                          const paths = [\"src/a.rs\", \"src/b.rs\"];\n\
                          const rs = await Promise.allSettled(paths.map((path) => tools.read({ path })));\n\
                          for (let i = 0; i < rs.length; i++) {\n\
                            const r = rs[i];\n\
                            if (r.status === \"fulfilled\" && r.value.status === \"completed\") {\n\
                              text(r.value.text.split(\"\\n\").filter((l) => l.includes(\"fn \")));\n\
                            } else {\n\
                              text({ path: paths[i], error: r.status === \"rejected\" ? r.reason.message : r.value.text });\n\
                            }\n\
                          }\n\
                          脚本跑在纯内存 djs（现代严格模式 JS + TypeScript 类型擦除）沙箱里：引擎内没有文件、网络、\
                          子进程或环境变量；读文件走 await tools.read({ path })，需要完整本地环境或第三方库用 bash。\n\
                          - await tools.<name>(args)：所有工具统一为 async 函数，参数传单个对象（可省），返回 \
                          { status, text, result }。先检查 status；failed（如 bash 非零退出）正常返回对象，\
                          不自动抛 ToolError。MCP 的 result 是原生 CallToolResult，业务数据可能在 structuredContent，\
                          或需 JSON.parse(content 中的 text)；不要猜外层有 data。\
                          工具名里的 - 写成 _（或用 tools[\"mcp__a-b__c\"]）；写错名字立即抛带候选提示的 ToolError，\
                          可用名字见 TOOLS。能调 read、rg、edit、write、bash 与 MCP 工具（含写操作）；\
                          编排工具（task、monitor 等）与 codemode 自己只能在脚本外直接调。\n\
                          - 并发：直接用 Promise.all / Promise.allSettled（支持多跳链式并发，先完先续跑）。\n\
                          - text(value)：加一段输出（非字符串自动 JSON 缩进 2 格）；也可直接顶层 return 值。\
                          console.log(...) 附在输出末尾。\n\
                          - ToolError：工具调用失败或被拒时抛出的 Error 子类（可用 e instanceof ToolError 判断）。\n\
                          - TOOLS：本次可调的工具名数组。\n\
                          工具发现（searchTools / describeTool / describeNamespace / ALL_TOOLS）、中断重跑语义、\
                          调用次数上限与结果预算见 tool://codemode/doc。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "TypeScript 或 JavaScript 源码（async 函数体，顶层可用 await / return；非 Python）"
                    }
                },
                "required": ["code"],
                "additionalProperties": false
            }),
            doc: Some(CODEMODE_DOC.into()),
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
                    "脚本里调不了 {}：脚本没有运行。脚本里能调 read、rg、edit、write、bash 与 MCP 工具；\
                     编排操作请在脚本外直接调",
                    refused.join("、")
                ),
            });
        }
        if let Err(syntax) = crate::codemode::worker::compile_check(&args.code) {
            return Err(ToolError::InvalidArguments {
                message: format!("脚本编译失败，没有运行：{syntax}"),
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
            message: "codemode 由 executor 执行：这台 Gateway 没有接上 worker".into(),
        })
    }
}

/// 脚本正文里出现的 `tools.<名字>(`，名字在 [`NOT_IN_SCRIPTS`] 里的那些（去重、按出现顺序）。
///
/// 只认字面点访问，不解析 JS：`tools["task"]` 这种绕法认不出，执行器照样会在那一次子调用上拒
/// （同时 `tools` Proxy 里本来也没有这个键）。前面紧挨着标识符字符的不算（`mytools.task(` 不是我们的 `tools`）。
fn refused_calls(code: &str) -> Vec<&'static str> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
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
    fn orchestration_and_recursive_scripts_are_found_before_the_script_runs() {
        let code = "await tools.edit({ path: 'a', edits: [] });\n\
                    await tools.write({ path: 'b', content: 'x' });\n\
                    await tools.task({ prompt: 'x' });\n\
                    await tools.codemode({ code: 'x' });\n\
                    await tools.task({ prompt: 'y' });";
        assert_eq!(refused_calls(code), vec!["task", "codemode"]);
    }

    #[test]
    fn file_tools_bash_mcp_and_lookalikes_are_not_refused() {
        let code = "await tools.edit({ path: 'a', edits: [] });\n\
                    await tools.write({ path: 'b', content: 'x' });\n\
                    await tools.rg({ pattern: 'x' });\n\
                    await tools.bash({ command: 'echo hi' });\n\
                    await tools.read({ path: 'a' });\n\
                    await tools.mcp__devops_mcp_prod__issue_get_info({ key: 'GD-1' });\n\
                    mytools.task({ prompt: 'x' });\n\
                    console.log(tools.task_history);";
        assert!(refused_calls(code).is_empty());
    }
}
