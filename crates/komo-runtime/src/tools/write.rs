//! `write`：完整内容 + 可选预期版本；**原子替换**，覆盖现有文件必须版本匹配（§4）。
//!
//! `verify` 用内容哈希回答 §8.6 的那三个问题：目标已是预期内容 → 已满足；仍是原内容
//! → 确定未执行，可以重做同一次原子修改；第三种内容 → 冲突。**文件已存在不算写入
//! 成功**。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use komo_kernel::traits::Tool;
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{
    ApprovedPlan, ExecutionPlan, Operation, PlanTarget, PlanVersions, RecoveryMode, TargetAccess,
    Verification,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::{ExpectedVersion, FileVersion, current, normalized, parse_args, plan_time};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
    /// 覆盖现有文件时的预期版本。文件已存在而没给它 → 版本冲突。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<ExpectedVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteResult {
    pub path: String,
    pub created: bool,
    pub version: FileVersion,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub struct WriteTool;

impl WriteTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write".into(),
            description:
                "把完整内容原子地写入一个文件。覆盖已存在的文件必须给出 expected_version。".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径；相对路径按工作目录解析" },
                    "content": { "type": "string", "description": "写入后文件的完整内容" },
                    "expected_version": {
                        "description": "read 返回的 version（或它的 hash）。覆盖已有文件时必填",
                        "anyOf": [{ "type": "string" }, { "type": "object" }]
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        let args: WriteArgs = parse_args(args, "write")?;
        let path = super::paths::resolve(&args.path, &ctx.cwd)?;
        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "write".into(),
            operation: Operation::WriteFile,
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![PlanTarget {
                path,
                access: TargetAccess::Write,
                expected_version: args.expected_version.as_ref().map(ExpectedVersion::hash),
            }],
            versions: PlanVersions::default(),
            resources: vec![],
            // 写入的恢复靠核对内容哈希（§8.6）。
            recovery: RecoveryMode::VerifyTarget,
        })
    }

    async fn execute(
        &self,
        plan: ApprovedPlan,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let plan = plan.plan();
        let args: WriteArgs = parse_args(plan.args.clone(), "write")?;
        let path = target_path(plan)?;

        let existing = current(&path)?;
        check_version(
            &path,
            existing.as_ref().map(|(_, v)| v),
            args.expected_version.as_ref(),
        )?;

        atomic_replace(&path, args.content.as_bytes())?;
        let metadata = std::fs::metadata(&path).ok();
        let version = FileVersion::of(args.content.as_bytes(), metadata.as_ref());
        let result = WriteResult {
            path: args.path.clone(),
            created: existing.is_none(),
            bytes: version.size,
            version,
        };
        Ok(ToolOutput {
            status: ToolResultStatus::Completed,
            result: serde_json::to_value(&result).map_err(|error| ToolError::Failed {
                message: error.to_string(),
            })?,
            exit_code: None,
            artifacts: vec![],
            preview: Some(format!(
                "{} {}，{} 字节",
                result.path,
                if result.created {
                    "已创建"
                } else {
                    "已覆盖"
                },
                result.bytes
            )),
        })
    }

    /// §8.6：started 而无结果的调用，靠内容哈希判断这次写入到底发生了没有。
    async fn verify(
        &self,
        plan: &ExecutionPlan,
        _ctx: &ToolContext,
    ) -> Result<Verification, ToolError> {
        let args: WriteArgs = parse_args(plan.args.clone(), "write")?;
        let path = target_path(plan)?;
        let intended = ContentHash::of_str(&args.content);
        let expected = plan
            .targets
            .first()
            .and_then(|target| target.expected_version.clone());
        verify_content(&path, &intended, expected.as_ref())
    }
}

/// 三种内容，三个结论（§8.6）。
pub fn verify_content(
    path: &Path,
    intended: &ContentHash,
    before: Option<&ContentHash>,
) -> Result<Verification, ToolError> {
    let Some((_, version)) = current(path)? else {
        // 文件不在。原本就没有它 → 确定没写成；原本有它 → 出现了第三种状态。
        return Ok(match before {
            None => Verification::NotPerformed {
                evidence: format!("{} 仍然不存在", path.display()),
            },
            Some(_) => Verification::Conflict {
                evidence: format!("{} 在写入前存在，现在不见了", path.display()),
            },
        });
    };
    if &version.hash == intended {
        return Ok(Verification::AlreadySatisfied {
            evidence: format!("{} 的内容哈希已是预期值 {intended}", path.display()),
        });
    }
    match before {
        Some(before) if &version.hash == before => Ok(Verification::NotPerformed {
            evidence: format!("{} 仍是原内容 {before}", path.display()),
        }),
        Some(before) => Ok(Verification::Conflict {
            evidence: format!(
                "{} 既不是原内容 {before} 也不是预期内容 {intended}，当前 {}",
                path.display(),
                version.hash
            ),
        }),
        // 计划没绑定原内容（新建文件），而现在文件在、内容又不是预期的：说不清楚。
        None => Ok(Verification::Unknown {
            reason: format!(
                "{} 存在但内容不是预期值，且计划没有记录写入前的版本",
                path.display()
            ),
        }),
    }
}

pub fn target_path(plan: &ExecutionPlan) -> Result<PathBuf, ToolError> {
    plan.targets
        .first()
        .map(|target| target.path.clone())
        .ok_or_else(|| ToolError::Failed {
            message: "计划里没有目标路径".into(),
        })
}

/// 覆盖现有文件需检查版本（§4）。
pub fn check_version(
    path: &Path,
    on_disk: Option<&FileVersion>,
    expected: Option<&ExpectedVersion>,
) -> Result<(), ToolError> {
    match (on_disk, expected) {
        (None, _) => Ok(()),
        (Some(_), None) => Err(ToolError::VersionConflict {
            path: format!(
                "{} 已存在；覆盖它必须给出 expected_version（先 read 一次）",
                path.display()
            ),
        }),
        (Some(actual), Some(expected)) if actual.hash == expected.hash() => Ok(()),
        (Some(actual), Some(expected)) => Err(ToolError::VersionConflict {
            path: format!(
                "{}：预期 {}，实际 {}",
                path.display(),
                expected.hash(),
                actual.hash
            ),
        }),
    }
}

/// 原子替换：同目录临时文件 → fsync → rename → fsync 目录。
///
/// 同目录是必需的：跨文件系统的 rename 不是原子的，会退化成复制。目录也要同步，否则
/// 崩溃后可能看到一个名字还没落盘的新文件（§8.3 对新建文件的要求）。
pub fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    use std::io::Write;

    let parent = path.parent().ok_or_else(|| ToolError::InvalidArguments {
        message: format!("{} 没有父目录", path.display()),
    })?;
    std::fs::create_dir_all(parent).map_err(|error| ToolError::Failed {
        message: format!("创建 {} 失败：{error}", parent.display()),
    })?;

    let temp = parent.join(format!("{TEMP_PREFIX}{}", super::unique_suffix()));
    // 失败路径上不留垃圾：守卫在任何一次 `?` 上都会把它删掉。
    let guard = TempGuard(temp.clone());

    let mut file = std::fs::File::create(&temp).map_err(|error| ToolError::Failed {
        message: format!("在 {} 创建临时文件失败：{error}", parent.display()),
    })?;
    file.write_all(bytes).map_err(|error| ToolError::Failed {
        message: format!("写入临时文件失败：{error}"),
    })?;
    file.flush().map_err(|error| ToolError::Failed {
        message: format!("刷新临时文件失败：{error}"),
    })?;
    // 关闭文件不等于已验证同步成功（§8.3）。
    file.sync_all().map_err(|error| ToolError::Failed {
        message: format!("同步临时文件失败：{error}"),
    })?;
    drop(file);

    std::fs::rename(&temp, path).map_err(|error| ToolError::Failed {
        message: format!("替换 {} 失败：{error}", path.display()),
    })?;
    guard.keep();

    // 目录项本身也要落盘；同步失败不掩盖。
    if let Ok(dir) = std::fs::File::open(parent) {
        dir.sync_all().map_err(|error| ToolError::Failed {
            message: format!("同步目录 {} 失败：{error}", parent.display()),
        })?;
    }
    Ok(())
}

/// 临时文件的前缀，测试按它断言"没留下垃圾"。
pub const TEMP_PREFIX: &str = ".komo-write-";

/// 半途失败时把临时文件删掉。`keep` 之后它什么都不做。
struct TempGuard(PathBuf);

impl TempGuard {
    fn keep(mut self) {
        self.0 = PathBuf::new();
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.0.as_os_str().is_empty() {
            return;
        }
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{approved, context};

    fn write_result(output: &ToolOutput) -> WriteResult {
        serde_json::from_value(output.result.clone()).expect("write 的结果")
    }

    #[tokio::test]
    async fn a_new_file_needs_no_expected_version() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = WriteTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({ "path": "new.txt", "content": "hi\n" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(plan.operation, Operation::WriteFile);
        assert_eq!(plan.recovery, RecoveryMode::VerifyTarget);

        let result = write_result(&tool.execute(approved(plan), &ctx).await.unwrap());
        assert!(result.created);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "hi\n"
        );
    }

    #[tokio::test]
    async fn overwriting_without_a_version_is_a_visible_conflict() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let ctx = context(dir.path());
        let tool = WriteTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({ "path": "a.txt", "content": "new\n" }),
                &ctx,
            )
            .await
            .unwrap();
        let error = tool.execute(approved(plan), &ctx).await.unwrap_err();
        let ToolError::VersionConflict { path } = &error else {
            panic!("{error:?}")
        };
        assert!(path.contains("expected_version"), "{path}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "old\n",
            "拒绝之后原文件不能被动过"
        );
    }

    #[tokio::test]
    async fn a_stale_expected_version_is_a_visible_conflict() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let ctx = context(dir.path());
        let tool = WriteTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({
                    "path": "a.txt",
                    "content": "new\n",
                    "expected_version": ContentHash::of_str("something else").as_str()
                }),
                &ctx,
            )
            .await
            .unwrap();
        let error = tool.execute(approved(plan), &ctx).await.unwrap_err();
        assert!(
            matches!(error, ToolError::VersionConflict { .. }),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "old\n"
        );
    }

    #[tokio::test]
    async fn a_matching_expected_version_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let ctx = context(dir.path());
        let tool = WriteTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({
                    "path": "a.txt",
                    "content": "new\n",
                    "expected_version": ContentHash::of_str("old\n").as_str()
                }),
                &ctx,
            )
            .await
            .unwrap();
        let result = write_result(&tool.execute(approved(plan), &ctx).await.unwrap());
        assert!(!result.created);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "new\n"
        );
    }

    #[tokio::test]
    async fn verify_tells_the_three_states_apart() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let ctx = context(dir.path());
        let tool = WriteTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({
                    "path": "a.txt",
                    "content": "new\n",
                    "expected_version": ContentHash::of_str("old\n").as_str()
                }),
                &ctx,
            )
            .await
            .unwrap();

        // 还没写：仍是原内容 → 确定未执行。
        assert!(matches!(
            tool.verify(&plan, &ctx).await.unwrap(),
            Verification::NotPerformed { .. }
        ));

        // 写完了：已是预期内容 → 目标已满足，不是"可以再写一次"。
        std::fs::write(dir.path().join("a.txt"), "new\n").unwrap();
        assert!(matches!(
            tool.verify(&plan, &ctx).await.unwrap(),
            Verification::AlreadySatisfied { .. }
        ));

        // 第三种内容 → 冲突。
        std::fs::write(dir.path().join("a.txt"), "somebody else\n").unwrap();
        assert!(matches!(
            tool.verify(&plan, &ctx).await.unwrap(),
            Verification::Conflict { .. }
        ));
    }

    #[tokio::test]
    async fn a_file_that_exists_is_not_proof_a_write_succeeded() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = WriteTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({ "path": "new.txt", "content": "intended\n" }),
                &ctx,
            )
            .await
            .unwrap();
        // 别人建了一个同名文件，内容不是我们要写的。
        std::fs::write(dir.path().join("new.txt"), "someone else\n").unwrap();
        let verdict = tool.verify(&plan, &ctx).await.unwrap();
        assert!(
            matches!(verdict, Verification::Unknown { .. }),
            "{verdict:?}"
        );
    }

    #[test]
    fn an_atomic_replace_leaves_no_temporary_behind() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested/deep/a.txt");
        atomic_replace(&file, b"hello").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");
        let leftovers: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
