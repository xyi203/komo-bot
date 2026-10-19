//! 埋点（`docs/observability.md` §2、§3）。
//!
//! runtime 只写 `tracing`：span 用 OpenTelemetry GenAI 语义约定的名字（`invoke_agent`、
//! `chat`、`execute_tool`）与属性键（`gen_ai.*`），约定没有的用 `komo.*`；指标是
//! `target = "komo::metrics"` 的 DEBUG 事件，字段带 `histogram.` / `monotonic_counter.`
//! 前缀（tracing-opentelemetry 的 `MetricsLayer` 约定），其余字段是低基数维度。
//! 这里不依赖任何 OpenTelemetry 包——span 与事件怎么出站是 bin 里 `init_tracing` 的事。
//!
//! 两条铁律：**不上报内容**（提示词、回复、工具参数与输出都不进 span 与指标），
//! **未知不是零**（服务商没报的 token 数不记录，不是记 0）。

use std::time::{Duration, Instant};

use komo_kernel::traits::{EmbedError, EmbeddingClient, SystemOne, TurnDriver};
use komo_kernel::types::model::{InputKind, ModelConfig, TokenUsage, Vector};
use komo_kernel::types::systemone::{SystemOneError, SystemOneRequest, SystemOneResponse};
use komo_kernel::types::turn::{LlmError, Round, RoundInput};
use tracing::field::Empty;
use tracing::{Instrument, Span};

/// `komo.llm.role` 的取值（`docs/observability.md` §3.1）。
pub mod role {
    pub const MAIN: &str = "main";
    pub const COMPACTION: &str = "compaction";
    pub const MEMORY_EXTRACT: &str = "memory.extract";
    pub const MEMORY_CONSOLIDATE: &str = "memory.consolidate";
    pub const RERANK: &str = "rerank";
    pub const TITLE: &str = "title";
    pub const EMBEDDING: &str = "embedding";
}

/// 指标事件的 target。init 侧按它放行 DEBUG。
pub const METRICS: &str = "komo::metrics";

/// `error.type`：低基数的失败类别，不带错误正文。
pub fn error_type(error: &LlmError) -> String {
    match error {
        LlmError::Rejected { status, .. } => status.to_string(),
        LlmError::Timeout => "timeout".into(),
        LlmError::Incomplete => "incomplete".into(),
        LlmError::UnsupportedEffort { .. } => "unsupported_effort".into(),
        LlmError::Transport(_) => "transport".into(),
        LlmError::Unknown(_) => "unknown".into(),
    }
}

/// 一次模型请求的 `chat` span。值只有事后才知道的字段先留空，用 [`finish_chat`]（以及
/// 适配器里的 [`note_response`]）补。
pub fn chat_span(role: &'static str, model: &str, effort: Option<&str>, attempt: u32) -> Span {
    tracing::info_span!(
        "chat",
        gen_ai.operation.name = "chat",
        gen_ai.request.model = model,
        komo.llm.role = role,
        komo.llm.effort = effort,
        komo.llm.attempt = attempt,
        gen_ai.response.model = Empty,
        gen_ai.response.id = Empty,
        gen_ai.usage.input_tokens = Empty,
        gen_ai.usage.output_tokens = Empty,
        komo.usage.cache_read_tokens = Empty,
        komo.usage.cache_write_tokens = Empty,
        komo.usage.reasoning_tokens = Empty,
        error.type = Empty,
    )
}

/// 适配器在读到服务商给的响应 id / 实际模型时调用：记到当前的 `chat` span 上（没有
/// 这个 span——比如测试里直接用适配器——就是空操作）。
pub fn note_response(id: Option<&str>, model: Option<&str>) {
    let span = Span::current();
    if let Some(id) = id {
        span.record("gen_ai.response.id", id);
    }
    if let Some(model) = model {
        span.record("gen_ai.response.model", model);
    }
}

fn record_usage(span: &Span, usage: &TokenUsage) {
    if let Some(input) = usage.input {
        span.record("gen_ai.usage.input_tokens", input);
    }
    if let Some(output) = usage.output {
        span.record("gen_ai.usage.output_tokens", output);
    }
    if let Some(read) = usage.cache_read {
        span.record("komo.usage.cache_read_tokens", read);
    }
    if let Some(write) = usage.cache_write {
        span.record("komo.usage.cache_write_tokens", write);
    }
    if let Some(reasoning) = usage.reasoning {
        span.record("komo.usage.reasoning_tokens", reasoning);
    }
}

/// 一次模型请求收尾：span 上补用量 / 失败类别，并发 LLM 指标。
pub fn finish_chat(
    span: &Span,
    role: &'static str,
    model: &str,
    elapsed: Duration,
    outcome: Result<&TokenUsage, &str>,
) {
    let failure = outcome.err();
    if let Some(kind) = failure {
        span.record("error.type", kind);
    }
    tracing::event!(
        target: "komo::metrics",
        tracing::Level::DEBUG,
        {
        histogram.gen_ai.client.operation.duration = elapsed.as_secs_f64(),
        gen_ai.request.model = model,
        komo.llm.role = role,
        error.type = failure,
        }
    );
    if let Ok(usage) = outcome {
        record_usage(span, usage);
        for (kind, tokens) in [
            ("input", usage.input),
            ("output", usage.output),
            ("cache_read", usage.cache_read),
            ("cache_write", usage.cache_write),
            ("reasoning", usage.reasoning),
        ] {
            if let Some(tokens) = tokens {
                tracing::event!(
                    target: "komo::metrics",
                    tracing::Level::DEBUG,
                    {
                    histogram.gen_ai.client.token.usage = tokens,
                    gen_ai.request.model = model,
                    komo.llm.role = role,
                    gen_ai.token.type = kind,
                    }
                );
            }
        }
    }
}

/// 模型请求失败后让出名额去等退避：重试计数（原因 = [`RetryCause`] 的词）。
///
/// [`RetryCause`]: komo_kernel::types::status::RetryCause
pub fn count_retry(model: &str, cause: &'static str) {
    tracing::event!(
        target: "komo::metrics",
        tracing::Level::DEBUG,
        {
        monotonic_counter.komo.llm.retries = 1u64,
        gen_ai.request.model = model,
        komo.retry.cause = cause,
        }
    );
}

/// 跑一回合（一次 provider 往返）并留下 `chat` span 与指标。成功失败都记。
///
/// 当前 span 就是它的父（主对话里是 `komo.round`）。`attempt` 是这条 Run 的第几次
/// 模型请求尝试，从 1 起；退避重试跨执行段，所以失败的那次与重试的那次在 Tempo 里是
/// 同一个 `komo.run.id` 下两棵树里的 `chat` span，用这个数对上。
pub async fn turn(
    driver: &mut dyn TurnDriver,
    input: RoundInput,
    role: &'static str,
    model: &ModelConfig,
    attempt: u32,
) -> Result<Round, LlmError> {
    let effort = model.effort.as_ref().map(ToString::to_string);
    let span = chat_span(role, &model.model, effort.as_deref(), attempt);
    let started = Instant::now();
    let result = driver.next(input).instrument(span.clone()).await;
    match &result {
        Ok(round) => finish_chat(
            &span,
            role,
            &model.model,
            started.elapsed(),
            Ok(&round.usage),
        ),
        Err(error) => finish_chat(
            &span,
            role,
            &model.model,
            started.elapsed(),
            Err(&error_type(error)),
        ),
    }
    result
}

/// 判断后端请求（重排）：同样是一个 `chat` span，用量取响应里**真报了**的数。
pub async fn ask(
    backend: &dyn SystemOne,
    request: SystemOneRequest,
    role: &'static str,
) -> Result<SystemOneResponse, SystemOneError> {
    let model = request.model.clone();
    let span = chat_span(role, &model, None, 1);
    let started = Instant::now();
    let result = backend.ask(request).instrument(span.clone()).await;
    match &result {
        Ok(response) => {
            span.record("gen_ai.response.model", response.model.as_str());
            let usage = TokenUsage {
                input: response.usage.input_tokens,
                output: response.usage.output_tokens,
                ..TokenUsage::default()
            };
            finish_chat(&span, role, &model, started.elapsed(), Ok(&usage));
        }
        Err(error) => {
            let kind = match error {
                SystemOneError::Transport(_) => "transport".to_string(),
                SystemOneError::Status { status, .. } => status.to_string(),
                SystemOneError::Decode(_) => "decode".to_string(),
                SystemOneError::Unconfigured(_) => "unconfigured".to_string(),
            };
            finish_chat(&span, role, &model, started.elapsed(), Err(&kind));
        }
    }
    result
}

/// 向量请求：一个 `embeddings` span 加耗时指标。服务商不报 token，所以只有耗时。
pub async fn embed(
    client: &dyn EmbeddingClient,
    kind: InputKind,
    texts: &[String],
) -> Result<Vec<Vector>, EmbedError> {
    let model = client.space().model.clone();
    let span = tracing::info_span!(
        "embeddings",
        gen_ai.operation.name = "embeddings",
        gen_ai.request.model = %model,
        komo.llm.role = role::EMBEDDING,
        error.type = Empty,
    );
    let started = Instant::now();
    let result = client.embed(kind, texts).instrument(span.clone()).await;
    let failure = result.as_ref().err().map(|error| match error {
        EmbedError::InvalidVector(_) => "invalid_vector",
        EmbedError::Unavailable(_) => "unavailable",
        EmbedError::Timeout => "timeout",
        EmbedError::Other(_) => "other",
    });
    finish_chat(
        &span,
        role::EMBEDDING,
        &model,
        started.elapsed(),
        failure.map_or(Ok(&TokenUsage::default()), Err),
    );
    result
}

#[cfg(any(test, feature = "test-support"))]
pub mod capture;
#[cfg(test)]
mod tests;
