//! 分发（`docs/tasks.md` §4.11、§4.14 F2）：主会话收到消息不起模型轮次，由分发任务判断去向，
//! 任务的回复原文直投渠道。模型都是假的：Router 用一个读提示再回答的替身（要点名候选任务的短号），
//! 任务会话用脚本化的 `FakeLlm`。

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::durable::storage::{EntryQuery, TaskQuery};
use komo_kernel::durable::{EntryRecord, Message, ScanOrder, SessionKind, SessionRecord};
use komo_kernel::traits::{LlmClient, TurnDriver};
use komo_kernel::types::chat::ChannelPlatform;
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::turn::{LlmError, Round, RoundInput, TurnRequest};

use super::*;
use crate::channels::ChannelSender;
use crate::service::test_support::Gate;
use crate::service::test_support::harness::{
    FakeLlm, FixedFactory, GatewayBuilder, MemSender, TestGateway, call_round, eventually, inbound,
    telegram_config, text_round,
};

/// Router 的替身：每次判断把提示（user 段）记下来，答案由闭包读提示给出。
struct FnRouter {
    prompts: Mutex<Vec<String>>,
    answer: Box<dyn Fn(&str) -> Result<String, LlmError> + Send + Sync>,
}

impl FnRouter {
    fn new(
        answer: impl Fn(&str) -> Result<String, LlmError> + Send + Sync + 'static,
    ) -> Arc<FnRouter> {
        Arc::new(FnRouter {
            prompts: Mutex::new(Vec::new()),
            answer: Box::new(answer),
        })
    }

    fn calls(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }

    fn prompt(&self, n: usize) -> String {
        self.prompts.lock().unwrap()[n].clone()
    }
}

struct Said(Option<String>);

#[async_trait]
impl TurnDriver for Said {
    async fn next(&mut self, _input: RoundInput) -> Result<Round, LlmError> {
        text_round(1, &self.0.take().unwrap_or_default())
    }

    fn usage(&self) -> TokenUsage {
        TokenUsage::default()
    }
}

#[async_trait]
impl LlmClient for FnRouter {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        let prompt = req.messages[0].text.clone().unwrap_or_default();
        let answer = (self.answer)(&prompt)?;
        self.prompts.lock().unwrap().push(prompt);
        Ok(Box::new(Said(Some(answer))))
    }
}

/// 候选里标题含 `title` 的那一行的短号。
fn handle_of(prompt: &str, title: &str) -> String {
    prompt
        .lines()
        .find(|line| line.starts_with("- id：") && line.contains(title))
        .and_then(|line| line.strip_prefix("- id："))
        .and_then(|rest| rest.split('｜').next())
        .unwrap_or_else(|| panic!("候选里没有「{title}」：{prompt}"))
        .to_owned()
}

fn new_task(title: &str, description: &str) -> String {
    json!({ "route": "new", "title": title, "description": description }).to_string()
}

struct Rig {
    gateway: TestGateway,
    sender: Arc<MemSender>,
}

async fn rig(router: Arc<FnRouter>, worker: Arc<FakeLlm>, gate: Option<&Arc<Gate>>) -> Rig {
    rig_with(
        router,
        worker,
        gate.map(|gate| gate.tool()).into_iter().collect(),
    )
    .await
}

async fn rig_with(
    router: Arc<FnRouter>,
    worker: Arc<FakeLlm>,
    tools: Vec<Arc<dyn komo_kernel::traits::Tool>>,
) -> Rig {
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let mut builder = GatewayBuilder::new(&telegram_config("111"))
        .llm(router as Arc<dyn LlmClient>)
        .stream(worker as Arc<dyn komo_llm::ModelStream>)
        .factory(FixedFactory::sender_only(
            Arc::clone(&sender) as Arc<dyn ChannelSender>
        ));
    for tool in tools {
        builder = builder.tool(tool);
    }
    Rig {
        gateway: builder.start().await,
        sender,
    }
}

impl Rig {
    async fn say(&self, text: &str, key: &str) {
        self.gateway
            .handle(inbound(
                ChannelPlatform::Telegram,
                "111",
                "111",
                text,
                key,
                true,
            ))
            .await;
    }

    fn core(&self) -> Arc<DurableCore> {
        self.gateway.state().core().unwrap().clone()
    }

    async fn main(&self) -> SessionRecord {
        let agent = self.gateway.state().snapshot().agent.default_agent.clone();
        self.core().main_session(&agent).await.unwrap().0
    }

    /// 主线上的全部 entry（旧的在前）。
    async fn main_line(&self) -> Vec<EntryRecord> {
        let main = self.main().await;
        entries_of(&self.core(), main.current_branch).await
    }

    async fn task_sessions(&self) -> Vec<SessionRecord> {
        let query = komo_kernel::durable::storage::SessionQuery {
            kind: Some(SessionKind::Task),
            ..Default::default()
        };
        self.core()
            .storage
            .scan_sessions(&query, 100, None)
            .await
            .unwrap()
            .items
    }

    async fn dispatch_tasks(&self) -> usize {
        let main = self.main().await;
        let query = TaskQuery {
            conversation: Some(main.current_branch),
            kind: Some(DISPATCH_KIND.to_owned()),
            ..Default::default()
        };
        self.core()
            .storage
            .scan_tasks(&query, 100, None)
            .await
            .unwrap()
            .items
            .len()
    }

    async fn wait_texts(&self, what: &str, n: usize) -> Vec<String> {
        let sender = Arc::clone(&self.sender);
        eventually(what, move || sender.texts().len() >= n).await;
        self.sender.texts()
    }
}

async fn entries_of(core: &DurableCore, conversation: ConversationId) -> Vec<EntryRecord> {
    let query = EntryQuery {
        order: Some(ScanOrder::Ascending),
        ..EntryQuery::new(conversation)
    };
    core.storage
        .scan_entries(&query, 500, None)
        .await
        .unwrap()
        .items
}

fn texts_of(entries: &[EntryRecord], kind: &str) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| entry.kind == kind)
        .filter_map(|entry| {
            entry
                .data
                .as_ref()?
                .get("text")?
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

/// 任务会话第一次请求里用户输入的全文。
fn first_input(worker: &FakeLlm) -> String {
    worker.requests.lock().unwrap()[0]
        .messages
        .iter()
        .filter_map(|message| message.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn no_model_turns_in_main(entries: &[EntryRecord]) {
    for entry in entries {
        assert!(
            !entry.kind.starts_with("komo.assistant") && !entry.kind.starts_with("komo.user"),
            "主会话不该有模型轮次，却有 {}",
            entry.kind
        );
    }
}

#[tokio::test]
async fn a_main_message_becomes_a_task_and_its_reply_goes_straight_to_the_channel() {
    let router = FnRouter::new(|_| Ok(new_task("整理周报", "整理本周周报并给出要点")));
    let worker = FakeLlm::new(vec![vec![text_round(
        1,
        "周报整理好了，一共三条。\n\n状态：完成",
    )]]);
    let rig = rig(router.clone(), worker.clone(), None).await;

    rig.say("帮我整理下周报", "telegram:1").await;
    let sent = rig.wait_texts("任务回复投到渠道", 1).await;

    // 原文投出：没有状态行、没有前缀、没有任务号或"任务结果"。
    assert_eq!(sent, ["周报整理好了，一共三条。"]);
    // 没有回执：渠道上只有这一条。
    assert_eq!(rig.sender.sent().len(), 1);

    // 主线：展示消息与回复各一条；主会话没有起过模型轮次，模型只被问了一次（任务会话那次）。
    let line = rig.main_line().await;
    assert_eq!(texts_of(&line, MAIN_USER_ENTRY), ["帮我整理下周报"]);
    assert_eq!(
        texts_of(&line, MAIN_REPLY_ENTRY),
        ["周报整理好了，一共三条。"]
    );
    no_model_turns_in_main(&line);
    assert_eq!(worker.turns(), 1, "只有任务会话请求过模型");
    assert_eq!(router.calls(), 1);

    // 任务会话的第一条输入：任务说明、用户原话、主线最近几条消息，都在输入里。
    let input = first_input(&worker);
    assert!(input.contains("整理本周周报并给出要点"), "{input}");
    assert!(input.contains("帮我整理下周报"), "{input}");
    // 这条消息之前主线上没有别的消息：不会出现"最近的几条消息"一节。
    assert!(!input.contains("最近的几条消息"), "{input}");
    // 任务的提示是"直接对话"那一段，不是"交回给派你的会话"。
    let system = worker.requests.lock().unwrap()[0].system_prompt.clone();
    assert!(system.contains("直接和用户对话"), "{system}");
    assert!(!system.contains("你是一个后台任务"), "{system}");

    let tasks = rig.task_sessions().await;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title.as_deref(), Some("整理周报"));
}

#[tokio::test]
async fn a_new_task_carries_the_recent_main_line_messages() {
    let router = FnRouter::new(|_| Ok(new_task("订机票", "订下周一去上海的机票")));
    let worker = FakeLlm::new(vec![
        vec![text_round(1, "先看了下周的行程。\n状态：完成")],
        vec![text_round(1, "订好了。\n状态：完成")],
    ]);
    let rig = rig(router.clone(), worker.clone(), None).await;

    rig.say("下周我要出差", "telegram:1").await;
    rig.wait_texts("第一个回复", 1).await;
    rig.say("订个去上海的机票", "telegram:2").await;
    rig.wait_texts("第二个回复", 2).await;

    // 第二个任务的第一条输入带着主线上此前的话：用户那句与回复。
    let second = {
        let requests = worker.requests.lock().unwrap();
        requests[1]
            .messages
            .iter()
            .filter_map(|message| message.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(second.contains("下周我要出差"), "{second}");
    assert!(second.contains("先看了下周的行程。"), "{second}");
    // Router 看到的最近消息是同一份。
    let prompt = router.prompt(1);
    assert!(prompt.contains("下周我要出差"), "{prompt}");
    assert!(prompt.contains("先看了下周的行程。"), "{prompt}");
}

#[tokio::test]
async fn a_blocked_task_speaks_its_reason_as_a_plain_sentence() {
    let router = FnRouter::new(|_| Ok(new_task("看仓库", "看一下仓库里的改动")));
    let worker = FakeLlm::new(vec![vec![text_round(
        1,
        "试了两种办法都不行。\n状态：受阻：需要你给仓库的读权限",
    )]]);
    let rig = rig(router, worker, None).await;

    rig.say("看下仓库", "telegram:1").await;
    let sent = rig.wait_texts("受阻的回复", 1).await;

    assert_eq!(sent, ["试了两种办法都不行。\n\n需要你给仓库的读权限"]);
    assert!(!sent[0].contains("状态"));
}

#[tokio::test]
async fn continue_sends_the_message_to_the_task_and_an_ended_task_picks_it_up_again() {
    let router = FnRouter::new(|prompt| {
        if prompt.contains("## 这条消息\n再加一节风险") {
            let handle = handle_of(prompt, "整理周报");
            return Ok(json!({ "route": "continue", "task_id": handle }).to_string());
        }
        Ok(new_task("整理周报", "整理本周周报"))
    });
    let worker = FakeLlm::new(vec![
        vec![text_round(1, "整理好了。\n状态：完成")],
        vec![text_round(1, "风险一节加好了。\n状态：完成")],
    ]);
    let rig = rig(router.clone(), worker.clone(), None).await;

    rig.say("整理周报", "telegram:1").await;
    rig.wait_texts("第一个回复", 1).await;
    // 任务已经结束：判断时它在"最近结束"里。
    rig.say("再加一节风险", "telegram:2").await;
    let sent = rig.wait_texts("接着做的回复", 2).await;

    assert_eq!(sent, ["整理好了。", "风险一节加好了。"]);
    // 没有再开新任务；消息原文进了同一个任务的 conversation。
    let tasks = rig.task_sessions().await;
    assert_eq!(tasks.len(), 1);
    let entries = entries_of(&rig.core(), tasks[0].root_conversation).await;
    let said: Vec<String> = entries
        .iter()
        .filter(|entry| entry.kind == "komo.user")
        .flat_map(|entry| entry.model.iter().flatten())
        .filter_map(|message| match message {
            Message::User(user) => match &user.content {
                Content::Text(text) => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(said.last().map(String::as_str), Some("再加一节风险"));
    assert_eq!(worker.turns(), 2);
    assert!(
        router.prompt(1).contains("已结束"),
        "判断时那个任务在最近结束的里面"
    );
    no_model_turns_in_main(&rig.main_line().await);
}

#[tokio::test]
async fn stop_cancels_the_target_task() {
    let gate = Gate::new();
    let router = FnRouter::new(|prompt| {
        if prompt.contains("## 这条消息\n别做了") {
            let handle = handle_of(prompt, "跑脚本");
            return Ok(json!({ "route": "stop", "task_id": handle }).to_string());
        }
        Ok(new_task("跑脚本", "跑一个很慢的脚本"))
    });
    let worker = FakeLlm::new(vec![vec![
        call_round(1, "g1", "gate", json!({})),
        text_round(2, "跑完了。\n状态：完成"),
    ]]);
    let rig = rig(router, worker, Some(&gate)).await;

    rig.say("跑一下那个脚本", "telegram:1").await;
    gate.wait_entered(1).await;
    rig.say("别做了", "telegram:2").await;

    let core = rig.core();
    let task = rig.task_sessions().await.remove(0);
    let storage = Arc::clone(&core.storage);
    let live = {
        let core = Arc::clone(&core);
        let conversation = task.current_branch;
        move || {
            let core = Arc::clone(&core);
            async move {
                komo_runtime::durable::harness::Harness::live(&*core.storage, conversation)
                    .await
                    .unwrap()
                    .run
                    .is_some()
            }
        }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while live().await {
        assert!(std::time::Instant::now() < deadline, "任务一直没被取消");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    drop(storage);
    let doc = komo_runtime::durable::harness::task_doc(&*core.storage, task.root_conversation)
        .await
        .unwrap()
        .expect("komo.task");
    assert_eq!(
        doc.last_run.map(|run| run.state).as_deref(),
        Some("cancelled")
    );
    // 取消的任务自己不说话；对端收到的是分发落的那一句。
    assert_eq!(rig.sender.texts(), ["好，先停了。"]);
    gate.release();
}

#[tokio::test]
async fn list_answers_from_the_task_list_without_numbers_and_without_a_model() {
    let gate = Gate::new();
    let router = FnRouter::new(|prompt| {
        if prompt.contains("## 这条消息\n手上有什么事") {
            return Ok(json!({ "route": "list" }).to_string());
        }
        Ok(new_task("跑脚本", "跑一个很慢的脚本"))
    });
    let worker = FakeLlm::new(vec![vec![
        call_round(1, "g1", "gate", json!({})),
        text_round(2, "跑完了。\n状态：完成"),
    ]]);
    let rig = rig(router.clone(), worker.clone(), Some(&gate)).await;

    rig.say("跑一下那个脚本", "telegram:1").await;
    gate.wait_entered(1).await;
    rig.say("手上有什么事", "telegram:2").await;
    let sent = rig.wait_texts("清单", 1).await;

    assert_eq!(sent[0], "正在做的：\n· 跑脚本");
    let handle = {
        let prompt = router.prompt(1);
        handle_of(&prompt, "跑脚本")
    };
    assert!(!sent[0].contains(&handle), "清单不带任务号");
    // 清单是主线上的一条展示消息，没有经过任务模型（它只被请求过一次，就是那个脚本任务）。
    let line = rig.main_line().await;
    assert_eq!(texts_of(&line, MAIN_REPLY_ENTRY), ["正在做的：\n· 跑脚本"]);
    assert_eq!(worker.turns(), 1);
    gate.release();
}

#[tokio::test]
async fn an_empty_list_says_there_is_nothing_in_hand() {
    let router = FnRouter::new(|_| Ok(json!({ "route": "list" }).to_string()));
    let rig = rig(router, FakeLlm::finisher("不该被问到"), None).await;

    rig.say("在做什么", "telegram:1").await;
    let sent = rig.wait_texts("清单", 1).await;

    assert_eq!(sent, ["现在手上没有事。"]);
}

#[tokio::test]
async fn a_failing_judgement_still_becomes_a_new_task() {
    let router = FnRouter::new(|_| {
        Err(LlmError::Rejected {
            status: 503,
            message: "down".into(),
        })
    });
    let worker = FakeLlm::new(vec![vec![text_round(1, "收到，查好了。\n状态：完成")]]);
    let rig = rig(router.clone(), worker.clone(), None).await;

    rig.say("查一下今天的告警", "telegram:1").await;
    let sent = rig.wait_texts("兜底任务的回复", 1).await;

    assert_eq!(sent, ["收到，查好了。"]);
    // 兜底的任务说明就是消息原文，不丢消息。
    assert!(first_input(&worker).contains("查一下今天的告警"));
    assert_eq!(rig.task_sessions().await.len(), 1);
}

#[tokio::test]
async fn the_judgement_is_written_into_the_checkpoint_with_its_fallback_reason() {
    let router = FnRouter::new(|_| {
        Err(LlmError::Rejected {
            status: 503,
            message: "down".into(),
        })
    });
    let rig = rig(
        router.clone(),
        FakeLlm::finisher("好了。\n状态：完成"),
        None,
    )
    .await;
    let core = rig.core();
    core.dispatch_hold[1].store(true, Ordering::SeqCst);

    rig.say("查一下今天的告警", "telegram:1").await;
    let main = rig.main().await;
    let query = TaskQuery {
        conversation: Some(main.current_branch),
        kind: Some(DISPATCH_KIND.to_owned()),
        ..Default::default()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let checkpoint = loop {
        let tasks = core
            .storage
            .scan_tasks(&query, 10, None)
            .await
            .unwrap()
            .items;
        if let Some(komo_kernel::durable::TaskState::Running { checkpoint }) =
            tasks.first().map(|task| task.state.clone())
            && checkpoint["phase"] == "execute"
        {
            break checkpoint;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "检查点一直没进到 execute"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    let Checkpoint::Execute { decision } = serde_json::from_value(checkpoint).unwrap() else {
        unreachable!()
    };
    assert!(matches!(decision.route, Route::New { .. }));
    let fallback = decision.fallback.expect("兜底得来的去向要留记录");
    assert_eq!(fallback.attempts, 2);
    assert!(
        fallback.reason.to_string().contains("503"),
        "{}",
        fallback.reason
    );
    core.dispatch_hold[1].store(false, Ordering::SeqCst);
    rig.wait_texts("放行后任务照常派出", 1).await;
}

#[tokio::test]
async fn a_delivered_reply_is_not_sent_again_after_a_restart() {
    let router = FnRouter::new(|_| Ok(new_task("整理周报", "整理本周周报")));
    let worker = FakeLlm::new(vec![vec![text_round(1, "好了。\n状态：完成")]]);
    let Rig {
        mut gateway,
        sender,
    } = rig(router, worker, None).await;
    gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "111",
            "111",
            "整理周报",
            "telegram:1",
            true,
        ))
        .await;
    let first = Arc::clone(&sender);
    eventually("第一次投出", move || first.texts().len() == 1).await;

    let router2 = FnRouter::new(|_| panic!("不该再判断"));
    let (second, sender2) = restart(&mut gateway, router2, FakeLlm::finisher("不该被问到")).await;
    // 启动时的补投逐条过一遍主线上的回复，已经投过的按逻辑键不会再发。
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(sender2.texts().is_empty(), "{:?}", sender2.texts());
    drop(second);
}

#[tokio::test]
async fn the_same_platform_message_makes_one_dispatch_task_and_one_task_session() {
    let router = FnRouter::new(|_| Ok(new_task("整理周报", "整理本周周报")));
    let worker = FakeLlm::new(vec![vec![text_round(1, "好了。\n状态：完成")]]);
    let rig = rig(router.clone(), worker, None).await;

    rig.say("整理周报", "telegram:1").await;
    rig.wait_texts("回复", 1).await;
    // 平台重投同一条（持久去重：先绕过 Dispatcher 进程内的表直接走存储）。
    let main = rig.main().await;
    let again = rig
        .core()
        .dispatch_message(
            &main.id,
            MainMessage {
                text: "整理周报".into(),
                request_id: "telegram:1".into(),
                peer: None,
                principal: None,
            },
        )
        .await
        .unwrap();
    assert!(again.deduplicated);

    assert_eq!(rig.dispatch_tasks().await, 1);
    assert_eq!(rig.task_sessions().await.len(), 1);
    assert_eq!(router.calls(), 1);
    assert_eq!(texts_of(&rig.main_line().await, MAIN_USER_ENTRY).len(), 1);
}

/// 把一台停下来的 Gateway 在同一个数据目录上重新起来，Router 与任务模型换成新的。
async fn restart(
    old: &mut TestGateway,
    router: Arc<FnRouter>,
    worker: Arc<FakeLlm>,
) -> (TestGateway, Arc<MemSender>) {
    old.stop().await;
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gateway = GatewayBuilder::new(&telegram_config("111"))
        .at(&old.home)
        .llm(router as Arc<dyn LlmClient>)
        .stream(worker as Arc<dyn komo_llm::ModelStream>)
        .factory(FixedFactory::sender_only(
            Arc::clone(&sender) as Arc<dyn ChannelSender>
        ))
        .start()
        .await;
    (gateway, sender)
}

#[tokio::test]
async fn a_restart_between_acceptance_and_judgement_still_dispatches_the_message_once() {
    let router = FnRouter::new(|_| Ok(new_task("整理周报", "整理本周周报")));
    let rig_ = rig(router.clone(), FakeLlm::finisher("不该被问到"), None).await;
    let Rig { mut gateway, .. } = rig_;
    let core = gateway.state().core().unwrap().clone();
    core.dispatch_hold[0].store(true, Ordering::SeqCst);

    gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "111",
            "111",
            "整理周报",
            "telegram:1",
            true,
        ))
        .await;
    // 受理已提交（主线上有这条消息），判断被拦住：模型一次都没问过。
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(router.calls(), 0);
    drop(core);

    let router2 = FnRouter::new(|_| Ok(new_task("整理周报", "整理本周周报")));
    let worker2 = FakeLlm::new(vec![vec![text_round(1, "好了。\n状态：完成")]]);
    let (second, sender) = restart(&mut gateway, router2.clone(), worker2.clone()).await;
    let rig2 = Rig {
        gateway: second,
        sender,
    };

    let sent = rig2.wait_texts("重启后的回复", 1).await;
    assert_eq!(sent, ["好了。"]);
    assert_eq!(router2.calls(), 1);
    assert_eq!(rig2.task_sessions().await.len(), 1, "只派一次");
    assert_eq!(rig2.dispatch_tasks().await, 1);
    // 平台在重启后重投同一条：还是同一个分发任务（Dispatcher 进程内的表是空的，靠请求键）。
    rig2.say("整理周报", "telegram:1").await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(rig2.dispatch_tasks().await, 1);
    assert_eq!(rig2.task_sessions().await.len(), 1);
}

#[tokio::test]
async fn a_restart_between_judgement_and_execution_dispatches_the_judged_route_once() {
    let router = FnRouter::new(|_| Ok(new_task("整理周报", "整理本周周报")));
    let Rig { mut gateway, .. } = rig(router.clone(), FakeLlm::finisher("不该被问到"), None).await;
    let core = gateway.state().core().unwrap().clone();
    core.dispatch_hold[1].store(true, Ordering::SeqCst);

    gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "111",
            "111",
            "整理周报",
            "telegram:1",
            true,
        ))
        .await;
    // 判断做完了（去向写进了检查点），执行被拦住：还没有任务会话。
    let probe = Arc::clone(&core);
    eventually("判断写进检查点", move || router.calls() == 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let query = komo_kernel::durable::storage::SessionQuery {
        kind: Some(SessionKind::Task),
        ..Default::default()
    };
    assert!(
        probe
            .storage
            .scan_sessions(&query, 10, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    drop(core);
    drop(probe);

    // 重启后的 Router 不该再被问（判断已经在检查点里）；模型换成会报错的，问了就是兜底也会被看出来。
    let router2 = FnRouter::new(|_| panic!("判断已经落进检查点，重启后不该再判断"));
    let worker2 = FakeLlm::new(vec![vec![text_round(1, "好了。\n状态：完成")]]);
    let (second, sender) = restart(&mut gateway, router2.clone(), worker2).await;
    let rig2 = Rig {
        gateway: second,
        sender,
    };

    let sent = rig2.wait_texts("重启后的回复", 1).await;
    assert_eq!(sent, ["好了。"]);
    assert_eq!(router2.calls(), 0);
    assert_eq!(rig2.task_sessions().await.len(), 1, "只派一次");
}

#[tokio::test]
async fn an_http_message_to_the_main_session_is_dispatched_and_only_the_main_line_gets_the_reply() {
    let router = FnRouter::new(|_| Ok(new_task("整理周报", "整理本周周报")));
    let worker = FakeLlm::new(vec![vec![text_round(1, "好了。\n状态：完成")]]);
    let rig = rig(router, worker.clone(), None).await;
    let main = rig.main().await;

    let (code, _) = rig
        .gateway
        .post_json(
            &format!("/v1/sessions/{}/runs", main.id),
            json!({ "request_key": "http-1", "text": "整理周报" }),
        )
        .await;
    assert_eq!(code, 200);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while texts_of(&rig.main_line().await, MAIN_REPLY_ENTRY).is_empty() {
        assert!(std::time::Instant::now() < deadline, "主线上一直没有回复");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let line = rig.main_line().await;
    assert_eq!(texts_of(&line, MAIN_REPLY_ENTRY), ["好了。"]);
    assert_eq!(worker.turns(), 1);
    // HTTP 来的没有渠道对端：不投。
    assert!(rig.sender.texts().is_empty());
}

#[tokio::test]
async fn messages_are_dispatched_in_arrival_order() {
    // 第二句判断时必须已经看得到第一句派出的任务；否则它会被当成新任务多开一个。
    let router = FnRouter::new(|prompt| {
        if prompt.contains("## 这条消息\n再补一句") {
            if prompt.contains("整理周报") {
                let handle = handle_of(prompt, "整理周报");
                return Ok(json!({ "route": "continue", "task_id": handle }).to_string());
            }
            return Ok(new_task("多开的任务", "不该出现"));
        }
        Ok(new_task("整理周报", "整理本周周报"))
    });
    let worker = FakeLlm::finisher("好了。\n状态：完成");
    let rig = rig(router, worker, None).await;
    let core = rig.core();
    // 两条消息都先受理、都停在判断之前，再同时放行：次序靠分发任务自己守。
    core.dispatch_hold[0].store(true, Ordering::SeqCst);
    rig.say("整理周报", "telegram:1").await;
    rig.say("再补一句", "telegram:2").await;
    core.dispatch_hold[0].store(false, Ordering::SeqCst);

    rig.wait_texts("两条消息都有回复", 2).await;
    let tasks = rig.task_sessions().await;
    assert_eq!(tasks.len(), 1, "第二句接在第一句派出的任务上：{tasks:?}");
    assert_eq!(tasks[0].title.as_deref(), Some("整理周报"));
}

#[tokio::test]
async fn a_dispatched_task_without_a_status_line_is_continued_before_anything_is_sent() {
    let router = FnRouter::new(|_| Ok(new_task("整理周报", "整理本周周报")));
    let worker = FakeLlm::new(vec![
        vec![text_round(1, "改了一半。")],
        vec![text_round(1, "全做完了。\n状态：完成")],
    ]);
    let rig = rig(router, worker.clone(), None).await;

    rig.say("整理周报", "telegram:1").await;
    let sent = rig.wait_texts("续跑之后的回复", 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // 没写状态行的那一轮不外发（它在自动续跑），渠道只收到续跑之后的完整回复。
    assert_eq!(sent, ["全做完了。"]);
    assert_eq!(rig.sender.texts(), ["全做完了。"]);
    assert_eq!(worker.turns(), 2);
}

// ---------------------------------------------------------------- 停下（§4.14 F4）

use komo_kernel::durable::{InputState, SubmissionKind};
use komo_runtime::durable::harness::orchestration::monitor::{self, MonitorState};

fn start_monitor(command: &str) -> Result<komo_kernel::types::turn::Round, LlmError> {
    call_round(
        1,
        "m1",
        "monitor",
        json!({ "command": command, "title": "盯构建" }),
    )
}

/// 判断去向：含 `phrase` 的那条消息按 `route` 指向标题含 `title` 的任务，其余都新建 `title` 的任务。
fn route_by(phrase: &'static str, route: &'static str, title: &'static str) -> Arc<FnRouter> {
    FnRouter::new(move |prompt| {
        if prompt.contains(&format!("## 这条消息\n{phrase}")) {
            let handle = handle_of(prompt, title);
            return Ok(json!({ "route": route, "task_id": handle }).to_string());
        }
        Ok(new_task(title, "照用户说的做"))
    })
}

async fn task_doc_of(rig: &Rig, task: &SessionRecord) -> komo_runtime::durable::harness::TaskDoc {
    komo_runtime::durable::harness::task_doc(&*rig.core().storage, task.root_conversation)
        .await
        .unwrap()
        .expect("komo.task")
}

async fn registry(rig: &Rig) -> Vec<monitor::MonitorRecord> {
    monitor::list(&*rig.core().storage).await.unwrap()
}

async fn settled(rig: &Rig, id: SubmissionId) -> SubmissionKind {
    rig.core()
        .storage
        .submission(id)
        .await
        .unwrap()
        .expect("submission")
        .kind
}

/// 挂着监听的任务被停下：监听登记为已停、不交回结束消息，之后晚到的事件不起轮次，对端收到"好，先停了。"；
/// 之后一条被判成 `continue` 的用户消息让它重新做起来并正常回复，事件又起得了轮次。
#[tokio::test]
async fn a_stopped_task_ends_its_monitors_ignores_late_events_and_resumes_on_continue() {
    let router = route_by("别盯了", "stop", "盯构建");
    let worker = FakeLlm::new(vec![vec![
        start_monitor("sleep 300"),
        text_round(2, "在盯了。\n状态：完成"),
    ]]);
    let mut rig = rig(
        FnRouter::new({
            let stop = router.clone();
            move |prompt| {
                if prompt.contains("## 这条消息\n接着盯") {
                    let handle = handle_of(prompt, "盯构建");
                    return Ok(json!({ "route": "continue", "task_id": handle }).to_string());
                }
                (stop.answer)(prompt)
            }
        }),
        worker.clone(),
        None,
    )
    .await;

    rig.say("盯一下构建", "telegram:1").await;
    rig.wait_texts("挂好监听的回复", 1).await;
    let task = rig.task_sessions().await.remove(0);
    let live = registry(&rig).await;
    assert_eq!(live.len(), 1);
    assert!(live[0].is_live(), "{live:?}");
    let monitor_id = live[0].id;

    rig.say("别盯了", "telegram:2").await;
    let sent = rig.wait_texts("停下的那句话", 2).await;
    assert_eq!(sent, ["在盯了。", "好，先停了。"]);

    // 监听登记为已停；已停的任务不交回结束消息，也没有起新一轮。
    let records = registry(&rig).await;
    assert!(
        matches!(&records[0].state, MonitorState::Ended { reason, .. } if reason == "stopped"),
        "{records:?}"
    );
    let core = rig.core();
    assert!(
        core.storage
            .submission_by_request(task.current_branch, &format!("monitor-end:{monitor_id}"))
            .await
            .unwrap()
            .is_none(),
        "已停的任务不该收到结束消息"
    );
    assert!(task_doc_of(&rig, &task).await.stopped);
    // 任务清单里已停的任务不是"进行中"。
    let listed = core.task_page(false, None, None).await.unwrap().tasks;
    let row = listed
        .iter()
        .find(|row| row.session.as_str() == task.id)
        .unwrap();
    assert_eq!(row.state, komo_kernel::protocol::http::TaskState::Returned);
    assert_eq!(
        row.outcome,
        Some(komo_kernel::protocol::http::TaskOutcome::NotCompleted)
    );

    // 晚到的监听事件：受理处就丢，submission 记 `unanswered{stopped}`，没有新一轮。
    let late = core
        .harness
        .submit(
            &core.engine,
            task.current_branch,
            monitor::event_draft(monitor_id, "盯构建", "9:1", "[监听 #x「盯构建」]\n又有一行"),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            settled(&rig, late).await,
            SubmissionKind::Input {
                state: InputState::Unanswered { ref reason, .. }
            } if reason == "stopped"
        ),
        "晚到的事件该被丢掉"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(worker.turns(), 1, "没有为晚到的事件起轮次");

    // 用户的消息被判成 continue：标记清掉，任务重新做起来并正常回复。
    worker.push_script(vec![text_round(1, "接着盯，没有新动静。\n状态：完成")]);
    rig.say("接着盯", "telegram:3").await;
    let sent = rig.wait_texts("接着做的回复", 3).await;
    assert_eq!(sent[2], "接着盯，没有新动静。");
    assert!(!task_doc_of(&rig, &task).await.stopped);
    let event = core
        .harness
        .submit(
            &core.engine,
            task.current_branch,
            monitor::event_draft(monitor_id, "盯构建", "9:2", "[监听 #x「盯构建」]\n新的一行"),
        )
        .await
        .unwrap();
    assert!(
        !matches!(
            settled(&rig, event).await,
            SubmissionKind::Input {
                state: InputState::Unanswered { .. }
            }
        ),
        "清掉标记之后事件照常受理"
    );
    rig.gateway.stop().await;
}

/// 排队的监听事件在停下时被撤回，不会在取消之后把任务叫醒。
#[tokio::test]
async fn stop_discards_the_monitor_events_queued_in_the_inbox() {
    let gate = Gate::new();
    let router = route_by("停", "stop", "跑脚本");
    let worker = FakeLlm::new(vec![vec![
        call_round(1, "g1", "gate", json!({})),
        text_round(2, "跑完了。\n状态：完成"),
    ]]);
    let rig = rig(router, worker.clone(), Some(&gate)).await;

    rig.say("跑个脚本", "telegram:1").await;
    gate.wait_entered(1).await;
    let task = rig.task_sessions().await.remove(0);
    let core = rig.core();
    // 任务在忙：事件排进收件箱（内部输入，`abort_conversation` 本来会保留它）。
    let queued = core
        .harness
        .submit(
            &core.engine,
            task.current_branch,
            monitor::event_draft(7, "盯", "1:1", "[监听 #x「盯」]\n一行"),
        )
        .await
        .unwrap();
    assert!(matches!(
        settled(&rig, queued).await,
        SubmissionKind::Input {
            state: InputState::Queued
        }
    ));

    rig.say("停", "telegram:2").await;
    rig.wait_texts("停下的那句话", 1).await;
    assert!(
        matches!(
            settled(&rig, queued).await,
            SubmissionKind::Input {
                state: InputState::Unanswered { .. }
            }
        ),
        "排队的事件该被撤回"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(worker.turns(), 1, "撤回之后没有为它起后继轮次");
    assert_eq!(rig.sender.texts(), ["好，先停了。"]);
    gate.release();
}

// ---------------------------------------------------------------- 不许沉默（§4.9 B1）

/// 用户消息起的一轮回 `NO_REPLY`：不当作完成，追问一次；仍然没话，往对端说一句没做成。
#[tokio::test]
async fn a_user_round_that_stays_silent_is_asked_once_then_reported_as_not_done() {
    let router = FnRouter::new(|_| Ok(new_task("整理周报", "整理本周周报")));
    let worker = FakeLlm::new(vec![
        vec![text_round(1, "NO_REPLY")],
        vec![text_round(1, "NO_REPLY")],
    ]);
    let rig = rig(router, worker.clone(), None).await;

    rig.say("整理周报", "telegram:1").await;
    let sent = rig.wait_texts("没做成的那句话", 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(sent, ["这件事我没做成，要不要再试一次？"]);
    assert_eq!(rig.sender.texts().len(), 1);
    assert_eq!(worker.turns(), 2, "追问了一次，不多不少");
    // 追问用的是自动续跑那条输入。
    let asked = worker.requests.lock().unwrap()[1]
        .messages
        .iter()
        .filter_map(|message| message.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(asked.contains("自动发的"), "{asked}");
    // 主线上也落了这一句，任务清单里它没有"完成"。
    assert_eq!(
        texts_of(&rig.main_line().await, MAIN_REPLY_ENTRY),
        ["这件事我没做成，要不要再试一次？"]
    );
    let task = rig.task_sessions().await.remove(0);
    let last = task_doc_of(&rig, &task).await.last_run.unwrap();
    assert_eq!(last.state, "failed");
}

/// 追问之后接上话了，就照常回复，不会再说没做成。
#[tokio::test]
async fn a_silent_round_that_answers_when_asked_is_delivered_normally() {
    let router = FnRouter::new(|_| Ok(new_task("整理周报", "整理本周周报")));
    let worker = FakeLlm::new(vec![
        vec![text_round(1, "NO_REPLY")],
        vec![text_round(1, "整理好了。\n状态：完成")],
    ]);
    let rig = rig(router, worker, None).await;

    rig.say("整理周报", "telegram:1").await;
    rig.wait_texts("回复", 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(rig.sender.texts(), ["整理好了。"]);
}

/// 只由监听事件起的一轮回 `NO_REPLY` 保持现状：不交回、不通知、不追问。
#[tokio::test]
async fn an_events_only_round_may_still_stay_silent() {
    let router = FnRouter::new(|_| Ok(new_task("盯构建", "盯一下构建")));
    let worker = FakeLlm::new(vec![
        vec![
            start_monitor("sleep 1; echo hi; sleep 300"),
            text_round(2, "挂好了。\n状态：完成"),
        ],
        vec![text_round(1, "NO_REPLY")],
    ]);
    let mut rig = rig(router, worker.clone(), None).await;

    rig.say("盯一下构建", "telegram:1").await;
    rig.wait_texts("挂好监听的回复", 1).await;
    let w = Arc::clone(&worker);
    eventually("事件起了一轮", move || w.turns() >= 2).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    assert_eq!(rig.sender.texts(), ["挂好了。"], "事件那一轮静默");
    assert_eq!(worker.turns(), 2, "事件那一轮不追问");
    rig.gateway.stop().await;
}

// ---------------------------------------------------------------- 放开两条规则（C）

async fn goals(rig: &Rig) -> Vec<komo_kernel::types::matter::Matter> {
    rig.gateway.state().matters.list(None, true).await.unwrap()
}

/// 由用户消息派出的任务里 `goal.add` 成功，记下的原话是那条用户消息；同一个任务里由监听事件起的一轮
/// 同样的操作被拒。
#[tokio::test]
async fn a_dispatched_task_may_add_a_goal_on_the_users_words_but_not_on_an_event_round() {
    let router = FnRouter::new(|_| Ok(new_task("盯构建", "盯一下构建")));
    let worker = FakeLlm::new(vec![
        vec![
            call_round(
                1,
                "k1",
                "komo",
                json!({ "action": "goal.add", "params": { "title": "盯住构建" } }),
            ),
            start_monitor("sleep 1; echo hi; sleep 300"),
            text_round(2, "记下了，也挂了监听。\n状态：完成"),
        ],
        vec![
            call_round(
                1,
                "k2",
                "komo",
                json!({ "action": "goal.add", "params": { "title": "事件替我记的" } }),
            ),
            text_round(2, "NO_REPLY"),
        ],
    ]);
    let mut rig = rig(router, worker.clone(), None).await;

    rig.say("盯一下构建，顺便记个目标", "telegram:1").await;
    rig.wait_texts("回复", 1).await;
    let recorded = goals(&rig).await;
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].title, "盯住构建");
    // 证据是任务会话里这一轮收到的那条输入（它带着用户的原话）。
    let task = rig.task_sessions().await.remove(0);
    let komo_kernel::types::memory::EvidenceRef::Event { session, .. } =
        &recorded[0].evidence[0].reference
    else {
        panic!("{:?}", recorded[0].evidence)
    };
    assert_eq!(session.as_str(), task.id);
    assert_eq!(recorded[0].evidence.len(), 1);
    assert!(first_input(&worker).contains("盯一下构建，顺便记个目标"));

    // 监听事件起的一轮：同一个任务，被拒。
    let w = Arc::clone(&worker);
    eventually("事件那一轮的 goal.add 被拒", move || {
        format!("{:?}", w.inputs.lock().unwrap()).contains("找不到用户说的话")
    })
    .await;
    assert_eq!(goals(&rig).await.len(), 1, "事件那一轮不能新建目标");
    rig.gateway.stop().await;
}

/// 由用户消息派出的任务调对外工具不停在确认上；同一个任务里由监听事件起的一轮仍然要确认。
#[tokio::test]
async fn a_dispatched_task_calls_outward_tools_unconfirmed_on_a_user_round_but_not_on_an_event_round()
 {
    let probe = komo_gateway_probe();
    let router = FnRouter::new(|_| Ok(new_task("盯构建", "盯一下构建")));
    let worker = FakeLlm::new(vec![
        vec![
            call_round(1, "o1", "outward_ping", json!({})),
            start_monitor("sleep 1; echo hi; sleep 300"),
            text_round(2, "发了，也挂了监听。\n状态：完成"),
        ],
        vec![
            call_round(1, "o2", "outward_ping", json!({})),
            text_round(2, "NO_REPLY"),
        ],
    ]);
    let mut rig = rig_with(router, worker.clone(), vec![probe.tool()]).await;

    rig.say("通知一下大家，再盯着构建", "telegram:1").await;
    rig.wait_texts("回复", 1).await;
    assert_eq!(probe.calls(), 1, "用户消息起的一轮不用确认");

    // 事件那一轮：停在确认卡上，没有执行。
    let core = rig.core();
    let waiting = {
        let core = Arc::clone(&core);
        move || {
            let core = Arc::clone(&core);
            async move {
                !core
                    .interventions(&komo_kernel::protocol::http::InterventionListQuery::default())
                    .await
                    .unwrap()
                    .is_empty()
            }
        }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !waiting().await {
        assert!(
            std::time::Instant::now() < deadline,
            "事件那一轮没有停在确认上"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(probe.calls(), 1, "确认之前不执行");
    rig.gateway.stop().await;
}

fn komo_gateway_probe() -> Arc<crate::service::test_support::OutwardProbe> {
    crate::service::test_support::OutwardProbe::new()
}

/// 由用户消息派出的任务，如果收到子任务交回的 `task.result` 起了一轮，那一轮仍是触发器：对外工具要停在
/// 确认上，`goal.add` 被拒（用户原话不含交回的内容）。
#[tokio::test]
async fn a_task_result_round_in_a_dispatched_task_is_still_a_trigger() {
    use komo_runtime::durable::harness::TASK_RESULT_ENTRY;
    use komo_runtime::durable::harness::inbox::{InputEntry, SubmissionDraft, WhenBusy};

    let probe = komo_gateway_probe();
    let router = FnRouter::new(|_| Ok(new_task("整理周报", "整理本周周报")));
    let worker = FakeLlm::new(vec![
        vec![text_round(1, "整理好了。\n状态：完成")],
        vec![
            call_round(
                1,
                "k1",
                "komo",
                json!({ "action": "goal.add", "params": { "title": "子任务替我记的" } }),
            ),
            call_round(2, "o1", "outward_ping", json!({})),
            text_round(3, "NO_REPLY"),
        ],
    ]);
    let mut rig = rig_with(router, worker.clone(), vec![probe.tool()]).await;

    rig.say("整理周报", "telegram:1").await;
    rig.wait_texts("回复", 1).await;
    let task = rig.task_sessions().await.remove(0);
    let core = rig.core();
    core.harness
        .submit(
            &core.engine,
            task.current_branch,
            SubmissionDraft::Input {
                content: Content::Text("[后台任务 #x「子」已完成]\n结果".into()),
                entry: Some(InputEntry::new(TASK_RESULT_ENTRY, None)),
                when_busy: WhenBusy::Steer,
                request_id: Some("task-result:1:2".into()),
                internal: true,
            },
        )
        .await
        .unwrap();

    let w = Arc::clone(&worker);
    eventually("goal.add 被拒", move || {
        format!("{:?}", w.inputs.lock().unwrap()).contains("找不到用户说的话")
    })
    .await;
    assert!(goals(&rig).await.is_empty());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let waiting = !core
            .interventions(&komo_kernel::protocol::http::InterventionListQuery::default())
            .await
            .unwrap()
            .is_empty();
        if waiting {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "没有停在确认上");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(probe.calls(), 0, "确认之前不执行");
    rig.gateway.stop().await;
}

/// `/pending` 与 `/answer` 在新路径下仍找得到分发派出的任务会话里待答的确认：列得出来，`/answer approve`
/// 之后对外动作真的执行。
#[tokio::test]
async fn pending_and_answer_still_reach_a_confirmation_inside_a_dispatched_task() {
    use komo_kernel::protocol::InboundAck;
    use komo_runtime::durable::harness::TASK_RESULT_ENTRY;
    use komo_runtime::durable::harness::inbox::{InputEntry, SubmissionDraft, WhenBusy};

    let probe = komo_gateway_probe();
    let router = FnRouter::new(|_| Ok(new_task("整理周报", "整理本周周报")));
    let worker = FakeLlm::new(vec![
        vec![text_round(1, "整理好了。\n状态：完成")],
        vec![
            call_round(1, "o1", "outward_ping", json!({})),
            text_round(2, "NO_REPLY"),
        ],
    ]);
    let rig = rig_with(router, worker, vec![probe.tool()]).await;
    rig.say("整理周报", "telegram:1").await;
    rig.wait_texts("回复", 1).await;
    let task = rig.task_sessions().await.remove(0);
    let core = rig.core();
    core.harness
        .submit(
            &core.engine,
            task.current_branch,
            SubmissionDraft::Input {
                content: Content::Text("[后台任务 #x「子」已完成]\n结果".into()),
                entry: Some(InputEntry::new(TASK_RESULT_ENTRY, None)),
                when_busy: WhenBusy::Steer,
                request_id: Some("task-result:1:2".into()),
                internal: true,
            },
        )
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let handle = loop {
        let list = core
            .interventions(&komo_kernel::protocol::http::InterventionListQuery::default())
            .await
            .unwrap();
        if let Some(one) = list.first() {
            break one.handle.clone();
        }
        assert!(std::time::Instant::now() < deadline, "没有停在确认上");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(probe.calls(), 0, "确认之前不执行");

    let command = |text: &str, key: &str| {
        rig.gateway.handle(inbound(
            ChannelPlatform::Telegram,
            "111",
            "111",
            text,
            key,
            true,
        ))
    };
    let pending = match command("/pending", "telegram:p1").await {
        InboundAck::Replied { text } => text,
        other => panic!("{other:?}"),
    };
    assert!(
        pending.contains(&handle) && pending.contains("待确认"),
        "{pending}"
    );
    let answered = command(&format!("/answer {handle} approve"), "telegram:p2").await;
    assert!(
        matches!(answered, InboundAck::Replied { ref text } if text.contains("已批准")),
        "{answered:?}"
    );
    // 等确认的任务每 5 秒（`probe_interval`）看一次答复。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while probe.calls() == 0 {
        assert!(std::time::Instant::now() < deadline, "批准之后没有执行");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(probe.calls(), 1);
}

/// 卡死检测（`docs/tasks.md` §4.9 B4）：按"没有进展"判。时钟是测试里拨的 `TestClock`，扫描由测试直接调
/// `stall::sweep`，不真的睡。
mod stall_detection {
    use std::sync::atomic::AtomicUsize;

    use komo_kernel::durable::AssistantMessage;
    use komo_kernel::test_support::TestClock;
    use komo_llm::{ModelRequest, ModelStream};
    use komo_runtime::durable::harness::TASK_RESULT_ENTRY;
    use komo_runtime::durable::harness::inbox::{InputEntry, SubmissionDraft, WhenBusy};

    use super::*;
    use crate::service::durable::dispatch::stall::sweep;

    const STALLED: &str = "这件事卡住了，我先停下。要不要再试一次？";

    /// 每次模型调用先等一张许可再放行给 `FakeLlm`：测试用许可一张一张放，"模型调用卡住"就是没许可。
    struct Paced {
        inner: Arc<FakeLlm>,
        permits: Arc<tokio::sync::Semaphore>,
        waiting: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelStream for Paced {
        async fn stream(
            &self,
            request: &ModelRequest<'_>,
            progress: &mut (dyn for<'m> FnMut(&'m AssistantMessage) + Send),
        ) -> Result<AssistantMessage, LlmError> {
            self.waiting.fetch_add(1, Ordering::SeqCst);
            let permit = self.permits.acquire().await;
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            permit.expect("信号量没关").forget();
            self.inner.stream(request, progress).await
        }
    }

    struct Stalling {
        rig: Rig,
        clock: Arc<TestClock>,
        permits: Arc<tokio::sync::Semaphore>,
        waiting: Arc<AtomicUsize>,
        started: time::OffsetDateTime,
    }

    fn config(model: u64, tool: u64) -> String {
        format!(
            "{}\n[execution]\nstall_model_secs = {model}\nstall_tool_secs = {tool}\n",
            telegram_config("111")
        )
    }

    async fn start(
        config: &str,
        worker: Arc<FakeLlm>,
        permits: usize,
        tools: Vec<Arc<dyn komo_kernel::traits::Tool>>,
        home: Option<&std::path::Path>,
        clock: Arc<TestClock>,
    ) -> (Rig, Arc<tokio::sync::Semaphore>, Arc<AtomicUsize>) {
        let sender = MemSender::new(ChannelPlatform::Telegram);
        let permits = Arc::new(tokio::sync::Semaphore::new(permits));
        let waiting = Arc::new(AtomicUsize::new(0));
        let paced = Arc::new(Paced {
            inner: worker,
            permits: Arc::clone(&permits),
            waiting: Arc::clone(&waiting),
        });
        let router = FnRouter::new(|_| Ok(new_task("跑个活", "跑一个活")));
        let mut builder = GatewayBuilder::new(config)
            .llm(router as Arc<dyn LlmClient>)
            .stream(paced as Arc<dyn ModelStream>)
            .clock(clock as Arc<dyn komo_kernel::traits::Clock>)
            .factory(FixedFactory::sender_only(
                Arc::clone(&sender) as Arc<dyn ChannelSender>
            ));
        if let Some(home) = home {
            builder = builder.at(home);
        }
        for tool in tools {
            builder = builder.tool(tool);
        }
        let gateway = builder.start().await;
        (Rig { gateway, sender }, permits, waiting)
    }

    impl Stalling {
        async fn new(model: u64, tool: u64, worker: Arc<FakeLlm>, permits: usize) -> Stalling {
            let clock = Arc::new(TestClock::fixed());
            let started = komo_kernel::traits::Clock::now(&*clock);
            let (rig, permits, waiting) = start(
                &config(model, tool),
                worker,
                permits,
                vec![],
                None,
                Arc::clone(&clock),
            )
            .await;
            Stalling {
                rig,
                clock,
                permits,
                waiting,
                started,
            }
        }

        fn advance_to(&self, secs: i64) {
            self.clock.set(self.started + time::Duration::seconds(secs));
        }

        async fn sweep(&self) {
            sweep(self.rig.gateway.state()).await.unwrap();
        }

        async fn model_waiting(&self, n: usize) {
            let waiting = Arc::clone(&self.waiting);
            eventually("模型调用在等许可", move || {
                waiting.load(Ordering::SeqCst) >= n
            })
            .await;
        }

        /// 任务会话收件箱里排着的输入数。
        async fn inbox(&self) -> usize {
            let task = self.rig.task_sessions().await.remove(0);
            komo_runtime::durable::harness::Harness::inbox(
                &*self.rig.core().storage,
                task.current_branch,
            )
            .await
            .unwrap()
            .items
            .len()
        }

        async fn task_doc(&self) -> komo_runtime::durable::harness::TaskDoc {
            let task = self.rig.task_sessions().await.remove(0);
            task_doc_of(&self.rig, &task).await
        }
    }

    /// 一轮卡在模型调用上：到 80% 先提醒（同一段无进展里只插一次），到点没动静就说一句普通的话、停掉；
    /// 那句话落成线上的 `main.reply`，投给来源对端，不再多说别的。
    #[tokio::test]
    async fn a_round_without_progress_is_reminded_then_stopped_and_the_peer_is_told() {
        let worker = FakeLlm::new(vec![vec![text_round(1, "做好了。\n状态：完成")]]);
        let mut t = Stalling::new(100, 500, worker, 0).await;
        t.rig.say("跑个活", "telegram:1").await;
        t.model_waiting(1).await;

        t.advance_to(50);
        t.sweep().await;
        assert_eq!(t.inbox().await, 0, "阈值的 80% 之前什么都不做");
        assert!(t.rig.sender.texts().is_empty());

        t.advance_to(85);
        t.sweep().await;
        assert_eq!(t.inbox().await, 1, "到 80% 插了一条提醒");
        t.sweep().await;
        assert_eq!(t.inbox().await, 1, "同一段无进展里不重复提醒");
        assert!(t.rig.sender.texts().is_empty());

        t.advance_to(101);
        t.sweep().await;
        let sent = t.rig.wait_texts("卡住的那句话", 1).await;
        assert_eq!(sent, [STALLED]);
        assert!(t.task_doc().await.stopped, "任务被停下了");
        let line = t.rig.main_line().await;
        assert_eq!(texts_of(&line, MAIN_REPLY_ENTRY), [STALLED]);

        // 停掉之后不再被判：再扫、再走时间，也不会重复说。
        t.advance_to(1000);
        t.sweep().await;
        t.sweep().await;
        assert_eq!(t.rig.sender.texts(), [STALLED]);
        t.permits.add_permits(10);
        t.rig.gateway.stop().await;
    }

    /// 一直有进展的长任务跑多久都不被停：每次模型调用完成都是进展，累计远超阈值也没事。
    #[tokio::test]
    async fn a_long_task_that_keeps_making_progress_is_never_stopped() {
        let rounds: Vec<_> = (1..=5)
            .map(|n| call_round(n, &format!("g{n}"), "gate", json!({})))
            .chain([text_round(6, "总算做完了。\n状态：完成")])
            .collect();
        let gate = Gate::new();
        gate.release();
        let worker = FakeLlm::new(vec![rounds]);
        let clock = Arc::new(TestClock::fixed());
        let started = komo_kernel::traits::Clock::now(&*clock);
        let (mut rig, permits, waiting) = start(
            &config(100, 500),
            worker,
            0,
            vec![gate.tool()],
            None,
            Arc::clone(&clock),
        )
        .await;
        rig.say("跑个活", "telegram:1").await;
        let mut elapsed = 0;
        for round in 1..=6 {
            let waiting = Arc::clone(&waiting);
            eventually("模型调用在等许可", move || {
                waiting.load(Ordering::SeqCst) >= 1
            })
            .await;
            // 每一轮之间过去 70 秒（低于 80%），累计 420 秒，远超 100 秒的阈值。
            elapsed += 70;
            clock.set(started + time::Duration::seconds(elapsed));
            sweep(rig.gateway.state()).await.unwrap();
            assert!(
                rig.sender.texts().is_empty(),
                "第 {round} 轮：不该被停或提醒"
            );
            permits.add_permits(1);
            if round < 6 {
                let gate = Arc::clone(&gate);
                eventually("工具跑完", move || gate.entered() >= round).await;
            }
        }
        let sent = rig.wait_texts("最后的回复", 1).await;
        assert_eq!(sent, ["总算做完了。"]);
        assert!(elapsed > 100);
        rig.gateway.stop().await;
    }

    /// 有工具在执行用工具的阈值：模型阈值早就过了，工具阈值没到就不动；工具阈值到了照样提醒再停。
    #[tokio::test]
    async fn a_running_tool_is_judged_by_the_tool_threshold() {
        let gate = Gate::new();
        let worker = FakeLlm::new(vec![vec![call_round(1, "g1", "gate", json!({}))]]);
        let clock = Arc::new(TestClock::fixed());
        let started = komo_kernel::traits::Clock::now(&*clock);
        let (mut rig, permits, _waiting) = start(
            &config(100, 500),
            worker,
            1,
            vec![gate.tool()],
            None,
            Arc::clone(&clock),
        )
        .await;
        rig.say("跑个活", "telegram:1").await;
        gate.wait_entered(1).await;
        let at = |secs: i64| clock.set(started + time::Duration::seconds(secs));
        let core = rig.core();
        let task = rig.task_sessions().await.remove(0);
        let inbox = || async {
            komo_runtime::durable::harness::Harness::inbox(&*core.storage, task.current_branch)
                .await
                .unwrap()
                .items
                .len()
        };

        at(300);
        sweep(rig.gateway.state()).await.unwrap();
        assert_eq!(inbox().await, 0, "过了模型阈值，但这是工具在跑");
        assert!(rig.sender.texts().is_empty());
        at(420);
        sweep(rig.gateway.state()).await.unwrap();
        assert_eq!(inbox().await, 1, "到工具阈值的 80% 提醒");
        at(501);
        sweep(rig.gateway.state()).await.unwrap();
        assert_eq!(rig.wait_texts("卡住的那句话", 1).await, [STALLED]);
        gate.release();
        permits.add_permits(10);
        rig.gateway.stop().await;
    }

    /// 阈值 0 = 不检测（两段各自）。
    #[tokio::test]
    async fn a_zero_threshold_turns_detection_off() {
        let worker = FakeLlm::new(vec![vec![text_round(1, "做好了。\n状态：完成")]]);
        let mut t = Stalling::new(0, 0, worker, 0).await;
        t.rig.say("跑个活", "telegram:1").await;
        t.model_waiting(1).await;
        t.advance_to(100_000);
        t.sweep().await;
        assert_eq!(t.inbox().await, 0);
        assert!(t.rig.sender.texts().is_empty());
        assert!(!t.task_doc().await.stopped);
        t.permits.add_permits(10);
        t.rig.gateway.stop().await;
    }

    /// 停在 `confirm` 上等操作者的任务不算卡死：时间走得再久也不提醒、不停。
    #[tokio::test]
    async fn a_task_waiting_on_a_confirmation_is_not_stalled() {
        let probe = komo_gateway_probe();
        let worker = FakeLlm::new(vec![
            vec![text_round(1, "整理好了。\n状态：完成")],
            vec![
                call_round(1, "o1", "outward_ping", json!({})),
                text_round(2, "NO_REPLY"),
            ],
        ]);
        let clock = Arc::new(TestClock::fixed());
        let started = komo_kernel::traits::Clock::now(&*clock);
        let (mut rig, permits, _waiting) = start(
            &config(100, 500),
            worker,
            10,
            vec![probe.tool()],
            None,
            Arc::clone(&clock),
        )
        .await;
        rig.say("整理周报", "telegram:1").await;
        rig.wait_texts("回复", 1).await;
        let task = rig.task_sessions().await.remove(0);
        let core = rig.core();
        core.harness
            .submit(
                &core.engine,
                task.current_branch,
                SubmissionDraft::Input {
                    content: Content::Text("[后台任务 #x「子」已完成]\n结果".into()),
                    entry: Some(InputEntry::new(TASK_RESULT_ENTRY, None)),
                    when_busy: WhenBusy::Steer,
                    request_id: Some("task-result:1:2".into()),
                    internal: true,
                },
            )
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let waiting = !core
                .interventions(&komo_kernel::protocol::http::InterventionListQuery::default())
                .await
                .unwrap()
                .is_empty();
            if waiting {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "没有停在确认上");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        clock.set(started + time::Duration::seconds(100_000));
        sweep(rig.gateway.state()).await.unwrap();
        let texts = rig.sender.texts();
        assert_eq!(texts.len(), 2, "只有回复与确认卡，没有卡住的话：{texts:?}");
        assert!(texts[1].starts_with("对外动作等你确认"), "{texts:?}");
        assert!(!task_doc_of(&rig, &task).await.stopped);
        permits.add_permits(10);
        rig.gateway.stop().await;
    }

    /// 重启后检测仍然有效：判断只靠已提交的 `activityAt` 与时钟。重启给一个完整的阈值（进展的下限是这一代
    /// 起来的时刻），之后再没有进展就照样被停。
    #[tokio::test]
    async fn detection_still_works_after_a_restart() {
        let worker = FakeLlm::new(vec![vec![text_round(1, "做好了。\n状态：完成")]]);
        let mut t = Stalling::new(100, 500, worker, 0).await;
        t.rig.say("跑个活", "telegram:1").await;
        t.model_waiting(1).await;
        t.advance_to(60);
        t.rig.gateway.stop().await;
        let home = t.rig.gateway.home.clone();

        // 同一个数据目录、同一个（已拨到 60 秒的）时钟再起一台；任务被恢复、又卡在模型调用上。
        let worker = FakeLlm::new(vec![vec![text_round(1, "做好了。\n状态：完成")]]);
        let (mut rig, permits, waiting) = start(
            &config(100, 500),
            worker,
            0,
            vec![],
            Some(&home),
            Arc::clone(&t.clock),
        )
        .await;
        let w = Arc::clone(&waiting);
        eventually("恢复后的模型调用在等许可", move || {
            w.load(Ordering::SeqCst) >= 1
        })
        .await;
        t.clock.set(t.started + time::Duration::seconds(60 + 50));
        sweep(rig.gateway.state()).await.unwrap();
        assert!(rig.sender.texts().is_empty(), "重启后从头算：50 秒还没到");
        t.clock.set(t.started + time::Duration::seconds(60 + 101));
        sweep(rig.gateway.state()).await.unwrap();
        assert_eq!(rig.wait_texts("卡住的那句话", 1).await, [STALLED]);
        permits.add_permits(10);
        rig.gateway.stop().await;
    }
}

/// 线上的消息要能被客户端读到：`main.user` → `message.user`、`main.reply` → `line.reply`，都不挂 Run。
/// 读出来的与订阅时收到的逐条一致。
mod line_events {
    use komo_kernel::events::{Event, EventPayload};
    use komo_kernel::protocol::sse::SseEvent;
    use komo_kernel::types::ids::{Seq, SessionId};

    use super::*;

    /// 订阅到的 `Event` 帧（只取事件本身，不取派生的状态帧）。
    fn drain(
        receiver: &mut tokio::sync::broadcast::Receiver<komo_kernel::protocol::sse::SseFrame>,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(frame) = receiver.try_recv() {
            if let SseEvent::Event(event) = frame.event {
                out.push(*event);
            }
        }
        out
    }

    /// 线上事件的形状：（类型, 正文）。
    fn spoken(events: &[Event]) -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::MessageUser(body) => {
                    Some(("user".to_owned(), body.text.clone().unwrap_or_default()))
                }
                EventPayload::LineReply(body) => Some(("reply".to_owned(), body.text.clone())),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn the_main_line_reads_back_user_messages_task_replies_and_lists_in_order() {
        let router = FnRouter::new(|prompt| {
            if prompt.contains("## 这条消息\n手上有什么事") {
                return Ok(json!({ "route": "list" }).to_string());
            }
            Ok(new_task("整理周报", "整理本周周报"))
        });
        let worker = FakeLlm::new(vec![vec![text_round(1, "周报好了。\n状态：完成")]]);
        let mut rig = rig(router, worker, None).await;
        let session = SessionId::from_raw(rig.main().await.id);
        let mut live = rig.gateway.state().hub.subscribe(&session);

        rig.say("整理下周报", "telegram:1").await;
        rig.wait_texts("任务回复", 1).await;
        rig.say("手上有什么事", "telegram:2").await;
        rig.wait_texts("清单", 2).await;
        // 订阅者能收到的帧是异步推的：等清单那一帧到。
        let mut seen = Vec::new();
        for _ in 0..250 {
            seen.extend(drain(&mut live));
            if spoken(&seen).len() >= 4 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let read = |gateway: &TestGateway| {
            let core = gateway.state().core().unwrap().clone();
            let session = session.clone();
            async move {
                crate::service::durable::feed::backfill(&*core.storage, session.as_str(), Seq(0))
                    .await
                    .unwrap()
            }
        };
        let backfill = read(&rig.gateway).await;
        assert_eq!(
            spoken(&backfill),
            [
                ("user".to_owned(), "整理下周报".to_owned()),
                ("reply".to_owned(), "周报好了。".to_owned()),
                ("user".to_owned(), "手上有什么事".to_owned()),
                ("reply".to_owned(), "现在手上没有事。".to_owned()),
            ]
        );
        assert!(
            backfill
                .iter()
                .filter(|event| matches!(
                    event.payload,
                    EventPayload::MessageUser(_) | EventPayload::LineReply(_)
                ))
                .all(|event| event.run.is_none())
        );
        // 任务的回复带任务会话与回的那条用户消息；清单没有任务。
        let replies: Vec<_> = backfill
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::LineReply(body) => Some(body),
                _ => None,
            })
            .collect();
        let users: Vec<_> = backfill
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::MessageUser(_)))
            .collect();
        assert_eq!(replies[0].origin.as_ref(), Some(&users[0].event_id));
        let task = replies[0].task.as_ref().expect("任务的回复带任务");
        assert_eq!(task.title, "整理周报");
        let task_session = rig.task_sessions().await.remove(0).id;
        assert_eq!(
            task.session.as_ref().map(SessionId::as_str),
            Some(task_session.as_str())
        );
        assert_eq!(replies[1].origin.as_ref(), Some(&users[1].event_id));
        assert!(replies[1].task.is_none());

        // 实时推送与读到的逐条一致（事件整体相等）。
        let live_lines: Vec<&Event> = seen
            .iter()
            .filter(|event| {
                matches!(
                    event.payload,
                    EventPayload::MessageUser(_) | EventPayload::LineReply(_)
                )
            })
            .collect();
        let read_lines: Vec<&Event> = backfill
            .iter()
            .filter(|event| {
                matches!(
                    event.payload,
                    EventPayload::MessageUser(_) | EventPayload::LineReply(_)
                )
            })
            .collect();
        assert_eq!(live_lines, read_lines);

        // 重启后回放与重启前读到的逐帧相同。
        let (mut gateway, _sender) = restart(
            &mut rig.gateway,
            FnRouter::new(|_| Ok(String::new())),
            FakeLlm::new(vec![]),
        )
        .await;
        assert_eq!(read(&gateway).await, backfill);
        gateway.stop().await;
    }
}

/// 对话位置的路由归属、飞书私聊话题与群的线（`docs/tasks.md` §4.7、§4.13、§4.14 F5）。
mod threads {
    use komo_kernel::durable::TaskStatus;
    use komo_kernel::protocol::{InboundAck, InboundMessage};
    use komo_kernel::types::chat::{ChannelPeer, Outbound};

    use super::*;
    use crate::dispatcher::chat_origin;
    use crate::service::durable::sessions::GROUP_LINE_PREFIX;

    const FEISHU_ENV: &str =
        "KOMO_LLM_API_KEY=test-key\nFEISHU_APP_ID=cli_test\nFEISHU_APP_SECRET=test-app-secret\n";

    async fn feishu_rig(router: Arc<FnRouter>, worker: Arc<FakeLlm>) -> Rig {
        feishu_rig_with(router, worker, Vec::new()).await
    }

    async fn feishu_rig_with(
        router: Arc<FnRouter>,
        worker: Arc<FakeLlm>,
        tools: Vec<Arc<dyn komo_kernel::traits::Tool>>,
    ) -> Rig {
        let config = crate::service::test_support::harness::config_toml(
            &crate::service::test_support::harness::feishu_block("\"ou_op\""),
        );
        feishu_rig_config(&config, router, worker, tools).await
    }

    async fn feishu_rig_config(
        config: &str,
        router: Arc<FnRouter>,
        worker: Arc<FakeLlm>,
        tools: Vec<Arc<dyn komo_kernel::traits::Tool>>,
    ) -> Rig {
        let sender = MemSender::new(ChannelPlatform::Feishu);
        let mut builder = GatewayBuilder::new(config)
            .env(FEISHU_ENV)
            .llm(router as Arc<dyn LlmClient>)
            .stream(worker as Arc<dyn komo_llm::ModelStream>)
            .factory(FixedFactory::sender_only(
                Arc::clone(&sender) as Arc<dyn ChannelSender>
            ));
        for tool in tools {
            builder = builder.tool(tool);
        }
        let gateway = builder.start().await;
        Rig { gateway, sender }
    }

    /// 飞书里的一条消息：`thread` 是话题根（没有就是没话题）。
    fn feishu(
        chat: &str,
        thread: Option<&str>,
        private: bool,
        text: &str,
        key: &str,
    ) -> InboundMessage {
        let mut message = inbound(ChannelPlatform::Feishu, chat, "ou_op", text, key, private);
        if let Some(thread) = thread {
            message.peer = message.peer.in_thread(thread);
        }
        message
    }

    fn dm(thread: &str, text: &str, key: &str) -> InboundMessage {
        feishu("oc_dm", Some(thread), true, text, key)
    }

    fn group(chat: &str, thread: &str, text: &str, key: &str) -> InboundMessage {
        feishu(chat, Some(thread), false, text, key)
    }

    impl Rig {
        async fn send(&self, message: InboundMessage) -> InboundAck {
            self.gateway.handle(message).await
        }

        async fn line_of(&self, chat: &str) -> SessionRecord {
            let agent = self.gateway.state().snapshot().agent.default_agent.clone();
            self.core()
                .group_line(&agent, &ChannelPeer::new(ChannelPlatform::Feishu, chat))
                .await
                .unwrap()
                .0
        }

        async fn sessions_of(&self, kind: SessionKind) -> Vec<SessionRecord> {
            let query = komo_kernel::durable::storage::SessionQuery {
                kind: Some(kind),
                ..Default::default()
            };
            self.core()
                .storage
                .scan_sessions(&query, 100, None)
                .await
                .unwrap()
                .items
        }

        /// 这条线上的分发任务都做完了（终态）。
        async fn settled(&self, line: &SessionRecord) {
            let core = self.core();
            let conversation = line.current_branch;
            for _ in 0..250 {
                let query = TaskQuery {
                    conversation: Some(conversation),
                    kind: Some(DISPATCH_KIND.to_owned()),
                    ..Default::default()
                };
                let tasks = core
                    .storage
                    .scan_tasks(&query, 100, None)
                    .await
                    .unwrap()
                    .items;
                if !tasks.is_empty()
                    && tasks
                        .iter()
                        .all(|task| task.state.status() == TaskStatus::Terminal)
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("分发任务没有在 5 秒内做完");
        }

        /// 标题为 `title` 的任务会话。
        async fn task_titled(&self, title: &str) -> SessionRecord {
            self.sessions_of(SessionKind::Task)
                .await
                .into_iter()
                .find(|task| task.title.as_deref() == Some(title))
                .unwrap_or_else(|| panic!("没有任务「{title}」"))
        }

        async fn reply_peers(&self) -> Vec<ChannelPeer> {
            self.sender
                .sent()
                .into_iter()
                .map(|message| message.peer)
                .collect()
        }
    }

    fn handle_for(
        summary_title: &str,
        rig_core: &Arc<DurableCore>,
    ) -> impl std::future::Future<Output = String> {
        let core = Arc::clone(rig_core);
        let title = summary_title.to_owned();
        async move {
            core.task_page(false, None, None)
                .await
                .unwrap()
                .tasks
                .into_iter()
                .find(|task| task.title == title)
                .unwrap_or_else(|| panic!("清单里没有「{title}」"))
                .handle
        }
    }

    /// 飞书私聊里一条开出新话题的主线消息：不再开普通会话，由主会话受理并分发；任务的回话落在这条消息的
    /// 话题里，话题的归属是这个任务；渠道的"处理中"卡片靠同一个 run 收尾。
    #[tokio::test]
    async fn a_new_feishu_dm_topic_is_dispatched_from_main_and_the_task_owns_the_topic() {
        let router = FnRouter::new(|_| Ok(new_task("订机票", "订下周一的机票")));
        let worker = FakeLlm::new(vec![vec![text_round(1, "订好了。\n状态：完成")]]);
        let rig = feishu_rig(router.clone(), worker.clone()).await;

        let ack = rig.send(dm("om_1", "帮我订机票", "feishu:e1")).await;
        let InboundAck::Queued { session, run } = ack else {
            panic!("私聊应该排队：{ack:?}");
        };
        let sent = rig.wait_texts("回复", 1).await;
        assert_eq!(sent, ["订好了。"]);

        // 没有开出普通会话：受理落在主会话。
        assert!(rig.sessions_of(SessionKind::Normal).await.is_empty());
        let main = rig.main().await;
        assert_eq!(session.as_str(), main.id);
        assert_eq!(
            texts_of(&rig.main_line().await, MAIN_USER_ENTRY),
            ["帮我订机票"]
        );
        assert_eq!(router.calls(), 1);

        // 回话在这条消息的话题里；卡片收尾靠的 run 与回执里的一致。
        let reply = rig.sender.sent().remove(0);
        assert_eq!(reply.peer.to_string(), "feishu:oc_dm#om_1");
        let Outbound::RunFinished { run: finished, .. } = reply.outbound else {
            panic!("应该是 RunFinished：{:?}", reply.outbound);
        };
        assert_eq!(finished, run);

        // 话题的归属是这个任务，回话去处也是这个话题。
        let task = rig.task_titled("订机票").await;
        let doc = task_doc_of(&rig, &task).await;
        assert_eq!(doc.thread.as_deref(), Some("feishu:oc_dm#om_1"));
        assert_eq!(
            doc.reply.and_then(|reply| reply.peer).as_deref(),
            Some("feishu:oc_dm#om_1")
        );
    }

    /// 同一话题里的后续消息直通给归属的任务：没有调 Router；任务这一轮已做完（空闲）也照样直通；
    /// 同一条平台消息重投只处理一次。
    #[tokio::test]
    async fn a_followup_in_the_topic_goes_straight_to_the_task_even_when_it_is_idle() {
        let router = FnRouter::new(|_| Ok(new_task("整理周报", "整理周报")));
        let worker = FakeLlm::new(vec![
            vec![text_round(1, "整理好了。\n状态：完成")],
            vec![text_round(1, "加了风险一节。\n状态：完成")],
        ]);
        let rig = feishu_rig(router.clone(), worker.clone()).await;

        rig.send(dm("om_1", "整理周报", "feishu:e1")).await;
        rig.wait_texts("第一个回复", 1).await;
        // 任务已经做完、在空闲：不是结束。
        let ack = rig.send(dm("om_1", "再加一节风险", "feishu:e2")).await;
        assert!(matches!(ack, InboundAck::Queued { .. }), "{ack:?}");
        let sent = rig.wait_texts("第二个回复", 2).await;

        assert_eq!(sent[1], "加了风险一节。");
        assert_eq!(router.calls(), 1, "直通不调 Router");
        assert_eq!(rig.task_sessions().await.len(), 1, "没有再开任务");
        // 展示消息落在线上；回话仍在这个话题里。
        let line = rig.main_line().await;
        assert_eq!(
            texts_of(&line, MAIN_USER_ENTRY),
            ["整理周报", "再加一节风险"]
        );
        assert_eq!(rig.reply_peers().await[1].to_string(), "feishu:oc_dm#om_1");
        let second = {
            let requests = worker.requests.lock().unwrap();
            requests[1]
                .messages
                .iter()
                .filter_map(|message| message.text.clone())
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(second.contains("再加一节风险"), "{second}");

        // 平台重投同一条：直通只处理一次。
        let again = rig.send(dm("om_1", "再加一节风险", "feishu:e2")).await;
        assert!(matches!(again, InboundAck::Duplicate { .. }), "{again:?}");
        rig.settled(&rig.main().await).await;
        assert_eq!(worker.turns(), 2);
        assert_eq!(texts_of(&rig.main_line().await, MAIN_USER_ENTRY).len(), 2);
    }

    /// 任务被停后，同一话题再来消息走 Router；判成 `continue` 后重新做起来并再次直通。
    #[tokio::test]
    async fn after_a_stop_the_topic_goes_through_the_router_and_continue_resumes_it() {
        let router = FnRouter::new(|prompt| {
            if prompt.contains("## 这条消息\n先别订了") {
                return Ok(
                    json!({ "route": "stop", "task_id": handle_of(prompt, "订机票") }).to_string(),
                );
            }
            if prompt.contains("## 这条消息\n接着订") {
                return Ok(
                    json!({ "route": "continue", "task_id": handle_of(prompt, "订机票") })
                        .to_string(),
                );
            }
            Ok(new_task("订机票", "订机票"))
        });
        let worker = FakeLlm::new(vec![
            vec![text_round(1, "订好了。\n状态：完成")],
            vec![text_round(1, "接着订好了。\n状态：完成")],
            vec![text_round(1, "改到周二了。\n状态：完成")],
        ]);
        let rig = feishu_rig(router.clone(), worker.clone()).await;

        rig.send(dm("om_1", "订机票", "feishu:e1")).await;
        rig.wait_texts("第一个回复", 1).await;
        // 操作者在没有话题的主线上叫停（话题里的话都会直通给任务，叫停走 Router）。
        rig.send(feishu("oc_dm", None, true, "先别订了", "feishu:e2"))
            .await;
        rig.wait_texts("停下的回复", 2).await;
        let task = rig.task_titled("订机票").await;
        assert!(task_doc_of(&rig, &task).await.stopped);
        let calls = router.calls();

        // 同一话题：归属的任务已被停 → 走 Router，判 continue → 清掉已停、做起来。
        rig.send(dm("om_1", "接着订", "feishu:e3")).await;
        let sent = rig.wait_texts("接着做的回复", 3).await;
        assert_eq!(sent[2], "接着订好了。");
        assert_eq!(router.calls(), calls + 1, "已停的任务不直通");
        assert!(!task_doc_of(&rig, &task).await.stopped);

        // 又直通了。
        rig.send(dm("om_1", "改到周二", "feishu:e4")).await;
        let sent = rig.wait_texts("直通的回复", 4).await;
        assert_eq!(sent[3], "改到周二了。");
        assert_eq!(router.calls(), calls + 1);
    }

    /// Router 在一个归属任务已被停的话题里判 `new`：新任务拿到归属，旧任务不再收这个话题的消息。
    #[tokio::test]
    async fn a_new_task_takes_over_the_topic_from_the_stopped_owner() {
        let router = FnRouter::new(|prompt| {
            if prompt.contains("## 这条消息\n先别查了") {
                return Ok(
                    json!({ "route": "stop", "task_id": handle_of(prompt, "查日志") }).to_string(),
                );
            }
            if prompt.contains("## 这条消息\n换个事") {
                return Ok(new_task("写周报", "写周报"));
            }
            Ok(new_task("查日志", "查日志"))
        });
        let worker = FakeLlm::new(vec![
            vec![text_round(1, "日志查完了。\n状态：完成")],
            vec![text_round(1, "周报写好了。\n状态：完成")],
            vec![text_round(1, "周报加了一节。\n状态：完成")],
        ]);
        let rig = feishu_rig(router.clone(), worker.clone()).await;

        rig.send(dm("om_1", "查日志", "feishu:e1")).await;
        rig.wait_texts("第一个回复", 1).await;
        rig.send(feishu("oc_dm", None, true, "先别查了", "feishu:e2"))
            .await;
        rig.wait_texts("停下的回复", 2).await;

        rig.send(dm("om_1", "换个事", "feishu:e3")).await;
        rig.wait_texts("新任务的回复", 3).await;
        let old = rig.task_titled("查日志").await;
        let new = rig.task_titled("写周报").await;
        assert_eq!(
            task_doc_of(&rig, &new).await.thread.as_deref(),
            Some("feishu:oc_dm#om_1")
        );
        assert_eq!(task_doc_of(&rig, &old).await.thread, None, "原归属作废");

        // 之后话题里的消息进新任务，旧任务的输入不再增加。
        let before = worker.turns();
        let count_old = || async {
            rig.core()
                .storage
                .scan_entries(&EntryQuery::new(old.current_branch), 500, None)
                .await
                .unwrap()
                .items
                .len()
        };
        let old_before = count_old().await;
        rig.send(dm("om_1", "加一节", "feishu:e4")).await;
        let sent = rig.wait_texts("直通新任务", 4).await;
        assert_eq!(sent[3], "周报加了一节。");
        assert_eq!(worker.turns(), before + 1);
        assert_eq!(
            count_old().await,
            old_before,
            "旧任务不该再收到这个话题的消息"
        );
        assert!(texts_of(&rig.main_line().await, MAIN_USER_ENTRY).contains(&"加一节".to_owned()));
    }

    /// 旧办法开出来的话题会话保持可用：话题里再来消息仍由它自己答，不进主会话、不调 Router。
    #[tokio::test]
    async fn a_topic_session_opened_the_old_way_still_answers_itself() {
        let router = FnRouter::new(|_| Ok(new_task("不该被调", "不该被调")));
        let worker = FakeLlm::new(vec![vec![text_round(1, "旧会话自己答的。")]]);
        let rig = feishu_rig(router.clone(), worker.clone()).await;
        let agent = rig.gateway.state().snapshot().agent.default_agent.clone();
        let peer = ChannelPeer::new(ChannelPlatform::Feishu, "oc_dm").in_thread("om_9");
        let (legacy, _) = rig
            .core()
            .session_for_source(&chat_origin(&peer), &agent)
            .await
            .unwrap();

        let ack = rig.send(dm("om_9", "还在吗", "feishu:e1")).await;
        let InboundAck::Queued { session, .. } = ack else {
            panic!("应该排队：{ack:?}");
        };
        assert_eq!(session.as_str(), legacy.id);
        let sent = rig.wait_texts("旧会话的回复", 1).await;
        assert_eq!(sent, ["旧会话自己答的。"]);
        assert_eq!(router.calls(), 0);
        assert!(texts_of(&rig.main_line().await, MAIN_USER_ENTRY).is_empty());
        assert!(rig.sessions_of(SessionKind::Task).await.is_empty());
    }

    /// 没有话题的私聊不设归属：每条消息都走 Router；任务不带 `thread`。
    #[tokio::test]
    async fn a_private_chat_without_topics_sets_no_owner_and_every_message_is_routed() {
        let router = FnRouter::new(|_| Ok(new_task("查天气", "查天气")));
        let worker = FakeLlm::new(vec![
            vec![text_round(1, "晴。\n状态：完成")],
            vec![text_round(1, "还是晴。\n状态：完成")],
        ]);
        let rig = rig(router.clone(), worker, None).await;

        rig.say("查天气", "telegram:1").await;
        rig.wait_texts("第一个回复", 1).await;
        rig.say("再查一次", "telegram:2").await;
        rig.wait_texts("第二个回复", 2).await;

        assert_eq!(router.calls(), 2);
        for task in rig.task_sessions().await {
            assert_eq!(task_doc_of(&rig, &task).await.thread, None);
        }
    }

    /// 群里 @机器人：受理落在这个群自己的线上，不落在操作者的主会话；Router 输入里没有操作者私聊的
    /// 任务与消息；`new` 之后以话题回复，回执不让渠道发"处理中"卡片；话题里的后续消息直通。
    #[tokio::test]
    async fn a_group_mention_is_dispatched_on_the_groups_own_line_and_the_router_sees_only_that_group()
     {
        let router = FnRouter::new(|prompt| {
            if prompt.contains("## 这条消息\n私聊里的事") {
                return Ok(new_task("私聊任务", "私聊里的事"));
            }
            Ok(new_task("查日志", "查一下日志"))
        });
        let worker = FakeLlm::new(vec![
            vec![text_round(1, "私聊任务做好了。\n状态：完成")],
            vec![text_round(1, "日志里没有异常。\n状态：完成")],
            vec![text_round(1, "再看了一遍，还是没有。\n状态：完成")],
        ]);
        let rig = feishu_rig(router.clone(), worker.clone()).await;

        // 操作者私聊里先有一个任务与几条消息。
        rig.send(dm("om_p", "私聊里的事", "feishu:p1")).await;
        rig.wait_texts("私聊任务的回复", 1).await;

        let ack = rig
            .send(group("oc_g", "om_g1", "看一下日志", "feishu:g1"))
            .await;
        assert_eq!(ack, InboundAck::Ignored, "群里不发卡片 / 表情");
        let sent = rig.wait_texts("群里的回复", 2).await;
        assert_eq!(sent[1], "日志里没有异常。");

        // 受理在群的线上；操作者的主线里看不到群里的内容。
        let line = rig.line_of("oc_g").await;
        assert_eq!(line.kind, SessionKind::Main);
        assert!(
            line.source
                .as_deref()
                .unwrap()
                .starts_with(GROUP_LINE_PREFIX)
        );
        assert_ne!(line.id, rig.main().await.id);
        let line_entries = entries_of(&rig.core(), line.current_branch).await;
        assert_eq!(texts_of(&line_entries, MAIN_USER_ENTRY), ["看一下日志"]);
        assert_eq!(
            texts_of(&line_entries, MAIN_REPLY_ENTRY),
            ["日志里没有异常。"]
        );
        let home = rig.main_line().await;
        assert_eq!(texts_of(&home, MAIN_USER_ENTRY), ["私聊里的事"]);
        assert_eq!(texts_of(&home, MAIN_REPLY_ENTRY), ["私聊任务做好了。"]);

        // Router 的输入：这个群的线上没有任何私聊的东西。
        let prompt = router.prompt(1);
        for leaked in ["私聊任务", "私聊里的事", "私聊任务做好了。"] {
            assert!(
                !prompt.contains(leaked),
                "{leaked} 漏进了群的输入：{prompt}"
            );
        }
        assert!(prompt.contains("## 这条消息\n看一下日志"), "{prompt}");

        // 回话在话题里；任务绑定这个话题。
        let reply = rig.sender.sent().remove(1);
        assert_eq!(reply.peer.to_string(), "feishu:oc_g#om_g1");
        let task = rig.task_titled("查日志").await;
        assert_eq!(task.parent_session.as_deref(), Some(line.id.as_str()));
        assert_eq!(
            task_doc_of(&rig, &task).await.thread.as_deref(),
            Some("feishu:oc_g#om_g1")
        );
        // 群里开出的任务用"直接对话"的那一段提示。
        let system = worker.requests.lock().unwrap()[1].system_prompt.clone();
        assert!(system.contains("直接和用户对话"), "{system}");

        // 话题里的后续消息直通，不调 Router。
        let calls = router.calls();
        rig.send(group("oc_g", "om_g1", "再看一遍", "feishu:g2"))
            .await;
        let sent = rig.wait_texts("直通的回复", 3).await;
        assert_eq!(sent[2], "再看了一遍，还是没有。");
        assert_eq!(router.calls(), calls);
    }

    /// 直接断言拼出来的输入：群的线上取到的候选与最近消息只有这个群的，范围是群。
    #[tokio::test]
    async fn the_router_input_for_a_group_holds_only_that_groups_tasks_and_messages() {
        let router = FnRouter::new(|prompt| {
            if prompt.contains("## 这条消息\n私聊里的事") {
                return Ok(new_task("私聊任务", "私聊里的事"));
            }
            if prompt.contains("## 这条消息\n乙群的事") {
                return Ok(new_task("乙群任务", "乙群的事"));
            }
            Ok(new_task("甲群任务", "甲群的事"))
        });
        let worker = FakeLlm::new(vec![
            vec![text_round(1, "私聊做好了。\n状态：完成")],
            vec![text_round(1, "乙群做好了。\n状态：完成")],
        ]);
        let rig = feishu_rig(router.clone(), worker).await;
        rig.send(dm("om_p", "私聊里的事", "feishu:p1")).await;
        rig.wait_texts("私聊的回复", 1).await;
        rig.send(group("oc_b", "om_b1", "乙群的事", "feishu:b1"))
            .await;
        rig.wait_texts("乙群的回复", 2).await;

        // 甲群来一条新消息：先受理（不等分发），再自己拼输入。
        let core = rig.core();
        let line = rig.line_of("oc_a").await;
        let position = ChannelPeer::new(ChannelPlatform::Feishu, "oc_a").in_thread("om_a1");
        core.dispatch_message(
            &line.id,
            MainMessage {
                text: "甲群的事".into(),
                request_id: "feishu:a1".into(),
                peer: Some(position.clone()),
                principal: None,
            },
        )
        .await
        .unwrap();
        rig.settled(&line).await;
        let entry = entries_of(&core, line.current_branch)
            .await
            .into_iter()
            .find(|entry| entry.kind == MAIN_USER_ENTRY)
            .unwrap();
        let built = input::build(&core, &line, entry.id, "甲群的事", Some(&position))
            .await
            .unwrap();
        assert_eq!(built.input.scope, komo_agent::router::RouteScope::Group);
        // 这条线上此刻只有甲群自己的任务（刚派出的那个），没有私聊与乙群的。
        let titles: Vec<&str> = built
            .input
            .tasks
            .iter()
            .map(|task| task.title.as_str())
            .collect();
        assert_eq!(titles, ["甲群任务"], "{titles:?}");
        assert!(built.input.recent.is_empty(), "{:?}", built.input.recent);
    }

    /// 群里判断后 `ignore`：什么都不发、不开任务、没有卡片（回执是 `Ignored`）；消息只留在线上。
    #[tokio::test]
    async fn an_ignored_group_message_sends_nothing_and_leaves_no_card() {
        let router = FnRouter::new(|_| Ok(json!({ "route": "ignore" }).to_string()));
        let worker = FakeLlm::new(vec![]);
        let rig = feishu_rig(router.clone(), worker.clone()).await;

        let ack = rig
            .send(group("oc_g", "om_g1", "老张，你看下这个", "feishu:g1"))
            .await;
        assert_eq!(ack, InboundAck::Ignored);
        let line = rig.line_of("oc_g").await;
        rig.settled(&line).await;

        assert!(rig.sender.sent().is_empty(), "{:?}", rig.sender.sent());
        assert_eq!(worker.turns(), 0);
        assert!(rig.sessions_of(SessionKind::Task).await.is_empty());
        let entries = entries_of(&rig.core(), line.current_branch).await;
        assert_eq!(texts_of(&entries, MAIN_USER_ENTRY), ["老张，你看下这个"]);
        assert!(texts_of(&entries, MAIN_REPLY_ENTRY).is_empty());
    }

    /// 群里判断模型报错：按 `ignore`，不是 `new`。
    #[tokio::test]
    async fn a_failing_judgement_in_a_group_means_ignore() {
        let router = FnRouter::new(|_| {
            Err(LlmError::Rejected {
                status: 503,
                message: "down".into(),
            })
        });
        let worker = FakeLlm::new(vec![]);
        let rig = feishu_rig(router.clone(), worker.clone()).await;

        rig.send(group("oc_g", "om_g1", "查一下", "feishu:g1"))
            .await;
        let line = rig.line_of("oc_g").await;
        rig.settled(&line).await;

        assert!(rig.sender.sent().is_empty());
        assert_eq!(worker.turns(), 0);
        assert!(rig.sessions_of(SessionKind::Task).await.is_empty());
    }

    /// 群里的 `stop` 指向不属于这个群的任务：不执行。模型点了别处的任务号（候选里没有它）→ 判断失败按
    /// `ignore`；执行时的第二道校验（`belongs`）也认得出不属于这个群的任务。
    #[tokio::test]
    async fn a_group_stop_aimed_at_another_groups_task_does_nothing() {
        let foreign = Arc::new(Mutex::new(String::new()));
        let router = FnRouter::new({
            let foreign = Arc::clone(&foreign);
            move |prompt| {
                if prompt.contains("## 这条消息\n先别做了") {
                    let handle = foreign.lock().unwrap().clone();
                    return Ok(json!({ "route": "stop", "task_id": handle }).to_string());
                }
                Ok(new_task("私聊任务", "私聊里的事"))
            }
        });
        let worker = FakeLlm::new(vec![vec![text_round(1, "私聊做好了。\n状态：完成")]]);
        let rig = feishu_rig(router.clone(), worker).await;
        rig.send(dm("om_p", "私聊里的事", "feishu:p1")).await;
        rig.wait_texts("私聊的回复", 1).await;
        let private_task = rig.task_titled("私聊任务").await;
        *foreign.lock().unwrap() = handle_for("私聊任务", &rig.core()).await;

        rig.send(group("oc_g", "om_g1", "先别做了", "feishu:g1"))
            .await;
        let line = rig.line_of("oc_g").await;
        rig.settled(&line).await;

        assert!(
            !task_doc_of(&rig, &private_task).await.stopped,
            "私聊的任务被群里的话停了"
        );
        assert_eq!(rig.sender.sent().len(), 1, "群里没有任何回话");

        // 第二道校验。
        let core = rig.core();
        let group_peer = ChannelPeer::new(ChannelPlatform::Feishu, "oc_g");
        assert!(
            !owner::belongs(
                &core,
                &line.id,
                Some(&group_peer),
                private_task.root_conversation
            )
            .await
            .unwrap()
        );
        let main = rig.main().await;
        assert!(
            owner::belongs(&core, &main.id, None, private_task.root_conversation)
                .await
                .unwrap()
        );
    }

    /// Telegram 的群没有话题：落在群的线上，每条消息都走 Router，不设归属。
    #[tokio::test]
    async fn a_telegram_group_uses_the_groups_line_and_sets_no_owner() {
        let router = FnRouter::new(|_| Ok(new_task("查库存", "查库存")));
        let worker = FakeLlm::new(vec![
            vec![text_round(1, "有货。\n状态：完成")],
            vec![text_round(1, "仍有货。\n状态：完成")],
        ]);
        let rig = rig(router.clone(), worker, None).await;
        for (text, key) in [("查库存", "telegram:g1"), ("再查", "telegram:g2")] {
            let ack = rig
                .gateway
                .handle(inbound(
                    ChannelPlatform::Telegram,
                    "-100",
                    "111",
                    text,
                    key,
                    false,
                ))
                .await;
            assert_eq!(ack, InboundAck::Ignored);
        }
        rig.wait_texts("两个回复", 2).await;
        assert_eq!(router.calls(), 2);
        let tasks = rig.task_sessions().await;
        assert_eq!(tasks.len(), 2);
        for task in tasks {
            assert_eq!(task_doc_of(&rig, &task).await.thread, None);
        }
        assert!(texts_of(&rig.main_line().await, MAIN_USER_ENTRY).is_empty());
    }

    /// 平台重投群里的第一条消息：分发只处理一次。
    #[tokio::test]
    async fn a_redelivered_group_message_is_dispatched_once() {
        let router = FnRouter::new(|_| Ok(new_task("查日志", "查日志")));
        let worker = FakeLlm::new(vec![vec![text_round(1, "没异常。\n状态：完成")]]);
        let rig = feishu_rig(router.clone(), worker.clone()).await;

        let first = rig
            .send(group("oc_g", "om_g1", "看日志", "feishu:g1"))
            .await;
        let second = rig
            .send(group("oc_g", "om_g1", "看日志", "feishu:g1"))
            .await;
        assert_eq!(first, InboundAck::Ignored);
        // 进程内的去重表先挡住（回原来的回执）；持久的那层在请求键上，由私聊的同名测试覆盖。
        assert!(
            matches!(second, InboundAck::Duplicate { .. } | InboundAck::Ignored),
            "{second:?}"
        );
        rig.wait_texts("回复", 1).await;
        let line = rig.line_of("oc_g").await;
        rig.settled(&line).await;

        assert_eq!(router.calls(), 1);
        assert_eq!(rig.task_sessions().await.len(), 1);
        assert_eq!(worker.turns(), 1);
    }

    /// 群的线同样能读到线上的消息，而且只有这个群的。
    #[tokio::test]
    async fn a_group_line_reads_back_its_own_messages_as_events() {
        use komo_kernel::events::EventPayload;
        let router = FnRouter::new(|_| Ok(new_task("查日志", "查一下日志")));
        let worker = FakeLlm::new(vec![vec![text_round(1, "日志里没有异常。\n状态：完成")]]);
        let rig = feishu_rig(router, worker).await;
        rig.send(group("oc_g", "om_g1", "看一下日志", "feishu:g1"))
            .await;
        rig.wait_texts("群里的回复", 1).await;

        let line = rig.line_of("oc_g").await;
        let events = crate::service::test_support::harness::TestGateway::events_of(
            rig.gateway.state(),
            &komo_kernel::types::ids::SessionId::from_raw(line.id.clone()),
        )
        .await;
        let lines: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::MessageUser(body) => body.text.clone(),
                EventPayload::LineReply(body) => Some(format!("回：{}", body.text)),
                _ => None,
            })
            .collect();
        assert_eq!(lines, ["看一下日志", "回：日志里没有异常。"]);
    }

    // ------------------------------------------------------------ 渠道命令（线不跑模型之后）

    fn replied(ack: InboundAck) -> String {
        match ack {
            InboundAck::Replied { text } => text,
            other => panic!("该是命令的回执：{other:?}"),
        }
    }

    /// 话题有归属任务时，`/cancel` 就地停掉那个任务（`stop_task`）并回一句"好，先停了。"：不调 Router，
    /// 任务不再说话。
    #[tokio::test]
    async fn cancel_in_an_owned_topic_stops_that_topics_task_without_the_router() {
        let gate = Gate::new();
        let router = FnRouter::new(|_| Ok(new_task("跑脚本", "跑一个很慢的脚本")));
        let worker = FakeLlm::new(vec![vec![
            call_round(1, "g1", "gate", json!({})),
            text_round(2, "不该走到这里。"),
        ]]);
        let rig = feishu_rig_with(router.clone(), worker, vec![gate.tool()]).await;
        rig.send(dm("om_a", "跑一下那个脚本", "feishu:1")).await;
        gate.wait_entered(1).await;

        let text = replied(rig.send(dm("om_a", "/cancel", "feishu:2")).await);
        assert_eq!(text, "好，先停了。");
        assert_eq!(router.calls(), 1, "命令没有调 Router");
        let task = rig.task_titled("跑脚本").await;
        assert!(task_doc_of(&rig, &task).await.stopped);
        let core = rig.core();
        let conversation = task.current_branch;
        let live = move || {
            let core = Arc::clone(&core);
            async move {
                komo_runtime::durable::harness::Harness::live(&*core.storage, conversation)
                    .await
                    .unwrap()
                    .run
                    .is_some()
            }
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while live().await {
            assert!(std::time::Instant::now() < deadline, "任务一直没被取消");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(rig.sender.texts().is_empty(), "取消的任务不说话");
        gate.release();
    }

    /// 没有归属的地方（私聊主线、群的线）`/cancel` 当成一条普通消息"先别做了"交给分发判断，由 Router 决定
    /// 停哪个；群里 Router 只看这个群。
    #[tokio::test]
    async fn cancel_without_an_owner_is_dispatched_as_a_plain_message() {
        let gate = Gate::new();
        let router = FnRouter::new(|prompt| {
            if prompt.contains("## 这条消息\n先别做了") {
                // 群里看不到私聊的任务：候选里没有就什么都不做。
                if !prompt.contains("跑脚本") {
                    return Ok(json!({ "route": "ignore" }).to_string());
                }
                let handle = handle_of(prompt, "跑脚本");
                return Ok(json!({ "route": "stop", "task_id": handle }).to_string());
            }
            Ok(new_task("跑脚本", "跑一个很慢的脚本"))
        });
        let worker = FakeLlm::new(vec![vec![
            call_round(1, "g1", "gate", json!({})),
            text_round(2, "不该走到这里。"),
        ]]);
        let rig = feishu_rig_with(router.clone(), worker, vec![gate.tool()]).await;
        // 无话题的私聊：没有归属。
        rig.send(feishu("oc_dm", None, true, "跑一下那个脚本", "feishu:1"))
            .await;
        gate.wait_entered(1).await;

        let ack = rig
            .send(feishu("oc_dm", None, true, "/cancel", "feishu:2"))
            .await;
        assert!(matches!(ack, InboundAck::Queued { .. }), "{ack:?}");
        let sent = rig.wait_texts("停下的那句话", 1).await;
        assert_eq!(sent, ["好，先停了。"]);
        let line = rig.main_line().await;
        assert_eq!(
            texts_of(&line, MAIN_USER_ENTRY),
            ["跑一下那个脚本", "先别做了"],
            "落在主线上的是那句普通话，不是命令"
        );
        assert!(router.prompt(1).contains("## 这条消息\n先别做了"));
        let task = rig.task_titled("跑脚本").await;
        assert!(task_doc_of(&rig, &task).await.stopped);

        // 群的线上同样：不直接停任何任务，进分发（这个群里没有任务，Router 只看得到这个群）。
        let before = router.calls();
        let ack = rig
            .send(group("oc_g", "om_g1", "/cancel", "feishu:g1"))
            .await;
        assert_eq!(ack, InboundAck::Ignored);
        let r = Arc::clone(&router);
        eventually("群里的 /cancel 进了分发", move || r.calls() > before).await;
        let prompt = router.prompt(before);
        assert!(prompt.contains("## 这条消息\n先别做了"), "{prompt}");
        assert!(!prompt.contains("跑脚本"), "{prompt}");
        gate.release();
    }

    /// `/status` 在私聊里等同于 `list`，在群里只列这个群的任务；都不带任务号、不调任何模型；
    /// 命令只有操作者能用。
    #[tokio::test]
    async fn status_lists_only_this_lines_tasks_without_numbers_and_without_a_model() {
        let gate = Gate::new();
        let router = FnRouter::new(|prompt| {
            if prompt.contains("## 这条消息\n群里的活") {
                return Ok(new_task("群任务", "群里的活"));
            }
            Ok(new_task("私聊任务", "私聊里的活"))
        });
        let worker = FakeLlm::new(vec![
            vec![call_round(1, "g1", "gate", json!({}))],
            vec![call_round(1, "g2", "gate", json!({}))],
        ]);
        let rig = feishu_rig_with(router.clone(), worker.clone(), vec![gate.tool()]).await;
        rig.send(feishu("oc_dm", None, true, "私聊里的活", "feishu:1"))
            .await;
        gate.wait_entered(1).await;
        rig.send(group("oc_g", "om_g1", "群里的活", "feishu:g1"))
            .await;
        gate.wait_entered(2).await;
        let (routed, turns) = (router.calls(), worker.turns());

        let dm_list = replied(
            rig.send(feishu("oc_dm", None, true, "/status", "feishu:s1"))
                .await,
        );
        assert_eq!(dm_list, "正在做的：\n· 私聊任务");
        let group_list = replied(
            rig.send(group("oc_g", "om_g1", "/status", "feishu:s2"))
                .await,
        );
        assert_eq!(group_list, "正在做的：\n· 群任务");
        for handle in [
            handle_for("私聊任务", &rig.core()).await,
            handle_for("群任务", &rig.core()).await,
        ] {
            assert!(!dm_list.contains(&handle) && !group_list.contains(&handle));
        }
        assert_eq!(
            (router.calls(), worker.turns()),
            (routed, turns),
            "没调模型"
        );

        // 非操作者用不了命令。
        let stranger = inbound(
            ChannelPlatform::Feishu,
            "oc_g",
            "ou_stranger",
            "/status",
            "feishu:s3",
            false,
        );
        assert!(matches!(
            rig.send(stranger).await,
            InboundAck::Rejected { .. }
        ));
        gate.release();
    }

    /// 线上 `/new` 只回说明，不改任何状态：主线上没有新增 entry、没有分发任务。
    #[tokio::test]
    async fn new_on_a_line_only_explains_and_changes_nothing() {
        let router = FnRouter::new(|_| Ok(json!({ "route": "ignore" }).to_string()));
        let rig = feishu_rig(router.clone(), FakeLlm::new(vec![])).await;
        let line = rig.line_of("oc_g").await;
        let before = entries_of(&rig.core(), line.current_branch).await.len();

        let text = replied(rig.send(group("oc_g", "om_g1", "/new", "feishu:n1")).await);
        assert_eq!(text, "现在每件事都是单独处理的，不需要重新开始。");
        let text = replied(
            rig.send(feishu("oc_dm", None, true, "/new", "feishu:n2"))
                .await,
        );
        assert_eq!(text, "现在每件事都是单独处理的，不需要重新开始。");
        assert_eq!(
            entries_of(&rig.core(), line.current_branch).await.len(),
            before
        );
        assert_eq!(router.calls(), 0);
        assert!(rig.main_line().await.is_empty());
    }

    // ------------------------------------------------------------ 群里派出的任务不碰操作者的记忆与目标

    /// 开着记忆（只走关键词检索）的飞书配置。
    fn memory_config() -> String {
        crate::service::test_support::harness::config_toml(
            &crate::service::test_support::harness::feishu_block("\"ou_op\""),
        )
        .replacen(
            "enabled = false",
            "enabled = true\nmodel = \"main\"\nextract_idle = \"1h\"\n\n[memory.retrieval]\nmode = \"keyword\"",
            1,
        )
    }

    /// 操作者的一条个人记忆与一个目标。
    async fn seed_personal(rig: &Rig) {
        use komo_kernel::protocol::http::MemoryAddRequest;
        use komo_kernel::types::memory::{MemoryKind, MemoryScope};
        let state = rig.gateway.state();
        state
            .memories
            .add(&MemoryAddRequest {
                content: "用户的深色主题偏好是黑底白字".into(),
                kind: MemoryKind::Preference,
                scope: MemoryScope::Personal,
                session: None,
                run: None,
                request_key: None,
                application: None,
                applies_when: None,
                belongs_to: None,
            })
            .await
            .unwrap();
        state
            .matters
            .add(
                &komo_kernel::protocol::matter::MatterAddRequest {
                    kind: komo_kernel::types::matter::MatterKind::Goal,
                    title: "空调巡检".into(),
                    next: Some("看温度".into()),
                    due: None,
                    standard: None,
                    session: None,
                    run: None,
                    request_key: None,
                },
                &komo_memory::matter::Caller::Operator,
            )
            .await
            .unwrap();
    }

    /// 一次模型请求里所有消息的文字（含召回块）。
    fn request_text(worker: &FakeLlm, n: usize) -> String {
        worker.requests.lock().unwrap()[n]
            .messages
            .iter()
            .filter_map(|message| message.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 群的线派出的任务的请求里没有记忆与目标；同样的话私聊派出的任务有（对照）。
    #[tokio::test]
    async fn a_task_dispatched_from_a_group_gets_no_memory_or_goal_injection() {
        let router = FnRouter::new(|prompt| {
            if prompt.contains("## 这条消息\n群里问深色主题") {
                return Ok(new_task("群任务", "群里问深色主题"));
            }
            Ok(new_task("私聊任务", "私聊问深色主题"))
        });
        let worker = FakeLlm::new(vec![
            vec![text_round(1, "私聊答了。\n状态：完成")],
            vec![text_round(1, "群里答了。\n状态：完成")],
        ]);
        let mut rig = feishu_rig_config(&memory_config(), router, worker.clone(), vec![]).await;
        seed_personal(&rig).await;

        rig.send(feishu("oc_dm", None, true, "私聊问深色主题", "feishu:1"))
            .await;
        rig.wait_texts("私聊的回复", 1).await;
        rig.send(group("oc_g", "om_g1", "群里问深色主题", "feishu:g1"))
            .await;
        rig.wait_texts("群里的回复", 2).await;

        // 对照：私聊派出的任务带着记忆与目标。
        let private = request_text(&worker, 0);
        assert!(private.contains("黑底白字"), "对照没有注入记忆：{private}");
        assert!(private.contains("空调巡检"), "对照没有注入目标：{private}");
        // 群里派出的：请求里一个字都没有，会话里也没有召回记录。
        let in_group = request_text(&worker, 1);
        for leaked in ["黑底白字", "空调巡检", "看温度"] {
            assert!(
                !in_group.contains(leaked),
                "{leaked} 漏进了群任务：{in_group}"
            );
        }
        let task = rig.task_titled("群任务").await;
        let entries = entries_of(&rig.core(), task.current_branch).await;
        assert!(
            entries
                .iter()
                .all(|entry| entry.kind != komo_kernel::durable::recall::MEMORY_RECALL_ENTRY_KIND),
            "群任务不该落召回记录"
        );
        rig.gateway.stop().await;
    }

    fn komo_call(
        id: &str,
        action: &str,
        params: Value,
    ) -> Result<komo_kernel::types::turn::Round, LlmError> {
        call_round(1, id, "komo", json!({ "action": action, "params": params }))
    }

    /// 工具结果里出现某句话的次数（`worker.inputs` 记着每一轮拿到的工具结果）。
    fn seen_in_results(worker: &FakeLlm, needle: &str) -> usize {
        format!("{:?}", worker.inputs.lock().unwrap())
            .matches(needle)
            .count()
    }

    /// 群里派出的任务调 `komo` 的记忆与目标动作（读和写）一律被拒；`cron.*` `session.*` `task.list` 也拒
    /// （会带出私聊里的内容）。私聊派出的任务这些照常。
    #[tokio::test]
    async fn a_group_task_cannot_use_the_komo_tool_but_a_private_task_can() {
        let router = FnRouter::new(|prompt| {
            if prompt.contains("## 这条消息\n群里干活") {
                return Ok(new_task("群任务", "群里干活"));
            }
            Ok(new_task("私聊任务", "私聊干活，顺便记个目标"))
        });
        let group_script = vec![
            komo_call("a1", "memory.search", json!({ "query": "深色主题" })),
            komo_call(
                "a2",
                "memory.add",
                json!({ "content": "群里的人让我记的", "kind": "fact" }),
            ),
            komo_call("a3", "goal.add", json!({ "title": "群里的目标" })),
            komo_call("a4", "goal.list", json!({})),
            komo_call("a5", "cron.list", json!({})),
            komo_call("a6", "session.list", json!({})),
            komo_call("a7", "task.list", json!({})),
            text_round(8, "试完了。\n状态：完成"),
        ];
        let private_script = vec![
            komo_call("b1", "memory.search", json!({ "query": "深色主题" })),
            komo_call("b2", "goal.list", json!({})),
            komo_call("b3", "goal.add", json!({ "title": "私聊的目标" })),
            komo_call("b4", "task.list", json!({})),
            text_round(5, "好了。\n状态：完成"),
        ];
        // 先私聊（脚本 1）再群里（脚本 2），各自用一份。
        let worker = FakeLlm::new(vec![private_script, group_script]);
        let mut rig = feishu_rig_config(&memory_config(), router, worker.clone(), vec![]).await;
        seed_personal(&rig).await;

        rig.send(feishu(
            "oc_dm",
            None,
            true,
            "私聊干活，顺便记个目标",
            "feishu:1",
        ))
        .await;
        rig.wait_texts("私聊的回复", 1).await;
        // 照常：查得到记忆与目标，新建目标也成功。
        let seen = format!("{:?}", worker.inputs.lock().unwrap());
        assert!(seen.contains("黑底白字"), "私聊 memory.search：{seen}");
        assert_eq!(
            seen_in_results(&worker, "群里派出的任务不能"),
            0,
            "私聊派出的任务不该被拒"
        );
        let after_private = goals(&rig).await;
        assert!(
            after_private.iter().any(|goal| goal.title == "私聊的目标"),
            "{after_private:?}"
        );

        rig.send(group("oc_g", "om_g1", "群里干活", "feishu:g1"))
            .await;
        rig.wait_texts("群里的回复", 2).await;
        // 四个记忆与目标的动作各被拒一次，三个会带出私聊内容的动作也各被拒一次。
        assert_eq!(
            seen_in_results(&worker, "群里派出的任务不能读写个人记忆与目标"),
            4
        );
        assert_eq!(
            seen_in_results(
                &worker,
                "群里派出的任务不能查看或触发定时任务、会话与任务清单"
            ),
            3
        );
        // 没有写进去：目标只有私聊那个和预置的，记忆里没有群里让记的。
        let titles: Vec<String> = goals(&rig)
            .await
            .into_iter()
            .map(|goal| goal.title)
            .collect();
        assert!(!titles.contains(&"群里的目标".to_owned()), "{titles:?}");
        // `memory.add` 成功的响应里有 `created`：群里那一次没有写成。
        assert_eq!(seen_in_results(&worker, "\\\"created\\\""), 0);
        rig.gateway.stop().await;
    }

    /// 后台提取不从群任务里取：它是任务会话，领取出来的批次类型是任务，提取（`komo-memory`）对任务会话
    /// 一律不调模型、记为已跳过。
    #[tokio::test]
    async fn memory_extraction_batches_from_a_group_task_are_task_sessions() {
        let router = FnRouter::new(|_| Ok(new_task("群任务", "群里干活")));
        let worker = FakeLlm::new(vec![vec![text_round(1, "好了。\n状态：完成")]]);
        let mut rig = feishu_rig_config(&memory_config(), router, worker, vec![]).await;
        rig.send(group("oc_g", "om_g1", "群里干活", "feishu:g1"))
            .await;
        rig.wait_texts("群里的回复", 1).await;

        let core = rig.core();
        let state = rig.gateway.state();
        let (_, work) =
            crate::service::durable::background::memory::seams(&core, &state.config, &state.clock);
        let task = rig.task_titled("群任务").await;
        let batches = work.claim(time::Duration::ZERO, 16).await.unwrap();
        let ours: Vec<_> = batches
            .iter()
            .filter(|batch| batch.session.as_str() == task.id)
            .collect();
        assert!(!ours.is_empty(), "群任务应有待提取的 Run：{batches:?}");
        assert!(
            ours.iter()
                .all(|batch| batch.kind == komo_store::models::SessionKind::Task),
            "{ours:?}"
        );
        rig.gateway.stop().await;
    }

    /// 判断只有一处：群的线派出的任务（含它再派的子任务）是，私聊派出的、群的线自己、cron 都不是。
    #[tokio::test]
    async fn only_tasks_issued_by_a_group_line_count_as_group_tasks() {
        use crate::service::durable::sessions::is_group_task;
        let router = FnRouter::new(|prompt| {
            if prompt.contains("群里") {
                return Ok(new_task("群任务", "群里干活"));
            }
            Ok(new_task("私聊任务", "私聊干活"))
        });
        let worker = FakeLlm::new(vec![
            vec![text_round(1, "私聊好了。\n状态：完成")],
            vec![text_round(1, "群里好了。\n状态：完成")],
        ]);
        let mut rig = feishu_rig(router, worker).await;
        rig.send(feishu("oc_dm", None, true, "私聊干活", "feishu:1"))
            .await;
        rig.wait_texts("私聊的回复", 1).await;
        rig.send(group("oc_g", "om_g1", "群里干活", "feishu:g1"))
            .await;
        rig.wait_texts("群里的回复", 2).await;

        let storage = Arc::clone(&rig.core().storage);
        let private = rig.task_titled("私聊任务").await;
        let in_group = rig.task_titled("群任务").await;
        assert!(!is_group_task(&*storage, &private.id).await);
        assert!(is_group_task(&*storage, &in_group.id).await);
        assert!(
            !is_group_task(&*storage, &rig.line_of("oc_g").await.id).await,
            "线本身不是任务"
        );
        assert!(!is_group_task(&*storage, &rig.main().await.id).await);

        // 群任务再派出的子任务也算（沿 parent 往上找到群的线）。
        let core = rig.core();
        let mut spec = crate::service::durable::sessions::NewSession::new("assistant")
            .kind(SessionKind::Task)
            .title("子任务");
        spec.parent_session = Some(in_group.id.clone());
        let child = core.create_session(spec).await.unwrap();
        assert!(is_group_task(&*storage, &child.id).await);
        rig.gateway.stop().await;
    }

    /// 操作者的主会话只认 `source = home` 的那一条：群的线（也是 `kind = main`）不会被当成它。
    #[tokio::test]
    async fn a_group_line_is_never_taken_for_the_operators_main_session() {
        let rig = feishu_rig(
            FnRouter::new(|_| Ok(json!({ "route": "ignore" }).to_string())),
            FakeLlm::new(vec![]),
        )
        .await;
        let line = rig.line_of("oc_g").await;
        let main = rig.main().await;
        assert_ne!(line.id, main.id);
        assert_eq!(rig.main().await.id, main.id);
    }
}
