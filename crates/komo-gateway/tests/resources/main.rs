//! 资源命名空间（`docs/komo_bot.md` §六、§4.7）：`skill://` / `tool://` / `artifact://`
//! 三种**只读**入口的端到端验收。
//!
//! 五条，每一条盯着一件"模型看到的东西":
//!
//! - **本地路径照旧**：`read` 一个 workspace 里的真文件，正文回来，计划里还是普通路径——
//!   兼容性就是"不出现资源语义"；
//! - **`skill://`**：数据目录里一份真 skill 读到正文，且审计里的计划目标是**逻辑 URI**
//!   （审计回答的是"读了哪个逻辑资源"，不是它落在哪个文件）；
//! - **`tool://`**：能力面之外的工具**在 `prepare` 就拒绝**，面内的读到
//!   JSON Schema；
//! - **`artifact://`**：会话目录 `artifacts/` 里的历史产物（python 工具删除前写下的），
//!   重启 Gateway 前后按同一条 URI 读到的正文一致；
//! - **别的会话的 run**：拿另一个会话的 `run` 拼出来的 URI 读不到——那是会话边界。
//!
//! 共用件在 `komo_gateway::service::test_support::harness`（真数据目录、真 `service::start`、
//! 脚本化模型）。这里断言的是**模型收到了什么**（`FakeLlm` 记下的轮输入）与**账本里写了
//! 什么**（`tool.planned` 的计划、`output.json` 的产物引用）。

use komo_gateway::service::test_support::harness::{
    DEFAULT_ENV, FakeLlm, Home, call_round, write_home,
};
use komo_kernel::types::turn::{LlmError, Round, RoundInput};

// ---------------------------------------------------------------- 配置

/// 一份最小的 config.toml，`agents` 是 `[agents.assistant]` 那一节。
///
/// **自己写而不是拼 `config_toml`**：这几条要按 Agent 收工具面（`tools = […]`），而
/// `config_toml` 已经写了 `[agents.assistant]`，再写一次就是 TOML 的重复表。
fn config(agents: &str) -> String {
    format!(
        r#"
default_agent = "assistant"

{agents}

[channels.telegram]
enabled = true
allow_from = [111]
home_chat = 111

[model.main]
type = "completion"
api_backend = "responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[models]
default = "main"

[memory]
enabled = false
"#
    )
}

/// 一份给 Agent 的工具面。
fn tools(list: &str) -> String {
    format!("[agents.assistant]\ntools = [{list}]\n")
}

/// 一个数据目录 + 一份给定的 config.toml。
///
/// 与 `tests/agents` 的 `rewrite_config` 同一个做法：几条 config 要引用数据目录**自己**的
/// 路径（skills 根），而 `Home::with_config` 建临时目录时那个路径还不知道。
fn home_with(config: &str) -> Home {
    let home = Home::new();
    write_home(home.path(), config, DEFAULT_ENV);
    home
}

// ---------------------------------------------------------------- 轮脚本

/// 一次 `read`（路径可以是本地路径，也可以是资源 URI）。
fn read_call(n: u32, id: &str, path: &str) -> Result<Round, LlmError> {
    call_round(n, id, "read", serde_json::json!({ "path": path }))
}

// ---------------------------------------------------------------- 断言助手

/// 模型被喂回来的**全部**工具结果，按顺序：正文 + 是不是错误。
///
/// 一次运行里同一个工具可以调多次（`tests/agents` 的 `fed_back` 只按 `needle` 取第一条），
/// 而这里要按顺序看每一条——被 `prepare` 拒绝的调用交回模型的是执行器那句话本身，不是
/// 投影出来的 `[工具 · 状态 · 耗时]` 抬头，所以不能只按抬头找。
fn fed_back(llm: &FakeLlm) -> Vec<(String, bool)> {
    llm.inputs
        .lock()
        .expect("轮输入")
        .iter()
        .flat_map(|input| match input {
            RoundInput::ToolResults { results, .. } => results
                .iter()
                .map(|result| (result.content.clone(), result.is_error))
                .collect::<Vec<_>>(),
            RoundInput::First => Vec::new(),
        })
        .collect()
}

// ---------------------------------------------------------------- 本地路径照旧

// ---------------------------------------------------------------- skill://

// ---------------------------------------------------------------- tool://

// ---------------------------------------------------------------- artifact://

// ---------------------------------------------------------------- 别的会话的 run

mod durable;
