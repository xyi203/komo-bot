//! 长 `bash` 命令自动转后台（`docs/monitor.md` §8）：对话会话里跑过 `detach_after_secs`
//! 还没完的命令，这次调用立刻带着「已转后台 #id」收尾，进程留在 Gateway 手里，结束时交回一条
//! `bash-done:` 消息；`monitor_stop` 停得掉它；超时在后台照样生效；Gateway 重启时没交回的
//! 那条补一句"结果不明"。
//!
//! 真数据目录、真 `service::start`、真子进程；模型是按输入分流的脚本。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use komo_gateway::service::test_support::harness::{Gw, Home, call_round, text_round};
use komo_kernel::traits::{LlmClient, TurnDriver};
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::{LlmError, Round, RoundInput, TurnRequest};

mod durable;

fn config(detach_after_secs: u64) -> String {
    format!(
        r#"
default_agent = "assistant"

[agents.assistant]
instructions = "你是助手。"
tools = ["monitor_stop", "bash"]

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

[execution]
detach_after_secs = {detach_after_secs}
"#
    )
}

/// 按这一段最后一条用户输入分流：
///
/// - `跑：<命令>` / `限时跑：<命令>` → 调 `bash`（后者 `timeout_secs = 3`），再念出工具结果；
/// - `停：<短号>` → 调 `monitor_stop`；
/// - `[后台命令 …` → 记下，回一句"收到"；
/// - 其他 → 原样回一句。
struct DetachLlm {
    inputs: Mutex<Vec<String>>,
}

impl DetachLlm {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inputs: Mutex::new(Vec::new()),
        })
    }

    fn handbacks(&self) -> Vec<String> {
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
impl LlmClient for DetachLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        let input = last_user_text(&req);
        let script = if input.starts_with("[后台命令") {
            self.inputs.lock().unwrap().push(input);
            Script::Reply("收到".into())
        } else if let Some(command) = input.strip_prefix("限时跑：") {
            Script::Call(
                "bash",
                serde_json::json!({ "command": command, "timeout_secs": 3 }),
            )
        } else if let Some(command) = input.strip_prefix("跑：") {
            Script::Call("bash", serde_json::json!({ "command": command }))
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

/// 发一句话，等这一轮收尾，返回模型念出来的工具结果与耗时。
async fn say(gateway: &Gw, session: &SessionId, key: &str, text: &str) -> (String, Duration) {
    let started = Instant::now();
    let submitted = gateway.submit(session, key, text).await;
    let detail = gateway.wait_terminal(&submitted.run).await;
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    (detail.final_message.unwrap_or_default(), started.elapsed())
}

fn short_of(reply: &str) -> String {
    reply
        .split("已转后台 #")
        .nth(1)
        .and_then(|rest| rest.get(..4))
        .unwrap_or_else(|| panic!("回执里没有短号：{reply}"))
        .to_string()
}

async fn runs_of(gateway: &Gw, session: &SessionId) -> Vec<komo_store::repos::runs::RunRecord> {
    komo_store::repos::runs::list_for_session(&gateway.state().db, session)
        .await
        .expect("读得出会话的 Run")
}

async fn wait_run_keyed(gateway: &Gw, session: &SessionId, prefix: &str, within: Duration) {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
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

fn records(home: &Home) -> Vec<std::path::PathBuf> {
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

/// 一个每 0.2 秒刷新标记文件的循环：标记不再被刷新，就是进程组没了。
fn touching(marker: &std::path::Path) -> String {
    format!("while true; do touch {}; sleep 0.2; done", marker.display())
}

async fn assert_stopped(marker: &std::path::Path) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = std::fs::remove_file(marker);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!marker.exists(), "进程组还在跑");
}

/// 端到端：跑过阈值的命令这次调用立刻返回「已转后台」，进程接着跑完，结束时交回带退出码、
/// 输出尾部与完整输出引用的消息；那次调用已经收口（Run 已 Completed），重启也不会再把它
/// 当没结果的调用。
#[tokio::test]
async fn a_long_command_detaches_and_its_end_comes_back_as_an_input() {
    let home = Home::with_config(&config(1));
    let llm = DetachLlm::new();
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;

    let (reply, took) = say(
        &gateway,
        &session,
        "run",
        "跑：echo before; sleep 4; echo after-line; exit 4",
    )
    .await;
    assert!(reply.contains("已转后台 #"), "{reply}");
    assert!(reply.contains("before"), "到现在的输出也给了：{reply}");
    assert!(took < Duration::from_millis(3500), "没等命令跑完：{took:?}");
    let short = short_of(&reply);

    wait_run_keyed(&gateway, &session, "bash-done:", Duration::from_secs(20)).await;
    let ends = llm.handbacks();
    assert_eq!(ends.len(), 1, "{ends:#?}");
    let end = &ends[0];
    assert!(
        end.starts_with(&format!("[后台命令 #{short}「echo before; sleep 4")),
        "{end}"
    );
    assert!(end.contains("退出码 4"), "{end}");
    assert!(end.contains("after-line"), "输出尾部：{end}");
    assert!(
        end.contains("artifact://") && end.contains("/background"),
        "{end}"
    );
    assert!(records(&home).is_empty(), "收尾后登记该删掉");

    // 完整输出引用读得回：转后台之后的输出落在 background.log。
    let log = std::fs::read_dir(home.path().join("sessions"))
        .unwrap()
        .flatten()
        .flat_map(|entry| walk(&entry.path()))
        .find(|path| path.file_name().is_some_and(|n| n == "background.log"))
        .expect("有 background.log");
    assert!(std::fs::read_to_string(log).unwrap().contains("after-line"));

    // 那次调用早已收口：重启后没有"没结果的调用"，原 Run 仍是 Completed。
    gateway.stop().await;
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let runs = runs_of(&gateway, &session).await;
    let original = runs
        .iter()
        .find(|run| run.request_key.as_str() == "run")
        .expect("原来的 Run");
    assert_eq!(original.state, RunState::Completed);
    assert_eq!(llm.handbacks().len(), 1, "不会再交回第二条");
    gateway.stop().await;
}

/// 端到端：阈值远没到，用户在 TUI 里插话——前台命令跑满宽限就转后台，宿主不再等它跑完，
/// 回执说清是插话；命令照样在后台跑完并交回。
#[tokio::test]
async fn an_interjection_moves_the_foreground_command_to_the_background() {
    let home = Home::with_config(&config(60));
    let llm = DetachLlm::new();
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;

    // 一共 15 秒，拆成三段：对话会话拒绝字面量 ≥10 秒的单个 `sleep`（`detach::long_sleep`）。
    let started = Instant::now();
    let host = gateway
        .submit(
            &session,
            "host",
            "跑：echo before; for i in 1 2 3; do sleep 5; done; echo after",
        )
        .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !home
            .event_types(&session)
            .iter()
            .any(|kind| kind == "tool.started")
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "命令没开跑：{}",
            std::fs::read_to_string(home.events_path(&session)).unwrap_or_default()
        )
    });
    let (code, body) = gateway
        .post(
            &format!("/v1/sessions/{session}/runs"),
            serde_json::json!({ "request_key": "aside", "text": "顺便说一句", "interject": true }),
        )
        .await;
    assert_eq!(code, 200, "{body}");

    let detail = gateway.wait_terminal(&host.run).await;
    let took = started.elapsed();
    assert_eq!(detail.summary.state, RunState::Completed, "{detail:?}");
    assert!(took < Duration::from_secs(10), "没等 15 秒的命令：{took:?}");
    let log = std::fs::read_to_string(home.events_path(&session)).unwrap();
    assert!(log.contains("用户插话了"), "回执说清是插话");

    wait_run_keyed(&gateway, &session, "bash-done:", Duration::from_secs(30)).await;
    let ends = llm.handbacks();
    assert_eq!(ends.len(), 1, "{ends:#?}");
    assert!(ends[0].contains("退出码 0"), "{}", ends[0]);
    gateway.stop().await;
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// `detach_after_secs = 0`：不转，命令在这次调用里跑完。
#[tokio::test]
async fn zero_means_the_command_is_waited_for_in_place() {
    let home = Home::with_config(&config(0));
    let llm = DetachLlm::new();
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;

    let (reply, took) = say(&gateway, &session, "run", "跑：sleep 2; echo finished").await;
    assert!(
        reply.contains("exit=0") && reply.contains("finished"),
        "{reply}"
    );
    assert!(!reply.contains("已转后台"), "{reply}");
    assert!(took >= Duration::from_secs(2), "{took:?}");
    assert!(llm.handbacks().is_empty());

    gateway.stop().await;
}

/// 原来的 `timeout_secs` 从调用开始算：到点在后台终止进程组，结束消息报超时。
#[tokio::test]
async fn the_original_timeout_still_applies_in_the_background() {
    let home = Home::with_config(&config(1));
    let llm = DetachLlm::new();
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;

    let marker = home.path().join("still-running");
    let (reply, _) = say(
        &gateway,
        &session,
        "run",
        &format!("限时跑：{}", touching(&marker)),
    )
    .await;
    assert!(reply.contains("已转后台 #"), "{reply}");

    wait_run_keyed(&gateway, &session, "bash-done:", Duration::from_secs(20)).await;
    let end = llm.handbacks().pop().unwrap_or_default();
    assert!(end.contains("超时"), "{end}");
    assert_stopped(&marker).await;
    assert!(records(&home).is_empty());

    gateway.stop().await;
}

/// `monitor_stop #id` 停得掉转后台的命令，结束消息说明它是被停的。
#[tokio::test]
async fn monitor_stop_stops_a_detached_command_and_the_end_says_so() {
    let home = Home::with_config(&config(1));
    let llm = DetachLlm::new();
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;

    let marker = home.path().join("still-running");
    let (reply, _) = say(
        &gateway,
        &session,
        "run",
        &format!("跑：{}", touching(&marker)),
    )
    .await;
    let short = short_of(&reply);

    let (stopped, _) = say(&gateway, &session, "stop", &format!("停：{short}")).await;
    assert!(stopped.contains(&format!("#{short}")), "{stopped}");

    wait_run_keyed(&gateway, &session, "bash-done:", Duration::from_secs(20)).await;
    let end = llm.handbacks().pop().unwrap_or_default();
    assert!(end.contains("已被 monitor_stop 停止"), "{end}");
    assert_stopped(&marker).await;
    assert!(records(&home).is_empty());

    gateway.stop().await;
}

/// 重启时还没交回的转后台命令：进程随 Gateway 没了，下一代给会话一条"结果不明"。
#[tokio::test]
async fn a_restart_reports_an_unfinished_detached_command_as_unknown() {
    let home = Home::with_config(&config(1));
    let llm = DetachLlm::new();
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;

    let marker = home.path().join("still-running");
    let (reply, _) = say(
        &gateway,
        &session,
        "run",
        &format!("跑：{}", touching(&marker)),
    )
    .await;
    let short = short_of(&reply);
    gateway.stop().await;
    assert_eq!(records(&home).len(), 1, "停机时登记留给下一代");
    assert_stopped(&marker).await;

    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    wait_run_keyed(&gateway, &session, "bash-done:", Duration::from_secs(20)).await;
    let end = llm.handbacks().pop().unwrap_or_default();
    assert!(end.starts_with(&format!("[后台命令 #{short}「")), "{end}");
    assert!(
        end.contains("Gateway 重启，后台命令中断，结果不明"),
        "{end}"
    );
    assert!(records(&home).is_empty(), "交回之后登记该删掉");
    let original = runs_of(&gateway, &session)
        .await
        .into_iter()
        .find(|run| run.request_key.as_str() == "run")
        .expect("原来的 Run");
    assert_eq!(
        original.state,
        RunState::Completed,
        "那次调用已收口，不是 uncertain"
    );

    gateway.stop().await;
}

/// 对话会话里 10 秒以上的字面量 sleep 在 `prepare` 就被拒绝，提示用 monitor。
#[tokio::test]
async fn a_long_sleep_in_a_conversation_is_refused_with_the_monitor_hint() {
    let home = Home::with_config(&config(120));
    let llm = DetachLlm::new();
    let gateway = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gateway.open_session().await;

    let (reply, _) = say(
        &gateway,
        &session,
        "loop",
        "跑：for i in $(seq 1 30); do echo $i; sleep 20; done",
    )
    .await;
    assert!(
        reply.contains("monitor") && reply.contains("sleep 20"),
        "{reply}"
    );
    let (reply, _) = say(&gateway, &session, "minute", "跑：sleep 1m").await;
    assert!(reply.contains("monitor"), "{reply}");
    let (reply, _) = say(&gateway, &session, "short", "跑：sleep 2 && echo up").await;
    assert!(reply.contains("up"), "短 sleep 放行：{reply}");

    gateway.stop().await;
}

/// 后台命令在宿主仍请求模型时结束：完成通知必须在下一个工具边界并入原 Run，
/// 不能排到整条宿主 Run 之后，让它继续按「仍在后台」的旧状态调度。
#[tokio::test]
async fn a_detached_completion_interjects_into_the_still_running_host() {
    use tokio::sync::Notify;

    struct BusyLlm {
        command: String,
        waiting: Arc<Notify>,
        proceed: Arc<Notify>,
    }
    struct BusyDriver {
        command: String,
        waiting: Arc<Notify>,
        proceed: Arc<Notify>,
        step: u32,
    }
    #[async_trait::async_trait]
    impl LlmClient for BusyLlm {
        async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
            if last_user_text(&req).starts_with("[后台命令") {
                return Ok(Box::new(ScriptDriver {
                    script: Script::Reply("已读到后台结束通知".into()),
                }));
            }
            Ok(Box::new(BusyDriver {
                command: self.command.clone(),
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
                    "pc-background",
                    "bash",
                    serde_json::json!({"command": self.command}),
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
                _ => text_round(3, "仍未读到后台结束通知"),
            }
        }
        fn usage(&self) -> TokenUsage {
            TokenUsage::default()
        }
    }

    let home = Home::with_config(&config(1));
    let release = home.path().join("release-background");
    let waiting = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let llm = Arc::new(BusyLlm {
        command: format!(
            "while [ ! -e '{}' ]; do sleep 0.05; done; echo finished; exit 1",
            release.display()
        ),
        waiting: waiting.clone(),
        proceed: proceed.clone(),
    });
    let gateway = home.start(llm).await;
    let session = gateway.open_session().await;
    let host = gateway
        .submit(&session, "busy-host", "启动后台命令后继续处理")
        .await;
    tokio::time::timeout(Duration::from_secs(10), waiting.notified())
        .await
        .expect("模型已拿到转后台回执");
    std::fs::write(&release, "release").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if home.events(&session).iter().any(|event| {
                matches!(&event.payload, komo_kernel::events::EventPayload::RunAccepted(body) if body.request_key.as_str().starts_with("bash-done:"))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("结束通知已受理");
    proceed.notify_one();
    let detail = gateway.wait_terminal(&host.run).await;
    wait_run_keyed(&gateway, &session, "bash-done:", Duration::from_secs(10)).await;
    let runs = runs_of(&gateway, &session).await;
    let end_state = runs
        .iter()
        .find(|run| run.request_key.as_str().starts_with("bash-done:"))
        .unwrap()
        .state;
    gateway.stop().await;

    assert_eq!(detail.final_message.as_deref(), Some("已读到后台结束通知"));
    assert_eq!(
        end_state,
        RunState::Absorbed,
        "完成通知应并进宿主，而非另排一轮"
    );
}
