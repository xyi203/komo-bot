//! 能力面选择：Profile 在工具目录里挑出来的那一份（`docs/komo_bot.md` §4 末）。
//!
//! 受理时（冻结快照）与没有快照时的兜底装配共用这里——"目录里没有的名字不算数"这句话
//! 只有一处说得准。

use komo_kernel::types::agent::AgentProfile;
use komo_kernel::types::surface::AgentSurface;

/// 编排操作名。**注册与过滤两处共用它**：Gateway 装配工具目录时挂上这个名字，`segment`
/// 给子 Run 装配能力面时摘掉它（§4 的深度只有一层）。另有一处测试钉住它与
/// `DelegateTool` 自报的名字一致——两处各写一个字面量迟早会漂。
pub const DELEGATE_TOOL: &str = "delegate";

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
