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
