//! `read`：路径 + 读取范围，返回文本和文件版本；大文件截断并**明确显示未读范围**
//! （§4）。
//!
//! 截断不是"少给一点"，是"给一部分并说清楚少了哪一段"：一个把第 200 行当成文件末尾
//! 的模型，会自信地报告一个不存在的结论。

use std::path::PathBuf;

use async_trait::async_trait;
use komo_kernel::traits::Tool;
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{
    ExecutionPlan, Operation, PlanTarget, PlanVersions, RecoveryMode, TargetAccess,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::{FileVersion, as_text, current, normalized, parse_args, plan_time};

/// 一次 `read` 最多交出多少字节的正文。超出就截断并报告未读范围。
pub const DEFAULT_BYTE_LIMIT: u64 = 64 * 1024;
/// 不指定行数时最多给多少行。
pub const DEFAULT_LINE_LIMIT: u64 = 2000;

/// 模型给的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadArgs {
    pub path: String,
    /// 从第几行开始（1 起）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u64>,
    /// 最多读几行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_count: Option<u64>,
}

/// 交回模型的结构化结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadResult {
    pub path: String,
    pub version: FileVersion,
    pub start_line: u64,
    /// 这次实际读到的最后一行（含）。文件为空时等于 `start_line - 1`。
    pub end_line: u64,
    pub total_lines: u64,
    #[serde(default)]
    pub truncated: bool,
    /// 没读到的部分，逐段说明。截断时非空。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unread: Vec<UnreadRange>,
    pub text: String,
}

/// 一段没有读到的范围。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadRange {
    pub from_line: u64,
    pub to_line: u64,
    pub reason: String,
}

#[derive(Debug)]
pub struct ReadTool {
    byte_limit: u64,
}

impl Default for ReadTool {
    /// `Default` 走和 [`ReadTool::new`] 同一条路——派生出来的 `byte_limit = 0`
    /// 会把每次读取都截成空的，那是一个安静到没人会发现的 bug。
    fn default() -> Self {
        Self::new()
    }
}

impl ReadTool {
    pub fn new() -> Self {
        Self {
            byte_limit: DEFAULT_BYTE_LIMIT,
        }
    }

    pub fn with_byte_limit(byte_limit: u64) -> Self {
        Self { byte_limit }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read".into(),
            description: "读取一个文本文件，返回正文与文件版本。大文件会截断，并列出未读的行范围。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径；相对路径按工作目录解析" },
                    "start_line": { "type": "integer", "minimum": 1, "description": "从第几行开始读，1 起" },
                    "line_count": { "type": "integer", "minimum": 1, "description": "最多读几行" }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let args: ReadArgs = parse_args(args, "read")?;
        if args.start_line == Some(0) {
            return Err(ToolError::InvalidArguments {
                message: "start_line 从 1 起".into(),
            });
        }
        // 真实路径（符号链接已解析）——规则只看这个。
        let path = super::paths::resolve(&args.path, &ctx.cwd)?;
        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "read".into(),
            operation: Operation::ReadFile,
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![PlanTarget {
                path,
                access: TargetAccess::Read,
                expected_version: None,
            }],
            versions: PlanVersions::default(),
            resources: vec![],
            // 普通文件读取可以安全重做；重做时记的是**这次**读到的内容和时间，
            // 不冒充重启前的观察（§8.6）。
            recovery: RecoveryMode::SafeReread,
        })
    }

    async fn execute(
        &self,
        plan: komo_kernel::types::plan::ApprovedPlan,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let plan = plan.plan();
        let args: ReadArgs = parse_args(plan.args.clone(), "read")?;
        let path: PathBuf = plan
            .targets
            .first()
            .map(|target| target.path.clone())
            .ok_or_else(|| ToolError::Failed {
                message: "计划里没有目标路径".into(),
            })?;

        let Some((bytes, version)) = current(&path)? else {
            return Err(ToolError::Failed {
                message: format!("{} 不存在", path.display()),
            });
        };
        let text = as_text(bytes, &path)?;
        let result = slice(&text, &version, &args, self.byte_limit, &args.path);
        let preview = preview_of(&result);
        Ok(ToolOutput {
            status: ToolResultStatus::Completed,
            result: serde_json::to_value(&result).map_err(|error| ToolError::Failed {
                message: error.to_string(),
            })?,
            exit_code: None,
            artifacts: vec![],
            preview: Some(preview),
        })
    }
}

/// 按行切一段，再按字节上限收一次口。
fn slice(
    text: &str,
    version: &FileVersion,
    args: &ReadArgs,
    byte_limit: u64,
    display_path: &str,
) -> ReadResult {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let total_lines = lines.len() as u64;
    let start_line = args.start_line.unwrap_or(1).max(1);
    let wanted = args.line_count.unwrap_or(DEFAULT_LINE_LIMIT);

    let mut unread = Vec::new();
    if start_line > 1 {
        unread.push(UnreadRange {
            from_line: 1,
            to_line: (start_line - 1).min(total_lines),
            reason: "start_line 之前".into(),
        });
    }

    let first = (start_line - 1).min(total_lines) as usize;
    let mut taken = String::new();
    let mut end_line = start_line - 1;
    let mut truncated = false;
    for (offset, line) in lines[first..].iter().enumerate() {
        if offset as u64 >= wanted {
            truncated = true;
            break;
        }
        if taken.len() as u64 + line.len() as u64 > byte_limit && !taken.is_empty() {
            truncated = true;
            break;
        }
        taken.push_str(line);
        end_line = start_line + offset as u64;
    }

    if end_line < total_lines {
        truncated = true;
        unread.push(UnreadRange {
            from_line: end_line + 1,
            to_line: total_lines,
            reason: if end_line + 1 == start_line {
                "起始行已越过文件末尾".into()
            } else {
                "超出本次读取的行数或字节上限".into()
            },
        });
    }

    ReadResult {
        path: display_path.to_string(),
        version: version.clone(),
        start_line,
        end_line,
        total_lines,
        truncated,
        unread,
        text: taken,
    }
}

fn preview_of(result: &ReadResult) -> String {
    let head: String = result.text.chars().take(400).collect();
    if result.truncated {
        let ranges: Vec<String> = result
            .unread
            .iter()
            .map(|range| format!("{}–{}", range.from_line, range.to_line))
            .collect();
        format!(
            "{} 行 {}–{}（共 {} 行，未读 {}）\n{head}",
            result.path,
            result.start_line,
            result.end_line,
            result.total_lines,
            ranges.join("、")
        )
    } else {
        format!("{} 共 {} 行\n{head}", result.path, result.total_lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{approved, context};

    fn read_result(output: &ToolOutput) -> ReadResult {
        serde_json::from_value(output.result.clone()).expect("read 的结果")
    }

    #[tokio::test]
    async fn it_returns_the_text_and_the_file_version() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let ctx = context(dir.path());

        let tool = ReadTool::new();
        let plan = tool
            .prepare(serde_json::json!({ "path": "a.txt" }), &ctx)
            .await
            .unwrap();
        assert_eq!(plan.operation, Operation::ReadFile);
        assert_eq!(plan.recovery, RecoveryMode::SafeReread);
        assert_eq!(plan.targets[0].path, std::fs::canonicalize(&file).unwrap());

        let output = tool.execute(approved(plan), &ctx).await.unwrap();
        let result = read_result(&output);
        assert_eq!(result.text, "one\ntwo\n");
        assert_eq!(result.total_lines, 2);
        assert!(!result.truncated);
        assert_eq!(result.version.size, 8);
    }

    #[tokio::test]
    async fn a_truncated_read_says_which_lines_it_did_not_show() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), &body).unwrap();
        let ctx = context(dir.path());

        let tool = ReadTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({ "path": "big.txt", "start_line": 3, "line_count": 2 }),
                &ctx,
            )
            .await
            .unwrap();
        let result = read_result(&tool.execute(approved(plan), &ctx).await.unwrap());

        assert_eq!(result.text, "line 3\nline 4\n");
        assert_eq!((result.start_line, result.end_line), (3, 4));
        assert!(result.truncated);
        assert_eq!(
            result
                .unread
                .iter()
                .map(|r| (r.from_line, r.to_line))
                .collect::<Vec<_>>(),
            vec![(1, 2), (5, 10)]
        );
    }

    #[tokio::test]
    async fn a_byte_budget_also_truncates_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), &body).unwrap();
        let ctx = context(dir.path());

        let tool = ReadTool::with_byte_limit(20);
        let plan = tool
            .prepare(serde_json::json!({ "path": "big.txt" }), &ctx)
            .await
            .unwrap();
        let result = read_result(&tool.execute(approved(plan), &ctx).await.unwrap());
        assert!(result.truncated);
        assert!(result.text.len() <= 28, "{:?}", result.text);
        assert!(!result.unread.is_empty());
    }

    #[tokio::test]
    async fn a_start_line_past_the_end_reads_nothing_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let ctx = context(dir.path());
        let tool = ReadTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({ "path": "a.txt", "start_line": 9 }),
                &ctx,
            )
            .await
            .unwrap();
        let result = read_result(&tool.execute(approved(plan), &ctx).await.unwrap());
        assert!(result.text.is_empty());
        assert_eq!(
            result.unread[0],
            UnreadRange {
                from_line: 1,
                to_line: 1,
                reason: "start_line 之前".into()
            }
        );
    }

    #[tokio::test]
    async fn a_missing_file_fails_as_a_result_the_model_can_act_on() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = ReadTool::new();
        let plan = tool
            .prepare(serde_json::json!({ "path": "nope.txt" }), &ctx)
            .await
            .unwrap();
        let error = tool.execute(approved(plan), &ctx).await.unwrap_err();
        assert!(matches!(error, ToolError::Failed { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_bad_start_line_is_refused_at_prepare_time() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let error = ReadTool::new()
            .prepare(
                serde_json::json!({ "path": "a.txt", "start_line": 0 }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::InvalidArguments { .. }),
            "{error:?}"
        );
    }
}
