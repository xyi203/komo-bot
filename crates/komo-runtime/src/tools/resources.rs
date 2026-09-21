//! 资源入口（§六）的**解析**：`skill://` / `tool://` / `artifact://` → 真实目标，或者拒绝。
//!
//! 语法与类型在 kernel（[`komo_kernel::types::resource`]），挂载点是装配那一刻抄下来的
//! **纯数据**（[`ResourceMounts`]：skill 根与按名字的那一份、会话内容目录、这次能力面），
//! 这里做的是把它落到盘上的那半步——按名字找 skill、拼路径、解析符号链接、现算 `tool://`
//! 的正文。注册表那半步在装配时就做完了，所以这一层没有第二个"哪个名字是哪一份"的来源。
//!
//! 三条纪律：
//!
//! - **解析链接之后必须仍在挂载点里面**（skill 目录、`<会话>/artifacts`、
//!   `<会话>/tool-output`）——链接（或者手拼出来的 `..`）指到外面一律拒绝。"挂载点"这三个
//!   字只有这样才是硬的；
//! - **看不懂就是错**：含 `://` 却解析不出来的字符串**不退回本地路径**——那是把一次想跳出
//!   挂载点的尝试读成一次普通读取（[`ResourceError::Malformed`]）；
//! - **不编路径**：`tool://` 没有磁盘目标，就不给它编一个假的——虚拟入口由规则表那一条
//!   `paths = virtual` 单管，硬塞一个假路径进去才是真的越权口子。

use std::path::{Path, PathBuf};

use komo_kernel::types::resource::{
    ResourceMounts, ResourceUri, ToolPart, UriError, output_file_path,
};
use komo_kernel::types::tool::{ToolDefinition, ToolError};

use super::paths;

/// 一个资源入口解析出来的东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedResource {
    /// 一份真实的文件（符号链接已解析）。
    File(PathBuf),
    /// 一组真实的根——`rg` 的搜索范围（`skill://`、`artifact://files`）。
    Roots(Vec<PathBuf>),
    /// 没有磁盘目标，内容由 [`virtual_content`] 现算。
    Virtual(VirtualResource),
}

/// `tool://` 那两种入口要现算的东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VirtualResource {
    /// 这次能力面里的某个工具的 schema 或说明。
    Tool { name: String, part: ToolPart },
    /// 这次能力面里的工具名。
    ToolNames,
}

/// 资源入口立不住的原因。**每一句都要能照着做**：模型读到它要知道下一步该改什么，而不是
/// 只知道"失败了"。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResourceError {
    /// 看着像资源，但解析不出来。**这条绝不退回本地路径**。
    #[error(
        "{raw} 看着像资源 URI（里面有 `://`），但它不成立：{why}。本地路径里不该出现 `://`，\
         别把它当一次普通读取；资源入口只有 `skill://` / `tool://` / `artifact://` 三种"
    )]
    Malformed { raw: String, why: UriError },

    /// URI 的路径段里有 `..`、`.`、空段或反斜杠——拼进真实路径之前再过一遍。
    #[error(
        "{uri} 的路径段不许有 `..`、`.`、空段或反斜杠；要读挂载点外面的东西请给绝对路径，\
         让它照常过一遍 Policy"
    )]
    Traversal { uri: String },

    /// 注册表（装配那一刻的那一份）里没有这个名字。
    #[error("{name} 不是这次装配里的 skill（用 `komo skills list` 看装了哪些、名字是不是写错了）")]
    NoSkill { name: String },

    /// 解析出来的真实目标跳出了挂载点（多半是个指向外面的符号链接）。
    #[error(
        "{uri} 解析之后落在 {resolved}，已经跳出「{kind}」{root} 了——指向挂载点之外的链接一律\
         拒绝；要读那份内容请给它绝对路径，走普通读取那条路（照常过 Policy）"
    )]
    Escapes {
        uri: String,
        kind: &'static str,
        resolved: PathBuf,
        root: PathBuf,
    },

    /// `artifact://` 认的是**这个会话自己的**内容目录，而这次运行没有。
    #[error("{uri} 要这个会话的内容目录，而这次运行没有——`artifact://` 只在带会话存储的运行里成立")]
    NoSessionRoot { uri: String },

    /// 这次一个可搜的根都没有。
    #[error("这次没有可搜的 {scheme}:// 根：{why}")]
    NoRoots {
        scheme: &'static str,
        why: &'static str,
    },

    /// 这个工具不在这次的能力面里——说明与 schema 也不给它。
    #[error(
        "tool://{name} 不在这次能力面里——这次能用的是：{available}。工具说明与 schema 只给这次\
         真的拿得到的工具（§4 末）"
    )]
    ToolNotInSurface { name: String, available: String },

    /// `read` 要的是一份文件，而它给的是一组根。
    #[error(
        "{uri} 是一组根，不是一份文件——`read` 要读到具体的那一份，例如 \
         `skill://<名字>/SKILL.md`、`artifact://files/<名字>`（要搜就用 `rg` 的 path）"
    )]
    Directory { uri: String },

    /// `rg` 要的是磁盘上的文件，而它给的是现算的正文。
    #[error(
        "{uri} 是现算出来的正文，磁盘上没有这份文件——`rg` 搜的是文件；要这份内容就用 `read` 读它"
    )]
    Virtual { uri: String },

    /// 工具输出那条入口没有落盘位置——**那是拼法出了问题**，如实报，不拿一个编出来的路径去读。
    #[error("{uri} 算不出落盘位置（工具输出的拼法只有 kernel 那一处；它变了这里就会先说）")]
    NotAnOutput { uri: String },
}

impl ResourceError {
    /// 工具那一侧的形状：**拒绝发生在 `prepare`**，所以一律是 [`ToolError::Failed`]——
    /// 模型拿到的是一条说得清"为什么不行"的结果（§六：不进计划就没有执行）。
    pub fn into_tool_error(self) -> ToolError {
        ToolError::Failed {
            message: self.to_string(),
        }
    }
}

/// 模型给的 `path` 参数 → 资源入口。**三种形状，各走各的路**：
///
/// - `Ok(Some(uri))`：这是一条资源，按 §六 解析；
/// - `Ok(None)`：这不是资源（本地路径），今天的行为一个字不变；
/// - `Err(..)`：**看着像资源却立不住**——含 `://` 的东西绝不当成本地路径（[`Self`] 的
///   `Malformed`），去文件系统里找 `skill://a/../x` 这种名字才是把越权读成正常读取。
pub fn entry(raw: &str) -> Result<Option<ResourceUri>, ResourceError> {
    match as_resource(raw) {
        Some(uri) => Ok(Some(uri)),
        None if resource_shaped(raw) => Err(malformed(raw)),
        None => Ok(None),
    }
}

/// 这个字符串是不是一条资源 URI（`scheme://` 开头、且解析得出来）。
pub fn as_resource(raw: &str) -> Option<ResourceUri> {
    let raw = raw.trim();
    if !resource_shaped(raw) {
        return None;
    }
    ResourceUri::parse(raw).ok()
}

/// 含 `://` 的字符串——**想当资源**的形状。解析得出来解析不出来是下一步的事。
fn resource_shaped(raw: &str) -> bool {
    raw.trim().contains("://")
}

fn malformed(raw: &str) -> ResourceError {
    let raw = raw.trim();
    ResourceError::Malformed {
        raw: raw.to_string(),
        why: ResourceUri::parse(raw)
            .err()
            .unwrap_or(UriError::NotAResource(raw.to_string())),
    }
}

/// 把一条资源 URI 落到真实的盘上。
///
/// 挂载点是装配那一刻的事实（skill 那一份来自活注册表的快照、会话根来自这一次的 Session、
/// 能力面来自这一次的 `AgentSurface`），这里只做 I/O 与核对。
pub fn resolve(
    uri: &ResourceUri,
    mounts: &ResourceMounts,
) -> Result<ResolvedResource, ResourceError> {
    match uri {
        // `skill://<name>/<相对路径>`：名字查装配那一刻的那一份，路径必须还在它目录里。
        ResourceUri::Skill { name, path } => {
            let Some(dir) = mounts.skill(name) else {
                return Err(ResourceError::NoSkill { name: name.clone() });
            };
            Ok(ResolvedResource::File(inside(
                dir,
                path,
                "skill 目录",
                uri,
            )?))
        }
        ResourceUri::SkillRoot => roots(
            mounts,
            uri,
            "skill",
            "配置里没有 skill 目录（`paths.skill_dirs`）",
        ),
        // `tool://`：没有磁盘目标。能力面之外的名字**连说明都读不到**。
        ResourceUri::Tool { name, part } => {
            if !mounts.allows_tool(name) {
                return Err(ResourceError::ToolNotInSurface {
                    name: name.clone(),
                    available: available(mounts),
                });
            }
            Ok(ResolvedResource::Virtual(VirtualResource::Tool {
                name: name.clone(),
                part: *part,
            }))
        }
        ResourceUri::ToolRoot => Ok(ResolvedResource::Virtual(VirtualResource::ToolNames)),
        // `artifact://files/<相对路径>`：这个会话的产物目录。
        ResourceUri::ArtifactFile { path } => {
            let session = session_root(mounts, uri)?;
            Ok(ResolvedResource::File(inside(
                &session.join("artifacts"),
                path,
                "产物目录",
                uri,
            )?))
        }
        ResourceUri::ArtifactRoot => roots(
            mounts,
            uri,
            "artifact",
            "这个会话还没有产物目录（`artifacts/`）",
        ),
        // `artifact://<run>/<call>/<attempt>/<文件>`：这个会话的 `tool-output/`。
        ResourceUri::Output { .. } => {
            let session = session_root(mounts, uri)?;
            let Some(path) = output_file_path(session, uri) else {
                return Err(ResourceError::NotAnOutput {
                    uri: uri.to_string(),
                });
            };
            let root = paths::real_root(&session.join("tool-output"));
            let resolved = paths::real_root(&path);
            if !resolved.starts_with(&root) {
                return Err(ResourceError::Escapes {
                    uri: uri.to_string(),
                    kind: "工具输出目录",
                    resolved,
                    root,
                });
            }
            Ok(ResolvedResource::File(resolved))
        }
    }
}

/// 一次组根：真实路径、去重、**只留真的在的**（共享 skill 目录多半没装，那不该是一次失败）。
fn roots(
    mounts: &ResourceMounts,
    uri: &ResourceUri,
    scheme: &'static str,
    why: &'static str,
) -> Result<ResolvedResource, ResourceError> {
    let mut found: Vec<PathBuf> = Vec::new();
    for root in mounts.roots_for(uri) {
        let root = paths::real_root(&root);
        if root.is_dir() && !found.contains(&root) {
            found.push(root);
        }
    }
    if found.is_empty() {
        return Err(ResourceError::NoRoots { scheme, why });
    }
    Ok(ResolvedResource::Roots(found))
}

fn session_root<'a>(
    mounts: &'a ResourceMounts,
    uri: &ResourceUri,
) -> Result<&'a Path, ResourceError> {
    mounts
        .session_root
        .as_deref()
        .ok_or_else(|| ResourceError::NoSessionRoot {
            uri: uri.to_string(),
        })
}

/// `<挂载点>/<相对路径>`：解析符号链接，然后核对它**还在挂载点里面**。
///
/// 两件事都必要：`..` 检查管的是"拼出去的路径"（URI 也可能来自旧日志或手拼），链接检查管
/// 的是"拼出来的路径上有一个指向外面的链接"。少一样，"挂载点"就只是句好话。
fn inside(
    root: &Path,
    relative: &str,
    kind: &'static str,
    uri: &ResourceUri,
) -> Result<PathBuf, ResourceError> {
    if !is_relative(relative) {
        return Err(ResourceError::Traversal {
            uri: uri.to_string(),
        });
    }
    let root = paths::real_root(root);
    // `real_root` 解析符号链接；还不存在的尾段按字面保留（读一个还没产出的文件是正常动作，
    // 由 `read` 在**执行**那一步报"不存在"，不必在这里替它下结论）。
    let resolved = paths::real_root(&root.join(relative));
    if !resolved.starts_with(&root) {
        return Err(ResourceError::Escapes {
            uri: uri.to_string(),
            kind,
            resolved,
            root,
        });
    }
    Ok(resolved)
}

/// 一段**相对**路径：不能是绝对路径，段不许空、不许 `.` / `..`、也不许反斜杠（它在别的
/// 平台上也是路径分隔符，放过去就等于给越权留了一条路）。
fn is_relative(path: &str) -> bool {
    !path.starts_with('/')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
}

fn available(mounts: &ResourceMounts) -> String {
    let names: Vec<&str> = mounts.tools.iter().map(|tool| tool.name.as_str()).collect();
    match names.is_empty() {
        true => "（这次没有工具）".into(),
        false => names.join("、"),
    }
}

/// 计划里那个 URI 要现算的东西——`prepare` 检查过能力面，这里只按 URI 的形状分派。
///
/// **不重新解析**：一次 `read` 的计划与执行之间，挂载点不会变（`ToolContext` 是这一次调用
/// 的），而能力面那一步已经在 `prepare` 拒绝过了。
pub fn virtual_of(uri: &ResourceUri) -> Option<VirtualResource> {
    match uri {
        ResourceUri::Tool { name, part } => Some(VirtualResource::Tool {
            name: name.clone(),
            part: *part,
        }),
        ResourceUri::ToolRoot => Some(VirtualResource::ToolNames),
        _ => None,
    }
}

/// `tool://` 的正文：从**这次能力面**的说明书现算，不碰磁盘。
///
/// `definitions` 就是能力面本身（顺序即能力面的顺序）：`tool://` 列得出它们，
/// `schema` / `doc` 也只答得出它们。
pub fn virtual_content(resource: &VirtualResource, definitions: &[ToolDefinition]) -> String {
    match resource {
        VirtualResource::Tool { name, part } => {
            let Some(definition) = definitions.iter().find(|tool| &tool.name == name) else {
                // 计划那一刻在能力面里，执行时却找不到了 = 装配不同源。**如实说**
                // （§六：不编），而不是给一份空 schema 让模型以为这个工具没有参数。
                return format!("这次能力面里没有 {name} 的说明书。\n");
            };
            match part {
                ToolPart::Schema => format!(
                    "{name} 的参数 schema：\n{}\n",
                    pretty(&definition.parameters)
                ),
                ToolPart::Doc => format!(
                    "{name}：{}\n（它的参数 schema 在 tool://{name}/schema）\n",
                    definition.description
                ),
            }
        }
        VirtualResource::ToolNames => match definitions.is_empty() {
            true => "这次能力面里没有工具。\n".into(),
            false => {
                let names: Vec<&str> = definitions
                    .iter()
                    .map(|definition| definition.name.as_str())
                    .collect();
                format!(
                    "这次能用的工具（{} 个）：{}\n",
                    names.len(),
                    names.join("、")
                )
            }
        },
    }
}

fn pretty(value: &serde_json::Value) -> String {
    // `Value` 一定能序列化；这个 `unwrap_or` 只是不在库里留一个 `expect`。
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use komo_kernel::types::ids::{AttemptId, RunId, ToolCallId};
    use komo_kernel::types::resource::SkillMount;

    const RUN: &str = "01a0c495-d37a-7036-83bc-cb9981ba308a";
    const CALL: &str = "01a0c495-d37a-7036-83bc-cb9981ba308b";
    const ATTEMPT: &str = "01a0c495-d37a-7036-83bc-cb9981ba308c";

    fn definition(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        }
    }

    /// 一个装着一份 skill、一个会话目录（含产物与工具输出）的现场。
    struct Scene {
        dir: tempfile::TempDir,
        session: PathBuf,
    }

    impl Scene {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let session = dir.path().join("sessions").join("s1");
            std::fs::create_dir_all(session.join("artifacts")).unwrap();
            std::fs::create_dir_all(
                session
                    .join("tool-output")
                    .join(RUN)
                    .join(CALL)
                    .join(ATTEMPT),
            )
            .unwrap();
            std::fs::write(
                session
                    .join("tool-output")
                    .join(RUN)
                    .join(CALL)
                    .join(ATTEMPT)
                    .join("stdout.txt"),
                "跑完了\n",
            )
            .unwrap();
            std::fs::write(session.join("artifacts").join("report.md"), "报告\n").unwrap();

            let skill = dir.path().join("skills").join("review");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), "---\nname: review\n---\n照着做\n").unwrap();
            std::fs::write(skill.join("notes.md"), "笔记\n").unwrap();
            Self { dir, session }
        }

        fn skill_dir(&self) -> PathBuf {
            std::fs::canonicalize(self.dir.path().join("skills").join("review")).unwrap()
        }

        fn mounts(&self) -> ResourceMounts {
            ResourceMounts {
                skill_dirs: vec![std::fs::canonicalize(self.dir.path().join("skills")).unwrap()],
                skills: vec![SkillMount {
                    name: "review".into(),
                    dir: self.skill_dir(),
                }],
                session_root: Some(std::fs::canonicalize(&self.session).unwrap()),
                tools: Arc::from([
                    definition("read", "读取一个文本文件"),
                    definition("rg", "在文件里搜正则"),
                ]),
            }
        }
    }

    fn file(resolved: ResolvedResource) -> PathBuf {
        match resolved {
            ResolvedResource::File(path) => path,
            other => panic!("这不是一份文件：{other:?}"),
        }
    }

    fn roots_of(resolved: ResolvedResource) -> Vec<PathBuf> {
        match resolved {
            ResolvedResource::Roots(roots) => roots,
            other => panic!("这不是一组根：{other:?}"),
        }
    }

    fn resolve_raw(raw: &str, mounts: &ResourceMounts) -> Result<ResolvedResource, ResourceError> {
        let uri = as_resource(raw).unwrap_or_else(|| panic!("{raw} 该解析成资源"));
        assert_eq!(uri.to_string(), raw, "规范化之后要逐字回到原样");
        resolve(&uri, mounts)
    }

    #[test]
    fn a_skill_resource_lands_on_the_file_under_that_skill() {
        let scene = Scene::new();
        let mounts = scene.mounts();
        assert_eq!(
            file(resolve_raw("skill://review/notes.md", &mounts).unwrap()),
            scene.skill_dir().join("notes.md")
        );
        assert_eq!(
            file(resolve_raw("skill://review/SKILL.md", &mounts).unwrap()),
            scene.skill_dir().join("SKILL.md")
        );
    }

    #[test]
    fn a_skill_root_is_the_search_roots_the_mounts_hold() {
        let scene = Scene::new();
        let mounts = scene.mounts();
        assert_eq!(
            roots_of(resolve_raw("skill://", &mounts).unwrap()),
            mounts.skill_dirs
        );
    }

    #[test]
    fn an_artifact_resource_lands_under_this_sessions_artifacts() {
        let scene = Scene::new();
        let mounts = scene.mounts();
        assert_eq!(
            file(resolve_raw("artifact://files/report.md", &mounts).unwrap()),
            std::fs::canonicalize(&scene.session)
                .unwrap()
                .join("artifacts/report.md")
        );
        assert_eq!(
            roots_of(resolve_raw("artifact://files", &mounts).unwrap()),
            vec![
                std::fs::canonicalize(&scene.session)
                    .unwrap()
                    .join("artifacts")
            ]
        );
    }

    /// 工具输出那条：`artifact://<run>/<call>/<attempt>/stdout` 落在**落盘的那一份**上
    /// （拼法只有 kernel 那一处，这里只解析链接）。
    #[test]
    fn an_output_resource_lands_on_the_file_the_store_wrote() {
        let scene = Scene::new();
        let mounts = scene.mounts();
        assert_eq!(
            file(
                resolve_raw(
                    &format!("artifact://{RUN}/{CALL}/{ATTEMPT}/stdout"),
                    &mounts
                )
                .unwrap()
            ),
            std::fs::canonicalize(&scene.session)
                .unwrap()
                .join(format!("tool-output/{RUN}/{CALL}/{ATTEMPT}/stdout.txt"))
        );
    }

    /// 别的会话的 run：`artifact://` 里没有"会话"这一段——**它认的就是挂载点里那个会话**，
    /// 所以一条外来的 run 只会落到本会话的 `tool-output/` 底下，那儿没有它。跨会话访问在
    /// 这条路上**根本表达不出来**，这就是它被拒的方式（不是拒在 prepare，是根本落不到）。
    #[test]
    fn another_sessions_run_cannot_name_a_file_outside_this_session() {
        let scene = Scene::new();
        let mounts = scene.mounts();
        let foreign = format!("artifact://{ATTEMPT}/{CALL}/{ATTEMPT}/stdout");
        let resolved = file(resolve_raw(&foreign, &mounts).unwrap());
        let root = std::fs::canonicalize(&scene.session).unwrap();
        assert!(
            resolved.starts_with(root.join("tool-output")),
            "{resolved:?} 跳出了本会话的 tool-output"
        );
        assert!(!resolved.exists(), "本会话里没有这个 run 的落盘");
    }

    #[test]
    fn a_tool_resource_is_only_readable_when_it_is_on_this_surface() {
        let scene = Scene::new();
        let mounts = scene.mounts();
        assert_eq!(
            resolve_raw("tool://read/schema", &mounts).unwrap(),
            ResolvedResource::Virtual(VirtualResource::Tool {
                name: "read".into(),
                part: ToolPart::Schema
            })
        );
        assert_eq!(
            resolve_raw("tool://", &mounts).unwrap(),
            ResolvedResource::Virtual(VirtualResource::ToolNames)
        );

        let error = resolve_raw("tool://write/schema", &mounts).unwrap_err();
        let ResourceError::ToolNotInSurface { name, available } = &error else {
            panic!("{error:?}")
        };
        assert_eq!(name, "write");
        assert!(available.contains("read"), "{available}");
        assert!(error.to_string().contains("不在这次能力面里"), "{error}");
    }

    /// 越权的写法：解析这一层就拒（`..`、空段），落到盘上那一层再拒一次（手拼的 URI）。
    #[test]
    fn traversal_is_refused_before_it_ever_reaches_a_path() {
        let scene = Scene::new();
        let mounts = scene.mounts();

        for raw in [
            "skill://review/../../etc/passwd",
            "skill://review//notes.md",
            "artifact://files/../state.db",
            "artifact://files/a/../../b",
        ] {
            let error = entry(raw).unwrap_err();
            assert!(
                matches!(error, ResourceError::Malformed { .. }),
                "{raw} 该被判成越权：{error:?}"
            );
            assert!(error.to_string().contains("://"), "{error}");
        }

        // 手拼出来的 URI（旧日志、别处构造）绕过了解析器：拼进真实路径之前再核一遍。
        let uri = ResourceUri::Skill {
            name: "review".into(),
            path: "../notes.md".into(),
        };
        assert_eq!(
            resolve(&uri, &mounts).unwrap_err(),
            ResourceError::Traversal {
                uri: "skill://review/../notes.md".into()
            }
        );
        let uri = ResourceUri::ArtifactFile {
            path: "/etc/passwd".into(),
        };
        assert!(matches!(
            resolve(&uri, &mounts).unwrap_err(),
            ResourceError::Traversal { .. }
        ));
    }

    /// 链接指到 skill 目录外面：**拒绝**，不跟着链接走出去。
    #[test]
    fn a_link_out_of_the_skill_directory_is_refused() {
        let scene = Scene::new();
        let outside = scene.dir.path().join("outside.txt");
        std::fs::write(&outside, "外面的东西\n").unwrap();
        std::os::unix::fs::symlink(&outside, scene.skill_dir().join("escape.md")).unwrap();

        let error = resolve_raw("skill://review/escape.md", &scene.mounts()).unwrap_err();
        let ResourceError::Escapes { kind, .. } = &error else {
            panic!("{error:?}")
        };
        assert_eq!(*kind, "skill 目录");
        assert!(error.to_string().contains("跳出"), "{error}");

        // 指到自己目录里面（或者干脆是个普通文件）照常读。
        std::os::unix::fs::symlink(
            scene.skill_dir().join("notes.md"),
            scene.skill_dir().join("alias.md"),
        )
        .unwrap();
        assert_eq!(
            file(resolve_raw("skill://review/alias.md", &scene.mounts()).unwrap()),
            scene.skill_dir().join("notes.md")
        );
    }

    /// 产物目录里的链接同样不许指到外面。
    #[test]
    fn a_link_out_of_the_artifacts_directory_is_refused() {
        let scene = Scene::new();
        let session = std::fs::canonicalize(&scene.session).unwrap();
        std::os::unix::fs::symlink(session.join("tool-output"), session.join("artifacts/link"))
            .unwrap();
        let error = resolve_raw("artifact://files/link", &scene.mounts()).unwrap_err();
        assert!(matches!(error, ResourceError::Escapes { .. }), "{error:?}");
    }

    #[test]
    fn a_name_that_is_not_a_skill_is_refused_with_where_to_look() {
        let scene = Scene::new();
        let error = resolve_raw("skill://nope/SKILL.md", &scene.mounts()).unwrap_err();
        assert_eq!(
            error,
            ResourceError::NoSkill {
                name: "nope".into()
            }
        );
        assert!(error.to_string().contains("komo skills list"), "{error}");
    }

    /// 没有会话内容目录：`artifact://` 的两条都拒——它认的就是"这个会话自己的"那两个目录。
    #[test]
    fn without_a_session_root_nothing_under_artifact_resolves() {
        let scene = Scene::new();
        let mounts = ResourceMounts {
            session_root: None,
            ..scene.mounts()
        };
        for raw in [
            "artifact://files/report.md".to_string(),
            format!("artifact://{RUN}/{CALL}/{ATTEMPT}/result"),
        ] {
            let error = resolve_raw(&raw, &mounts).unwrap_err();
            assert!(
                matches!(error, ResourceError::NoSessionRoot { .. }),
                "{raw}：{error:?}"
            );
            assert!(error.to_string().contains("会话"), "{error}");
        }
    }

    /// 一段相对路径的检查：绝对路径、空段、`.`、`..` 一个都不放过。
    #[test]
    fn only_a_clean_relative_path_gets_joined() {
        assert!(is_relative("a/b.txt"));
        assert!(is_relative("SKILL.md"));
        for path in [
            "",
            "/etc/passwd",
            "a//b",
            "./a",
            "a/../b",
            "a/..",
            "a\\..\\b",
        ] {
            assert!(!is_relative(path), "{path} 不该当成相对路径");
        }
    }

    /// 边界：看着像资源却立不住时**绝不**当本地路径。
    #[test]
    fn something_that_looks_like_a_resource_never_falls_back_to_a_path() {
        // 本地路径照旧。
        assert_eq!(entry("notes.md").unwrap(), None);
        assert_eq!(entry("/etc/hosts").unwrap(), None);

        for raw in ["tool://read/schema/extra", "artifact://", "skill://review"] {
            let error = entry(raw).unwrap_err();
            assert!(
                matches!(error, ResourceError::Malformed { .. }),
                "{raw}：{error:?}"
            );
        }
        // 别的 scheme 也一样：含 `://` 就不许当成一个叫这个名字的文件。
        let error = entry("http://example.com/a").unwrap_err();
        assert!(
            matches!(error, ResourceError::Malformed { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn tool_content_is_computed_from_this_surfaces_definitions() {
        let definitions = vec![
            definition("read", "读取一个文本文件"),
            definition("rg", "在文件里搜正则"),
        ];

        let schema = virtual_content(
            &VirtualResource::Tool {
                name: "read".into(),
                part: ToolPart::Schema,
            },
            &definitions,
        );
        assert!(schema.starts_with("read 的参数 schema：\n"), "{schema}");
        assert!(
            schema.contains("\n  \"type\": \"object\""),
            "schema 按层缩进：{schema}"
        );
        assert!(schema.contains("\"required\""), "{schema}");

        let doc = virtual_content(
            &VirtualResource::Tool {
                name: "read".into(),
                part: ToolPart::Doc,
            },
            &definitions,
        );
        assert!(doc.contains("read："), "{doc}");
        assert!(doc.contains("读取一个文本文件"), "{doc}");
        assert!(doc.contains("tool://read/schema"), "{doc}");

        // 名字与顺序都来自能力面。
        let names = virtual_content(&VirtualResource::ToolNames, &definitions);
        assert!(names.contains("read、rg"), "{names}");
        assert!(!names.contains("write"), "{names}");
    }

    /// 计划那一刻在能力面里、执行时却不在说明书里（装配不同源）：如实说，不编空 schema。
    #[test]
    fn a_tool_we_cannot_explain_says_so_instead_of_inventing_a_schema() {
        let content = virtual_content(
            &VirtualResource::Tool {
                name: "write".into(),
                part: ToolPart::Schema,
            },
            &[],
        );
        assert!(content.contains("没有 write 的说明书"), "{content}");
        assert!(!content.contains("\"type\""), "{content}");
    }

    #[test]
    fn the_plan_side_and_the_execute_side_agree_on_what_is_virtual() {
        for raw in ["tool://read/schema", "tool://"] {
            let uri = as_resource(raw).unwrap();
            assert!(virtual_of(&uri).is_some(), "{raw}");
        }
        for raw in ["skill://review/SKILL.md", "artifact://files/a", "skill://"] {
            let uri = as_resource(raw).unwrap();
            assert_eq!(virtual_of(&uri), None, "{raw}");
        }
    }

    #[test]
    fn ids_of_an_output_uri_come_from_the_uri_itself() {
        // 只是把拼法钉住：kernel 那一处拼错了（或者这里把段读错了），`artifact://` 就读不回来。
        let uri = ResourceUri::Output {
            run: RunId::from_raw(RUN),
            call: ToolCallId::from_raw(CALL),
            attempt: AttemptId::from_raw(ATTEMPT),
            file: komo_kernel::types::resource::OutputFile::Stdout,
        };
        assert_eq!(
            uri.to_string(),
            format!("artifact://{RUN}/{CALL}/{ATTEMPT}/stdout")
        );
    }
}
