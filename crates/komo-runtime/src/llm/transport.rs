//! HTTP 出站的那一层薄壳：生成与向量适配器共用它。
//!
//! 它存在有两个理由，都不是"以后可能换 HTTP 库"：
//!
//! - **reqwest 只出现在一个文件里。**`rustls-no-provider` 要求进程在造出第一个
//!   `Client` 之前装好 crypto provider（§13.4：由 bin 在 `main` 里装 ring）。装 provider
//!   是进程的事，而适配器的逻辑不该因此只能在装过 provider 的进程里被测。
//! - **流式收帧、错误分类、请求体形状**这三件事是协议适配器的正事，能在没有网络、
//!   没有 TLS 的情况下逐条断言。

use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde_json::Value;

/// 传输层失败。**超时单独成一个变体**：调用方要把它映射成协议自己的超时错误，而不是
/// 一个说不清的传输错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error("超时")]
    Timeout,
    #[error("{0}")]
    Failed(String),
}

/// 一次出站请求。
#[derive(Clone)]
pub struct HttpRequest {
    pub url: String,
    /// Bearer 凭证。**不进任何 Debug / 日志**——见下面的手写 `Debug`。
    pub api_key: Option<String>,
    pub timeout: Duration,
    pub body: Value,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 凭证不进 Debug：一个会把自己整个印出来的结构迟早会被 `{:?}` 到某条日志里。
        f.debug_struct("HttpRequest")
            .field("url", &self.url)
            .field("has_key", &self.api_key.is_some())
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl HttpRequest {
    pub fn new(url: impl Into<String>, body: Value) -> Self {
        HttpRequest {
            url: url.into(),
            api_key: None,
            timeout: Duration::from_secs(120),
            body,
        }
    }

    pub fn with_key(mut self, api_key: Option<String>) -> Self {
        self.api_key = api_key;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// 响应：状态码 + 还没读的正文流。
pub struct HttpResponse {
    pub status: u16,
    pub body: BoxStream<'static, Result<Vec<u8>, TransportError>>,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .finish()
    }
}

impl HttpResponse {
    /// 把正文收成一个字符串（错误正文、非流式接口用）。
    pub async fn text(mut self) -> Result<String, TransportError> {
        use futures_util::StreamExt;
        let mut out = Vec::new();
        while let Some(chunk) = self.body.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

#[async_trait]
pub trait HttpTransport: Send + Sync + fmt::Debug {
    async fn post(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// 生产实现。
#[derive(Debug, Default)]
pub struct ReqwestTransport {
    /// 懒建：`Client::builder().build()` 在没有 crypto provider 的进程里会 panic
    /// （§13.4 由 bin 在 `main` 里装），所以第一次真的要发请求时才建。
    client: OnceLock<reqwest::Client>,
}

impl ReqwestTransport {
    pub fn new() -> Self {
        Self::default()
    }

    fn client(&self) -> &reqwest::Client {
        self.client.get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(concat!("komo/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("HTTP 客户端：TLS provider 由 main 安装（§13.4）")
        })
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn post(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        use futures_util::StreamExt;

        let mut builder = self
            .client()
            .post(&request.url)
            .timeout(request.timeout)
            .json(&request.body);
        if let Some(key) = &request.api_key {
            builder = builder.bearer_auth(key);
        }
        let response = builder.send().await.map_err(map_reqwest)?;
        let status = response.status().as_u16();
        let body = response
            .bytes_stream()
            .map(|chunk| chunk.map(|bytes| bytes.to_vec()).map_err(map_reqwest))
            .boxed();
        Ok(HttpResponse { status, body })
    }
}

fn map_reqwest(error: reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Failed(error.to_string())
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! 脚本化的传输：适配器的端到端测试拿它当"服务端"。

    use std::sync::{Arc, Mutex};

    use super::*;

    /// 一次脚本化的回应。
    #[derive(Debug, Clone)]
    pub struct Reply {
        pub status: u16,
        /// 按到达顺序切好的字节段——半帧、跨帧的拼接都靠它模拟。
        pub chunks: Vec<Vec<u8>>,
    }

    impl Reply {
        /// 一个正常的 SSE 回应。`terminate` 为假时**没有 `[DONE]`**。
        pub fn sse(events: &[&str], terminate: bool) -> Self {
            let mut chunks: Vec<Vec<u8>> = events
                .iter()
                .map(|event| format!("data: {event}\n\n").into_bytes())
                .collect();
            if terminate {
                chunks.push(b"data: [DONE]\n\n".to_vec());
            }
            Reply {
                status: 200,
                chunks,
            }
        }

        pub fn json(status: u16, body: &str) -> Self {
            Reply {
                status,
                chunks: vec![body.as_bytes().to_vec()],
            }
        }
    }

    /// 记下收到的请求，按顺序吐出脚本里的回应。
    #[derive(Debug, Clone, Default)]
    pub struct ScriptedTransport {
        state: Arc<Mutex<State>>,
    }

    #[derive(Debug, Default)]
    struct State {
        replies: Vec<Reply>,
        seen: Vec<HttpRequest>,
    }

    impl ScriptedTransport {
        pub fn new(replies: Vec<Reply>) -> Self {
            ScriptedTransport {
                state: Arc::new(Mutex::new(State {
                    replies,
                    seen: Vec::new(),
                })),
            }
        }

        /// 收到过的请求——断言请求体的形状用它。
        pub fn requests(&self) -> Vec<HttpRequest> {
            self.state.lock().expect("脚本传输").seen.clone()
        }

        pub fn bodies(&self) -> Vec<Value> {
            self.requests().into_iter().map(|r| r.body).collect()
        }
    }

    #[async_trait]
    impl HttpTransport for ScriptedTransport {
        async fn post(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            use futures_util::StreamExt;
            use futures_util::stream;

            let mut state = self.state.lock().expect("脚本传输");
            let index = state.seen.len();
            state.seen.push(request);
            let reply = state.replies.get(index).cloned().unwrap_or(Reply {
                status: 500,
                chunks: vec!["脚本演完了".as_bytes().to_vec()],
            });
            Ok(HttpResponse {
                status: reply.status,
                body: stream::iter(reply.chunks.into_iter().map(Ok)).boxed(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Reply, ScriptedTransport};
    use super::*;

    #[test]
    fn a_request_never_prints_its_credential() {
        let request = HttpRequest::new("http://x/v1", serde_json::json!({}))
            .with_key(Some("sk-secret".into()));
        assert!(!format!("{request:?}").contains("sk-secret"));
    }

    #[tokio::test]
    async fn the_scripted_transport_replays_in_order_and_records_bodies() {
        let transport =
            ScriptedTransport::new(vec![Reply::json(200, "one"), Reply::json(400, "two")]);
        let first = transport
            .post(HttpRequest::new("u", serde_json::json!({"n": 1})))
            .await
            .unwrap();
        assert_eq!(first.text().await.unwrap(), "one");

        let second = transport
            .post(HttpRequest::new("u", serde_json::json!({"n": 2})))
            .await
            .unwrap();
        assert_eq!(second.status, 400);

        assert_eq!(transport.bodies().len(), 2);
        assert_eq!(transport.bodies()[1]["n"], serde_json::json!(2));
    }
}
