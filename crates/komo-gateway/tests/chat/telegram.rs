//! Telegram（Durable 存储，D5 W3a）：与 `../telegram.rs` 同名同断言，Gateway 跑在新存储上。
//! 与 Legacy 不同的只有读会话 / 输入的帮手（`durable_*`）与模型替身（`FakeLlm` 兼当 `ModelStream`）。

use std::sync::Arc;

use komo_kernel::protocol::config::ChannelConfig;
use komo_kernel::traits::Channel;
use komo_kernel::types::chat::ChannelPlatform;

use komo_gateway::channels::ChannelSender;
use komo_gateway::channels::telegram::TelegramChannel;

use crate::harness::{
    FakeLlm, FixedFactory, GatewayBuilder, TestGateway, eventually, telegram_config,
};
use komo_gateway::channels::telegram::fake::{Behavior, FakeBotApi, text_update};

/// 一台起着的 Gateway + 它背后的假 Bot API。渠道是**真的**：`TelegramChannel::serve`
/// 在长轮询，回执与主动投递都真的发到假服务端上。
struct Wired {
    gateway: TestGateway,
    fake: Arc<FakeBotApi>,
}

async fn wire(behavior: Behavior, updates: Vec<serde_json::Value>) -> Wired {
    wire_with(&telegram_config("111"), behavior, updates).await
}

async fn wire_with(config: &str, behavior: Behavior, updates: Vec<serde_json::Value>) -> Wired {
    crate::harness::install_crypto();
    let fake = Arc::new(FakeBotApi::start_with(behavior, updates).await);
    let channel = Arc::new(TelegramChannel::with_api(
        fake.api(),
        ChannelConfig::default(),
    ));
    let sender = channel.sender() as Arc<dyn ChannelSender>;
    let factory = FixedFactory::new(
        ChannelPlatform::Telegram,
        Arc::clone(&channel) as Arc<dyn Channel>,
        sender,
    );
    let gateway = GatewayBuilder::new(config)
        .fake_llm(FakeLlm::finisher("好"))
        .factory(factory)
        .start()
        .await;
    Wired { gateway, fake }
}

impl Wired {
    /// 等假服务端至少被轮询过 `n` 次——"这一批已经被消化过 n 轮了"。
    async fn polled_at_least(&self, n: usize) {
        let fake = Arc::clone(&self.fake);
        eventually(&format!("getUpdates 被调用 {n} 次"), move || {
            fake.calls_to("getUpdates").len() >= n
        })
        .await;
    }
}

/// 验证列①：同一 `update_id` 重发只产生一个 Run。
///
/// 假服务端**无视 offset**，把同一条 `Update` 一直重投；真渠道按"已经处理过的不再处理
/// 一次"跳过，Dispatcher 那一层的 `telegram:{update_id}` 是第二道。
#[tokio::test]
async fn a_replayed_update_produces_one_run() {
    let wired = wire(
        Behavior::replaying(),
        vec![text_update(7, 111, "private", 111, "帮我看看")],
    )
    .await;

    let home = {
        let gateway = &wired.gateway;
        eventually("home session 被这条消息建出来", || true).await;
        // 消息到达之后 home session 才有行；先等 Run 落账再读。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(session) = gateway.session_of("home").await {
                break session;
            }
            assert!(std::time::Instant::now() < deadline, "等不到 home session");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };

    // 让重投再来几轮。
    wired.polled_at_least(5).await;

    let inputs = wired.gateway.inputs(&home).await;
    assert_eq!(inputs, 1, "重投不该开第二个 Run");
}

/// §11.2：群里只响应 @机器人 的消息，并剥掉提及；群不用事先登记。
#[tokio::test]
async fn a_group_message_only_counts_when_it_names_the_bot() {
    let wired = wire(
        Behavior::default(),
        vec![text_update(61, 222, "supergroup", 111, "大家早")],
    )
    .await;

    // 没 @ 的那条走完了（至少轮询过几轮），不该建出任何会话。
    wired.polled_at_least(3).await;
    assert!(
        wired.gateway.origins().await.is_empty(),
        "群里没 @机器人 的消息不进 Dispatcher"
    );

    // @ 了就进。
    wired.fake.push(text_update(
        62,
        222,
        "supergroup",
        111,
        &format!(
            "@{} 看一下日志",
            komo_gateway::channels::telegram::fake::BOT_USERNAME
        ),
    ));

    let gateway_sessions = || async { wired.gateway.origins().await };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let origins = gateway_sessions().await;
        if origins.iter().any(|origin| origin == "chat:telegram:222") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "@机器人 的群消息要进 Run：{origins:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // 群会话不是主会话（§11.2：群按 `{platform}:{chat_id}` 各自一个）。
    let origins = gateway_sessions().await;
    assert!(
        !origins.iter().any(|origin| origin == "home"),
        "群消息不该落进主会话：{origins:?}"
    );
}

/// 机器人所在的任何群，操作者 @ 了就响应，不用事先登记。
#[tokio::test]
async fn an_unregistered_group_is_answered_when_it_names_the_bot() {
    let wired = wire(
        Behavior::default(),
        vec![text_update(
            71,
            333,
            "supergroup",
            111,
            &format!(
                "@{} 在吗",
                komo_gateway::channels::telegram::fake::BOT_USERNAME
            ),
        )],
    )
    .await;

    let gateway = &wired.gateway;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let origins: Vec<String> = gateway.origins().await;
        if origins.iter().any(|origin| origin == "chat:telegram:333") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "没登记的群 @ 了也要进 Run：{origins:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// §11.3 命令表：`/status`、`/pending`、`/cancel` 都有回执；退役的 `/approve` 答一句帮助。
#[tokio::test]
async fn the_command_table_answers_every_command() {
    let wired = wire(Behavior::default(), Vec::new()).await;

    for (id, command) in [
        (81, "/status"),
        (82, "/pending"),
        (83, "/cancel"),
        (84, "/approve"),
    ] {
        wired
            .fake
            .push(text_update(id, 111, "private", 111, command));
    }

    let fake = Arc::clone(&wired.fake);
    eventually("四条命令都答了", move || {
        fake.texts_to("111").len() >= 4
    })
    .await;

    let texts = wired.fake.texts_to("111");
    // §7.5：`/status` 的计数是**两类合计**，而且分得开是哪两类。
    assert!(
        texts
            .iter()
            .any(|text| text.contains("待处理（共 0 条）：结果不明 0 · 前提没了 0")),
        "/status：{texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains("现在没有待处理的 Intervention")),
        "/pending：{texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("没有在跑的任务")),
        "/cancel：{texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains("已经不是命令") && text.contains("/answer")),
        "/approve 要答一句帮助，不进 Run：{texts:?}"
    );
}
