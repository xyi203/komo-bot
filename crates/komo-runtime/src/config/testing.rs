//! 配置测试的公用夹具。

use std::path::Path;

use super::{ConfigHolder, LoadOptions, Sources};

/// `.env` 里那个值——泄漏测试拿它当探针。
pub const SECRET_VALUE: &str = "sk-do-not-log-this-0xdeadbeef";

pub fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

/// 一个真实的数据目录：config.toml + .env。
pub struct Fixture {
    home: tempfile::TempDir,
}

impl Fixture {
    pub fn valid() -> Self {
        let home = tempfile::tempdir().unwrap();
        let sources = Sources::under(home.path());
        write(&sources.config, &Self::config_text("chat-a", "medium"));
        write(&sources.env, &Self::env_text());
        Fixture { home }
    }

    pub fn path(&self) -> &Path {
        self.home.path()
    }

    pub fn sources(&self) -> Sources {
        Sources::under(self.home.path())
    }

    pub fn options(&self) -> LoadOptions {
        LoadOptions::at(self.home.path())
    }

    pub fn holder(&self) -> ConfigHolder {
        ConfigHolder::load(&self.options()).unwrap()
    }

    pub fn config_text(model: &str, effort: &str) -> String {
        format!(
            r#"
default_agent = "assistant"

[agents.assistant]
[model_providers.openrouter]
base_url = "https://llm.example.com/v1"
env_key = "KOMO_LLM_API_KEY"
api_backend = "responses"

[model.chat]
type = "completion"
model = "{model}"
model_provider = "openrouter"
effort = "{effort}"

[model.memory]
type = "completion"
base_url = "https://memory-llm.example.com/v1"
model = "memory-a"
api_key_env = "KOMO_MEMORY_API_KEY"
api_backend = "responses"
effort = "low"

[model.embedding]
type = "embedding"
base_url = "https://embedding.example.com/v1"
model = "embed-a"
api_key_env = "KOMO_EMBEDDING_API_KEY"
api_backend = "embeddings"
dimensions = 1024

[models]
default = "chat"

[memory]
enabled = true
model = "memory"
embedding = "embedding"

[memory.retrieval]
mode = "hybrid"
candidate_limit = 40
top_k = 8
max_tokens = 1500

[channels.feishu]
enabled = true
allow_from = ["ou_operator"]
home_chat = "oc_home"
"#
        )
    }

    pub fn env_text() -> String {
        format!(
            "KOMO_LLM_API_KEY={SECRET_VALUE}\n\
             KOMO_MEMORY_API_KEY={SECRET_VALUE}-memory\n\
             KOMO_EMBEDDING_API_KEY={SECRET_VALUE}-embed\n\
             FEISHU_APP_ID=cli_fixture\n\
             FEISHU_APP_SECRET={SECRET_VALUE}-feishu\n"
        )
    }
}

/// 跨 crate 的测试替身按 §13.4 住在 kernel 的 `test-support` 里（`komo-agent` 的测试也
/// 要它）。这里只转一道，保持 `crate::config::testing::snapshot_fixture` 的调用点不动。
pub use komo_kernel::test_support::snapshot_fixture;
