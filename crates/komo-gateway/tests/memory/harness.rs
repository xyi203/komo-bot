//! 一整台真 Gateway，带上 `[memory]`：真数据目录、真 state.db、真 HTTP 监听、真
//! MemoryManager，模型与向量端点是替身。
//!
//! 照 `tests/chat/harness.rs` 的形状写（那一份是 `#[cfg(test)]` 的，集成测试拿不到）。
//! 这里只多两样：一份带 `[memory]` 的 config.toml，和一个**按概念**给向量的 embedding
//! 替身——同一件事的两种说法要落在同一个方向上，否则"换一种表述仍可语义召回"这条验收
//! 就只是在测哈希。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_gateway::service::state::GatewayState;
use komo_gateway::service::{Running, ServiceOptions, start};
use komo_kernel::traits::{EmbedError, EmbeddingClient, LlmClient};
use komo_kernel::types::model::{DistanceRule, EmbeddingSpace, InputKind, Vector};

// ---------------------------------------------------------------- 配置

/// 一份开着 `[memory]` 的 config.toml。`embedding` 那一段照写，但端点由测试替身接管
/// （`ServiceOptions::embeddings`），所以地址是假的也无妨。
pub fn memory_config(mode: &str, with_embedding: bool) -> String {
    let embedding = if with_embedding {
        r#"
[memory.embedding]
provider = "openai_compatible"
base_url = "https://embedding.example.com/v1"
model = "concept-v1"
api_key_env = "KOMO_EMBEDDING_API_KEY"
dimensions = 4
"#
    } else {
        ""
    };
    format!(
        r#"
[model]
provider = "openai_responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[memory]
enabled = true

[memory.model]
provider = "openai_responses"
base_url = "https://memory-llm.example.com/v1"
model = "memory-test"
api_key_env = "KOMO_MEMORY_API_KEY"
effort = "low"
{embedding}
[memory.retrieval]
mode = "{mode}"
candidate_limit = 40
top_k = 8
max_tokens = 1500
"#
    )
}

const DEFAULT_ENV: &str = "KOMO_LLM_API_KEY=test-key\n\
                           KOMO_MEMORY_API_KEY=test-memory-key\n\
                           KOMO_EMBEDDING_API_KEY=test-embedding-key\n";

// ---------------------------------------------------------------- 向量替身

/// 每个概念占一维；文本里出现了哪几个概念，那几维就是 1。
///
/// 于是"喜欢深色主题"与"界面都给我用暗色"真的指向同一个方向——按哈希摊开的替身在这件事
/// 上什么都证明不了。
pub struct ConceptEmbeddings {
    space: EmbeddingSpace,
    down: Mutex<bool>,
    concepts: Vec<Vec<&'static str>>,
}

impl std::fmt::Debug for ConceptEmbeddings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConceptEmbeddings").finish_non_exhaustive()
    }
}

impl ConceptEmbeddings {
    pub fn new() -> Arc<Self> {
        let concepts: Vec<Vec<&'static str>> = vec![
            vec!["深色", "暗色", "dark", "黑色"],
            vec!["主题", "界面", "theme", "配色"],
            vec!["空调", "温度", "制冷"],
            vec!["cargo", "测试", "test"],
        ];
        Arc::new(ConceptEmbeddings {
            space: EmbeddingSpace {
                provider: "openai_compatible".into(),
                endpoint: "https://embedding.example.com/v1".into(),
                model: "concept-v1".into(),
                revision: None,
                dimensions: concepts.len() as u32,
                preprocessing: "trim-v1".into(),
                document_prefix: String::new(),
                query_prefix: String::new(),
                normalized: false,
                distance: DistanceRule::Cosine,
                effort: None,
            },
            down: Mutex::new(false),
            concepts,
        })
    }

    /// 把端点关掉 / 打开——测 §9.4 的降级那两句。
    pub fn set_down(&self, down: bool) {
        *self.down.lock().expect("向量替身") = down;
    }
}

#[async_trait]
impl EmbeddingClient for ConceptEmbeddings {
    fn space(&self) -> &EmbeddingSpace {
        &self.space
    }

    async fn embed(&self, _kind: InputKind, texts: &[String]) -> Result<Vec<Vector>, EmbedError> {
        if *self.down.lock().expect("向量替身") {
            return Err(EmbedError::Unavailable("测试里把端点关掉了".into()));
        }
        Ok(texts
            .iter()
            .map(|text| {
                let lower = text.to_lowercase();
                let mut values: Vec<f32> = self
                    .concepts
                    .iter()
                    .map(|synonyms| {
                        if synonyms.iter().any(|word| lower.contains(word)) {
                            1.0
                        } else {
                            0.0
                        }
                    })
                    .collect();
                if values.iter().all(|v| *v == 0.0) {
                    values[0] = 0.01;
                }
                Vector(values)
            })
            .collect())
    }
}

// ---------------------------------------------------------------- Gateway

pub struct GatewayBuilder {
    config: String,
    llm: Option<Arc<dyn LlmClient>>,
    embeddings: Option<Arc<dyn EmbeddingClient>>,
    home: Option<PathBuf>,
}

impl GatewayBuilder {
    pub fn new(config: &str) -> Self {
        GatewayBuilder {
            config: config.to_string(),
            llm: None,
            embeddings: None,
            home: None,
        }
    }

    pub fn llm(mut self, llm: Arc<dyn LlmClient>) -> Self {
        self.llm = Some(llm);
        self
    }

    pub fn embeddings(mut self, client: Arc<dyn EmbeddingClient>) -> Self {
        self.embeddings = Some(client);
        self
    }

    pub fn at(mut self, home: &Path) -> Self {
        self.home = Some(home.to_path_buf());
        self
    }

    pub async fn start(self) -> TestGateway {
        install_crypto();
        let (home_dir, home) = match self.home {
            Some(path) => (None, path),
            None => {
                let dir = tempfile::tempdir().expect("临时数据目录");
                let path = dir.path().to_path_buf();
                (Some(dir), path)
            }
        };
        write_home(&home, &self.config);

        let running = start(ServiceOptions {
            home: Some(home.clone()),
            listen: Some("127.0.0.1:0".into()),
            channels: Vec::new(),
            llm: self.llm,
            embeddings: self.embeddings,
        })
        .await
        .expect("Gateway 起得来");

        TestGateway {
            _home_dir: home_dir,
            home,
            running: Some(running),
        }
    }
}

pub struct TestGateway {
    _home_dir: Option<tempfile::TempDir>,
    pub home: PathBuf,
    running: Option<Running>,
}

impl TestGateway {
    fn running(&self) -> &Running {
        self.running.as_ref().expect("Gateway 还在跑")
    }

    pub fn state(&self) -> &Arc<GatewayState> {
        &self.running().state
    }

    pub fn base_url(&self) -> &str {
        &self.running().base_url
    }

    pub fn token(&self) -> &str {
        &self.running().state.token
    }

    pub async fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop().await;
        }
    }

    /// 停机再按**同一个数据目录**起一台（"重建中崩溃可接续"用它）。
    pub async fn restart(&mut self, config: &str, embeddings: Arc<dyn EmbeddingClient>) {
        self.stop().await;
        let started = GatewayBuilder::new(config)
            .at(&self.home)
            .embeddings(embeddings)
            .start()
            .await;
        self.running = started.running;
    }

    pub async fn get(&self, path: &str) -> (u16, serde_json::Value) {
        self.request(reqwest::Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }

    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base_url()))
            .bearer_auth(self.token());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("请求发得出去");
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let json =
            serde_json::from_str(&text).unwrap_or_else(|_| serde_json::Value::String(text.clone()));
        (status, json)
    }
}

fn write_home(home: &Path, config: &str) {
    std::fs::create_dir_all(home).expect("数据目录");
    std::fs::write(home.join("config.toml"), config).expect("写 config.toml");
    std::fs::write(home.join(".env"), DEFAULT_ENV).expect("写 .env");
    std::fs::write(home.join("policy.toml"), "rules = []\n").expect("写 policy.toml");
}

/// reqwest 用 `rustls-no-provider`：provider 由 `komo` 的 `main` 装（§13.4）。
pub fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// 等一个条件成真（或超时）。
pub async fn eventually<F: Fn() -> bool>(what: &str, done: F) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("等了 10 秒还没有：{what}");
}

/// 等一个 Run 走到终态。
///
/// **不能用 `eventually` 包一个 `block_on`**：那会在 tokio 的工作线程上阻塞着等另一个
/// 需要同一个运行时的 future，是个现成的死锁。所以这一条自己 `await`。
pub async fn wait_for_terminal(gateway: &TestGateway, run: &komo_kernel::types::ids::RunId) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Ok(Some(record)) = komo_store::repos::runs::get(&gateway.state().db, run).await
            && record.status.is_terminal()
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("等了 10 秒 run {run} 还没到终态");
}
