//! `agent` 的测试共用的几个小装配（只在 `cfg(test)` 下编译）。

use komo_kernel::test_support::sample_model;
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::turn::{ProviderToolCall, Round, TurnRequest};

pub fn round(number: u32, text: Option<&str>, calls: Vec<ProviderToolCall>) -> Round {
    Round {
        round: number,
        text: text.map(str::to_string),
        tool_calls: calls,
        provider_blocks: None,
        usage: Default::default(),
        truncated: false,
    }
}

pub fn call(id: &str, name: &str, args: serde_json::Value) -> ProviderToolCall {
    ProviderToolCall {
        provider_call_id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

pub fn turn_request(session: &SessionId, run: &RunId) -> TurnRequest {
    TurnRequest {
        session: session.clone(),
        run: run.clone(),
        model: sample_model(),
        system_prompt: "你是 komo".into(),
        messages: vec![],
        tools: vec![],
        memories: vec![],
        covers: None,
    }
}

/// 一开口就报同一个错的 [`LlmClient`]——`ScriptedLlm` 只演得了成功的回合。
pub struct FailingLlm {
    error: komo_kernel::types::turn::LlmError,
    /// 错在 `begin_turn` 上，还是错在第一次 `next` 上。
    at_begin: bool,
}

impl FailingLlm {
    pub fn at_begin(error: komo_kernel::types::turn::LlmError) -> Self {
        Self {
            error,
            at_begin: true,
        }
    }

    pub fn at_round(error: komo_kernel::types::turn::LlmError) -> Self {
        Self {
            error,
            at_begin: false,
        }
    }
}

#[async_trait::async_trait]
impl komo_kernel::traits::LlmClient for FailingLlm {
    async fn begin_turn(
        &self,
        _req: TurnRequest,
    ) -> Result<Box<dyn komo_kernel::traits::TurnDriver>, komo_kernel::types::turn::LlmError> {
        if self.at_begin {
            return Err(self.error.clone());
        }
        Ok(Box::new(FailingDriver {
            error: self.error.clone(),
        }))
    }
}

/// 一碰 `begin_turn` 就 panic 的 [`LlmClient`]。命令 Run 全程零模型请求（§10）是这个
/// 断言的唯一可靠证明方式：不是"没观察到调用"，而是"调了就当场爆"。
pub struct PanickingLlm;

#[async_trait::async_trait]
impl komo_kernel::traits::LlmClient for PanickingLlm {
    async fn begin_turn(
        &self,
        _req: TurnRequest,
    ) -> Result<Box<dyn komo_kernel::traits::TurnDriver>, komo_kernel::types::turn::LlmError> {
        panic!("命令 Run 不该发起任何模型请求（§10）")
    }
}

struct FailingDriver {
    error: komo_kernel::types::turn::LlmError,
}

#[async_trait::async_trait]
impl komo_kernel::traits::TurnDriver for FailingDriver {
    async fn next(
        &mut self,
        _input: komo_kernel::types::turn::RoundInput,
    ) -> Result<Round, komo_kernel::types::turn::LlmError> {
        Err(self.error.clone())
    }

    fn usage(&self) -> komo_kernel::types::model::TokenUsage {
        Default::default()
    }
}

/// 一次压缩决定：摘要请求没有工具，切在 seq 2。决策取 kernel 替身里那组压得过来的数。
pub fn compaction_job(session: &SessionId, run: &RunId) -> komo_kernel::compaction::CompactionJob {
    let komo_kernel::events::ContextCompacted::Compacted { decision, .. } =
        komo_kernel::test_support::compacted(komo_kernel::types::ids::Seq(2), "")
    else {
        unreachable!("替身给的是 compacted")
    };
    komo_kernel::compaction::CompactionJob {
        request: TurnRequest {
            system_prompt: "压缩上下文".into(),
            ..turn_request(session, run)
        },
        first_kept: komo_kernel::types::ids::Seq(2),
        decision,
    }
}

/// 一次换短视图的决定：把 `provider_call_id` 那条结果换成短视图。
pub fn decay_job(provider_call_id: &str) -> komo_kernel::compaction::DecayJob {
    let komo_kernel::events::ContextCompacted::Compacted { decision, .. } =
        komo_kernel::test_support::compacted(komo_kernel::types::ids::Seq(2), "")
    else {
        unreachable!("替身给的是 compacted")
    };
    let call = komo_kernel::types::ids::ToolCallId::from_raw("decayed-call");
    komo_kernel::compaction::DecayJob {
        calls: vec![call.clone()],
        revised: vec![komo_kernel::types::turn::ToolResultForModel {
            provider_call_id: provider_call_id.into(),
            call_id: call,
            content: "[read · 完成 · 短视图]".into(),
            is_error: false,
        }],
        decision,
    }
}

/// 按脚本作答的 [`super::CompactionPlanner`]：第 n 次问给第 n 个答案，用完之后一律不压。
pub struct FakePlanner {
    answers: std::sync::Mutex<std::collections::VecDeque<Option<komo_kernel::compaction::Reshape>>>,
    pub asked: std::sync::atomic::AtomicUsize,
}

impl FakePlanner {
    pub fn new(answers: Vec<Option<komo_kernel::compaction::Reshape>>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            answers: std::sync::Mutex::new(answers.into_iter().collect()),
            asked: Default::default(),
        })
    }

    pub fn asked(&self) -> usize {
        self.asked.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl super::CompactionPlanner for FakePlanner {
    async fn plan(&self) -> Option<komo_kernel::compaction::Reshape> {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.answers.lock().expect("脚本").pop_front().flatten()
    }
}

/// 一个 [`super::Compactor`]：不知道窗口，只在计划边界上问。
pub fn compactor(planner: std::sync::Arc<FakePlanner>) -> super::Compactor {
    super::Compactor {
        planner,
        pressure_tokens: None,
        estimate: 0,
        pending: false,
    }
}

/// 开口就答应、回一句却永远不来的模型：摘要请求挂在半路上，只有取消能让它停。
pub struct HangingLlm;

#[async_trait::async_trait]
impl komo_kernel::traits::LlmClient for HangingLlm {
    async fn begin_turn(
        &self,
        _req: TurnRequest,
    ) -> Result<Box<dyn komo_kernel::traits::TurnDriver>, komo_kernel::types::turn::LlmError> {
        Ok(Box::new(HangingDriver))
    }
}

struct HangingDriver;

#[async_trait::async_trait]
impl komo_kernel::traits::TurnDriver for HangingDriver {
    async fn next(
        &mut self,
        _input: komo_kernel::types::turn::RoundInput,
    ) -> Result<Round, komo_kernel::types::turn::LlmError> {
        std::future::pending().await
    }

    fn usage(&self) -> komo_kernel::types::model::TokenUsage {
        Default::default()
    }
}
