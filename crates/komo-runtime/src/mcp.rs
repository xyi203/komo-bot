//! MCP 客户端（`docs/mcp.md`）：连上一个服务器、列出它的工具、调用其中一个。
//!
//! 传输与协议都交给 `rmcp`（官方 Rust SDK），这里只管 komo 自己的三件事：
//!
//! - **凭证只给声明过的人。** stdio 子进程不继承 Gateway 的环境：先清空，只放回跑得起来
//!   所需的几个变量，再加上配置里 `env` 列出的 `.env` 变量。HTTP 的 bearer token 同理。
//! - **工具名进 komo 的命名空间**：`mcp__<server>__<tool>`，与内置工具不会撞名。
//! - **子进程的 stderr 进日志**，不糊在 Gateway 的终端上。

use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use komo_kernel::traits::Tool;

use komo_kernel::protocol::config::{McpServerConfig, McpTransport};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};

use crate::config::Secrets;
use crate::tools::McpTool;

/// 连接 + 握手 + 列工具的总时限：一个起不来的服务器不该把 Gateway 的启动拖住。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 模型侧工具名的长度上限（OpenAI 的 function name 是 64）。
const MAX_TOOL_NAME: usize = 64;

/// stdio 子进程从 Gateway 继承的变量：只有让 `npx` / `uvx` 这类启动器跑得起来的那几个。
const INHERITED_ENV: &[&str] = &["PATH", "HOME", "USER", "LANG", "TMPDIR", "SHELL"];

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("`.env` 里没有 {0}")]
    MissingSecret(String),
    #[error("起不来：{0}")]
    Spawn(String),
    #[error("握手失败：{0}")]
    Handshake(String),
    #[error("列不出工具：{0}")]
    ListTools(String),
    #[error("{}s 内没连上", CONNECT_TIMEOUT.as_secs())]
    Timeout,
}

/// 一个连着的服务器。丢掉它就断开（stdio 子进程随之被杀掉）。
pub struct McpServer {
    name: String,
    service: RunningService<RoleClient, ()>,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer")
            .field("name", &self.name)
            .finish()
    }
}

impl McpServer {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 调一次工具。传输层的错误原样交回：发出去之后断掉的那一种，调用方不知道副作用
    /// 发生没有。
    pub async fn call(&self, tool: &str, arguments: JsonObject) -> Result<CallToolResult, String> {
        self.service
            .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
            .await
            .map_err(|error| error.to_string())
    }
}

/// 连上并列出工具。
pub async fn connect(
    name: &str,
    config: &McpServerConfig,
    secrets: &Secrets,
) -> Result<(McpServer, Vec<rmcp::model::Tool>), McpError> {
    tokio::time::timeout(CONNECT_TIMEOUT, connect_inner(name, config, secrets))
        .await
        .map_err(|_elapsed| McpError::Timeout)?
}

async fn connect_inner(
    name: &str,
    config: &McpServerConfig,
    secrets: &Secrets,
) -> Result<(McpServer, Vec<rmcp::model::Tool>), McpError> {
    let service = match &config.transport {
        McpTransport::Stdio { command, args, env } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args).env_clear();
            for key in INHERITED_ENV {
                if let Ok(value) = std::env::var(key) {
                    cmd.env(key, value);
                }
            }
            for key in env {
                let value = secrets
                    .get(key)
                    .ok_or_else(|| McpError::MissingSecret(key.clone()))?;
                cmd.env(key, value);
            }
            let (transport, stderr) = TokioChildProcess::builder(cmd)
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| McpError::Spawn(error.to_string()))?;
            if let Some(stderr) = stderr {
                forward_stderr(name.to_string(), stderr);
            }
            ().serve(transport)
                .await
                .map_err(|error| McpError::Handshake(error.to_string()))?
        }
        McpTransport::Http {
            url,
            bearer_token_env,
        } => {
            let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url.clone());
            if let Some(key) = bearer_token_env {
                let token = secrets
                    .get(key)
                    .ok_or_else(|| McpError::MissingSecret(key.clone()))?;
                transport_config = transport_config.auth_header(token);
            }
            ().serve(StreamableHttpClientTransport::from_config(transport_config))
                .await
                .map_err(|error| McpError::Handshake(error.to_string()))?
        }
    };
    let tools = service
        .list_all_tools()
        .await
        .map_err(|error| McpError::ListTools(error.to_string()))?;
    Ok((
        McpServer {
            name: name.to_string(),
            service,
        },
        tools,
    ))
}

fn forward_stderr(server: String, stderr: tokio::process::ChildStderr) {
    use tokio::io::AsyncBufReadExt;
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(server = %server, "{line}");
        }
    });
}

/// 模型看到的工具名：`mcp__<server>__<tool>`，JS / OpenAI 标识符里不能有的字符换成 `_`。
/// 超过 [`MAX_TOOL_NAME`] 就是 `None`——截断会让两个工具撞成一个名字。
pub fn tool_name(server: &str, tool: &str) -> Option<String> {
    let clean = |raw: &str| -> String {
        raw.chars()
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
                _ => '_',
            })
            .collect()
    };
    let name = format!("mcp__{}__{}", clean(server), clean(tool));
    (name.len() <= MAX_TOOL_NAME).then_some(name)
}

/// 按配置挑出要挂的工具：`tools` 白名单（没写 = 全部）。白名单里写了、服务器没有的名字
/// 报出来——静默采纳一份写错的配置，等于让操作者以为给了。
pub fn select<'a>(
    config: &McpServerConfig,
    listed: &'a [rmcp::model::Tool],
) -> (Vec<&'a rmcp::model::Tool>, Vec<String>) {
    let Some(wanted) = &config.tools else {
        return (listed.iter().collect(), Vec::new());
    };
    let wanted: BTreeSet<&str> = wanted.iter().map(String::as_str).collect();
    let kept: Vec<_> = listed
        .iter()
        .filter(|tool| wanted.contains(tool.name.as_ref()))
        .collect();
    let missing = wanted
        .into_iter()
        .filter(|name| !listed.iter().any(|tool| tool.name == *name))
        .map(str::to_string)
        .collect();
    (kept, missing)
}

/// 启动时连上全部服务器，交回它们的工具。服务器之间并行；连不上的、名字放不进去的
/// 都记一条 warn 跳过——一个坏掉的 MCP 服务器不该让整台 Gateway 起不来，也不该让别的
/// 服务器的工具跟着消失。
pub async fn connect_all(
    servers: &BTreeMap<String, McpServerConfig>,
    secrets: &Secrets,
) -> Vec<Arc<dyn Tool>> {
    let connected =
        futures_util::future::join_all(servers.iter().map(|(name, config)| async move {
            (name, config, connect(name, config, secrets).await)
        }))
        .await;

    let mut seen = BTreeSet::new();
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for (name, config, result) in connected {
        let (server, listed) = match result {
            Ok(connected) => connected,
            Err(error) => {
                tracing::warn!(server = %name, %error, "MCP 服务器连不上，它的工具这次不挂");
                continue;
            }
        };
        let (selected, missing) = select(config, &listed);
        if !missing.is_empty() {
            tracing::warn!(
                server = %name,
                missing = %missing.join("、"),
                "[mcp.servers] tools 里写了服务器没有的工具，它们不算数"
            );
        }
        let server = Arc::new(server);
        let mut count = 0usize;
        for remote in selected {
            let Some(tool_name) = tool_name(name, &remote.name) else {
                tracing::warn!(server = %name, tool = %remote.name, "工具名太长，放不进模型的工具表");
                continue;
            };
            if !seen.insert(tool_name.clone()) {
                tracing::warn!(server = %name, tool = %remote.name, %tool_name, "换掉特殊字符之后撞名，跳过");
                continue;
            }
            let read_only = config
                .read_only
                .iter()
                .any(|tool| tool == remote.name.as_ref());
            tools.push(Arc::new(McpTool::new(
                Arc::clone(&server),
                remote,
                tool_name,
                read_only,
            )));
            count += 1;
        }
        tracing::info!(server = %name, tools = count, "MCP 服务器已连上");
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_namespaced_and_cleaned() {
        assert_eq!(
            tool_name("dev-radius", "search.issues").as_deref(),
            Some("mcp__dev-radius__search_issues")
        );
        assert_eq!(tool_name("s", &"x".repeat(80)), None);
    }

    fn tool(name: &str) -> rmcp::model::Tool {
        rmcp::model::Tool::new(
            name.to_string(),
            "d",
            std::sync::Arc::new(JsonObject::new()),
        )
    }

    #[test]
    fn select_keeps_the_allow_list_and_reports_unknown_names() {
        let listed = vec![tool("a"), tool("b")];
        let config = McpServerConfig {
            transport: McpTransport::Http {
                url: "http://x".into(),
                bearer_token_env: None,
            },
            tools: Some(vec!["b".into(), "ghost".into()]),
            read_only: Vec::new(),
        };
        let (kept, missing) = select(&config, &listed);
        assert_eq!(
            kept.iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["b"]
        );
        assert_eq!(missing, vec!["ghost".to_string()]);
    }
}
