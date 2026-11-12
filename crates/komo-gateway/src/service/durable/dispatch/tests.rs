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
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let mut builder = GatewayBuilder::new(&telegram_config("111"))
        .llm(router as Arc<dyn LlmClient>)
        .stream(worker as Arc<dyn komo_llm::ModelStream>)
        .factory(FixedFactory::sender_only(
            Arc::clone(&sender) as Arc<dyn ChannelSender>
        ));
    if let Some(gate) = gate {
        builder = builder.tool(gate.tool());
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
    // 取消的任务不说话（完整的"停下"是后续步骤）。
    assert!(rig.sender.texts().is_empty(), "{:?}", rig.sender.texts());
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
