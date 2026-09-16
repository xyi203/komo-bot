//! OpenAI 兼容的 `POST {base_url}/embeddings`。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::traits::{EmbedError, EmbeddingClient};
use komo_kernel::types::model::{EmbeddingConfig, EmbeddingSpace, InputKind, Vector};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::Backend;
use crate::llm::transport::HttpTransport;

pub struct OpenAiEmbeddings {
    backend: Backend,
    model: String,
    endpoint: String,
    /// 服务端支持时按它裁剪维度；省略维度的配置里它是 `None`，维度由返回值决定。
    dimensions: Option<u32>,
}

impl std::fmt::Debug for OpenAiEmbeddings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiEmbeddings")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("dimensions", &self.backend.space.dimensions)
            .finish()
    }
}

impl OpenAiEmbeddings {
    pub fn new(
        config: EmbeddingConfig,
        space: EmbeddingSpace,
        api_key: Option<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        OpenAiEmbeddings {
            endpoint: format!("{}/embeddings", config.model.base_url.trim_end_matches('/')),
            model: config.model.model.clone(),
            dimensions: config.dimensions,
            backend: Backend::new(&config.model, space, api_key, transport),
        }
    }
}

#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    #[serde(default)]
    data: Vec<EmbeddingRow>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingRow {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    embedding: Vec<f32>,
}

#[async_trait]
impl EmbeddingClient for OpenAiEmbeddings {
    fn space(&self) -> &EmbeddingSpace {
        &self.backend.space
    }

    async fn embed(&self, kind: InputKind, texts: &[String]) -> Result<Vec<Vector>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let input = self.backend.prepare(kind, texts);

        // **只有向量接口自己的字段**：没有 messages / tools / reasoning_effort（§13.3）。
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("input".into(), json!(input));
        if let Some(dimensions) = self.dimensions {
            body.insert("dimensions".into(), json!(dimensions));
        }

        let started = std::time::Instant::now();
        let response = self
            .backend
            .post(self.endpoint.clone(), Value::Object(body))
            .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(role = "memory.embedding", model = %self.model, elapsed_ms, error = %error, "向量请求失败");
                return Err(error);
            }
        };
        if !(200..300).contains(&response.status) {
            let error = self.backend.refuse(response).await;
            tracing::warn!(role = "memory.embedding", model = %self.model, elapsed_ms, error = %error, "向量请求被拒绝");
            return Err(error);
        }

        let text = response
            .text()
            .await
            .map_err(|e| EmbedError::Unavailable(e.to_string()))?;
        let parsed: EmbeddingsResponse = serde_json::from_str(&text)
            .map_err(|e| EmbedError::InvalidVector(format!("回复解析不了：{e}")))?;

        // 服务端允许乱序返回，按 index 排回去——错位的向量是最难查的那种错。
        let mut rows = parsed.data;
        rows.sort_by_key(|row| row.index);
        let vectors = self.backend.check(
            rows.into_iter().map(|row| row.embedding).collect(),
            texts.len(),
        )?;

        // §13.3：每次向量请求记录角色、模型身份、耗时；不记密钥。
        tracing::info!(
            role = "memory.embedding",
            model = %self.model,
            endpoint = %self.endpoint,
            kind = ?kind,
            batch = texts.len(),
            dimensions = self.backend.space.dimensions,
            elapsed_ms,
            "embedding request completed"
        );
        Ok(vectors)
    }
}
