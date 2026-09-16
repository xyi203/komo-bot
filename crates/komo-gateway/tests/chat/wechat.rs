//! 微信：真 `WeChatChannel` 对着 loopback 上的假 iLink `serve`，真 Dispatcher 在后面
//! （§11.1 的去重键、§11.4 的 `Deferred` 与冲刷）。

use std::sync::Arc;

use komo_gateway::channels::ChannelSender;
use komo_kernel::traits::Channel;
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, DeliveryState};

use crate::fake_wechat::{Behavior, FakeILink, text_wire};
use crate::harness::{
    FixedFactory, GatewayBuilder, TestGateway, config_toml, eventually, wechat_block,
};

struct Wired {
    gateway: TestGateway,
    fake: Arc<FakeILink>,
}

async fn wire(behavior: Behavior, msgs: Vec<wechatbot::types::WireMessage>) -> Wired {
    crate::harness::install_crypto();
    let fake = Arc::new(FakeILink::start_with(behavior, msgs).await);
    let channel = fake.channel();
    let sender = channel.sender() as Arc<dyn ChannelSender>;
    let factory = FixedFactory::new(
        ChannelPlatform::Wechat,
        Arc::clone(&channel) as Arc<dyn Channel>,
        sender,
    );
    let gateway = GatewayBuilder::new(&config_toml(&wechat_block("\"wxid_op\"")))
        .factory(factory)
        .start()
        .await;
    Wired { gateway, fake }
}

impl Wired {
    async fn polled_at_least(&self, n: usize) {
        let fake = Arc::clone(&self.fake);
        eventually(&format!("getupdates 被调用 {n} 次"), move || {
            fake.calls_to("/ilink/bot/getupdates").len() >= n
        })
        .await;
    }

    async fn home_session(&self) -> Option<komo_kernel::types::ids::SessionId> {
        self.gateway
            .sessions()
            .await
            .into_iter()
            .find(|record| record.origin == "home")
            .map(|record| record.session)
    }
}

/// 验证列①：同一 `client_id` 重投只产生一个 Run。
///
/// 假服务端走的是 spike §2.5 的那条路：**回了消息却回空游标**，SDK 因此原地把同一批再
/// 取一次。去重键是 `wechat:{from_user_id}:{client_id}`（§11.1）。
#[tokio::test]
async fn a_replayed_client_id_produces_one_run() {
    let wired = wire(
        Behavior::replaying(),
        vec![text_wire("wxid_op", "cid-1", "帮我看看")],
    )
    .await;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let home = loop {
        if let Some(session) = wired.home_session().await {
            break session;
        }
        assert!(std::time::Instant::now() < deadline, "等不到 home session");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };

    // 让重投再来几轮。
    wired.polled_at_least(5).await;

    let runs = wired.gateway.runs_of(&home).await;
    assert_eq!(runs.len(), 1, "重投不该开第二个 Run：{runs:?}");
}

/// 验证列②：进程起来后用户还没说过话 → 主动投递 `Deferred`，行留在 pending，
/// **且不联网补**（§11.4）。
#[tokio::test]
async fn a_delivery_before_the_user_speaks_is_deferred() {
    let wired = wire(Behavior::default(), Vec::new()).await;
    wired.polled_at_least(1).await;
    let record = wired.gateway.pending_approval().await;

    let deliveries = wired
        .gateway
        .state()
        .notifier
        .deliver_approval(None, komo_runtime::approvals::presentation(&record))
        .await
        .expect("登记得上");
    assert_eq!(deliveries.len(), 1, "只有 home chat 一个目标");
    assert_eq!(
        deliveries[0].state,
        DeliveryState::Deferred,
        "没有回复令牌就推不出去"
    );

    assert!(
        wired.fake.calls_to("/ilink/bot/sendmessage").is_empty(),
        "`Deferred` 不联网补（§11.4）"
    );
    let pending = wired.gateway.pending_deliveries().await;
    assert_eq!(pending.len(), 1, "行留在 pending：{pending:?}");
}

/// 验证列③：用户下一条消息到达时，Dispatcher **先冲刷积压的投递，再处理新消息**
/// （§11.1 第 4 步）。
#[tokio::test]
async fn the_backlog_is_flushed_before_the_new_message() {
    let wired = wire(Behavior::default(), Vec::new()).await;
    wired.polled_at_least(1).await;
    let record = wired.gateway.pending_approval().await;

    wired
        .gateway
        .state()
        .notifier
        .deliver_approval(None, komo_runtime::approvals::presentation(&record))
        .await
        .expect("登记得上");
    assert!(wired.fake.sent_texts().is_empty(), "此刻还推不出去");

    // 用户说话了：令牌有了，积压的审批先送到，然后才是这条 `/status` 的回执。
    wired.fake.push(&text_wire("wxid_op", "cid-2", "/status"));

    let fake = Arc::clone(&wired.fake);
    eventually("积压的与新消息的回执都送到了", move || {
        fake.sent_texts().len() >= 2
    })
    .await;

    let texts = wired.fake.sent_texts();
    assert!(
        texts[0].contains(record.short_id.as_str()) && texts[0].contains("shell"),
        "第一条要是积压的审批请求：{texts:?}"
    );
    assert!(
        texts[1].contains("待处理审批"),
        "第二条才是 `/status` 的回执：{texts:?}"
    );
    assert!(
        wired.gateway.pending_deliveries().await.is_empty(),
        "冲刷之后不再 pending"
    );
}

/// §11.2：微信只有 DM，不在 `allow_from` 里的人被拒、不留记录，`/id` 仍然可用。
#[tokio::test]
async fn a_wechat_stranger_is_refused_but_can_still_ask_for_its_id() {
    let wired = wire(
        Behavior::default(),
        vec![text_wire("wxid_other", "cid-s1", "在吗")],
    )
    .await;
    wired.polled_at_least(2).await;

    let fake = Arc::clone(&wired.fake);
    eventually("被拒的提示回出去了", move || {
        !fake.sent_texts().is_empty()
    })
    .await;
    let hint = wired.fake.sent_texts().remove(0);
    assert!(hint.contains("wxid_other"), "{hint}");
    assert!(hint.contains("allow_from"), "{hint}");
    assert!(
        wired.gateway.sessions().await.is_empty(),
        "被拒绝的消息不建会话"
    );
    assert!(
        wired.gateway.pending_deliveries().await.is_empty(),
        "也不留投递记录"
    );

    // `/id` 对任何人可用。
    wired.fake.push(&text_wire("wxid_other", "cid-s2", "/id"));
    let fake = Arc::clone(&wired.fake);
    eventually("`/id` 也答了", move || fake.sent_texts().len() >= 2).await;
    let id = wired.fake.sent_texts().remove(1);
    assert!(id.contains("wechat:wxid_other"), "{id}");
}

/// `ChannelPlatform::supports_unsolicited_push`：微信是唯一答 `false` 的（§11.4）。
#[tokio::test]
async fn only_wechat_cannot_push_unsolicited() {
    assert!(!ChannelPlatform::Wechat.supports_unsolicited_push());
    assert!(ChannelPlatform::Feishu.supports_unsolicited_push());
    assert!(ChannelPlatform::Telegram.supports_unsolicited_push());
    let _ = ChannelPeer::new(ChannelPlatform::Wechat, "wxid_op");
}
