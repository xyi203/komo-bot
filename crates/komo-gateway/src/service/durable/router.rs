//! Router 分发判断的模型调用（`docs/tasks.md` §4.11）：把 komo-agent 的 [`RouterModel`] 接到 Gateway
//! 的模型后端。
//!
//! **这一步只有"判断"，没有接线**：它不读存储、不建任务、不进 `Dispatcher`。下一步把它放进
//! 引擎任务的"判断"检查点；判断没有副作用，所以重启后重做无妨。
//!
//! 调用方式照记忆整理（`komo-memory` 的 `offline.rs::judge`）：`begin_turn` 起一轮、不带工具、
//! 提示要求只输出 JSON，解析与校验在 komo-agent 的 [`route`]。每次调用都按**当前**配置解析模型。

use std::sync::Arc;

use async_trait::async_trait;
use komo_agent::router::{Routed, RouterInput, RouterModel, RouterPrompt, route};
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::traits::LlmClient;
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::model::ModelConfig;
use komo_kernel::types::turn::{ReplayMessage, Role, RoundInput, TurnRequest};

/// 这个 Agent 的 Router 用哪个模型：`router_model`，没写用 Agent 的 `model`，再没写用主模型。
///
/// `router_model` 指向不存在的 alias 在校验时就被拒绝（不会装进快照）；这里仍然逐级退回，
/// 是因为 `model` 的写错历来只记日志、不让输入失败（见 `hooks::setup::profile_model`）。
pub fn router_model_config(config: &ConfigSnapshot, agent_id: &str) -> ModelConfig {
    let profile = config.agent.get(agent_id);
    profile
        .and_then(|profile| {
            [profile.router_model.as_deref(), profile.model.as_deref()]
                .into_iter()
                .flatten()
                .find_map(|alias| config.model_catalog.completion(alias))
        })
        .unwrap_or(&config.model)
        .clone()
}

/// 用 [`LlmClient`] 跑一次不带工具的判断。
pub struct LlmRouterModel {
    llm: Arc<dyn LlmClient>,
    model: ModelConfig,
}

impl LlmRouterModel {
    pub fn new(llm: Arc<dyn LlmClient>, model: ModelConfig) -> Self {
        LlmRouterModel { llm, model }
    }
}

#[async_trait]
impl RouterModel for LlmRouterModel {
    async fn complete(&self, prompt: &RouterPrompt) -> Result<String, String> {
        let request = TurnRequest {
            session: SessionId::from_raw("router"),
            run: RunId::from_raw("router"),
            model: self.model.clone(),
            system_prompt: prompt.system.clone(),
            messages: vec![ReplayMessage {
                role: Role::User,
                seq: komo_kernel::types::ids::Seq::ZERO,
                text: Some(prompt.user.clone()),
                tool_calls: vec![],
                tool_results: vec![],
                provider_blocks: None,
            }],
            tools: vec![],
            memories: vec![],
            covers: None,
            cache_session: None,
        };
        let mut driver = self
            .llm
            .begin_turn(request)
            .await
            .map_err(|error| error.to_string())?;
        let round = komo_llm::telemetry::turn(
            &mut *driver,
            RoundInput::First,
            komo_llm::telemetry::role::ROUTER,
            &self.model,
            1,
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(round.text.unwrap_or_default())
    }
}

/// 对一条消息做一次分发判断：永远得到一个合法去向（失败已兜底，见 [`Routed::fallback`]）。
pub async fn judge(
    llm: Arc<dyn LlmClient>,
    config: &ConfigSnapshot,
    agent_id: &str,
    input: &RouterInput,
) -> Routed {
    let model = LlmRouterModel::new(llm, router_model_config(config, agent_id));
    route(input, &model).await
}

#[cfg(test)]
mod tests {
    use komo_agent::router::{
        CandidateState, CandidateTask, MAX_ATTEMPTS, Route, RouteFailure, RouteScope, RouterLimits,
    };
    use komo_kernel::protocol::config::ConfigSnapshot;
    use komo_kernel::test_support::snapshot_fixture;
    use komo_kernel::types::turn::LlmError;

    use super::*;
    use crate::service::test_support::harness::{FakeLlm, text_round};

    fn input(scope: RouteScope) -> RouterInput {
        RouterInput::new(
            scope,
            "把周报发我",
            vec![CandidateTask {
                id: "t1".into(),
                title: "整理周报".into(),
                state: CandidateState::Running,
                last_said: "我先整理".into(),
            }],
            vec![],
            RouterLimits::default(),
        )
    }

    fn with_router(router: Option<&str>, model: Option<&str>) -> ConfigSnapshot {
        let mut config = snapshot_fixture();
        let profile = config.agent.agents.get_mut("assistant").expect("fixture");
        profile.router_model = router.map(str::to_string);
        profile.model = model.map(str::to_string);
        config
    }

    #[test]
    fn the_router_uses_router_model_then_the_agent_model_then_the_main_model() {
        let name = |config: &ConfigSnapshot| router_model_config(config, "assistant").model;
        assert_eq!(name(&with_router(None, None)), "chat-a");
        assert_eq!(name(&with_router(None, Some("memory"))), "memory-a");
        assert_eq!(name(&with_router(Some("memory"), Some("chat"))), "memory-a");
        assert_eq!(name(&with_router(Some("chat"), Some("memory"))), "chat-a");
        // 别的 Agent 没声明就是主模型。
        assert_eq!(
            router_model_config(&with_router(Some("memory"), None), "gone").model,
            "chat-a"
        );
    }

    #[tokio::test]
    async fn a_judgement_runs_one_toolless_turn_on_the_router_model() {
        let llm = FakeLlm::new(vec![vec![text_round(
            1,
            "```json\n{\"route\":\"continue\",\"task_id\":\"t1\"}\n```",
        )]]);
        let config = with_router(Some("memory"), None);

        let routed = judge(
            llm.clone(),
            &config,
            "assistant",
            &input(RouteScope::Private),
        )
        .await;

        assert_eq!(routed.route, Route::Continue { task: "t1".into() });
        assert_eq!(routed.fallback, None);
        assert_eq!(llm.models(), ["memory-a"], "用的是 router_model 指的模型");
    }

    #[tokio::test]
    async fn a_failing_backend_falls_back_after_the_attempt_limit() {
        let error = || {
            Err(LlmError::Rejected {
                status: 503,
                message: "down".into(),
            })
        };
        let llm = FakeLlm::new(vec![vec![error()], vec![error()], vec![error()]]);

        let routed = judge(
            llm.clone(),
            &with_router(None, None),
            "assistant",
            &input(RouteScope::Group),
        )
        .await;

        assert_eq!(routed.route, Route::Ignore);
        assert!(matches!(
            routed.fallback.map(|fallback| fallback.reason),
            Some(RouteFailure::Model(_))
        ));
        assert_eq!(llm.turns(), MAX_ATTEMPTS as usize);
    }
}
