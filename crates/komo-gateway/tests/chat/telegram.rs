//! Telegram：真 `TelegramChannel` 对着 loopback 上的假 Bot API `serve`，真 Dispatcher
//! 在后面（§11.1 的六步、§11.3 的命令与按钮回调）。

use std::sync::Arc;

use komo_kernel::protocol::config::ChannelConfig;
use komo_kernel::traits::Channel;
use komo_kernel::types::chat::{ApprovalScope, ChannelPeer, ChannelPlatform};

use komo_gateway::channels::ChannelSender;
use komo_gateway::channels::telegram::TelegramChannel;

use crate::harness::{FixedFactory, GatewayBuilder, TestGateway, eventually, telegram_config};
use komo_gateway::channels::telegram::fake::{Behavior, FakeBotApi, callback_update, text_update};

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
    let gateway = GatewayBuilder::new(config).factory(factory).start().await;
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
            let sessions = gateway.sessions().await;
            if let Some(record) = sessions.iter().find(|record| record.origin == "home") {
                break record.session.clone();
            }
            assert!(std::time::Instant::now() < deadline, "等不到 home session");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };

    // 让重投再来几轮。
    wired.polled_at_least(5).await;

    let runs = wired.gateway.runs_of(&home).await;
    assert_eq!(runs.len(), 1, "重投不该开第二个 Run：{runs:?}");
}

/// 验证列②：`/approve` 与按钮回调重发**只批准一次**。
///
/// 两条输入带着**两个不同的** `update_id`（去重键挡不住它们，也不该挡），挡住第二次
/// 执行的是审批按 `approval_id` 的幂等（§11.3）。这里断言的是"只有一个决定、而且是
/// 第一次那个"——账本侧的安全性。
#[tokio::test]
async fn a_button_and_a_text_approve_decide_once() {
    let wired = wire(Behavior::default(), Vec::new()).await;
    let record = wired.gateway.pending_approval().await;
    let short = record.short_id.clone();

    // 先按「拒绝」按钮，再发文本 `/approve`——如果第二条也生效，结论会被改写。
    wired.fake.push(callback_update(
        31,
        111,
        111,
        &komo_gateway::render::telegram::callback_data(false, &short),
    ));
    wired.fake.push(text_update(
        32,
        111,
        "private",
        111,
        &format!("/approve {short}"),
    ));

    let fake = Arc::clone(&wired.fake);
    eventually("两条命令都答了", move || {
        fake.texts_to("111").len() >= 3
    })
    .await;

    let stored = wired
        .gateway
        .state()
        .approval_repo
        .get(&record.approval)
        .await
        .expect("读得到")
        .expect("有这条");
    let decision = stored.decision.expect("有结论");
    assert!(
        !decision.approved,
        "第一次是拒绝；第二条命令不该把它改写成批准：{:?}",
        wired.fake.texts_to("111")
    );
}

/// 回给操作者的命令回执（`{short_id} 已批准。` / `… 已经决定过了：…`）。
///
/// 要与 `ApprovalSettled` 的界面回写（`已批准 · {short_id} · 谁 · 何时`）分开——后者是
/// 投递，不是对某一次命令的回答。
fn replies(fake: &FakeBotApi, short: &komo_kernel::types::ids::ShortId) -> Vec<String> {
    fake.texts_to("111")
        .into_iter()
        .filter(|text| text.starts_with(short.as_str()))
        .collect()
}

/// 验证列③：同一人连点两次，**第二次得到「已决定」**。
///
// BUG(1): 第二次得到的是「没有这条待处理的审批（也可能它已经有结论了）。」
// 链路：`Dispatcher::decide`（`src/dispatcher.rs:217`）对短 ID 只查一次
// `ApprovalRepo::find_by_short_id`，而那个实现（`komo-store/src/repos/approvals.rs:146`）
// 写死 `decided().eq(false)`——**这是它自己的正确行为**，短 ID 只在待处理集合内唯一，
// 决定之后要能被下一条审批重用（那里有一条单元测试盯着：
// `a_short_id_only_resolves_while_the_approval_is_pending`）。缺的是 Dispatcher 这一侧
// 的第二次查找：`None` 被当成"没有这条"，于是"已决定"这条路在聊天里根本走不到。
// 按 `approval_id` 走 HTTP 的那条路（`GatewayState::decide_approval` 的
// `already_decided`）是对的——**只有聊天里的短 ID 这条路没有接上**，而§11.3 的命令表
// 要求「已决定的返回原决定，不报错」，§14 的验证列逐字要求「同一人连点两次第二次得到
// 『已决定』」。
// 建议：给 `ApprovalRepo` 加一个 `find_latest_by_short_id`（待处理优先，没有则取最近
// 一条已决定的），`Dispatcher::decide` 用它；查到的那条带着 `decision` 就直接回原决定。
// 安全性今天没有丢（账本上只有一个决定），丢的是"第二次点击得到的是一句准确的话"。
#[tokio::test]
async fn two_clicks_and_the_second_is_told_it_was_decided() {
    let wired = wire(Behavior::default(), Vec::new()).await;
    let record = wired.gateway.pending_approval().await;
    let data = komo_gateway::render::telegram::callback_data(true, &record.short_id);

    // 两次真实点击 = 两个不同的 update_id（§11.1：去重键不管用户连点）。
    wired.fake.push(callback_update(41, 111, 111, &data));
    wired.fake.push(callback_update(42, 111, 111, &data));

    let fake = Arc::clone(&wired.fake);
    eventually("两次点击都答了", move || {
        fake.texts_to("111").len() >= 3
    })
    .await;

    let answers = replies(&wired.fake, &record.short_id);
    assert_eq!(
        answers.len(),
        2,
        "两次点击要有两条以短 ID 开头的回执：{:?}",
        wired.fake.texts_to("111")
    );
    assert!(
        !answers[0].contains("已经决定过"),
        "第一次不是重复：{answers:?}"
    );
    assert!(
        answers[1].contains("已经决定过"),
        "第二次要得到原决定：{answers:?}"
    );
    // `answerCallbackQuery` 两次都调了——不调客户端会一直转圈。
    assert_eq!(wired.fake.calls_to("answerCallbackQuery").len(), 2);
}

/// 验证列④：决定后消息原地更新（按钮被去掉）。
#[tokio::test]
async fn a_decision_removes_the_buttons() {
    let wired = wire(Behavior::default(), Vec::new()).await;
    let record = wired.gateway.pending_approval().await;

    // 审批请求先真的发出去（这一步才让渠道记住它落在哪条消息上）。
    wired
        .gateway
        .state()
        .notifier
        .deliver_approval(None, komo_runtime::approvals::presentation(&record))
        .await
        .expect("投得出去");
    assert!(
        !wired.fake.calls_to("sendMessage").is_empty(),
        "审批请求要真的发到会话里"
    );

    // 按钮回答。
    wired.fake.push(callback_update(
        51,
        111,
        111,
        &komo_gateway::render::telegram::callback_data(true, &record.short_id),
    ));

    let fake = Arc::clone(&wired.fake);
    // 去掉按钮与写回结论是**两次**平台往返（`update_after_decision` 先 markup 再 text），
    // 所以等的是这两样都到——只等前一样就会在负载下读到半路的结果。
    eventually("按钮被去掉且结论写回", move || {
        !fake.calls_to("editMessageReplyMarkup").is_empty()
            && !fake.calls_to("editMessageText").is_empty()
    })
    .await;

    let edits = wired.fake.calls_to("editMessageReplyMarkup");
    assert_eq!(edits[0]["chat_id"], "111");
    assert_eq!(
        edits[0]["reply_markup"]["inline_keyboard"],
        serde_json::json!([]),
        "决定过的请求不该还长着可点的按钮：{edits:?}"
    );
    // 正文末尾补上"已批准 · 谁 · 何时"。
    let texts = wired.fake.calls_to("editMessageText");
    assert!(
        texts
            .iter()
            .any(|body| body["text"].as_str().unwrap_or_default().contains("已批准")),
        "{texts:?}"
    );
}

/// §11.2：群里只响应 @机器人 的消息，并剥掉提及；不在 `groups` 里的群一律不响应。
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
        wired.gateway.sessions().await.is_empty(),
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

    let gateway_sessions = || async {
        wired
            .gateway
            .sessions()
            .await
            .into_iter()
            .map(|record| record.origin)
            .collect::<Vec<_>>()
    };
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

    // 群会话不是 home session（§11.2：群按 `{platform}:{chat_id}` 各自一个）。
    let origins = gateway_sessions().await;
    assert!(
        !origins.iter().any(|origin| origin == "home"),
        "群消息不该落进 home session：{origins:?}"
    );
}

/// 不在 `groups` 里的群，@ 了也不响应。
#[tokio::test]
async fn a_group_outside_the_list_is_ignored_even_when_it_names_the_bot() {
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

    wired.polled_at_least(3).await;
    assert!(
        wired.gateway.sessions().await.is_empty(),
        "只有 groups 列出的群会被响应（§11.2）"
    );
}

/// §11.3 命令表：`/status`、`/pending`、`/cancel` 都有回执。
#[tokio::test]
async fn the_command_table_answers_every_command() {
    let wired = wire(Behavior::default(), Vec::new()).await;
    wired.gateway.pending_approval().await;

    for (id, command) in [(81, "/status"), (82, "/pending"), (83, "/cancel")] {
        wired
            .fake
            .push(text_update(id, 111, "private", 111, command));
    }

    let fake = Arc::clone(&wired.fake);
    eventually("三条命令都答了", move || {
        fake.texts_to("111").len() >= 3
    })
    .await;

    let texts = wired.fake.texts_to("111");
    assert!(
        texts.iter().any(|text| text.contains("待处理审批")),
        "/status：{texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("shell")),
        "/pending 要列出待处理的那条：{texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("没有在跑的任务")),
        "/cancel：{texts:?}"
    );
}

/// §11.3 末 + §11.4：审批请求投到**来源会话 + home chat**，决定之后两处的消息都该
/// 原地更新——「决定过的请求不该还长着可点的按钮」。
///
// BUG(2): 只有 home chat 与**回答的那个会话**会收到 `ApprovalSettled`。
// `GatewayState::decide_approval`（`src/service/state.rs:573` 的 `deliver_home`）只投
// home targets；`Dispatcher::decide`（`src/dispatcher.rs:262`）补的是**下命令的那个**
// 会话。于是"请求投到了来源群 222 + home 111，操作者在 TUI / home 里批"这条路上，群
// 222 的那条消息永远留着两个可点的按钮。再点一次得到的是「已决定」（幂等兜住了安全
// 性），但界面与文档不符。
#[tokio::test]
async fn a_decision_updates_the_source_chat_too() {
    let wired = wire(Behavior::default(), Vec::new()).await;
    let record = wired.gateway.pending_approval().await;

    let source = ChannelPeer::new(ChannelPlatform::Telegram, "222");
    wired
        .gateway
        .state()
        .notifier
        .deliver_approval(
            Some(&source),
            komo_runtime::approvals::presentation(&record),
        )
        .await
        .expect("投得出去");

    assert!(
        !wired.fake.texts_to("222").is_empty(),
        "来源会话要收到审批请求"
    );
    assert!(
        !wired.fake.texts_to("111").is_empty(),
        "home chat 也要收到一份"
    );
    let before = wired.fake.calls_to("editMessageReplyMarkup").len();

    // 在别处（这里是 HTTP / TUI 那条路）做出决定。
    wired
        .gateway
        .state()
        .decide_approval(&record.approval, true, ApprovalScope::Once, None)
        .await
        .expect("决定得了");

    let fake = Arc::clone(&wired.fake);
    eventually("home chat 的那条被原地更新了", move || {
        fake.calls_to("editMessageReplyMarkup").len() > before
    })
    .await;

    let edited: Vec<String> = wired
        .fake
        .calls_to("editMessageReplyMarkup")
        .into_iter()
        .filter_map(|body| body["chat_id"].as_str().map(str::to_string))
        .collect();
    assert!(edited.contains(&"111".to_string()), "home chat：{edited:?}");
    assert!(
        edited.contains(&"222".to_string()),
        "来源会话的那条也该去掉按钮：{edited:?}"
    );
}
