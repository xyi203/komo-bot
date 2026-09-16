//! Ollama 的 `POST {base_url}/api/embed`。
//!
//! 它和 `/embeddings` 的差别只有路径、字段名和"不带 Bearer"，所以除了这三处以外都走
//! [`Backend`]。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::traits::{EmbedError, EmbeddingClient};
use komo_kernel::types::model::{EmbeddingConfig, EmbeddingSpace, InputKind, Vector};
use serde::Deserialize;
use serde_json::json;

use super::Backend;
use crate::llm::transport::HttpTransport;

pub struct OllamaEmbeddings {
    backend: Backend,
    model: String,
    endpoint: String,
}

impl std::fmt::Debug for OllamaEmbeddings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaEmbeddings")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("dimensions", &self.backend.space.dimensions)
            .finish()
    }
}

impl OllamaEmbeddings {
    pub fn new(
        config: EmbeddingConfig,
        space: EmbeddingSpace,
        api_key: Option<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        OllamaEmbeddings {
            endpoint: format!("{}/api/embed", config.model.base_url.trim_end_matches('/')),
            model: config.model.model.clone(),
            backend: Backend::new(&config.model, space, api_key, transport),
        }
    }
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    #[serde(default)]
    embeddings: Vec<Vec<f32>>,
}

#[async_trait]
impl EmbeddingClient for OllamaEmbeddings {
    fn space(&self) -> &EmbeddingSpace {
        &self.backend.space
    }

    async fn embed(&self, kind: InputKind, texts: &[String]) -> Result<Vec<Vector>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let input = self.backend.prepare(kind, texts);
        // `/api/embed` 的字段就这两个（外加可选的 options / truncate）——没有 effort，
        // 也没有任何聊天字段。
        let body = json!({ "model": self.model, "input": input });

        let started = std::time::Instant::now();
        let response = self.backend.post(self.endpoint.clone(), body).await;
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
        let parsed: EmbedResponse = serde_json::from_str(&text)
            .map_err(|e| EmbedError::InvalidVector(format!("回复解析不了：{e}")))?;
        let vectors = self.backend.check(parsed.embeddings, texts.len())?;

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
