//! TypeSafe「System One」客户端（§9.4 的可选判断后端）。
//!
//! 只做三件事：把 [`SystemOneRequest`] 发出去、把 [`SystemOneResponse`] 解回来、把失败
//! 分成可重试与不可重试两类。**它不决定判断怎么用**——那是调用点的事（记忆重排）。
//!
//! reqwest 不在这里出现：出站走 [`HttpTransport`]（与模型、向量适配器同一层薄壳），
//! 所以"请求体是什么形状、什么样的答复算解不开"能在没有网络、没有凭证的情况下断言。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use komo_kernel::protocol::config::TypesafeConfig;
use komo_kernel::traits::SystemOne;
use komo_kernel::types::systemone::{SystemOneError, SystemOneRequest, SystemOneResponse};

use crate::config::Secrets;
use crate::llm::transport::{HttpRequest, HttpTransport, TransportError};

/// 一次判断最多试几次（含第一次）。4xx 不重试，429 / 5xx 与传输失败重试。
const MAX_ATTEMPTS: u32 = 3;

/// 造一个判断后端。**不碰网络**——端点、模型、凭证齐了就成。
pub fn connect(
    config: &TypesafeConfig,
    secrets: &Secrets,
    transport: Arc<dyn HttpTransport>,
) -> Result<Arc<dyn SystemOne>, SystemOneError> {
    if !config.enabled {
        return Err(SystemOneError::Unconfigured(
            "[typesafe] 没有 enabled".into(),
        ));
    }
    if config.endpoint.trim().is_empty() {
        return Err(SystemOneError::Unconfigured("endpoint 是空的".into()));
    }
    if config.model.trim().is_empty() {
        return Err(SystemOneError::Unconfigured("model 是空的".into()));
    }
    let Some(key) = secrets.get(&config.api_key).map(str::to_string) else {
        return Err(SystemOneError::Unconfigured(format!(
            "`.env` 里没有 {} 的值",
            config.api_key
        )));
    };
    Ok(Arc::new(Client {
        config: config.clone(),
        key: Some(key),
        transport,
    }))
}

struct Client {
    config: TypesafeConfig,
    /// 凭证值。**不进 `Debug`**：`Client` 的手写 `Debug` 只说有没有。
    key: Option<String>,
    transport: Arc<dyn HttpTransport>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypesafeClient")
            .field("endpoint", &self.config.endpoint)
            .field("model", &self.config.model)
            .field("has_key", &self.key.is_some())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl SystemOne for Client {
    async fn ask(&self, request: SystemOneRequest) -> Result<SystemOneResponse, SystemOneError> {
        let body = serde_json::to_value(&request)
            .map_err(|error| SystemOneError::Decode(format!("请求序列化不出来：{error}")))?;
        let mut attempt = 1;
        loop {
            let http = HttpRequest::new(self.config.endpoint.clone(), body.clone())
                .with_key(self.key.clone())
                .with_timeout(Duration::from_secs(self.config.timeout_secs.max(1)));
            let outcome = match self.transport.post(http).await {
                Ok(response) => read(response).await,
                Err(error) => Err(classify_transport(&error)),
            };
            match outcome {
                Ok(response) => return Ok(response),
                Err(error) if attempt < MAX_ATTEMPTS && retryable(&error) => {
                    // 退避只跟尝试次数走：判断是可选的，宁可这一次不做，也不在这里排长队
                    // （调用点已经给了"没有判断"的那条路）。
                    let backoff = Duration::from_millis(200 * u64::from(attempt));
                    tracing::info!(%error, attempt, "判断后端这一次没成，退避后重试");
                    tokio::time::sleep(backoff).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// 读一次响应：状态码 → 错误，正文 → 值。
async fn read(
    response: crate::llm::transport::HttpResponse,
) -> Result<SystemOneResponse, SystemOneError> {
    let status = response.status;
    let text = response
        .text()
        .await
        .map_err(|error| SystemOneError::Transport(error.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(SystemOneError::Status {
            status,
            // 错误正文是给日志看的，截一段就够——它会进 trace，别把一整页 HTML 抄进去。
            message: text.chars().take(400).collect(),
        });
    }
    serde_json::from_str(&text).map_err(|error| {
        SystemOneError::Decode(format!(
            "{error}：{}",
            text.chars().take(400).collect::<String>()
        ))
    })
}

fn classify_transport(error: &TransportError) -> SystemOneError {
    match error {
        TransportError::Timeout => SystemOneError::Transport("超时".into()),
        TransportError::Failed(message) => SystemOneError::Transport(message.clone()),
    }
}

/// 值得再试一次的失败：429、5xx、以及传输层（连不上 / 超时）。
fn retryable(error: &SystemOneError) -> bool {
    match error {
        SystemOneError::Status { status, .. } => *status == 429 || *status >= 500,
        SystemOneError::Transport(_) => true,
        SystemOneError::Decode(_) | SystemOneError::Unconfigured(_) => false,
    }
}

#[cfg(test)]
mod tests;
