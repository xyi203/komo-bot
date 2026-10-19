//! `task`：把一件耗时的事交给一条**独立的任务会话**在后台跑（`docs/background-tasks.md`，
//! `docs/komo_bot.md` §4）。2026-10-16 起取代 `dispatch` / `follow` 与同步的 `delegate`。
//!
//! 不带 `task_id` 是新派：建任务会话、提交第一条输入；带 `task_id` 是接着做：提交进那个已有
//! 的任务会话。两种都不等任务跑完，同一会话严格串行，等它就把用户后面的每一句都堵住了。
//!
//! 只给 `task_id`、不给 `prompt` 是第三种：查进度（`Operation::TaskStatus`）。返回那个任务
//! 最近一条 Run 的快照，什么都不提交，不打扰它。
//!
//! 两件**不在这里**发生的事：
//!
//! - **执行不在工具里。** 建任务会话、提交输入在 executor（`Operation::Task` 的分流），
//!   因为那要一份 [`komo_kernel::traits::TaskSpawner`]——工具只有
//!   [`komo_kernel::types::tool::ToolContext`]，够不到它。
//! - **结果不在这里交回。** 任务的 Run 进终态时 Gateway 把结果当作一条内部输入交回派它的
//!   会话。
//!
//! `model` / `effort` 在这里按**当前配置快照**校验（§13.3），可选的 alias 以 enum 写进
//! schema；只给 `effort` 不给 `model` 时，所选模型（Agent 的默认，或那个任务上一条 Run 的）
//! 只有执行侧知道，档位在那边校验。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::delegate::{DelegateContract, SchemaMode};
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::model::Effort;
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanSource, PlanVersions, RecoveryMode};
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::{normalized, parse_args, plan_time};
use crate::config::ConfigHolder;

/// 模型给的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArgs {
    /// 自包含的任务描述：**任务会话看不到派它的那段对话**。只给 `task_id` 而空着它 = 查进度。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    /// 给人看的标题，新派时必填（接着做时任务已经有标题，不用给）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 接着做哪一个任务（派出时给的短号）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// 结果契约：一份 JSON Schema（kernel 认得的那个子集）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<serde_json::Value>,
}

pub struct TaskTool {
    config: Arc<ConfigHolder>,
}

impl std::fmt::Debug for TaskTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskTool").finish_non_exhaustive()
    }
}

impl TaskTool {
    pub fn new(config: Arc<ConfigHolder>) -> Self {
        Self { config }
    }

    /// 当前配置里可选的 completion alias。
    fn aliases(&self) -> Vec<String> {
        self.config
            .current()
            .model_catalog
            .completions()
            .map(|(alias, _)| alias.to_string())
            .collect()
    }

    /// `model` 认不认识、`effort` 在那个模型上立不立得住。只给 `effort` 时放过（见模块文档）。
    fn check_model(&self, model: &str, effort: Option<&Effort>) -> Result<(), ToolError> {
        let config = self.config.current();
        let Some(resolved) = config.model_catalog.completion(model) else {
            return Err(ToolError::InvalidArguments {
                message: format!(
                    "model `{model}` 不是可选的模型；可选值：{}",
                    self.aliases().join("、")
                ),
            });
        };
        if let Some(effort) = effort {
            let mut probe = resolved.clone();
            probe.effort = Some(effort.clone());
            self.config
                .caps()
                .check(&probe)
                .map_err(|problem| ToolError::InvalidArguments {
                    message: format!("effort 立不住：{problem}"),
                })?;
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn definition(&self) -> ToolDefinition {
        let mut model = serde_json::json!({
            "type": "string",
            "description": "任务用哪个模型（配置里的 alias）。省略：新任务用你的默认模型，\
                            接着做沿用那个任务上一次用的"
        });
        let aliases = self.aliases();
        if !aliases.is_empty() {
            model["enum"] = serde_json::json!(aliases);
        }
        ToolDefinition {
            name: "task".into(),
            description: "派一个后台任务：建一个独立的任务会话去做要跑很久的事（长时间的排查、\
                          批量操作、要等外部设备的事），你自己不等它，这一轮照常往下。\
                          任务会话**看不到这段对话**，所以 prompt 要自包含——把用户的原话和已知的\
                          结论都写进去。提交成功就告诉用户\"已派出 #短号：标题\"。任务结束时，\
                          结果会作为一条以 [后台任务 #短号…] 开头的消息交回给你，用户看不到\
                          那条消息，由你把结论转告用户。同一轮派几个就是几个在并行，各自交回。\
                          几秒钟能做完的事直接做，不要派。\n\
                          追问一个派出去的任务：带上 task_id（派出时给的短号，例如 3f2a），prompt \
                          作为新的一句话提交进它自己的会话，它带着自己查到的东西接着做，不要为同一件\
                          事另派一个。要结构化的结果就给 contract（一份 JSON Schema），任务的最后\
                          回复会按它复验，复验结论随结果一起交回。\n\
                          只给 task_id、不给 prompt 是查进度：返回那个任务现在在跑什么、跑了多久、\
                          最近的输出，不打扰它；用户问进度时先查再答，别凭印象。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "交给任务会话的自包含任务；它看不到这段对话，所以别写\"见上文\"。\
                                        只查进度时不给（只给 task_id）"
                    },
                    "title": {
                        "type": "string",
                        "description": "给人看的标题，建议不超过 30 字；新派时必填"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "接着做或查进度的任务：派出时给的短号（例如 3f2a）；不给就是新派一个。\
                                        只给它不给 prompt = 查进度"
                    },
                    "model": model,
                    "effort": {
                        "type": "string",
                        "description": "推理档位，按所选模型支持的档位校验；省略同 model"
                    },
                    "contract": {
                        "type": "object",
                        "description": "结果契约（JSON Schema 的子集：type / required / properties / \
                                        items / enum / additionalProperties）。任务的最后一条回复\
                                        要给一个符合它的 JSON 对象"
                    }
                },
                "required": [],
                "additionalProperties": false
            }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        // Cron 触发的 Run 没有对端也没有看客：交回 Cron 会话的那条结果，回复投不到任何人。
        if let PlanSource::Cron { .. } = ctx.source {
            return Err(ToolError::Denied {
                reason: "Cron 任务本来就在后台跑，不能再派后台任务：自己把这件事做完".into(),
            });
        }
        let args: TaskArgs = parse_args(args, "task")?;
        let status_query = args.prompt.trim().is_empty() && args.task_id.is_some();
        if args.prompt.trim().is_empty() && !status_query {
            return Err(ToolError::InvalidArguments {
                message: "prompt 不能是空的：任务会话只看得见它，空任务等于派出去一个什么都做不了\
                          的会话；只想查某个任务的进度就只给 task_id"
                    .into(),
            });
        }
        // `#3f2a` 这种带井号的写法也收——模型把回执里的短号原样抄过来时常带着它。
        let task_id = match &args.task_id {
            Some(raw) => {
                let id = raw.trim().trim_start_matches('#').to_string();
                if id.is_empty() {
                    return Err(ToolError::InvalidArguments {
                        message: "task_id 不能是空的：接着做总得说清是哪一个；新派就别给它".into(),
                    });
                }
                Some(id)
            }
            None => None,
        };
        if status_query {
            if args.title.is_some()
                || args.model.is_some()
                || args.effort.is_some()
                || args.contract.is_some()
            {
                return Err(ToolError::InvalidArguments {
                    message:
                        "查进度只要 task_id：不给 prompt 时 title / model / effort / contract \
                              都不该出现；要给任务派活就写 prompt"
                            .into(),
                });
            }
            let Some(task_id) = task_id else {
                unreachable!("status_query 蕴含 task_id")
            };
            return Ok(ExecutionPlan {
                operation_id: OperationId::new_at(plan_time()),
                source: ctx.source.clone(),
                tool: "task".into(),
                operation: Operation::TaskStatus { task_id },
                run: Some(ctx.run.clone()),
                tool_call: Some(ctx.call.clone()),
                args: normalized(&args)?,
                cwd: Some(ctx.cwd.clone()),
                targets: vec![],
                versions: PlanVersions::default(),
                // 纯读我们自己的账本，重放一遍没有副作用。
                recovery: RecoveryMode::SafeReread,
                then_run: None,
            });
        }
        let title = match (&task_id, args.title.as_deref().map(str::trim)) {
            (Some(_), _) => None,
            (None, Some(title)) if !title.is_empty() => Some(title.to_string()),
            (None, _) => {
                return Err(ToolError::InvalidArguments {
                    message: "新派任务要给 title：它是任务在回执与结果里的名字".into(),
                });
            }
        };
        let model = args.model.as_deref().map(str::trim).map(str::to_string);
        if let Some(model) = &model {
            self.check_model(model, args.effort.as_ref())?;
        }
        let contract = match &args.contract {
            Some(schema) if schema.is_object() => Some(DelegateContract {
                schema: schema.clone(),
                mode: SchemaMode::Permissive,
            }),
            Some(_) => {
                return Err(ToolError::InvalidArguments {
                    message: "contract 要是一个 JSON Schema 对象".into(),
                });
            }
            None => None,
        };

        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "task".into(),
            operation: Operation::Task {
                prompt: args.prompt.clone(),
                title,
                task_id,
                model,
                effort: args.effort.clone(),
                contract,
            },
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            // 派任务不碰任何文件：它触及的目标是"一个会话"，不是路径。
            targets: vec![],
            versions: PlanVersions::default(),
            // 核对对象是我们自己的账本（幂等的请求键有没有对应的 Run），§8.6。
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
            message: "task 不由工具执行：一次派任务要么被 executor 的编排受理并立刻收尾，\
                      要么根本不该走到执行。"
                .into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testing::Fixture;
    use crate::tools::test_support::{context, writer};

    fn tool(fixture: &Fixture) -> TaskTool {
        TaskTool::new(Arc::new(fixture.holder()))
    }

    #[tokio::test]
    async fn a_new_task_carries_its_prompt_title_and_overrides() {
        let fixture = Fixture::valid();
        let dir = tempfile::tempdir().unwrap();
        let plan = tool(&fixture)
            .prepare(
                serde_json::json!({
                    "prompt": "查一下空调状态",
                    "title": "查空调",
                    "model": "chat",
                    "effort": "low",
                    "contract": { "type": "object", "required": ["on"] }
                }),
                &context(dir.path()),
            )
            .await
            .unwrap();

        let Operation::Task {
            prompt,
            title,
            task_id,
            model,
            effort,
            contract,
        } = &plan.operation
        else {
            panic!("{:?}", plan.operation)
        };
        assert_eq!(prompt, "查一下空调状态");
        assert_eq!(title.as_deref(), Some("查空调"));
        assert!(task_id.is_none());
        assert_eq!(model.as_deref(), Some("chat"));
        assert_eq!(effort.as_ref().map(Effort::as_str), Some("low"));
        assert_eq!(
            contract.as_ref().unwrap().schema["required"],
            serde_json::json!(["on"])
        );
        assert!(plan.targets.is_empty());
        assert_eq!(plan.recovery, RecoveryMode::VerifyTarget);
    }

    /// 接着做：短号去掉井号，标题不用给。
    #[tokio::test]
    async fn following_up_takes_a_task_id_and_needs_no_title() {
        let fixture = Fixture::valid();
        let dir = tempfile::tempdir().unwrap();
        let plan = tool(&fixture)
            .prepare(
                serde_json::json!({ "prompt": "再看看功耗", "task_id": "#3f2a" }),
                &context(dir.path()),
            )
            .await
            .unwrap();
        let Operation::Task { task_id, title, .. } = &plan.operation else {
            panic!("{:?}", plan.operation)
        };
        assert_eq!(task_id.as_deref(), Some("3f2a"));
        assert!(title.is_none());
    }

    #[tokio::test]
    async fn an_empty_prompt_or_a_missing_title_is_refused() {
        let fixture = Fixture::valid();
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(&fixture);
        for args in [
            serde_json::json!({ "prompt": "  ", "title": "标题" }),
            serde_json::json!({ "prompt": "干活" }),
            serde_json::json!({ "prompt": "干活", "title": " " }),
            serde_json::json!({ "prompt": "干活", "task_id": " # " }),
        ] {
            let error = tool
                .prepare(args.clone(), &context(dir.path()))
                .await
                .unwrap_err();
            assert!(
                matches!(error, ToolError::InvalidArguments { .. }),
                "{args}：{error:?}"
            );
        }
    }

    /// 查进度：只给 task_id，计划是只读的 `TaskStatus`，参数里没有 prompt。
    #[tokio::test]
    async fn a_task_id_without_prompt_is_a_status_query() {
        let fixture = Fixture::valid();
        let dir = tempfile::tempdir().unwrap();
        let plan = tool(&fixture)
            .prepare(
                serde_json::json!({ "task_id": "#3f2a" }),
                &context(dir.path()),
            )
            .await
            .unwrap();
        let Operation::TaskStatus { task_id } = &plan.operation else {
            panic!("{:?}", plan.operation)
        };
        assert_eq!(task_id, "3f2a");
        assert_eq!(plan.recovery, RecoveryMode::SafeReread);
        assert!(plan.targets.is_empty());
        assert!(plan.args.get("prompt").is_none(), "{}", plan.args);
        assert_eq!(plan.args["task_id"], "#3f2a");
    }

    /// 查进度不带别的字段：带了就是没想清楚是查还是派，拒绝而不是悄悄丢掉。
    #[tokio::test]
    async fn a_status_query_with_extra_fields_is_refused() {
        let fixture = Fixture::valid();
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(&fixture);
        for args in [
            serde_json::json!({ "task_id": "3f2a", "title": "t" }),
            serde_json::json!({ "task_id": "3f2a", "prompt": " ", "model": "chat" }),
            serde_json::json!({ "task_id": "3f2a", "effort": "low" }),
            serde_json::json!({ "task_id": "3f2a", "contract": { "type": "object" } }),
            serde_json::json!({ "task_id": " # " }),
        ] {
            let error = tool
                .prepare(args.clone(), &context(dir.path()))
                .await
                .unwrap_err();
            assert!(
                matches!(error, ToolError::InvalidArguments { .. }),
                "{args}：{error:?}"
            );
        }
    }

    /// 不认识的 alias 在 `prepare` 被拒，理由里列出可选值；enum 也写进了 schema。
    #[tokio::test]
    async fn an_unknown_model_is_refused_listing_the_aliases() {
        let fixture = Fixture::valid();
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(&fixture);
        let error = tool
            .prepare(
                serde_json::json!({ "prompt": "干活", "title": "t", "model": "gpt-9" }),
                &context(dir.path()),
            )
            .await
            .unwrap_err();
        let ToolError::InvalidArguments { message } = &error else {
            panic!("{error:?}")
        };
        assert!(
            message.contains("gpt-9") && message.contains("chat"),
            "{message}"
        );

        let schema = tool.definition().parameters;
        let aliases = schema["properties"]["model"]["enum"].as_array().unwrap();
        assert!(aliases.contains(&serde_json::json!("chat")), "{schema}");
        assert!(
            !aliases.contains(&serde_json::json!("embedding")),
            "向量模型不是可选的：{schema}"
        );
    }

    #[tokio::test]
    async fn an_effort_the_model_does_not_support_is_refused() {
        let fixture = Fixture::valid();
        let dir = tempfile::tempdir().unwrap();
        let error = tool(&fixture)
            .prepare(
                serde_json::json!({
                    "prompt": "干活", "title": "t", "model": "chat", "effort": "ultra"
                }),
                &context(dir.path()),
            )
            .await
            .unwrap_err();
        let ToolError::InvalidArguments { message } = &error else {
            panic!("{error:?}")
        };
        assert!(message.contains("ultra"), "{message}");
    }

    /// Cron 触发的 Run 不能派任务：交回 Cron 会话的结果没有人看（§4）。
    #[tokio::test]
    async fn a_cron_run_cannot_spawn_a_task() {
        let fixture = Fixture::valid();
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        ctx.source = PlanSource::Cron {
            job: komo_kernel::types::ids::CronJobId::from_raw("job-1"),
            job_version: 1,
        };
        let error = tool(&fixture)
            .prepare(serde_json::json!({ "prompt": "干活", "title": "t" }), &ctx)
            .await
            .unwrap_err();
        let ToolError::Denied { reason } = &error else {
            panic!("{error:?}")
        };
        assert!(reason.contains("Cron"), "{reason}");
    }

    #[tokio::test]
    async fn execute_refuses_because_task_is_orchestrated_not_executed() {
        let fixture = Fixture::valid();
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = tool(&fixture);
        let plan = tool
            .prepare(serde_json::json!({ "prompt": "干活", "title": "t" }), &ctx)
            .await
            .unwrap();
        let error = tool
            .execute(plan, &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        let ToolError::Failed { message } = &error else {
            panic!("{error:?}")
        };
        assert!(message.contains("task"), "{message}");
    }
}
