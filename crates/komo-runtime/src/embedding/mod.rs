//! EmbeddingClient：空间指纹与维度校验（§9.5、§13.5）。
//!
//! 两种后端，都**只发向量接口自己的字段**——「不向 embedding 端点发送聊天工具字段」
//! （§13.3），也不发 effort：`/embeddings` 与 `/api/embed` 都没有这个参数，而
//! 「dimensions、批大小和输入长度是另一组控制，不用 effort 冒充」。
//!
//! 空间指纹（§9.5）在构造时就定死：provider、端点、模型、revision、维度、预处理版本、
//! 文档 / 查询前缀、是否归一化、距离规则。**凭证不进指纹**——[`EmbeddingSpace`] 里根本
//! 没有能放凭证的字段，这条性质因此是结构上的，不是约定。

mod ollama;
mod openai;

use std::sync::Arc;

use komo_kernel::traits::{EmbedError, EmbeddingClient};
use komo_kernel::types::model::{
    DistanceRule, EmbeddingConfig, EmbeddingSpace, InputKind, ModelConfig, Vector,
};

use crate::config::Secrets;
use crate::llm::transport::{HttpRequest, HttpResponse, HttpTransport, TransportError};

pub use ollama::OllamaEmbeddings;
pub use openai::OpenAiEmbeddings;

/// OpenAI 兼容的 `/embeddings`。
pub const OPENAI_COMPATIBLE: &str = "embeddings";
/// Ollama 的 `/api/embed`。
pub const OLLAMA: &str = "ollama_embeddings";

/// 文本预处理的版本号。改了预处理就要改它——同一段文本经不同预处理得到的向量不在
/// 一个空间里（§9.5）。
pub const PREPROCESSING: &str = "trim-v1";

/// 探测维度时用的那段文本。
const PROBE_TEXT: &str = "komo";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EmbeddingBuildError {
    #[error("不认识的向量 provider `{provider}`：首版接入 `{OPENAI_COMPATIBLE}` 与 `{OLLAMA}`")]
    UnknownProvider { provider: String },
    /// §13.3：普通向量接口没有 effort 参数时必须省略。
    #[error("模型 {model} 的向量接口没有 effort 参数，请去掉对应 `model.<alias>.effort`")]
    EffortNotSupported { model: String },
    #[error(transparent)]
    Embed(#[from] EmbedError),
}

/// 维度已知时**不碰网络**地造一个客户端。
pub fn build_embedding(
    config: &EmbeddingConfig,
    secrets: &Secrets,
    transport: Arc<dyn HttpTransport>,
    dimensions: u32,
) -> Result<Arc<dyn EmbeddingClient>, EmbeddingBuildError> {
    if dimensions == 0 {
        return Err(EmbedError::InvalidVector("维度不能是 0".into()).into());
    }
    build_with(config, secrets, transport, dimensions)
}

/// 同上，但允许 `dimensions == 0`——那是**探测模式**，见 [`Backend::check`]。
fn build_with(
    config: &EmbeddingConfig,
    secrets: &Secrets,
    transport: Arc<dyn HttpTransport>,
    dimensions: u32,
) -> Result<Arc<dyn EmbeddingClient>, EmbeddingBuildError> {
    if config.model.effort.is_some() {
        return Err(EmbeddingBuildError::EffortNotSupported {
            model: config.model.model.clone(),
        });
    }
    let key = secrets.get(&config.model.api_key_env).map(str::to_string);
    let space = space_for(config, dimensions);
    match config.model.provider.as_str() {
        OPENAI_COMPATIBLE | "openai_compatible" => Ok(Arc::new(OpenAiEmbeddings::new(
            config.clone(),
            space,
            key,
            transport,
        ))),
        OLLAMA | "ollama" => Ok(Arc::new(OllamaEmbeddings::new(
            config.clone(),
            space,
            key,
            transport,
        ))),
        other => Err(EmbeddingBuildError::UnknownProvider {
            provider: other.to_string(),
        }),
    }
}

/// 造一个客户端，维度省略时先**探测一次**并固定下来（§9.5：「省略时使用模型返回维度，
/// 校验后固定到索引代次」）。
pub async fn connect_embedding(
    config: &EmbeddingConfig,
    secrets: &Secrets,
    transport: Arc<dyn HttpTransport>,
) -> Result<Arc<dyn EmbeddingClient>, EmbeddingBuildError> {
    let dimensions = match config.dimensions {
        Some(dimensions) => dimensions,
        None => {
            // 探测模式的客户端（`dimensions == 0`）只发这一次请求，拿到维度就扔掉。
            let probe = build_with(config, secrets, Arc::clone(&transport), 0)?;
            let dimensions = probe
                .embed(InputKind::Document, &[PROBE_TEXT.to_string()])
                .await?
                .first()
                .map(|vector| vector.dimensions() as u32)
                .unwrap_or_default();
            if dimensions == 0 {
                return Err(EmbedError::InvalidVector("探测没有拿到维度".into()).into());
            }
            tracing::info!(
                model = %config.model.model,
                endpoint = %config.model.base_url,
                dimensions,
                "向量维度由一次探测固定下来"
            );
            dimensions
        }
    };
    build_embedding(config, secrets, transport, dimensions)
}

/// §9.5 的空间指纹。
fn space_for(config: &EmbeddingConfig, dimensions: u32) -> EmbeddingSpace {
    EmbeddingSpace {
        provider: config.model.provider.clone(),
        endpoint: config.model.base_url.clone(),
        model: config.model.model.clone(),
        revision: config.revision.clone(),
        dimensions,
        preprocessing: PREPROCESSING.to_string(),
        // §9.5：查询与文档各自的输入规则，两条都进指纹——同一个模型换了前缀就是另一个
        // 空间，旧向量不能拿来比。没配就是空前缀，那也是一个明确的、进了指纹的选择。
        document_prefix: config.document_prefix.clone().unwrap_or_default(),
        query_prefix: config.query_prefix.clone().unwrap_or_default(),
        // 我们不动服务端给的向量，所以不能声称它是归一化的。
        normalized: false,
        distance: DistanceRule::Cosine,
        // 两个后端都没有这个参数；配了的话在 `build_embedding` 就被拒了。
        effort: None,
    }
}

/// 两个后端共用的那点事：发请求、校验、按空间前缀处理输入。
pub(crate) struct Backend {
    pub(crate) space: EmbeddingSpace,
    pub(crate) api_key: Option<String>,
    pub(crate) transport: Arc<dyn HttpTransport>,
    pub(crate) timeout: std::time::Duration,
}

impl Backend {
    pub(crate) fn new(
        config: &ModelConfig,
        space: EmbeddingSpace,
        api_key: Option<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Backend {
            space,
            api_key,
            transport,
            timeout: std::time::Duration::from_secs(config.timeout_secs.max(1)),
        }
    }

    /// 按空间规定的那一套处理输入（§9.5：文档侧与查询侧的规则不同，必须用同一空间
    /// 规定的那一套）。
    pub(crate) fn prepare(&self, kind: InputKind, texts: &[String]) -> Vec<String> {
        let prefix = match kind {
            InputKind::Document => &self.space.document_prefix,
            InputKind::Query => &self.space.query_prefix,
        };
        texts
            .iter()
            .map(|text| format!("{prefix}{}", text.trim()))
            .collect()
    }

    pub(crate) async fn post(
        &self,
        url: String,
        body: serde_json::Value,
    ) -> Result<HttpResponse, EmbedError> {
        let request = HttpRequest::new(url, body)
            .with_key(self.api_key.clone())
            .with_timeout(self.timeout);
        self.transport
            .post(request)
            .await
            .map_err(|error| match error {
                TransportError::Timeout => EmbedError::Timeout,
                TransportError::Failed(message) => EmbedError::Unavailable(message),
            })
    }

    /// 非 2xx 的统一处理。5xx / 429 是"端点不可用"，其余是请求本身有问题。
    pub(crate) async fn refuse(&self, response: HttpResponse) -> EmbedError {
        let status = response.status;
        let body = response.text().await.unwrap_or_default();
        let message = format!(
            "HTTP {status}：{}",
            body.trim().chars().take(300).collect::<String>()
        );
        if status >= 500 || status == 429 {
            EmbedError::Unavailable(message)
        } else {
            EmbedError::Other(message)
        }
    }

    /// 维度、数值、范数（§9.5）。**截断或结构错误的向量不接受。**
    ///
    /// `space.dimensions == 0` 是**探测模式**：维度正是这一次要问出来的东西，所以这一
    /// 项跳过，数值与范数照查。它只在 [`connect_embedding`] 里出现，且那个客户端拿到
    /// 答案就被扔掉。
    pub(crate) fn check(
        &self,
        vectors: Vec<Vec<f32>>,
        expected: usize,
    ) -> Result<Vec<Vector>, EmbedError> {
        if vectors.len() != expected {
            return Err(EmbedError::InvalidVector(format!(
                "要了 {expected} 条向量，回来 {} 条",
                vectors.len()
            )));
        }
        vectors
            .into_iter()
            .map(|values| {
                let vector = Vector(values);
                let usable = if self.space.dimensions == 0 {
                    vector.is_usable(vector.dimensions() as u32)
                } else {
                    vector.is_usable(self.space.dimensions)
                };
                if usable {
                    Ok(vector)
                } else {
                    Err(EmbedError::InvalidVector(format!(
                        "维度 / 数值校验不过：空间是 {} 维，实际 {} 维",
                        self.space.dimensions,
                        vector.dimensions()
                    )))
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
