//! 向量适配器的断言：请求体里有什么、没有什么，以及维度不符时会怎样。

use std::sync::Arc;

use komo_kernel::traits::EmbedError;
use komo_kernel::types::model::{Effort, EmbeddingConfig, InputKind, ModelConfig};
use serde_json::json;

use super::*;
use crate::config::Secrets;
use crate::llm::transport::testing::{Reply, ScriptedTransport};

fn config(provider: &str, dimensions: Option<u32>) -> EmbeddingConfig {
    EmbeddingConfig {
        model: ModelConfig {
            provider: provider.into(),
            base_url: "https://embedding.example.com/v1".into(),
            model: "embed-a".into(),
            api_key_env: "KOMO_EMBEDDING_API_KEY".into(),
            effort: None,
            efforts: None,
            timeout_secs: 10,
        },
        revision: Some("2026-09".into()),
        dimensions,
        document_prefix: None,
        query_prefix: None,
    }
}

fn secrets() -> Secrets {
    Secrets::from_pairs([("KOMO_EMBEDDING_API_KEY", "sk-embedding-secret")])
}

fn client(
    config: &EmbeddingConfig,
    transport: &ScriptedTransport,
    dimensions: u32,
) -> Arc<dyn EmbeddingClient> {
    build_embedding(config, &secrets(), Arc::new(transport.clone()), dimensions).unwrap()
}

fn openai_reply(vectors: &[&[f32]]) -> Reply {
    let data: Vec<_> = vectors
        .iter()
        .enumerate()
        .map(|(index, values)| json!({ "index": index, "embedding": values }))
        .collect();
    Reply::json(200, &json!({ "data": data }).to_string())
}

#[tokio::test]
async fn an_embedding_request_carries_no_chat_fields() {
    let transport = ScriptedTransport::new(vec![openai_reply(&[&[0.1, 0.2]])]);
    let client = client(&config(OPENAI_COMPATIBLE, Some(2)), &transport, 2);
    client
        .embed(InputKind::Document, &["你好".into()])
        .await
        .unwrap();

    let body = &transport.bodies()[0];
    assert_eq!(body["model"], json!("embed-a"));
    assert_eq!(body["input"], json!(["你好"]));
    assert_eq!(body["dimensions"], json!(2));
    for forbidden in [
        "messages",
        "tools",
        "tool_choice",
        "stream",
        "reasoning_effort",
    ] {
        assert!(
            body.get(forbidden).is_none(),
            "{forbidden} 不该出现：{body}"
        );
    }
    assert_eq!(
        transport.requests()[0].url,
        "https://embedding.example.com/v1/embeddings"
    );
}

#[tokio::test]
async fn the_ollama_backend_posts_to_api_embed() {
    let transport = ScriptedTransport::new(vec![Reply::json(
        200,
        &json!({ "embeddings": [[0.3, 0.4]] }).to_string(),
    )]);
    let mut cfg = config(OLLAMA, Some(2));
    cfg.model.base_url = "http://localhost:11434".into();
    let client = client(&cfg, &transport, 2);

    let vectors = client
        .embed(InputKind::Query, &["查询".into()])
        .await
        .unwrap();
    assert_eq!(vectors[0].dimensions(), 2);
    assert_eq!(
        transport.requests()[0].url,
        "http://localhost:11434/api/embed"
    );
    let body = &transport.bodies()[0];
    assert!(body.get("reasoning_effort").is_none());
    assert!(body.get("tools").is_none());
}

/// §9.5：截断或结构错误的向量不接受。
#[tokio::test]
async fn a_vector_of_the_wrong_dimension_is_refused() {
    let transport = ScriptedTransport::new(vec![openai_reply(&[&[0.1, 0.2, 0.3]])]);
    let client = client(&config(OPENAI_COMPATIBLE, Some(2)), &transport, 2);
    let Err(error) = client.embed(InputKind::Document, &["x".into()]).await else {
        panic!("维度不符必须拒绝")
    };
    assert!(matches!(error, EmbedError::InvalidVector(_)), "{error:?}");
    assert!(error.to_string().contains("实际 3"), "{error}");
}

#[tokio::test]
async fn a_zero_vector_is_refused_too() {
    let transport = ScriptedTransport::new(vec![openai_reply(&[&[0.0, 0.0]])]);
    let client = client(&config(OPENAI_COMPATIBLE, Some(2)), &transport, 2);
    let Err(error) = client.embed(InputKind::Document, &["x".into()]).await else {
        panic!("零范数向量不可用")
    };
    assert!(matches!(error, EmbedError::InvalidVector(_)), "{error:?}");
}

#[tokio::test]
async fn a_short_batch_is_refused_rather_than_silently_misaligned() {
    let transport = ScriptedTransport::new(vec![openai_reply(&[&[0.1, 0.2]])]);
    let client = client(&config(OPENAI_COMPATIBLE, Some(2)), &transport, 2);
    let Err(error) = client
        .embed(InputKind::Document, &["a".into(), "b".into()])
        .await
    else {
        panic!("少回一条就是错位")
    };
    assert!(matches!(error, EmbedError::InvalidVector(_)), "{error:?}");
}

#[tokio::test]
async fn rows_out_of_order_are_put_back_by_index() {
    let transport = ScriptedTransport::new(vec![Reply::json(
        200,
        &json!({ "data": [
            { "index": 1, "embedding": [0.0, 1.0] },
            { "index": 0, "embedding": [1.0, 0.0] }
        ]})
        .to_string(),
    )]);
    let client = client(&config(OPENAI_COMPATIBLE, Some(2)), &transport, 2);
    let vectors = client
        .embed(InputKind::Document, &["a".into(), "b".into()])
        .await
        .unwrap();
    assert_eq!(vectors[0].0, vec![1.0, 0.0]);
    assert_eq!(vectors[1].0, vec![0.0, 1.0]);
}

#[tokio::test]
async fn a_server_that_is_down_is_unavailable_not_an_empty_result() {
    let transport = ScriptedTransport::new(vec![Reply::json(503, "upstream down")]);
    let client = client(&config(OPENAI_COMPATIBLE, Some(2)), &transport, 2);
    let Err(error) = client.embed(InputKind::Document, &["x".into()]).await else {
        panic!("端点挂了不能读成「没有相关记忆」")
    };
    assert!(matches!(error, EmbedError::Unavailable(_)), "{error:?}");
}

/// §9.5：同维度不代表同一空间；凭证不进指纹。
#[test]
fn the_space_fingerprint_separates_models_and_revisions_and_holds_no_credential() {
    let transport = ScriptedTransport::new(vec![]);
    let base = client(&config(OPENAI_COMPATIBLE, Some(1024)), &transport, 1024);

    let mut other_model = config(OPENAI_COMPATIBLE, Some(1024));
    other_model.model.model = "embed-b".into();
    let other = client(&other_model, &transport, 1024);
    assert_ne!(
        base.space().fingerprint(),
        other.space().fingerprint(),
        "同维度不同模型不是同一个空间"
    );

    let mut other_revision = config(OPENAI_COMPATIBLE, Some(1024));
    other_revision.revision = Some("2026-10".into());
    let rotated = client(&other_revision, &transport, 1024);
    assert_ne!(base.space().fingerprint(), rotated.space().fingerprint());

    // 凭证不在空间里——序列化、Debug 都找不到它。
    let space = serde_json::to_string(base.space()).unwrap();
    assert!(!space.contains("sk-embedding-secret"), "{space}");
    assert!(!format!("{:?}", base.space()).contains("sk-embedding-secret"));
    assert_eq!(base.space().effort, None, "向量接口不带 effort");
    assert_eq!(base.space().preprocessing, PREPROCESSING);
}

/// §13.3：普通向量接口没有 effort 参数时必须省略——造都造不出来。
#[test]
fn an_embedding_config_with_an_effort_is_refused_at_construction() {
    let transport = ScriptedTransport::new(vec![]);
    let mut cfg = config(OPENAI_COMPATIBLE, Some(2));
    cfg.model.effort = Some(Effort::new("low"));
    let Err(error) = build_embedding(&cfg, &secrets(), Arc::new(transport), 2) else {
        panic!("带 effort 的向量配置不该造得出客户端")
    };
    assert!(matches!(
        error,
        EmbeddingBuildError::EffortNotSupported { .. }
    ));
}

#[test]
fn an_unknown_vector_provider_is_refused() {
    let transport = ScriptedTransport::new(vec![]);
    let Err(error) = build_embedding(
        &config("cohere", Some(2)),
        &secrets(),
        Arc::new(transport),
        2,
    ) else {
        panic!("不认识的向量 provider 不该造得出客户端")
    };
    assert!(matches!(error, EmbeddingBuildError::UnknownProvider { .. }));
}

/// §9.5：维度省略时用模型返回的维度，**校验后固定**到索引代次。
#[tokio::test]
async fn an_omitted_dimension_is_probed_once_and_then_fixed() {
    let transport = ScriptedTransport::new(vec![
        openai_reply(&[&[0.1, 0.2, 0.3, 0.4]]),
        openai_reply(&[&[0.5, 0.6, 0.7, 0.8]]),
    ]);
    let client = connect_embedding(
        &config(OPENAI_COMPATIBLE, None),
        &secrets(),
        Arc::new(transport.clone()),
    )
    .await
    .unwrap();

    assert_eq!(client.space().dimensions, 4);
    let vectors = client
        .embed(InputKind::Document, &["真的一次".into()])
        .await
        .unwrap();
    assert_eq!(vectors[0].dimensions(), 4);
    assert_eq!(transport.requests().len(), 2, "探测一次 + 真的一次");
    // 探测请求不带 dimensions（还不知道），真的那次也不带（配置里省略了）。
    assert!(transport.bodies()[1].get("dimensions").is_none());
}

#[tokio::test]
async fn a_configured_dimension_never_costs_a_probe() {
    let transport = ScriptedTransport::new(vec![openai_reply(&[&[0.1, 0.2]])]);
    let client = connect_embedding(
        &config(OPENAI_COMPATIBLE, Some(2)),
        &secrets(),
        Arc::new(transport.clone()),
    )
    .await
    .unwrap();
    assert!(transport.requests().is_empty(), "维度已知就不该碰网络");
    client
        .embed(InputKind::Document, &["x".into()])
        .await
        .unwrap();
    assert_eq!(transport.requests().len(), 1);
}
