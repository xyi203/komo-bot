//! `edit`：精确匹配替换。**匹配失败返回错误，不做模糊猜测**（§4）。
//!
//! 「不模糊猜测」有两面：找不到要替换的内容是错误，找到**好几处**同样是错误——替换
//! 哪一处需要猜，而猜错和没找到的代价不一样，后者立刻可见，前者要等到有人读那份文件
//! 才发现。想全改就明写 `replace_all`。
//!
//! `verify` 与 `write` 同源（内容哈希），但它需要**替换之后**那份内容的哈希，而那要
//! 读文件才算得出来——所以 `prepare` 把它算好写进规范化参数（`result_version`）。计划
//! 因此绑定了"从这一份内容出发"：文件在审批期间被人动过，重新 prepare 得到的哈希就
//! 不同，旧授权自然覆盖不到（§7.2「批准后再次校验目标和版本，变化则重新评估」）。

use std::path::PathBuf;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{
    ApprovedPlan, ExecutionPlan, Operation, PlanTarget, PlanVersions, RecoveryMode, TargetAccess,
    Verification,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::write::{atomic_replace, target_path, verify_content};
use super::{ExpectedVersion, FileVersion, as_text, current, normalized, parse_args, plan_time};

/// 模型给的参数。`result_version` 是 `prepare` 补上的，模型不填。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditArgs {
    pub path: String,
    /// 要被替换掉的那段内容，逐字匹配。
    pub match_text: String,
    pub replace_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<ExpectedVersion>,
    /// 匹配到多处时全部替换。默认 false：多处即歧义，歧义就报错。
    #[serde(default)]
    pub replace_all: bool,
    /// **prepare 填写**：替换之后文件内容的哈希。`verify` 靠它分辨"已满足"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_version: Option<ContentHash>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditResult {
    pub path: String,
    pub replacements: u64,
    pub version: FileVersion,
}

#[derive(Debug, Default)]
pub struct EditTool;

impl EditTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for EditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".into(),
            description:
                "把文件里逐字匹配的一段内容替换掉。匹配不到或匹配到多处都会报错，不做模糊猜测。"
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径；相对路径按工作目录解析" },
                    "match_text": { "type": "string", "description": "要替换掉的原文，逐字匹配" },
                    "replace_text": { "type": "string", "description": "替换成什么" },
                    "expected_version": {
                        "description": "read 返回的 version（或它的 hash）",
                        "anyOf": [{ "type": "string" }, { "type": "object" }]
                    },
                    "replace_all": { "type": "boolean", "description": "匹配到多处时全部替换；默认报错" }
                },
                "required": ["path", "match_text", "replace_text"],
                "additionalProperties": false
            }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let mut args: EditArgs = parse_args(args, "edit")?;
        // 模型不能自己指定结果版本——那是 prepare 从真实文件算出来的。
        args.result_version = None;
        if args.match_text.is_empty() {
            return Err(ToolError::InvalidArguments {
                message: "match_text 不能是空串：空串在任何位置都匹配".into(),
            });
        }
        let path = super::paths::resolve(&args.path, &ctx.cwd)?;

        // 读元信息与正文是允许的（prepare 不得执行未审核代码，读文件不是执行）。
        let (before_hash, result_version) = match current(&path)? {
            Some((bytes, version)) => {
                let text = as_text(bytes, &path)?;
                let after = apply(&text, &args, &path)?;
                (Some(version.hash), Some(ContentHash::of_str(&after.0)))
            }
            None => (None, None),
        };
        args.result_version = result_version;

        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "edit".into(),
            operation: Operation::WriteFile,
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![PlanTarget {
                path,
                access: TargetAccess::Write,
                // 明确给了就按它；没给就按 prepare 这一刻看到的那份内容。
                expected_version: args
                    .expected_version
                    .as_ref()
                    .map(ExpectedVersion::hash)
                    .or(before_hash),
            }],
            versions: PlanVersions::default(),
            resources: vec![],
            recovery: RecoveryMode::VerifyTarget,
        })
    }

    async fn execute(
        &self,
        plan: ApprovedPlan,
        _ctx: &ToolContext,
        // 文件工具不产生流式输出：结构化结果由 executor 发布。
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let plan = plan.plan();
        let args: EditArgs = parse_args(plan.args.clone(), "edit")?;
        let path: PathBuf = target_path(plan)?;

        let Some((bytes, version)) = current(&path)? else {
            return Err(ToolError::Failed {
                message: format!("{} 不存在，没有可修改的内容", path.display()),
            });
        };
        // 版本核对：参数给了就按它，没给就按**计划里那一份**——`prepare` 从真实文件
        // 算出来的 before-hash。`edit` 与 `write` 在这里不一样：write 覆盖一个已存在
        // 的文件必须由模型明说预期版本（它写的是整份内容，没读过也能写），而 edit 的
        // 前提就是它刚刚匹配到的那份内容，计划已经把它钉住了。
        let expected = args
            .expected_version
            .as_ref()
            .map(ExpectedVersion::hash)
            .or_else(|| {
                plan.targets
                    .first()
                    .and_then(|t| t.expected_version.clone())
            });
        match &expected {
            Some(expected) if expected != &version.hash => {
                return Err(ToolError::VersionConflict {
                    path: format!("{}：预期 {expected}，当前 {}", path.display(), version.hash),
                });
            }
            Some(_) => {}
            // 计划是在文件还不存在时做的，现在它存在了：那是第三种状态。
            None => {
                return Err(ToolError::VersionConflict {
                    path: format!("{} 在计划生成时并不存在，现在却有内容了", path.display()),
                });
            }
        }

        let text = as_text(bytes, &path)?;
        let (after, replacements) = apply(&text, &args, &path)?;
        atomic_replace(&path, after.as_bytes())?;

        let metadata = std::fs::metadata(&path).ok();
        let result = EditResult {
            path: args.path.clone(),
            replacements,
            version: FileVersion::of(after.as_bytes(), metadata.as_ref()),
        };
        Ok(ToolOutput {
            status: ToolResultStatus::Completed,
            result: serde_json::to_value(&result).map_err(|error| ToolError::Failed {
                message: error.to_string(),
            })?,
            exit_code: None,
            artifacts: vec![],
            preview: Some(format!("{} 替换了 {} 处", result.path, result.replacements)),
        })
    }

    async fn verify(
        &self,
        plan: &ExecutionPlan,
        _ctx: &ToolContext,
    ) -> Result<Verification, ToolError> {
        let args: EditArgs = parse_args(plan.args.clone(), "edit")?;
        let path = target_path(plan)?;
        let Some(intended) = args.result_version.clone() else {
            return Ok(Verification::Unknown {
                reason: format!(
                    "{} 的计划里没有替换后的内容哈希，核对不出结论",
                    path.display()
                ),
            });
        };
        let before = plan
            .targets
            .first()
            .and_then(|target| target.expected_version.clone());
        verify_content(&path, &intended, before.as_ref())
    }
}

/// 逐字替换。匹配不到、或匹配到多处而没说要全改，都是错误。
fn apply(text: &str, args: &EditArgs, path: &std::path::Path) -> Result<(String, u64), ToolError> {
    let hits = text.matches(args.match_text.as_str()).count() as u64;
    match hits {
        0 => Err(ToolError::NoMatch {
            path: path.display().to_string(),
        }),
        _ if hits > 1 && !args.replace_all => Err(ToolError::InvalidArguments {
            message: format!(
                "{} 里匹配到 {hits} 处；给一段唯一的上下文，或显式 replace_all",
                path.display()
            ),
        }),
        _ => {
            let out = if args.replace_all {
                text.replace(args.match_text.as_str(), &args.replace_text)
            } else {
                text.replacen(args.match_text.as_str(), &args.replace_text, 1)
            };
            Ok((out, hits))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{approved, context, writer};

    async fn plan_for(
        tool: &EditTool,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ExecutionPlan, ToolError> {
        tool.prepare(args, ctx).await
    }

    #[tokio::test]
    async fn an_exact_match_is_replaced_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let ctx = context(dir.path());
        let tool = EditTool::new();
        let plan = plan_for(
            &tool,
            &ctx,
            serde_json::json!({ "path": "a.txt", "match_text": "beta", "replace_text": "BETA" }),
        )
        .await
        .unwrap();
        tool.execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[tokio::test]
    async fn a_missing_match_is_an_error_not_a_guess() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let ctx = context(dir.path());
        let error = plan_for(
            &EditTool::new(),
            &ctx,
            serde_json::json!({ "path": "a.txt", "match_text": "bta", "replace_text": "x" }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ToolError::NoMatch { .. }), "{error:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\n"
        );
    }

    #[tokio::test]
    async fn several_matches_are_ambiguous_unless_replace_all_says_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\nx\n").unwrap();
        let ctx = context(dir.path());
        let tool = EditTool::new();

        let error = plan_for(
            &tool,
            &ctx,
            serde_json::json!({ "path": "a.txt", "match_text": "x", "replace_text": "y" }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, ToolError::InvalidArguments { .. }),
            "{error:?}"
        );

        let plan = plan_for(
            &tool,
            &ctx,
            serde_json::json!({
                "path": "a.txt", "match_text": "x", "replace_text": "y", "replace_all": true
            }),
        )
        .await
        .unwrap();
        tool.execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "y\ny\n"
        );
    }

    #[tokio::test]
    async fn a_stale_expected_version_stops_the_edit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let ctx = context(dir.path());
        let tool = EditTool::new();
        let plan = plan_for(
            &tool,
            &ctx,
            serde_json::json!({
                "path": "a.txt",
                "match_text": "alpha",
                "replace_text": "ALPHA",
                "expected_version": ContentHash::of_str("something else").as_str()
            }),
        )
        .await
        .unwrap();
        let error = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::VersionConflict { .. }),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\n"
        );
    }

    #[tokio::test]
    async fn a_file_changed_between_prepare_and_execute_is_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let ctx = context(dir.path());
        let tool = EditTool::new();
        let plan = plan_for(
            &tool,
            &ctx,
            serde_json::json!({ "path": "a.txt", "match_text": "alpha", "replace_text": "ALPHA" }),
        )
        .await
        .unwrap();

        // 审批期间别人改了这个文件。
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let error = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::VersionConflict { .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn verify_tells_not_performed_from_already_satisfied() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let ctx = context(dir.path());
        let tool = EditTool::new();
        let plan = plan_for(
            &tool,
            &ctx,
            serde_json::json!({ "path": "a.txt", "match_text": "alpha", "replace_text": "ALPHA" }),
        )
        .await
        .unwrap();

        assert!(matches!(
            tool.verify(&plan, &ctx).await.unwrap(),
            Verification::NotPerformed { .. }
        ));

        tool.execute(approved(plan.clone()), &ctx, &mut writer(&ctx))
            .await
            .unwrap();
        assert!(matches!(
            tool.verify(&plan, &ctx).await.unwrap(),
            Verification::AlreadySatisfied { .. }
        ));

        std::fs::write(dir.path().join("a.txt"), "someone else\n").unwrap();
        assert!(matches!(
            tool.verify(&plan, &ctx).await.unwrap(),
            Verification::Conflict { .. }
        ));
    }

    #[tokio::test]
    async fn the_model_cannot_dictate_the_result_hash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let ctx = context(dir.path());
        let plan = plan_for(
            &EditTool::new(),
            &ctx,
            serde_json::json!({
                "path": "a.txt",
                "match_text": "alpha",
                "replace_text": "ALPHA",
                "result_version": "deadbeef"
            }),
        )
        .await
        .unwrap();
        let args: EditArgs = serde_json::from_value(plan.args).unwrap();
        assert_eq!(
            args.result_version,
            Some(ContentHash::of_str("ALPHA\n")),
            "result_version 由 prepare 从真实文件算出"
        );
    }

    #[tokio::test]
    async fn an_empty_match_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let ctx = context(dir.path());
        let error = plan_for(
            &EditTool::new(),
            &ctx,
            serde_json::json!({ "path": "a.txt", "match_text": "", "replace_text": "x" }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, ToolError::InvalidArguments { .. }),
            "{error:?}"
        );
    }
}
