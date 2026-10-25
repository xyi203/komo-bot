//! 把脚本化的 [`LlmClient`]（`begin_turn` → [`TurnDriver`]）演成 Durable 的 [`ModelStream`]。
//!
//! 旧测试里的模型替身大多是"按这一段最后一条用户输入分流"的 `LlmClient`（`TasksLlm`、`MonitorLlm`
//! 一类）。Durable 的 harness 走无状态的 `ModelStream`：每次请求给完整消息序列、没有 driver。这个适配器
//! 与 [`FakeLlm`](super::harness::FakeLlm) 的 `ModelStream` 同一个规矩：
//!
//! - 最后一条非 system 消息是**用户输入**：把请求投成旧 `TurnRequest` 的形状交给 `begin_turn`，取
//!   driver，`next(First)` 得第一轮，driver 按"回合身份"（缓存键 + 最后一条用户消息的位置与正文）留着；
//! - 最后一条是**工具结果**且有 driver：`next(ToolResults)` 取下一轮；
//! - 工具结果但没有 driver（重启之后的续跑）：重新 `begin_turn`（请求里已经带着那些工具结果，脚本据此
//!   分流），`next(First)`。
//!
//! 局限同 `FakeLlm`：`TurnRequest.session` 取缓存键，`run` 恒空，`cache_session` 恒空。
//!
//! 文件后半是迁到 Durable 的集成测试（`tests/{monitor,detach,agents}`）共用的几个助手：直接调
//! `DurableCore::submit_input`（不经 HTTP，v2 路由是 W2a 的），取回答、按请求键前缀找内部输入。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::durable::storage::SubmissionQuery;
use komo_kernel::durable::{
    AssistantMessage, ContentBlock, Message, Storage, SubmissionRecord, SubmissionStatus,
};
use komo_kernel::traits::{LlmClient, TurnDriver};
use komo_kernel::types::ids::RunId;
use komo_kernel::types::turn::{LlmError, RoundInput};
use komo_runtime::llm::{ModelRequest, ModelStream};

use super::harness::{TestGateway, assistant_of, stream_key, trailing_tool_results, turn_request};
use crate::service::durable::DurableCore;
use crate::service::durable::sessions::NewSession;
use crate::service::durable::submit::{Accepted, InputRequest};

pub struct DriverStream {
    llm: Arc<dyn LlmClient>,
    drivers: tokio::sync::Mutex<HashMap<String, Box<dyn TurnDriver>>>,
    /// 取过的轮数，给断言用。
    turns: Mutex<usize>,
}

impl DriverStream {
    pub fn new(llm: Arc<dyn LlmClient>) -> Arc<Self> {
        Arc::new(DriverStream {
            llm,
            drivers: tokio::sync::Mutex::new(HashMap::new()),
            turns: Mutex::new(0),
        })
    }

    /// 向模型要过几轮（含工具结果之后的续轮）。
    pub fn rounds(&self) -> usize {
        *self.turns.lock().expect("轮数")
    }
}

#[async_trait]
impl ModelStream for DriverStream {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        _progress: &mut (dyn for<'m> FnMut(&'m AssistantMessage) + Send),
    ) -> Result<AssistantMessage, LlmError> {
        let key = stream_key(request);
        let continuing = matches!(
            request
                .messages
                .iter()
                .rev()
                .find(|message| !message.is_system()),
            Some(Message::ToolResult(_))
        );
        let mut driver = if continuing {
            self.drivers.lock().await.remove(&key)
        } else {
            None
        };
        let round = match driver.as_mut() {
            Some(driver) => {
                let results = trailing_tool_results(request.messages);
                driver
                    .next(RoundInput::ToolResults {
                        results,
                        revised: Vec::new(),
                    })
                    .await?
            }
            None => {
                let mut fresh = self.llm.begin_turn(turn_request(request)).await?;
                let round = fresh.next(RoundInput::First).await?;
                driver = Some(fresh);
                round
            }
        };
        *self.turns.lock().expect("轮数") += 1;
        if !round.tool_calls.is_empty()
            && !round.truncated
            && let Some(driver) = driver
        {
            self.drivers.lock().await.insert(key, driver);
        }
        Ok(assistant_of(request.model, &round))
    }
}

// ---------------------------------------------------------------- 逐次请求的记录

/// 一次模型请求的样子（`RecordingStream` 记的）。
#[derive(Debug, Clone)]
pub struct SeenRequest {
    pub model: String,
    pub effort: Option<String>,
    /// 回放出来的系统提示（各段拼起来）。
    pub system_prompt: String,
    pub tools: Vec<String>,
    /// 最后一条非 system 消息是工具结果（续轮）。
    pub continuing: bool,
    /// 请求里全部工具结果消息：`(tool_call_id, 正文)`，按出现顺序（看哪一个换成了短视图）。
    pub tool_results: Vec<(String, String)>,
}

/// 包一层 [`ModelStream`]，记下**每一次**请求（含续轮）。`FakeLlm` 的 `requests` 只记"新一轮"的第一
/// 次请求，续轮看不到系统提示与模型；要看"在飞的 run 后面那次请求带的是哪一版"就用它。
///
/// `hold_next` 让**下一次**请求记完之后停住，直到 `release`——"请求已发出、配置在这时被改"的窗口。
pub struct RecordingStream {
    inner: Arc<dyn ModelStream>,
    seen: Mutex<Vec<SeenRequest>>,
    hold: Mutex<bool>,
    released: tokio::sync::Notify,
}

impl RecordingStream {
    pub fn new(inner: Arc<dyn ModelStream>) -> Arc<Self> {
        Arc::new(RecordingStream {
            inner,
            seen: Mutex::new(Vec::new()),
            hold: Mutex::new(false),
            released: tokio::sync::Notify::new(),
        })
    }

    pub fn seen(&self) -> Vec<SeenRequest> {
        self.seen.lock().expect("请求记录").clone()
    }

    /// 下一次请求记完就停住。
    pub fn hold_next(&self) {
        *self.hold.lock().expect("hold") = true;
    }

    pub fn release(&self) {
        self.released.notify_one();
    }

    /// 等记到至少 `n` 次请求（最多 20 秒）。
    pub async fn wait_seen(&self, n: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while self.seen.lock().expect("请求记录").len() < n {
            assert!(
                std::time::Instant::now() < deadline,
                "等了 20 秒也没有第 {n} 次请求"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

#[async_trait]
impl ModelStream for RecordingStream {
    async fn stream(
        &self,
        request: &ModelRequest<'_>,
        progress: &mut (dyn for<'m> FnMut(&'m AssistantMessage) + Send),
    ) -> Result<AssistantMessage, LlmError> {
        let system = komo_runtime::llm::replay_system(request.messages);
        let continuing = matches!(
            request
                .messages
                .iter()
                .rev()
                .find(|message| !message.is_system()),
            Some(Message::ToolResult(_))
        );
        self.seen.lock().expect("请求记录").push(SeenRequest {
            model: request.model.model.clone(),
            effort: request.model.effort.as_ref().map(ToString::to_string),
            system_prompt: system.prompt.clone(),
            tools: system.tools.iter().map(|tool| tool.name.clone()).collect(),
            continuing,
            tool_results: request
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::ToolResult(result) => Some((
                        result.tool_call_id.clone(),
                        result
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text, .. } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )),
                    _ => None,
                })
                .collect(),
        });
        let hold = std::mem::take(&mut *self.hold.lock().expect("hold"));
        if hold {
            self.released.notified().await;
        }
        self.inner.stream(request, progress).await
    }
}

// ---------------------------------------------------------------- 迁到 Durable 的测试用的助手

pub fn core(gateway: &TestGateway) -> Arc<DurableCore> {
    Arc::clone(gateway.state().core().expect("Durable 模式"))
}

/// 开一个默认 Agent（`assistant`）的普通会话，返回 Session ID。
pub async fn open_session(gateway: &TestGateway) -> String {
    core(gateway)
        .create_session(NewSession::new("assistant"))
        .await
        .expect("建得了会话")
        .id
}

/// 提交一条输入（请求键 `key`）。
pub async fn submit(gateway: &TestGateway, session: &str, key: &str, text: &str) -> Accepted {
    core(gateway)
        .submit_input(session, InputRequest::new(text).request_id(key))
        .await
        .expect("受理得了")
}

/// 一条 assistant entry 里的文字。
async fn assistant_text(storage: &dyn Storage, entry: komo_kernel::durable::EntryId) -> String {
    let stored = storage
        .entry(entry)
        .await
        .expect("读得出")
        .expect("回答 entry 在");
    stored
        .entry
        .model
        .iter()
        .flatten()
        .find_map(|message| match message {
            Message::Assistant(message) => Some(
                message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

/// 一条已结算的输入 submission 的回答文字；没被回答（被撤、失败……）就 panic。
pub async fn answer_of(gateway: &TestGateway, record: &SubmissionRecord) -> String {
    use komo_kernel::durable::{InputState, SubmissionKind};
    match &record.kind {
        SubmissionKind::Input {
            state: InputState::Done { answer, .. },
        } => assistant_text(&*core(gateway).storage, *answer).await,
        other => panic!("submission {} 没有被回答：{other:?}", record.id),
    }
}

/// 等这条 run 的输入结算，返回模型的最终回复；输入没被回答（被撤、失败……）就 panic。
pub async fn reply_of(gateway: &TestGateway, run: &RunId) -> String {
    let record = gateway.wait_run(run).await;
    answer_of(gateway, &record).await
}

/// 提交一条输入、等它收尾、返回回复。
pub async fn say(gateway: &TestGateway, session: &str, key: &str, text: &str) -> String {
    let accepted = submit(gateway, session, key, text).await;
    reply_of(gateway, &accepted.run).await
}

/// 请求键以 `prefix` 开头的全部 submission（任何 conversation，按 ID 序）。
pub async fn submissions_keyed(gateway: &TestGateway, prefix: &str) -> Vec<SubmissionRecord> {
    let page = core(gateway)
        .storage
        .scan_submissions(&SubmissionQuery::default(), 10_000, None)
        .await
        .expect("读得出");
    page.items
        .into_iter()
        .filter(|record| {
            record
                .request_id
                .as_deref()
                .is_some_and(|id| id.starts_with(prefix))
        })
        .collect()
}

/// 等到有一条请求键以 `prefix` 开头、已经结算的 submission（最多 `within`），返回它。
pub async fn wait_keyed(
    gateway: &TestGateway,
    prefix: &str,
    within: std::time::Duration,
) -> SubmissionRecord {
    let deadline = std::time::Instant::now() + within;
    loop {
        if let Some(record) = submissions_keyed(gateway, prefix)
            .await
            .into_iter()
            .find(|r| {
                matches!(
                    r.status(),
                    SubmissionStatus::Done | SubmissionStatus::Unanswered
                )
            })
        {
            return record;
        }
        if std::time::Instant::now() >= deadline {
            panic!("等了 {within:?} 也没有请求键以 {prefix} 开头、已结算的 submission");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
