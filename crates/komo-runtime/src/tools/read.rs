//! `read`：路径 + 读取范围，返回文本和文件版本；大文件截断并**明确显示未读范围**
//! （§4）。
//!
//! 截断不是"少给一点"，是"给一部分并说清楚少了哪一段"：一个把第 200 行当成文件末尾
//! 的模型，会自信地报告一个不存在的结论。
//!
//! `path` 除了本地路径，还认得 §六 的三种**只读资源入口**（`skill://` / `tool://` /
//! `artifact://`）：它们进计划时是资源目标（逻辑 URI + 解析出来的真实目标），磁盘上的那
//! 几条走的是和本地路径**同一套**读法；`tool://` 没有磁盘目标，正文从这次能力面现算。
//! 拒绝（越权、面外、不存在的 skill…）发生在 `prepare`——**不进计划就没有执行**。

use async_trait::async_trait;
use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::ids::OperationId;
use komo_kernel::types::plan::{
    ExecutionPlan, Operation, PlanTarget, PlanVersions, RecoveryMode, TargetAccess,
};
use komo_kernel::types::refs::ToolResultStatus;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

use super::resources::{self, ResolvedResource, ResourceError};
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
            description: "读取一份文本：本地文件，或者 skill:// / tool:// / artifact:// \
                 三种只读资源入口。大文件会截断，并列出未读的行范围。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "要读的东西。可以是本地路径（相对路径按工作目录解析），也可以是资源入口：`skill://<skill>/<文件>`（那份 skill 里的文件，例如 skill://review/SKILL.md）、`tool://<工具>/schema|doc`（这次能力面里的工具参数 schema 或说明）、`artifact://files/<名字>`（本次会话自己的产物）、`artifact://<run>/<call>/<attempt>/stdout|stderr|result`（某次工具输出的完整正文——正文被截断时里面会给这条引用）" },
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
        let target = target_of(&args.path, ctx)?;
        Ok(ExecutionPlan {
            operation_id: OperationId::new_at(plan_time()),
            source: ctx.source.clone(),
            tool: "read".into(),
            operation: Operation::ReadFile,
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args: normalized(&args)?,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![target],
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
        ctx: &ToolContext,
        // 文件工具不产生流式输出：结构化结果由 executor 发布。
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        let plan = plan.plan();
        let args: ReadArgs = parse_args(plan.args.clone(), "read")?;
        let target = plan.targets.first().ok_or_else(|| ToolError::Failed {
            message: "计划里没有目标".into(),
        })?;

        let result = match target.uri().and_then(resources::virtual_of) {
            // 虚拟入口：正文从**这次能力面**的说明书现算，不碰磁盘。版本就是这份正文的
            // 哈希——它是这次真的交给模型的那份内容，不是某个文件在某一刻的样子。
            Some(virtual_resource) => {
                let text = resources::virtual_content(&virtual_resource, &ctx.mounts.tools);
                let version = FileVersion::of(text.as_bytes(), None);
                slice(&text, &version, &args, self.byte_limit, &target.to_string())
            }
            None => {
                let path = target.path().ok_or_else(|| ToolError::Failed {
                    message: format!("{target} 没有磁盘目标"),
                })?;
                let Some((bytes, version)) = current(path)? else {
                    return Err(ToolError::Failed {
                        message: missing_message(target),
                    });
                };
                let text = as_text(bytes, path)?;
                // 抬头与截断提示：资源印**逻辑入口**（模型给的就是它），本地路径印的还是
                // 模型给的那个字符串——解析出来的绝对路径不是它要的那份观察（§8.3）。
                let display = match target.uri() {
                    Some(uri) => uri.to_string(),
                    None => args.path.clone(),
                };
                slice(&text, &version, &args, self.byte_limit, &display)
            }
        };
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

/// 这次读取触及的目标。
///
/// 本地路径照旧（符号链接解析掉，规则只看这个）；资源入口按 §六 解析：磁盘上的那条把
/// **逻辑 URI 与真实目标**都放进计划（审批看前者、规则匹配看后者），`tool://` 那条没有
/// 真实目标。`read` 要的是一份文件——给了一组根（`skill://`）是错的，那是 `rg` 的 path。
fn target_of(raw: &str, ctx: &ToolContext) -> Result<PlanTarget, ToolError> {
    let Some(uri) = resources::entry(raw).map_err(ResourceError::into_tool_error)? else {
        // 不是资源：本地路径的行为一个字都不变。
        let path = super::paths::resolve(raw, &ctx.cwd)?;
        return Ok(PlanTarget::local(path, TargetAccess::Read));
    };
    match resources::resolve(&uri, &ctx.mounts).map_err(ResourceError::into_tool_error)? {
        ResolvedResource::File(path) => {
            Ok(PlanTarget::resource(uri, Some(path), TargetAccess::Read))
        }
        ResolvedResource::Virtual(_) => Ok(PlanTarget::resource(uri, None, TargetAccess::Read)),
        ResolvedResource::Roots(_) => Err(ResourceError::Directory {
            uri: uri.to_string(),
        }
        .into_tool_error()),
    }
}

/// 读到的东西不在盘上。资源那条要把"这个入口在本会话里没有对应的落盘"说清楚——它和
/// "文件名叫错了"是两件事。
fn missing_message(target: &PlanTarget) -> String {
    match (target.uri(), target.path()) {
        (Some(uri), Some(path)) => format!(
            "{uri} 在本会话里没有对应的文件（{} 不存在）",
            path.display()
        ),
        (_, Some(path)) => format!("{} 不存在", path.display()),
        (Some(uri), None) => format!("{uri} 没有磁盘目标"),
        (None, None) => "计划里没有目标".into(),
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

/// 一次 `read` 交给模型多少正文。
///
/// **不是账本那 1 KiB**：JSONL 里那条事件只留前 1 KiB（行要小），而模型能看到多少由投影层
/// 按 `model_result_bytes` 决定。这里给的是一个够它裁的量——400 字符那种做法会让模型每次
/// 读文件都只看见开头一小截，而它以为那就是全部。
pub const PREVIEW_BYTES: usize = 8 * 1024;

fn preview_of(result: &ReadResult) -> String {
    let header = if result.truncated {
        let ranges: Vec<String> = result
            .unread
            .iter()
            .map(|range| format!("{}–{}", range.from_line, range.to_line))
            .collect();
        format!(
            "{} 行 {}–{}（共 {} 行，未读 {}）",
            result.path,
            result.start_line,
            result.end_line,
            result.total_lines,
            ranges.join("、")
        )
    } else {
        format!("{} 共 {} 行", result.path, result.total_lines)
    };
    let room = PREVIEW_BYTES.saturating_sub(header.len() + 1);
    format!("{header}\n{}", clip(&result.text, room))
}

/// 按字符边界截到不超过 `limit` 字节。
fn clip(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut cut = limit;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{approved, context, writer};
    use komo_kernel::types::resource::{ResourceMounts, SkillMount};
    use std::path::PathBuf;

    fn read_result(output: &ToolOutput) -> ReadResult {
        serde_json::from_value(output.result.clone()).expect("read 的结果")
    }

    /// 资源那几条共用的现场：一份 skill + 一个会话内容目录。
    struct Scene {
        dir: tempfile::TempDir,
        session: PathBuf,
    }

    impl Scene {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let session = dir.path().join("s1");
            std::fs::create_dir_all(session.join("artifacts")).unwrap();
            let skill = dir.path().join("skills").join("review");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), "---\nname: review\n---\n照着做\n").unwrap();
            Self { dir, session }
        }

        fn skill_file(&self) -> PathBuf {
            std::fs::canonicalize(self.dir.path().join("skills/review/SKILL.md")).unwrap()
        }

        fn mounts(&self) -> ResourceMounts {
            ResourceMounts {
                skill_dirs: vec![std::fs::canonicalize(self.dir.path().join("skills")).unwrap()],
                skills: vec![SkillMount {
                    name: "review".into(),
                    dir: std::fs::canonicalize(self.dir.path().join("skills/review")).unwrap(),
                }],
                session_root: Some(std::fs::canonicalize(&self.session).unwrap()),
                tools: std::sync::Arc::from([ToolDefinition {
                    name: "read".into(),
                    description: "读取一份文本".into(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                        "required": ["path"]
                    }),
                }]),
            }
        }

        /// 带这套挂载点的上下文。
        fn ctx(&self) -> ToolContext {
            let mut ctx = context(self.dir.path());
            ctx.mounts = self.mounts();
            ctx
        }
    }

    /// 磁盘上的资源走的是和本地路径**同一套**读法：计划里逻辑 URI 与真实目标都在，正文的
    /// 抬头印的是 URI。
    #[tokio::test]
    async fn a_skill_uri_reads_the_file_under_that_skill() {
        let scene = Scene::new();
        let ctx = scene.ctx();
        let tool = ReadTool::new();
        let plan = tool
            .prepare(
                serde_json::json!({ "path": "skill://review/SKILL.md" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(plan.operation, Operation::ReadFile);
        assert_eq!(plan.recovery, RecoveryMode::SafeReread);
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(
            plan.targets[0].describe(),
            format!(
                "skill://review/SKILL.md（{}）",
                scene.skill_file().display()
            ),
            "审批那一行：逻辑入口 + 真实目标"
        );
        assert_eq!(plan.targets[0].access, TargetAccess::Read);

        let result = read_result(
            &tool
                .execute(approved(plan), &ctx, &mut writer(&ctx))
                .await
                .unwrap(),
        );
        assert_eq!(result.path, "skill://review/SKILL.md", "抬头印逻辑入口");
        assert!(result.text.contains("照着做"), "{}", result.text);
        assert_eq!(result.total_lines, 4);
    }

    /// 虚拟入口：正文从**这次能力面**现算，不碰磁盘（会话目录里连 `tool-output/` 都没有）。
    #[tokio::test]
    async fn a_tool_uri_is_computed_without_touching_the_disk() {
        let scene = Scene::new();
        let ctx = scene.ctx();
        let tool = ReadTool::new();

        let plan = tool
            .prepare(serde_json::json!({ "path": "tool://read/schema" }), &ctx)
            .await
            .unwrap();
        assert!(plan.targets[0].is_virtual());
        assert_eq!(plan.targets[0].path(), None, "虚拟入口不给它编路径");
        assert_eq!(plan.targets[0].describe(), "tool://read/schema");
        let result = read_result(
            &tool
                .execute(approved(plan), &ctx, &mut writer(&ctx))
                .await
                .unwrap(),
        );
        assert_eq!(result.path, "tool://read/schema");
        assert!(result.text.contains("\"required\""), "{}", result.text);
        assert_eq!(
            result.version.size,
            result.text.len() as u64,
            "版本是这份正文自己的哈希与大小"
        );

        // 说明与工具名也读得到。
        for (raw, needle) in [("tool://read/doc", "读取一份文本"), ("tool://", "read")] {
            let plan = tool
                .prepare(serde_json::json!({ "path": raw }), &ctx)
                .await
                .unwrap();
            let result = read_result(
                &tool
                    .execute(approved(plan), &ctx, &mut writer(&ctx))
                    .await
                    .unwrap(),
            );
            assert!(result.text.contains(needle), "{raw}: {}", result.text);
        }

        // 虚拟正文也受字节上限管：截断要说出来。
        let small = ReadTool::with_byte_limit(16);
        let plan = small
            .prepare(serde_json::json!({ "path": "tool://read/schema" }), &ctx)
            .await
            .unwrap();
        let result = read_result(
            &small
                .execute(approved(plan), &ctx, &mut writer(&ctx))
                .await
                .unwrap(),
        );
        assert!(result.truncated);
        assert!(!result.unread.is_empty());
    }

    /// 越权、面外、名字不存在、给了一组根、没有会话目录——**都在 `prepare` 拒绝**，
    /// 而且说的话要能照着改。
    #[tokio::test]
    async fn a_resource_that_does_not_hold_up_is_refused_before_any_plan() {
        let scene = Scene::new();
        let ctx = scene.ctx();
        let tool = ReadTool::new();
        for (raw, needle) in [
            ("skill://review/../../etc/passwd", "://"),
            ("skill://nope/SKILL.md", "komo skills list"),
            ("tool://write/schema", "不在这次能力面里"),
            ("skill://", "一组根"),
        ] {
            let error = tool
                .prepare(serde_json::json!({ "path": raw }), &ctx)
                .await
                .unwrap_err();
            let ToolError::Failed { message } = &error else {
                panic!("{raw}：{error:?}")
            };
            assert!(message.contains(needle), "{raw}：{message}");
        }

        // 精简装配（没有会话内容目录）：`artifact://` 两条都拒。
        let bare = context(scene.dir.path());
        const RUN: &str = "01a0c495-d37a-7036-83bc-cb9981ba308a";
        const CALL: &str = "01a0c495-d37a-7036-83bc-cb9981ba308b";
        const ATTEMPT: &str = "01a0c495-d37a-7036-83bc-cb9981ba308c";
        let outputs = [
            "artifact://files/report.md".to_string(),
            format!("artifact://{RUN}/{CALL}/{ATTEMPT}/stdout"),
        ];
        for raw in outputs {
            let error = tool
                .prepare(serde_json::json!({ "path": raw }), &bare)
                .await
                .unwrap_err();
            let ToolError::Failed { message } = &error else {
                panic!("{raw}：{error:?}")
            };
            assert!(message.contains("会话"), "{raw}：{message}");
        }
    }

    /// 本地路径的行为一个字不变：不是资源就照旧（解析成真实路径，正文抬头还是模型给的那个
    /// 字符串）。
    #[tokio::test]
    async fn a_plain_path_behaves_exactly_as_before() {
        let scene = Scene::new();
        std::fs::write(scene.dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let ctx = scene.ctx();
        let tool = ReadTool::new();
        let plan = tool
            .prepare(serde_json::json!({ "path": "a.txt" }), &ctx)
            .await
            .unwrap();
        assert!(plan.targets[0].uri().is_none());
        assert_eq!(
            plan.targets[0].path(),
            Some(
                std::fs::canonicalize(scene.dir.path().join("a.txt"))
                    .unwrap()
                    .as_path()
            )
        );
        let result = read_result(
            &tool
                .execute(approved(plan), &ctx, &mut writer(&ctx))
                .await
                .unwrap(),
        );
        assert_eq!(result.path, "a.txt", "本地路径印的还是模型给的那个字符串");
    }

    /// **投影给模型的入口必须真能读回来**（§8.3：超限时给"可读取的完整输出"）。
    ///
    /// 这条把两半接起来：投影那一行里的 URI → 原样交给 `read` → 读到的就是那份落盘的完整
    /// 正文。名字与拼法各改各的就会在这里断掉。
    #[tokio::test]
    async fn the_uri_the_projection_hands_the_model_reads_back_through_read() {
        use komo_kernel::projection::{ProjectionContext, ToolResultFacts, project};
        use komo_kernel::types::digest::ContentHash;
        use komo_kernel::types::refs::{ContentRef, OutputRef, ToolResultStatus};

        let scene = Scene::new();
        let run = "01a0c495-d37a-7036-83bc-cb9981ba308a";
        let call = "01a0c495-d37a-7036-83bc-cb9981ba308b";
        let attempt = "01a0c495-d37a-7036-83bc-cb9981ba308c";
        let relative = format!("tool-output/{run}/{call}/{attempt}/output.json");
        let body = "exit=0\n完整的那一份正文\n";
        let path = scene.session.join(&relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();

        let output = OutputRef(ContentRef {
            path: relative,
            size: body.len() as u64,
            hash: ContentHash::of_str(body),
            pointer: None,
        });
        let facts = ToolResultFacts {
            tool: "shell",
            status: ToolResultStatus::Completed,
            elapsed_ms: 3,
            // 正文没给（超限时就是这样）：只剩引用可给。
            text: None,
            output: &output,
            stdout: None,
            stderr: None,
            artifacts: &[],
        };
        let printed = project(
            &facts,
            &ProjectionContext {
                model_result_bytes: 64,
            },
        );
        let uri = printed
            .lines()
            .find_map(|line| line.strip_prefix("完整输出："))
            .expect("投影给了引用")
            .split('（')
            .next()
            .expect("引用")
            .to_string();
        assert_eq!(uri, format!("artifact://{run}/{call}/{attempt}/result"));

        let ctx = scene.ctx();
        let tool = ReadTool::new();
        let plan = tool
            .prepare(serde_json::json!({ "path": uri }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            plan.targets[0].describe(),
            format!(
                "artifact://{run}/{call}/{attempt}/result（{}）",
                path.display()
            )
        );
        let result = read_result(
            &tool
                .execute(approved(plan), &ctx, &mut writer(&ctx))
                .await
                .unwrap(),
        );
        assert_eq!(result.text, body, "投影指的那一份就是读到的那一份");
        assert_eq!(result.path, uri);
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
        assert_eq!(
            plan.targets[0].path(),
            Some(std::fs::canonicalize(&file).unwrap().as_path())
        );

        let output = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap();
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
        let result = read_result(
            &tool
                .execute(approved(plan), &ctx, &mut writer(&ctx))
                .await
                .unwrap(),
        );

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
        let result = read_result(
            &tool
                .execute(approved(plan), &ctx, &mut writer(&ctx))
                .await
                .unwrap(),
        );
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
        let result = read_result(
            &tool
                .execute(approved(plan), &ctx, &mut writer(&ctx))
                .await
                .unwrap(),
        );
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
        let error = tool
            .execute(approved(plan), &ctx, &mut writer(&ctx))
            .await
            .unwrap_err();
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
