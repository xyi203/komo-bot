//! `write`：完整内容 + 可选预期版本；**原子替换**，覆盖现有文件必须版本匹配（§4）。
//!
//! `verify` 用内容哈希回答 §8.6 的那三个问题：目标已是预期内容 → 已满足；仍是原内容
//! → 确定未执行，可以重做同一次原子修改；第三种内容 → 冲突。**文件已存在不算写入
//! 成功**。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{
    ApprovedPlan, ExecutionPlan, Operation, PlanTarget, PlanVersions, RecoveryMode, TargetAccess,
    Verification,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::resource::TargetRef;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::fusion::{ThenRun, attach};
use super::{ExpectedVersion, FileVersion, current, normalized, parse_args, plan_time};
use crate::toolbox::diff;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
    /// 覆盖现有文件时的预期版本。文件已存在而没给它 → 版本冲突。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<ExpectedVersion>,
    /// 写完紧接着跑的命令（[`super::fusion`]）。`prepare` 把它挪进计划的 `then_run`，
    /// 计划里的参数不再带它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then_run: Option<ThenRun>,
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
                    },
                    "then_run": super::fusion::schema("写入")
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
        let mut args: WriteArgs = parse_args(args, "write")?;
        let then_run = args.then_run.take();
        let path = super::paths::resolve(&args.path, &ctx.cwd)?;
        let mut plan = ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "write".into(),
            operation: Operation::WriteFile,
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![PlanTarget {
                target: TargetRef::local(path),
                access: TargetAccess::Write,
                expected_version: args.expected_version.as_ref().map(ExpectedVersion::hash),
            }],
            versions: PlanVersions::default(),
            resources: vec![],
            // 写入的恢复靠核对内容哈希（§8.6）。带 then_run 时 `attach` 改成 NoSafeRecovery。
            recovery: RecoveryMode::VerifyTarget,
            then_run: None,
        };
        attach(&mut plan, then_run, ctx)?;
        Ok(plan)
    }

    async fn execute(
        &self,
        plan: ApprovedPlan,
        _ctx: &ToolContext,
        // 文件工具不产生流式输出：结构化结果由 executor 发布。
        _sink: &mut dyn OutputWriter,
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

    /// 与 `execute` 同一道版本核对：执行时会因版本不符失败的计划不给 diff。
    async fn changes(&self, plan: &ExecutionPlan) -> Option<String> {
        let args: WriteArgs = parse_args(plan.args.clone(), "write").ok()?;
        let path = target_path(plan).ok()?;
        let existing = current(&path).ok()?;
        if let Err(conflict) = check_version(
            &path,
            existing.as_ref().map(|(_, v)| v),
            args.expected_version.as_ref(),
        ) {
            return Some(unrunnable(conflict));
        }
        let Some((bytes, _)) = existing else {
            return proposed(&path, None, &args.content);
        };
        match String::from_utf8(bytes) {
            Ok(before) => proposed(&path, Some(&before), &args.content),
            Err(_) => Some(format!(
                "（{} 当前不是 UTF-8 文本，列不出 diff；批准后整份替换成 {} 字节的新内容）",
                path.display(),
                args.content.len()
            )),
        }
    }
}

/// 审批里一份改动最多带这么多字节。渠道各自再截到自己的上限；这一份是给 TUI 与
/// `komo approval show` 看的，而且要进 state.db 的审批行，所以在源头就截。
pub const CHANGES_LIMIT: usize = 8 * 1024;

/// 给人看的改动（§11.3）。`before` 为 `None` 是新文件。两边相同就没有 diff 可列。
pub fn proposed(path: &Path, before: Option<&str>, after: &str) -> Option<String> {
    let diff = match before {
        Some(before) => diff::unified(before, after),
        None => format!("（新建 {}）\n{}", path.display(), diff::unified("", after)),
    };
    if diff.is_empty() {
        return Some("（内容与当前文件相同）".into());
    }
    Some(clip(diff))
}

/// 执行时注定失败的计划：那份 diff 不会发生，只说一句为什么。
pub fn unrunnable(error: ToolError) -> String {
    format!("（不列 diff：{error}——执行时会因此失败）")
}

fn clip(diff: String) -> String {
    if diff.len() <= CHANGES_LIMIT {
        return diff;
    }
    let mut cut = CHANGES_LIMIT;
    while !diff.is_char_boundary(cut) {
        cut -= 1;
    }
    let head = &diff[..cut];
    let newline = if head.ends_with('\n') { "" } else { "\n" };
    format!(
        "{head}{newline}…（diff 已截断：共 {} 字节，只列前 {cut} 字节）\n",
        diff.len()
    )
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
        .and_then(|target| target.path())
        .map(Path::to_path_buf)
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
    use crate::tools::test_support::{approved, context, writer};

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

        let result = write_result(
            &tool
                .execute(approved(plan), &ctx, &mut writer(&ctx))
                .await
                .unwrap(),
        );
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
        let error = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
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
        let result = write_result(
            &tool
                .execute(approved(plan), &ctx, &mut writer(&ctx))
                .await
                .unwrap(),
        );
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

    #[tokio::test]
    async fn a_new_file_lists_every_line_as_added() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = WriteTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({ "path": "new.txt", "content": "one\ntwo\n" }),
                &ctx,
            )
            .await
            .unwrap();
        let changes = tool.changes(&plan).await.expect("有改动");
        assert!(changes.starts_with("（新建 "), "{changes}");
        assert!(changes.contains("+ one\n+ two\n"), "{changes}");
        assert!(!dir.path().join("new.txt").exists(), "列改动不落盘");
    }

    #[tokio::test]
    async fn an_overwrite_lists_the_diff_against_the_file_on_disk() {
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
        let changes = tool.changes(&plan).await.expect("有改动");
        assert!(changes.contains("- old\n+ new\n"), "{changes}");
    }

    #[tokio::test]
    async fn a_stale_version_gets_a_note_instead_of_a_diff() {
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

        // 计划之后有人改了它：执行时会因版本不符失败，这份 diff 不会发生。
        std::fs::write(dir.path().join("a.txt"), "somebody else\n").unwrap();
        let changes = tool.changes(&plan).await.expect("有一句说明");
        assert!(changes.contains("版本冲突"), "{changes}");
        assert!(!changes.contains("+ new"), "{changes}");
        assert_eq!(changes.lines().count(), 1, "{changes}");
    }

    #[tokio::test]
    async fn a_non_utf8_file_gets_a_note_instead_of_a_diff() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = [0xff, 0xfe, 0x00];
        std::fs::write(dir.path().join("a.bin"), bytes).unwrap();
        let ctx = context(dir.path());
        let tool = WriteTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({
                    "path": "a.bin",
                    "content": "text\n",
                    "expected_version": ContentHash::of_bytes(&bytes).as_str()
                }),
                &ctx,
            )
            .await
            .unwrap();
        let changes = tool.changes(&plan).await.expect("有一句说明");
        assert!(changes.contains("不是 UTF-8 文本"), "{changes}");
        assert_eq!(changes.lines().count(), 1, "{changes}");
    }

    #[tokio::test]
    async fn a_long_diff_is_clipped_at_the_source_with_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = WriteTool::new();
        // 多字节字符：截断点落不到字符中间。
        let content = "改动行\n".repeat(CHANGES_LIMIT);
        let plan = tool
            .prepare(
                serde_json::json!({ "path": "big.txt", "content": content }),
                &ctx,
            )
            .await
            .unwrap();
        let changes = tool.changes(&plan).await.expect("有改动");
        assert!(changes.len() < CHANGES_LIMIT + 200, "{}", changes.len());
        assert!(changes.contains("diff 已截断"), "{changes}");
        assert!(changes.ends_with("字节）\n"), "{changes}");
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
