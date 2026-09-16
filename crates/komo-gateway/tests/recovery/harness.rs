//! W5 恢复验收的脚手架：一台**真** Gateway（真 config.toml、真 state.db、真 TcpListener、
//! 真调度器与账本），加上两把注入用的刀。
//!
//! 两条注入路（§14「恢复故障注入验收」）：
//!
//! - **(a) 在 §8.5 的步骤之间停下**：[`FaultLedger`] 包住装配出来的 `Arc<dyn Ledger>`，
//!   在指定的那一步之前 / 之后返回 `Err` 并**毒化**其余全部写入，等价于"这个进程从账本
//!   的角度已经死了"。测试等到故障跳闸就 `stop()`，再用同一个数据目录重启。
//! - **(b) 直接篡改磁盘**：停机之后改 `sessions/<id>/events.jsonl`（截半行、删中间一行、
//!   手写一条 `tool.result`）、删 / 改 `tool-output/.../output.json`、写
//!   `runtime/children/<executor>.json`，再重启。
//!
//! 副作用一律用文件计数：模型让 `write` 写一个文件，或让 `shell` 往计数文件里追加一行；
//! "跑了几次"就是文件里有几行。**断言的是次数，不是状态**（§14 最后一段）。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::events::{Event, EventPayload};
use komo_kernel::traits::{Ledger, LedgerError, LlmClient, TurnDriver};
use komo_kernel::types::ids::{AttemptId, EventId, ExecutorId, RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::turn::{
    AcceptInput, LlmError, ProviderToolCall, Round, RoundInput, TurnRequest,
};
use komo_store::Db;

// ---------------------------------------------------------------- 数据目录

/// 一个真实的数据目录。**故意不写 policy.toml**——那样 §7.1 的初始建议生效：
/// 根内写入是 Allow，任意 shell 是 Ask，正好是验收要的两种放行方式。
pub struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    pub fn new() -> Home {
        install_crypto();
        let dir = tempfile::tempdir().expect("临时数据目录");
        std::fs::write(
            dir.path().join("config.toml"),
            r#"
[model]
provider = "openai_responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[memory]
enabled = false
"#,
        )
        .expect("写 config.toml");
        std::fs::write(dir.path().join(".env"), "KOMO_LLM_API_KEY=test-key\n").expect("写 .env");
        std::fs::create_dir_all(dir.path().join("workspaces")).expect("工作目录");
        Home { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn workspace(&self) -> PathBuf {
        self.dir.path().join("workspaces")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.dir.path().join("sessions")
    }

    pub fn session_dir(&self, session: &SessionId) -> PathBuf {
        self.sessions_dir().join(session.as_str())
    }

    pub fn events_path(&self, session: &SessionId) -> PathBuf {
        self.session_dir(session).join("events.jsonl")
    }

    pub fn quarantine_path(&self, session: &SessionId) -> PathBuf {
        self.session_dir(session).join("events.jsonl.quarantine")
    }

    /// 起一台 Gateway。**同一个数据目录**，所以这就是"重启"。
    pub async fn start(&self, llm: Arc<dyn LlmClient>) -> Gw {
        let running = komo_gateway::service::start(komo_gateway::service::ServiceOptions {
            home: Some(self.dir.path().to_path_buf()),
            listen: Some("127.0.0.1:0".into()),
            channels: Vec::new(),
            llm: Some(llm),
        })
        .await
        .expect("Gateway 起得来");
        let base = running.base_url.clone();
        let token = running.state.token.clone();
        Gw {
            running,
            base,
            token,
        }
    }

    /// 装一个故障账本；下一次 `start` 生效。
    pub fn inject(&self, fault: Fault) -> Arc<FaultState> {
        let state = Arc::new(FaultState::new(fault));
        let for_wrap = Arc::clone(&state);
        komo_gateway::service::test_support::install_ledger_wrap(
            self.dir.path(),
            Arc::new(move |inner| {
                Arc::new(FaultLedger {
                    inner,
                    state: Arc::clone(&for_wrap),
                }) as Arc<dyn Ledger>
            }),
        );
        state
    }

    pub fn clear_injection(&self) {
        komo_gateway::service::test_support::clear_ledger_wrap(self.dir.path());
    }

    /// 停机之后单独打开 state.db（Turso 对 db 文件持进程独占锁，所以**必须先 stop**）。
    pub async fn open_db(&self) -> Db {
        Db::connect(self.dir.path().join("state.db"))
            .await
            .expect("打开 state.db")
    }

    /// 这个 Session 的全部事件，按 seq。
    pub fn events(&self, session: &SessionId) -> Vec<Event> {
        let text = std::fs::read_to_string(self.events_path(session)).unwrap_or_default();
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| Event::from_line(line).expect("事件解析得了"))
            .collect()
    }

    pub fn event_types(&self, session: &SessionId) -> Vec<String> {
        self.events(session)
            .iter()
            .map(|event| event.type_name().to_string())
            .collect()
    }

    /// 一次尝试的输出目录（`tool-output/<run>/<call>/<attempt>/`，§8.3 的形状）。
    pub fn attempt_dir(
        &self,
        session: &SessionId,
        run: &RunId,
        started: &komo_kernel::events::ToolStarted,
    ) -> PathBuf {
        self.session_dir(session)
            .join("tool-output")
            .join(run.as_str())
            .join(started.call_id.as_str())
            .join(started.attempt_id.as_str())
    }

    /// 把这次尝试的输出整棵删掉——等价于**`ToolOutputStore::publish` 从未发生**。
    ///
    /// 故障装饰器包的是 `Ledger`，包不到 `ToolOutputStore`，而"外部副作用已经发生、
    /// 完整输出还没落盘"这一段（§14 故障注入表第 6 行）的分界正好在 `publish` 上。所以
    /// 这一刀走的是另一条注入路：停机之后直接改磁盘。`orphan::find` 对"文件不在"的判断
    /// 就是"没跑到落盘那一步"，与真正的中断无法区分。
    pub fn drop_attempt_output(
        &self,
        session: &SessionId,
        run: &RunId,
        started: &komo_kernel::events::ToolStarted,
    ) {
        let dir = self.attempt_dir(session, run, started);
        std::fs::remove_dir_all(&dir).unwrap_or_else(|e| panic!("删 {}：{e}", dir.display()));
    }

    /// 在 `runtime/children/<executor>.json` 里登记一个**还活着**的子进程
    /// （§8.7「无法确认旧执行已结束时，阻止该任务重复启动」）。
    pub fn register_live_child(&self, executor: &str, pid: u32) {
        let dir = self.dir.path().join("runtime").join("children");
        std::fs::create_dir_all(&dir).expect("children 目录");
        let body = serde_json::json!([{
            "pid": pid,
            "pgid": pid,
            "what": "shell: 上一代留下的"
        }]);
        std::fs::write(
            dir.join(format!("{executor}.json")),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .expect("写子进程登记");
    }
}

// ---------------------------------------------------------------- 一台起着的 Gateway

pub struct Gw {
    pub running: komo_gateway::service::Running,
    base: String,
    token: String,
}

impl Gw {
    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub fn executor_id(&self) -> String {
        self.running.state.instance_id.clone()
    }

    /// 一个**真的** CLI 客户端，指着这台 Gateway（`komo resume` 走的就是它）。
    pub fn client(&self) -> komo_client::KomoClient {
        komo_client::KomoClient::new(&self.base, Some(self.token.clone())).expect("客户端")
    }

    pub async fn get(&self, path: &str) -> (u16, String) {
        self.request(reqwest::Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: serde_json::Value) -> (u16, String) {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }

    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, String) {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("请求发得出去");
        let status = response.status().as_u16();
        (status, response.text().await.unwrap_or_default())
    }

    /// 建一个会话（走 HTTP，和 `komo` 那条命令一样）。
    pub async fn open_session(&self) -> SessionId {
        let (status, body) = self.post("/v1/sessions", serde_json::json!({})).await;
        assert_eq!(status, 200, "{body}");
        let summary: komo_kernel::protocol::http::SessionSummary =
            serde_json::from_str(&body).expect("会话");
        summary.session
    }

    /// 提交一条输入。
    pub async fn submit(
        &self,
        session: &SessionId,
        key: &str,
        text: &str,
    ) -> komo_kernel::protocol::http::SubmitRunResponse {
        let (status, body) = self
            .post(
                &format!("/v1/sessions/{session}/runs"),
                serde_json::json!({"request_key": key, "text": text}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).expect("提交")
    }

    pub async fn run_detail(&self, run: &RunId) -> komo_kernel::protocol::http::RunDetail {
        let (status, body) = self.get(&format!("/v1/runs/{run}")).await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).expect("run detail")
    }

    /// 读不出来也是一个答案（会话损坏时 `/v1/runs/{id}` 会报 `corrupt`）。
    pub async fn try_run_detail(
        &self,
        run: &RunId,
    ) -> Result<komo_kernel::protocol::http::RunDetail, String> {
        let (status, body) = self.get(&format!("/v1/runs/{run}")).await;
        if status != 200 {
            return Err(body);
        }
        serde_json::from_str(&body).map_err(|e| e.to_string())
    }

    /// 直接从 state.db 读这一行的状态——会话日志坏掉时 HTTP 那条路读不出来。
    pub async fn db_status(&self, run: &RunId) -> komo_kernel::types::status::RunStatus {
        komo_store::repos::runs::get(&self.running.state.db, run)
            .await
            .expect("读得到")
            .expect("有这一行")
            .status
    }

    pub async fn wait_db_status(
        &self,
        run: &RunId,
        want: impl Fn(komo_kernel::types::status::RunStatus) -> bool,
        what: &str,
    ) -> komo_kernel::types::status::RunStatus {
        for _ in 0..400 {
            let status = self.db_status(run).await;
            if want(status) {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!(
            "等了 20 秒 run {run} 还没到「{what}」，现在是 {:?}",
            self.db_status(run).await
        );
    }

    pub async fn run_status(&self, run: &RunId) -> komo_kernel::types::status::RunStatus {
        self.run_detail(run).await.summary.status
    }

    /// 等这个 Run 到某个状态。
    pub async fn wait_status(
        &self,
        run: &RunId,
        want: impl Fn(komo_kernel::types::status::RunStatus) -> bool,
        what: &str,
    ) -> komo_kernel::protocol::http::RunDetail {
        for _ in 0..400 {
            let detail = self.run_detail(run).await;
            if want(detail.summary.status) {
                return detail;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!(
            "等了 20 秒 run {run} 还没到「{what}」，现在是 {:?}",
            self.run_status(run).await
        );
    }

    pub async fn wait_terminal(&self, run: &RunId) -> komo_kernel::protocol::http::RunDetail {
        self.wait_status(run, |s| s.is_terminal(), "终态").await
    }

    pub async fn approvals(&self) -> Vec<komo_kernel::protocol::http::ApprovalRecord> {
        let (status, body) = self.get("/v1/approvals").await;
        assert_eq!(status, 200, "{body}");
        let list: komo_kernel::protocol::http::ApprovalListResponse =
            serde_json::from_str(&body).expect("审批列表");
        list.approvals
    }

    /// 等到有一条待处理审批。
    pub async fn wait_approval(&self) -> komo_kernel::protocol::http::ApprovalRecord {
        for _ in 0..400 {
            if let Some(record) = self.approvals().await.into_iter().next() {
                return record;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("等了 20 秒也没有待处理审批");
    }

    pub async fn decide(
        &self,
        approval: &komo_kernel::types::ids::ApprovalId,
        approved: bool,
    ) -> String {
        let (status, body) = self
            .post(
                &format!("/v1/approvals/{approval}/decision"),
                serde_json::json!({"approved": approved, "scope": "once"}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        body
    }

    pub async fn stop(self) {
        self.running.stop().await;
    }
}

// ---------------------------------------------------------------- 脚本化模型

/// 一个回合的脚本。
pub fn text_round(n: u32, text: &str) -> Result<Round, LlmError> {
    Ok(Round {
        round: n,
        text: Some(text.to_string()),
        tool_calls: Vec::new(),
        provider_blocks: None,
        usage: TokenUsage::default(),
        truncated: false,
    })
}

pub fn call_round(
    n: u32,
    provider_id: &str,
    tool: &str,
    args: serde_json::Value,
) -> Result<Round, LlmError> {
    Ok(Round {
        round: n,
        text: None,
        tool_calls: vec![ProviderToolCall {
            provider_call_id: provider_id.to_string(),
            name: tool.to_string(),
            arguments: args,
        }],
        provider_blocks: None,
        usage: TokenUsage::default(),
        truncated: false,
    })
}

/// 脚本化的 `LlmClient`：每次 `begin_turn` 取一段脚本。
///
/// 脚本用完之后给的是**一句收尾**，不是一个错误，也不是把上一段重放一遍。两边都踩过：
/// 报错会让故障注入之后的重领变成一个终态失败，把要测的东西盖掉；重放会让续跑的那一轮
/// 把同一个工具调用**又要一次**，于是"只跑一次"根本无从断言。
pub struct FakeLlm {
    scripts: Mutex<std::collections::VecDeque<Vec<Result<Round, LlmError>>>>,
    /// 脚本用完之后每一段都用它。
    fallback: Mutex<Vec<Result<Round, LlmError>>>,
    turns: AtomicUsize,
    pub requests: Mutex<Vec<TurnRequest>>,
}

impl FakeLlm {
    pub fn new(scripts: Vec<Vec<Result<Round, LlmError>>>) -> Arc<FakeLlm> {
        Arc::new(FakeLlm {
            scripts: Mutex::new(scripts.into_iter().collect()),
            fallback: Mutex::new(vec![text_round(99, "（没有别的要做了）")]),
            turns: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        })
    }

    /// **每一段**都是这个脚本（模型一直给同一个答复的情形，比如一直"回复未收齐"）。
    pub fn always(rounds: Vec<Result<Round, LlmError>>) -> Arc<FakeLlm> {
        let llm = FakeLlm::new(vec![]);
        *llm.fallback.lock().expect("脚本模型") = rounds;
        llm
    }

    /// 一段"什么都不做，直接收尾"的脚本——续跑的测试用它：这一段里**模型不该再要求
    /// 任何调用**，跑起来的调用只能来自日志里那份原计划。
    pub fn finisher(text: &str) -> Arc<FakeLlm> {
        FakeLlm::always(vec![text_round(1, text)])
    }

    /// 到目前为止开了几个 turn（= 向模型请求了几次）。
    pub fn turns(&self) -> usize {
        self.turns.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmClient for FakeLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        self.turns.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().expect("脚本模型").push(req);
        let rounds = {
            let mut scripts = self.scripts.lock().expect("脚本模型");
            match scripts.pop_front() {
                Some(rounds) => rounds,
                None => self.fallback.lock().expect("脚本模型").clone(),
            }
        };
        Ok(Box::new(FakeDriver {
            rounds: rounds.into_iter().collect(),
            usage: TokenUsage::default(),
        }))
    }
}

struct FakeDriver {
    rounds: std::collections::VecDeque<Result<Round, LlmError>>,
    usage: TokenUsage,
}

#[async_trait]
impl TurnDriver for FakeDriver {
    async fn next(&mut self, _input: RoundInput) -> Result<Round, LlmError> {
        match self.rounds.pop_front() {
            Some(round) => round,
            // 脚本演完：给一句收尾，而不是一个错误（理由见 `FakeLlm` 的注释）。
            None => text_round(99, "（脚本演完了，收尾）"),
        }
    }

    fn usage(&self) -> TokenUsage {
        self.usage
    }
}

// ---------------------------------------------------------------- 故障账本

/// 在哪一步停下。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// `start_run` 之前——Run 已经 queued，执行者一个字都没写下（表第 1 行）。
    BeforeStartRun,
    /// `start_call` 之前——计划已同步到 JSONL，`tool.started` 没有（表第 4 行）。
    BeforeStartCall,
    /// `start_call` 之后——started 已提交，真实动作还没发出（表第 5 行）。
    AfterStartCall,
    /// `finish_call` 之前——工具跑完、output.json 已发布，`tool.result` 没写（表第 6 / 8 行）。
    BeforeFinishCall,
    /// 第 n 次 `record_round` 之前（n 从 1 数）。用来在一轮工具收尾之后停住。
    BeforeRecordRound(u32),
    /// `append_audit` 之前——审批决定已在 state.db，审计事件补写不进 JSONL（表第 10 行）。
    BeforeAppendAudit,
    /// 不注入任何故障（只为了拿一个可观察的账本）。
    None,
}

pub struct FaultState {
    fault: Fault,
    rounds: AtomicUsize,
    tripped: tokio::sync::Notify,
    poisoned: std::sync::atomic::AtomicBool,
    trip_count: AtomicUsize,
}

impl FaultState {
    fn new(fault: Fault) -> FaultState {
        FaultState {
            fault,
            rounds: AtomicUsize::new(0),
            tripped: tokio::sync::Notify::new(),
            poisoned: std::sync::atomic::AtomicBool::new(false),
            trip_count: AtomicUsize::new(0),
        }
    }

    /// 等故障跳闸。跳闸之后这台 Gateway 的账本**全部写入都失败**，等价于进程已死。
    pub async fn wait_tripped(&self) {
        if self.poisoned.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::timeout(std::time::Duration::from_secs(20), self.tripped.notified())
            .await
            .expect("故障没有跳闸");
    }

    pub fn tripped(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }

    fn trip(&self) -> LedgerError {
        self.poisoned.store(true, Ordering::SeqCst);
        self.trip_count.fetch_add(1, Ordering::SeqCst);
        self.tripped.notify_waiters();
        LedgerError::Persist("注入的故障：这个进程从这一步起写不动账本了".into())
    }

    fn poisoned(&self) -> Option<LedgerError> {
        self.poisoned
            .load(Ordering::SeqCst)
            .then(|| LedgerError::Persist("注入的故障：这个进程从这一步起写不动账本了".into()))
    }
}

/// 包在真账本外面的故障装饰器。
///
/// 毒化是刻意的：一次 `Err` 会让调度器 `release` 之后**再领一次**，于是同一个故障点会
/// 被反复穿过，日志上多出几轮谁也没要求的事件。毒化之后所有写入都失败，Run 干净地停在
/// 原地，测试可以从容 `stop()`。
struct FaultLedger {
    inner: Arc<dyn Ledger>,
    state: Arc<FaultState>,
}

macro_rules! poisoned {
    ($self:expr) => {
        if let Some(error) = $self.state.poisoned() {
            return Err(error);
        }
    };
}

#[async_trait]
impl Ledger for FaultLedger {
    async fn accept_input(
        &self,
        input: AcceptInput,
    ) -> Result<komo_kernel::types::turn::Accepted, LedgerError> {
        poisoned!(self);
        self.inner.accept_input(input).await
    }

    async fn record_round(
        &self,
        run: &RunId,
        round: komo_kernel::types::turn::AssistantRound,
    ) -> Result<Vec<ToolCallId>, LedgerError> {
        poisoned!(self);
        let n = self.state.rounds.fetch_add(1, Ordering::SeqCst) as u32 + 1;
        if self.state.fault == Fault::BeforeRecordRound(n) {
            return Err(self.state.trip());
        }
        self.inner.record_round(run, round).await
    }

    async fn start_run(
        &self,
        run: &RunId,
        executor: &ExecutorId,
        generation: u64,
    ) -> Result<(), LedgerError> {
        poisoned!(self);
        if self.state.fault == Fault::BeforeStartRun {
            return Err(self.state.trip());
        }
        self.inner.start_run(run, executor, generation).await
    }

    async fn plan_call(
        &self,
        call: &ToolCallId,
        plan: &komo_kernel::types::plan::ExecutionPlan,
    ) -> Result<EventId, LedgerError> {
        poisoned!(self);
        self.inner.plan_call(call, plan).await
    }

    async fn start_call(
        &self,
        call: &ToolCallId,
        plan: &komo_kernel::types::plan::ExecutionPlan,
        grant: Option<komo_kernel::types::turn::GrantUse>,
    ) -> Result<AttemptId, LedgerError> {
        poisoned!(self);
        if self.state.fault == Fault::BeforeStartCall {
            return Err(self.state.trip());
        }
        let attempt = self.inner.start_call(call, plan, grant).await?;
        if self.state.fault == Fault::AfterStartCall {
            // JSONL 与 state.db 两步都提交了，**真实动作还没发出**。
            return Err(self.state.trip());
        }
        Ok(attempt)
    }

    async fn finish_call(
        &self,
        attempt: &AttemptId,
        published: komo_kernel::types::refs::PublishedOutput,
    ) -> Result<(), LedgerError> {
        poisoned!(self);
        if self.state.fault == Fault::BeforeFinishCall {
            return Err(self.state.trip());
        }
        self.inner.finish_call(attempt, published).await
    }

    async fn suspend(
        &self,
        run: &RunId,
        wait: komo_kernel::types::status::Wait,
    ) -> Result<(), LedgerError> {
        poisoned!(self);
        self.inner.suspend(run, wait).await
    }

    async fn complete(
        &self,
        run: &RunId,
        end: komo_kernel::types::status::RunEnd,
    ) -> Result<(), LedgerError> {
        poisoned!(self);
        self.inner.complete(run, end).await
    }

    async fn read(
        &self,
        session: &SessionId,
        from: Seq,
        limit: u32,
    ) -> Result<komo_kernel::types::turn::EventBatch, LedgerError> {
        // 读永远放行：恢复扫描要读得到日志。
        self.inner.read(session, from, limit).await
    }

    async fn boundary(&self, session: &SessionId) -> Result<Seq, LedgerError> {
        poisoned!(self);
        self.inner.boundary(session).await
    }

    async fn append_audit(
        &self,
        session: &SessionId,
        event_id: &EventId,
        payload: EventPayload,
        occurred_at: time::OffsetDateTime,
    ) -> Result<Seq, LedgerError> {
        poisoned!(self);
        if self.state.fault == Fault::BeforeAppendAudit {
            return Err(self.state.trip());
        }
        self.inner
            .append_audit(session, event_id, payload, occurred_at)
            .await
    }
}

// ---------------------------------------------------------------- 副作用计数

/// 一个副作用计数器：`shell` 往它里面追加一行，文件里有几行就是跑了几次。
pub struct Counter {
    path: PathBuf,
}

impl Counter {
    pub fn new(home: &Home, name: &str) -> Counter {
        Counter {
            path: home.workspace().join(name),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 让 `shell` 追加一行的命令。
    pub fn append_command(&self) -> String {
        format!("echo once >> {}", self.path.display())
    }

    /// 跑了几次。
    pub fn count(&self) -> usize {
        std::fs::read_to_string(&self.path)
            .map(|text| text.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------- 事件小工具

pub fn of_run<'a>(events: &'a [Event], run: &RunId) -> Vec<&'a Event> {
    events
        .iter()
        .filter(|event| event.run.as_ref() == Some(run))
        .collect()
}

pub fn tool_started(events: &[Event]) -> Vec<&komo_kernel::events::ToolStarted> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolStarted(body) => Some(body),
            _ => None,
        })
        .collect()
}

pub fn tool_results(events: &[Event]) -> Vec<&komo_kernel::events::ToolResult> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolResult(body) => Some(body),
            _ => None,
        })
        .collect()
}

/// 这个 Run 的结果事件的状态，按顺序。
pub fn result_statuses(events: &[Event]) -> Vec<komo_kernel::types::refs::ToolResultStatus> {
    tool_results(events)
        .iter()
        .map(|result| result.status)
        .collect()
}

pub fn planned_calls(events: &[Event]) -> Vec<ToolCallId> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolPlanned(body) => Some(body.call_id.clone()),
            _ => None,
        })
        .collect()
}

/// 模型这一轮要求的调用号（`message.assistant` 里的）。
pub fn requested_calls(events: &[Event]) -> Vec<ToolCallId> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::MessageAssistant(body) => Some(body.tool_calls.clone()),
            _ => None,
        })
        .flatten()
        .map(|call| call.call_id)
        .collect()
}

/// **事件配对**：每个 `tool.started` 有且只有一个 `tool.result`，或者一个明确的
/// uncertain / 中断说明。返回没有配上的那些 attempt。
pub fn unpaired_attempts(events: &[Event]) -> Vec<AttemptId> {
    let settled: Vec<AttemptId> = tool_results(events)
        .iter()
        .map(|result| result.attempt_id.clone())
        .collect();
    tool_started(events)
        .iter()
        .map(|started| started.attempt_id.clone())
        .filter(|attempt| !settled.contains(attempt))
        .collect()
}

pub fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
