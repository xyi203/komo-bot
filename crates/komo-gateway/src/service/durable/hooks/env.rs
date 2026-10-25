//! 工具执行环境（`ToolEnv`）：根与挂载、投影、时限、Session 目录上的输出存储与图片读回。
//!
//! 这些在旧路径里散在 `GatewaySegments::segment`（`run_roots`、`ResourceMounts`、`call_timeout`、
//! `detach_after`）；这里按同一个口径复制一份——**不能由模型参数提升**（§4）。

use std::sync::Arc;
use std::time::Duration;

use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::traits::ToolOutputStore;
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::resource::{ResourceMounts, SkillMount};
use komo_kernel::types::surface::AgentSurface;
use komo_runtime::durable::harness::ToolEnv;
use komo_runtime::llm::{ImageLoadError, ImageRef, ImageResolver};
use komo_runtime::tools::paths;
use komo_store::SessionPaths;

use super::setup::Resolved;
use crate::service::segment::run_roots;
use crate::service::state::GatewayState;

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
        conversation,
        outputs,
        images: Some(Arc::new(SessionImages {
            paths: session_paths,
        })),
    }
}
