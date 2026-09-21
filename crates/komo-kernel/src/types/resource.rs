//! 资源命名空间（§六）：`skill://` / `tool://` / `artifact://` 三种只读入口。
//!
//! 模型仍然只用现有的 `read` / `rg`，把资源 URI 当路径交给它们。**URI 不是"特殊文件
//! 路径"**：它在执行计划里是一个明确的目标类型（[`TargetRef`]），计划同时带着**逻辑**
//! URI 与解析出来的真实目标——所以审批与审计回答的是"允许读哪个逻辑资源"，而规则匹配的
//! 仍然是真实路径。
//!
//! ```text
//! skill://<skill>/<path…>                    → <skill 根>/<skill>/<path…>   （只读）
//! skill://                                   → 所有 skill 根（`rg` 的搜索根）
//! tool://<tool>/schema|doc                   → 虚拟：内容由运行时从能力面现算
//! tool://                                    → 这次能力面里的工具名
//! artifact://files/<path…>                   → <会话>/artifacts/<path…>     （只读）
//! artifact://<run>/<call>/<attempt>/<file>   → <会话>/tool-output/…         （只读）
//! ```
//!
//! **首阶段不做**（§六）：memory / session 的任意写入、shell / Python 里的 URI 展开、
//! 通用 VFS。本地路径的用法一个字都不变。
//!
//! 解析（把 URI 变成真实目标、`tool://` 现算内容）需要 I/O，落在运行时
//! （`komo_runtime::tools::resources`）；这里只放**语法与类型**，以及"哪些资源配得上
//! 哪个根"的那份纯数据（[`ResourceMounts`]）。

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::types::ids::{AttemptId, RunId, ToolCallId};
use crate::types::tool::ToolDefinition;

/// `tool://<tool>/…` 的两种入口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPart {
    /// 交给模型的那份 JSON Schema。
    Schema,
    /// 这个工具做什么、参数怎么给（就是它给模型看的那段说明）。
    Doc,
}

impl ToolPart {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Schema => "schema",
            Self::Doc => "doc",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "schema" => Some(Self::Schema),
            "doc" => Some(Self::Doc),
            _ => None,
        }
    }
}

/// 工具输出的三个文件（`tool-output/<run>/<call>/<attempt>/`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFile {
    Stdout,
    Stderr,
    /// 落盘的那份 `output.json`（完整结果，投影出来的正文就是它的一个窗口）。
    Result,
}

impl OutputFile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Result => "result",
        }
    }

    /// 在 `tool-output/` 里它对应哪个文件——**只有这一处拼文件名**，落盘与读回同源。
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout.txt",
            Self::Stderr => "stderr.txt",
            Self::Result => "output.json",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "stdout" => Some(Self::Stdout),
            "stderr" => Some(Self::Stderr),
            "result" => Some(Self::Result),
            _ => None,
        }
    }
}

/// 一个资源 URI。
///
/// 三种 scheme 里只有 `skill` 与 `artifact` 有磁盘目标；`tool` 是虚拟入口（工具说明或
/// schema 从能力面现算），所以它解析不出路径——**不要**给它编一个假路径出来。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResourceUri {
    /// `skill://<skill>/<path…>`。
    Skill { name: String, path: String },
    /// `skill://`：所有 skill 根。
    SkillRoot,
    /// `tool://<tool>/<part>`。
    Tool { name: String, part: ToolPart },
    /// `tool://`：这次能力面里的工具名。
    ToolRoot,
    /// `artifact://files/<path…>`：产物。
    ArtifactFile { path: String },
    /// `artifact://files`：产物根。
    ArtifactRoot,
    /// `artifact://<run>/<call>/<attempt>/<stdout|stderr|result>`：工具输出。
    Output {
        run: RunId,
        call: ToolCallId,
        attempt: AttemptId,
        file: OutputFile,
    },
}

/// 解析资源 URI 时的失败。**看不懂就是错**：不做任何猜测性的补全（那不是宽容，是把
/// 一次越权尝试读成一次正常读取）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UriError {
    #[error("这不是一个资源 URI：{0}")]
    NotAResource(String),
    #[error("资源 URI 里有不认识的段：{0}")]
    UnknownPart(String),
    #[error("资源 URI 里不许有 `..` 或空段（那是想跳出挂载点）：{0}")]
    Traversal(String),
    #[error("资源 URI 缺了要读的那一段：{0}")]
    Missing(String),
    #[error("资源 URI 里的 {part} 不是合法标识：{value}")]
    BadId { part: &'static str, value: String },
}

/// `artifact://files` 之外，`artifact://` 的第一个段只可能是 `run`（工具输出）——
/// 而 run ID 是 UUID，`files` 永远不会撞上它。这个常量把这条约定写在一处。
const ARTIFACT_FILES: &str = "files";

impl ResourceUri {
    /// 解析模型给的那个字符串。
    ///
    /// 只认 `skill://` / `tool://` / `artifact://`；`scheme:` 与 `//` 之间不许有空白，
    /// 分段不许空、不许 `..`，路径段里的 `%` 不解释（本机路径本来就是字面量）。
    pub fn parse(raw: &str) -> Result<Self, UriError> {
        let raw = raw.trim();
        let (scheme, rest) = raw
            .split_once("://")
            .ok_or_else(|| UriError::NotAResource(raw.to_string()))?;
        if scheme.contains(char::is_whitespace) || scheme.is_empty() {
            return Err(UriError::NotAResource(raw.to_string()));
        }
        let segments = split_segments(rest).map_err(|()| UriError::Traversal(raw.to_string()))?;
        match scheme {
            "skill" => parse_skill(segments, raw),
            "tool" => parse_tool(segments, raw),
            "artifact" => parse_artifact(segments, raw),
            other => Err(UriError::NotAResource(format!("{other}://"))),
        }
    }

    /// 这个 scheme 是什么（日志与审批用）。
    pub fn scheme(&self) -> &'static str {
        match self {
            Self::Skill { .. } | Self::SkillRoot => "skill",
            Self::Tool { .. } | Self::ToolRoot => "tool",
            Self::ArtifactFile { .. } | Self::ArtifactRoot | Self::Output { .. } => "artifact",
        }
    }

    /// 资源内部的那段相对路径（`skill://a/b/c` 的 `b/c`、`artifact://files/b/c` 的 `b/c`）。
    ///
    /// **拼进真实路径之前必须再过一次 `..` 检查**：URI 可以是从旧日志或模型那边来的，
    /// 解析器只保证它自己解析过的那些段是干净的。
    pub fn relative_path(&self) -> Option<&str> {
        match self {
            Self::Skill { path, .. } | Self::ArtifactFile { path } => Some(path),
            _ => None,
        }
    }

    /// 这是个虚拟入口吗（`tool://`，没有磁盘目标）。
    pub fn is_virtual(&self) -> bool {
        matches!(self, Self::Tool { .. } | Self::ToolRoot)
    }
}

fn split_segments(rest: &str) -> Result<Vec<&str>, ()> {
    if rest.is_empty() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for segment in rest.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(());
        }
        out.push(segment);
    }
    Ok(out)
}

fn join_path(segments: &[&str]) -> String {
    segments.join("/")
}

fn parse_skill(segments: Vec<&str>, raw: &str) -> Result<ResourceUri, UriError> {
    match segments.as_slice() {
        [] => Ok(ResourceUri::SkillRoot),
        [name] => Err(UriError::Missing(format!(
            "{raw}：要给到文件，例如 `skill://{name}/SKILL.md`"
        ))),
        [name, path @ ..] => Ok(ResourceUri::Skill {
            name: (*name).to_string(),
            path: join_path(path),
        }),
    }
}

fn parse_tool(segments: Vec<&str>, raw: &str) -> Result<ResourceUri, UriError> {
    match segments.as_slice() {
        [] => Ok(ResourceUri::ToolRoot),
        [name, part] => {
            let part =
                ToolPart::parse(part).ok_or_else(|| UriError::UnknownPart(raw.to_string()))?;
            Ok(ResourceUri::Tool {
                name: (*name).to_string(),
                part,
            })
        }
        [name, ..] => Err(UriError::UnknownPart(format!(
            "{raw}：`tool://{name}/…` 只有 `schema` 与 `doc` 两种"
        ))),
    }
}

fn parse_artifact(segments: Vec<&str>, raw: &str) -> Result<ResourceUri, UriError> {
    match segments.as_slice() {
        [] => Err(UriError::Missing(format!(
            "{raw}：要指明是哪一份产物或工具输出"
        ))),
        [ARTIFACT_FILES] => Ok(ResourceUri::ArtifactRoot),
        [ARTIFACT_FILES, path @ ..] => Ok(ResourceUri::ArtifactFile {
            path: join_path(path),
        }),
        [run, call, attempt, file] => {
            let run = RunId::parse(run).map_err(|_| UriError::BadId {
                part: "run",
                value: (*run).to_string(),
            })?;
            let call = ToolCallId::parse(call).map_err(|_| UriError::BadId {
                part: "call",
                value: (*call).to_string(),
            })?;
            let attempt = AttemptId::parse(attempt).map_err(|_| UriError::BadId {
                part: "attempt",
                value: (*attempt).to_string(),
            })?;
            let file =
                OutputFile::parse(file).ok_or_else(|| UriError::UnknownPart(raw.to_string()))?;
            Ok(ResourceUri::Output {
                run,
                call,
                attempt,
                file,
            })
        }
        [first, ..] if *first != ARTIFACT_FILES => Err(UriError::UnknownPart(format!(
            "{raw}：`artifact://` 底下要么是 `files/<路径>`，要么是 `<run>/<call>/<attempt>/<stdout|stderr|result>`"
        ))),
        _ => Err(UriError::UnknownPart(raw.to_string())),
    }
}

impl fmt::Display for ResourceUri {
    /// **规范化**的拼法：审批、投影正文、日志用的是同一份，改一处就全改。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skill { name, path } => write!(f, "skill://{name}/{path}"),
            Self::SkillRoot => f.write_str("skill://"),
            Self::Tool { name, part } => write!(f, "tool://{name}/{}", part.as_str()),
            Self::ToolRoot => f.write_str("tool://"),
            Self::ArtifactFile { path } => write!(f, "artifact://{ARTIFACT_FILES}/{path}"),
            Self::ArtifactRoot => write!(f, "artifact://{ARTIFACT_FILES}"),
            Self::Output {
                run,
                call,
                attempt,
                file,
            } => write!(f, "artifact://{run}/{call}/{attempt}/{}", file.as_str()),
        }
    }
}

/// 执行计划触及的一个目标（§六）。
///
/// 序列化上**与改造前逐字兼容**：本地路径那条写成 `{"path": "…"}`，与旧的
/// `PlanTarget` 一模一样——所以旧审批计划仍按原哈希校验，不会因为多了资源这一维失效
/// （§八）。资源那条多出 `uri`（逻辑入口，审批与审计看它）与 `resolved`（计划那一刻
/// 解析出来的真实目标；`tool://` 这类虚拟入口没有）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TargetRef {
    LocalPath {
        path: PathBuf,
    },
    Resource {
        uri: ResourceUri,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolved: Option<PathBuf>,
    },
}

impl TargetRef {
    /// 本地路径目标。
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Self::LocalPath { path: path.into() }
    }

    /// 资源目标；`resolved` = 计划那一刻解析出来的真实目标。
    pub fn resource(uri: ResourceUri, resolved: Option<PathBuf>) -> Self {
        Self::Resource { uri, resolved }
    }

    /// 解析出来的真实目标（规则匹配看它）。虚拟入口没有。
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::LocalPath { path } => Some(path),
            Self::Resource { resolved, .. } => resolved.as_deref(),
        }
    }

    /// 逻辑资源入口（本地路径没有）。
    pub fn uri(&self) -> Option<&ResourceUri> {
        match self {
            Self::LocalPath { .. } => None,
            Self::Resource { uri, .. } => Some(uri),
        }
    }

    /// 没有磁盘目标的那一类（`tool://`）——规则表为它单留一条。
    pub fn is_virtual(&self) -> bool {
        matches!(self, Self::Resource { resolved: None, .. })
    }
}

impl fmt::Display for TargetRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalPath { path } => write!(f, "{}", path.display()),
            // 审批要回答的是"允许读哪个**逻辑**资源"，所以印 URI；真实目标在它后面
            // 已经有了（`PlanTarget` 里），需要时由渲染方补。
            Self::Resource { uri, .. } => write!(f, "{uri}"),
        }
    }
}

impl From<PathBuf> for TargetRef {
    fn from(path: PathBuf) -> Self {
        Self::local(path)
    }
}

/// 一次装配里的一份 skill：名字 + 它的目录。
///
/// 装配那一刻从**活的** skill 注册表抄下来（影子规则、`disable`、门控都已经算过），所以
/// 工具那边只做一次查表。两份事实——提示目录按注册表一份、读的时候另找一份——正是 §5.6
/// 要避免的那件事：提示里看见的名字读不到，或者读到的是被盖住的那一份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMount {
    /// 注册表里的名字（frontmatter 的 `name`，没写就是目录名）。
    pub name: String,
    /// 那份 skill 的目录（`SKILL.md` 所在处）。
    pub dir: PathBuf,
}

/// 解析资源 URI 时要看的挂载点（§4.7）。
///
/// **纯数据**：由 Gateway 装配（skill 那一份来自这一次的活注册表、会话根来自这一次的
/// Session、能力面来自这一次的 `AgentSurface`），随 [`crate::types::tool::ToolContext`]
/// 交给工具。解析本身在运行时——它要做 I/O（链接解析、现算 schema），而 kernel 不做 I/O。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceMounts {
    /// skill 根，按顺序找：`skill://` 这条搜索根用它，顺序就是注册表的优先级。
    pub skill_dirs: Vec<PathBuf>,
    /// 按名字查的那一份（`skill://<name>/…`）。装配那一刻的快照。
    pub skills: Vec<SkillMount>,
    /// 这个会话自己的内容目录（`<sessions>/<session>`）。
    pub session_root: Option<PathBuf>,
    /// 这次运行能力面里的工具**定义**——`tool://` 只能读它里面的。
    ///
    /// 与 [`Self::skills`] 同一个道理：装配那一刻从这一次的 `AgentSurface` 渲染出来的
    /// 那一份抄下来，名字与 schema 因此是**同一份事实**（分成"名字一份、schema 另一份"
    /// 就会出现"名字里有、schema 拿不到"）。`Arc` 是因为 `ToolContext` 每个调用克隆一次，
    /// 而一份 schema 有一 KB 量级。
    pub tools: Arc<[ToolDefinition]>,
}

impl ResourceMounts {
    /// 这只是不是这次能力面里的工具。
    pub fn allows_tool(&self, name: &str) -> bool {
        self.tool(name).is_some()
    }

    /// 这个名字的工具定义（装配那一刻能力面渲染出来的那一份）。
    pub fn tool(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    /// 这个名字的 skill 在哪（装配那一刻注册表认的那一份）。
    pub fn skill(&self, name: &str) -> Option<&Path> {
        self.skills
            .iter()
            .find(|skill| skill.name == name)
            .map(|skill| skill.dir.as_path())
    }

    /// 这次允许当搜索根的那些真实根（`rg` 的 `skill://`、`artifact://files`）。
    pub fn roots_for(&self, uri: &ResourceUri) -> Vec<PathBuf> {
        match uri {
            ResourceUri::SkillRoot => self.skill_dirs.clone(),
            ResourceUri::ArtifactRoot => self
                .session_root
                .as_ref()
                .map(|root| vec![root.join("artifacts")])
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

/// 会话相对路径（`artifacts/<run>/<path…>`）→ 它的资源入口（`artifact://files/…`）。
///
/// **落盘与投影共用这一处映射**：`body.artifacts` 里存的是会话相对路径，模型看到的那句
/// 引用是资源入口，两边各拼一次就会有一天对不上。拼出来的串**再过一次解析器**——`..`、
/// 空段这些校验因此不用在这里重写一遍。
pub fn artifact_uri(session_relative: &str) -> Option<ResourceUri> {
    let rest = session_relative.strip_prefix("artifacts/")?;
    ResourceUri::parse(&format!("artifact://{ARTIFACT_FILES}/{rest}")).ok()
}

/// 这个工具输出的三个文件在磁盘上的位置（`<session>/tool-output/<run>/<call>/<attempt>/…`）。
///
/// 落盘（`komo-store`）与读回（`artifact://`）都用它——两处各拼一次就会有一天对不上。
pub fn output_file_path(session_root: &Path, uri: &ResourceUri) -> Option<PathBuf> {
    let ResourceUri::Output {
        run,
        call,
        attempt,
        file,
    } = uri
    else {
        return None;
    };
    Some(
        session_root
            .join("tool-output")
            .join(run.as_str())
            .join(call.as_str())
            .join(attempt.as_str())
            .join(file.file_name()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(raw: &str) -> ResourceUri {
        let uri = ResourceUri::parse(raw).unwrap_or_else(|error| panic!("{raw}：{error}"));
        assert_eq!(uri.to_string(), raw, "规范化之后要逐字回到原样");
        uri
    }

    #[test]
    fn the_three_schemes_parse_and_print_back_unchanged() {
        assert_eq!(
            round_trip("skill://review/SKILL.md"),
            ResourceUri::Skill {
                name: "review".into(),
                path: "SKILL.md".into()
            }
        );
        assert_eq!(round_trip("skill://"), ResourceUri::SkillRoot);
        assert_eq!(
            round_trip("tool://python/schema"),
            ResourceUri::Tool {
                name: "python".into(),
                part: ToolPart::Schema
            }
        );
        assert_eq!(round_trip("tool://"), ResourceUri::ToolRoot);
        assert_eq!(
            round_trip("artifact://files/out/report.md"),
            ResourceUri::ArtifactFile {
                path: "out/report.md".into()
            }
        );
        assert_eq!(round_trip("artifact://files"), ResourceUri::ArtifactRoot);

        // 工具输出那条：run / call / attempt 都是 UUID，id 要能解析。
        let run = RunId::from_raw("01a0c495-d37a-7036-83bc-cb9981ba308a");
        let call = ToolCallId::from_raw("01a0c495-d37a-7036-83bc-cb9981ba308b");
        let attempt = AttemptId::from_raw("01a0c495-d37a-7036-83bc-cb9981ba308c");
        let raw = format!("artifact://{run}/{call}/{attempt}/stdout");
        assert_eq!(
            ResourceUri::parse(&raw).unwrap(),
            ResourceUri::Output {
                run,
                call,
                attempt,
                file: OutputFile::Stdout
            }
        );
        assert_eq!(round_trip(&raw).to_string(), raw);
    }

    /// 越权的写法**当场**是错，不留给"拼进路径之后再说"。
    #[test]
    fn traversal_and_unknown_parts_are_refused() {
        for raw in [
            "skill://a/../../etc/passwd",
            "skill://a//b",
            "skill://a/./b",
            "artifact://files/../other-session/state.db",
            "skill://a/b/../../..",
        ] {
            assert!(
                matches!(ResourceUri::parse(raw), Err(UriError::Traversal(_))),
                "{raw} 该被判成越权"
            );
        }
        // 不是资源 URI 的东西照样是错——`read` 那侧先剥 scheme，轮不到这里。
        for raw in [
            "/etc/passwd",
            "http://example.com",
            "skill:/a/b",
            "file:///x",
        ] {
            assert!(
                ResourceUri::parse(raw).is_err(),
                "{raw} 不是资源 URI，不该解析成功"
            );
        }
        // 段数不对 / 不认识的段。
        assert!(matches!(
            ResourceUri::parse("tool://python/whatever"),
            Err(UriError::UnknownPart(_))
        ));
        assert!(
            matches!(
                ResourceUri::parse("tool://python/a/b"),
                Err(UriError::UnknownPart(_))
            ),
            "`tool://` 只认 schema 与 doc"
        );
        assert_eq!(
            round_trip("artifact://files/a/b/c/d/e"),
            ResourceUri::ArtifactFile {
                path: "a/b/c/d/e".into()
            },
            "产物那条就是一段路径，多深都行"
        );
        assert!(matches!(
            ResourceUri::parse("skill://review"),
            Err(UriError::Missing(_))
        ));
        assert!(matches!(
            ResourceUri::parse("artifact://"),
            Err(UriError::Missing(_))
        ));
        assert!(matches!(
            ResourceUri::parse("artifact://not-a-uuid/x/y/stdout"),
            Err(UriError::BadId { part: "run", .. })
        ));
    }

    /// 本地路径那一条**与旧行逐字兼容**：旧审批计划里存的就是 `{"path": …}`。
    #[test]
    fn a_local_target_still_reads_and_writes_the_old_shape() {
        let old = serde_json::json!({
            "path": "/tmp/w/a.txt",
            "access": "read",
            "expected_version": null,
        });
        let target: TargetRef = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(target, TargetRef::local("/tmp/w/a.txt"));
        assert_eq!(target.path(), Some(Path::new("/tmp/w/a.txt")));
        assert!(target.uri().is_none());
        assert!(!target.is_virtual());

        let plan_target: crate::types::plan::PlanTarget =
            serde_json::from_value(old.clone()).unwrap();
        assert_eq!(plan_target.path(), Some(Path::new("/tmp/w/a.txt")));
        assert_eq!(
            serde_json::to_value(&plan_target).unwrap()["path"],
            serde_json::json!("/tmp/w/a.txt")
        );
    }

    /// 资源目标带着逻辑 URI 与解析出来的真实目标，两样都要留在计划里。
    #[test]
    fn a_resource_target_keeps_both_the_uri_and_the_real_path() {
        let uri = ResourceUri::parse("skill://review/SKILL.md").unwrap();
        let target = TargetRef::resource(
            uri.clone(),
            Some(PathBuf::from("/home/u/.komo/skills/review/SKILL.md")),
        );
        assert_eq!(target.uri(), Some(&uri));
        assert_eq!(
            target.path(),
            Some(Path::new("/home/u/.komo/skills/review/SKILL.md"))
        );
        assert_eq!(target.to_string(), "skill://review/SKILL.md");

        let virtual_target = TargetRef::resource(ResourceUri::ToolRoot, None);
        assert!(virtual_target.is_virtual());
        assert_eq!(virtual_target.path(), None);
    }

    #[test]
    fn mounts_know_which_roots_a_search_gets() {
        let mounts = ResourceMounts {
            skill_dirs: vec![
                PathBuf::from("/shared/skills"),
                PathBuf::from("/home/u/.komo/skills"),
            ],
            skills: vec![SkillMount {
                name: "review".into(),
                dir: PathBuf::from("/shared/skills/review"),
            }],
            session_root: Some(PathBuf::from("/home/u/.komo/sessions/s1")),
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "读文件".into(),
                parameters: serde_json::json!({ "type": "object" }),
            }]
            .into(),
        };
        assert_eq!(
            mounts.roots_for(&ResourceUri::SkillRoot),
            mounts.skill_dirs,
            "`skill://` 的搜索根就是那几份 skill 根"
        );
        assert_eq!(
            mounts.roots_for(&ResourceUri::ArtifactRoot),
            vec![PathBuf::from("/home/u/.komo/sessions/s1/artifacts")]
        );
        assert!(mounts.allows_tool("read"));
        assert!(!mounts.allows_tool("write"), "能力面外的工具读不到说明");
        assert_eq!(
            mounts.tool("read").map(|tool| tool.name.as_str()),
            Some("read"),
            "`tool://` 的正文与名字来自同一份定义"
        );
        assert_eq!(mounts.tool("write"), None);
        assert_eq!(
            mounts.skill("review"),
            Some(Path::new("/shared/skills/review")),
            "按名字查的是装配那一刻注册表认的那一份"
        );
        assert_eq!(mounts.skill("nope"), None);
    }

    /// 产物那条：落盘存的会话相对路径与模型看到的资源入口，是同一个映射。
    #[test]
    fn an_artifact_path_maps_to_its_resource_entry() {
        let uri =
            artifact_uri("artifacts/01a0c495-d37a-7036-83bc-cb9981ba308a/out/报告.md").unwrap();
        assert_eq!(
            uri.to_string(),
            "artifact://files/01a0c495-d37a-7036-83bc-cb9981ba308a/out/报告.md"
        );
        assert_eq!(
            uri.relative_path(),
            Some("01a0c495-d37a-7036-83bc-cb9981ba308a/out/报告.md")
        );
        assert_eq!(
            artifact_uri("artifacts/01a0c495-d37a-7036-83bc-cb9981ba308a/x.md"),
            Some(ResourceUri::ArtifactFile {
                path: "01a0c495-d37a-7036-83bc-cb9981ba308a/x.md".into()
            })
        );
        // 不是产物目录下的、或者想跳出去的，映射不出来。
        assert_eq!(
            artifact_uri("tool-output/run-1/call-7/attempt-1/stdout.txt"),
            None
        );
        assert_eq!(artifact_uri("artifacts/../state.db"), None);
        // 直接落在 `artifacts/` 下的文件也映射得出来——映射只管"这个路径是不是产物"，
        // "产物按 <run>/ 分目录"是生产者的约定（§4.7），不在这里硬编码。
        assert_eq!(
            artifact_uri("artifacts/run-1"),
            Some(ResourceUri::ArtifactFile {
                path: "run-1".into()
            })
        );
    }

    /// 工具输出的位置只有一处拼法：落盘与读回同源。
    #[test]
    fn the_output_file_path_is_derived_from_the_uri() {
        let run = RunId::from_raw("01a0c495-d37a-7036-83bc-cb9981ba308a");
        let call = ToolCallId::from_raw("01a0c495-d37a-7036-83bc-cb9981ba308b");
        let attempt = AttemptId::from_raw("01a0c495-d37a-7036-83bc-cb9981ba308c");
        let uri = ResourceUri::Output {
            run,
            call,
            attempt,
            file: OutputFile::Stderr,
        };
        assert_eq!(
            output_file_path(Path::new("/sessions/s1"), &uri),
            Some(PathBuf::from(format!(
                "/sessions/s1/tool-output/{}/{}/{}/stderr.txt",
                uri_run(&uri),
                uri_call(&uri),
                uri_attempt(&uri)
            )))
        );
    }

    fn uri_run(uri: &ResourceUri) -> String {
        let ResourceUri::Output { run, .. } = uri else {
            unreachable!()
        };
        run.to_string()
    }
    fn uri_call(uri: &ResourceUri) -> String {
        let ResourceUri::Output { call, .. } = uri else {
            unreachable!()
        };
        call.to_string()
    }
    fn uri_attempt(uri: &ResourceUri) -> String {
        let ResourceUri::Output { attempt, .. } = uri else {
            unreachable!()
        };
        attempt.to_string()
    }
}
