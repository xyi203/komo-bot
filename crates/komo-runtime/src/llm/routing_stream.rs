//! 按模型配置挑无状态后端的 [`ModelStream`]（今天 [`RoutingLlm`](super::RoutingLlm) 的
//! 无状态版，`docs/storage-durable.md` §12.3）。
//!
//! 生成任务（`durable::harness`）每次请求都把这一轮用的 [`ModelConfig`] 放在
//! [`ModelRequest::model`] 里，所以路由键就是那份配置的身份：换了端点、模型或凭证变量就是
//! 另一个实例。主模型与记忆模型在构造时就建好（配置有问题要在接线时炸，不是凌晨三点），
//! 别的配置（Cron 的模型覆盖、Profile 指定的别名）第一次用到时按需建。
//!
//! 测试可以用 [`RoutingStream::with_backend`] 给某份配置塞一个自己的后端。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::durable::AssistantMessage;
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::types::model::{ModelConfig, ModelRole};
use komo_kernel::types::turn::LlmError;

use super::{LlmBuildError, LlmFactory, ModelRequest, ModelStream, model_key};

pub struct RoutingStream {
    factory: LlmFactory,
    backends: Mutex<BTreeMap<String, Arc<dyn ModelStream>>>,
    roles: BTreeMap<String, ModelRole>,
}

impl std::fmt::Debug for RoutingStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingStream")
            .field("known", &self.roles)
            .finish()
    }
}

impl RoutingStream {
    /// 主模型 + 记忆模型：同一个 trait 的两个实例（§13.3）。
    pub fn from_snapshot(
        snapshot: &ConfigSnapshot,
        factory: LlmFactory,
    ) -> Result<Self, LlmBuildError> {
        let mut backends = BTreeMap::new();
        let mut roles = BTreeMap::new();
        let main = factory.build_stream(&snapshot.model, ModelRole::Main)?;
        roles.insert(model_key(&snapshot.model), ModelRole::Main);
        backends.insert(model_key(&snapshot.model), main);
        if snapshot.memory.enabled {
            let key = model_key(&snapshot.memory.model);
            if let std::collections::btree_map::Entry::Vacant(slot) = backends.entry(key.clone()) {
                let memory = factory.build_stream(&snapshot.memory.model, ModelRole::Memory)?;
                roles.insert(key, ModelRole::Memory);
                slot.insert(memory);
            }
        }
        Ok(Self {
            factory,
            backends: Mutex::new(backends),
            roles,
        })
    }

    /// 只有一个后端时。
    pub fn single(config: &ModelConfig, factory: LlmFactory) -> Result<Self, LlmBuildError> {
        let backend = factory.build_stream(config, ModelRole::Main)?;
        Ok(Self {
            factory,
            backends: Mutex::new(BTreeMap::from([(model_key(config), backend)])),
            roles: BTreeMap::from([(model_key(config), ModelRole::Main)]),
        })
    }

    /// 没有预建后端，按需建（测试里再用 [`Self::with_backend`] 指定）。
    pub fn lazy(factory: LlmFactory) -> Self {
        Self {
            factory,
            backends: Mutex::new(BTreeMap::new()),
            roles: BTreeMap::new(),
        }
    }

    /// 给某份配置指定后端（覆盖同键的旧实例）。
    pub fn with_backend(self, config: &ModelConfig, backend: Arc<dyn ModelStream>) -> Self {
        self.backends
            .lock()
            .expect("路由表")
            .insert(model_key(config), backend);
        self
    }

    fn backend_for(&self, config: &ModelConfig) -> Result<Arc<dyn ModelStream>, LlmBuildError> {
        let key = model_key(config);
        if let Some(backend) = self.backends.lock().expect("路由表").get(&key) {
            return Ok(Arc::clone(backend));
        }
        let role = self.roles.get(&key).copied().unwrap_or(ModelRole::Main);
        let backend = self.factory.build_stream(config, role)?;
        self.backends
            .lock()
            .expect("路由表")
            .insert(key, Arc::clone(&backend));
        Ok(backend)
    }
}

#[async_trait]
impl ModelStream for RoutingStream {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        progress: &mut (dyn for<'m> FnMut(&'m AssistantMessage) + Send),
    ) -> Result<AssistantMessage, LlmError> {
        let backend = self.backend_for(request.model)?;
        backend.stream(request, progress).await
    }
}

#[cfg(test)]
mod tests {
    use komo_kernel::durable::{ContentBlock, StopReason};

    use super::*;
    use crate::config::{EffortCapabilities, Secrets};

    struct Fixed(&'static str);

    #[async_trait]
    impl ModelStream for Fixed {
        async fn stream(
            &self,
            _request: &ModelRequest<'_>,
            _progress: &mut (dyn for<'m> FnMut(&'m AssistantMessage) + Send),
        ) -> Result<AssistantMessage, LlmError> {
            Ok(AssistantMessage {
                content: vec![ContentBlock::text(self.0)],
                stop_reason: StopReason::Stop,
                timestamp: 0,
                extra: Default::default(),
            })
        }
    }

    fn config(model: &str) -> ModelConfig {
        ModelConfig {
            provider: "responses".into(),
            base_url: "http://localhost".into(),
            model: model.into(),
            api_key_env: "KEY".into(),
            auth: None,
            effort: None,
            efforts: None,
            timeout_secs: 5,
            context_window: None,
        }
    }

    // 对照 `RoutingLlm`：同一份配置的身份决定实例，换了模型就是另一个后端。
    #[tokio::test]
    async fn the_backend_is_picked_by_the_requests_model_config() {
        let (main, memory) = (config("main"), config("memory"));
        let factory = LlmFactory::new(Arc::new(Secrets::new()), EffortCapabilities::builtin());
        let router = RoutingStream::lazy(factory)
            .with_backend(&main, Arc::new(Fixed("from main")))
            .with_backend(&memory, Arc::new(Fixed("from memory")));
        for (config, expected) in [(&main, "from main"), (&memory, "from memory")] {
            let request = ModelRequest::new(config, &[]);
            let message = router.stream(&request, &mut |_| {}).await.unwrap();
            let ContentBlock::Text { text, .. } = &message.content[0] else {
                panic!("text expected");
            };
            assert_eq!(text, expected);
        }
    }
}
