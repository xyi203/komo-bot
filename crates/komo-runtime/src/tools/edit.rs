//! `edit`：精确匹配替换。**匹配失败返回错误，不做模糊猜测**（§4）。
//!
//! 「不模糊猜测」有两面：找不到要替换的内容是错误，找到**好几处**同样是错误——替换
//! 哪一处需要猜，而猜错和没找到的代价不一样，后者立刻可见，前者要等到有人读那份文件
//! 才发现。想全改就明写 `replace_all`。
//!
//! `prepare` 读一遍文件、先替换一遍：匹配不上当场失败，并把这一刻的内容哈希记成预期版本。
//! 计划因此绑定了"从这一份内容出发"：文件在计划之后被人动过，执行时版本对不上就
//! 按版本冲突失败（§7.2「执行前再次校验目标和版本」）。
//!
//! 预期版本只有这一个来源，模型不给（2026-10-14 起 schema 里没有 `expected_version`）。
//! 实测会话里模型给的那份几乎都是错的：空串 18 次全冲突，非空的 41 次冲突 22 次（多是
//! 自己上一次 edit 之前的版本），不给的 282 次一次没冲突——prepare 刚读过的那份比模型
//! 记得的任何一份都新。旧参数里带着的照常解码，`prepare` 把它丢掉。
//!
//! 重启后不重跑（`Tool::replay` 用默认的 unsafe，§8.6）：替换文本里含着被替换文本时，
//! 重跑一遍会再替换一次。

use std::path::PathBuf;

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{
    ExecutionPlan, Operation, PlanTarget, PlanVersions, RecoveryMode, TargetAccess,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::resource::TargetRef;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::fusion::{ThenRun, attach};
use super::protect::Protected;
use super::write::{atomic_replace, target_path};
use super::{ExpectedVersion, FileVersion, as_text, current, normalized, parse_args, plan_time};

/// 模型给的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditArgs {
    pub path: String,
    /// 要被替换掉的那段内容，逐字匹配。
    pub match_text: String,
    pub replace_text: String,
    /// 2026-10-14 之前 schema 里有它、由模型给。只为解码旧参数保留：`prepare` 丢掉它，
    /// 新计划里永远是空的，预期版本只认 `prepare` 读到的那份。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<ExpectedVersion>,
    /// 匹配到多处时全部替换。默认 false：多处即歧义，歧义就报错。
    #[serde(default)]
    pub replace_all: bool,
    /// 2026-10-13 之前 `prepare` 在这里记替换之后的内容哈希，给核对梯子用。只为解码
    /// 旧计划保留，新计划里永远是空的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_version: Option<ContentHash>,
    /// 改完紧接着跑的命令（[`super::fusion`]）。`prepare` 把它挪进计划的 `then_run`，
    /// 计划里的参数不再带它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then_run: Option<ThenRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditResult {
    pub path: String,
    pub replacements: u64,
    pub version: FileVersion,
}

#[derive(Debug, Default)]
pub struct EditTool {
    /// komo 自己的状态（§8.10）：目标落在这里就在 `prepare` 里拒（[`super::protect`]）。
    protected: Protected,
}

impl EditTool {
    pub fn new() -> Self {
        Self::default()
    }

    /// 装上受保护路径名单（数据目录下的 `sessions/`、`state.db`、`runtime/`、`.env`）。
    /// 不装 = 空名单，什么都不拦。
    pub fn protecting(mut self, paths: Vec<PathBuf>) -> Self {
        self.protected = Protected::new(paths);
        self
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
                    "replace_all": { "type": "boolean", "description": "匹配到多处时全部替换；默认报错" },
                    "then_run": super::fusion::schema("编辑")
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
        args.result_version = None;
        args.expected_version = None;
        let then_run = args.then_run.take();
        if args.match_text.is_empty() {
            return Err(ToolError::InvalidArguments {
                message: "match_text 不能是空串：空串在任何位置都匹配".into(),
            });
        }
        let path = super::paths::resolve(&args.path, &ctx.cwd)?;
        // 唯一的硬限制（§7）：komo 自己的状态不许改。带 `then_run` 也一样——计划不成立，
        // 命令就不会跑。
        self.protected.check(&path)?;

        // 读元信息与正文是允许的（prepare 不得执行未审核代码，读文件不是执行）。
        let before_hash = match current(&path)? {
            Some((bytes, version)) => {
                let text = as_text(bytes, &path)?;
                apply(&text, &args, &path)?;
                Some(version.hash)
            }
            None => None,
        };

        let mut plan = ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "edit".into(),
            operation: Operation::WriteFile,
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![PlanTarget {
                target: TargetRef::local(path),
                access: TargetAccess::Write,
                // 只按 prepare 这一刻看到的那份内容。
                expected_version: before_hash,
            }],
            versions: PlanVersions::default(),
            // 重启后不重跑，中断结果交给模型（§8.6）。
            recovery: RecoveryMode::NoSafeRecovery,
            then_run: None,
        };
        attach(&mut plan, then_run, ctx)?;
        Ok(plan)
    }

    async fn execute(
        &self,
        plan: ExecutionPlan,
        _ctx: &ToolContext,
        // 文件工具不产生流式输出：结构化结果由 executor 发布。
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let plan = &plan;
        let args: EditArgs = parse_args(plan.args.clone(), "edit")?;
        let path: PathBuf = target_path(plan)?;

        let Some((bytes, version)) = current(&path)? else {
            return Err(missing(&path));
        };
        check_expected(&path, &version, expected(&args, plan).as_ref())?;

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
}

fn missing(path: &std::path::Path) -> ToolError {
    ToolError::Failed {
        message: format!("{} 不存在，没有可修改的内容", path.display()),
    }
}

/// 版本核对按哪一份：旧计划的参数里带了就按它（新计划里永远没有），否则按**计划里那一份**
/// ——`prepare` 从真实文件算出来的 before-hash。`edit` 与 `write` 在这里不一样：write 覆盖一个已存在的文件
/// 必须由模型明说预期版本（它写的是整份内容，没读过也能写），而 edit 的前提就是它刚刚
/// 匹配到的那份内容，计划已经把它钉住了。
fn expected(args: &EditArgs, plan: &ExecutionPlan) -> Option<ContentHash> {
    args.expected_version
        .as_ref()
        .map(ExpectedVersion::hash)
        .or_else(|| {
            plan.targets
                .first()
                .and_then(|t| t.expected_version.clone())
        })
}

fn check_expected(
    path: &std::path::Path,
    on_disk: &FileVersion,
    expected: Option<&ContentHash>,
) -> Result<(), ToolError> {
    match expected {
        Some(expected) if expected != &on_disk.hash => Err(ToolError::VersionConflict {
            path: format!("{}：预期 {expected}，当前 {}", path.display(), on_disk.hash),
        }),
        Some(_) => Ok(()),
        // 计划是在文件还不存在时做的，现在它存在了：那是第三种状态。
        None => Err(ToolError::VersionConflict {
            path: format!("{} 在计划生成时并不存在，现在却有内容了", path.display()),
        }),
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
    use crate::tools::test_support::{context, writer};

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
        tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap();
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
        tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "y\ny\n"
        );
    }

    #[tokio::test]
    async fn an_expected_version_from_the_model_is_ignored_for_what_prepare_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let ctx = context(dir.path());
        let tool = EditTool::new();
        for stale in [
            "".to_string(),
            ContentHash::of_str("older").as_str().to_string(),
        ] {
            std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
            let plan = plan_for(
                &tool,
                &ctx,
                serde_json::json!({
                    "path": "a.txt",
                    "match_text": "alpha",
                    "replace_text": "ALPHA",
                    "expected_version": stale
                }),
            )
            .await
            .unwrap();
            assert!(plan.args.get("expected_version").is_none(), "{}", plan.args);
            tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
                "ALPHA\n"
            );
        }
    }

    #[test]
    fn the_schema_no_longer_offers_expected_version() {
        let definition = EditTool::new().definition();
        assert!(
            definition.parameters["properties"]
                .get("expected_version")
                .is_none()
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

        // 计划之后、执行之前别人改了这个文件。
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let error = tool
            .execute(plan, &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::VersionConflict { .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_result_version_from_the_model_never_reaches_the_plan() {
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
            args.result_version, None,
            "result_version 已退役，模型给了也不进计划"
        );
    }

    /// §7 唯一的硬限制同样管 `edit`：`state.db` 一族与 `.env` 都拒在 `prepare`。
    #[tokio::test]
    async fn editing_komos_own_state_is_refused_at_prepare() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(dir.path().join("ws")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".env"), "KEY=old\n").unwrap();
        let ctx = context(&dir.path().join("ws"));
        let tool = EditTool::new().protecting(vec![home.join("state.db"), home.join(".env")]);
        let error = tool
            .prepare(
                serde_json::json!({
                    "path": home.join(".env").display().to_string(),
                    "match_text": "old",
                    "replace_text": "new",
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied { .. }), "{error:?}");
        assert_eq!(
            std::fs::read_to_string(home.join(".env")).unwrap(),
            "KEY=old\n"
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
