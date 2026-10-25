//! 压缩钩子交回来的值：设置、摘要提示、摘要请求用的模型。
//!
//! runtime 不依赖 komo-agent，所以"摘要提示长什么样、用哪个模型、配置里的阈值是多少"都在钩子
//! ([`AgentHooks::compaction`](crate::durable::harness::AgentHooks::compaction)) 背后；这里只定义
//! 它交回来的值。默认实现从 [`AgentSetup`](crate::durable::harness::AgentSetup) 推：压缩关着、
//! 设置取 `[compaction]` 的默认值、摘要用这一轮的模型与 pi 的结构化摘要提示——所以没有接
//! 配置的测试替身与 D4b 的钩子都照旧能用，溢出恢复也照样救（它不看 `enabled`，同旧实现）。

use komo_kernel::compaction::CompactionEconomics;
use komo_kernel::protocol::config::CompactionConfig;
use komo_kernel::types::model::ModelConfig;

use crate::durable::harness::hooks::RetryPolicy;

/// 决策的设置：当前配置里的 `[compaction]`，加后台阈值。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionSettings {
    pub economics: CompactionEconomics,
    pub cache_write_read_ratio: Option<f64>,
    pub keep_recent_tokens: u64,
    pub memo_tokens: u64,
    /// `[compaction] max_context_tokens`：到了就按贴着窗口处理（`effective_window`）。
    pub max_context_tokens: u64,
    /// 后台阈值（pi-durable §8.7 的 `backgroundTokens`）：上下文到
    /// `阻塞阈值 - background_tokens` 就在后台起一个压缩，不挡当前轮。0 = 不启用。
    pub background_tokens: u64,
}

impl From<&CompactionConfig> for CompactionSettings {
    fn from(config: &CompactionConfig) -> Self {
        Self {
            economics: config.economics(),
            cache_write_read_ratio: Some(config.cache_write_read_ratio),
            keep_recent_tokens: config.keep_recent_tokens,
            memo_tokens: config.memo_token_estimate,
            max_context_tokens: config.max_context_tokens,
            background_tokens: 0,
        }
    }
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self::from(&CompactionConfig::default())
    }
}

/// 摘要请求的提示：系统提示，加跟在 `<conversation>` 后面的指令。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SummaryPrompt {
    pub system: String,
    pub instructions: String,
}

const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work. If the conversation starts with an earlier summary, preserve its information and fold the newer messages into it.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

impl Default for SummaryPrompt {
    /// pi 的结构化检查点格式。gateway 可以换成 komo 自己的（`komo-agent` 里的摘要提示）。
    fn default() -> Self {
        Self {
            system: SUMMARIZATION_SYSTEM_PROMPT.to_owned(),
            instructions: SUMMARIZATION_PROMPT.to_owned(),
        }
    }
}

/// 压缩钩子交回的全部。
#[derive(Clone)]
pub struct CompactionSetup {
    /// 在线压缩（阻塞与后台）开着没有。衰减不看它；溢出恢复也不看它。
    pub enabled: bool,
    pub settings: CompactionSettings,
    /// 摘要请求用的模型（含 effort）。
    pub model: ModelConfig,
    pub prompt: SummaryPrompt,
    /// 摘要请求失败的有界退避（同生成的）。
    pub retry: RetryPolicy,
    /// 提示缓存的路由键。
    pub cache_key: Option<String>,
}
