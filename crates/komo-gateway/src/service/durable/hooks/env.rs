//! 工具执行环境（`ToolEnv`）：根与挂载、投影、时限、Session 目录上的输出存储与图片读回。
//!
//! 这些在旧路径里散在 `GatewaySegments::segment`（`run_roots`、`ResourceMounts`、`call_timeout`、
//! `detach_after`）；这里按同一个口径复制一份——**不能由模型参数提升**（§4）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::traits::ToolOutputStore;
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::resource::{ResourceMounts, SkillMount};
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::tool::WorkspaceRoot;
use komo_runtime::durable::harness::ToolEnv;
use komo_runtime::llm::{ImageLoadError, ImageRef, ImageResolver};
use komo_runtime::tools::paths;
use komo_store::SessionPaths;

use super::setup::Resolved;
use crate::service::state::GatewayState;

/// 一条 Run 能用哪些根。
///
/// 除了工作目录，还有**这个 Session 自己的输出与产物**（只读）：模型看到的每条
/// `tool.result` 都写着"完整输出在哪"，那条路径必须真能读——否则那句提示就是一根死链
/// （§8.3「当前 Session 获授权的输出可通过 `read` 只读访问」）。
///
/// **只读**是标注（`writable: false`）；真正拦住写的是 §8.10 第 4 条——`sessions/` 在
/// `write` / `edit` 的受保护名单里，写它一律 `Denied`（§7 唯一的硬限制）。
///
/// 根与目标必须在同一套坐标系里：这里用 `real_root` 解掉符号链接，工具解析目标时走的也
/// 是它（macOS 上 `/var` → `/private/var`，字面前缀匹配会全落空）。
pub(super) fn run_roots(
    sessions_dir: Option<&std::path::Path>,
    session: &SessionId,
    cwd: PathBuf,
    skill_dirs: &[PathBuf],
) -> Vec<WorkspaceRoot> {
    let mut roots = vec![WorkspaceRoot {
        path: cwd,
        writable: true,
        label: "workspace".into(),
    }];
    // §5.6：skill 目录是**只读根**——模型用 `read` 读 SKILL.md，`skill://` 那条路也要它们
    // 落在根里，挂载点才解析得出来。
    for dir in skill_dirs {
        roots.push(WorkspaceRoot {
            path: paths::real_root(dir),
            writable: false,
            label: "skill".into(),
        });
    }
    let Some(sessions_dir) = sessions_dir else {
        return roots;
    };
    let paths = komo_store::SessionPaths::new(sessions_dir, session);
    for (path, label) in [
        (paths.tool_output(), "session-output"),
        (paths.artifacts(), "session-artifacts"),
    ] {
        roots.push(WorkspaceRoot {
            path: paths::real_root(&path),
            writable: false,
            label: label.into(),
        });
    }
    roots
}

/// 图片引用 `{mime, path, hash}` 的 `path` 相对 Session 目录（§3.5）：读 `sessions/{uuid}/<path>`。
struct SessionImages {
    paths: SessionPaths,
}

impl ImageResolver for SessionImages {
    fn load(&self, image: &ImageRef) -> Result<Vec<u8>, ImageLoadError> {
        let fail = |reason: String| ImageLoadError {
            path: image.path.clone(),
            reason,
        };
        let path = self
            .paths
            .resolve(&image.path)
            .map_err(|error| fail(error.to_string()))?;
        std::fs::read(path).map_err(|error| fail(error.to_string()))
    }
}

pub(crate) fn build(
    state: &GatewayState,
    config: &ConfigSnapshot,
    resolved: &Resolved,
    surface: &AgentSurface,
    outputs: Arc<dyn ToolOutputStore>,
) -> ToolEnv {
    let session: &SessionId = &resolved.session;
    let sessions_dir = &config.paths.sessions_dir;
    let cwd = resolved.workspace.clone();
    let skills = state.skills.read().expect("skills 注册表");
    let skill_dirs = skills.dirs().to_vec();
    let roots = run_roots(Some(sessions_dir), session, cwd.clone(), &skill_dirs);

    // 资源命名空间的挂载点（§六）：skill 那一份是**这一次**的活注册表（只决定 `skill://` 按名字
    // 读得到哪个目录），会话根来自这个 Session，`tool://` 只读得到这次能力面。
    let session_paths = SessionPaths::new(sessions_dir, session);
    let mounts = ResourceMounts {
        skill_dirs,
        skills: skills
            .list()
            .into_iter()
            .filter_map(|skill| {
                // `Skill.path` 是 SKILL.md 的字面路径；拿不到父目录就跳过这一份。
                let dir = skill.path.parent()?.to_path_buf();
                Some(SkillMount {
                    name: skill.name,
                    dir,
                })
            })
            .collect(),
        session_root: Some(paths::real_root(session_paths.root())),
        tools: state.tool_executor.callable_definitions_for(surface).into(),
    };
    drop(skills);

    let conversation = resolved.is_conversation();
    let detach_after = (config.execution.detach_after_secs > 0 && conversation)
        .then(|| Duration::from_secs(config.execution.detach_after_secs));
    ToolEnv {
        session: session.clone(),
        source: PlanSource::Interactive {
            session: session.clone(),
        },
        cwd,
        roots,
        mounts,
        projection: config.execution.projection(),
        call_timeout: Duration::from_secs(config.execution.call_timeout_secs),
        // 只有对话会话转后台：任务会话、cron 没有"这条会话等着用户说话"的问题（`docs/monitor.md` §8）。
        detach_after,
        // 用户插话的旗子：只有对话会话会转后台，也只有它们会被插话顶到。
        interjected: conversation.then(|| state.durable.interjection(resolved.conversation)),
        conversation,
        confirm_timeout: Duration::from_secs(config.execution.confirm_timeout_mins * 60),
        outputs,
        images: Some(Arc::new(SessionImages {
            paths: session_paths,
        })),
    }
}
