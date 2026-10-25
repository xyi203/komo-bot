//! `ConfigSnapshot`：config.toml + .env 解析出来的**一份完整快照**（§3）。
//!
//! `policy.toml` 在 2026-10-11 删掉 Policy 之后不再读；文件还在时由 runtime 报一句
//! "已不再使用"，不静默忽略（D13）。
//!
//! 进程里只有一个 `Arc<ConfigSnapshot>`，热重载时原子替换。读配置的地方按用途读当前
//! 快照、不缓存——名单改完下一条消息就按新名单判。
//!
//! 两条性质写在类型里：
//!
//! - **凭证不进快照**。只有 `api_key_env`（变量名）和 [`ConfigSnapshot::credentials`]
//!   里的指纹（值的哈希）。指纹让"凭证变了 → 换掉对应的 LlmClient"判断得出来，而
//!   重载日志仍然只出键名（§3 第 3 步）。
//! - **只在启动时生效的键单独成一个结构**（[`StartOnly`]）。改了它们，重载照常完成
//!   其余部分，然后明确报告"以下键需要 `komo gateway restart`"——不静默忽略，也不假
//!   装已生效。

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::compaction::CompactionEconomics;
use crate::projection::{DecayPolicy, ProjectionContext};
use crate::types::agent::AgentConfig;
use crate::types::chat::{ChannelPlatform, PeerId};
use crate::types::digest::ContentHash;
use crate::types::memory::RetrievalMode;
use crate::types::model::{EmbeddingConfig, ModelCatalog, ModelConfig};

/// 一个配置键的路径，例如 `channels.feishu.allow_from`。**只有键名，没有值。**
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyPath(String);

impl KeyPath {
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn child(&self, segment: &str) -> Self {
        if self.0.is_empty() {
            Self(segment.to_string())
        } else {
            Self(format!("{}.{segment}", self.0))
        }
    }
}

impl fmt::Display for KeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 改了要重启 Gateway 才生效的那一小组键（§3 第 4 步）。
///
/// MCP 服务器不在这里：增删改热重载时就连上 / 断开（`docs/mcp.md` §2.2），见
/// [`ConfigSnapshot::mcp`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartOnly {
    /// 数据目录 / `KOMO_HOME`。
    pub data_dir: PathBuf,
    /// 监听地址与端口。
    pub listen: String,
    pub db_path: PathBuf,
    /// `[telemetry] otlp_endpoint`：OTLP/HTTP 端点。`None` = 不上报（docs/observability.md §4）。
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
}

/// 一个 MCP 服务器（`docs/mcp.md`）。凭证不在这里：`env` / `bearer_token_env` 写的是
/// `.env` 里的变量名。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub transport: McpTransport,
    /// 挂哪些工具。`None` = 服务器列出的全部。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// 操作者认定为只读的工具：中断后可以重做（`SafeReread`）；codemode 的 MCP 准入不取决于此声明。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_only: Vec<String>,
    /// 一句话说清这台服务器干什么，进系统提示的服务器段。没写就用服务器 instructions 的第一行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 模型怎么够到这台服务器的工具（`docs/mcp.md` §3）。
    #[serde(default, skip_serializing_if = "McpExposure::is_default")]
    pub exposure: McpExposure,
    /// 按工具覆盖 `exposure`：键是服务器上的工具原名，或带 `*` 的模式。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_exposure: BTreeMap<String, McpExposure>,
}

/// MCP 工具给模型的方式（照 pi 的 `exposure`，没有 `deferred`：komo 没有 `tool_search`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpExposure {
    /// 不声明给模型，只在 codemode 脚本里调。
    #[default]
    Codemode,
    /// 像内置工具一样声明给模型，脚本里也能调。
    Direct,
    /// 不挂：哪里都调不到。
    Hidden,
}

impl McpExposure {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl McpServerConfig {
    /// 一个工具实际的 exposure：`tool_exposure` 里的原名优先；其次是匹配的模式，字面部分最长的
    /// 那条赢（一样长按键的字典序）；都不中就是服务器的 `exposure`。
    ///
    /// pi 是"第一条匹配的模式赢"，靠 JSON 的键序；TOML 表读进来不保序，所以这里换成最具体的赢。
    pub fn exposure_of(&self, tool: &str) -> McpExposure {
        if let Some(exposure) = self.tool_exposure.get(tool) {
            return *exposure;
        }
        self.tool_exposure
            .iter()
            .filter(|(pattern, _)| pattern.contains('*') && glob_matches(pattern, tool))
            .max_by(|(a, _), (b, _)| {
                let literal = |p: &str| p.chars().filter(|c| *c != '*').count();
                literal(a).cmp(&literal(b)).then_with(|| b.cmp(a))
            })
            .map(|(_, exposure)| *exposure)
            .unwrap_or(self.exposure)
    }
}

/// `*` 匹配任意个字符，其余字面相等。
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let [first, middle @ .., last] = parts.as_slice() else {
        return pattern == text;
    };
    if !text.starts_with(first) {
        return false;
    }
    let mut rest = &text[first.len()..];
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpTransport {
    /// 起一个子进程，走 stdin / stdout。
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// 从 `.env` 带进子进程的变量名。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<String>,
    },
    /// Streamable HTTP。
    Http {
        url: String,
        /// `Authorization: Bearer` 用的 `.env` 变量名。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bearer_token_env: Option<String>,
        /// 走 MCP 的 OAuth（授权码 + PKCE）：token 由 `komo auth mcp login <name>` 写进
        /// 数据目录，不在 `.env`。与 `bearer_token_env` 互斥。
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        oauth: bool,
    },
}

/// 一个聊天渠道的行为键（§11.2）。凭证在 .env，不在这里。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 操作者的平台 id。**为空 = 只出不进**：还能作 home chat 收投递，但没人能通过它
    /// 下指令。
    #[serde(default)]
    pub allow_from: Vec<PeerId>,
    /// 与机器人同一租户（企业）的人都算操作者，等同写进 `allow_from`。只有飞书能核实，
    /// 且外部联系人、外部群（含组织外成员的群）里的消息不算。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_tenant: bool,
    /// 主动投递（Run 结束、需要人判断等）的目标会话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_chat: Option<PeerId>,
}

impl ChannelConfig {
    /// 这个发送者是不是操作者。
    pub fn is_operator(&self, sender: &PeerId) -> bool {
        self.allow_from.contains(sender)
    }

    /// 这条消息的发送者算不算操作者：在 `allow_from` 里，或开了 `allow_tenant` 且渠道
    /// 核实了同一组织（[`InboundMessage::same_org`](crate::protocol::InboundMessage)）。
    pub fn admits(&self, sender: &PeerId, same_org: bool) -> bool {
        self.is_operator(sender) || (self.allow_tenant && same_org)
    }

    /// enabled 但没人能说话——`komo doctor` 把它列为警告（§11.2）。
    pub fn is_outbound_only(&self) -> bool {
        self.enabled && self.allow_from.is_empty() && !self.allow_tenant
    }
}

/// 三个渠道。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelsConfig {
    #[serde(default)]
    pub feishu: ChannelConfig,
    #[serde(default)]
    pub telegram: ChannelConfig,
    #[serde(default)]
    pub wechat: ChannelConfig,
}

impl ChannelsConfig {
    pub fn get(&self, platform: ChannelPlatform) -> Option<&ChannelConfig> {
        match platform {
            ChannelPlatform::Feishu => Some(&self.feishu),
            ChannelPlatform::Telegram => Some(&self.telegram),
            ChannelPlatform::Wechat => Some(&self.wechat),
            ChannelPlatform::Api => None,
        }
    }

    /// home chat 的候选，按 §11.4 的默认顺序：**飞书 > Telegram > WeChat**。前两者能
    /// 对任意已加入的会话主动推送，微信不能。
    pub fn home_chats(&self) -> Vec<(ChannelPlatform, PeerId)> {
        [
            (ChannelPlatform::Feishu, &self.feishu),
            (ChannelPlatform::Telegram, &self.telegram),
            (ChannelPlatform::Wechat, &self.wechat),
        ]
        .into_iter()
        .filter(|(_, c)| c.enabled)
        .filter_map(|(platform, c)| c.home_chat.clone().map(|chat| (platform, chat)))
        .collect()
    }
}

/// `[memory.retrieval]`（§9.4）。这些是可调整的初始预算，不强行填满。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalConfig {
    #[serde(default)]
    pub mode: RetrievalMode,
    #[serde(default = "default_candidate_limit")]
    pub candidate_limit: u32,
    #[serde(default = "default_top_k")]
    pub top_k: u32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// 把融合后的短名单交给判断后端重排（§9.4）。默认关：开着就要发一次外部请求。
    #[serde(default)]
    pub rerank: bool,
    /// 重排看多宽的短名单。**必须大于 `top_k`**——一样宽就只是换个顺序，重排换不出
    /// 任何本来进不了 `top_k` 的条目（校验会拒绝这种组合）。
    #[serde(default = "default_rerank_shortlist")]
    pub rerank_shortlist: u32,
}

fn default_rerank_shortlist() -> u32 {
    20
}

fn default_candidate_limit() -> u32 {
    40
}
fn default_top_k() -> u32 {
    8
}
fn default_max_tokens() -> u32 {
    1500
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        RetrievalConfig {
            mode: RetrievalMode::default(),
            candidate_limit: default_candidate_limit(),
            top_k: default_top_k(),
            max_tokens: default_max_tokens(),
            rerank: false,
            rerank_shortlist: default_rerank_shortlist(),
        }
    }
}

/// `[typesafe]`：可选的判断后端（TypeSafe「System One」，§9.4 的记忆重排用它）。
///
/// **默认关**。它有两个代价，都要说清楚：要一份凭证；把送进 `state` 的正文（比如记忆
/// 正文）发给第三方。关着的时候一个请求都不发，召回顺序就是关键词 + 向量的融合结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypesafeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_typesafe_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_typesafe_model")]
    pub model: String,
    /// 凭证的**变量名**：值只住在 `.env` 里（`Secrets`），快照里只有变量名。
    #[serde(default = "default_typesafe_key")]
    pub api_key: String,
    #[serde(default = "default_typesafe_timeout")]
    pub timeout_secs: u64,
}

fn default_typesafe_endpoint() -> String {
    "https://api.typesafe.ai/v1/systemone".into()
}

fn default_typesafe_model() -> String {
    "jev-latest".into()
}

fn default_typesafe_key() -> String {
    "TYPESAFE_API_KEY".into()
}

fn default_typesafe_timeout() -> u64 {
    30
}

impl Default for TypesafeConfig {
    fn default() -> Self {
        TypesafeConfig {
            enabled: false,
            endpoint: default_typesafe_endpoint(),
            model: default_typesafe_model(),
            api_key: default_typesafe_key(),
            timeout_secs: default_typesafe_timeout(),
        }
    }
}

/// `[memory]`（§13.3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 整段省略时继承主模型的**完整**配置，包括 effort；解析时就填好，不在使用处
    /// 拼接（§13.3）。
    pub model: ModelConfig,
    /// embedding 必须独立指定；明确选择 keyword 模式时可以没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingConfig>,
    #[serde(default)]
    pub retrieval: RetrievalConfig,
    #[serde(default)]
    pub consolidate: ConsolidateConfig,
    /// 会话空闲多久之后才提取（§9.10）。文件里写 `extract_idle = "10m"`，快照里是秒。
    #[serde(default = "default_extract_idle_secs")]
    pub extract_idle_secs: u64,
    /// 一批（一个会话空闲后的全部 Run）最多收几条观察（§9.10）。
    #[serde(default = "default_max_observations")]
    pub max_observations: usize,
}

/// `[memory.consolidate]`：离线整理的分组预算（§9.8）。初值待实测调。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidateConfig {
    /// 一次运行最多处理几组（所有作用域合计）。
    #[serde(default = "default_consolidate_max_groups")]
    pub max_groups: u32,
    /// 一组最多几条；超了按相似度切开。
    #[serde(default = "default_consolidate_group_size")]
    pub group_size: u32,
    /// 近邻关系成立的最低相似度（正文词元的 Jaccard，百分数）。0 = 检索给的近邻都算。
    #[serde(default)]
    pub min_similarity_pct: u32,
}

fn default_consolidate_max_groups() -> u32 {
    50
}
fn default_consolidate_group_size() -> u32 {
    8
}

impl Default for ConsolidateConfig {
    fn default() -> Self {
        ConsolidateConfig {
            max_groups: default_consolidate_max_groups(),
            group_size: default_consolidate_group_size(),
            min_similarity_pct: 0,
        }
    }
}

impl MemoryConfig {
    /// 提取节奏的两个默认值：（`extract_idle_secs`，`max_observations`）。
    pub fn default_extraction() -> (u64, usize) {
        (default_extract_idle_secs(), default_max_observations())
    }
}

fn default_true() -> bool {
    true
}

fn default_extract_idle_secs() -> u64 {
    600
}

fn default_max_observations() -> usize {
    8
}

/// `[execution]`：Gateway 设置的执行预算（§6）。
///
/// **它是热生效的**：读者每用一次就读一次当前快照（§3），改完配置新起的 Run 就用新值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionConfig {
    /// 交给模型的工具结果正文字节上限。**完整输出永远在 `output.json` 里**，这里限的是
    /// 投影给模型看多少（§8.3）。
    #[serde(default = "default_model_result_bytes")]
    pub model_result_bytes: usize,
    /// 单次工具调用的活动执行时限（秒）。工具自己的超时（如 bash 的 `timeout_secs`）
    /// 会被收紧到它以内，所以到点时是工具自己杀进程、报超时，而不是执行器丢下一个结果不明。
    #[serde(default = "default_call_timeout_secs")]
    pub call_timeout_secs: u64,
    /// 对话会话里的 `bash` 跑过这么久还没完，就自动转到后台（`docs/monitor.md` §8）。
    /// 0 = 不转。任务会话与 Cron 的 Run 不受它管。
    #[serde(default = "default_detach_after_secs")]
    pub detach_after_secs: u64,
    /// 投影超过这么多字节的结果，完整给过 `decay_full_sends` 次之后换成首尾摘录（§8.3）。
    /// 0 = 不衰减。
    #[serde(default = "default_decay_threshold_bytes")]
    pub decay_threshold_bytes: usize,
    #[serde(default = "default_decay_full_sends")]
    pub decay_full_sends: u32,
    #[serde(default = "default_decay_head_bytes")]
    pub decay_head_bytes: usize,
    #[serde(default = "default_decay_tail_bytes")]
    pub decay_tail_bytes: usize,
}

impl ExecutionConfig {
    /// 这份配置给出的投影设置。受理 Run 时冻结进 `RunSnapshot`（§6.2）。
    pub fn projection(&self) -> ProjectionContext {
        ProjectionContext {
            model_result_bytes: self.model_result_bytes,
            decay: (self.decay_threshold_bytes > 0).then_some(DecayPolicy {
                threshold_bytes: self.decay_threshold_bytes,
                full_sends: self.decay_full_sends,
                head_bytes: self.decay_head_bytes,
                tail_bytes: self.decay_tail_bytes,
            }),
        }
    }
}

pub const DEFAULT_DECAY_THRESHOLD_BYTES: usize = 4096;
pub const DEFAULT_DECAY_FULL_SENDS: u32 = 2;
pub const DEFAULT_DECAY_HEAD_BYTES: usize = 2048;
pub const DEFAULT_DECAY_TAIL_BYTES: usize = 1536;

fn default_decay_threshold_bytes() -> usize {
    DEFAULT_DECAY_THRESHOLD_BYTES
}

fn default_decay_full_sends() -> u32 {
    DEFAULT_DECAY_FULL_SENDS
}

fn default_decay_head_bytes() -> usize {
    DEFAULT_DECAY_HEAD_BYTES
}

fn default_decay_tail_bytes() -> usize {
    DEFAULT_DECAY_TAIL_BYTES
}

/// 一次 `cargo build --release` 这种量级的命令要能跑完。
pub const DEFAULT_CALL_TIMEOUT_SECS: u64 = 1800;

fn default_call_timeout_secs() -> u64 {
    DEFAULT_CALL_TIMEOUT_SECS
}

pub const DEFAULT_DETACH_AFTER_SECS: u64 = 120;

fn default_detach_after_secs() -> u64 {
    DEFAULT_DETACH_AFTER_SECS
}

fn default_model_result_bytes() -> usize {
    komo_kernel_model_result_bytes()
}

/// 默认值只有一处：真正渲染投影的那个常量。
fn komo_kernel_model_result_bytes() -> usize {
    crate::projection::DEFAULT_MODEL_RESULT_BYTES
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            model_result_bytes: default_model_result_bytes(),
            call_timeout_secs: DEFAULT_CALL_TIMEOUT_SECS,
            detach_after_secs: DEFAULT_DETACH_AFTER_SECS,
            decay_threshold_bytes: DEFAULT_DECAY_THRESHOLD_BYTES,
            decay_full_sends: DEFAULT_DECAY_FULL_SENDS,
            decay_head_bytes: DEFAULT_DECAY_HEAD_BYTES,
            decay_tail_bytes: DEFAULT_DECAY_TAIL_BYTES,
        }
    }
}

/// `[compaction]`：在线上下文压缩（§6）。**默认关**——要先在真实会话上核对触发频率与
/// 省下的量（§14），再打开。
///
/// 每到一次决策就读当前快照，不随 Run 冻结：决定本身连同全部中间量记进
/// `context.compacted`，回放只认那条事件，不重算。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 缓存写与缓存读的价格比（Anthropic：1.25 / 0.1 = 12.5）。
    #[serde(default = "default_cache_write_read_ratio")]
    pub cache_write_read_ratio: f64,
    /// 切点之后至少原样保留这么多 token 的近期轮次。
    #[serde(default = "default_keep_recent_tokens")]
    pub keep_recent_tokens: u64,
    /// 离窗口还剩这么多 token 时不看账，直接压。
    #[serde(default = "default_window_reserve_tokens")]
    pub window_reserve_tokens: u64,
    /// 上下文到这么多 token 就按"贴着窗口"处理（与模型的 `context_window` 取先到的
    /// 那个）。1M 窗口的模型要到 98 万才贴窗口，压缩又只在贴着窗口时发生。
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: u64,
    /// 第一次压缩时把剩余请求估计放大的倍数。
    #[serde(default = "default_first_compaction_scale")]
    pub first_compaction_scale: f64,
    /// 之后的压缩要求 `回本请求数 × margin ≤ 剩余请求`。
    #[serde(default = "default_subsequent_margin")]
    pub subsequent_margin: f64,
    /// 上次压缩之后至少过了这么多次请求，才按经济账再压。
    #[serde(default = "default_cooldown_requests")]
    pub cooldown_requests: u64,
    /// 摘要本身的 token 估计。
    #[serde(default = "default_memo_token_estimate")]
    pub memo_token_estimate: u64,
    /// 后台压缩阈值（新存储 harness，pi-durable §8.7 的 `backgroundTokens`）：上下文到
    /// `阻塞阈值 - background_tokens` 就在后台起一个压缩，不挡当前轮。0 = 不启用。
    #[serde(default)]
    pub background_tokens: u64,
}

impl CompactionConfig {
    /// 决策的旋钮；没有配置项的两个（剩余请求的缩放与标准差系数）取默认。
    pub fn economics(&self) -> CompactionEconomics {
        CompactionEconomics {
            window_reserve_tokens: self.window_reserve_tokens,
            first_compaction_request_scale: self.first_compaction_scale,
            subsequent_compaction_margin: self.subsequent_margin,
            minimum_requests_since_compaction: self.cooldown_requests,
            ..CompactionEconomics::default()
        }
    }

    /// 判"贴着窗口"用的窗口（[`crate::compaction::effective_window`]）。
    pub fn window(&self, model_window: Option<u64>) -> u64 {
        crate::compaction::effective_window(
            model_window,
            self.max_context_tokens,
            self.window_reserve_tokens,
        )
    }
}

fn default_cache_write_read_ratio() -> f64 {
    12.5
}

fn default_keep_recent_tokens() -> u64 {
    20_000
}

fn default_window_reserve_tokens() -> u64 {
    CompactionEconomics::default().window_reserve_tokens
}

/// 与 Anthropic API 服务端压缩的默认触发点相同。
fn default_max_context_tokens() -> u64 {
    150_000
}

fn default_first_compaction_scale() -> f64 {
    CompactionEconomics::default().first_compaction_request_scale
}

fn default_subsequent_margin() -> f64 {
    CompactionEconomics::default().subsequent_compaction_margin
}

fn default_cooldown_requests() -> u64 {
    CompactionEconomics::default().minimum_requests_since_compaction
}

fn default_memo_token_estimate() -> u64 {
    1_000
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cache_write_read_ratio: default_cache_write_read_ratio(),
            keep_recent_tokens: default_keep_recent_tokens(),
            window_reserve_tokens: default_window_reserve_tokens(),
            max_context_tokens: default_max_context_tokens(),
            first_compaction_scale: default_first_compaction_scale(),
            subsequent_margin: default_subsequent_margin(),
            cooldown_requests: default_cooldown_requests(),
            memo_token_estimate: default_memo_token_estimate(),
            background_tokens: 0,
        }
    }
}

/// 数据目录下的各个位置（§12）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathsConfig {
    pub sessions_dir: PathBuf,
    /// Skills 的有序搜索路径，**同名先到先得**（§5.6）。
    #[serde(default)]
    pub skill_dirs: Vec<PathBuf>,
    pub runtime_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub workspaces_dir: PathBuf,
}

/// 一个来源文件及其 mtime——`komo doctor` 拿它和"当前生效配置的加载时间"比对，
/// 不一致就是"文件改了但没装上"（§3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: PathBuf,
    #[serde(with = "time::serde::rfc3339")]
    pub mtime: OffsetDateTime,
}

/// 一份完整的配置快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub start_only: StartOnly,
    /// `model.<alias>` 解析后的完整模型目录。运行时按 alias 选择，不在调用处拼接端点。
    #[serde(default)]
    pub model_catalog: ModelCatalog,
    pub model: ModelConfig,
    pub memory: MemoryConfig,
    /// 助手定义（§四）。**没有隐含默认**：`[agents.<id>]` 是唯一写法，
    /// `default_agent` 指向其中一个。
    pub agent: AgentConfig,
    /// 可选判断后端。默认关（见 [`TypesafeConfig`]）。
    #[serde(default)]
    pub typesafe: TypesafeConfig,
    #[serde(default)]
    pub execution: ExecutionConfig,
    #[serde(default)]
    pub compaction: CompactionConfig,
    pub channels: ChannelsConfig,
    pub paths: PathsConfig,
    /// MCP 服务器（`[mcp.servers.<name>]`，`docs/mcp.md`）。重载时按名字比对：新增的后台连上，
    /// 删掉的撤工具、断开，改了的先删后加（照 pi 的 `mcp_servers_change`）。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpServerConfig>,
    /// 每个凭证**值**的哈希，按变量名索引。值本身不在快照里，这里只够回答"它变了
    /// 没有"。
    #[serde(default)]
    pub credentials: BTreeMap<String, ContentHash>,
    #[serde(with = "time::serde::rfc3339")]
    pub loaded_at: OffsetDateTime,
    #[serde(default)]
    pub sources: Vec<SourceFile>,
}

impl ConfigSnapshot {
    /// 两份快照的差异，**只出键名不出值**（§3 第 3 步）。
    pub fn diff(&self, other: &ConfigSnapshot) -> Vec<KeyPath> {
        let left = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        let right = serde_json::to_value(other).unwrap_or(serde_json::Value::Null);
        let mut out = Vec::new();
        diff_value(&KeyPath::new(""), &left, &right, &mut out);
        // `loaded_at` 和 `sources` 每次重载都不同，它们不是配置内容。
        out.retain(|key| {
            !key.as_str().starts_with("loaded_at") && !key.as_str().starts_with("sources")
        });
        out
    }

    /// 差异里有哪些是**只在启动时生效**的键。重载要把它们原样报给操作者。
    pub fn start_only_changes(&self, other: &ConfigSnapshot) -> Vec<KeyPath> {
        self.diff(other)
            .into_iter()
            .filter(|key| key.as_str().starts_with("start_only."))
            .collect()
    }
}

fn diff_value(
    path: &KeyPath,
    left: &serde_json::Value,
    right: &serde_json::Value,
    out: &mut Vec<KeyPath>,
) {
    use serde_json::Value;
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let child = path.child(key);
                match (a.get(key), b.get(key)) {
                    (Some(l), Some(r)) => diff_value(&child, l, r, out),
                    _ => out.push(child),
                }
            }
        }
        // 数组整体比较：出键名就够了，出到下标反而把值的形状泄出来。
        _ if left != right => out.push(path.clone()),
        _ => {}
    }
}

/// 配置校验的一条问题。`komo config check` 与重载共用同一套校验（§11.2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigIssue {
    pub key: KeyPath,
    pub severity: IssueSeverity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    /// 校验不过——**旧快照原样保留**，哪怕只错一个键（§3 第 1 步）。
    Error,
    /// 例如"某渠道 enabled 但 allow_from 为空"。
    Warning,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::model::Effort;
    use time::macros::datetime;

    use crate::types::AgentProfile;

    fn model(name: &str) -> ModelConfig {
        ModelConfig {
            provider: "openai_compatible".into(),
            base_url: "https://llm.example.com/v1".into(),
            model: name.into(),
            api_key_env: "KOMO_LLM_API_KEY".into(),
            auth: None,
            effort: Some(Effort::new("medium")),
            efforts: None,
            timeout_secs: 120,
            context_window: None,
        }
    }

    fn snapshot() -> ConfigSnapshot {
        ConfigSnapshot {
            execution: ExecutionConfig::default(),
            compaction: CompactionConfig::default(),
            start_only: StartOnly {
                data_dir: PathBuf::from("/home/u/.komo"),
                listen: "127.0.0.1:7777".into(),
                db_path: PathBuf::from("/home/u/.komo/state.db"),
                otlp_endpoint: None,
            },
            model_catalog: ModelCatalog::default(),
            model: model("chat-a"),
            memory: MemoryConfig {
                enabled: true,
                model: model("memory-a"),
                embedding: None,
                retrieval: RetrievalConfig::default(),
                consolidate: ConsolidateConfig::default(),
                extract_idle_secs: 600,
                max_observations: 8,
            },
            agent: AgentConfig {
                default_agent: "assistant".into(),
                agents: BTreeMap::from([("assistant".into(), AgentProfile::new("assistant"))]),
            },
            typesafe: TypesafeConfig::default(),
            channels: ChannelsConfig {
                feishu: ChannelConfig {
                    enabled: true,
                    allow_from: vec![PeerId::new("ou_xxx")],
                    allow_tenant: false,
                    home_chat: Some(PeerId::new("oc_xxx")),
                },
                telegram: ChannelConfig::default(),
                wechat: ChannelConfig::default(),
            },
            paths: PathsConfig {
                sessions_dir: PathBuf::from("/home/u/.komo/sessions"),
                skill_dirs: vec![PathBuf::from("/home/u/.komo/skills")],
                runtime_dir: PathBuf::from("/home/u/.komo/runtime"),
                logs_dir: PathBuf::from("/home/u/.komo/logs"),
                workspaces_dir: PathBuf::from("/home/u/.komo/workspaces"),
            },
            mcp: BTreeMap::new(),
            credentials: BTreeMap::from([(
                "KOMO_LLM_API_KEY".into(),
                ContentHash::of_str("secret-1"),
            )]),
            loaded_at: datetime!(2026-09-15 08:00:00 UTC),
            sources: vec![],
        }
    }

    #[test]
    fn an_identical_snapshot_has_no_diff() {
        let mut later = snapshot();
        later.loaded_at = datetime!(2026-09-15 09:00:00 UTC);
        assert_eq!(snapshot().diff(&later), vec![]);
    }

    #[test]
    fn a_diff_names_keys_and_never_values() {
        let before = snapshot();
        let mut after = snapshot();
        after.model.model = "chat-b".into();
        after.channels.feishu.allow_from.push(PeerId::new("ou_yyy"));

        let keys = before.diff(&after);
        assert_eq!(
            keys,
            vec![
                KeyPath::new("channels.feishu.allow_from"),
                KeyPath::new("model.model"),
            ]
        );
        for key in &keys {
            assert!(!key.as_str().contains("chat-b"), "{key}");
            assert!(!key.as_str().contains("ou_yyy"), "{key}");
        }
    }

    #[test]
    fn a_changed_credential_shows_up_as_a_key_not_a_secret() {
        let before = snapshot();
        let mut after = snapshot();
        after
            .credentials
            .insert("KOMO_LLM_API_KEY".into(), ContentHash::of_str("secret-2"));
        assert_eq!(
            before.diff(&after),
            vec![KeyPath::new("credentials.KOMO_LLM_API_KEY")]
        );
    }

    #[test]
    fn start_only_keys_are_reported_separately() {
        let before = snapshot();
        let mut after = snapshot();
        after.start_only.listen = "0.0.0.0:7777".into();
        after.model.model = "chat-b".into();

        assert_eq!(
            before.start_only_changes(&after),
            vec![KeyPath::new("start_only.listen")]
        );
        assert_eq!(before.diff(&after).len(), 2, "其余部分照常报告");
    }

    /// MCP 服务器热重载就生效（`docs/mcp.md` §2.2）：差异键是 `mcp`（从无到有、从有到无）或
    /// `mcp.<name>`，不算"要重启"。
    #[test]
    fn mcp_changes_are_not_start_only() {
        let server = |command: &str| McpServerConfig {
            transport: McpTransport::Stdio {
                command: command.into(),
                args: vec![],
                env: vec![],
            },
            tools: None,
            read_only: vec![],
            description: None,
            exposure: McpExposure::default(),
            tool_exposure: BTreeMap::new(),
        };
        let empty = snapshot();
        let mut one = snapshot();
        one.mcp.insert("gh".into(), server("x"));
        assert_eq!(empty.diff(&one), vec![KeyPath::new("mcp")]);
        assert_eq!(empty.start_only_changes(&one), vec![]);

        let mut two = one.clone();
        two.mcp.insert("jira".into(), server("y"));
        assert_eq!(one.diff(&two), vec![KeyPath::new("mcp.jira")]);
        assert_eq!(one.start_only_changes(&two), vec![]);
    }

    #[test]
    fn home_chat_candidates_follow_feishu_then_telegram_then_wechat() {
        let mut snapshot = snapshot();
        snapshot.channels.telegram = ChannelConfig {
            enabled: true,
            allow_from: vec![PeerId::new("123")],
            allow_tenant: false,
            home_chat: Some(PeerId::new("123")),
        };
        snapshot.channels.wechat = ChannelConfig {
            enabled: true,
            allow_from: vec![PeerId::new("wxid_x")],
            allow_tenant: false,
            home_chat: Some(PeerId::new("wxid_x")),
        };
        let order: Vec<ChannelPlatform> = snapshot
            .channels
            .home_chats()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(
            order,
            vec![
                ChannelPlatform::Feishu,
                ChannelPlatform::Telegram,
                ChannelPlatform::Wechat
            ]
        );
    }

    #[test]
    fn an_enabled_channel_with_an_empty_allow_list_is_outbound_only() {
        let channel = ChannelConfig {
            enabled: true,
            allow_from: vec![],
            allow_tenant: false,
            home_chat: Some(PeerId::new("oc_x")),
        };
        assert!(channel.is_outbound_only());
        assert!(!channel.is_operator(&PeerId::new("ou_x")));
    }

    #[test]
    fn allow_tenant_admits_a_verified_same_org_sender_only() {
        let channel = ChannelConfig {
            enabled: true,
            allow_tenant: true,
            ..ChannelConfig::default()
        };
        let sender = PeerId::new("ou_colleague");
        assert!(channel.admits(&sender, true));
        assert!(!channel.admits(&sender, false), "渠道没核实就不算");
        assert!(!channel.is_outbound_only(), "靠租户进来的人也能下指令");

        let closed = ChannelConfig {
            enabled: true,
            ..ChannelConfig::default()
        };
        assert!(
            !closed.admits(&sender, true),
            "没开 allow_tenant 就不看 same_org"
        );
    }

    /// `tool_exposure`：原名最优先；模式里字面部分最长的赢；都不中就是服务器的 `exposure`。
    #[test]
    fn tool_exposure_prefers_exact_names_then_the_most_specific_pattern() {
        let mut server = McpServerConfig {
            transport: McpTransport::Stdio {
                command: "x".into(),
                args: vec![],
                env: vec![],
            },
            tools: None,
            read_only: vec![],
            description: None,
            exposure: McpExposure::Direct,
            tool_exposure: BTreeMap::new(),
        };
        server
            .tool_exposure
            .insert("get_*".into(), McpExposure::Codemode);
        server
            .tool_exposure
            .insert("get_secret*".into(), McpExposure::Hidden);
        server
            .tool_exposure
            .insert("get_secret_ok".into(), McpExposure::Direct);
        assert_eq!(server.exposure_of("get_issue"), McpExposure::Codemode);
        assert_eq!(server.exposure_of("get_secret_key"), McpExposure::Hidden);
        assert_eq!(server.exposure_of("get_secret_ok"), McpExposure::Direct);
        assert_eq!(server.exposure_of("list"), McpExposure::Direct);
    }

    #[test]
    fn glob_star_matches_any_run_of_characters() {
        assert!(glob_matches("*", ""));
        assert!(glob_matches("a*c", "abbc"));
        assert!(glob_matches("*_get_*", "issue_get_info"));
        assert!(!glob_matches("a*c", "abcd"));
        assert!(!glob_matches("ab*ba", "aba"));
        assert!(glob_matches("exact", "exact"));
    }
}
