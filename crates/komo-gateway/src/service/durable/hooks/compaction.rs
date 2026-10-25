//! 压缩钩子（`AgentHooks::compaction`）：`[compaction]` 的旋钮、摘要提示、摘要用的模型。
//!
//! - 开关与阈值每次读**当前**快照（热重载）；`[compaction] background_tokens` 是新存储 harness
//!   才有的后台阈值（0 = 不启用）。
//! - 摘要提示用 komo-agent 的 [`SUMMARY_INSTRUCTIONS`]（旧在线压缩的同一份）。
//! - 摘要用这一轮的模型（同旧实现：压缩请求与它压的那条会话同一个模型与缓存键）。

use komo_agent::context::compaction::SUMMARY_INSTRUCTIONS;
use komo_kernel::protocol::config::ConfigSnapshot;
use komo_runtime::durable::harness::{CompactionSettings, CompactionSetup, SummaryPrompt};

use super::setup::Resolved;

/// 跟在 `<conversation>…</conversation>` 后面的指令（系统提示是 [`SUMMARY_INSTRUCTIONS`]）。
const SUMMARY_REQUEST: &str = "上面是要压缩的工作记录，按系统提示的要求写摘要。只输出摘要正文。";

pub(super) fn build(config: &ConfigSnapshot, resolved: &Resolved) -> CompactionSetup {
    let mut settings = CompactionSettings::from(&config.compaction);
    settings.background_tokens = config.compaction.background_tokens;
    CompactionSetup {
        enabled: config.compaction.enabled,
        settings,
        model: resolved.model.clone(),
        prompt: SummaryPrompt {
            system: SUMMARY_INSTRUCTIONS.to_owned(),
            instructions: SUMMARY_REQUEST.to_owned(),
        },
        retry: resolved.budget.retry,
        cache_key: Some(resolved.cache_key.clone()),
    }
}
