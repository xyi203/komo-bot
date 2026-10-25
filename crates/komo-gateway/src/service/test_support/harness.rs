//! 集成测试目标共用的那一台 Gateway。
//!
//! 「真数据目录、真 `service::start`、内存发送口、脚本化模型」搬到这里，各目标的 `harness.rs`
//! 只留自己特有的那几个助手——cron 的触发记录断言、memory 的假向量后端。
//!
//! **真的东西一个没少**：真 config.toml、真 durable.db / aux.db、真 TcpListener、真 harness
//! 引擎、真 Dispatcher、真 `HomeNotifier`。替身只有两个——模型与渠道，因为这两样一个要钱、一个
//! 要网。
//!
//! 两套门面，因为两种测试问的是两个问题：
//!
//! - [`GatewayBuilder`] / [`TestGateway`]：**一台起着的 Gateway**，围着它发 HTTP、喂
//!   入站消息（chat / memory）。
//! - [`Home`] / [`Gw`]：**一个数据目录**，可以反复起落（cron / recovery 的"重启之后"）。
//!
//! 它们共用同一个 `start`，分别只在"谁持有临时目录"。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::durable::{
    AssistantMessage, Content, ContentBlock, Message, StopReason, ToolResultMessage,
};
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_kernel::protocol::http::{InterventionSummary, RunDetail, SubmitRunResponse};
use komo_kernel::protocol::{InboundAck, InboundMessage};
use komo_kernel::traits::{
    Channel, ChannelError, DeliverError, EmbeddingClient, Inbound, LlmClient, Tool, TurnDriver,
};
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, Outbound};
use komo_kernel::types::ids::{RunId, Seq, SessionId, ToolCallId};
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::status::RunState;
use komo_kernel::types::turn::{
    LlmError, ProviderToolCall, ReplayMessage, Role, Round, RoundInput, ToolCallRequest,
    ToolResultForModel, TurnRequest,
};
use komo_runtime::config::Secrets;
use komo_runtime::llm::{ModelRequest, ModelStream, replay_system};

use crate::channels::{BuiltChannel, ChannelFactory, ChannelRegistry, ChannelSender, SendOutcome};
use crate::service::state::GatewayState;
use crate::service::{Running, ServiceOptions, start};

// ---------------------------------------------------------------- 配置文本

/// 一份能通过校验的最小 config.toml，`extra` 追加在后面。
pub fn config_toml(extra: &str) -> String {
    format!(
        r#"
default_agent = "assistant"

[agents.assistant]
[model.main]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[model.job]
type = "completion"
api_backend = "responses"
base_url = "https://jobs.example.com/v1"
model = "job-model"
api_key_env = "KOMO_LLM_API_KEY"

[models]
default = "main"

[memory]
enabled = false
{extra}
"#
    )
}

/// `[channels.telegram]`：操作者 111，home chat 111。
pub fn telegram_block(allow_from: &str) -> String {
    format!(
        r#"
[channels.telegram]
enabled = true
allow_from = [{allow_from}]
home_chat = 111
"#
    )
}

/// `[channels.feishu]`：操作者 `ou_op`，home chat `oc_home`。
pub fn feishu_block(allow_from: &str) -> String {
    format!(
        r#"
[channels.feishu]
enabled = true
allow_from = [{allow_from}]
home_chat = "oc_home"
"#
    )
}

/// `[channels.wechat]`：操作者 `wxid_op`，home chat 也是他（微信只有 DM）。
pub fn wechat_block(allow_from: &str) -> String {
    format!(
        r#"
[channels.wechat]
enabled = true
allow_from = [{allow_from}]
home_chat = "wxid_op"
"#
    )
}

/// 默认：只有 Telegram 一个渠道。
pub fn telegram_config(allow_from: &str) -> String {
    config_toml(&telegram_block(allow_from))
}

/// [`Home`] 的默认配置：**配了 home chat 的** Telegram 渠道。
///
/// 无人值守的那些测试（cron / recovery）要的正是这一份：要人处理的介入有一个出口，而
/// "群里说话"不在它们的范围里。
pub fn home_config() -> String {
    config_toml(
        r#"
[channels.telegram]
enabled = true
allow_from = [111]
home_chat = 111
"#,
    )
}

pub const DEFAULT_ENV: &str = "KOMO_LLM_API_KEY=test-key\nTELEGRAM_BOT_TOKEN=test-bot-token\n";

/// 写出一个数据目录。不写 `policy.toml`：它已经不再被读（§7，2026-10-11）。
pub fn write_home(home: &Path, config: &str, env: &str) {
    std::fs::create_dir_all(home).expect("数据目录");
    std::fs::write(home.join("config.toml"), config).expect("写 config.toml");
    std::fs::write(home.join(".env"), env).expect("写 .env");
    // **故意不建 `workspaces/`**：它是数据目录骨架的一部分，该由 Gateway 自己在启动时
    // 建（§12）。这里替它建过，于是生产路径上少了那一步也一直看不出来——每一个 bash
    // 调用都在报"程序起不来"，而真正不在的是会话 cwd。
}

// ---------------------------------------------------------------- 渠道替身

/// 送出去的一条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    pub peer: ChannelPeer,
    pub outbound: Outbound,
}

/// 一个内存发送口：`send` 记在 `sent` 里，可以被调成"此刻推不出去"、也可以被扣住。
#[derive(Debug)]
pub struct MemSender {
    platform: ChannelPlatform,
    sent: Mutex<Vec<SentMessage>>,
    deferring: std::sync::atomic::AtomicBool,
    /// 扣住时每条发送都停在平台往返上，直到放开。**启动补发是网络 I/O**：扣住它，
    /// "就绪等不等补发"就是一个确定的问题，不必拿墙钟去量（§11.4 的补发不得压在就绪之前）。
    held: tokio::sync::watch::Sender<bool>,
}

impl MemSender {
    pub fn new(platform: ChannelPlatform) -> Arc<Self> {
        Arc::new(MemSender {
            platform,
            sent: Mutex::new(Vec::new()),
            deferring: std::sync::atomic::AtomicBool::new(false),
            held: tokio::sync::watch::Sender::new(false),
        })
    }

    pub fn sent(&self) -> Vec<SentMessage> {
        self.sent.lock().expect("发送记录").clone()
    }

    pub fn texts(&self) -> Vec<String> {
        self.sent()
            .into_iter()
            .filter_map(|message| match message.outbound {
                Outbound::Text { text } => Some(text),
                Outbound::RunFinished { summary, .. } => Some(summary),
                Outbound::NeedsAttention { reason, .. } => Some(reason),
                // 不是文本：只是告诉渠道"这条并进别的 Run 了"。
                Outbound::RunAbsorbed { .. } => None,
            })
            .collect()
    }

    /// 送到某个会话的那些。
    pub fn to_chat(&self, chat: &str) -> Vec<SentMessage> {
        self.sent()
            .into_iter()
            .filter(|message| message.peer.chat_id.as_str() == chat)
            .collect()
    }

    /// 微信那条路径：没有回复令牌时 `Deferred`。
    pub fn defer(&self, deferring: bool) {
        self.deferring
            .store(deferring, std::sync::atomic::Ordering::SeqCst);
    }

    /// 扣住之后的每条发送都停在"跟平台往返"上，直到 [`Self::release`]。
    pub fn hold(&self) {
        self.held.send_replace(true);
    }

    pub fn release(&self) {
        self.held.send_replace(false);
    }
}

#[async_trait]
impl ChannelSender for MemSender {
    fn platform(&self) -> ChannelPlatform {
        self.platform
    }

    async fn send(&self, peer: &ChannelPeer, msg: Outbound) -> Result<SendOutcome, DeliverError> {
        let _ = self.held.subscribe().wait_for(|held| !*held).await;
        if self.deferring.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(SendOutcome::Deferred {
                reason: "没有回复令牌".into(),
            });
        }
        self.sent.lock().expect("发送记录").push(SentMessage {
            peer: peer.clone(),
            outbound: msg,
        });
        Ok(SendOutcome::Sent)
    }
}

/// 一个什么都不收的 `Channel`：`serve` 只是把形状补全，入站由测试自己驱动。
pub struct IdleChannel;

#[async_trait]
impl Channel for IdleChannel {
    fn name(&self) -> &'static str {
        "idle"
    }

    async fn serve(
        &self,
        _inbound: Arc<dyn Inbound>,
        shutdown: komo_kernel::traits::Shutdown,
    ) -> Result<(), ChannelError> {
        while !shutdown.is_cancelled() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(())
    }
}

/// 把一对**已经造好**的收发两半当成一个工厂交给 `service::start`。
///
/// `build` 照 `ChannelFactory` 的契约看当前快照：`enabled = false` → `None`，于是热重载
/// 时"停掉这一个"这条路也走得通。
pub struct FixedFactory {
    platform: ChannelPlatform,
    channel: Arc<dyn Channel>,
    sender: Arc<dyn ChannelSender>,
}

impl FixedFactory {
    pub fn new(
        platform: ChannelPlatform,
        channel: Arc<dyn Channel>,
        sender: Arc<dyn ChannelSender>,
    ) -> Arc<Self> {
        Arc::new(FixedFactory {
            platform,
            channel,
            sender,
        })
    }

    /// 只有出站一半（入站由测试直接调 Dispatcher）。
    pub fn sender_only(sender: Arc<dyn ChannelSender>) -> Arc<Self> {
        let platform = sender.platform();
        Self::new(platform, Arc::new(IdleChannel), sender)
    }
}

#[async_trait]
impl ChannelFactory for FixedFactory {
    fn platform(&self) -> ChannelPlatform {
        self.platform
    }

    fn build(
        &self,
        snapshot: &ConfigSnapshot,
        _secrets: &Secrets,
    ) -> Result<Option<BuiltChannel>, ChannelError> {
        if !snapshot
            .channels
            .get(self.platform)
            .is_some_and(|channel| channel.enabled)
        {
            return Ok(None);
        }
        Ok(Some(BuiltChannel {
            channel: Arc::clone(&self.channel),
            sender: Arc::clone(&self.sender),
        }))
    }
}

// ---------------------------------------------------------------- 脚本化模型

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

/// 一轮里**多个**调用（`provider_id` / 工具名 / 参数各一份）。
///
/// 一轮多调用是一种独立形状：第一个调用停下时，后面的连 `tool.planned` 都还没有，
/// 恢复要照样把它们跑完。
pub fn multi_call_round(
    n: u32,
    calls: &[(&str, &str, serde_json::Value)],
) -> Result<Round, LlmError> {
    Ok(Round {
        round: n,
        text: None,
        tool_calls: calls
            .iter()
            .map(|(provider_id, tool, args)| ProviderToolCall {
                provider_call_id: provider_id.to_string(),
                name: tool.to_string(),
                arguments: args.clone(),
            })
            .collect(),
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
    /// 每次 `TurnDriver::next` 拿到的**轮输入**。
    ///
    /// `requests` 只说明"模型被问了什么"，这一条才说明"它拿到手的是什么"——工具结果是通过
    /// 轮输入回去的（不是回放窗口里的消息），"父到底收没收到子代理的结果"（§4）只有这里
    /// 答得出来。
    pub inputs: Arc<Mutex<Vec<RoundInput>>>,
    /// `ModelStream` 那一侧：每个"回合"（一条用户输入起、到下一条用户输入止）手上还剩的脚本，
    /// 按 [`stream_key`] 记。无状态的 `stream` 没有 driver 可握，续轮只能从这里取。
    running: Mutex<
        std::collections::BTreeMap<String, std::collections::VecDeque<Result<Round, LlmError>>>,
    >,
}

impl FakeLlm {
    pub fn new(scripts: Vec<Vec<Result<Round, LlmError>>>) -> Arc<FakeLlm> {
        Arc::new(FakeLlm {
            scripts: Mutex::new(scripts.into_iter().collect()),
            fallback: Mutex::new(vec![text_round(99, "（没有别的要做了）")]),
            turns: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            inputs: Arc::new(Mutex::new(Vec::new())),
            running: Mutex::new(std::collections::BTreeMap::new()),
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

    /// 这些回合各自用的是哪个模型（§13.3：不串用模型配置，断言的就是它）。
    pub fn models(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("脚本模型")
            .iter()
            .map(|request| request.model.model.clone())
            .collect()
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
            inputs: Arc::clone(&self.inputs),
        }))
    }
}

// ---------------------------------------------------------------- ModelStream

/// Durable 模式的 harness 走 [`ModelStream`]：每次请求给完整消息序列、返回一条 assistant
/// 消息，没有 driver。`FakeLlm` 用同一份脚本、同一组观察口把它演出来，所以现有测试对
/// `requests` / `turns()` / `inputs` 的断言在 Durable 下**不用改**：
///
/// - 最后一条非 system 消息是**用户输入**（或没有可续的脚本）：开一段新脚本，`turns += 1`，
///   `requests` 记一份 [`TurnRequest`] 形状的请求，`inputs` 记 `RoundInput::First`；
/// - 最后一条是**工具结果**：取当前这段脚本的下一轮，`inputs` 记 `ToolResults`（`revised`
///   恒为空：衰减在 Durable 下就是下一次请求里那条消息换了视图）。
///
/// 局限：请求里没有 session / run，`TurnRequest.session` 取缓存键（= 会话 ID，没有就空），
/// `run` 恒为空；`memories` / `covers` / `cache_session` 恒为空。
#[async_trait]
impl ModelStream for FakeLlm {
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
        ) && self.running.lock().expect("脚本模型").contains_key(&key);

        let round = if continuing {
            let results = trailing_tool_results(request.messages);
            self.inputs
                .lock()
                .expect("脚本模型")
                .push(RoundInput::ToolResults {
                    results,
                    revised: Vec::new(),
                });
            self.running
                .lock()
                .expect("脚本模型")
                .get_mut(&key)
                .and_then(std::collections::VecDeque::pop_front)
        } else {
            self.turns.fetch_add(1, Ordering::SeqCst);
            self.requests
                .lock()
                .expect("脚本模型")
                .push(turn_request(request));
            self.inputs
                .lock()
                .expect("脚本模型")
                .push(RoundInput::First);
            let rounds = {
                let mut scripts = self.scripts.lock().expect("脚本模型");
                match scripts.pop_front() {
                    Some(rounds) => rounds,
                    None => self.fallback.lock().expect("脚本模型").clone(),
                }
            };
            let mut rounds: std::collections::VecDeque<_> = rounds.into_iter().collect();
            let first = rounds.pop_front();
            self.running.lock().expect("脚本模型").insert(key, rounds);
            first
        };
        // 脚本演完：给一句收尾，而不是一个错误（理由见 `FakeLlm` 的注释）。
        let round = round.unwrap_or_else(|| text_round(99, "（脚本演完了，收尾）"))?;
        Ok(assistant_of(request.model, &round))
    }
}

/// 一个回合的身份：缓存键 + 最后一条用户消息的位置与正文。续轮请求与开头那一条请求同键。
pub(super) fn stream_key(request: &ModelRequest<'_>) -> String {
    let (index, text) = request
        .messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| match message {
            Message::User(user) => Some((index, content_text(&user.content))),
            _ => None,
        })
        .unwrap_or((0, String::new()));
    format!("{}|{index}|{text}", request.options.cache_key.unwrap_or(""))
}

fn content_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn tool_result_of(result: &ToolResultMessage) -> ToolResultForModel {
    ToolResultForModel {
        provider_call_id: result.tool_call_id.clone(),
        call_id: ToolCallId::from_raw(result.tool_call_id.clone()),
        content: result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        is_error: result.is_error,
        images: Vec::new(),
    }
}

/// 消息序列末尾那一串连续的工具结果（一轮的全部结果）。
pub(super) fn trailing_tool_results(messages: &[Message]) -> Vec<ToolResultForModel> {
    let mut results: Vec<_> = messages
        .iter()
        .rev()
        .take_while(|message| matches!(message, Message::ToolResult(_)))
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(tool_result_of(result)),
            _ => None,
        })
        .collect();
    results.reverse();
    results
}

/// 把请求投成旧 `TurnRequest` 的形状：系统提示与工具表是 system 消息回放的结果，user /
/// assistant 各一条，紧挨着的工具结果并成一条 `Role::Tool`。
pub(super) fn turn_request(request: &ModelRequest<'_>) -> TurnRequest {
    let system = replay_system(request.messages);
    let mut messages: Vec<ReplayMessage> = Vec::new();
    let mut index = 0;
    let body: Vec<&Message> = request
        .messages
        .iter()
        .filter(|message| !message.is_system())
        .collect();
    while let Some(message) = body.get(index) {
        let seq = Seq(index as u64);
        match message {
            Message::User(user) => messages.push(ReplayMessage {
                role: Role::User,
                seq,
                text: Some(content_text(&user.content)),
                tool_calls: Vec::new(),
                tool_results: Vec::new(),
                provider_blocks: None,
            }),
            Message::Assistant(assistant) => {
                let text: String = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                messages.push(ReplayMessage {
                    role: Role::Assistant,
                    seq,
                    text: (!text.is_empty()).then_some(text),
                    tool_calls: assistant
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolCall {
                                id,
                                name,
                                arguments,
                                ..
                            } => Some(ToolCallRequest {
                                call_id: ToolCallId::from_raw(id.clone()),
                                provider_call_id: id.clone(),
                                name: name.clone(),
                                arguments: arguments.clone(),
                                arguments_ref: None,
                            }),
                            _ => None,
                        })
                        .collect(),
                    tool_results: Vec::new(),
                    provider_blocks: None,
                });
            }
            Message::ToolResult(_) => {
                let mut results = Vec::new();
                while let Some(Message::ToolResult(result)) = body.get(index) {
                    results.push(tool_result_of(result));
                    index += 1;
                }
                messages.push(ReplayMessage {
                    role: Role::Tool,
                    seq,
                    text: None,
                    tool_calls: Vec::new(),
                    tool_results: results,
                    provider_blocks: None,
                });
                continue;
            }
            Message::System(_) => {}
        }
        index += 1;
    }
    TurnRequest {
        session: SessionId::from_raw(request.options.cache_key.unwrap_or("")),
        run: RunId::from_raw(""),
        model: request.model.clone(),
        system_prompt: system.prompt,
        messages,
        tools: system.tools,
        memories: Vec::new(),
        covers: None,
        cache_session: None,
    }
}

/// 脚本里的一轮 → 一条 assistant 消息（`extra` 同真 provider：api / provider / model / usage）。
pub(super) fn assistant_of(
    model: &komo_kernel::types::model::ModelConfig,
    round: &Round,
) -> AssistantMessage {
    let mut content = Vec::new();
    if let Some(text) = round.text.as_deref().filter(|text| !text.is_empty()) {
        content.push(ContentBlock::text(text));
    }
    // 被截断的一轮不含任何工具调用（`ModelStream` 的契约）。
    if !round.truncated {
        for call in &round.tool_calls {
            content.push(ContentBlock::tool_call(
                call.provider_call_id.clone(),
                call.name.clone(),
                call.arguments.clone(),
            ));
        }
    }
    let stop_reason = if round.truncated {
        StopReason::Length
    } else if !round.tool_calls.is_empty() {
        StopReason::ToolUse
    } else {
        StopReason::Stop
    };
    let mut extra = serde_json::Map::new();
    extra.insert("api".into(), serde_json::json!("openai-responses"));
    extra.insert("provider".into(), serde_json::json!(model.provider));
    extra.insert("model".into(), serde_json::json!(model.model));
    extra.insert("usage".into(), komo_runtime::llm::usage_json(&round.usage));
    if let Some(blocks) = &round.provider_blocks {
        extra.insert(komo_runtime::llm::PROVIDER_BLOCKS.into(), blocks.clone());
    }
    // 回复的时间戳取现在：缓存冷不冷看的是上一条回复离现在多久（0 = 1970 年，等于"缓存早就冷了"，
    // 压缩 / 衰减会因此误判成 `cache_cold`）。
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64);
    AssistantMessage {
        content,
        stop_reason,
        timestamp,
        extra,
    }
}

struct FakeDriver {
    rounds: std::collections::VecDeque<Result<Round, LlmError>>,
    usage: TokenUsage,
    inputs: Arc<Mutex<Vec<RoundInput>>>,
}

#[async_trait]
impl TurnDriver for FakeDriver {
    async fn next(&mut self, input: RoundInput) -> Result<Round, LlmError> {
        self.inputs.lock().expect("脚本模型").push(input);
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

// ---------------------------------------------------------------- 起一台

/// 起一台 Gateway 要给的东西。
pub struct GatewayBuilder {
    config: String,
    env: String,
    llm: Option<Arc<dyn LlmClient>>,
    embeddings: Option<Arc<dyn EmbeddingClient>>,
    factories: Vec<Arc<dyn ChannelFactory>>,
    home: Option<PathBuf>,
    tools: Vec<Arc<dyn Tool>>,
    stream: Option<Arc<dyn ModelStream>>,
}

impl GatewayBuilder {
    pub fn new(config: &str) -> Self {
        GatewayBuilder {
            config: config.to_string(),
            env: DEFAULT_ENV.to_string(),
            llm: None,
            embeddings: None,
            factories: Vec::new(),
            home: None,
            tools: Vec::new(),
            stream: None,
        }
    }

    /// 注入无状态的模型后端（harness 的主模型）。
    pub fn stream(mut self, stream: Arc<dyn ModelStream>) -> Self {
        self.stream = Some(stream);
        self
    }

    /// `FakeLlm` 同时是 `LlmClient` 与 `ModelStream`：两条路都装上同一个，脚本与观察口共用。
    pub fn fake_llm(mut self, llm: Arc<FakeLlm>) -> Self {
        self.llm = Some(Arc::clone(&llm) as Arc<dyn LlmClient>);
        self.stream = Some(llm as Arc<dyn ModelStream>);
        self
    }

    /// 多挂一个测试工具（[`super::Gate`]）。
    pub fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn env(mut self, env: &str) -> Self {
        self.env = env.to_string();
        self
    }

    pub fn llm(mut self, llm: Arc<dyn LlmClient>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// 注入向量后端（`tests/memory` 用）。
    pub fn embeddings(mut self, client: Arc<dyn EmbeddingClient>) -> Self {
        self.embeddings = Some(client);
        self
    }

    pub fn factory(mut self, factory: Arc<dyn ChannelFactory>) -> Self {
        self.factories.push(factory);
        self
    }

    /// 复用一个已有的数据目录（"停机再起来"的测试用）。
    pub fn at(mut self, home: &Path) -> Self {
        self.home = Some(home.to_path_buf());
        self
    }

    pub async fn start(self) -> TestGateway {
        install_crypto();
        let (home_dir, home) = match self.home {
            Some(path) => (None, path),
            None => {
                let dir = tempfile::tempdir().expect("临时数据目录");
                let path = dir.path().to_path_buf();
                (Some(dir), path)
            }
        };
        write_home(&home, &self.config, &self.env);
        super::install_test_tools(&home, self.tools);

        let running = start(ServiceOptions {
            home: Some(home.clone()),
            // 端口 0：内核挑一个空的，发现文件里写的是真实地址。
            listen: Some("127.0.0.1:0".into()),
            channels: self.factories.clone(),
            llm: self.llm,
            stream: self.stream,
            embeddings: self.embeddings,
            // 共享 skill 目录也钉在这份临时数据目录上（§5.6）：否则跑测试那台机器上
            // `~/.agents/skills` 里有什么，别的测试的断言就跟着变。
            shared_home: Some(home.clone()),
            telemetry: None,
        })
        .await
        .expect("Gateway 起得来");

        TestGateway {
            _home_dir: home_dir,
            home,
            env: self.env,
            running: Some(running),
            factories: self.factories,
        }
    }
}

/// 一台起着的 Gateway。
pub struct TestGateway {
    /// 临时目录的所有权；`None` = 目录是别人的（`at` 复用的那一种）。
    _home_dir: Option<tempfile::TempDir>,
    pub home: PathBuf,
    /// 起这一台时用的 `.env`——重启要用**同一份**，否则第二次起来会缺钥匙。
    env: String,
    running: Option<Running>,
    factories: Vec<Arc<dyn ChannelFactory>>,
}

impl TestGateway {
    /// 默认配置：一个 Telegram 渠道（操作者 111）+ 一个内存发送口，`FakeLlm::finisher("好")` 当模型。
    pub async fn start() -> (TestGateway, Arc<MemSender>) {
        let sender = MemSender::new(ChannelPlatform::Telegram);
        let gateway = GatewayBuilder::new(&telegram_config("111"))
            .fake_llm(FakeLlm::finisher("好"))
            .factory(FixedFactory::sender_only(
                Arc::clone(&sender) as Arc<dyn ChannelSender>
            ))
            .start()
            .await;
        (gateway, sender)
    }

    fn running(&self) -> &Running {
        self.running.as_ref().expect("Gateway 还在跑")
    }

    pub fn state(&self) -> &Arc<GatewayState> {
        &self.running().state
    }

    pub fn dispatcher(&self) -> &Arc<crate::Dispatcher> {
        &self.running().dispatcher
    }

    /// 这个 Session 的全部合成事件（见 [`Gw::events`]）。
    pub async fn events(&self, session: &str) -> Vec<komo_kernel::events::Event> {
        events_of(self.state(), &SessionId::from_raw(session)).await
    }

    pub fn channels(&self) -> &Arc<ChannelRegistry> {
        &self.state().channels
    }

    pub fn base_url(&self) -> &str {
        &self.running().base_url
    }

    pub fn token(&self) -> &str {
        &self.running().state.token
    }

    pub async fn handle(&self, msg: InboundMessage) -> InboundAck {
        self.dispatcher()
            .handle(msg)
            .await
            .expect("Dispatcher 处理")
    }

    /// 改 config.toml（热重载的测试用）。
    pub fn write_config(&self, config: &str) {
        let path = self.home.join("config.toml");
        std::fs::write(&path, config).expect("写 config.toml");
        // mtime 的粒度可能是秒；把时间推一下，`changed_since` 才看得出来。
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        let _ = std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_times(std::fs::FileTimes::new().set_modified(later)));
    }

    pub async fn reload(&self) {
        crate::reload::reload(self.state())
            .await
            .expect("新配置装得上");
    }

    /// 停机（放锁、删发现文件）。数据目录留着。
    pub async fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop().await;
        }
    }

    /// 停机再按**同一个数据目录**起一台（"重启后 pending 补发"的测试用）。
    pub async fn restart_with(&mut self, config: &str, factories: Vec<Arc<dyn ChannelFactory>>) {
        self.stop().await;
        let mut builder = GatewayBuilder::new(config)
            .env(&self.env)
            .at(&self.home)
            .fake_llm(FakeLlm::finisher("好"));
        for factory in &factories {
            builder = builder.factory(Arc::clone(factory));
        }
        let TestGateway {
            running, factories, ..
        } = builder.start().await;
        self.running = running;
        self.factories = factories;
    }

    pub async fn get(&self, path: &str) -> (u16, String) {
        self.request(reqwest::Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: serde_json::Value) -> (u16, String) {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }

    /// 同上，但把响应体当 JSON 读（`tests/memory` 断言的是字段）。
    pub async fn get_json(&self, path: &str) -> (u16, serde_json::Value) {
        let (code, body) = self.get(path).await;
        (code, as_json(body))
    }

    pub async fn post_json(&self, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let (code, body) = self.post(path, body).await;
        (code, as_json(body))
    }

    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, String) {
        request(self.base_url(), self.token(), method, path, body).await
    }

    /// 还没送到的投递。
    pub async fn pending_deliveries(&self) -> Vec<komo_store::DeliveryRecord> {
        self.state()
            .notifier
            .log()
            .pending(None)
            .await
            .expect("读得出")
    }

    /// 待处理清单（两类一起，§7.5）。
    pub async fn interventions(&self) -> Vec<InterventionSummary> {
        let (code, body) = self.get_json("/v1/interventions").await;
        assert_eq!(code, 200, "{body}");
        serde_json::from_value(body["interventions"].clone()).expect("清单读得出")
    }

    /// 等清单里出现这条 Run 的介入（最多 20 秒）。
    pub async fn wait_intervention(&self, run: &RunId) -> InterventionSummary {
        wait_intervention_in(|| self.interventions(), run).await
    }

    /// `POST /v1/interventions/{handle}/answer`（§7.5），断言 200。
    pub async fn answer(&self, handle: &str, verdict: &str) -> String {
        let body = serde_json::json!({ "verdict": verdict });
        let (code, body) = self
            .post(&format!("/v1/interventions/{handle}/answer"), body)
            .await;
        assert_eq!(code, 200, "{body}");
        body
    }

    /// 记一次答复的接口（`POST /v1/interventions/{handle}/answer`）**不做 200 断言**的
    /// 版本：要验错误码（422）的时候用它。
    pub async fn answer_raw(&self, handle: &str, verdict: &str) -> (u16, serde_json::Value) {
        self.post_json(
            &format!("/v1/interventions/{handle}/answer"),
            serde_json::json!({ "verdict": verdict }),
        )
        .await
    }

    /// 等这条 run 的输入 submission 结算（`done` 或 `unanswered`，最多 20 秒），返回那条记录。
    /// `RunId` 是 run 首个输入 submission 的十进制 ID。
    pub async fn wait_run(&self, run: &RunId) -> komo_kernel::durable::SubmissionRecord {
        wait_run(self.state(), run).await
    }

    /// 全部会话（含已删除的）的"来源"：主会话是 `home`，其余是 `source`（`chat:{peer}`）。
    /// 任务会话与 cron 会话不算（聊天测试里没有它们）。
    pub async fn origins(&self) -> Vec<String> {
        use komo_kernel::durable::SessionKind;
        let core = self.state().core().expect("durable");
        core.list_sessions(true)
            .await
            .expect("读得出")
            .into_iter()
            .filter_map(|record| match record.kind {
                SessionKind::Main => Some("home".to_owned()),
                SessionKind::Normal => Some(record.source.unwrap_or_default()),
                SessionKind::Task | SessionKind::Cron => None,
            })
            .collect()
    }

    /// 来源为 `origin`（`home` 或 `chat:…`）的会话 ID。
    pub async fn session_of(&self, origin: &str) -> Option<String> {
        use komo_kernel::durable::SessionKind;
        let core = self.state().core().expect("durable");
        core.list_sessions(true)
            .await
            .expect("读得出")
            .into_iter()
            .find(|record| match record.kind {
                SessionKind::Main => origin == "home",
                _ => record.source.as_deref() == Some(origin),
            })
            .map(|record| record.id)
    }

    /// 把一个会话逻辑删除到 `deleted`（HTTP `POST /v1/sessions/{id}/delete {"now":true}`，
    /// 即 W4a 的 lifecycle：`closing` → 撤回排队输入 → abort → `deleted`）。测"被删的会话不再接消息"用。
    pub async fn soft_delete(&self, session: &str) {
        let (code, body) = self
            .post_json(
                &format!("/v1/sessions/{session}/delete"),
                serde_json::json!({ "now": true }),
            )
            .await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["state"], "deleted", "{body}");
    }

    /// 一个 Session 的全部合成事件（`Gw` 与 `TestGateway` 共用）。
    pub async fn events_of(
        state: &GatewayState,
        session: &SessionId,
    ) -> Vec<komo_kernel::events::Event> {
        let core = state.core().expect("Durable 模式");
        crate::service::durable::feed::backfill(&*core.storage, session.as_str(), Seq(0))
            .await
            .expect("读得出事件")
    }

    /// 一个会话里受理过的输入 submission 数（排队的、并进别人 run 的都算一条）。
    pub async fn inputs(&self, session: &str) -> usize {
        inputs_of(self.state(), session).await
    }
}

// ---------------------------------------------------------------- 一个数据目录

/// 一个真实的数据目录，可以反复起落。
pub struct Home {
    dir: tempfile::TempDir,
}

impl Default for Home {
    fn default() -> Self {
        Home::new()
    }
}

impl Home {
    pub fn new() -> Home {
        Home::with_config(&home_config())
    }

    pub fn with_config(config: &str) -> Home {
        install_crypto();
        let dir = tempfile::tempdir().expect("临时数据目录");
        write_home(dir.path(), config, DEFAULT_ENV);
        Home { dir }
    }

    /// 之后每次在这个数据目录起 Gateway 都多挂这几个测试工具（[`super::Gate`]）。
    pub fn install_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        super::install_test_tools(self.dir.path(), tools);
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

    /// 起一台 Gateway。**同一个数据目录**，再调一次就是"重启"。`llm` 同时当辅助调用的
    /// `LlmClient` 与 harness 的 `ModelStream`。
    pub async fn start(&self, llm: Arc<FakeLlm>) -> Gw {
        let sender = MemSender::new(ChannelPlatform::Telegram);
        self.start_with(llm, sender).await
    }

    /// 同上，但发送口自己给。
    pub async fn start_with(&self, llm: Arc<FakeLlm>, sender: Arc<MemSender>) -> Gw {
        let running = start(ServiceOptions {
            home: Some(self.dir.path().to_path_buf()),
            listen: Some("127.0.0.1:0".into()),
            channels: vec![FixedFactory::sender_only(
                Arc::clone(&sender) as Arc<dyn ChannelSender>
            )],
            llm: Some(Arc::clone(&llm) as Arc<dyn LlmClient>),
            stream: Some(llm as Arc<dyn ModelStream>),
            embeddings: None,
            shared_home: Some(self.dir.path().to_path_buf()),
            telemetry: None,
        })
        .await
        .expect("Gateway 起得来");
        let base = running.base_url.clone();
        let token = running.state.token.clone();
        Gw {
            running,
            base,
            token,
            home: sender,
        }
    }

    /// 停机之后单独打开 durable.db 读（同样要**先 stop**：Turso 对库文件持进程独占锁）。
    pub async fn open_durable(&self) -> komo_store::durable::TursoStorage {
        komo_store::durable::TursoStorage::open(self.dir.path())
            .await
            .expect("打开 durable.db")
    }
}

/// 一台起着的 Gateway（`Home` 起的那一种）。
pub struct Gw {
    pub running: Running,
    base: String,
    token: String,
    /// home chat 的那一半——无人值守的投递全在这里。
    pub home: Arc<MemSender>,
}

impl Gw {
    pub fn state(&self) -> &Arc<GatewayState> {
        &self.running.state
    }

    pub fn dispatcher(&self) -> &Arc<crate::Dispatcher> {
        &self.running.dispatcher
    }

    pub fn channels(&self) -> &Arc<ChannelRegistry> {
        &self.state().channels
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub async fn get(&self, path: &str) -> (u16, String) {
        self.request(reqwest::Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: serde_json::Value) -> (u16, String) {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }

    pub async fn patch(&self, path: &str, body: serde_json::Value) -> (u16, String) {
        self.request(reqwest::Method::PATCH, path, Some(body)).await
    }

    /// 同上，但把响应体当 JSON 读（§8.10 的 `PurgeBlocked` 正文就是一份数据）。
    pub async fn get_json(&self, path: &str) -> (u16, serde_json::Value) {
        let (code, body) = self.get(path).await;
        (code, as_json(body))
    }

    pub async fn post_json(&self, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let (code, body) = self.post(path, body).await;
        (code, as_json(body))
    }

    /// 待处理清单（两类一起，§7.5）。
    pub async fn interventions(&self) -> Vec<InterventionSummary> {
        let (code, body) = self.get_json("/v1/interventions").await;
        assert_eq!(code, 200, "{body}");
        serde_json::from_value(body["interventions"].clone()).expect("清单读得出")
    }

    /// 等清单里出现这条 Run 的介入（最多 20 秒）。
    pub async fn wait_intervention(&self, run: &RunId) -> InterventionSummary {
        wait_intervention_in(|| self.interventions(), run).await
    }

    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, String) {
        request(&self.base, &self.token, method, path, body).await
    }

    /// 开一个会话。
    pub async fn open_session(&self) -> SessionId {
        let (code, body) = self.post("/v1/sessions", serde_json::json!({})).await;
        assert_eq!(code, 200, "{body}");
        let summary: komo_kernel::protocol::http::SessionSummary =
            serde_json::from_str(&body).expect("会话");
        summary.session
    }

    /// 提交一条输入。
    pub async fn submit(&self, session: &SessionId, key: &str, text: &str) -> SubmitRunResponse {
        let (code, body) = self
            .post(
                &format!("/v1/sessions/{session}/runs"),
                serde_json::json!({ "request_key": key, "text": text }),
            )
            .await;
        assert_eq!(code, 200, "{body}");
        serde_json::from_str(&body).expect("提交")
    }

    pub async fn run_detail(&self, run: &RunId) -> RunDetail {
        self.try_run_detail(run).await.expect("读得到运行详情")
    }

    /// 这个 Session 的全部事件，按 seq（与 `GET /v1/sessions/{id}/events` 的补读同一个投影）。
    pub async fn events(&self, session: &SessionId) -> Vec<komo_kernel::events::Event> {
        events_of(self.state(), session).await
    }

    /// 同上，但读不出来时把**原话**带回来——一个中间损坏的会话该报损坏，不该是
    /// 一个 `None`（§8.5）。
    pub async fn try_run_detail(&self, run: &RunId) -> Result<RunDetail, String> {
        let (status, body) = self.get(&format!("/v1/runs/{run}")).await;
        if status != 200 {
            return Err(body);
        }
        serde_json::from_str(&body).map_err(|error| error.to_string())
    }

    /// 这个 Run 现在什么状态：由 submission / entry 拼（`GET /v1/runs/{id}` 读的就是那份），
    /// 所以读详情。
    pub async fn db_state(&self, run: &RunId) -> RunState {
        self.run_detail(run).await.summary.state
    }

    pub async fn wait_db_state<F: Fn(RunState) -> bool>(
        &self,
        run: &RunId,
        done: F,
        what: &str,
    ) -> RunState {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut last = RunState::Queued;
        while std::time::Instant::now() < deadline {
            last = self.db_state(run).await;
            if done(last) {
                return last;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("等了 20 秒 run {run} 还没到「{what}」，现在是 {last:?}");
    }

    pub async fn run_state(&self, run: &RunId) -> RunState {
        self.db_state(run).await
    }

    /// 这条 Run 现在**在等什么**（§8.4 的第二维）。`None` = 它没停在等待上。
    pub async fn db_wait(&self, run: &RunId) -> Option<komo_kernel::types::status::WaitReason> {
        self.run_detail(run).await.summary.wait
    }

    /// 等它停在 `waiting` 上并说得出在等什么，返回那个理由。
    ///
    /// 「`Waiting` 的 Run 一定说得出在等什么」是这套状态机的验收口径（§8.4）：一条
    /// `waiting` 却答不出理由的 Run 是损坏，所以这里直接 panic。
    pub async fn wait_waiting(&self, run: &RunId) -> komo_kernel::types::status::WaitReason {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut last = None;
        while std::time::Instant::now() < deadline {
            if self.db_state(run).await == RunState::Waiting {
                match self.db_wait(run).await {
                    Some(wait) => return wait,
                    None => panic!("run {run} 是 waiting 却说不出在等什么（§8.4）"),
                }
            }
            last = self.db_wait(run).await;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("等了 20 秒 run {run} 还没停在等待上，现在是 {last:?}");
    }

    pub async fn wait_state<F: Fn(RunState) -> bool>(
        &self,
        run: &RunId,
        done: F,
        what: &str,
    ) -> RunDetail {
        self.wait_db_state(run, done, what).await;
        self.run_detail(run).await
    }

    pub async fn wait_terminal(&self, run: &RunId) -> RunDetail {
        self.wait_state(run, |status| status.is_terminal(), "终态")
            .await
    }

    /// `POST /v1/interventions/{handle}/answer`（§7.5），断言 200。
    pub async fn answer(&self, handle: &str, verdict: &str) -> String {
        let body = serde_json::json!({ "verdict": verdict });
        let (code, body) = self
            .post(&format!("/v1/interventions/{handle}/answer"), body)
            .await;
        assert_eq!(code, 200, "{body}");
        body
    }

    /// 停机（放锁、删发现文件）。数据目录留着。
    pub async fn stop(self) {
        self.running.stop().await;
    }
}

// ---------------------------------------------------------------- 零碎

/// `RunId`（首个输入 submission 的十进制 ID）→ submission。
// W1a/W0a：`komo_kernel::durable::legacy_ids::submission_of` 合进来后换成它。
fn submission_of(run: &RunId) -> komo_kernel::durable::SubmissionId {
    let raw: u64 = run
        .as_str()
        .parse()
        .unwrap_or_else(|_| panic!("{run} 不是 Durable 的 RunId（十进制 submission ID）"));
    komo_kernel::durable::SubmissionId::new(raw)
}

/// 一个 Session 的全部合成事件（`Gw` 与 `TestGateway` 共用）。
pub async fn events_of(
    state: &GatewayState,
    session: &SessionId,
) -> Vec<komo_kernel::events::Event> {
    let core = state.core().expect("Durable 模式");
    crate::service::durable::feed::backfill(&*core.storage, session.as_str(), Seq(0))
        .await
        .expect("读得出事件")
}

/// 一个会话里受理过的输入 submission 数（排队的、并进别人 run 的都算一条）。
pub async fn inputs_of(state: &GatewayState, session: &str) -> usize {
    use komo_kernel::durable::SubmissionKind;
    use komo_kernel::durable::storage::{ConversationQuery, SubmissionQuery};
    let core = state.core().expect("Durable 模式");
    let conversations = core
        .storage
        .scan_conversations(
            &ConversationQuery {
                session: Some(session.to_owned()),
                ..ConversationQuery::default()
            },
            10_000,
            None,
        )
        .await
        .expect("读得出")
        .items;
    let mut count = 0;
    for conversation in conversations {
        let page = core
            .storage
            .scan_submissions(
                &SubmissionQuery {
                    conversation: Some(conversation.id),
                    ..SubmissionQuery::default()
                },
                10_000,
                None,
            )
            .await
            .expect("读得出");
        count += page
            .items
            .iter()
            .filter(|record| matches!(record.kind, SubmissionKind::Input { .. }))
            .count();
    }
    count
}

/// 轮询 submission 状态直到结算（最多 20 秒）。`TestGateway::wait_run` 两份共用。
pub async fn wait_run(state: &GatewayState, run: &RunId) -> komo_kernel::durable::SubmissionRecord {
    use komo_kernel::durable::SubmissionStatus;
    let core = state.core().expect("wait_run 只用在 Durable 模式下");
    let id = submission_of(run);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(record) = core.storage.submission(id).await.expect("读得出")
            && matches!(
                record.status(),
                SubmissionStatus::Done | SubmissionStatus::Unanswered
            )
        {
            return record;
        }
        if std::time::Instant::now() >= deadline {
            panic!("等了 20 秒 run {run} 的输入还没有结算");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn request(
    base: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, String) {
    let mut request = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("请求发得出去");
    let status = response.status().as_u16();
    (status, response.text().await.unwrap_or_default())
}

fn as_json(body: String) -> serde_json::Value {
    serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body))
}

/// 等某条 Run 的介入出现在清单里（最多 20 秒）。
async fn wait_intervention_in<F, Fut>(list: F, run: &RunId) -> InterventionSummary
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Vec<InterventionSummary>>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(found) = list()
            .await
            .into_iter()
            .find(|one| one.run.as_ref() == Some(run))
        {
            return found;
        }
        if std::time::Instant::now() >= deadline {
            panic!("等了 20 秒 run {run} 也没有出现在待处理清单里");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// reqwest 用 `rustls-no-provider`：provider 由 `komo` 的 `main` 装（§13.4）。测试进程里
/// 没有那个 `main`，所以这里装一次——**只装一次**。
pub fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// 一条来自某个平台的入站消息。
pub fn inbound(
    platform: ChannelPlatform,
    chat: &str,
    sender: &str,
    text: &str,
    request_key: &str,
    is_private: bool,
) -> InboundMessage {
    crate::dispatcher::inbound(platform, chat, sender, text, request_key, is_private)
}

/// 等一个条件成真（或超时）。
pub async fn eventually<F: Fn() -> bool>(what: &str, done: F) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("等了 5 秒还没有：{what}");
}
