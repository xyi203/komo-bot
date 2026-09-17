//! 这一组测试要的那一台 Gateway：共用件在
//! `komo_gateway::service::test_support::harness`，这里只留 memory 特有的两样——一份带
//! `[memory]` 的 config.toml，和一个**按概念**给向量的 embedding 替身（同一件事的两种
//! 说法要落在同一个方向上，否则"换一种表述仍可语义召回"这条验收就只是在测哈希）。

#![allow(dead_code, unused_imports)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::traits::{EmbedError, EmbeddingClient};
use komo_kernel::types::model::{DistanceRule, EmbeddingSpace, InputKind, Vector};

pub use komo_gateway::service::test_support::harness::*;

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

/// `[memory]` 的三把钥匙：主模型、记忆模型、向量端点各一把（§13.3）。
pub const MEMORY_ENV: &str = "KOMO_LLM_API_KEY=test-key\n\
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

// ---------------------------------------------------------------- 起一台

/// 带 `[memory]` 三把钥匙的 builder（共用件的默认 `.env` 只有主模型那一把）。
pub fn memory_gateway(config: &str) -> GatewayBuilder {
    GatewayBuilder::new(config).env(MEMORY_ENV)
}

// ---------------------------------------------------------------- 等待

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
