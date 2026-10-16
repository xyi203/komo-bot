//! 能力面选择：Profile 在工具目录里挑出来的那一份（`docs/komo_bot.md` §4 末）。
//!
//! 受理时（冻结快照）与没有快照时的兜底装配共用这里——"目录里没有的名字不算数"这句话
//! 只有一处说得准。

use komo_kernel::types::agent::AgentProfile;
use komo_kernel::types::surface::AgentSurface;

/// 旧的同步委派（2026-10-16 删掉）：工具已经不在目录里，名字只剩旧子 Run 的能力面还要
/// 摘掉它（`segment` 的 `without_delegate`，§4 的深度只有一层）。
pub const DELEGATE_TOOL: &str = "delegate";

/// `task`：把耗时的事派给独立任务会话在后台跑（`docs/background-tasks.md`）。受理时给
/// 任务会话冻结能力面要摘掉它（深度只有一层），注册的那个工具自报同一个名字。
pub const TASK_TOOL: &str = "task";

/// 一份 Profile 在**这份工具目录**里挑出来的能力面。
///
/// 目录里没有的名字**不算数**，而且必须报出来：静默采纳一份写错的配置，等于让操作者以为
/// 某个工具给了、其实没给。
pub fn surface_of(
    profile: &AgentProfile,
    catalog: &[String],
    file: &std::path::Path,
) -> AgentSurface {
    let (surface, unknown) = profile.surface(catalog);
    if !unknown.is_empty() {
        tracing::warn!(
            file = %file.display(),
            agent = %profile.id,
            unknown = %unknown.join("、"),
            catalog = %catalog.join("、"),
            "`[agents]` 里写了工具目录里没有的名字，它们不算数（能力面只留真的装着的那些）"
        );
    }
    surface
}
