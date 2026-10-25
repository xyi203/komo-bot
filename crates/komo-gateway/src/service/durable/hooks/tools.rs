//! 能力面与工具表（`AgentHooks::tools` / `setup` 的工具部分）。
//!
//! - **能力面**：Profile 在**当前**工具目录里挑出来的那份（`komo_agent::surface_of`）。目录 =
//!   执行器此刻装着的工具，加上撤掉的名字（热重载删掉的 MCP 服务器：名字留在能力面里，模型
//!   再调时拿到"为什么没了"，而不是一句"没有这个工具"）与 Profile 点名、但服务器还没连上的
//!   MCP 工具。
//! - **MCP 有超时地等连上**：能力面里有、目录里还没有的 MCP 工具，先等那几台服务器第一次连上
//!   （`ToolExecutor::await_mcp`，至多 `STARTUP_WAIT`）；连上的挂进目录，没着落的放桩，桩的
//!   `prepare` 报原因（"还没连上" / "连接失败" / "要登录"）。
//! - **fresh 任务会话**去掉 `task`（深度只有一层，runtime 的编排里还有第二道）与 `monitor` /
//!   `monitor_stop`。**fork 任务保留父的能力面**：工具声明在提示缓存的最前面，去掉一个就整段
//!   缓存作废，而 fork 存在的理由就是共用父的前缀；runtime 照样拒绝任务里再派任务。

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::traits::{OutputWriter, Tool, ToolOutputStore};
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolExposure, ToolOutput};
use komo_runtime::durable::harness::{HookError, ToolSet};

use super::env;
use super::setup::Resolved;
use crate::service::state::GatewayState;

/// fresh 任务会话里不给的编排工具：`task`（深度只有一层）与 `monitor`（监听的事件与结束消息
/// 交回挂它的会话，任务会话不挂）。
const ORCHESTRATION_TOOLS: [&str; 3] = [komo_agent::TASK_TOOL, "monitor", "monitor_stop"];

/// 撤掉 / 没连上的工具的桩：名字还在能力面里，模型调它就拿到原因。不进模型的 schema（声明里
/// 本来就没有它：撤掉之后的下一次系统段规划把它从已展示的工具里删掉）。
struct WithdrawnTool {
    name: String,
    reason: String,
}

#[async_trait]
impl Tool for WithdrawnTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.reason.clone(),
            parameters: serde_json::json!({ "type": "object" }),
            doc: None,
        }
    }

    async fn prepare(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        Err(ToolError::Failed {
            message: self.reason.clone(),
        })
    }

    async fn execute(
        &self,
        _plan: ExecutionPlan,
        _ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed {
            message: self.reason.clone(),
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Codemode
    }
}

/// 按当前目录与配置算这个 conversation 的能力面。
pub(crate) fn surface_for(
    state: &GatewayState,
    resolved: &Resolved,
    catalog: &[String],
) -> AgentSurface {
    let surface = komo_agent::surface_of(
        &resolved.profile,
        catalog,
        &crate::service::agent_config_file(&state.config),
    );
    // fork 任务保留父的能力面（见模块文档）；只有 fresh 任务去掉编排工具。
    if !resolved.is_task() || resolved.is_fork() {
        return surface;
    }
    AgentSurface::new(surface.names().iter().filter_map(|name| {
        let dropped = ORCHESTRATION_TOOLS.contains(&name.as_str());
        (!dropped).then(|| name.clone())
    }))
}

/// 组 `ToolSet`：能力面、工具实现（含桩）与执行环境。
pub(super) async fn build(
    state: &Arc<GatewayState>,
    config: &ConfigSnapshot,
    resolved: &Resolved,
    outputs: Arc<dyn ToolOutputStore>,
) -> Result<ToolSet, HookError> {
    let executor = &state.tool_executor;
    let mut catalog = executor.tool_names();
    let withdrawn = executor.withdrawn();
    catalog.extend(withdrawn.keys().cloned());
    // Profile 点名了、目录里还没有的 MCP 工具：留在能力面里等服务器（等不到就放桩说原因），
    // 而不是被 `surface_of` 当成"未知名字"丢掉。
    if let Some(listed) = &resolved.profile.tools {
        for name in listed {
            if name.starts_with("mcp__") && !catalog.contains(name) {
                catalog.push(name.clone());
            }
        }
    }
    let surface = surface_for(state, resolved, &catalog);

    // 能力面里有、目录里还没有的 MCP 工具：有超时地等（至多 `STARTUP_WAIT`）。
    let unresolved = executor.await_mcp(surface.names()).await;

    let present = executor.tools();
    let mut tools: Vec<Arc<dyn Tool>> = present.values().cloned().collect();
    let reasons: BTreeMap<&str, &str> = withdrawn
        .iter()
        .map(|(name, reason)| (name.as_str(), reason.as_str()))
        .chain(
            unresolved
                .iter()
                .map(|(name, reason)| (name.as_str(), reason.as_str())),
        )
        .collect();
    for name in surface.names() {
        if present.contains_key(name.as_str()) {
            continue;
        }
        if let Some(reason) = reasons.get(name.as_str()) {
            tools.push(Arc::new(WithdrawnTool {
                name: name.clone(),
                reason: (*reason).to_owned(),
            }));
        }
    }

    let env = env::build(state, config, resolved, &surface, outputs);
    Ok(ToolSet::new(tools, surface, env))
}
