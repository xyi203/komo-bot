//! LlmClient / TurnDriver 的协议适配器（§13.3、§13.5）。
//!
//! 两个生成协议 adapter：OpenAI Chat Completions 与 Responses。二者共用传输与
//! [`LlmClient`] seam，但请求、流式终态和 provider 回放各自在自己的模块里实现。
//!
//! 主模型与记忆模型是**同一个 trait 的两个实例**（§13.3），按各自的 [`ModelConfig`]
//! 构造；[`RoutingLlm`] 按每个 Run 固定下来的那份配置挑实例，所以配置热重载不会在半路
//! 换掉正在跑的那个（§3 第 2 步）。

mod chat;
mod responses;
mod sse;
pub mod transport;
mod wire;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::traits::{LlmClient, TurnDriver};
use komo_kernel::types::model::{ModelConfig, ModelRole};
use komo_kernel::types::turn::{LlmError, TurnRequest};

pub use chat::ChatCompletionsLlm;
pub use responses::{OpenAiResponsesLlm, SystemPreamble};
pub use transport::{HttpTransport, ReqwestTransport, TransportError};

use crate::config::{EffortCapabilities, Secrets};

/// 本 crate 认识的生成协议（§13.2）。
pub const CHAT_COMPLETIONS: &str = "chat_completions";
pub const RESPONSES: &str = "responses";
const LEGACY_OPENAI_RESPONSES: &str = "openai_responses";

/// 构造一个后端时会出的问题。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LlmBuildError {
    #[error("不认识 api_backend `{provider}`：生成协议只有 `{CHAT_COMPLETIONS}` 与 `{RESPONSES}`")]
    UnknownProvider { provider: String },
    /// 档位不可用一类——在**请求前**就定得下来的那些（§13.3）。
    #[error(transparent)]
    Model(#[from] LlmError),
}

impl From<LlmBuildError> for LlmError {
    fn from(error: LlmBuildError) -> Self {
        match error {
            LlmBuildError::Model(error) => error,
            other => LlmError::Rejected {
                status: 501,
                message: other.to_string(),
            },
        }
    }
}

/// 这次失败可以重试吗。
///
/// 穷举匹配，因为「哪些错误可以再发一次」这件事必须随 [`LlmError`] 一起长——新增一个
/// 变体时编译器会在这里停下来问。两条是硬的：**流没收到终止帧**（`Incomplete`）可以
/// 重试；**effort 不被支持**不可以——重试只会再发一次同样的参数，而"删掉 effort 再试"
/// 是 §13.3 明令禁止的。
pub fn is_retryable(error: &LlmError) -> bool {
    match error {
        // 408 请求超时 / 409 冲突 / 425 too early / 429 限流 / 5xx。
        LlmError::Rejected { status, .. } => {
            matches!(status, 408 | 409 | 425 | 429) || (500..600).contains(status)
        }
        LlmError::Timeout => true,
        // 「回复未收齐」——包括流断在终止帧之前。
        LlmError::Incomplete => true,
        LlmError::Transport(_) => true,
        LlmError::UnsupportedEffort { .. } => false,
        // §8.5：结果与用量都未知时保留未知标记，不能当成零，也不能自动再来一次。
        LlmError::Unknown(_) => false,
    }
}

/// 适配器共用的出站通道。连接池共用一份就够；每次请求自己的超时由
/// [`ModelConfig::timeout_secs`] 给。TLS provider 由 bin 在 `main` 里装（§13.4）。
pub fn default_transport() -> Arc<dyn HttpTransport> {
    Arc::new(ReqwestTransport::new())
}

/// 按 [`ModelConfig`] 造一个后端。
pub fn build_llm(
    config: &ModelConfig,
    role: ModelRole,
    secrets: &Secrets,
    caps: &EffortCapabilities,
    transport: Arc<dyn HttpTransport>,
) -> Result<Arc<dyn LlmClient>, LlmBuildError> {
    LlmFactory::new(Arc::new(secrets.clone()), caps.clone())
        .with_transport(transport)
        .build(config, role)
}

/// 造后端的那点东西打包在一起：凭证、档位声明、HTTP 客户端。
///
/// 凭证的**值**只在 [`Secrets`] 里；快照里是指纹。凭证变了就换掉对应的客户端
/// （§3 第 3 步）——做法是重新造一个 [`LlmFactory`] 和一个 [`RoutingLlm`]，而不是
/// 在运行中的实例上改一个字段。
#[derive(Clone)]
pub struct LlmFactory {
    secrets: Arc<Secrets>,
    caps: EffortCapabilities,
    transport: Arc<dyn HttpTransport>,
    preamble: Option<Arc<dyn SystemPreamble>>,
}

impl std::fmt::Debug for LlmFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmFactory")
            .field("secrets", &self.secrets)
            .finish()
    }
}

impl LlmFactory {
    pub fn new(secrets: Arc<Secrets>, caps: EffortCapabilities) -> Self {
        LlmFactory {
            secrets,
            caps,
            transport: default_transport(),
            preamble: None,
        }
    }

    pub fn with_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// 记忆注入的接口（§9.4）：正文由 MemoryManager 给，这里只负责把它放进系统提示。
    pub fn with_preamble(mut self, preamble: Arc<dyn SystemPreamble>) -> Self {
        self.preamble = Some(preamble);
        self
    }

    pub fn build(
        &self,
        config: &ModelConfig,
        role: ModelRole,
    ) -> Result<Arc<dyn LlmClient>, LlmBuildError> {
        match config.provider.as_str() {
            RESPONSES | LEGACY_OPENAI_RESPONSES => {
                let key = self.secrets.get(&config.api_key_env).map(str::to_string);
                let mut client = OpenAiResponsesLlm::new(
                    config.clone(),
                    role,
                    key,
                    Arc::clone(&self.transport),
                    self.caps.clone(),
                )?;
                if let Some(preamble) = &self.preamble {
                    client = client.with_preamble(Arc::clone(preamble));
                }
                Ok(Arc::new(client))
            }
            CHAT_COMPLETIONS => {
                let key = self.secrets.get(&config.api_key_env).map(str::to_string);
                let mut client = ChatCompletionsLlm::new(
                    config.clone(),
                    role,
                    key,
                    Arc::clone(&self.transport),
                    self.caps.clone(),
                )?;
                if let Some(preamble) = &self.preamble {
                    client = client.with_preamble(Arc::clone(preamble));
                }
                Ok(Arc::new(client))
            }
            other => Err(LlmBuildError::UnknownProvider {
                provider: other.to_string(),
            }),
        }
    }
}

/// 按每个 Run 自己那份模型配置分发。
///
/// 一个 Run 在 `accept_input` 时抓住一份 [`ModelConfig`]（§3 第 2 步），之后整轮都用它
/// ——所以这里按**那份配置**找实例，而不是按"当前主模型"。主模型与记忆模型在构造时就
/// 建好（配置有问题要在接线时就炸，不是凌晨三点），Cron Job 的模型覆盖（§10）第一次
/// 用到时按需建。
pub struct RoutingLlm {
    factory: LlmFactory,
    clients: Mutex<BTreeMap<String, Arc<dyn LlmClient>>>,
    roles: BTreeMap<String, ModelRole>,
}

impl std::fmt::Debug for RoutingLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingLlm")
            .field("known", &self.roles)
            .finish()
    }
}

/// 一份模型配置的身份：换了端点、模型或凭证变量就是另一个实例。
fn model_key(config: &ModelConfig) -> String {
    format!(
        "{}|{}|{}|{}",
        config.provider, config.base_url, config.model, config.api_key_env
    )
}

impl RoutingLlm {
    /// 主模型 + 记忆模型：**同一个 trait 的两个实例**（§13.3）。
    pub fn from_snapshot(
        snapshot: &ConfigSnapshot,
        factory: LlmFactory,
    ) -> Result<Self, LlmBuildError> {
        let mut clients = BTreeMap::new();
        let mut roles = BTreeMap::new();

        let main = factory.build(&snapshot.model, ModelRole::Main)?;
        roles.insert(model_key(&snapshot.model), ModelRole::Main);
        clients.insert(model_key(&snapshot.model), main);

        if snapshot.memory.enabled {
            let key = model_key(&snapshot.memory.model);
            // 记忆模型可能和主模型是同一份配置（整段省略时继承）——那就只有一个实例。
            if let std::collections::btree_map::Entry::Vacant(slot) = clients.entry(key.clone()) {
                let memory = factory.build(&snapshot.memory.model, ModelRole::Memory)?;
                roles.insert(key, ModelRole::Memory);
                slot.insert(memory);
            }
        }

        Ok(RoutingLlm {
            factory,
            clients: Mutex::new(clients),
            roles,
        })
    }

    /// 只有一个后端时。
    pub fn single(config: &ModelConfig, factory: LlmFactory) -> Result<Self, LlmBuildError> {
        let client = factory.build(config, ModelRole::Main)?;
        Ok(RoutingLlm {
            factory,
            clients: Mutex::new(BTreeMap::from([(model_key(config), client)])),
            roles: BTreeMap::from([(model_key(config), ModelRole::Main)]),
        })
    }

    fn client_for(&self, config: &ModelConfig) -> Result<Arc<dyn LlmClient>, LlmBuildError> {
        let key = model_key(config);
        if let Some(client) = self.clients.lock().expect("路由表").get(&key) {
            return Ok(Arc::clone(client));
        }
        // Cron Job 的覆盖按完整模型配置解析（§10），仍然是主模型这个角色。
        let role = self.roles.get(&key).copied().unwrap_or(ModelRole::Main);
        let client = self.factory.build(config, role)?;
        self.clients
            .lock()
            .expect("路由表")
            .insert(key, Arc::clone(&client));
        Ok(client)
    }
}

#[async_trait]
impl LlmClient for RoutingLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        let client = self.client_for(&req.model)?;
        client.begin_turn(req).await
    }
}

#[cfg(test)]
mod tests;
