//! `codemode`：一段在操作系统沙箱里跑的 Python 脚本（`docs/codemode.md`）。
//!
//! 与 [`super::dispatch`] 同一个模式：这里只有定义与 `prepare`，执行在 executor
//! （`Operation::Codemode` 分流到 `run_codemode`）——脚本里的工具调用要回到执行器
//! 判定，工具自己够不到它。

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanVersions, RecoveryMode};
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::{normalized, parse_args, plan_time};

pub const CODEMODE_TOOL: &str = "codemode";

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
                          那几行、把几处结果拼成一张表。\n\
                          脚本跑在沙箱里：不能写文件、不能联网、不能起子进程，只能用标准库\
                          （和受管理环境里装好的库）。\n\
                          - tools.<name>(**args)：调工具，返回 {\"status\", \"text\", \"result\"}；\
                          text 是工具的正文输出（read 是文件内容，rg 是匹配行），result 是结构化\
                          结果。工具名里的 - 写成 _。**只能调只读的工具**（read、rg、声明为只读的 \
                          MCP 工具），会改东西或要审批的调用会抛 ToolError，那种请在脚本外直接调。\n\
                          - text(value)：加一段输出（非字符串转 JSON）。print 的内容附在最后。\n\
                          - TOOLS：能调的工具名。\n\
                          一段脚本最多调 256 次工具，输出超过 1 MiB 会被截断。"
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
            resources: vec![],
            // 里面只有只读调用：中断后整段重跑与重读同理（docs/codemode.md §7）。
            recovery: RecoveryMode::SafeReread,
        })
    }

    async fn execute(
        &self,
        _plan: komo_kernel::types::plan::ApprovedPlan,
        _ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed {
            message: "codemode 由 executor 执行：这台 Gateway 没有接上沙箱".into(),
        })
    }
}
