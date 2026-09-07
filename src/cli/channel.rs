//! Channel inventory and read-only connectivity checks.
//!
//! The public interface is intentionally small: `list` renders the resolved
//! configuration plus the gateway's mounted-channel snapshot; `probe` checks
//! one provider without sending a message.

use std::time::Duration;

use serde::Serialize;
use serde_json::json;

use crate::infra::gateway_client::GatewayClient;
use komo_config::{ApiConfig, ChannelState, ConfigSnapshot};

#[derive(Debug, Clone, Serialize)]
pub struct ChannelSummary {
    pub name: &'static str,
    pub kind: &'static str,
    pub config: String,
    pub gateway: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// List every built-in channel. This remains useful with no gateway running:
/// configuration is always rendered, while the mounted-channel column becomes
/// `unavailable` rather than failing the command.
pub async fn list(config: &ConfigSnapshot, json: bool) -> anyhow::Result<()> {
    let gateway_channels = match GatewayClient::try_connect().await {
        Some(client) => client.status().await.ok().map(|status| status.channels),
        None => None,
    };
    let rows = summaries(config, gateway_channels.as_deref());
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!("CHANNEL         KIND      CONFIG             GATEWAY");
    for row in rows {
        println!(
            "{:<15} {:<9} {:<18} {}",
            row.name, row.kind, row.config, row.gateway
        );
        if let Some(detail) = row.detail {
            println!("  {detail}");
        }
    }
    Ok(())
}

pub async fn probe(_config: &ConfigSnapshot, name: &str) -> anyhow::Result<()> {
    let name = normalize_name(name)?;
    match name.as_str() {
        "feishu" => probe_feishu(_config).await,
        "telegram" => probe_telegram(_config).await,
        "wechat" => probe_wechat(_config),
        "api" => probe_api().await,
        _ => unreachable!("normalize_name accepts only built-in channels"),
    }
}

fn normalize_name(name: &str) -> anyhow::Result<String> {
    let value = name.trim().to_ascii_lowercase();
    match value.as_str() {
        "feishu" | "telegram" | "wechat" | "api" => Ok(value),
        _ => anyhow::bail!("unknown channel `{name}` (expected feishu | telegram | wechat | api)"),
    }
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?)
}

fn require_ready<'a, T>(name: &str, state: &'a ChannelState<T>) -> anyhow::Result<&'a T> {
    match state {
        ChannelState::Ready(config) => Ok(config),
        ChannelState::Disabled => {
            anyhow::bail!("{name} is disabled; enable [channels.{name}] in config.toml first")
        }
        ChannelState::Misconfigured(error) => anyhow::bail!("{name} is misconfigured: {error}"),
    }
}

/// Verify feishu credentials against the live API. Shared with `komo doctor`,
/// which reports liveness per enabled channel — a config line saying "enabled"
/// while the credential 401s on every poll is doctor lying by omission.
pub(crate) async fn check_feishu_live(config: &ConfigSnapshot) -> anyhow::Result<()> {
    let channel = require_ready("feishu", &config.runtime.feishu)?;
    let response = http_client()?
        .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
        .json(&json!({ "app_id": channel.app_id, "app_secret": channel.app_secret }))
        .send()
        .await
        .map_err(|_| {
            anyhow::anyhow!("Feishu authentication request failed; check network connectivity")
        })?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    if !status.is_success() || body.get("code").and_then(|value| value.as_i64()) != Some(0) {
        anyhow::bail!(
            "Feishu authentication failed ({status}): {}",
            body.get("msg")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown error")
        );
    }
    Ok(())
}

async fn probe_feishu(config: &ConfigSnapshot) -> anyhow::Result<()> {
    check_feishu_live(config).await?;
    println!("✓ feishu credentials accepted");
    Ok(())
}

/// Verify telegram credentials against the live API; the bot's username on
/// success. Shared with `komo doctor` — see [`check_feishu_live`].
pub(crate) async fn check_telegram_live(config: &ConfigSnapshot) -> anyhow::Result<String> {
    let channel = require_ready("telegram", &config.runtime.telegram)?;
    let response = http_client()?
        .get(format!(
            "https://api.telegram.org/bot{}/getMe",
            channel.bot_token
        ))
        .send()
        .await
        .map_err(|_| {
            anyhow::anyhow!("Telegram authentication request failed; check network connectivity")
        })?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    if !status.is_success() || body.get("ok").and_then(|value| value.as_bool()) != Some(true) {
        anyhow::bail!(
            "Telegram authentication failed ({status}): {}",
            body.get("description")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown error")
        );
    }
    Ok(body["result"]["username"]
        .as_str()
        .unwrap_or("bot")
        .to_string())
}

async fn probe_telegram(config: &ConfigSnapshot) -> anyhow::Result<()> {
    let identity = check_telegram_live(config).await?;
    println!("✓ telegram credentials accepted (@{identity})");
    Ok(())
}

fn probe_wechat(config: &ConfigSnapshot) -> anyhow::Result<()> {
    require_ready("wechat", &config.runtime.wechat)?;
    let path = komo_config::wechat_cred_path();
    if !path.exists() {
        anyhow::bail!("WeChat is enabled but not logged in; run `komo channel wechat login`");
    }
    let body = std::fs::read_to_string(&path)?;
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
        anyhow::anyhow!("WeChat credential file is not valid JSON; run `komo channel wechat login`")
    })?;
    if !value.is_object() {
        anyhow::bail!(
            "WeChat credential file has an unexpected format; run `komo channel wechat login`"
        );
    }
    anyhow::bail!(
        "WeChat credential file is valid ({}) but live iLink probing is unsupported without opening a bot session; no message was sent",
        path.display()
    );
}

async fn probe_api() -> anyhow::Result<()> {
    let client = GatewayClient::try_connect()
        .await
        .ok_or_else(|| anyhow::anyhow!("gateway is not reachable"))?;
    let status = client.status().await?;
    println!(
        "✓ api gateway reachable (channels: {})",
        status.channels.join(", ")
    );
    Ok(())
}

pub fn summaries(
    config: &ConfigSnapshot,
    gateway_channels: Option<&[String]>,
) -> Vec<ChannelSummary> {
    let rt = &config.runtime;
    vec![
        summary("feishu", "chat", state_status(&rt.feishu), gateway_channels),
        summary(
            "telegram",
            "chat",
            state_status(&rt.telegram),
            gateway_channels,
        ),
        summary(
            "wechat",
            "chat",
            wechat_status(&rt.wechat),
            gateway_channels,
        ),
        summary("api", "control", api_status(&rt.api), gateway_channels),
    ]
}

fn summary(
    name: &'static str,
    kind: &'static str,
    (config, detail): (String, Option<String>),
    gateway_channels: Option<&[String]>,
) -> ChannelSummary {
    let gateway = match gateway_channels {
        Some(channels) if channels.iter().any(|channel| channel == name) => "loaded".to_string(),
        Some(_) if config != "disabled" => "not loaded (restart needed)".to_string(),
        Some(_) => "not loaded".to_string(),
        None => "unavailable".to_string(),
    };
    ChannelSummary {
        name,
        kind,
        config,
        gateway,
        detail,
    }
}

fn state_status<T>(state: &ChannelState<T>) -> (String, Option<String>) {
    match state {
        ChannelState::Disabled => ("disabled".to_string(), None),
        ChannelState::Ready(_) => ("ready".to_string(), None),
        ChannelState::Misconfigured(error) => ("misconfigured".to_string(), Some(error.clone())),
    }
}

fn wechat_status(state: &ChannelState<komo_config::WeChatConfig>) -> (String, Option<String>) {
    match state {
        ChannelState::Ready(_) if !komo_config::wechat_cred_path().exists() => (
            "login required".to_string(),
            Some("run `komo channel wechat login`".to_string()),
        ),
        _ => state_status(state),
    }
}

fn api_status(state: &ChannelState<ApiConfig>) -> (String, Option<String>) {
    match state {
        ChannelState::Ready(config) if config.port == 0 => (
            "loopback".to_string(),
            Some("local CLI control channel".to_string()),
        ),
        ChannelState::Ready(config) => (
            "external".to_string(),
            Some(format!("{}:{}", config.bind, config.port)),
        ),
        _ => state_status(state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_mark_missing_wechat_login_and_stale_gateway() {
        let wechat = summary(
            "wechat",
            "chat",
            ("login required".to_string(), None),
            Some(&["telegram".to_string()]),
        );
        assert_eq!(wechat.config, "login required");

        let telegram = summary(
            "telegram",
            "chat",
            ("ready".to_string(), None),
            Some(&["feishu".to_string()]),
        );
        assert_eq!(telegram.gateway, "not loaded (restart needed)");
    }
}
