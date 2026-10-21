//! `edit`：精确匹配替换。**匹配失败返回错误，不做模糊猜测**（§4）。
//!
//! 「不模糊猜测」有两面：找不到要替换的内容是错误，找到**好几处**同样是错误——替换
//! 哪一处需要猜，而猜错和没找到的代价不一样，后者立刻可见，前者要等到有人读那份文件
//! 才发现。想全改就明写 `replace_all`。
//!
//! 一次调用可以改同一个文件的多处（`edits[]`，2026-10-21，照 pi 的
//! `applyEditsToNormalizedContent`）：每一处都对**原文件**匹配，不在前一处改完的结果上
//! 接着找；每一处必须唯一，除非它自己写了 `replace_all`；各处的匹配区间不许重叠；全部
//! 成立才从后往前替换，前面的偏移不受后面影响。任意一处不成立整个调用就失败，文件一个
//! 字节都不动。此前模型改多处要连发几个 edit，每个都要一次往返、各绑一次版本。旧的单处
//! 参数（顶层 `match_text` / `replace_text` / `replace_all`）照常解码、执行，就是只有一处的
//! `edits`；schema 里不再给它们，两种写法不能混用。
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

use std::path::{Path, PathBuf};

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
    /// 单处写法：要被替换掉的那段内容，逐字匹配。与 `edits` 二选一；schema 里不再给，
    /// 只为旧参数与旧计划保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_text: Option<String>,
    /// 2026-10-14 之前 schema 里有它、由模型给。只为解码旧参数保留：`prepare` 丢掉它，
    /// 新计划里永远是空的，预期版本只认 `prepare` 读到的那份。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<ExpectedVersion>,
    /// 单处写法：匹配到多处时全部替换。没给按 false：多处即歧义，歧义就报错。`prepare`
    /// 给单处写法补上明确的值，计划里的参数与有 `edits` 之前逐字节相同。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_all: Option<bool>,
    /// 多处写法：每一处都对原文件匹配（见模块说明）。与顶层的单处写法二选一。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<Vec<Replacement>>,
    /// 2026-10-13 之前 `prepare` 在这里记替换之后的内容哈希，给核对梯子用。只为解码
    /// 旧计划保留，新计划里永远是空的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_version: Option<ContentHash>,
    /// 改完紧接着跑的命令（[`super::fusion`]）。`prepare` 把它挪进计划的 `then_run`，
    /// 计划里的参数不再带它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then_run: Option<ThenRun>,
}

/// `edits[]` 里的一处替换。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    /// 要被替换掉的那段内容，在原文件里逐字匹配。
    pub match_text: String,
    pub replace_text: String,
    /// 这一处匹配到多处时全部替换。默认 false：多处即歧义，歧义就报错。
    #[serde(default)]
    pub replace_all: bool,
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
            description: "把一个文件里逐字匹配的内容替换掉。同一文件改多处，用一次 edit 带多个 \
                 edits[]，别连发几个 edit；每一处都对原文件匹配，必须唯一、互不重叠，相邻或挨得近\
                 的改动合成一处。匹配不到或匹配到多处都会报错，不做模糊猜测；任意一处失败，文件\
                 一个字节都不动。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径；相对路径按工作目录解析" },
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "description": "这个文件的一处或多处替换。每一处都对原文件匹配，不是在前一处改完的结果上接着找；不要给重叠或嵌套的几处，挨得近的改动合成一处",
                        "items": {
                            "type": "object",
                            "properties": {
                                "match_text": { "type": "string", "description": "这一处要替换掉的原文，逐字匹配；在原文件里必须唯一，且不与别处的 match_text 重叠。够唯一就行，别为了连起远处的改动带上大段没改的内容" },
                                "replace_text": { "type": "string", "description": "这一处替换成什么" },
                                "replace_all": { "type": "boolean", "description": "这一���匹配到多处时全部替换；默认报错" }
                            },
                            "required": ["match_text", "replace_text"],
                            "additionalProperties": false
                        }
                    },
                    "then_run": super::fusion::schema("编辑")
                },
                "required": ["path", "edits"],
                "additionalProperties": false
            }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let mut args: EditArgs = parse_args(lenient(args), "edit")?;
        args.result_version = None;
        args.expected_version = None;
        let then_run = args.then_run.take();
        // 写法不对、有一处是空串，不用读文件就能拒。
        replacements(&args)?;
        if args.edits.is_none() {
            args.replace_all = Some(args.replace_all.unwrap_or(false));
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

/// 照 pi 的 `prepareArguments`：有的模型把 `edits` 编成一段 JSON 字符串，或只给一个对象
/// 而不是数组。认得出来就摆正，认不出来原样交给解析去报错。
fn lenient(mut args: serde_json::Value) -> serde_json::Value {
    let Some(edits) = args.get_mut("edits") else {
        return args;
    };
    if let Some(Ok(parsed @ (serde_json::Value::Array(_) | serde_json::Value::Object(_)))) = edits
        .as_str()
        .map(serde_json::from_str::<serde_json::Value>)
    {
        *edits = parsed;
    }
    if edits.is_object() {
        *edits = serde_json::Value::Array(vec![edits.take()]);
    }
    args
}

/// 这次调用要做的那几处替换：单处写法就是只有一处。两种写法混用、都没给、`edits` 是空的、
/// 哪一处的 `match_text` 是空串，都是参数错误。
fn replacements(args: &EditArgs) -> Result<Vec<Replacement>, ToolError> {
    let invalid = |message: String| ToolError::InvalidArguments { message };
    let single = args.match_text.is_some() || args.replace_text.is_some();
    let list = match (&args.edits, &args.match_text, &args.replace_text) {
        (Some(_), ..) if single || args.replace_all.is_some() => {
            return Err(invalid(
                "edits 与顶层的 match_text / replace_text / replace_all 只能用一种：\
                 几处都放进 edits[]"
                    .into(),
            ));
        }
        (Some(edits), ..) if edits.is_empty() => {
            return Err(invalid("edits 不能是空数组：至少给一处替换".into()));
        }
        (Some(edits), ..) => edits.clone(),
        (None, Some(match_text), Some(replace_text)) => vec![Replacement {
            match_text: match_text.clone(),
            replace_text: replace_text.clone(),
            replace_all: args.replace_all.unwrap_or(false),
        }],
        (None, ..) if single => {
            return Err(invalid("match_text 与 replace_text 要一起给".into()));
        }
        (None, ..) => {
            return Err(invalid(
                "没有要替换的内容：给 edits[]，每一处带 match_text 与 replace_text".into(),
            ));
        }
    };
    for (index, each) in list.iter().enumerate() {
        if each.match_text.is_empty() {
            return Err(invalid(format!(
                "{}match_text 不能是空串：空串在任何位置都匹配",
                which(args, index)
            )));
        }
    }
    Ok(list)
}

/// 报错时怎么点名第 `index` 处：多处写法给下标，单处写法只有一处，不用点名。
fn which(args: &EditArgs, index: usize) -> String {
    match args.edits {
        Some(_) => format!("edits[{index}]."),
        None => String::new(),
    }
}

/// 逐字替换，照 pi 的 `applyEditsToNormalizedContent`：每一处都在原文里找，匹配不到、
/// 匹配到多处而这一处没说要全改、与别处的匹配区间重叠，都是错误；全部成立才从后往前换，
/// 前面的偏移不受后面影响。返回新内容与一共换了几段。
fn apply(text: &str, args: &EditArgs, path: &Path) -> Result<(String, u64), ToolError> {
    let list = replacements(args)?;
    // 每一段匹配：（起，止，第几处）。
    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    for (index, each) in list.iter().enumerate() {
        let hits: Vec<usize> = text
            .match_indices(each.match_text.as_str())
            .map(|(at, _)| at)
            .collect();
        match hits.len() {
            0 => {
                return Err(ToolError::NoMatch {
                    path: match args.edits {
                        Some(_) => format!("{} 的 edits[{index}]", path.display()),
                        None => path.display().to_string(),
                    },
                });
            }
            count if count > 1 && !each.replace_all => {
                return Err(ToolError::InvalidArguments {
                    message: format!(
                        "{} 里 {}match_text 匹配到 {count} 处；给一段唯一的上下文，或显式 replace_all",
                        path.display(),
                        which(args, index)
                    ),
                });
            }
            _ => spans.extend(
                hits.into_iter()
                    .map(|at| (at, at + each.match_text.len(), index)),
            ),
        }
    }
    spans.sort_unstable();
    for pair in spans.windows(2) {
        let ((_, end, before), (start, _, after)) = (pair[0], pair[1]);
        if end > start {
            return Err(ToolError::InvalidArguments {
                message: format!(
                    "{} 里 edits[{before}] 与 edits[{after}] 的匹配区间重叠；合成一处，或各自对准不相交的地方",
                    path.display()
                ),
            });
        }
    }
    let mut out = text.to_string();
    for &(start, end, index) in spans.iter().rev() {
        out.replace_range(start..end, &list[index].replace_text);
    }
    Ok((out, spans.len() as u64))
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
        let path = home.join(".env").display().to_string();
        for args in [
            serde_json::json!({ "path": path, "match_text": "old", "replace_text": "new" }),
            serde_json::json!({
                "path": path,
                "edits": [{ "match_text": "old", "replace_text": "new" }],
            }),
        ] {
            let error = tool.prepare(args, &ctx).await.unwrap_err();
            assert!(matches!(error, ToolError::Denied { .. }), "{error:?}");
        }
        assert_eq!(
            std::fs::read_to_string(home.join(".env")).unwrap(),
            "KEY=old\n"
        );
    }

    /// 改多处的那份计划：先 prepare 再 execute，返回改完的文件内容与回执。
    async fn edit_many(
        dir: &std::path::Path,
        before: &str,
        edits: serde_json::Value,
    ) -> Result<(String, ToolOutput), ToolError> {
        std::fs::write(dir.join("a.txt"), before).unwrap();
        let ctx = context(dir);
        let tool = EditTool::new();
        let plan = plan_for(
            &tool,
            &ctx,
            serde_json::json!({ "path": "a.txt", "edits": edits }),
        )
        .await?;
        let output = tool.execute(plan, &ctx, &mut writer(&ctx)).await?;
        Ok((std::fs::read_to_string(dir.join("a.txt")).unwrap(), output))
    }

    #[tokio::test]
    async fn several_edits_in_one_call_all_apply() {
        let dir = tempfile::tempdir().unwrap();
        let (after, output) = edit_many(
            dir.path(),
            "alpha\nbeta\ngamma\n",
            serde_json::json!([
                { "match_text": "gamma", "replace_text": "GAMMA" },
                { "match_text": "alpha", "replace_text": "ALPHA" },
            ]),
        )
        .await
        .unwrap();
        assert_eq!(after, "ALPHA\nbeta\nGAMMA\n");
        assert_eq!(output.result["replacements"], 2);
    }

    /// 每一处都对原文件匹配：第二处找的 `b` 只有原文里那一个，不算上第一处换出来的 `b`。
    #[tokio::test]
    async fn each_edit_matches_the_original_not_the_previous_result() {
        let dir = tempfile::tempdir().unwrap();
        let (after, _) = edit_many(
            dir.path(),
            "a\nb\n",
            serde_json::json!([
                { "match_text": "a", "replace_text": "b" },
                { "match_text": "b", "replace_text": "c" },
            ]),
        )
        .await
        .unwrap();
        assert_eq!(after, "b\nc\n");
    }

    /// `replace_all` 只管写了它的那一处；其余各处照样必须唯一。
    #[tokio::test]
    async fn replace_all_belongs_to_its_own_edit() {
        let dir = tempfile::tempdir().unwrap();
        let (after, output) = edit_many(
            dir.path(),
            "x y x\n",
            serde_json::json!([
                { "match_text": "x", "replace_text": "1", "replace_all": true },
                { "match_text": "y", "replace_text": "2" },
            ]),
        )
        .await
        .unwrap();
        assert_eq!(after, "1 2 1\n");
        assert_eq!(output.result["replacements"], 3);

        let error = edit_many(
            dir.path(),
            "x y x y\n",
            serde_json::json!([
                { "match_text": "x", "replace_text": "1", "replace_all": true },
                { "match_text": "y", "replace_text": "2" },
            ]),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, ToolError::InvalidArguments { message } if message.contains("edits[1]")),
            "{error:?}"
        );
    }

    /// 哪一处不成立，报错就点名它的下标；文件一个字节都不动。
    #[tokio::test]
    async fn a_failing_edit_names_its_index_and_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let before = "alpha\nbeta\nbeta\n";
        let cases = [
            (
                serde_json::json!([
                    { "match_text": "alpha", "replace_text": "ALPHA" },
                    { "match_text": "delta", "replace_text": "DELTA" },
                ]),
                "edits[1]",
            ),
            (
                serde_json::json!([
                    { "match_text": "alpha", "replace_text": "ALPHA" },
                    { "match_text": "beta", "replace_text": "BETA" },
                ]),
                "edits[1]",
            ),
            (
                serde_json::json!([
                    { "match_text": "", "replace_text": "x" },
                    { "match_text": "alpha", "replace_text": "ALPHA" },
                ]),
                "edits[0]",
            ),
        ];
        for (edits, named) in cases {
            let error = edit_many(dir.path(), before, edits).await.unwrap_err();
            assert!(error.to_string().contains(named), "{error}");
            assert_eq!(
                std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
                before
            );
        }
    }

    /// 两处的匹配区间重叠（含嵌套）就报错，点名是哪两处；首尾相接不算重叠。
    #[tokio::test]
    async fn overlapping_edits_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for edits in [
            serde_json::json!([
                { "match_text": "alpha beta", "replace_text": "x" },
                { "match_text": "beta gamma", "replace_text": "y" },
            ]),
            serde_json::json!([
                { "match_text": "alpha beta gamma", "replace_text": "x" },
                { "match_text": "beta", "replace_text": "y" },
            ]),
        ] {
            let error = edit_many(dir.path(), "alpha beta gamma\n", edits)
                .await
                .unwrap_err();
            assert!(
                matches!(&error, ToolError::InvalidArguments { message }
                    if message.contains("edits[0]") && message.contains("edits[1]") && message.contains("重叠")),
                "{error:?}"
            );
        }

        let (after, _) = edit_many(
            dir.path(),
            "alphabeta\n",
            serde_json::json!([
                { "match_text": "beta", "replace_text": "B" },
                { "match_text": "alpha", "replace_text": "A" },
            ]),
        )
        .await
        .unwrap();
        assert_eq!(after, "AB\n");
    }

    /// 两种写法只能用一种；都没给、`edits` 是空数组，同样是参数错误。
    #[tokio::test]
    async fn mixing_both_forms_or_giving_neither_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let ctx = context(dir.path());
        let one = serde_json::json!([{ "match_text": "alpha", "replace_text": "ALPHA" }]);
        for args in [
            serde_json::json!({ "path": "a.txt", "edits": one, "match_text": "alpha", "replace_text": "x" }),
            serde_json::json!({ "path": "a.txt", "edits": one, "replace_all": true }),
            serde_json::json!({ "path": "a.txt", "edits": [] }),
            serde_json::json!({ "path": "a.txt" }),
            serde_json::json!({ "path": "a.txt", "match_text": "alpha" }),
        ] {
            let error = plan_for(&EditTool::new(), &ctx, args.clone())
                .await
                .unwrap_err();
            assert!(
                matches!(error, ToolError::InvalidArguments { .. }),
                "{args}: {error:?}"
            );
        }
    }

    /// 照 pi：`edits` 编成 JSON 字符串、或只给了一个对象，摆正了照常改。
    #[tokio::test]
    async fn edits_sent_as_a_string_or_a_single_object_are_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = EditTool::new();
        for edits in [
            serde_json::json!(r#"[{"match_text":"alpha","replace_text":"ALPHA"}]"#),
            serde_json::json!({ "match_text": "alpha", "replace_text": "ALPHA" }),
        ] {
            std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
            let plan = plan_for(
                &tool,
                &ctx,
                serde_json::json!({ "path": "a.txt", "edits": edits }),
            )
            .await
            .unwrap();
            assert!(plan.args["edits"].is_array(), "{}", plan.args);
            tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
                "ALPHA\n"
            );
        }
    }

    /// 单处写法的计划参数和有 `edits` 之前逐字节相同；旧计划里那份参数照样能执行。
    #[tokio::test]
    async fn a_single_edit_plan_keeps_its_old_shape_and_old_plans_still_run() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let ctx = context(dir.path());
        let tool = EditTool::new();
        let mut plan = plan_for(
            &tool,
            &ctx,
            serde_json::json!({ "path": "a.txt", "match_text": "alpha", "replace_text": "ALPHA" }),
        )
        .await
        .unwrap();
        let old_shape = serde_json::json!({
            "path": "a.txt",
            "match_text": "alpha",
            "replace_text": "ALPHA",
            "replace_all": false
        });
        assert_eq!(
            serde_json::to_string(&plan.args).unwrap(),
            serde_json::to_string(&old_shape).unwrap()
        );

        // 旧计划：参数里还带着当时会写进去的 `result_version`。
        plan.args = serde_json::json!({
            "path": "a.txt",
            "match_text": "alpha",
            "replace_text": "ALPHA",
            "replace_all": false,
            "result_version": ContentHash::of_str("ALPHA\n").as_str()
        });
        tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "ALPHA\n"
        );
    }

    /// schema 只给 `edits`，引导"同一文件改多处用一次 edit"；顶层单处写法不再给模型看。
    #[test]
    fn the_schema_offers_edits_instead_of_a_single_match() {
        let definition = EditTool::new().definition();
        let parameters = &definition.parameters;
        assert_eq!(parameters["required"], serde_json::json!(["path", "edits"]));
        assert_eq!(
            parameters["properties"]["edits"]["items"]["required"],
            serde_json::json!(["match_text", "replace_text"])
        );
        assert!(parameters["properties"].get("match_text").is_none());
        assert!(definition.description.contains("edits[]"));
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
