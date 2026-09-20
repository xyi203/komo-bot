//! 配置热重载：三个入口，一个函数（§3）。
//!
//! 「文件 mtime 变化（每秒轮询一次，不引入 inotify 依赖）、`komo config reload`、
//! `SIGHUP`」——三条路都落到 [`reload`]，流程固定：
//!
//! 1. 重新解析成**完整的**新快照并校验；任何错误 → 旧快照原样保留，错误写日志并投递到
//!    home chat，重载命令返回该错误。**校验不过的配置永远不会被装上。**
//! 2. 校验通过 → 原子替换进程内唯一的 `Arc<ConfigSnapshot>`（`ConfigHolder::reload`）。
//! 3. 逐段比对，只重建变化了的东西：某个 `[channels.*]` 变了就只重启那个渠道；模型 /
//!    凭证变了就换掉 `LlmClient` 实例。
//! 4. 只在启动时生效的那一组键：照常完成其余部分，然后**明确报告**它们需要
//!    `komo gateway restart`。

use std::sync::Arc;

use komo_kernel::protocol::config::KeyPath;
use komo_kernel::protocol::http::ConfigReloadResponse;
use komo_kernel::types::chat::{ChannelPlatform, Outbound};
use komo_runtime::config::ConfigError;

use crate::http::error::ApiFailure;
use crate::service::state::GatewayState;

/// 每秒看一眼三个文件的 mtime。
pub const POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// 走一遍 §3 的四步。
pub async fn reload(state: &Arc<GatewayState>) -> Result<ConfigReloadResponse, ApiFailure> {
    match state.config.reload() {
        Ok(report) => {
            tracing::info!(
                changed = report.changed.len(),
                start_only = report.start_only.len(),
                "配置已重载"
            );
            apply(state, &report.changed).await;
            announce_recovered(state).await;
            announce_start_only(state, &report.start_only).await;
            Ok(ConfigReloadResponse {
                changed: report.changed,
                start_only: report.start_only,
                warnings: report.warnings,
            })
        }
        Err(error) => {
            // 「旧快照原样保留，错误写日志并投递到 home chat」。编辑器每次自动保存都会
            // 触发一次重载，所以**同一条错误只投一次**。
            tracing::error!(%error, "配置校验不过，继续用旧的那一份");
            let text = format!("配置没装上，还在用上一份：{error}");
            let repeated = {
                let mut last = state.reload_notice.lock().expect("锁没中毒");
                let repeated = last.as_deref() == Some(text.as_str());
                *last = Some(text.clone());
                repeated
            };
            if !repeated {
                let _ = state.notifier.deliver_home(Outbound::Text { text }).await;
            }
            Err(config_failure(error))
        }
    }
}

/// 上一次没装上、这一次装上了：说一声，不然操作者只看到过错误。
async fn announce_recovered(state: &Arc<GatewayState>) {
    let had_error = state
        .reload_notice
        .lock()
        .expect("锁没中毒")
        .take()
        .is_some();
    if had_error {
        let _ = state
            .notifier
            .deliver_home(Outbound::Text {
                text: "配置已装上。".into(),
            })
            .await;
    }
}

/// 第 3 步：按变化的键前缀重建。
async fn apply(state: &Arc<GatewayState>, changed: &[KeyPath]) {
    if changed.iter().any(|key| {
        let key = key.as_str();
        key.starts_with("model_catalog.")
            || key.starts_with("model.")
            || key.starts_with("memory.model")
            || key.starts_with("credentials.")
    }) {
        state.rebuild_llm();
    }
    // 判决用的规则表：executor 手里与这里**是同一份**（`GatewayState::policy`），换上
    // 就走（§3：Policy 每次决策读规则，不缓存）。线上踩到过它的缺席：`policy.toml`
    // 换成了 `mode = "auto"`，快照确实变了、日志也说重载成功，但每一次调用仍然按旧表
    // 问人——因为 executor 那份 engine 是装配时造的，没人动它。
    if changed.iter().any(|key| key.as_str().starts_with("policy")) {
        state.policy.install(state.snapshot().policy.clone());
    }
    // §5.6 的目录行是启动快照，重载是唯一会动它的时刻：`paths.skill_dirs` 改了、
    // 或者人刚 `komo skills disable` 过，新的一段系统提示就该按新的来。
    state.refresh_skills_prompt();
    for platform in [
        ChannelPlatform::Feishu,
        ChannelPlatform::Telegram,
        ChannelPlatform::Wechat,
    ] {
        let prefix = format!("channels.{platform}.");
        if changed.iter().any(|key| key.as_str().starts_with(&prefix)) {
            state.restart_channel(platform).await;
        }
    }
}

/// 第 4 步：**不静默忽略，也不假装已生效**。
async fn announce_start_only(state: &Arc<GatewayState>, start_only: &[KeyPath]) {
    if start_only.is_empty() {
        return;
    }
    let text = format!(
        "以下键需要 `komo gateway restart` 才生效：{}",
        start_only
            .iter()
            .map(KeyPath::as_str)
            .collect::<Vec<_>>()
            .join("、")
    );
    tracing::warn!(%text, "重载完成，但有键只在启动时生效");
    let _ = state.notifier.deliver_home(Outbound::Text { text }).await;
}

/// 校验失败 → 422 `ConfigInvalid`，`keys` 带定位。
fn config_failure(error: ConfigError) -> ApiFailure {
    let keys = match &error {
        ConfigError::Invalid { issues } => issues.iter().map(|issue| issue.key.clone()).collect(),
        _ => Vec::new(),
    };
    ApiFailure::config_invalid(error.to_string(), keys)
}

/// 每秒一次的 mtime 轮询。**这是 §3 三个入口里的第一个。**
pub async fn watch(state: Arc<GatewayState>, shutdown: komo_kernel::traits::Shutdown) {
    loop {
        tokio::time::sleep(POLL).await;
        if shutdown.is_cancelled() {
            return;
        }
        if !state.config.changed_since() {
            continue;
        }
        tracing::info!("配置文件变了，重新解析");
        let _ = reload(&state).await;
    }
}

/// `SIGHUP`——三个入口里的第二个（第三个是 `POST /v1/config/reload`）。
#[cfg(unix)]
pub async fn on_sighup(state: Arc<GatewayState>, shutdown: komo_kernel::traits::Shutdown) {
    let mut hup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        Ok(hup) => hup,
        Err(error) => {
            tracing::warn!(%error, "装不上 SIGHUP 处理器，热重载只剩轮询与命令两个入口");
            return;
        }
    };
    loop {
        hup.recv().await;
        if shutdown.is_cancelled() {
            return;
        }
        tracing::info!("收到 SIGHUP，重载配置");
        let _ = reload(&state).await;
    }
}

#[cfg(not(unix))]
pub async fn on_sighup(_state: Arc<GatewayState>, _shutdown: komo_kernel::traits::Shutdown) {}
