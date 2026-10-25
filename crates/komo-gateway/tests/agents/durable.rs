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
use komo_kernel::durable::{ConversationId, Message};
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
