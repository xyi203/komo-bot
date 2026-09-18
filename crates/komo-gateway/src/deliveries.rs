//! `deliveries`：先写记录再发送，补发按 [`DeliveryId`] 幂等（§11.4）。
//!
//! 「§8.8：应为待发送结果持久保存投递记录，**不能为补发一条结果消息重跑任务**。」
//!
//! 这个文件只管"记录 → 发送 → 结算"这一条顺序与补发；**目标怎么挑**（来源会话 + home
//! chat、飞书 > Telegram > WeChat）在 [`crate::notifier`] 里。

use std::sync::Arc;

use komo_kernel::traits::{Clock, DeliverError};
use komo_kernel::types::chat::{
    ChannelPeer, ChannelPlatform, Delivery, DeliveryState, DeliveryTarget, Outbound,
};
use komo_kernel::types::ids::DeliveryId;
use komo_store::{DeliveryRecord, TursoDeliveryRepo};

use crate::channels::{ChannelRegistry, SendOutcome};

/// 投递账：写行、发送、结算、补发。
pub struct DeliveryLog {
    repo: Arc<TursoDeliveryRepo>,
    channels: Arc<ChannelRegistry>,
    clock: Arc<dyn Clock>,
    /// 补发是**排他的**：同一时刻只跑一趟。
    ///
    /// 补发有两个入口——渠道起来时按平台冲刷一次（§11.4）、以及启动时（与微信入站前）
    /// 的整体冲刷。两趟并发地在同一批 pending 行上跑，一行就会被送两次：`send_recorded`
    /// 按行结算，但**发送本身在结算之前**，所以两个任务都会把那条消息发出去。
    flushing: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for DeliveryLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeliveryLog").finish_non_exhaustive()
    }
}

impl DeliveryLog {
    pub fn new(
        repo: Arc<TursoDeliveryRepo>,
        channels: Arc<ChannelRegistry>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        DeliveryLog {
            repo,
            channels,
            clock,
            flushing: tokio::sync::Mutex::new(()),
        }
    }

    pub fn repo(&self) -> &Arc<TursoDeliveryRepo> {
        &self.repo
    }

    /// **先写行，再发送。**
    pub async fn deliver(
        &self,
        target: &DeliveryTarget,
        msg: Outbound,
    ) -> Result<Delivery, DeliverError> {
        let now = self.clock.now();
        let id = DeliveryId::new_at(now);
        let record = self
            .repo
            .record(&id, target, &msg, now)
            .await
            .map_err(|error| DeliverError::Persist(error.to_string()))?;
        self.send_recorded(&record).await
    }

    /// 发一条**已经登记过**的投递（首次发送与补发走同一段代码）。
    pub async fn send_recorded(&self, record: &DeliveryRecord) -> Result<Delivery, DeliverError> {
        let platform = record.target.peer.platform;
        let Some(sender) = self.channels.get(platform) else {
            // 渠道此刻不在（没起来、或者被重载停掉了）：留在 pending，下一次冲刷再试。
            self.settle(
                &record.id,
                DeliveryState::Deferred,
                Some(format!("{platform} 渠道当前不可用")),
            )
            .await;
            return Ok(Delivery {
                id: record.id.clone(),
                state: DeliveryState::Deferred,
            });
        };

        match sender
            .send(&record.target.peer, record.outbound.clone())
            .await
        {
            Ok(SendOutcome::Sent) => {
                self.settle(&record.id, DeliveryState::Sent, None).await;
                Ok(Delivery {
                    id: record.id.clone(),
                    state: DeliveryState::Sent,
                })
            }
            Ok(SendOutcome::Deferred { reason }) => {
                // **按错误变体判定**，不按字符串（§11.4）：渠道自己答的 Deferred。
                self.settle(&record.id, DeliveryState::Deferred, Some(reason))
                    .await;
                Ok(Delivery {
                    id: record.id.clone(),
                    state: DeliveryState::Deferred,
                })
            }
            Err(error) => {
                self.settle(&record.id, DeliveryState::Deferred, Some(error.to_string()))
                    .await;
                Err(error)
            }
        }
    }

    async fn settle(&self, id: &DeliveryId, state: DeliveryState, error: Option<String>) {
        if let Err(problem) = self.repo.settle(id, state, error, self.clock.now()).await {
            tracing::warn!(delivery = %id, %problem, "投递状态写不回去");
        }
    }

    /// 还没送到的那些。`peer` 给出时只看那个会话（§11.1 第 4 步的冲刷）。
    pub async fn pending(
        &self,
        peer: Option<&ChannelPeer>,
    ) -> Result<Vec<DeliveryRecord>, DeliverError> {
        self.repo
            .pending(peer)
            .await
            .map_err(|error| DeliverError::Persist(error.to_string()))
    }

    /// 冲刷**一个平台**名下的 pending。渠道刚登记完发送口时走它。
    pub async fn flush_platform(&self, platform: ChannelPlatform) -> usize {
        // 先拿锁再读：拿到锁的那一刻看到的是**别人刚结算完**的那一份，于是同一行不会
        // 被两趟补发各送一次。
        let _exclusive = self.flushing.lock().await;
        let pending = match self.pending(None).await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%error, "读不出待补发的投递");
                return 0;
            }
        };
        let mut sent = 0;
        for record in pending
            .into_iter()
            .filter(|record| record.target.peer.platform == platform)
        {
            match self.send_recorded(&record).await {
                Ok(Delivery {
                    state: DeliveryState::Sent,
                    ..
                }) => sent += 1,
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(delivery = %record.id, %error, "补发失败，留在 pending");
                }
            }
        }
        sent
    }

    /// 冲刷：pending 的行逐条再发一次。返回这次送出去了几条。
    ///
    /// 启动时（补发上一次没送到的）与每条入站消息之前（微信那条路径）都调它。**一趟一
    /// 趟排队**（见 [`DeliveryLog::flushing`]）：两趟并发会在同一行上各发一次。
    pub async fn flush(&self, peer: Option<&ChannelPeer>) -> usize {
        let _exclusive = self.flushing.lock().await;
        let pending = match self.pending(peer).await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%error, "读不出待补发的投递");
                return 0;
            }
        };
        let mut sent = 0;
        for record in pending {
            match self.send_recorded(&record).await {
                Ok(Delivery {
                    state: DeliveryState::Sent,
                    ..
                }) => sent += 1,
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(delivery = %record.id, %error, "补发失败，留在 pending");
                }
            }
        }
        if sent > 0 {
            tracing::info!(sent, "补发了待投递的消息");
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::{ChannelSender, test_channel::MemChannel};
    use komo_kernel::test_support::TestClock;
    use komo_kernel::types::chat::ChannelPlatform;
    use komo_store::test_support::TempStore;

    fn target(platform: ChannelPlatform, chat: &str) -> DeliveryTarget {
        DeliveryTarget::to_peer(ChannelPeer::new(platform, chat))
    }

    async fn log(store: &TempStore, channel: &Arc<MemChannel>) -> DeliveryLog {
        let channels = Arc::new(ChannelRegistry::new());
        channels.register(Arc::clone(channel) as Arc<dyn ChannelSender>);
        DeliveryLog::new(
            Arc::new(TursoDeliveryRepo::new(store.db().clone())),
            channels,
            Arc::new(TestClock::fixed()),
        )
    }

    #[tokio::test]
    async fn a_delivery_is_recorded_before_it_is_sent_and_settled_after() {
        let store = TempStore::open().await.unwrap();
        let channel = MemChannel::new(ChannelPlatform::Telegram);
        let log = log(&store, &channel).await;

        let delivery = log
            .deliver(
                &target(ChannelPlatform::Telegram, "123"),
                Outbound::Text {
                    text: "好了".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(delivery.state, DeliveryState::Sent);
        assert_eq!(channel.sent().len(), 1);
        assert!(
            log.pending(None).await.unwrap().is_empty(),
            "送到了就不再 pending"
        );
    }

    #[tokio::test]
    async fn a_deferred_delivery_stays_pending_until_the_next_flush() {
        let store = TempStore::open().await.unwrap();
        let channel = MemChannel::new(ChannelPlatform::Wechat);
        let log = log(&store, &channel).await;
        channel.defer(true);

        let delivery = log
            .deliver(
                &target(ChannelPlatform::Wechat, "wxid_x"),
                Outbound::Text {
                    text: "要批一下".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(delivery.state, DeliveryState::Deferred);
        assert!(channel.sent().is_empty(), "推不出去就没有送出这一条");
        assert_eq!(log.pending(None).await.unwrap().len(), 1);

        // 用户发了消息，令牌有了。
        channel.defer(false);
        assert_eq!(
            log.flush(Some(&ChannelPeer::new(ChannelPlatform::Wechat, "wxid_x")))
                .await,
            1
        );
        assert_eq!(channel.sent().len(), 1);
        assert!(log.pending(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_channel_defers_rather_than_dropping_the_message() {
        let store = TempStore::open().await.unwrap();
        let channel = MemChannel::new(ChannelPlatform::Telegram);
        let log = log(&store, &channel).await;

        let delivery = log
            .deliver(
                &target(ChannelPlatform::Feishu, "oc_x"),
                Outbound::Text { text: "在".into() },
            )
            .await
            .unwrap();
        assert_eq!(delivery.state, DeliveryState::Deferred);
        assert_eq!(
            log.pending(None).await.unwrap().len(),
            1,
            "行留着，等渠道回来"
        );
    }
}
