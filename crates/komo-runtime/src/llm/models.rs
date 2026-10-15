//! 向 provider 的 `/models` 端点问一个模型的上下文窗口（配置里没写 `context_window` 时用）。
//!
//! 只是元数据查询：provider 不说就是 `Ok(None)`，由调用方当"未知"处理，这里不猜。

use std::time::Duration;

use komo_kernel::types::model::ModelConfig;
use serde_json::Value;

use super::anthropic::{ANTHROPIC_VERSION, ANTHROPIC_VERSION_HEADER};
use super::responses::Credential;
use super::transport::HttpRequest;
use super::{
    ANTHROPIC_MESSAGES, CHAT_COMPLETIONS, CHATGPT_AUTH, LEGACY_OPENAI_RESPONSES, LlmFactory,
    RESPONSES,
};

/// 探测上限：这是一次元数据调用，不该占住启动或热重载。
const PROBE_TIMEOUT_CAP_SECS: u64 = 10;
const BODY_EXCERPT_CHARS: usize = 200;

/// 探测失败。文案只含状态码、正文摘要与配置名，不含凭证。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ProbeError(String);

/// 返回 `config.model` 在 provider 那里登记的输入窗口：先 `max_input_tokens`（不含输出），
/// 再 `context_window`、`context_length`。
pub async fn probe_context_window(
    factory: &LlmFactory,
    config: &ModelConfig,
) -> Result<Option<u64>, ProbeError> {
    let request = build_request(factory, config).await?;
    let response = factory
        .transport
        .get(request)
        .await
        .map_err(|e| ProbeError(format!("请求 /models 失败：{e}")))?;
    let status = response.status;
    let text = response
        .text()
        .await
        .map_err(|e| ProbeError(format!("读取 /models 响应失败：{e}")))?;
    if !(200..300).contains(&status) {
        let excerpt: String = text.chars().take(BODY_EXCERPT_CHARS).collect();
        return Err(ProbeError(format!("/models 返回 {status}：{excerpt}")));
    }
    let body: Value = serde_json::from_str(&text)
        .map_err(|e| ProbeError(format!("/models 响应不是 JSON：{e}")))?;
    Ok(window_of(&body, &config.model))
}

async fn build_request(
    factory: &LlmFactory,
    config: &ModelConfig,
) -> Result<HttpRequest, ProbeError> {
    let base = config.base_url.trim_end_matches('/');
    let timeout = Duration::from_secs(config.timeout_secs.clamp(1, PROBE_TIMEOUT_CAP_SECS));
    let key_error = |e: komo_kernel::types::turn::LlmError| ProbeError(e.to_string());
    match config.provider.as_str() {
        RESPONSES | LEGACY_OPENAI_RESPONSES => {
            let credential = factory
                .credential_for(config)
                .map_err(|e| ProbeError(e.to_string()))?;
            let chatgpt = matches!(credential, Credential::ChatGpt(_));
            let url = if chatgpt {
                format!("{base}/models?client_version={}", env!("CARGO_PKG_VERSION"))
            } else {
                format!("{base}/models")
            };
            let mut request = HttpRequest::get(url).with_timeout(timeout);
            // 没配 key 的本地网关照样可能放行 /models，所以缺 key 不在这里拦。
            if !matches!(credential, Credential::Env(None)) {
                let (key, headers) = credential
                    .resolve(&config.api_key_env)
                    .await
                    .map_err(key_error)?;
                request = request.with_key(Some(key));
                for (name, value) in headers {
                    request = request.with_header(name, value);
                }
            }
            Ok(request)
        }
        CHAT_COMPLETIONS => {
            reject_auth(config)?;
            let key = factory.secrets.get(&config.api_key_env).map(str::to_string);
            Ok(HttpRequest::get(format!("{base}/models"))
                .with_key(key)
                .with_timeout(timeout))
        }
        ANTHROPIC_MESSAGES => {
            reject_auth(config)?;
            let mut request = HttpRequest::get(format!("{base}/models"))
                .with_header(ANTHROPIC_VERSION_HEADER, ANTHROPIC_VERSION)
                .with_timeout(timeout);
            if let Some(key) = factory.secrets.get(&config.api_key_env) {
                request = request.with_header("x-api-key", key);
            }
            Ok(request)
        }
        other => Err(ProbeError(format!("不认识 api_backend `{other}`"))),
    }
}

fn reject_auth(config: &ModelConfig) -> Result<(), ProbeError> {
    match &config.auth {
        None => Ok(()),
        Some(auth) if auth == CHATGPT_AUTH => Err(ProbeError(format!(
            "auth = \"{CHATGPT_AUTH}\" 只支持 `{RESPONSES}`"
        ))),
        Some(auth) => Err(ProbeError(format!("不认识的 auth `{auth}`"))),
    }
}

fn window_of(body: &Value, model: &str) -> Option<u64> {
    let list = body
        .get("data")
        .or_else(|| body.get("models"))?
        .as_array()?;
    let entry = list.iter().find(|entry| {
        ["id", "slug"]
            .iter()
            .any(|k| entry.get(k).and_then(Value::as_str) == Some(model))
    })?;
    ["max_input_tokens", "context_window", "context_length"]
        .iter()
        .find_map(|k| entry.get(k).and_then(Value::as_u64).filter(|n| *n > 0))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::transport::testing::{Reply, ScriptedTransport};
    use super::*;
    use crate::config::{EffortCapabilities, Secrets};
    use crate::llm::codex_auth::{self, CodexAuth};

    fn config(provider: &str, base_url: &str, model: &str) -> ModelConfig {
        ModelConfig {
            provider: provider.into(),
            base_url: base_url.into(),
            model: model.into(),
            api_key_env: "KOMO_KEY".into(),
            auth: None,
            effort: None,
            efforts: None,
            timeout_secs: 120,
            context_window: None,
        }
    }

    fn factory(transport: &ScriptedTransport) -> LlmFactory {
        LlmFactory::new(
            Arc::new(Secrets::from_pairs([("KOMO_KEY", "sk-secret-key")])),
            EffortCapabilities::builtin(),
        )
        .with_transport(Arc::new(transport.clone()))
    }

    const MAGPIE: &str = r#"{"data":[{"id":"claude/claude-opus-5-5","context_window":1000000,"max_input_tokens":900000,"max_output_tokens":128000,"context_length":1000000},{"id":"other","context_window":1}]}"#;
    const DEEPSEEK: &str = r#"{"object":"list","data":[{"id":"deepseek-flash","context_window":1048576,"max_output_tokens":393216}]}"#;
    const RELAY: &str = r#"{"data":[{"id":"gpt-5.6-sol","object":"model","owned_by":"anthropic","description":"x"}]}"#;
    const CODEX: &str = r#"{"models":[{"slug":"gpt-6-sol","context_window":272000}]}"#;

    #[tokio::test]
    async fn magpie_prefers_max_input_tokens_over_context_window() {
        let transport = ScriptedTransport::new(vec![Reply::json(200, MAGPIE)]);
        let cfg = config(
            RESPONSES,
            "http://127.0.0.1:3425/v1/",
            "claude/claude-opus-5-5",
        );
        let window = probe_context_window(&factory(&transport), &cfg)
            .await
            .unwrap();
        assert_eq!(window, Some(900_000));
        let request = &transport.requests()[0];
        assert_eq!(request.url, "http://127.0.0.1:3425/v1/models");
        assert_eq!(request.api_key.as_deref(), Some("sk-secret-key"));
        assert!(request.timeout <= Duration::from_secs(PROBE_TIMEOUT_CAP_SECS));
    }

    #[tokio::test]
    async fn deepseek_falls_back_to_context_window() {
        let transport = ScriptedTransport::new(vec![Reply::json(200, DEEPSEEK)]);
        let cfg = config(
            CHAT_COMPLETIONS,
            "https://api.deepseek.com",
            "deepseek-flash",
        );
        let window = probe_context_window(&factory(&transport), &cfg)
            .await
            .unwrap();
        assert_eq!(window, Some(1_048_576));
    }

    #[tokio::test]
    async fn a_relay_without_window_fields_yields_none() {
        let transport = ScriptedTransport::new(vec![Reply::json(200, RELAY)]);
        let cfg = config(
            CHAT_COMPLETIONS,
            "https://relay.example.com/v1",
            "gpt-5.6-sol",
        );
        assert_eq!(
            probe_context_window(&factory(&transport), &cfg)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn an_unlisted_model_id_yields_none() {
        let transport = ScriptedTransport::new(vec![Reply::json(200, MAGPIE)]);
        let cfg = config(RESPONSES, "http://x/v1", "claude/claude-opus");
        assert_eq!(
            probe_context_window(&factory(&transport), &cfg)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn chatgpt_auth_reads_slug_shape_with_account_header_and_client_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex").join("auth.json");
        let expires = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        codex_auth::save(
            &path,
            &CodexAuth {
                access_token: "tok-chatgpt".into(),
                refresh_token: "refresh".into(),
                id_token: None,
                account_id: "acct_1".into(),
                email: None,
                plan: None,
                expires_at: expires,
            },
        )
        .unwrap();
        let transport = ScriptedTransport::new(vec![Reply::json(200, CODEX)]);
        let mut cfg = config(
            RESPONSES,
            "https://chatgpt.com/backend-api/codex",
            "gpt-6-sol",
        );
        cfg.auth = Some(CHATGPT_AUTH.into());
        let factory = factory(&transport).with_chatgpt_credentials_path(path);
        assert_eq!(
            probe_context_window(&factory, &cfg).await.unwrap(),
            Some(272_000)
        );
        let request = &transport.requests()[0];
        assert_eq!(
            request.url,
            format!(
                "https://chatgpt.com/backend-api/codex/models?client_version={}",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(request.api_key.as_deref(), Some("tok-chatgpt"));
        assert!(
            request
                .headers
                .contains(&("ChatGPT-Account-ID".into(), "acct_1".into()))
        );
    }

    #[tokio::test]
    async fn anthropic_request_carries_x_api_key_and_version() {
        let body = r#"{"data":[{"id":"claude-x","max_input_tokens":200000}]}"#;
        let transport = ScriptedTransport::new(vec![Reply::json(200, body)]);
        let cfg = config(
            ANTHROPIC_MESSAGES,
            "https://api.anthropic.com/v1",
            "claude-x",
        );
        assert_eq!(
            probe_context_window(&factory(&transport), &cfg)
                .await
                .unwrap(),
            Some(200_000)
        );
        let request = &transport.requests()[0];
        assert_eq!(request.url, "https://api.anthropic.com/v1/models");
        assert!(request.api_key.is_none());
        assert!(
            request
                .headers
                .contains(&("x-api-key".into(), "sk-secret-key".into()))
        );
        assert!(
            request
                .headers
                .contains(&("anthropic-version".into(), ANTHROPIC_VERSION.into()))
        );
    }

    #[tokio::test]
    async fn a_401_is_an_error_that_does_not_leak_the_key() {
        let transport =
            ScriptedTransport::new(vec![Reply::json(401, r#"{"error":"bad credentials"}"#)]);
        let cfg = config(RESPONSES, "http://x/v1", "m");
        let error = probe_context_window(&factory(&transport), &cfg)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("401") && error.contains("bad credentials"),
            "{error}"
        );
        assert!(!error.contains("sk-secret-key"), "{error}");
    }
}
