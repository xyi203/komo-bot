//! 监听在 Durable 存储上（`docs/storage-durable-d5.md` W2c）：同样 8 个行为，换成新 harness——
//! 登记在 `durable.db` 的 `komo.monitors`，事件与结束消息是内部 steer 输入，重启由
//! `Harness::reconcile_monitors` 对账。测试名与 Legacy 版本（`main.rs`）一一对应；输入直接走
//! `DurableCore::submit_input`（HTTP v2 是 W2a 的），模型替身是同一批脚本（`MonitorLlm` 等）经
//! `DriverStream` 演成 `ModelStream`。
//!
//! 与 Legacy 版本不同的断言各有理由，写在对应测试的注释里；共同的两条：
//!
//! - **句柄是监听 id**（工具任务的 ID，十进制），不再是 UUID 后四位；
//! - **登记在 `komo.monitors`**，"登记删掉"变成"登记里这条已 `ended`（理由是……）"。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use komo_gateway::service::test_support::driver_stream::{
    DriverStream, core, open_session, reply_of, say, submissions_keyed, submit, wait_keyed,
};
use komo_gateway::service::test_support::harness::{GatewayBuilder, TestGateway};
use komo_kernel::traits::LlmClient;
use komo_runtime::durable::harness::orchestration::monitor::{self, MonitorRecord, MonitorState};
use serde_json::json;

use super::{MonitorLlm, Script, ScriptDriver, config, last_user_text};

async fn start(llm: &Arc<MonitorLlm>, at: Option<&Path>) -> TestGateway {
    let mut builder = GatewayBuilder::new(&config())
        .llm(Arc::clone(llm) as Arc<dyn LlmClient>)
        .stream(DriverStream::new(Arc::clone(llm) as Arc<dyn LlmClient>));
    if let Some(home) = at {
        builder = builder.at(home);
    }
    builder.start().await
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

/// 挂一个监听，返回会话与句柄（从模型念出来的工具结果里抠：`已开始监听 #<id>：…`）。
async fn arm(gateway: &TestGateway, command: &str) -> (String, String) {
    arm_with(gateway, "挂", command).await
}

async fn arm_with(gateway: &TestGateway, verb: &str, command: &str) -> (String, String) {
    let session = open_session(gateway).await;
    let reply = say(gateway, &session, "arm", &format!("{verb}：{command}")).await;
    let id = reply
        .split("已开始监听 #")
        .nth(1)
        .and_then(|rest| rest.split('：').next())
        .unwrap_or_else(|| panic!("回执里没有句柄：{reply}"))
        .to_string();
    (session, id)
}

/// 端到端：脚本的行作为 `[监听 #句柄…]` 输入交回同一个会话，脚本退出后交回一条带退出码的结束
/// 消息，登记随之 `ended{exit}`。
#[tokio::test]
async fn lines_come_back_as_inputs_and_the_exit_is_reported() {
    let llm = MonitorLlm::new(Duration::ZERO);
    let mut gateway = start(&llm, None).await;

    let (_session, id) = arm(
        &gateway,
        "for i in 1 2 3; do echo tick $i; sleep 0.2; done; echo boom >&2; exit 3",
    )
    .await;
    wait_keyed(&gateway, "monitor-end:", Duration::from_secs(20)).await;

    let events = llm.events();
    let joined = events.join("\n");
    for tick in ["tick 1", "tick 2", "tick 3"] {
        assert!(joined.contains(tick), "{tick} 没有交回：{events:#?}");
    }
    assert!(
        events
            .iter()
            .all(|event| event.starts_with(&format!("[监听 #{id}「测试监听」"))),
        "{events:#?}"
    );
    let end = events.last().unwrap();
    assert!(end.contains("exit=3"), "{end}");
    assert!(end.contains("stderr 末尾：\nboom"), "{end}");
    assert!(
        !joined.contains("boom\n（这是监听脚本"),
        "stderr 不该当成事件：{events:#?}"
    );
    // 旧版："登记文件删掉"。新版：登记里它已经 `ended{exit}`。
    let records = registry(&gateway).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(ended_reason(&records[0]), Some("exit"), "{records:?}");

    gateway.stop().await;
}

/// 模型处理得慢时，脚本这段时间出的行合进下一条，而不是按批排一串 Run。
#[tokio::test]
async fn lines_printed_while_an_event_is_being_handled_are_merged_into_the_next() {
    let llm = MonitorLlm::new(Duration::from_secs(3));
    let mut gateway = start(&llm, None).await;

    let (_session, _) = arm(
        &gateway,
        "for i in $(seq 1 30); do echo line $i; sleep 0.2; done",
    )
    .await;
    wait_keyed(&gateway, "monitor-end:", Duration::from_secs(40)).await;

    let events = llm.events();
    let joined = events.join("\n");
    for i in 1..=30 {
        assert!(
            joined.lines().any(|line| line == format!("line {i}")),
            "line {i} 丢了：{events:#?}"
        );
    }
    let event_runs = submissions_keyed(&gateway, "monitor:").await.len();
    // 6 秒里出 30 行，每条事件要处理 3 秒：同时只有一条在飞时是两条左右（其余进结束
    // 消息）；不等上一条收尾、每秒交一条的话是六条。
    assert!(
        event_runs <= 3,
        "上一条没收尾时出的行该合进下一条：{event_runs} 条 {events:#?}"
    );

    gateway.stop().await;
}

/// `monitor_stop` 终止进程组，之后不再交回结束消息；给错句柄时列出还在跑的那些。
#[tokio::test]
async fn stopping_kills_the_script_and_reports_nothing_more() {
    let llm = MonitorLlm::new(Duration::ZERO);
    let mut gateway = start(&llm, None).await;

    let marker = gateway.home.join("still-running");
    let (session, id) = arm(
        &gateway,
        &format!("while true; do touch {}; sleep 0.2; done", marker.display()),
    )
    .await;

    let reply = say(&gateway, &session, "stop-wrong", "停：beef").await;
    assert!(reply.contains(&format!("#{id}「测试监听」")), "{reply}");

    let reply = say(&gateway, &session, "stop", &format!("停：{id}")).await;
    assert!(reply.contains(&format!("已停止监听 #{id}")), "{reply}");

    // 进程组没了：标记文件不再被刷新。
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = std::fs::remove_file(&marker);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!marker.exists(), "停掉之后脚本还在跑");

    let reply = say(&gateway, &session, "stop-again", &format!("停：{id}")).await;
    assert!(reply.contains("已经停掉了"), "再停一次是幂等的：{reply}");

    assert!(
        submissions_keyed(&gateway, "monitor-end:").await.is_empty(),
        "模型自己停的，不该再交回结束消息"
    );
    let records = registry(&gateway).await;
    assert_eq!(ended_reason(&records[0]), Some("stopped"), "{records:?}");

    gateway.stop().await;
}

/// 脚本自己退出之后才调 `monitor_stop`（处理上一条事件的 Run 与脚本收尾竞态）：答“已经结束了”
/// 和怎么结束的，不算失败，也不说它“不在跑”。起因是 session `01a153c3` seq 3578。
#[tokio::test]
async fn stopping_a_monitor_that_already_exited_says_how_it_ended() {
    let llm = MonitorLlm::new(Duration::ZERO);
    let mut gateway = start(&llm, None).await;

    let (session, id) = arm(&gateway, "sleep 0.5; exit 0").await;
    wait_keyed(&gateway, "monitor-end:", Duration::from_secs(20)).await;

    let reply = say(&gateway, &session, "stop-late", &format!("停：{id}")).await;
    assert!(
        reply.contains(&format!(
            "不用停了：监听 #{id}「测试监听」已结束：脚本退出（exit=0）"
        )),
        "{reply}"
    );
    assert!(!reply.contains("停不了"), "{reply}");

    // 别的会话用同一个句柄停不到它。
    let other = open_session(&gateway).await;
    let reply = say(&gateway, &other, "stop-other", &format!("停：{id}")).await;
    assert!(reply.contains("停不了"), "{reply}");

    gateway.stop().await;
}

/// 停机终止监听但登记留着 `running`；下一代启动时对账："已中断"交回会话，脚本没有被重新起来。
///
/// 改了断言：Legacy 的结束消息以 `[监听 #短号「…」已中断` 开头；新版由 runtime 的
/// `reconcile_monitors` 写（`[监听 #id「…」已结束]` + 正文"已中断：Gateway 重启了……"），所以只认
/// 开头与"已中断"。"登记文件删掉 / 留着"也换成登记里的状态。
#[tokio::test]
async fn a_restart_interrupts_the_monitor_and_tells_the_session() {
    let llm = MonitorLlm::new(Duration::ZERO);
    let mut gateway = start(&llm, None).await;

    let marker = gateway.home.join("still-running");
    let (_session, id) = arm(
        &gateway,
        &format!("while true; do touch {}; sleep 0.2; done", marker.display()),
    )
    .await;
    gateway.stop().await;
    let records = registry_after_stop(&gateway).await;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].state,
        MonitorState::Running,
        "停机时登记留给下一代"
    );

    let _ = std::fs::remove_file(&marker);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!marker.exists(), "停机之后脚本还在跑");

    let mut second = start(&llm, Some(&gateway.home)).await;
    wait_keyed(&second, "monitor-end:", Duration::from_secs(20)).await;
    let end = llm.events().last().cloned().unwrap_or_default();
    assert!(
        end.starts_with(&format!("[监听 #{id}「测试监听」已结束")) && end.contains("已中断"),
        "{end}"
    );
    let records = registry(&second).await;
    assert_eq!(
        ended_reason(&records[0]),
        Some("interrupted"),
        "{records:?}"
    );

    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!marker.exists(), "重启不该把脚本重新起来");

    second.stop().await;
}

/// 停机之后读登记：库被引擎握着，没法另开，所以重开一台读完再停。
async fn registry_after_stop(gateway: &TestGateway) -> Vec<MonitorRecord> {
    // 同一个数据目录起一台只读不动的：对账会把 `running` 的判掉，所以这里直接读文件级的存储。
    let storage = komo_store::durable::TursoStorage::open(&gateway.home)
        .await
        .expect("停机之后能打开 durable.db");
    use komo_kernel::durable::Storage;
    let records = monitor::list(&storage).await.expect("读得出");
    storage.close().await.expect("关得掉");
    records
}

/// 声明了 `resume_after_restart` 的监听：重启后按原句柄重新起来，不交回"中断了"，下一条事件
/// 注明中间断过；重启前后的事件都交回了（请求键不撞）。
#[tokio::test]
async fn a_resumable_monitor_is_relaunched_after_a_restart_under_the_same_short_id() {
    let llm = MonitorLlm::new(Duration::ZERO);
    let mut gateway = start(&llm, None).await;

    let marker = gateway.home.join("still-running");
    let (_session, id) = arm_with(
        &gateway,
        "续挂",
        &format!(
            "echo started; while true; do touch {}; sleep 0.2; done",
            marker.display()
        ),
    )
    .await;
    wait_keyed(&gateway, "monitor:", Duration::from_secs(20)).await;
    gateway.stop().await;
    let records = registry_after_stop(&gateway).await;
    assert_eq!(
        records[0].state,
        MonitorState::Running,
        "停机时登记留给下一代"
    );
    let _ = std::fs::remove_file(&marker);

    let mut second = start(&llm, Some(&gateway.home)).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while llm.events().len() < 2 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let events = llm.events();
    assert_eq!(events.len(), 2, "重启前后各一条 started：{events:#?}");
    assert!(
        events
            .iter()
            .all(|e| e.starts_with(&format!("[监听 #{id}「测试监听」]"))),
        "{events:#?}"
    );
    assert!(!events[0].contains("Gateway 重启过"), "{events:#?}");
    assert!(events[1].contains("Gateway 重启过"), "{events:#?}");
    assert!(marker.exists(), "重挂之后脚本该在跑");
    assert!(
        submissions_keyed(&second, "monitor-end:").await.is_empty(),
        "重挂了就不该交回中断"
    );
    assert_eq!(
        registry(&second).await[0].state,
        MonitorState::Running,
        "还在跑，登记仍是 running"
    );

    second.stop().await;
}

/// 重启时时限已经过了的可重挂监听：不重起，交回一条"时限已到"。
///
/// 改了做法：Legacy 改 `runtime/monitors/<id>.json` 里的 `expires_at`；新版登记在引擎的库里，
/// 停机前经引擎把登记里的截止时刻改到过去。
#[tokio::test]
async fn a_resumable_monitor_past_its_deadline_is_not_relaunched() {
    let llm = MonitorLlm::new(Duration::ZERO);
    let mut gateway = start(&llm, None).await;

    let marker = gateway.home.join("still-running");
    let (_session, id) = arm_with(
        &gateway,
        "续挂",
        &format!("while true; do touch {}; sleep 0.2; done", marker.display()),
    )
    .await;
    core(&gateway)
        .engine
        .commit(|tx| {
            Box::pin(async move {
                let (_, mut value) = tx
                    .document(&monitor::monitors_address())
                    .await?
                    .expect("有登记");
                for item in value["items"].as_array_mut().expect("items") {
                    item["deadline"] = json!(0);
                }
                tx.put_document(
                    monitor::monitors_address(),
                    monitor::MONITORS_VERSION,
                    value,
                )
                .await
            })
        })
        .await
        .expect("改得了登记");
    gateway.stop().await;
    let _ = std::fs::remove_file(&marker);

    let mut second = start(&llm, Some(&gateway.home)).await;
    wait_keyed(&second, "monitor-end:", Duration::from_secs(20)).await;
    let end = llm.events().last().cloned().unwrap_or_default();
    assert!(
        end.starts_with(&format!("[监听 #{id}「测试监听」已结束")),
        "{end}"
    );
    assert!(end.contains("时限已经到了"), "{end}");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!marker.exists(), "过了时限不该重起");
    let records = registry(&second).await;
    assert_eq!(ended_reason(&records[0]), Some("timeout"), "{records:?}");

    second.stop().await;
}

/// 监听事件在宿主仍在跑时到达：必须在下一个工具边界并入原 Run，不能排到整条宿主 Run
/// 之后（session `01a1588d` seq 177：结束消息排了 46 秒）。
///
/// 改了断言：Legacy 看事件那条 Run 的状态是 `Absorbed`；新版没有这个状态，并进去的输入在宿主 run
/// 收尾时一起结算，所以看宿主的最终回复——事件并进去了，宿主在下一轮读到它（`已读到监听事件`）；
/// 排到后面的话宿主会先答 `仍未读到监听事件`。
#[tokio::test]
async fn a_monitor_event_interjects_into_the_still_running_host() {
    use komo_gateway::service::test_support::harness::{call_round, text_round};
    use komo_kernel::traits::TurnDriver;
    use komo_kernel::types::model::TokenUsage;
    use komo_kernel::types::turn::{LlmError, Round, RoundInput, TurnRequest};
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

    let waiting = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let llm: Arc<dyn LlmClient> = Arc::new(BusyLlm {
        waiting: waiting.clone(),
        proceed: proceed.clone(),
    });
    let mut gateway = GatewayBuilder::new(&config())
        .llm(Arc::clone(&llm))
        .stream(DriverStream::new(llm))
        .start()
        .await;
    let session = open_session(&gateway).await;
    let host = submit(&gateway, &session, "busy-host", "挂上监听后继续处理").await;
    tokio::time::timeout(Duration::from_secs(10), waiting.notified())
        .await
        .expect("模型已拿到监听回执");
    tokio::time::timeout(Duration::from_secs(10), async {
        while submissions_keyed(&gateway, "monitor:").await.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("监听事件已受理");
    proceed.notify_one();
    let reply = reply_of(&gateway, &host.run).await;
    let event = wait_keyed(&gateway, "monitor:", Duration::from_secs(10)).await;
    gateway.stop().await;

    assert_eq!(reply, "已读到监听事件", "事件应并进宿主，而非另排一轮");
    assert!(
        matches!(event.status(), komo_kernel::durable::SubmissionStatus::Done),
        "{event:?}"
    );
}
