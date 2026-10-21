//! 监听（`docs/monitor.md`）：`monitor` 挂上的脚本每出一行，就作为一条内部输入交回挂它的
//! 会话；脚本收尾时交回一条结束消息；`monitor_stop` 停掉它；Gateway 重启时监听中断并
//! 告诉会话，不自动重挂——除非挂的时候声明了 `resume_after_restart`。
//!
//! 真数据目录、真 `service::start`、真子进程；模型是按输入分流的脚本。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use komo_gateway::service::test_support::harness::{Gw, Home, call_round, text_round};
use komo_kernel::traits::{LlmClient, TurnDriver};
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::{LlmError, Round, RoundInput, TurnRequest};

fn config() -> String {
    r#"
default_agent = "assistant"

[agents.assistant]
instructions = "你是助手。"
tools = ["monitor", "monitor_stop", "bash"]

[model.main]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[models]
default = "main"

[memory]
enabled = false
"#
    .to_string()
}

/// 按这一段最后一条用户输入分流：
///
/// - `挂：<命令>` → 调 `monitor`，再把工具结果原样念出来；
/// - `停：<短号>` → 调 `monitor_stop`；
/// - `[监听 …` → 记下这条输入，回一句"收到"（`event_delay` 模拟模型处理得慢）；
/// - 其他 → 原样回一句。
struct MonitorLlm {
    inputs: Mutex<Vec<String>>,
    event_delay: Duration,
}

impl MonitorLlm {
    fn new(event_delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            inputs: Mutex::new(Vec::new()),
            event_delay,
        })
    }

    fn events(&self) -> Vec<String> {
        self.inputs.lock().unwrap().clone()
    }
}

fn last_user_text(req: &TurnRequest) -> String {
    req.messages
        .iter()
        .rev()
        .find(|message| message.role == komo_kernel::types::turn::Role::User)
        .and_then(|message| message.text.clone())
        .unwrap_or_default()
}

#[async_trait::async_trait]
impl LlmClient for MonitorLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        let input = last_user_text(&req);
        let script = if input.starts_with("[监听") {
            self.inputs.lock().unwrap().push(input);
            tokio::time::sleep(self.event_delay).await;
            Script::Reply("收到".into())
        } else if let Some(command) = input.strip_prefix("续挂：") {
            Script::Call(
                "monitor",
                serde_json::json!({
                    "command": command,
                    "title": "测试监听",
                    "timeout_minutes": 2,
                    "resume_after_restart": true
                }),
            )
        } else if let Some(command) = input.strip_prefix("挂：") {
            Script::Call(
                "monitor",
                serde_json::json!({ "command": command, "title": "测试监听", "timeout_minutes": 2 }),
            )
        } else if let Some(id) = input.strip_prefix("停：") {
            Script::Call("monitor_stop", serde_json::json!({ "monitor_id": id }))
        } else {
            Script::Reply(input)
        };
        Ok(Box::new(ScriptDriver { script }))
    }
}

enum Script {
    Reply(String),
    Call(&'static str, serde_json::Value),
}

struct ScriptDriver {
    script: Script,
}

#[async_trait::async_trait]
impl TurnDriver for ScriptDriver {
    async fn next(&mut self, input: RoundInput) -> Result<Round, LlmError> {
        match (&self.script, input) {
            (Script::Reply(text), _) => text_round(1, text),
            (Script::Call(tool, args), RoundInput::First) => {
                call_round(1, &format!("pc-{tool}"), tool, args.clone())
            }
            (Script::Call(..), RoundInput::ToolResults { results, .. }) => {
                let text = results
                    .first()
                    .map(|result| result.content.clone())
                    .unwrap_or_default();
                text_round(2, &text)
            }
        }
    }

    fn usage(&self) -> TokenUsage {
        TokenUsage::default()
    }
}

/// 挂一个监听，返回会话与短号（从模型念出来的工具结果里抠）。
async fn arm(gateway: &Gw, command: &str) -> (SessionId, String) {
    arm_with(gateway, "挂", command).await
}

async fn arm_with(gateway: &Gw, verb: &str, command: &str) -> (SessionId, String) {
    let session = gateway.open_session().await;
    let submitted = gateway
        .submit(&session, "arm", &format!("{verb}：{command}"))
        .await;
    let detail = gateway.wait_terminal(&submitted.run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    let reply = detail.final_message.clone().unwrap_or_default();
    let short = reply
        .split("已开始监听 #")
        .nth(1)
        .and_then(|rest| rest.get(..4))
        .unwrap_or_else(|| panic!("回执里没有短号：{reply}"))
        .to_string();
    (session, short)
}

async fn runs_of(gateway: &Gw, session: &SessionId) -> Vec<komo_store::repos::runs::RunRecord> {
    komo_store::repos::runs::list_for_session(&gateway.state().db, session)
        .await
        .expect("读得出会话的 Run")
}

/// 等这个会话出现一条请求键以 `prefix` 开头、已经收尾的 Run。
async fn wait_run_keyed(gateway: &Gw, session: &SessionId, prefix: &str, within: Duration) {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline {
        if runs_of(gateway, session)
            .await
            .iter()
            .any(|run| run.request_key.as_str().starts_with(prefix) && run.state.is_terminal())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("等了 {within:?} 也没有请求键以 {prefix} 开头的 Run");
}

fn monitor_records(home: &Home) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(home.path().join("runtime/monitors"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .collect()
        })
        .unwrap_or_default()
}

/// 端到端：脚本的行作为 `[监听 #短号…]` 输入交回同一个会话，脚本退出后交回一条带退出码
/// 的结束消息，登记随之删掉。
#[tokio::test]
async fn lines_come_back_as_inputs_and_the_exit_is_reported() {
    let home = Home::with_config(&config());
    let llm = MonitorLlm::new(Duration::ZERO);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let (session, short) = arm(
        &gateway,
        "for i in 1 2 3; do echo tick $i; sleep 0.2; done; echo boom >&2; exit 3",
    )
    .await;
    wait_run_keyed(&gateway, &session, "monitor-end:", Duration::from_secs(20)).await;

    let events = llm.events();
    let joined = events.join("\n");
    for tick in ["tick 1", "tick 2", "tick 3"] {
        assert!(joined.contains(tick), "{tick} 没有交回：{events:#?}");
    }
    assert!(
        events
            .iter()
            .all(|event| event.starts_with(&format!("[监听 #{short}「测试监听」"))),
        "{events:#?}"
    );
    let end = events.last().unwrap();
    assert!(end.contains("exit=3"), "{end}");
    assert!(end.contains("stderr 末尾：\nboom"), "{end}");
    assert!(
        !joined.contains("boom\n（这是监听脚本"),
        "stderr 不该当成事件：{events:#?}"
    );
    assert!(monitor_records(&home).is_empty(), "收尾后登记该删掉");

    gateway.stop().await;
}

/// 模型处理得慢时，脚本这段时间出的行合进下一条，而不是按批排一串 Run。
#[tokio::test]
async fn lines_printed_while_an_event_is_being_handled_are_merged_into_the_next() {
    let home = Home::with_config(&config());
    let llm = MonitorLlm::new(Duration::from_secs(3));
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let (session, _) = arm(
        &gateway,
        "for i in $(seq 1 30); do echo line $i; sleep 0.2; done",
    )
    .await;
    wait_run_keyed(&gateway, &session, "monitor-end:", Duration::from_secs(40)).await;

    let events = llm.events();
    let joined = events.join("\n");
    for i in 1..=30 {
        assert!(
            joined.lines().any(|line| line == format!("line {i}")),
            "line {i} 丢了：{events:#?}"
        );
    }
    let event_runs = runs_of(&gateway, &session)
        .await
        .into_iter()
        .filter(|run| run.request_key.as_str().starts_with("monitor:"))
        .count();
    // 6 秒里出 30 行，每条事件要处理 3 秒：同时只有一条在飞时是两条左右（其余进结束
    // 消息）；不等上一条收尾、每秒交一条的话是六条。
    assert!(
        event_runs <= 3,
        "上一条没收尾时出的行该合进下一条：{event_runs} 条 {events:#?}"
    );

    gateway.stop().await;
}

/// `monitor_stop` 终止进程组，之后不再交回结束消息；给错短号时列出还在跑的那些。
#[tokio::test]
async fn stopping_kills_the_script_and_reports_nothing_more() {
    let home = Home::with_config(&config());
    let llm = MonitorLlm::new(Duration::ZERO);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let marker = home.path().join("still-running");
    let (session, short) = arm(
        &gateway,
        &format!("while true; do touch {}; sleep 0.2; done", marker.display()),
    )
    .await;

    let wrong = gateway.submit(&session, "stop-wrong", "停：beef").await;
    let detail = gateway.wait_terminal(&wrong.run).await;
    let reply = detail.final_message.unwrap_or_default();
    assert!(reply.contains(&format!("#{short}「测试监听」")), "{reply}");

    let stop = gateway
        .submit(&session, "stop", &format!("停：{short}"))
        .await;
    let detail = gateway.wait_terminal(&stop.run).await;
    let reply = detail.final_message.unwrap_or_default();
    assert!(reply.contains(&format!("已停止监听 #{short}")), "{reply}");

    // 进程组没了：标记文件不再被刷新。
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = std::fs::remove_file(&marker);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!marker.exists(), "停掉之后脚本还在跑");

    let again = gateway
        .submit(&session, "stop-again", &format!("停：{short}"))
        .await;
    let detail = gateway.wait_terminal(&again.run).await;
    let reply = detail.final_message.unwrap_or_default();
    assert!(reply.contains("已经停掉了"), "再停一次是幂等的：{reply}");

    assert!(
        !runs_of(&gateway, &session)
            .await
            .iter()
            .any(|run| run.request_key.as_str().starts_with("monitor-end:")),
        "模型自己停的，不该再交回结束消息"
    );
    assert!(monitor_records(&home).is_empty());

    gateway.stop().await;
}

/// 脚本自己退出之后才调 `monitor_stop`（处理上一条事件的 Run 与脚本收尾竞态）：答“已经结束了”
/// 和怎么结束的，不算失败，也不说它“不在跑”。起因是 session `01a153c3` seq 3578。
#[tokio::test]
async fn stopping_a_monitor_that_already_exited_says_how_it_ended() {
    let home = Home::with_config(&config());
    let llm = MonitorLlm::new(Duration::ZERO);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let (session, short) = arm(&gateway, "sleep 0.5; exit 0").await;
    wait_run_keyed(&gateway, &session, "monitor-end:", Duration::from_secs(20)).await;

    let stop = gateway
        .submit(&session, "stop-late", &format!("停：{short}"))
        .await;
    let detail = gateway.wait_terminal(&stop.run).await;
    let reply = detail.final_message.unwrap_or_default();
    assert!(
        reply.contains(&format!(
            "不用停了：监听 #{short}「测试监听」已结束：脚本退出（exit=0）"
        )),
        "{reply}"
    );
    assert!(!reply.contains("停不了"), "{reply}");

    // 别的会话用同一个短号停不到它。
    let other = gateway.open_session().await;
    let stop = gateway
        .submit(&other, "stop-other", &format!("停：{short}"))
        .await;
    let detail = gateway.wait_terminal(&stop.run).await;
    let reply = detail.final_message.unwrap_or_default();
    assert!(reply.contains("停不了"), "{reply}");

    gateway.stop().await;
}

/// 停机终止监听但留下登记；下一代启动时告诉会话"中断了"，脚本没有被重新起来。
#[tokio::test]
async fn a_restart_interrupts_the_monitor_and_tells_the_session() {
    let home = Home::with_config(&config());
    let llm = MonitorLlm::new(Duration::ZERO);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let marker = home.path().join("still-running");
    let (session, short) = arm(
        &gateway,
        &format!("while true; do touch {}; sleep 0.2; done", marker.display()),
    )
    .await;
    gateway.stop().await;
    assert_eq!(monitor_records(&home).len(), 1, "停机时登记留给下一代");

    let _ = std::fs::remove_file(&marker);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!marker.exists(), "停机之后脚本还在跑");

    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    wait_run_keyed(&gateway, &session, "monitor-end:", Duration::from_secs(20)).await;
    let end = llm.events().last().cloned().unwrap_or_default();
    assert!(
        end.starts_with(&format!("[监听 #{short}「测试监听」已中断")),
        "{end}"
    );
    assert!(monitor_records(&home).is_empty(), "交回之后登记该删掉");

    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!marker.exists(), "重启不该把脚本重新起来");

    gateway.stop().await;
}

/// 声明了 `resume_after_restart` 的监听：重启后按原短号重新起来，不交回"中断了"，下一条事件
/// 注明中间断过；重启前后的事件都交回了（请求键不撞）。
#[tokio::test]
async fn a_resumable_monitor_is_relaunched_after_a_restart_under_the_same_short_id() {
    let home = Home::with_config(&config());
    let llm = MonitorLlm::new(Duration::ZERO);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let marker = home.path().join("still-running");
    let (session, short) = arm_with(
        &gateway,
        "续挂",
        &format!(
            "echo started; while true; do touch {}; sleep 0.2; done",
            marker.display()
        ),
    )
    .await;
    wait_run_keyed(&gateway, &session, "monitor:", Duration::from_secs(20)).await;
    gateway.stop().await;
    assert_eq!(monitor_records(&home).len(), 1, "停机时登记留给下一代");
    let _ = std::fs::remove_file(&marker);

    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while llm.events().len() < 2 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let events = llm.events();
    assert_eq!(events.len(), 2, "重启前后各一条 started：{events:#?}");
    assert!(
        events
            .iter()
            .all(|e| e.starts_with(&format!("[监听 #{short}「测试监听」]"))),
        "{events:#?}"
    );
    assert!(!events[0].contains("Gateway 重启过"), "{events:#?}");
    assert!(events[1].contains("Gateway 重启过"), "{events:#?}");
    assert!(marker.exists(), "重挂之后脚本该在跑");
    assert!(
        !runs_of(&gateway, &session)
            .await
            .iter()
            .any(|run| run.request_key.as_str().starts_with("monitor-end:")),
        "重挂了就不该交回中断"
    );
    assert_eq!(monitor_records(&home).len(), 1, "还在跑，登记留着");

    gateway.stop().await;
}

/// 重启时时限已经过了的可重挂监听：不重起，交回一条"时限已到"。
#[tokio::test]
async fn a_resumable_monitor_past_its_deadline_is_not_relaunched() {
    let home = Home::with_config(&config());
    let llm = MonitorLlm::new(Duration::ZERO);
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;

    let marker = home.path().join("still-running");
    let (session, short) = arm_with(
        &gateway,
        "续挂",
        &format!("while true; do touch {}; sleep 0.2; done", marker.display()),
    )
    .await;
    gateway.stop().await;
    // 把登记里的截止时刻改到过去。
    let record = monitor_records(&home).pop().expect("有登记");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record).unwrap()).unwrap();
    value["resume"]["expires_at"] = serde_json::json!("2020-01-01T00:00:00Z");
    std::fs::write(&record, value.to_string()).unwrap();
    let _ = std::fs::remove_file(&marker);

    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    wait_run_keyed(&gateway, &session, "monitor-end:", Duration::from_secs(20)).await;
    let end = llm.events().last().cloned().unwrap_or_default();
    assert!(
        end.starts_with(&format!("[监听 #{short}「测试监听」已结束")),
        "{end}"
    );
    assert!(end.contains("时限已经到了"), "{end}");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!marker.exists(), "过了时限不该重起");
    assert!(monitor_records(&home).is_empty());

    gateway.stop().await;
}

/// 监听事件在宿主仍在跑时到达：必须在下一个工具边界并入原 Run，不能排到整条宿主 Run
/// 之后（session `01a1588d` seq 177：结束消息排了 46 秒）。
#[tokio::test]
async fn a_monitor_event_interjects_into_the_still_running_host() {
    use tokio::sync::Notify;

    struct BusyLlm {
        waiting: Arc<Notify>,
        proceed: Arc<Notify>,
    }
    struct BusyDriver {
        waiting: Arc<Notify>,
        proceed: Arc<Notify>,
        step: u32,
    }
    #[async_trait::async_trait]
    impl LlmClient for BusyLlm {
        async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
            if last_user_text(&req).starts_with("[监听") {
                return Ok(Box::new(ScriptDriver {
                    script: Script::Reply("已读到监听事件".into()),
                }));
            }
            Ok(Box::new(BusyDriver {
                waiting: self.waiting.clone(),
                proceed: self.proceed.clone(),
                step: 0,
            }))
        }
    }
    #[async_trait::async_trait]
    impl TurnDriver for BusyDriver {
        async fn next(&mut self, _input: RoundInput) -> Result<Round, LlmError> {
            let step = self.step;
            self.step += 1;
            match step {
                0 => call_round(
                    1,
                    "pc-monitor",
                    "monitor",
                    serde_json::json!({
                        "command": "echo tick; while true; do sleep 1; done",
                        "title": "测试监听",
                        "timeout_minutes": 2
                    }),
                ),
                1 => {
                    self.waiting.notify_one();
                    self.proceed.notified().await;
                    call_round(
                        2,
                        "pc-boundary",
                        "bash",
                        serde_json::json!({"command": "echo checkpoint"}),
                    )
                }
                _ => text_round(3, "仍未读到监听事件"),
            }
        }
        fn usage(&self) -> TokenUsage {
            TokenUsage::default()
        }
    }

    let home = Home::with_config(&config());
    let waiting = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let llm = Arc::new(BusyLlm {
        waiting: waiting.clone(),
        proceed: proceed.clone(),
    });
    let gateway = home.start(llm).await;
    let session = gateway.open_session().await;
    let host = gateway
        .submit(&session, "busy-host", "挂上监听后继续处理")
        .await;
    tokio::time::timeout(Duration::from_secs(10), waiting.notified())
        .await
        .expect("模型已拿到监听回执");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if home.events(&session).iter().any(|event| {
                matches!(&event.payload, komo_kernel::events::EventPayload::RunAccepted(body) if body.request_key.as_str().starts_with("monitor:"))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("监听事件已受理");
    proceed.notify_one();
    let detail = gateway.wait_terminal(&host.run).await;
    wait_run_keyed(&gateway, &session, "monitor:", Duration::from_secs(10)).await;
    let event_state = runs_of(&gateway, &session)
        .await
        .iter()
        .find(|run| run.request_key.as_str().starts_with("monitor:"))
        .unwrap()
        .state;
    gateway.stop().await;

    assert_eq!(detail.final_message.as_deref(), Some("已读到监听事件"));
    assert_eq!(
        event_state,
        RunState::Absorbed,
        "监听事件应并进宿主，而非另排一轮"
    );
}
