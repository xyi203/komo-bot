//! 长 `bash` 命令自动转后台在 Durable 存储上（`docs/storage-durable-d5.md` W2c；
//! `docs/monitor.md` §8）：同样 8 个行为，换成新 harness。转后台的进程由 gateway 的
//! `GatewayMonitorRunner` 接手并在 `komo.monitors` 里登记（`id` = 那次 `bash` 调用的工具任务 ID，
//! 句柄 `#id`），结束时 `Harness::finish_monitor`：登记 `ended` 与结束消息（请求键 `monitor-end:{id}`
//! 的内部 steer 输入）同一次提交。测试名与 Legacy 版本（`main.rs`）一一对应，输入直接走
//! `DurableCore::submit_input`；模型替身是同一批脚本经 `DriverStream` 演成 `ModelStream`。
//!
//! 与 Legacy 不同的断言各有理由，写在对应测试的注释里。共同的：句柄是工具任务 ID（十进制），结束消息
//! 的请求键从 `bash-done:{call}` 变成 `monitor-end:{id}`，"登记文件删掉"变成"登记里 `ended`"。

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use komo_gateway::service::test_support::driver_stream::{
    DriverStream, core, open_session, reply_of, say, submissions_keyed, submit, wait_keyed,
};
use komo_gateway::service::test_support::harness::{GatewayBuilder, TestGateway};
use komo_kernel::durable::ConversationId;
use komo_kernel::durable::storage::EntryQuery;
use komo_kernel::traits::LlmClient;
use komo_runtime::durable::harness::orchestration::monitor::{self, MonitorRecord, MonitorState};
use komo_runtime::durable::harness::{Harness, SlotStatus, WhenBusy};

use super::{DetachLlm, Script, ScriptDriver, config, last_user_text};

async fn start(llm: &Arc<DetachLlm>, detach_after_secs: u64, at: Option<&Path>) -> TestGateway {
    let mut builder = GatewayBuilder::new(&config(detach_after_secs))
        .durable()
        .llm(Arc::clone(llm) as Arc<dyn LlmClient>)
        .stream(DriverStream::new(Arc::clone(llm) as Arc<dyn LlmClient>));
    if let Some(home) = at {
        builder = builder.at(home);
    }
    builder.start().await
}

/// 发一句话，等这一轮收尾，返回模型念出来的工具结果与耗时。
async fn timed_say(
    gateway: &TestGateway,
    session: &str,
    key: &str,
    text: &str,
) -> (String, Duration) {
    let started = Instant::now();
    let reply = say(gateway, session, key, text).await;
    (reply, started.elapsed())
}

/// 回执里的句柄：`已转后台 #<id>：…`。
fn handle_of(reply: &str) -> String {
    reply
        .split("已转后台 #")
        .nth(1)
        .map(|rest| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| panic!("回执里没有句柄：{reply}"))
}

async fn registry(gateway: &TestGateway) -> Vec<MonitorRecord> {
    monitor::list(&*core(gateway).storage)
        .await
        .expect("读得出监听登记")
}

fn ended_reason(record: &MonitorRecord) -> Option<&str> {
    match &record.state {
        MonitorState::Ended { reason, .. } => Some(reason),
        _ => None,
    }
}

/// 一个每 0.2 秒刷新标记文件的循环：标记不再被刷新，就是进程组没了。
fn touching(marker: &Path) -> String {
    format!("while true; do touch {}; sleep 0.2; done", marker.display())
}

async fn assert_stopped(marker: &Path) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = std::fs::remove_file(marker);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!marker.exists(), "进程组还在跑");
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
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

/// 这个 conversation 里全部 entry 的 JSON 文本（找回执里的某句话用）。
async fn entries_text(gateway: &TestGateway, conversation: ConversationId) -> String {
    let page = core(gateway)
        .storage
        .scan_entries(&EntryQuery::new(conversation), 10_000, None)
        .await
        .expect("读得出");
    page.items
        .iter()
        .map(|stored| serde_json::to_string(stored).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 端到端：跑过阈值的命令这次调用立刻返回「已转后台」，进程接着跑完，结束时交回带退出码、
/// 输出尾部与完整输出引用的消息；那次调用已经收口（Run 已 Completed），重启也不会再把它
/// 当没结果的调用。
#[tokio::test]
async fn a_long_command_detaches_and_its_end_comes_back_as_an_input() {
    let llm = DetachLlm::new();
    let mut gateway = start(&llm, 1, None).await;
    let session = open_session(&gateway).await;

    let (reply, took) = timed_say(
        &gateway,
        &session,
        "run",
        "跑：echo before; sleep 4; echo after-line; exit 4",
    )
    .await;
    assert!(reply.contains("已转后台 #"), "{reply}");
    assert!(reply.contains("before"), "到现在的输出也给了：{reply}");
    assert!(took < Duration::from_millis(3500), "没等命令跑完：{took:?}");
    let id = handle_of(&reply);

    wait_keyed(&gateway, "monitor-end:", Duration::from_secs(20)).await;
    let ends = llm.handbacks();
    assert_eq!(ends.len(), 1, "{ends:#?}");
    let end = &ends[0];
    assert!(
        end.starts_with(&format!("[后台命令 #{id}「echo before; sleep 4")),
        "{end}"
    );
    assert!(end.contains("退出码 4"), "{end}");
    assert!(end.contains("after-line"), "输出尾部：{end}");
    assert!(
        end.contains("artifact://") && end.contains("/background"),
        "{end}"
    );
    let records = registry(&gateway).await;
    assert_eq!(
        ended_reason(&records[0]),
        Some("exit"),
        "收尾后登记里 ended：{records:?}"
    );

    // 完整输出引用读得回：转后台之后的输出落在 background.log。
    let log = walk(&gateway.home.join("sessions"))
        .into_iter()
        .find(|path| path.file_name().is_some_and(|n| n == "background.log"))
        .expect("有 background.log");
    assert!(std::fs::read_to_string(log).unwrap().contains("after-line"));

    // 那次调用早已收口：重启后没有"没结果的调用"，原 Run 仍是 Completed。
    gateway.stop().await;
    let mut second = start(&llm, 1, Some(&gateway.home)).await;
    let original = submissions_keyed(&second, "run").await;
    assert_eq!(original.len(), 1);
    assert_eq!(
        original[0].status(),
        komo_kernel::durable::SubmissionStatus::Done
    );
    assert_eq!(llm.handbacks().len(), 1, "不会再交回第二条");
    second.stop().await;
}

/// 端到端：阈值远没到，用户在 TUI 里插话——前台命令跑满宽限就转后台，宿主不再等它跑完，
/// 回执说清是插话；命令照样在后台跑完并交回。
///
/// 改了做法：Legacy 经 HTTP `interject: true`；新版直接 `submit_input(.. interject(true))`（W2a 的
/// 路由把 `interject` 接到它上面）。回执在工具结果 entry 里找（宿主的最终回复此时是插话那句）。
#[tokio::test]
async fn an_interjection_moves_the_foreground_command_to_the_background() {
    let llm = DetachLlm::new();
    let mut gateway = start(&llm, 60, None).await;
    let session = open_session(&gateway).await;

    // 一共 15 秒，拆成三段：对话会话拒绝字面量 ≥10 秒的单个 `sleep`（`detach::long_sleep`）。
    let started = Instant::now();
    let host = submit(
        &gateway,
        &session,
        "host",
        "跑：echo before; for i in 1 2 3; do sleep 5; done; echo after",
    )
    .await;
    let storage = Arc::clone(&core(&gateway).storage);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let live = Harness::live(&*storage, host.conversation).await.unwrap();
            if live
                .tools
                .iter()
                .flatten()
                .any(|slot| slot.name == "bash" && slot.status == SlotStatus::Running)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("命令没开跑");
    let aside = core(&gateway)
        .submit_input(
            &session,
            komo_gateway::service::durable::submit::InputRequest::new("顺便说一句")
                .request_id("aside")
                .when_busy(WhenBusy::Steer)
                .interject(true),
        )
        .await
        .expect("插话受理得了");
    assert!(!aside.deduplicated);

    let _ = reply_of(&gateway, &host.run).await;
    let took = started.elapsed();
    assert!(took < Duration::from_secs(10), "没等 15 秒的命令：{took:?}");
    let text = entries_text(&gateway, host.conversation).await;
    assert!(text.contains("用户插话了"), "回执说清是插话：{text}");

    wait_keyed(&gateway, "monitor-end:", Duration::from_secs(30)).await;
    let ends = llm.handbacks();
    assert_eq!(ends.len(), 1, "{ends:#?}");
    assert!(ends[0].contains("退出码 0"), "{}", ends[0]);
    gateway.stop().await;
}

/// `detach_after_secs = 0`：不转，命令在这次调用里跑完。
#[tokio::test]
async fn zero_means_the_command_is_waited_for_in_place() {
    let llm = DetachLlm::new();
    let mut gateway = start(&llm, 0, None).await;
    let session = open_session(&gateway).await;

    let (reply, took) = timed_say(&gateway, &session, "run", "跑：sleep 2; echo finished").await;
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
    let llm = DetachLlm::new();
    let mut gateway = start(&llm, 1, None).await;
    let session = open_session(&gateway).await;

    let marker = gateway.home.join("still-running");
    let (reply, _) = timed_say(
        &gateway,
        &session,
        "run",
        &format!("限时跑：{}", touching(&marker)),
    )
    .await;
    assert!(reply.contains("已转后台 #"), "{reply}");

    wait_keyed(&gateway, "monitor-end:", Duration::from_secs(20)).await;
    let end = llm.handbacks().pop().unwrap_or_default();
    assert!(end.contains("超时"), "{end}");
    assert_stopped(&marker).await;
    let records = registry(&gateway).await;
    assert_eq!(ended_reason(&records[0]), Some("timeout"), "{records:?}");

    gateway.stop().await;
}

/// `monitor_stop #id` 停得掉转后台的命令。
///
/// 改了断言：Legacy 的结束消息会说"已被 monitor_stop 停止"。新版 `monitor_stop` 先把登记写成
/// `ended{stopped}`（模型自己停的，它知道）、再停进程，之后泵的 `finish_monitor` 不是第一个结束它的人，
/// 就不再交回结束消息——与 `monitor` 工具挂的监听同一个规矩。所以这里断言：停止的回执、登记
/// `stopped`、进程组没了、没有多余的结束消息。
#[tokio::test]
async fn monitor_stop_stops_a_detached_command_and_the_end_says_so() {
    let llm = DetachLlm::new();
    let mut gateway = start(&llm, 1, None).await;
    let session = open_session(&gateway).await;

    let marker = gateway.home.join("still-running");
    let (reply, _) = timed_say(
        &gateway,
        &session,
        "run",
        &format!("跑：{}", touching(&marker)),
    )
    .await;
    let id = handle_of(&reply);

    let (stopped, _) = timed_say(&gateway, &session, "stop", &format!("停：{id}")).await;
    assert!(stopped.contains(&format!("已停止监听 #{id}")), "{stopped}");

    assert_stopped(&marker).await;
    let records = registry(&gateway).await;
    assert_eq!(ended_reason(&records[0]), Some("stopped"), "{records:?}");
    assert!(
        submissions_keyed(&gateway, "monitor-end:").await.is_empty(),
        "停掉的不再交回结束消息"
    );
    assert!(llm.handbacks().is_empty());

    gateway.stop().await;
}

/// 重启时还没交回的转后台命令：进程随 Gateway 没了，下一代给会话一条"结果不明"。
///
/// 按新语义改写：Legacy 靠 `runtime/monitors/<id>.json` 留着、下一代读它交回；新版登记在
/// `komo.monitors`，停机时它还是 `running`，下一代启动时 `MonitorRunner::boot` 对没结束的转后台命令
/// 交回"结果不明"（带输出末尾与完整输出引用），请求键是 `monitor-end:{id}`，登记随之 `ended{interrupted}`。
/// 原调用仍是 Completed（已收口，不是 uncertain）。
#[tokio::test]
async fn a_restart_reports_an_unfinished_detached_command_as_unknown() {
    let llm = DetachLlm::new();
    let mut gateway = start(&llm, 1, None).await;
    let session = open_session(&gateway).await;

    let marker = gateway.home.join("still-running");
    let (reply, _) = timed_say(
        &gateway,
        &session,
        "run",
        &format!("跑：{}", touching(&marker)),
    )
    .await;
    let id = handle_of(&reply);
    gateway.stop().await;
    assert_stopped(&marker).await;

    let mut second = start(&llm, 1, Some(&gateway.home)).await;
    wait_keyed(&second, "monitor-end:", Duration::from_secs(20)).await;
    let end = llm.handbacks().pop().unwrap_or_default();
    assert!(end.starts_with(&format!("[后台命令 #{id}「")), "{end}");
    assert!(
        end.contains("Gateway 重启，后台命令中断，结果不明"),
        "{end}"
    );
    assert!(end.contains("artifact://"), "完整输出引用还在：{end}");
    let records = registry(&second).await;
    assert_eq!(
        ended_reason(&records[0]),
        Some("interrupted"),
        "{records:?}"
    );
    let original = submissions_keyed(&second, "run").await;
    assert_eq!(
        original[0].status(),
        komo_kernel::durable::SubmissionStatus::Done,
        "那次调用已收口，不是 uncertain"
    );

    second.stop().await;
}

/// 对话会话里 10 秒以上的字面量 sleep 在 `prepare` 就被拒绝，提示用 monitor。
#[tokio::test]
async fn a_long_sleep_in_a_conversation_is_refused_with_the_monitor_hint() {
    let llm = DetachLlm::new();
    let mut gateway = start(&llm, 120, None).await;
    let session = open_session(&gateway).await;

    let (reply, _) = timed_say(
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
    let (reply, _) = timed_say(&gateway, &session, "minute", "跑：sleep 1m").await;
    assert!(reply.contains("monitor"), "{reply}");
    let (reply, _) = timed_say(&gateway, &session, "short", "跑：sleep 2 && echo up").await;
    assert!(reply.contains("up"), "短 sleep 放行：{reply}");

    gateway.stop().await;
}

/// 后台命令在宿主仍请求模型时结束：完成通知必须在下一个工具边界并入原 Run，
/// 不能排到整条宿主 Run 之后，让它继续按「仍在后台」的旧状态调度。
///
/// 改了断言：Legacy 看通知那条 Run 的状态是 `Absorbed`；新版没有这个状态，看宿主的最终回复——
/// 通知并进去了，宿主在下一轮读到它（`已读到后台结束通知`）；排到后面的话宿主会先答
/// `仍未读到后台结束通知`。
#[tokio::test]
async fn a_detached_completion_interjects_into_the_still_running_host() {
    use komo_gateway::service::test_support::harness::{call_round, text_round};
    use komo_kernel::traits::TurnDriver;
    use komo_kernel::types::model::TokenUsage;
    use komo_kernel::types::turn::{LlmError, Round, RoundInput, TurnRequest};
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

    let dir = tempfile::tempdir().unwrap();
    let release = dir.path().join("release-background");
    let waiting = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let llm: Arc<dyn LlmClient> = Arc::new(BusyLlm {
        command: format!(
            "while [ ! -e '{}' ]; do sleep 0.05; done; echo finished; exit 1",
            release.display()
        ),
        waiting: waiting.clone(),
        proceed: proceed.clone(),
    });
    let mut gateway = GatewayBuilder::new(&config(1))
        .durable()
        .llm(Arc::clone(&llm))
        .stream(DriverStream::new(llm))
        .start()
        .await;
    let session = open_session(&gateway).await;
    let host = submit(&gateway, &session, "busy-host", "启动后台命令后继续处理").await;
    tokio::time::timeout(Duration::from_secs(10), waiting.notified())
        .await
        .expect("模型已拿到转后台回执");
    std::fs::write(&release, "release").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while submissions_keyed(&gateway, "monitor-end:").await.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("结束通知已受理");
    proceed.notify_one();
    let reply = reply_of(&gateway, &host.run).await;
    let end = wait_keyed(&gateway, "monitor-end:", Duration::from_secs(10)).await;
    gateway.stop().await;

    assert_eq!(
        reply, "已读到后台结束通知",
        "完成通知应并进宿主，而非另排一轮"
    );
    assert!(
        matches!(end.status(), komo_kernel::durable::SubmissionStatus::Done),
        "{end:?}"
    );
}
