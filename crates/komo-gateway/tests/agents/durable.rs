//! 后台任务（`task`）在 Durable 存储上（`docs/storage-durable-d5.md` W2c）：`tests/agents` 里不依赖
//! 渠道投递的 5 个，换成新 harness——任务会话是 runtime 在派出提交里建的子 conversation（短号 `#<id>`），
//! 结果由 runtime 在子 run 收尾的同一次提交里交回父 conversation。测试名与 Legacy 版本（`main.rs`）一一
//! 对应；home 会话是 `main_session`，输入直接走 `DurableCore::submit_input`（不经渠道与 HTTP），
//! 模型替身是同一个 `TasksLlm` 经 `DriverStream` 演成 `ModelStream`。涉及渠道投递的
//! （`a_background_task_reports_back_through_home` 等）留给 W3a。
//!
//! 与 Legacy 不同的断言各有理由，写在对应测试的注释里。共同的：
//!
//! - 句柄是任务会话根 conversation 的 ID（十进制）；
//! - `TurnRequest.session` 在 Durable 下是**提示缓存的路由键**（home 的 Session ID；fork 用父的），
//!   不是"这条 Run 属于哪个会话"，所以 Legacy 里用 `request.session == 任务会话` 找 fork 的请求，
//!   这里改按输入开头的 `FORK_NOTICE` 找，并用它断言"缓存按父会话路由"。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use komo_gateway::service::durable::tasks::TaskLine;
use komo_gateway::service::test_support::driver_stream::{
    DriverStream, answer_of, core, say, submit, wait_keyed,
};
use komo_gateway::service::test_support::harness::{GatewayBuilder, TestGateway};
use komo_kernel::durable::storage::{EntryQuery, SubmissionQuery};
use komo_kernel::durable::{ConversationId, Message, SubmissionRecord};
use komo_kernel::traits::{LlmClient, OutputWriter, Tool};
use komo_kernel::types::ids::{OperationId, SessionId};
use komo_kernel::types::plan::{ExecutionPlan, Operation, PlanVersions, RecoveryMode};
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolOutput};
use komo_kernel::types::turn::{Role, TurnRequest};
use komo_runtime::durable::harness::{Harness, SlotStatus};
use tokio::sync::Notify;

use super::{TasksLlm, last_user_text, tasks_config};

/// 与 Legacy 同一份配置，去掉渠道（这里不经渠道）。
fn config() -> String {
    let config = tasks_config();
    let at = config.find("[channels.telegram]").expect("有渠道段");
    let end = config.find("[model.main]").expect("有模型段");
    format!("{}{}", &config[..at], &config[end..])
}

async fn start(llm: &Arc<TasksLlm>, at: Option<&Path>, tool: Option<Arc<dyn Tool>>) -> TestGateway {
    let mut builder = GatewayBuilder::new(&config())
        .durable()
        .llm(Arc::clone(llm) as Arc<dyn LlmClient>)
        .stream(DriverStream::new(Arc::clone(llm) as Arc<dyn LlmClient>));
    if let Some(home) = at {
        builder = builder.at(home);
    }
    if let Some(tool) = tool {
        builder = builder.tool(tool);
    }
    builder.start().await
}

/// home 会话：默认 Agent 的主会话。
async fn home_session(gateway: &TestGateway) -> String {
    core(gateway)
        .main_session("assistant")
        .await
        .expect("主会话")
        .0
        .id
}

async fn tasks(gateway: &TestGateway) -> Vec<TaskLine> {
    core(gateway).task_sessions(None).await.expect("任务列表")
}

/// 等到恰好派出了一个任务会话，返回它。
async fn the_task(gateway: &TestGateway) -> TaskLine {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let mut found = tasks(gateway).await;
        if found.len() == 1 {
            return found.remove(0);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "没有派出任务：{found:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 一个 conversation 里全部工具结果：`(是不是错误, 正文)`。
async fn tool_results(gateway: &TestGateway, conversation: ConversationId) -> Vec<(bool, String)> {
    let page = core(gateway)
        .storage
        .scan_entries(&EntryQuery::new(conversation), 10_000, None)
        .await
        .expect("读得出");
    page.items
        .iter()
        .flat_map(|stored| stored.model.iter().flatten())
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some((
                result.is_error,
                result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        komo_kernel::durable::ContentBlock::Text { text, .. } => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
            _ => None,
        })
        .collect()
}

async fn submission_count(gateway: &TestGateway, conversation: ConversationId) -> usize {
    core(gateway)
        .storage
        .scan_submissions(
            &SubmissionQuery {
                conversation: Some(conversation),
                ..SubmissionQuery::default()
            },
            10_000,
            None,
        )
        .await
        .expect("读得出")
        .items
        .len()
}

/// 任务会话里调不动 `task`：能力面里没有它（§14 `task` 验收 ⑦）。
///
/// 改了做法：Legacy 等任务的 Run 终态、再往任务会话里 `submit`；新版等结果交回 home
/// （`task-result:` 的输入在 home 里结算），再往任务会话里 `submit_input`。"调用都失败了"改成看
/// 任务 conversation 里的工具结果都是错误。
#[tokio::test]
async fn a_task_session_cannot_spawn_another_task() {
    let llm = TasksLlm::new();
    let mut gateway = start(&llm, None, None).await;

    // 先让 home 派出一个任务、等它跑完（结果交回 home），再往那个任务会话里说"再派一层"。
    let home = home_session(&gateway).await;
    // 回执（`已派出 #…`）不一定是这条 run 的最终回复：任务很快跑完，结果可能先并进 home 还在跑的 run。
    say(&gateway, &home, "dm-1", "帮我查一下空调").await;
    wait_keyed(&gateway, "task-result:", Duration::from_secs(20)).await;
    let task = the_task(&gateway).await;
    let inner = submit(&gateway, &task.session, "inner-1", "再派一层").await;
    gateway.wait_run(&inner.run).await;

    assert_eq!(tasks(&gateway).await.len(), 1, "任务会话里没有再派出任务");
    let results = tool_results(&gateway, task.conversation).await;
    assert!(
        !results.is_empty() && results.iter().all(|(is_error, _)| *is_error),
        "{results:?}"
    );

    gateway.stop().await;
}

/// `context: "fork"`（`docs/background-tasks.md` §2b）：任务带着 home 此刻的对话起步——
/// 首请求的系统提示、工具定义与 home 上一次请求相同，消息以 home 上一次请求的消息为前缀，
/// 之后才是派它的那次调用、补的结果与任务；缓存按 home 路由；模型沿用 home 的。fork 里的
/// 追问照样带着前缀，即使 home 的目录已经不在；fork 里不能再派任务。
///
/// 改了断言：①缓存路由——Legacy 看 `fork.cache_session == home 会话`；新版 `TurnRequest` 没有那一格，
/// `session` 就是提示缓存键，断言 `fork.session == parent.session == home 会话`；②找请求——见模块文档；
/// ③结果经 home 转告——Legacy 看渠道里的文本；新版看 home 处理 `task-result:` 输入的回答；
/// ④"fork 只读自己的目录"——新版 fork 的上下文来自库里的 conversation 祖先链，本来就不读 home 的目录，
/// 仍把 home 的目录挪走再追问，证明追问不依赖它。
#[tokio::test]
async fn a_fork_starts_from_the_parents_conversation_and_shares_its_prefix() {
    let llm = TasksLlm::new();
    let mut gateway = start(&llm, None, None).await;

    let home = home_session(&gateway).await;
    say(&gateway, &home, "dm-1", "先看一眼").await;
    say(&gateway, &home, "dm-2", "派个 fork 去修").await;
    let task = the_task(&gateway).await;
    let report = wait_keyed(&gateway, "task-result:", Duration::from_secs(20)).await;

    let is_fork =
        |request: &TurnRequest| last_user_text(request).starts_with("（以上是派你的那个会话");
    let requests = llm.requests.lock().expect("请求").clone();
    let parent = requests
        .iter()
        .find(|request| last_user_text(request) == "派个 fork 去修")
        .cloned()
        .expect("home 派任务的那次请求");
    let fork = requests
        .iter()
        .find(|request| is_fork(request))
        .cloned()
        .expect("fork 发过请求");

    // ① 同一个前缀：系统提示、工具定义逐字相同，消息以 home 上一次请求的消息开头。
    assert_eq!(fork.system_prompt, parent.system_prompt);
    assert_eq!(fork.tools, parent.tools, "fork 的工具定义与父相同，含 task");
    assert!(
        fork.tools
            .iter()
            .any(|tool| tool.name == komo_agent::TASK_TOOL)
    );
    let n = parent.messages.len();
    let unsequenced = |messages: &[komo_kernel::types::turn::ReplayMessage]| {
        messages
            .iter()
            .cloned()
            .map(|mut message| {
                message.seq = komo_kernel::types::ids::Seq::ZERO;
                message
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        unsequenced(&fork.messages[..n]),
        unsequenced(&parent.messages[..])
    );
    let call = &fork.messages[n];
    assert_eq!(call.role, Role::Assistant);
    assert_eq!(call.tool_calls[0].name, "task");
    let closed = &fork.messages[n + 1];
    assert_eq!(closed.role, Role::Tool);
    assert_eq!(closed.tool_results[0].call_id, call.tool_calls[0].call_id);
    assert!(
        closed.tool_results[0].content.contains("派出的就是你"),
        "{closed:?}"
    );
    let own = fork.messages[n + 2].text.clone().unwrap_or_default();
    assert!(own.starts_with("（以上是派你的那个会话"), "{own}");
    assert!(own.ends_with("\n\n修 B1"), "{own}");
    assert_eq!(
        fork.session,
        SessionId::from_raw(home.clone()),
        "缓存按父会话路由"
    );
    assert_eq!(fork.session, parent.session);
    assert_eq!(fork.model.model, parent.model.model, "fork 沿用父的模型");

    // 结果照常经 home 转告：home 处理交回的输入、回一句转告。
    assert_eq!(answer_of(&gateway, &report).await, "转告：B1 修好了。");

    // ② fork 里看得见 task，但派不出去：调用落一条失败结果，没有新的任务会话。
    let inner = submit(&gateway, &task.session, "inner-1", "fork 里再派").await;
    gateway.wait_run(&inner.run).await;
    assert_eq!(tasks(&gateway).await.len(), 1, "fork 里没有再派出任务");
    let failed = tool_results(&gateway, task.conversation).await;
    assert!(
        failed.iter().any(|(is_error, _)| *is_error),
        "派不出去的那次调用落了失败结果：{failed:?}"
    );

    // ④ 把 home 的目录挪走（放在最后，挪走之后 home 自己跑不了），追问照样带着前缀跑完。
    let home_dir = gateway.home.join("sessions").join(&home);
    if home_dir.exists() {
        std::fs::rename(&home_dir, home_dir.with_extension("moved")).expect("挪走 home 的目录");
    }
    let follow = submit(&gateway, &task.session, "follow-1", "B1 测了吗").await;
    gateway.wait_run(&follow.run).await;
    let follow_request = llm
        .requests
        .lock()
        .expect("请求")
        .iter()
        .rev()
        .find(|request| last_user_text(request) == "B1 测了吗")
        .cloned()
        .expect("追问发过请求");
    assert_eq!(follow_request.system_prompt, parent.system_prompt);
    assert_eq!(
        unsequenced(&follow_request.messages[..n]),
        unsequenced(&parent.messages[..])
    );
    assert_eq!(follow_request.tools, parent.tools, "追问沿用 fork 的能力面");
    assert_eq!(follow_request.session, parent.session);

    gateway.stop().await;
}

/// 一个会卡在半路的 `unsure`：调进去就等，直到被取消（停机）；计划没有安全恢复办法，所以重启之后
/// "开了头而没有结果"的这次调用只能落成 `uncertain` 交给模型，不会重跑。
struct Stuck {
    entered: Arc<Notify>,
}

#[async_trait]
impl Tool for Stuck {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "unsure".into(),
            description: "测试用：卡住，直到停机".into(),
            parameters: serde_json::json!({ "type": "object" }),
            doc: None,
        }
    }

    async fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        Ok(ExecutionPlan {
            operation_id: OperationId::from_raw(format!("op-unsure-{}", ctx.call)),
            source: ctx.source.clone(),
            tool: "unsure".into(),
            operation: Operation::McpCall {
                server: "test".into(),
                tool: "unsure".into(),
                read_only: false,
            },
            run: Some(ctx.run.clone()),
            tool_call: Some(ctx.call.clone()),
            args,
            cwd: Some(ctx.cwd.clone()),
            targets: vec![],
            versions: PlanVersions::default(),
            recovery: RecoveryMode::NoSafeRecovery,
            then_run: None,
        })
    }

    async fn execute(
        &self,
        _plan: ExecutionPlan,
        ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        self.entered.notify_one();
        while !ctx.cancel.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Err(ToolError::Cancelled)
    }
}

/// fork 在一次调用中途、Gateway 重启之后接着跑：恢复那一段的请求以重启前那次请求为前缀——冻结的
/// 前缀从库里的 conversation 祖先链读回，系统提示、工具与缓存键都不变（§2b）。
///
/// 改了做法：Legacy 让 fork 停在一条"结果不明"的介入上、重启后操作者 `answer`。新版没有介入停靠
/// （§1：`WaitReason` 不再产生），"开了头而没有结果"的调用重启后落成 `uncertain` 结果交给模型，
/// 模型接着答。所以这里让 fork 卡在一次 `unsure` 调用里，停机再起，看恢复出来的那次请求。
#[tokio::test]
async fn a_fork_resumed_after_a_restart_sends_the_same_prefix() {
    let llm = TasksLlm::new();
    let entered = Arc::new(Notify::new());
    let stuck = Arc::new(Stuck {
        entered: Arc::clone(&entered),
    }) as Arc<dyn Tool>;
    let mut gateway = start(&llm, None, Some(Arc::clone(&stuck))).await;

    let home = home_session(&gateway).await;
    say(&gateway, &home, "dm-1", "派个会停的 fork").await;
    let task = the_task(&gateway).await;
    tokio::time::timeout(Duration::from_secs(20), entered.notified())
        .await
        .expect("fork 一直没调到 unsure");
    gateway.stop().await;

    let resumed_llm = TasksLlm::new();
    let mut second = start(&resumed_llm, Some(&gateway.home), Some(stuck)).await;
    let report = wait_keyed(&second, "task-result:", Duration::from_secs(30)).await;
    assert_eq!(
        answer_of(&second, &report).await,
        "转告：等到了。",
        "fork 接着答完，结果交回 home"
    );

    let of_fork = |llm: &TasksLlm| {
        llm.requests
            .lock()
            .expect("请求")
            .iter()
            .find(|request| last_user_text(request).starts_with("（以上是派你的那个会话"))
            .cloned()
            .expect("fork 发过请求")
    };
    let before = of_fork(&llm);
    let after = of_fork(&resumed_llm);
    assert_eq!(after.system_prompt, before.system_prompt);
    assert_eq!(after.tools, before.tools);
    assert_eq!(after.session, before.session, "缓存键不变");
    assert!(
        after.messages.len() > before.messages.len(),
        "恢复那一段多了调用与结果"
    );
    assert_eq!(after.messages[..before.messages.len()], before.messages[..]);
    let _ = task;

    second.stop().await;
}

/// 查进度（§14 task 进度查询验收）：任务正跑着一条 `bash`（已经打出一行、还在 sleep），
/// home 只给 `task_id` 调 `task`——拿到的快照里有正在执行的调用、命令摘要和运行中输出里的
/// 尾部；查询不往任务会话里提交任何输入。
///
/// 改了做法：Legacy 等 `.partial` 文件里出现 `step-1`；新版运行中的输出尾巴在 `komo.live` 的工具槽位里，
/// 等槽位的 `output` 含 `step-1`。"没有新的 Run"改成"任务 conversation 的 submission 数不变"。
#[tokio::test]
async fn a_status_query_shows_what_a_running_task_is_doing() {
    let llm = TasksLlm::new();
    let mut gateway = start(&llm, None, None).await;

    let home = home_session(&gateway).await;
    say(&gateway, &home, "dm-1", "派个慢 bash").await;
    let task = the_task(&gateway).await;

    // 等 bash 的第一行输出进了槽位的输出尾巴：这时它还在 sleep。
    let storage = Arc::clone(&core(&gateway).storage);
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let live = Harness::live(&*storage, task.conversation).await.unwrap();
            if live.tools.iter().flatten().any(|slot| {
                slot.name == "bash"
                    && slot.status == SlotStatus::Running
                    && slot
                        .output
                        .as_deref()
                        .is_some_and(|out| out.contains("step-1"))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("bash 的 step-1 没有进输出尾巴");
    let before = submission_count(&gateway, task.conversation).await;
    assert_eq!(before, 1);

    llm.set_follow_target(&task.conversation.to_string());
    let snapshot = say(&gateway, &home, "dm-2", "进度呢").await;
    assert!(snapshot.contains("正在执行：bash"), "{snapshot}");
    assert!(snapshot.contains("echo step-1; sleep 30"), "{snapshot}");
    assert!(snapshot.contains("step-1"), "{snapshot}");
    assert_eq!(
        submission_count(&gateway, task.conversation).await,
        before,
        "查进度不提交任何输入"
    );

    gateway.stop().await;
}

/// 查一个不存在的短号：干净失败，开头是"查不到进度"。
#[tokio::test]
async fn a_status_query_for_an_unknown_task_fails_cleanly() {
    let llm = TasksLlm::new();
    let mut gateway = start(&llm, None, None).await;

    let home = home_session(&gateway).await;
    let reply = say(&gateway, &home, "dm-1", "进度呢 zzzz").await;
    assert!(reply.contains("查不到进度"), "{reply}");
    assert!(tasks(&gateway).await.is_empty(), "查进度不建任务会话");

    gateway.stop().await;
}

// ---------------------------------------------------------------- 渠道投递（W3a）
//
// 这四个与 Legacy（`main.rs`）同名：DM 走真 Dispatcher，回复、并入通知看内存发送口。与 Legacy 不同的
// 断言各有理由，写在对应测试的注释里；共同的：
//
// - Legacy 里"这条 Run 受理时带不带插话标记"（`run.accepted.interject`）是内部旗子，新存储的
//   `RunAccepted` 是合成的、没有那一格；插话体现为行为（并进宿主、终态 `absorbed`），断言改看行为；
// - 任务会话的"origin / agent_id"换成 sessions 行的 `parent_session` / `agent`。

use komo_gateway::channels::ChannelSender;
use komo_gateway::service::test_support::Gate;
use komo_gateway::service::test_support::harness::{FixedFactory, MemSender, eventually, inbound};
use komo_kernel::protocol::InboundAck;
use komo_kernel::types::chat::{ChannelPlatform, Outbound};
use komo_kernel::types::status::RunState;

async fn start_chat(
    llm: &Arc<TasksLlm>,
    sender: &Arc<MemSender>,
    gate: Option<&Arc<Gate>>,
) -> TestGateway {
    let mut builder = GatewayBuilder::new(&tasks_config())
        .durable()
        .llm(Arc::clone(llm) as Arc<dyn LlmClient>)
        .stream(DriverStream::new(Arc::clone(llm) as Arc<dyn LlmClient>))
        .factory(FixedFactory::sender_only(
            Arc::clone(sender) as Arc<dyn ChannelSender>
        ));
    if let Some(gate) = gate {
        builder = builder.tool(gate.tool());
    }
    builder.start().await
}

/// 送到某个渠道对端的全部文本回复：Run 的最终回复（`Outbound::RunFinished`）与其余文本。
fn texts_to(sender: &MemSender, chat: &str) -> Vec<String> {
    sender
        .to_chat(chat)
        .into_iter()
        .filter_map(|message| match message.outbound {
            Outbound::Text { text } => Some(text),
            Outbound::RunFinished { summary, .. } => Some(summary),
            _ => None,
        })
        .collect()
}

async fn dm(
    gateway: &TestGateway,
    text: &str,
    key: &str,
) -> (String, komo_kernel::types::ids::RunId) {
    let ack = gateway
        .handle(inbound(
            ChannelPlatform::Telegram,
            "111",
            "111",
            text,
            key,
            true,
        ))
        .await;
    let InboundAck::Queued { session, run } = ack else {
        panic!("{ack:?}")
    };
    (session.as_str().to_owned(), run)
}

/// 一个 conversation 里的输入 submission（按 ID 序）。
async fn inputs_of(gateway: &TestGateway, conversation: ConversationId) -> Vec<SubmissionRecord> {
    core(gateway)
        .storage
        .scan_submissions(
            &SubmissionQuery {
                conversation: Some(conversation),
                order: Some(komo_kernel::durable::ScanOrder::Ascending),
                ..SubmissionQuery::default()
            },
            10_000,
            None,
        )
        .await
        .expect("读得出")
        .items
        .into_iter()
        .filter(|record| {
            matches!(
                record.kind,
                komo_kernel::durable::SubmissionKind::Input { .. }
            )
        })
        .collect()
}

async fn run_state(gateway: &TestGateway, run: &komo_kernel::types::ids::RunId) -> RunState {
    let (code, body) = gateway.get(&format!("/v1/runs/{run}")).await;
    assert_eq!(code, 200, "{body}");
    serde_json::from_str::<komo_kernel::protocol::http::RunDetail>(&body)
        .expect("run 详情")
        .summary
        .state
}

/// 请求键以 `prefix` 开头的 submission 已经出现（还没结算也算）。
async fn wait_keyed_exists(gateway: &TestGateway, prefix: &str) -> SubmissionRecord {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(found) =
            komo_gateway::service::test_support::driver_stream::submissions_keyed(gateway, prefix)
                .await
                .into_iter()
                .next()
        {
            return found;
        }
        assert!(std::time::Instant::now() < deadline, "没有 {prefix} 的输入");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 端到端：DM → home 调 `task`、回"已派出 #短号" → 任务会话以 home 的 Agent、用 `task` 给的
/// 模型跑完 → 结果**不直接**投给渠道，而是作为一条内部输入交回 home（带契约复验结论、不带模型），
/// 由 home 转告 → 带 `task_id` 接着做，沿用那个任务上一条 Run 的模型，结果照样经 home 转告
/// （§14 `task` 验收 ①④⑤⑥⑦）。
///
/// 改了断言：①"home 的回复里说了已派出"——任务很快跑完时结果可能先并进 home 还在跑的那条 run，
/// 渠道上就只有转告那一句（回执留在 home 的工具结果里），所以改查 home conversation 的工具结果里有
/// `已派出 #<任务 conversation>`；②任务 Run 的模型从请求读（`TurnRequest.model`），不再读 runs 表；
/// ③任务会话的 `origin = task:{home}` 换成 `parent_session = home`；④插话标记换成行为（见模块注释）；
/// ⑤带契约的追问"照旧排队"改成只数 submission（排队 / 并入是 runtime 的事，runtime 测试盖了）。
#[tokio::test]
async fn a_background_task_reports_back_through_home() {
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let mut gateway = start_chat(&llm, &sender, None).await;

    // ── 1. DM 进 home，home 派出任务、回一句已派出。
    let (home_session, home_run) = dm(&gateway, "帮我查一下空调", "dm-1").await;
    gateway.wait_run(&home_run).await;

    // ── 2. 任务会话：kind = task、与 home 同一个 Agent、标题写死、挂在 home 名下。
    let task = the_task(&gateway).await;
    let row = core(&gateway)
        .session(&task.session)
        .await
        .expect("任务会话行");
    assert_eq!(row.kind, komo_kernel::durable::SessionKind::Task);
    assert_eq!(row.parent_session.as_deref(), Some(home_session.as_str()));
    assert_eq!(row.title.as_deref(), Some("查空调"));
    let home_row = core(&gateway).session(&home_session).await.unwrap();
    let receipts = tool_results(&gateway, home_row.root_conversation).await;
    assert!(
        receipts
            .iter()
            .any(|(_, text)| text.contains(&format!("已派出 #{}", task.conversation))),
        "{receipts:?}"
    );

    // ── 3. 任务跑完，结果交回 home，由 home 转告；任务自己的回复不直接投给渠道。
    let report = wait_keyed(&gateway, "task-result:", Duration::from_secs(20)).await;
    assert_eq!(answer_of(&gateway, &report).await, "转告：空调已经关掉了。");
    eventually_async(|| {
        texts_to(&sender, "111")
            .iter()
            .any(|t| t == "转告：空调已经关掉了。")
    })
    .await;
    let delivered = texts_to(&sender, "111");
    assert!(
        !delivered.iter().any(|text| text == "空调已经关掉了。"),
        "任务的原话不该直接投给渠道：{delivered:?}"
    );

    // ④ 任务那一段用 `task` 给的模型；交回的那条输入按 home 自己的默认。
    let requests = llm.requests.lock().expect("请求").clone();
    let task_request = requests
        .iter()
        .find(|request| last_user_text(request).starts_with("查一下空调状态"))
        .cloned()
        .expect("任务会话发过请求");
    assert_eq!(task_request.model.model, "gpt-cheap");
    let report_request = requests
        .iter()
        .find(|request| last_user_text(request).starts_with("[后台任务"))
        .cloned()
        .expect("交回的输入到过模型");
    assert_eq!(report_request.model.model, "gpt-test");
    // ⑥ 带契约：任务的提示里有契约段，交回的输入里写着复验结论（不合契约也照样交回）。
    assert!(
        last_user_text(&task_request).contains("（结果契约）"),
        "{:?}",
        task_request.messages
    );
    // ⑦ 任务会话的能力面里没有 `task`。
    assert!(
        !task_request
            .tools
            .iter()
            .any(|tool| tool.name == komo_agent::TASK_TOOL),
        "任务会话不该看得见 task"
    );
    assert!(
        last_user_text(&report_request).contains("结果契约复验：不符合"),
        "{}",
        last_user_text(&report_request)
    );

    // ── 4. 带 task_id 接着做：进同一个任务会话，沿用上一条 Run 的模型，结果照样经 home 转告。
    llm.set_follow_target(&task.handle());
    let (_, home_run_2) = dm(&gateway, "功耗呢", "dm-2").await;
    gateway.wait_run(&home_run_2).await;
    eventually_async(|| {
        texts_to(&sender, "111")
            .iter()
            .any(|t| t == "转告：功耗 300W。")
    })
    .await;
    assert_eq!(inputs_of(&gateway, task.conversation).await.len(), 2);
    let follow_request = llm
        .requests
        .lock()
        .expect("请求")
        .iter()
        .find(|request| last_user_text(request).starts_with("再看看功耗"))
        .cloned()
        .expect("追问发过请求");
    assert_eq!(
        follow_request.model.model, "gpt-cheap",
        "⑤ 沿用上一条的模型"
    );

    // ── 5. 带契约的追问也进同一个任务会话。
    let (_, home_run_3) = dm(&gateway, "功耗带契约", "dm-3").await;
    gateway.wait_run(&home_run_3).await;
    assert_eq!(inputs_of(&gateway, task.conversation).await.len(), 3);

    gateway.stop().await;
}

/// 等一个同步条件成真（最多 20 秒）。
async fn eventually_async(done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("等了 20 秒还没有想要的投递");
}

/// 渠道的普通消息插话（§6，2026-10-23）：home 忙着跑工具（停在一道门上）时说的第二句话，
/// 门开之后并进同一条 Run，不另开一条；被并走的那条是终态 `absorbed`，渠道收到一条
/// `RunAbsorbed`（飞书据此收尾它的"处理中"卡片），回答只由宿主给一次。
#[tokio::test]
async fn a_chat_message_is_absorbed_into_a_running_home_and_its_channel_is_told() {
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gate = Gate::new();
    let mut gateway = start_chat(&llm, &sender, Some(&gate)).await;

    let (_, host) = dm(&gateway, "等门", "dm-1").await;
    gate.wait_entered(1).await;
    let (_, second) = dm(&gateway, "补一句", "dm-2").await;
    assert_eq!(
        run_state(&gateway, &host).await,
        RunState::Running,
        "第二句到的时候 home 还在跑"
    );

    gate.release();
    gateway.wait_run(&host).await;
    gateway.wait_run(&second).await;
    assert_eq!(
        run_state(&gateway, &second).await,
        RunState::Absorbed,
        "第二句并进了 home，没有另开一条"
    );

    eventually("渠道收到了并入通知", || {
        sender.to_chat("111").iter().any(|message| {
            matches!(&message.outbound, Outbound::RunAbsorbed { run, into, .. }
                    if run == &second && into == &host)
        })
    })
    .await;
    // 回答只有宿主那一次：被并走的那条自己没有回复。
    eventually("宿主答了并进来的那句", || {
        texts_to(&sender, "111")
            .iter()
            .any(|text| text == "没见过这句：补一句")
    })
    .await;
    let finished = sender
        .to_chat("111")
        .iter()
        .filter(|message| matches!(&message.outbound, Outbound::RunFinished { run, .. } if run == &second))
        .count();
    assert_eq!(finished, 0, "被并走的那条不另投回复");

    gateway.stop().await;
}

/// home 还在跑时交回的结果在一轮收尾后并进那条 Run（`run.absorbed`），不另开一条
/// （§6 插话，§14 `task` 验收 ③）：home 派出任务后停在一道门上，任务跑完、结果交回时 home
/// 还在跑；门开之后那一轮收尾，结果并进来，home 接着把它转告出去。
///
/// 改了做法：Legacy 等交回的 Run 出现再看它终态；新版交回的输入是 home 里请求键 `task-result:…` 的
/// submission，出现即可（它要等宿主收尾才结算），之后看它的 run 状态是 `absorbed`。
#[tokio::test]
async fn a_task_result_is_absorbed_into_a_running_home() {
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gate = Gate::new();
    let mut gateway = start_chat(&llm, &sender, Some(&gate)).await;

    let (_, home_run) = dm(&gateway, "派了再等", "dm-1").await;
    let _task = the_task(&gateway).await;
    let report = wait_keyed_exists(&gateway, "task-result:").await;
    assert_eq!(
        run_state(&gateway, &home_run).await,
        RunState::Running,
        "交回时 home 还在跑"
    );

    gate.release();
    gateway.wait_run(&home_run).await;
    let report_run = komo_kernel::durable::legacy_ids::run_id(report.id);
    gateway.wait_run(&report_run).await;
    assert_eq!(
        run_state(&gateway, &report_run).await,
        RunState::Absorbed,
        "交回的那条并进了 home，没有另开一条"
    );
    eventually("home 把并进来的结果转告了", || {
        texts_to(&sender, "111")
            .iter()
            .any(|text| text == "转告：热水器 60 度。")
    })
    .await;

    gateway.stop().await;
}

/// 任务正忙时的追问（无契约）并进它正在跑的那条 Run：被并走的追问是终态 `absorbed`，
/// 结果只由宿主那一次交回 home，不会为追问另交一份（§4、§6，2026-10-23）。
///
/// 改了做法：交回的请求键是 `task-result:{任务 conversation}:{首个输入}`，"只交回一次"数的是 home
/// 里这种键的 submission，并核对它的键就是宿主那条的。
#[tokio::test]
async fn a_follow_up_absorbed_into_a_busy_task_reports_back_once() {
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gate = Gate::new();
    let mut gateway = start_chat(&llm, &sender, Some(&gate)).await;

    let (_, first) = dm(&gateway, "派等门任务", "dm-1").await;
    gateway.wait_run(&first).await;
    gate.wait_entered(1).await;
    let task = the_task(&gateway).await;
    let host = inputs_of(&gateway, task.conversation).await.remove(0);
    assert_eq!(
        run_state(&gateway, &komo_kernel::durable::legacy_ids::run_id(host.id)).await,
        RunState::Running
    );

    llm.set_follow_target(&task.handle());
    let (_, follow) = dm(&gateway, "追问等门任务", "dm-2").await;
    gateway.wait_run(&follow).await;
    let task_inputs = inputs_of(&gateway, task.conversation).await;
    assert_eq!(task_inputs.len(), 2, "{task_inputs:?}");
    let follow_up = komo_kernel::durable::legacy_ids::run_id(task_inputs[1].id);

    gate.release();
    gateway
        .wait_run(&komo_kernel::durable::legacy_ids::run_id(host.id))
        .await;
    gateway.wait_run(&follow_up).await;
    assert_eq!(
        run_state(&gateway, &follow_up).await,
        RunState::Absorbed,
        "追问并进了正在跑的任务，没有另开一条"
    );
    eventually("宿主那一次把结果交回并转告", || {
        texts_to(&sender, "111")
            .iter()
            .any(|text| text == "转告：没见过这句：任务补一句")
    })
    .await;

    tokio::time::sleep(Duration::from_millis(300)).await;
    let reports = komo_gateway::service::test_support::driver_stream::submissions_keyed(
        &gateway,
        "task-result:",
    )
    .await;
    assert_eq!(reports.len(), 1, "只有宿主那一次交回：{reports:?}");
    assert_eq!(
        reports[0].request_id.as_deref(),
        Some(format!("task-result:{}:{}", task.conversation, host.id).as_str()),
        "被并走的追问不另交回"
    );

    gateway.stop().await;
}

/// 任务停在一次还没返回的调用里（`gate`）时，home 照样几秒内答完下一句——任务在自己的会话里跑，不占
/// home 的串行位。
///
/// **换了场景**（对应 Legacy 的 `home_answers_while_a_task_waits_on_a_person`）：Legacy 让任务调 `unsure`
/// 停在一条要人答的介入上（`waiting + intervention`），再证 home 不被它挡。新存储没有"等人"这个状态——
/// 结果不明的调用直接作为结果交回模型，任务不会停下来等操作者；真正会让任务"停着不动"的只剩两种：
/// 还在调用里（`gate`），或 faulted。faulted 任务的呈现（`NeedsAttention`、intervention 列表）是 W4a 的
/// 事，而 home 与任务在不同 conversation、各有各的串行位这一条与任务为什么停着无关，所以用 `gate` 证
/// 这条不变量；faulted 版的 home 不被挡由 W4a 的恢复测试带上。
#[tokio::test]
async fn home_answers_while_a_task_is_stuck_in_a_call() {
    let llm = TasksLlm::new();
    let sender = MemSender::new(ChannelPlatform::Telegram);
    let gate = Gate::new();
    let mut gateway = start_chat(&llm, &sender, Some(&gate)).await;

    let (_, home_run) = dm(&gateway, "派等门任务", "dm-1").await;
    gateway.wait_run(&home_run).await;
    gate.wait_entered(1).await;
    let task = the_task(&gateway).await;
    let host = inputs_of(&gateway, task.conversation).await.remove(0);
    assert_eq!(
        run_state(&gateway, &komo_kernel::durable::legacy_ids::run_id(host.id)).await,
        RunState::Running,
        "任务停在 gate 里"
    );

    let (_, second) = dm(&gateway, "1+1 等于几", "dm-2").await;
    gateway.wait_run(&second).await;
    assert_eq!(
        run_state(&gateway, &second).await,
        RunState::Completed,
        "home 答完了"
    );
    eventually("渠道收到了答案", || {
        texts_to(&sender, "111").iter().any(|text| text == "2")
    })
    .await;
    assert_eq!(
        run_state(&gateway, &komo_kernel::durable::legacy_ids::run_id(host.id)).await,
        RunState::Running,
        "home 答完的时候任务还停在调用里"
    );

    gate.release();
    gateway.stop().await;
}
