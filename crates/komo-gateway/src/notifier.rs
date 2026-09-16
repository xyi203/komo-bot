//! 主动投递的目标怎么挑（§11.4）。
//!
//! 三条规则写在这里，各自只有一处实现：
//!
//! - **home chat 只看配置**：每个 enabled 渠道的 `home_chat`，顺序是飞书 > Telegram >
//!   WeChat（[`ChannelsConfig::home_chats`]）。没有运行时覆盖，没有 `/sethome`。
//! - **一个都没配时返回错误给调用方，不静默丢弃**（[`DeliverError::NoTarget`]）。
//! - **审批请求投到来源会话，加上 home chat（若不同）**：两处都能回答，第二个答复得到
//!   "已决定"——那是审批本身的幂等，不是这里的事（§11.3）。
//!
//! 名单**每次投递现读**当前快照（§3 第 2 步）：改了 home_chat 保存后，下一条投递就按
//! 新的走。

use std::sync::Arc;

use async_trait::async_trait;
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::traits::{DeliverError, Notifier};
use komo_kernel::types::chat::{
    ApprovalPresentation, ChannelPeer, ChannelPlatform, Delivery, DeliveryTarget, Outbound,
};
use komo_runtime::config::ConfigHolder;

use crate::deliveries::DeliveryLog;

/// `Notifier` 的生产实现。
pub struct HomeNotifier {
    log: Arc<DeliveryLog>,
    config: Arc<ConfigHolder>,
}

impl std::fmt::Debug for HomeNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HomeNotifier").finish_non_exhaustive()
    }
}

impl HomeNotifier {
    pub fn new(log: Arc<DeliveryLog>, config: Arc<ConfigHolder>) -> Self {
        HomeNotifier { log, config }
    }

    pub fn log(&self) -> &Arc<DeliveryLog> {
        &self.log
    }

    /// 当前快照里的 home chat 们（按 §11.4 的顺序）。
    pub fn home_targets(&self) -> Vec<DeliveryTarget> {
        home_targets(&self.config.current())
    }

    /// 投到 home chat。一个都没配 → [`DeliverError::NoTarget`]。
    pub async fn deliver_home(&self, msg: Outbound) -> Result<Vec<Delivery>, DeliverError> {
        let targets = self.home_targets();
        if targets.is_empty() {
            return Err(DeliverError::NoTarget(
                "没有配置任何 home_chat：改 config.toml 的 [channels.*].home_chat".into(),
            ));
        }
        let mut out = Vec::with_capacity(targets.len());
        for target in &targets {
            out.push(self.log.deliver(target, msg.clone()).await?);
        }
        Ok(out)
    }

    /// 审批请求：**来源会话 + home chat**（若不同）。
    ///
    /// 来源是 Cron 或已断开的 TUI 时只有 home chat；两处都没有就报错，不静默丢弃。
    pub async fn deliver_approval(
        &self,
        origin: Option<&ChannelPeer>,
        presentation: ApprovalPresentation,
    ) -> Result<Vec<Delivery>, DeliverError> {
        let msg = Outbound::ApprovalRequest(Box::new(presentation));
        let mut out = Vec::new();
        if let Some(peer) = origin {
            out.push(
                self.log
                    .deliver(&DeliveryTarget::to_peer(peer.clone()), msg.clone())
                    .await?,
            );
        }
        for target in self.home_targets() {
            if origin.is_some_and(|peer| peer == &target.peer) {
                continue; // 同一个会话不发两遍。
            }
            out.push(self.log.deliver(&target, msg.clone()).await?);
        }
        if out.is_empty() {
            return Err(DeliverError::NoTarget(
                "这条审批既没有来源会话也没有 home_chat，没人能回答它".into(),
            ));
        }
        Ok(out)
    }

    /// 冲刷某个会话（或全部）还没送到的投递。
    pub async fn flush(&self, peer: Option<&ChannelPeer>) -> usize {
        self.log.flush(peer).await
    }

    /// 冲刷**一个平台**名下还没送到的投递。渠道刚起来时用它——在发送口登记之前冲刷是
    /// 一个空动作（W5 验收 BUG(3)）。
    pub async fn flush_platform(&self, platform: ChannelPlatform) -> usize {
        self.log.flush_platform(platform).await
    }
}

#[async_trait]
impl Notifier for HomeNotifier {
    async fn deliver(
        &self,
        target: &DeliveryTarget,
        msg: Outbound,
    ) -> Result<Delivery, DeliverError> {
        self.log.deliver(target, msg).await
    }
}

/// 当前快照里的 home chat 们。纯函数，测试直接喂一份快照。
pub fn home_targets(snapshot: &ConfigSnapshot) -> Vec<DeliveryTarget> {
    snapshot
        .channels
        .home_chats()
        .into_iter()
        .map(|(platform, chat)| {
            DeliveryTarget::home(ChannelPeer::new(platform, chat.as_str().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::protocol::config::{ChannelConfig, ChannelsConfig};
    use komo_kernel::types::chat::{ChannelPlatform, PeerId};

    fn snapshot_with(channels: ChannelsConfig) -> ConfigSnapshot {
        let mut snapshot = crate::service::test_support::sample_snapshot();
        snapshot.channels = channels;
        snapshot
    }

    fn channel(chat: &str) -> ChannelConfig {
        ChannelConfig {
            enabled: true,
            allow_from: vec![PeerId::new("ou_op")],
            home_chat: Some(PeerId::new(chat)),
            groups: vec![],
        }
    }

    #[test]
    fn home_chats_come_in_feishu_telegram_wechat_order() {
        let snapshot = snapshot_with(ChannelsConfig {
            feishu: channel("oc_x"),
            telegram: channel("123"),
            wechat: channel("wxid_x"),
        });
        let platforms: Vec<ChannelPlatform> = home_targets(&snapshot)
            .into_iter()
            .map(|t| t.peer.platform)
            .collect();
        assert_eq!(
            platforms,
            vec![
                ChannelPlatform::Feishu,
                ChannelPlatform::Telegram,
                ChannelPlatform::Wechat
            ]
        );
        assert!(home_targets(&snapshot).iter().all(|t| t.is_home));
    }

    #[test]
    fn no_home_chat_configured_is_an_empty_target_list() {
        assert!(home_targets(&snapshot_with(ChannelsConfig::default())).is_empty());
    }
}
