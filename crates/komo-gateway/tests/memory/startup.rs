//! 启动就绪与向量维度探测的关系（§3、§9.5）。
//!
//! §9.5 说「省略 `dimensions` 时先探一次」。这一次探测是**一个网络往返**，而它曾经跑在
//! 绑监听与写发现文件之前——端点不回话时 `Gateway 就绪` 与发现文件一起被推后整整一个模型
//! 超时（本机实测 120s），`komo gateway restart` 看上去就是卡住。这里的断言是：探测不在
//! 就绪路径上，探不到就按 §9.4 降级。

use std::time::Duration;

use crate::harness::*;

/// 一个收下连接就再也不回话的端点。探测要等满模型超时，所以"等它"与"不等它"差着一个
/// 数量级——这正是这条测试能测出回归的地方。
async fn a_silent_endpoint() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("占一个端口");
    let port = listener.local_addr().expect("端口").port();
    tokio::spawn(async move {
        // 拿住连接不放：客户端读到的是"没有响应"，不是"连不上"。
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    port
}

/// 一份把向量端点指向那个哑端点的配置：`dimensions` **省略**，所以启动时必然要探一次。
fn config_pointing_at(port: u16) -> String {
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

[model.embedding]
type = "embedding"
api_backend = "embeddings"
base_url = "http://127.0.0.1:{port}/v1"
model = "concept-v1"
api_key_env = "KOMO_EMBEDDING_API_KEY"

[models]
default = "main"

[memory]
enabled = true
embedding = "embedding"

[memory.retrieval]
mode = "hybrid"
"#
    )
}

/// 端点不回话时 Gateway 照样起得来、发现文件照样写、HTTP 照样收请求。
#[tokio::test]
async fn a_silent_embedding_endpoint_never_holds_up_readiness() {
    let port = a_silent_endpoint().await;
    let mut started = tokio::time::timeout(
        Duration::from_secs(5),
        memory_gateway(&config_pointing_at(port)).start(),
    )
    .await
    .expect("就绪不该等向量探测：等满一个超时就是又回到「先探模型再起服务」");

    // 发现文件在，说明客户端能按 §3 第 1 步找到这台实例。
    let discovery = started.home.join("runtime/gateway.json");
    assert!(discovery.is_file(), "{}", discovery.display());

    // HTTP 也在服务（`/healthz` 不走认证）。
    let (status, body) = started.get("/healthz").await;
    assert_eq!(status, 200, "{body}");

    // 探测还没落定（端点不回话），所以这一刻没有空间指纹：§9.4 的降级，不是配置错误。
    assert!(
        started.state().memories.space().is_none(),
        "端点不回话时不该凭空有一个空间"
    );

    started.stop().await;
}
