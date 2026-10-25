use komo_kernel::test_support::{ScriptedLlm, sample_model};
use komo_kernel::traits::LlmClient;
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::turn::{LlmError, RoundInput};
use tracing::Instrument;

use super::capture::Capture;
use super::*;

fn round(
    number: u32,
    text: Option<&str>,
    calls: Vec<komo_kernel::types::turn::ProviderToolCall>,
) -> komo_kernel::types::turn::Round {
    komo_kernel::types::turn::Round {
        round: number,
        text: text.map(str::to_string),
        tool_calls: calls,
        provider_blocks: None,
        usage: Default::default(),
        truncated: false,
    }
}

fn turn_request(session: &SessionId, run: &RunId) -> komo_kernel::types::turn::TurnRequest {
    komo_kernel::types::turn::TurnRequest {
        session: session.clone(),
        run: run.clone(),
        model: sample_model(),
        system_prompt: "你是 komo".into(),
        messages: vec![],
        tools: vec![],
        memories: vec![],
        covers: None,
        cache_session: None,
    }
}

/// 每一轮都报同一个错的 [`LlmClient`]——`ScriptedLlm` 只演得了成功的回合。
struct FailingLlm {
    error: LlmError,
}

impl FailingLlm {
    fn at_round(error: LlmError) -> Self {
        Self { error }
    }
}

#[async_trait::async_trait]
impl LlmClient for FailingLlm {
    async fn begin_turn(
        &self,
        _req: komo_kernel::types::turn::TurnRequest,
    ) -> Result<Box<dyn komo_kernel::traits::TurnDriver>, LlmError> {
        Ok(Box::new(FailingDriver {
            error: self.error.clone(),
        }))
    }
}

struct FailingDriver {
    error: LlmError,
}

#[async_trait::async_trait]
impl komo_kernel::traits::TurnDriver for FailingDriver {
    async fn next(
        &mut self,
        _input: RoundInput,
    ) -> Result<komo_kernel::types::turn::Round, LlmError> {
        Err(self.error.clone())
    }

    fn usage(&self) -> TokenUsage {
        Default::default()
    }
}

fn request() -> komo_kernel::types::turn::TurnRequest {
    turn_request(&SessionId::from_raw("s"), &RunId::from_raw("r"))
}

/// 同一个 round 下，失败的那次与重试的那次是两个兄弟 `chat` span：第一个带 `error.type`，
/// 第二个没有；尝试序号递增。
#[tokio::test]
async fn a_failed_then_retried_request_yields_two_sibling_chat_spans() {
    let capture = Capture::default();
    let _guard = capture.install();
    let model = sample_model();

    let parent = tracing::info_span!("komo.round", komo.round = 1u32);
    async {
        let failing = FailingLlm::at_round(LlmError::Timeout);
        let mut driver = failing.begin_turn(request()).await.unwrap();
        turn(&mut *driver, RoundInput::First, role::MAIN, &model, 1)
            .await
            .unwrap_err();

        let mut answered = round(1, Some("好"), vec![]);
        answered.usage = TokenUsage {
            input: Some(12),
            ..TokenUsage::default()
        };
        let ok = ScriptedLlm::once(vec![answered]);
        let mut driver = ok.begin_turn(request()).await.unwrap();
        turn(&mut *driver, RoundInput::First, role::MAIN, &model, 2)
            .await
            .unwrap();
    }
    .instrument(parent)
    .await;

    let round_span = &capture.spans("komo.round")[0];
    let chats = capture.children(round_span, "chat");
    assert_eq!(chats.len(), 2);
    assert_eq!(chats[0].fields["komo.llm.attempt"], "1");
    assert_eq!(chats[0].fields["error.type"], "timeout");
    assert_eq!(chats[1].fields["komo.llm.attempt"], "2");
    assert!(!chats[1].fields.contains_key("error.type"));
}

/// 服务商只报了输入：span 与指标里只有输入，其余各类 token 不出现——不是 0。
#[tokio::test]
async fn token_kinds_the_provider_did_not_report_are_absent_not_zero() {
    let capture = Capture::default();
    let _guard = capture.install();

    let mut answered = round(1, Some("好"), vec![]);
    answered.usage = TokenUsage {
        input: Some(12),
        ..TokenUsage::default()
    };
    let llm = ScriptedLlm::once(vec![answered]);
    let mut driver = llm.begin_turn(request()).await.unwrap();
    turn(
        &mut *driver,
        RoundInput::First,
        role::MEMORY_EXTRACT,
        &sample_model(),
        1,
    )
    .await
    .unwrap();

    let chat = &capture.spans("chat")[0];
    assert_eq!(chat.fields["komo.llm.role"], "memory.extract");
    assert_eq!(chat.fields["gen_ai.usage.input_tokens"], "12");
    for absent in [
        "gen_ai.usage.output_tokens",
        "komo.usage.cache_read_tokens",
        "komo.usage.cache_write_tokens",
        "komo.usage.reasoning_tokens",
    ] {
        assert!(!chat.fields.contains_key(absent), "{absent} 不该出现");
    }
    let tokens = capture.metrics("histogram.gen_ai.client.token.usage");
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].fields["gen_ai.token.type"], "input");
    assert_eq!(tokens[0].fields["komo.llm.role"], "memory.extract");
}

/// 重排：用量取响应里真报了的，没报的不记。
#[tokio::test]
async fn a_rerank_request_records_only_the_usage_the_backend_reported() {
    struct Backend;

    #[async_trait::async_trait]
    impl SystemOne for Backend {
        async fn ask(
            &self,
            request: SystemOneRequest,
        ) -> Result<SystemOneResponse, SystemOneError> {
            Ok(SystemOneResponse {
                model: format!("{}-1.0", request.model),
                answers: Default::default(),
                usage: komo_kernel::types::systemone::SystemOneUsage {
                    input_tokens: Some(296),
                    output_tokens: None,
                },
            })
        }
    }

    let capture = Capture::default();
    let _guard = capture.install();
    ask(
        &Backend,
        SystemOneRequest {
            state: serde_json::json!("不进 span"),
            model: "jev".into(),
            questions: Default::default(),
        },
        role::RERANK,
    )
    .await
    .unwrap();

    let chat = &capture.spans("chat")[0];
    assert_eq!(chat.fields["komo.llm.role"], "rerank");
    assert_eq!(chat.fields["gen_ai.response.model"], "jev-1.0");
    assert_eq!(chat.fields["gen_ai.usage.input_tokens"], "296");
    assert!(!chat.fields.contains_key("gen_ai.usage.output_tokens"));
    assert!(
        !chat.fields.values().any(|value| value.contains("不进")),
        "请求内容不进 span"
    );
}
