//! `write`：完整内容 + 可选预期版本；**原子替换**，覆盖现有文件必须版本匹配（§4）。
//!
//! 重启后不重跑（`Tool::replay` 用默认的 unsafe，§8.6）：started 而没有结果的一次写入，
//! 中断结果交给模型，由它看一眼文件再决定。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
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
use super::{ExpectedVersion, FileVersion, current, normalized, parse_args, plan_time};

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
pub struct WriteTool {
    /// komo 自己的状态（§8.10）：目标落在这里就在 `prepare` 里拒（[`super::protect`]）。
    protected: Protected,
}

impl WriteTool {
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
                        "description": "read 抬头里「版本」后面那一串（write 回执里也有）。覆盖已有文件时必填",
                        "anyOf": [{ "type": "string" }, { "type": "object" }]
                    },
                    "then_run": super::fusion::schema("写入")
                },
                "required": ["path", "content"],
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
        let mut args: WriteArgs = parse_args(loose_version(args)?, "write")?;
        // 空串不是一个版本，是没给：按没给走，覆盖已有文件时报的是「先 read 一次」，
        // 而不是一句「预期 ，当前 …」的版本冲突。
        if matches!(&args.expected_version, Some(ExpectedVersion::Hash(raw)) if raw.trim().is_empty())
        {
            args.expected_version = None;
        }
        let then_run = args.then_run.take();
        let path = super::paths::resolve(&args.path, &ctx.cwd)?;
        // 唯一的硬限制（§7）：komo 自己的状态不许改。带 `then_run` 也一样——计划不成立，
        // 命令就不会跑。
        self.protected.check(&path)?;
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
            images: vec![],
            preview: Some(format!(
                "{} {}，{} 字节 · 版本 {}",
                result.path,
                if result.created {
                    "已创建"
                } else {
                    "已覆盖"
                },
                result.bytes,
                result.version.shown()
            )),
        })
    }
}

/// 模型手里没有版本时会传 `{}`、`null`，或只抄一个 `{"hash": …}`：前两种按没给走（覆盖时
/// 报「先 read」），后一种取出哈希。别的形状说清该传什么，而不是一句 untagged enum 解不开。
fn loose_version(mut args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let Some(object) = args.as_object_mut() else {
        return Ok(args);
    };
    let replacement = match object.get("expected_version") {
        None | Some(serde_json::Value::String(_)) => return Ok(args),
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(map)) if map.is_empty() => None,
        Some(serde_json::Value::Object(map)) => match map.get("hash") {
            Some(serde_json::Value::String(hash)) if map.get("size").is_none() => {
                Some(serde_json::Value::String(hash.clone()))
            }
            Some(serde_json::Value::String(_)) => return Ok(args),
            _ => return Err(bad_version()),
        },
        Some(_) => return Err(bad_version()),
    };
    match replacement {
        Some(hash) => object.insert("expected_version".into(), hash),
        None => object.remove("expected_version"),
    };
    Ok(args)
}

fn bad_version() -> ToolError {
    ToolError::InvalidArguments {
        message: "write 的 expected_version 应是一个版本串：read 抬头里「版本」后面那一串（或上次 \
                  write 回执里的），不知道就先 read 这个文件"
            .into(),
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
                "{} 已存在；覆盖它必须给出 expected_version（先 read 一次，抄抬头里「版本」后面那一串）",
                path.display()
            ),
        }),
        (Some(actual), Some(expected)) if expected.matches(&actual.hash) => Ok(()),
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
    use crate::tools::test_support::{context, writer};
    use komo_kernel::types::digest::ContentHash;

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
        assert_eq!(plan.recovery, RecoveryMode::NoSafeRecovery);

        let result = write_result(&tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap());
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
        // 空串、`{}`、`null` 与没给是同一回事：都要叫模型先 read。
        for args in [
            serde_json::json!({ "path": "a.txt", "content": "new\n" }),
            serde_json::json!({ "path": "a.txt", "content": "new\n", "expected_version": "" }),
            serde_json::json!({ "path": "a.txt", "content": "new\n", "expected_version": {} }),
            serde_json::json!({ "path": "a.txt", "content": "new\n", "expected_version": null }),
        ] {
            let plan = tool.prepare(args, &ctx).await.unwrap();
            let error = tool
                .execute(plan, &ctx, &mut writer(&ctx))
                .await
                .unwrap_err();
            let ToolError::VersionConflict { path } = &error else {
                panic!("{error:?}")
            };
            assert!(path.contains("先 read 一次"), "{path}");
        }
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
            .execute(plan, &ctx, &mut writer(&ctx))
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
        let result = write_result(&tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap());
        assert!(!result.created);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "new\n"
        );
    }

    /// 抬头里印的那一截（或 `{"hash": …}`）就能覆盖；回执再印新版本，接着写不用再读。
    /// 认不出的形状说清该传什么。
    #[tokio::test]
    async fn the_shown_version_is_enough_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let ctx = context(dir.path());
        let tool = WriteTool::new();
        let shown = FileVersion::of(b"old\n", None).shown().to_string();
        let plan = tool
            .prepare(
                serde_json::json!({ "path": "a.txt", "content": "new\n", "expected_version": shown }),
                &ctx,
            )
            .await
            .unwrap();
        let output = tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap();
        let next = FileVersion::of(b"new\n", None).shown().to_string();
        assert!(
            output
                .preview
                .as_deref()
                .unwrap()
                .ends_with(&format!("版本 {next}")),
            "{:?}",
            output.preview
        );

        let plan = tool
            .prepare(
                serde_json::json!({
                    "path": "a.txt",
                    "content": "newer\n",
                    "expected_version": { "hash": next }
                }),
                &ctx,
            )
            .await
            .unwrap();
        tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "newer\n"
        );

        for bad in [serde_json::json!(42), serde_json::json!({ "size": 3 })] {
            let error = tool
                .prepare(
                    serde_json::json!({ "path": "a.txt", "content": "x", "expected_version": bad }),
                    &ctx,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&error, ToolError::InvalidArguments { message } if message.contains("版本")),
                "{error:?}"
            );
        }
    }

    /// 数据目录与工作区同在一个临时目录下：`home/` 是 komo 的数据目录，`ws/` 是工作区。
    fn komo_home() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        Vec<std::path::PathBuf>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::create_dir_all(dir.path().join("ws")).unwrap();
        let protected = vec![
            home.join("sessions"),
            home.join("state.db"),
            home.join("runtime"),
            home.join(".env"),
        ];
        (dir, home, protected)
    }

    /// §7 唯一的硬限制：komo 自己的状态不许被 `write` 改，拒在 `prepare`。
    #[tokio::test]
    async fn writing_into_komos_own_state_is_refused_at_prepare() {
        let (dir, home, protected) = komo_home();
        let ctx = context(&dir.path().join("ws"));
        let tool = WriteTool::new().protecting(protected);
        for target in [
            home.join("sessions/x"),
            home.join("state.db-wal"),
            home.join(".env"),
        ] {
            let error = tool
                .prepare(
                    serde_json::json!({ "path": target.display().to_string(), "content": "x" }),
                    &ctx,
                )
                .await
                .unwrap_err();
            assert!(matches!(error, ToolError::Denied { .. }), "{error:?}");
            let text = error.to_string();
            assert!(text.contains("§8.10"), "{text}");
            assert!(
                text.contains(&target.file_name().unwrap().to_string_lossy().to_string()),
                "理由点名路径：{text}"
            );
            assert!(!target.exists() || target.is_dir(), "一个字节都没写");
        }

        // 工作区照常写。
        let plan = tool
            .prepare(serde_json::json!({ "path": "a.txt", "content": "x" }), &ctx)
            .await
            .unwrap();
        tool.execute(plan, &ctx, &mut writer(&ctx)).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ws/a.txt")).unwrap(),
            "x"
        );
    }

    /// 带 `then_run` 的组合调用：改动那一步落在受保护路径上，整份计划不成立，命令不跑。
    #[tokio::test]
    async fn a_fused_write_into_komos_state_never_runs_its_command() {
        let (dir, home, protected) = komo_home();
        let ctx = context(&dir.path().join("ws"));
        let marker = dir.path().join("ws/ran");
        let error = WriteTool::new()
            .protecting(protected)
            .prepare(
                serde_json::json!({
                    "path": home.join(".env").display().to_string(),
                    "content": "X=1\n",
                    "then_run": { "command": format!("touch {}", marker.display()) },
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied { .. }), "{error:?}");
        assert!(!marker.exists(), "命令一次都没跑");
        assert!(!home.join(".env").exists());
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
