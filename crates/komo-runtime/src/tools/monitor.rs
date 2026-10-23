//! `monitor` / `monitor_stop`：挂一个长时间跑的脚本，它往 stdout 每打一行，就作为一条
//! 内部输入交回这个会话（`docs/monitor.md`）。
//!
//! 与 [`super::task`] 同一类编排操作：工具只做 `prepare`，起进程、逐行交回都在
//! Gateway 的 `Monitors` 实现里——进程要活得比这次调用、这条 Run 都久，工具与执行器都
//! 拿不住它。`monitor` 提交成功就收尾，不等脚本跑完。

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::monitor::{MAX_MONITOR_MINUTES, clamp_minutes};
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanVersions, RecoveryMode};
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::{normalized, parse_args, plan_time};

/// `monitor` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorArgs {
    /// 交给 `bash -c` 的命令。
    pub command: String,
    /// 给人看的标题。
    pub title: String,
    /// 工作目录；相对路径按会话工作目录解析。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 最长跑多久，分钟。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<u64>,
    /// 脚本只读、重跑无副作用：Gateway 重启后自动重挂。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resume_after_restart: bool,
}

#[derive(Debug, Default)]
pub struct MonitorTool;

impl MonitorTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for MonitorTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "monitor".into(),
            description:
                "在后台挂一个脚本盯着某件事（流水线、工单、日志报错、指标阈值），你不用等，\
                这一轮可以直接回复用户。脚本往 stdout 每打一行就是一个事件，以 [监听 #短号…] 开头的\
                消息交回给你；用户看不到它，需要告知的由你转告。\n\
                写脚本：① 只打印要处理的行，先过滤（每行都会让你跑一轮）；② 管道每段逐行刷新\
                （grep --line-buffered、awk fflush()，别用 head）；③ 失败状态（failed / canceled / \
                Error / 崩溃）也要打印；④ 有终点的事让脚本到终态自己退出。对话会话里等状态一律挂\
                监听，后台任务会话里别挂。\n\
                用前先 read tool://monitor/doc。"
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "交给 bash -c 的脚本；只有 stdout 的行算事件"
                    },
                    "title": {
                        "type": "string",
                        "description": "给人看的标题，例如「MR !123 流水线」，建议不超过 30 字"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "工作目录；相对路径按会话工作目录解析"
                    },
                    "timeout_minutes": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "最长跑多久，分钟；省略 = 30"
                    },
                    "resume_after_restart": {
                        "type": "boolean",
                        "description": "脚本只读、重跑无副作用时设 true：Gateway 重启后自动重挂；默认 false"
                    }
                },
                "required": ["command", "title"],
                "additionalProperties": false
            }),
            doc: Some(format!(
                "- 事件：几秒内连着出的行合成一条；你处理上一条期间出的行会合进下一条。脚本退出、\
                 超时或被停时还会交回一条结束消息。\n\
                 - 轮询循环里 sleep 不要短于 15 秒。对话会话里 bash 里 10 秒以上的 sleep 会被拒绝，\
                 等状态（流水线、工单、部署进度）挂监听，脚本只在完成或出错时打一行；构建、下载这类\
                 慢命令照常交给 bash（跑久了自动转后台）。后台任务会话里别挂：任务这一轮一结束，\
                 结果就交回去了。\n\
                 - timeout_minutes 省略 = 30，上限 {MAX_MONITOR_MINUTES}。\n\
                 - Gateway 重启时监听会中断：脚本只读、重跑一遍没有副作用（轮询状态、查进度、\
                 tail 日志）就设 resume_after_restart = true，重启后按原短号自动重挂、剩余时限\
                 接着算，不用你再挂；会改东西的脚本（发消息、写文件、触发操作）别设，中断时会\
                 通知你。"
            )),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let args: MonitorArgs = parse_args(args, "monitor")?;
        if args.command.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                message: "command 不能是空的：监听要有一个跑着的脚本".into(),
            });
        }
        if args.title.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                message: "title 不能是空的：它是监听在每条事件里的名字".into(),
            });
        }
        let cwd = match &args.cwd {
            Some(raw) => super::paths::resolve(raw, &ctx.cwd)?,
            None => ctx.cwd.clone(),
        };

        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "monitor".into(),
            operation: Operation::Monitor {
                command: args.command.clone(),
                title: args.title.trim().to_string(),
                timeout_minutes: clamp_minutes(args.timeout_minutes),
                resume_after_restart: args.resume_after_restart,
            },
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(cwd),
            targets: vec![],
            versions: PlanVersions::default(),
            // 核对对象是 Gateway 手里的监听登记（同一个请求键还在不在跑），与 `dispatch`
            // 同一类；认不到就如实说"中断了"，不重新起进程。
            recovery: RecoveryMode::VerifyTarget,
            then_run: None,
        })
    }

    async fn execute(
        &self,
        _plan: ExecutionPlan,
        _ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed {
            message: "monitor 不由工具执行：挂监听要么被 executor 的编排受理并立刻收尾，\
                      要么根本不该走到执行。"
                .into(),
        })
    }
}

/// `monitor_stop` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorStopArgs {
    /// 监听短号（挂上时给的那四位）。
    pub monitor_id: String,
}

#[derive(Debug, Default)]
pub struct MonitorStopTool;

impl MonitorStopTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for MonitorStopTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "monitor_stop".into(),
            description: "停掉这个会话挂着的一个监听（终止它的整个进程组）。monitor_id 是挂上时\
                          给的短号（例如 3f2a）；给错了会列出还在跑的那些。停掉之后不会再交回\
                          结束消息。它已经自己结束了也没关系：会告诉你怎么结束的，\
                          结束消息照常单独交回。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "monitor_id": {
                        "type": "string",
                        "description": "监听短号，例如 3f2a"
                    }
                },
                "required": ["monitor_id"],
                "additionalProperties": false
            }),
            doc: None,
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let args: MonitorStopArgs = parse_args(args, "monitor_stop")?;
        let monitor_id = args.monitor_id.trim().trim_start_matches('#').to_string();
        if monitor_id.is_empty() {
            return Err(ToolError::InvalidArguments {
                message: "monitor_id 不能是空的：总得说清停哪一个".into(),
            });
        }
        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "monitor_stop".into(),
            operation: Operation::MonitorStop { monitor_id },
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![],
            versions: PlanVersions::default(),
            recovery: RecoveryMode::VerifyTarget,
            then_run: None,
        })
    }

    async fn execute(
        &self,
        _plan: ExecutionPlan,
        _ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed {
            message: "monitor_stop 不由工具执行：它只由 executor 的编排处理。".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{context, writer};

    #[tokio::test]
    async fn the_plan_carries_the_command_title_and_clamped_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());

        let plan = MonitorTool::new()
            .prepare(
                serde_json::json!({
                    "command": "glab ci status --live",
                    "title": " MR !12 流水线 ",
                    "timeout_minutes": 100_000
                }),
                &ctx,
            )
            .await
            .unwrap();

        let Operation::Monitor {
            command,
            title,
            timeout_minutes,
            resume_after_restart,
        } = &plan.operation
        else {
            panic!("{:?}", plan.operation)
        };
        assert_eq!(command, "glab ci status --live");
        assert_eq!(title, "MR !12 流水线");
        assert_eq!(*timeout_minutes, MAX_MONITOR_MINUTES);
        assert!(!resume_after_restart, "默认不自动重挂");
        assert_eq!(plan.cwd.as_deref(), Some(ctx.cwd.as_path()));
        assert_eq!(plan.recovery, RecoveryMode::VerifyTarget);
    }

    /// 旧计划没有 `resume_after_restart`：解码成 false，再编码一个字节不差（计划哈希不变）；
    /// 声明了的才进 JSON。
    #[tokio::test]
    async fn resume_after_restart_is_opt_in_and_old_plans_keep_their_bytes() {
        let old = serde_json::json!({
            "kind": "monitor", "command": "glab ci status", "title": "t", "timeout_minutes": 30
        });
        let decoded: Operation = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), old);

        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let plan = MonitorTool::new()
            .prepare(
                serde_json::json!({ "command": "glab ci status", "title": "t", "resume_after_restart": true }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(
            plan.operation,
            Operation::Monitor {
                resume_after_restart: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn an_empty_command_or_title_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = MonitorTool::new();

        for args in [
            serde_json::json!({ "command": " ", "title": "t" }),
            serde_json::json!({ "command": "echo hi", "title": "" }),
        ] {
            let error = tool.prepare(args, &ctx).await.unwrap_err();
            assert!(
                matches!(error, ToolError::InvalidArguments { .. }),
                "{error:?}"
            );
        }
    }

    #[tokio::test]
    async fn stop_takes_the_short_id_with_or_without_a_hash() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());

        let plan = MonitorStopTool::new()
            .prepare(serde_json::json!({ "monitor_id": "#3f2a" }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            plan.operation,
            Operation::MonitorStop {
                monitor_id: "3f2a".into()
            }
        );
    }

    /// 两个工具都只由编排处理，执行这条路不该存在。
    #[tokio::test]
    async fn execute_refuses_because_monitors_are_orchestrated() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let plan = MonitorTool::new()
            .prepare(
                serde_json::json!({ "command": "echo hi", "title": "t" }),
                &ctx,
            )
            .await
            .unwrap();

        let error = MonitorTool::new()
            .execute(plan, &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Failed { .. }), "{error:?}");
    }
}
