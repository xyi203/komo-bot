//! 判断后端客户端的验收：请求形状、答复解析、重试与失败的分类。
//!
//! 全部走 `ScriptedTransport`——**没有网络、没有凭证**，所以这几条断言的是我们的代码，
//! 不是远端服务当时的行为。

use std::collections::BTreeMap;

use komo_kernel::types::systemone::{Answer, Question, SystemOneError, SystemOneRequest};

use super::*;
use crate::config::Secrets;
use crate::llm::transport::testing::{Reply, ScriptedTransport};

fn config() -> TypesafeConfig {
    TypesafeConfig {
        enabled: true,
        endpoint: "https://api.typesafe.ai/v1/systemone".into(),
        model: "jev-latest".into(),
        api_key: "TYPESAFE_API_KEY".into(),
        timeout_secs: 30,
    }
}

fn secrets() -> Secrets {
    Secrets::from_pairs([("TYPESAFE_API_KEY", "apikey_test")])
}

fn question() -> SystemOneRequest {
    SystemOneRequest {
        state: serde_json::json!({ "input": "客厅空调调到 24 度" }),
        model: "jev-latest".into(),
        questions: BTreeMap::from([(
            "most_relevant".to_string(),
            Question::Choice {
                instructions: serde_json::json!("哪一条最该想起来？"),
                criteria: BTreeMap::from([
                    ("m1".to_string(), serde_json::json!("空调设定 26 度")),
                    ("none".to_string(), serde_json::Value::Null),
                ]),
            },
        )]),
    }
}

#[tokio::test]
async fn a_configured_backend_sends_the_state_and_reads_the_answers_back() {
    let transport = Arc::new(ScriptedTransport::new(vec![Reply::json(
        200,
        r#"{"model":"jev-1.13.0","answers":{"most_relevant":{"type":"choice","choice":"m1",
            "probabilities":{"m1":0.7,"none":0.3},"confidence":0.8}},
            "usage":{"input_tokens":120,"output_tokens":12}}"#,
    )]));
    let client = connect(
        &config(),
        &secrets(),
        transport.clone() as Arc<dyn HttpTransport>,
    )
    .expect("配好了就造得出来");

    let response = client.ask(question()).await.expect("这一次成了");

    assert_eq!(response.model, "jev-1.13.0");
    assert_eq!(response.usage.input_tokens, 120);
    let Answer::Choice { choice, .. } = &response.answers["most_relevant"] else {
        panic!("{:?}", response.answers["most_relevant"])
    };
    assert_eq!(choice, "m1");

    // 发出去的就是 kernel 那份线上格式：端点、模型、state、问题各就各位。
    let sent = &transport.bodies()[0];
    assert_eq!(sent["model"], serde_json::json!("jev-latest"));
    assert_eq!(
        sent["state"]["input"],
        serde_json::json!("客厅空调调到 24 度")
    );
    assert_eq!(
        sent["questions"]["most_relevant"]["type"],
        serde_json::json!("choice")
    );
    assert_eq!(
        sent["questions"]["most_relevant"]["criteria"]["none"],
        serde_json::Value::Null
    );
}

/// 429 与 5xx 值得再试一次；4xx 是"你发的这个请求不对"，再试一百次也一样。
#[tokio::test]
async fn a_rate_limit_is_retried_and_a_bad_request_is_not() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        Reply::json(429, r#"{"error":"slow down"}"#),
        Reply::json(
            200,
            r#"{"model":"jev-1.13.0","answers":{"q":{"type":"noul","noul":0.4}},"usage":{}}"#,
        ),
    ]));
    let client = connect(
        &config(),
        &secrets(),
        transport.clone() as Arc<dyn HttpTransport>,
    )
    .expect("配好了就造得出来");
    let response = client.ask(question()).await.expect("第二次成了");
    assert_eq!(transport.requests().len(), 2, "429 要重试一次");
    assert_eq!(response.model, "jev-1.13.0");

    let transport = Arc::new(ScriptedTransport::new(vec![Reply::json(
        400,
        r#"{"error":"bad question"}"#,
    )]));
    let client = connect(
        &config(),
        &secrets(),
        transport.clone() as Arc<dyn HttpTransport>,
    )
    .expect("配好了就造得出来");
    let error = client.ask(question()).await.expect_err("400 不重试");
    let SystemOneError::Status { status, message } = error else {
        panic!("{error:?}")
    };
    assert_eq!(status, 400);
    assert!(message.contains("bad question"), "{message}");
    assert_eq!(transport.requests().len(), 1, "4xx 只发一次");
}

/// 解不开的答复是 [`SystemOneError::Decode`]：调用点据此降级，不把它当成"判断说不相关"。
#[tokio::test]
async fn an_unreadable_answer_is_a_decode_error() {
    let transport = Arc::new(ScriptedTransport::new(vec![Reply::json(200, "not json")]));
    let client = connect(&config(), &secrets(), transport as Arc<dyn HttpTransport>)
        .expect("配好了就造得出来");
    let error = client.ask(question()).await.expect_err("解不开就是失败");
    assert!(matches!(error, SystemOneError::Decode(_)), "{error:?}");
}

/// 没开、没端点、没凭证：造的时候就说不出来，而不是等到发请求才发现。
#[tokio::test]
async fn a_backend_without_a_key_or_a_switch_is_refused_at_build_time() {
    let transport = Arc::new(ScriptedTransport::new(vec![])) as Arc<dyn HttpTransport>;
    let mut off = config();
    off.enabled = false;
    assert!(matches!(
        connect(&off, &secrets(), Arc::clone(&transport)),
        Err(SystemOneError::Unconfigured(_))
    ));

    let empty = Secrets::from_pairs(Vec::<(&str, &str)>::new());
    assert!(matches!(
        connect(&config(), &empty, Arc::clone(&transport)),
        Err(SystemOneError::Unconfigured(_))
    ));
}
