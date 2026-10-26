//! `codemode`：一段在 Monty 里跑的 Python 脚本（`docs/codemode.md`）。
//!
//! 与 [`super::task`] 同一个模式：这里只有定义与 `prepare`，执行在 executor
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

/// 脚本里调不了的：编排操作与脚本自己。
///
/// 执行器用它把这些名字挡在 `TOOLS` 与子调用之外；`prepare` 在脚本跑起来之前拒绝
/// 字面调用，避免执行到一半才发现不支持、前面的副作用却已经发生。
pub const NOT_IN_SCRIPTS: &[&str] = &["codemode", "task", "monitor", "monitor_stop"];

/// `tool://codemode/doc` 的长说明：不进工具定义（`ToolDefinition::doc`）。
const CODEMODE_DOC: &str = "\
- 工具发现：ALL_TOOLS（变量，不是函数：[{name, description}] 列表）、\
search_tools(query, limit=8, namespace=None)、describe_tool(name)（说明与参数 schema）、\
describe_namespace(name)（一台 MCP 服务器的说明、instructions 与工具名）：\
找没声明给你的工具，比如 MCP 工具。\n\
- read、rg、edit、write、bash 与 MCP 可在同一脚本里按顺序调用；批量跨文件可用 for 循环。\n\
- 并行：atools.<name>(**args) 调用时就发出去、返回 awaitable（像 JS 的 promise），\
脚本里可以直接在顶层 await。r = await settle(atools.read(path=a), atools.bash(command=b)) \
照 Promise.allSettled：按参数顺序给 [{\"ok\": True, \"value\": 工具结果} | {\"ok\": False, \"error\": \"..\"}]，\
一项抛 ToolError 不影响其他项。import asyncio 后 await asyncio.gather(...) 照 Promise.all：\
任一项抛错就整体抛出（没有 return_exceptions）。只并行彼此独立的调用；有先后依赖的用 tools 顺序调。\
并行的结果成批交回：一次 await 要等所有已发出的调用都完成。\
不要并行改同一个文件：后一个会得到版本冲突。\n\
- 同一文件改多处用一次 tools.edit(path=..., edits=[{\"match_text\": ..., \"replace_text\": ...}])；\
write 覆盖已有文件时，用 read 或上次写入回执的 result.version 作为 expected_version。\n\
- 每个子调用独立提交，已完成的修改不会因后续失败回滚；需要继续处理其他项时逐项捕获 ToolError。\
脚本里不支持 then_run，编辑成功后单独 tools.bash(command=...)。\n\
- 子调用逐次持久化，unsafe 中断不重跑；中断后重跑脚本，已记录的调用原样回放，\
之后的调用照常执行，分歧或结果不明会终止整段脚本。\n\
- bash 的 text 受结果预算限制，放不下时只给开头预览并写明裁了多少，完整输出按 artifact:// 引用用 read 取回；\
非零退出正常返回 status=failed 与 result.exit_code。\n\
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
            description: "写一段 Python 脚本，在脚本里调其他工具、在本地过滤和汇总，只有脚本的\
                          输出回到你这里。适合：一次批量读写或编辑多个文件、从大段结果里挑出要的\
                          那几行、把几处结果拼成一张表。\n\
                          例子（读两个文件，只留函数签名）：\n\
                          for p in [\"src/a.rs\", \"src/b.rs\"]:\n\
                          \x20   r = tools.read(path=p)\n\
                          \x20   if r[\"status\"] == \"completed\":\n\
                          \x20       text([l for l in r[\"text\"].splitlines() if \"fn \" in l])\n\
                          \x20   else:\n\
                          \x20       text({\"path\": p, \"status\": r[\"status\"], \"error\": r[\"text\"]})\n\
                          脚本跑在 Monty（一个受限的 Python 子集解释器）里：用来汇总和过滤工具\
                          结果。文件可以只读（open() / pathlib）；不能写文件、不能联网、不能起子进程、\
                          读不到环境变量，只有 json、re、math、time、datetime、collections、itertools \
                          等少数标准库，没有第三方库。要写文件用 write / edit，要完整的 Python \
                          （第三方库、写文件、联网）用 bash 跑 python3。每次调用都是独立脚本，\
                          json、re、time 等必须在本段显式 import。json 只有 loads / dumps，\
                          读 JSON 文件写 json.loads(open(p).read())。datetime.fromtimestamp / \
                          utcfromtimestamp 未实现；Unix 秒转 UTC 用：\n\
                          import time\n\
                          ts = 1729324800.25\n\
                          text(time.strftime(\"%Y-%m-%d %H:%M:%S\", time.gmtime(float(ts))))\n\
                          固定 UTC+8 在 gmtime 入参加 8*3600；只展示到秒，不处理地区夏令时。\n\
                          - tools.<name>(**args)：调工具，返回 {\"status\", \"text\", \"result\"}；\
                          text 是工具的正文输出（read 是文件内容，rg 是匹配行），result 是结构化\
                          结果。先检查 status；failed 也可能正常返回字典，并不自动抛 ToolError。\
                          MCP 的 result 是原生 CallToolResult，业务数据可能在 structuredContent，\
                          或需 import json 后解析 content 中的 text；不要猜外层有 data。路径先用 \
                          rg / list_files 定位，独立项可逐项捕获 ToolError 并保留成功项。\
                          工具名里的 - 写成 _，写错名字得到 AttributeError，能调的名字见 TOOLS。\
                          能调 read、rg、edit、write、bash 与 MCP 工具（MCP 含写操作）；\
                          编排工具（task、monitor 等）与 codemode 自己只能在脚本外直接调。\n\
                          - text(value)：加一段输出（非字符串转 JSON）。print 的内容附在最后。\n\
                          - atools.<name>(**args)：同上但并行，返回 awaitable；顶层可直接 await。\
                          rs = await settle(atools.read(path=a), atools.rg(pattern=p)) 照 Promise.allSettled，\
                          每项是 {\"ok\": True, \"value\": ..} 或 {\"ok\": False, \"error\": \"..\"}，按参数顺序。\n\
                          - TOOLS：能调的工具名。\n\
                          工具发现（找没声明给你的 MCP 工具）、中断重跑语义、调用次数上限与结果预算\
                          见 tool://codemode/doc。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Python 源码（模块级代码）" }
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

/// 脚本正文里出现的 `tools.<名字>(` / `atools.<名字>(`，名字在 [`NOT_IN_SCRIPTS`] 里的那些
/// （去重、按出现顺序）。
///
/// 只认字面写法，不解析 Python：`getattr(tools, "task")` 这种绕法认不出，执行器照样会在
/// 那一次子调用上拒。前面紧挨着标识符字符的不算（`mytools.bash(` 不是我们的 `tools`）。
fn refused_calls(code: &str) -> Vec<&'static str> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out: Vec<&'static str> = Vec::new();
    for (at, _) in code.match_indices("tools.") {
        let before = code[..at].strip_suffix('a').unwrap_or(&code[..at]);
        if before.chars().next_back().is_some_and(is_ident) {
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
        let code = "tools.edit(path='a', edits=[])\ntools.write(path='b', content='x')\n\
                    tools.task(prompt='x')\ntools.codemode(code='x')\ntools.task(prompt='y')";
        assert_eq!(refused_calls(code), vec!["task", "codemode"]);
    }

    #[test]
    fn the_async_namespace_is_checked_the_same_way() {
        let code =
            "await atools.monitor(x=1)\nr = atools.read(path='a')\nmyatools.task(prompt='x')";
        assert_eq!(refused_calls(code), vec!["monitor"]);
    }

    #[test]
    fn file_tools_bash_mcp_and_lookalikes_are_not_refused() {
        let code = "tools.edit(path='a', edits=[])\ntools.write(path='b', content='x')\n\
                    tools.rg(pattern='x')\ntools.bash(command='echo hi')\ntools.read(path='a')\ntools.mcp__devops_mcp_prod__issue_get_info(key='GD-1')\n\
                    mytools.bash(command='x')\nprint(tools.bash_history)\nTOOLS";
        assert!(refused_calls(code).is_empty());
    }
}
