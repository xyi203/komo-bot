//! MCP 客户端（`docs/mcp.md`）：连上一个服务器、列出它的工具、调用其中一个。
//!
//! 传输与协议都交给 `rmcp`（官方 Rust SDK），这里只管 komo 自己的三件事：
//!
//! - **凭证只给声明过的人。** stdio 子进程不继承 Gateway 的环境：先清空，只放回跑得起来
//!   所需的几个变量，再加上配置里 `env` 列出的 `.env` 变量。HTTP 的 bearer token 同理；
//!   `oauth = true` 的服务器用数据目录里那份登录凭证（[`crate::mcp_auth`]）。
//! - **工具名进 komo 的命名空间**：`mcp__<server>__<tool>`，与内置工具不会撞名。
//! - **子进程的 stderr 进日志**，不糊在 Gateway 的终端上。
//! - **连接一直在，连没连上是它的状态**（照 pi 的 `McpServerConnection`，`docs/mcp.md` §2.2）：
//!   启动时要登录的服务器记成"等登录"，凭证文件变了就重连、把工具挂进来；断掉的下一次调用时
//!   重连。登录之后不用重启 Gateway。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use komo_kernel::traits::Tool;

use komo_kernel::protocol::config::{McpExposure, McpServerConfig, McpTransport};
use komo_kernel::types::tool::{ToolExposure, ToolNamespace};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::auth::{AuthClient, AuthError};
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, StreamableHttpClientTransportConfig,
};
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
    #[error("还没登录：跑 `komo auth mcp login {0}`（登录完不用重启 Gateway）")]
    NotLoggedIn(String),
    #[error("OAuth：{0}")]
    Auth(String),
    /// 服务器拒绝了凭证（401 / 403，或 token 过期又刷新不了）。
    #[error("服务器要重新登录：{0}")]
    AuthRequired(String),
    #[error("起不来：{0}")]
    Spawn(String),
    #[error("握手失败：{0}")]
    Handshake(String),
    #[error("列不出工具：{0}")]
    ListTools(String),
    #[error("{}s 内没连上", CONNECT_TIMEOUT.as_secs())]
    Timeout,
}

impl McpError {
    /// 要人登录才好得了：重试没有用，等凭证文件变了再连。
    fn needs_login(&self) -> bool {
        matches!(self, McpError::NotLoggedIn(_) | McpError::AuthRequired(_))
    }
}

/// 一次连上的会话。丢掉它就断开（stdio 子进程随之被杀掉）。
pub struct McpServer {
    service: RunningService<RoleClient, ()>,
}

impl McpServer {
    /// 服务循环停了（子进程退出、传输断开）：这个会话不能再用，要重连。
    fn is_closed(&self) -> bool {
        self.service.is_closed() || self.service.peer().is_transport_closed()
    }

    async fn call(
        &self,
        tool: &str,
        arguments: JsonObject,
    ) -> Result<CallToolResult, ServiceError> {
        self.service
            .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
            .await
    }
}

/// 握手交回的东西：会话、工具清单、instructions。
pub struct Connected {
    pub server: McpServer,
    pub tools: Vec<rmcp::model::Tool>,
    pub instructions: Option<String>,
}

/// 连上并列出工具。`data_dir` 只给 `oauth = true` 的服务器找登录凭证。
pub async fn connect(
    name: &str,
    config: &McpServerConfig,
    secrets: &Secrets,
    data_dir: &Path,
) -> Result<Connected, McpError> {
    tokio::time::timeout(
        CONNECT_TIMEOUT,
        connect_inner(name, config, secrets, data_dir),
    )
    .await
    .map_err(|_elapsed| McpError::Timeout)?
}

async fn connect_inner(
    name: &str,
    config: &McpServerConfig,
    secrets: &Secrets,
    data_dir: &Path,
) -> Result<Connected, McpError> {
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
            ().serve(transport).await.map_err(handshake_error)?
        }
        McpTransport::Http {
            url, oauth: true, ..
        } => {
            let path = crate::mcp_auth::credentials_path(data_dir, name);
            let manager = crate::mcp_auth::authorized_manager(url, &path)
                .await
                .map_err(|error| McpError::Auth(error.to_string()))?
                .ok_or_else(|| McpError::NotLoggedIn(name.to_string()))?;
            let client = AuthClient::new(reqwest::Client::new(), manager);
            let transport_config = StreamableHttpClientTransportConfig::with_uri(url.clone());
            ().serve(StreamableHttpClientTransport::with_client(
                client,
                transport_config,
            ))
            .await
            .map_err(handshake_error)?
        }
        McpTransport::Http {
            url,
            bearer_token_env,
            oauth: false,
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
                .map_err(handshake_error)?
        }
    };
    let tools = service.list_all_tools().await.map_err(|error| {
        if auth_required(&error) {
            McpError::AuthRequired(error.to_string())
        } else {
            McpError::ListTools(error.to_string())
        }
    })?;
    let instructions = service
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    Ok(Connected {
        server: McpServer { service },
        tools,
        instructions,
    })
}

fn handshake_error(error: rmcp::service::ClientInitializeError) -> McpError {
    if error.is_authorization_required() {
        McpError::AuthRequired(error.to_string())
    } else {
        McpError::Handshake(error.to_string())
    }
}

/// 服务器拒绝了凭证：401（`AuthRequiredError`），或本地 token 过期又刷新不了
/// （`AuthError::AuthorizationRequired`）。两种都是请求没被服务器处理过。
fn auth_required(error: &ServiceError) -> bool {
    let ServiceError::TransportSend(error) = error else {
        return false;
    };
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error.error.as_ref());
    while let Some(current) = source {
        if current.is::<AuthRequiredError>()
            || matches!(
                current.downcast_ref::<AuthError>(),
                Some(AuthError::AuthorizationRequired)
            )
        {
            return true;
        }
        source = current.source();
    }
    false
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

/// 脚本里这台服务器的名字：`mcp__<server>`，`-` 换成 `_`（与 `tools.<name>` 的写法一致）。
pub fn namespace_name(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

/// `tool_exposure` 里一条都没匹配上服务器工具的键：多半是写错了名字，报出来。
pub fn unmatched_exposure_keys(
    config: &McpServerConfig,
    listed: &[rmcp::model::Tool],
) -> Vec<String> {
    config
        .tool_exposure
        .keys()
        .filter(|key| {
            !listed
                .iter()
                .any(|tool| komo_kernel::protocol::config::glob_matches(key, &tool.name))
        })
        .cloned()
        .collect()
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

/// 一台服务器此刻连没连上（pi 的 `connected / needs-auth / failed / disconnected`；断开不单列，
/// 会话自己报 [`McpServer::is_closed`]）。
enum Link {
    Connected(Arc<McpServer>),
    /// 要人登录。`tokens` 是进入这个状态之前凭证文件的内容：文件变了 = 有人跑过
    /// `komo auth mcp login`，该重连了（pi 的 `tokensAtSignIn`）。只有 `oauth = true` 的服务器
    /// 会到这里——bearer token 在 `.env` 里，改它走配置。
    NeedsAuth {
        tokens: Option<Vec<u8>>,
    },
    /// 连不上，不是登录的问题。下一次用到时再连。
    Failed(String),
}

/// 调用没成的两种情况：确定没到服务器（没连上、凭证被拒），或发出去之后断了——后一种
/// 调用方不知道副作用发生没有。
#[derive(Debug)]
pub enum CallError {
    NotSent(String),
    Sent(String),
}

/// 配置里的一台服务器，Gateway 整个生命周期里都在（pi 的 `McpServerConnection`）。连没连上
/// 是它的状态，不是"有没有这个对象"：启动时要登录的服务器也在，登录之后连上、工具再挂进来。
pub struct McpConnection {
    name: String,
    config: McpServerConfig,
    secrets: Secrets,
    data_dir: PathBuf,
    /// 重连在锁里做：同时到的几次调用只连一次。
    link: tokio::sync::Mutex<Link>,
    /// 系统提示的服务器段与脚本的 `describe_namespace` 用它（`docs/mcp.md` §3）。instructions
    /// 取第一次握手交回的。
    namespace: Mutex<ToolNamespace>,
    /// 最近一次连上时列出、还没交给 [`McpHub::new_tools`] 的工具。交出去就清空：同一次连接的
    /// 告警（名字太长、撞名）只记一次。
    pending: Mutex<Vec<rmcp::model::Tool>>,
}

impl std::fmt::Debug for McpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnection")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl McpConnection {
    fn new(name: &str, config: &McpServerConfig, secrets: &Secrets, data_dir: &Path) -> Self {
        McpConnection {
            name: name.to_string(),
            config: config.clone(),
            secrets: secrets.clone(),
            data_dir: data_dir.to_path_buf(),
            link: tokio::sync::Mutex::new(Link::Failed("还没连过".to_string())),
            namespace: Mutex::new(ToolNamespace {
                name: namespace_name(name),
                server: name.to_string(),
                description: config.description.clone(),
                instructions: None,
            }),
            pending: Mutex::new(Vec::new()),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn namespace(&self) -> ToolNamespace {
        self.namespace.lock().expect("MCP namespace").clone()
    }

    fn is_oauth(&self) -> bool {
        matches!(
            self.config.transport,
            McpTransport::Http { oauth: true, .. }
        )
    }

    /// 凭证文件此刻的内容；不是 OAuth 服务器、或文件不在，就是 `None`。
    fn stored_tokens(&self) -> Option<Vec<u8>> {
        if !self.is_oauth() {
            return None;
        }
        std::fs::read(crate::mcp_auth::credentials_path(
            &self.data_dir,
            &self.name,
        ))
        .ok()
    }

    fn login_hint(&self) -> String {
        format!(
            "MCP 服务器 {name} 要登录：跑 `komo auth mcp login {name}`，登录完下一次调用自动连上，不用重启 Gateway",
            name = self.name
        )
    }

    /// 连一次，结果写进 `link`。调用方握着锁。
    async fn connect_into(&self, link: &mut Link) {
        let tokens = self.stored_tokens();
        *link = match connect(&self.name, &self.config, &self.secrets, &self.data_dir).await {
            Ok(connected) => {
                self.adopt(&connected);
                Link::Connected(Arc::new(connected.server))
            }
            Err(error) if error.needs_login() && self.is_oauth() => {
                tracing::warn!(server = %self.name, %error, "MCP 服务器要登录，登录后下一次用到时自动连上");
                Link::NeedsAuth { tokens }
            }
            Err(error) => {
                tracing::warn!(server = %self.name, %error, "MCP 服务器连不上，下一次用到时再连");
                Link::Failed(error.to_string())
            }
        };
    }

    /// 刚连上：记下 instructions 与工具清单，配置里对不上服务器的名字报出来。
    fn adopt(&self, connected: &Connected) {
        let (selected, missing) = select(&self.config, &connected.tools);
        if !missing.is_empty() {
            tracing::warn!(
                server = %self.name,
                missing = %missing.join("、"),
                "[mcp.servers] tools 里写了服务器没有的工具，它们不算数"
            );
        }
        let unmatched = unmatched_exposure_keys(&self.config, &connected.tools);
        if !unmatched.is_empty() {
            tracing::warn!(
                server = %self.name,
                unmatched = %unmatched.join("、"),
                "[mcp.servers] tool_exposure 里的键没有匹配上服务器的任何工具"
            );
        }
        let count = selected
            .iter()
            .filter(|tool| self.config.exposure_of(&tool.name) != McpExposure::Hidden)
            .count();
        // 只记第一次连上时的：服务器段摘要取它的第一行，重连后换掉会让在跑的会话的系统提示变样。
        let mut namespace = self.namespace.lock().expect("MCP namespace");
        if namespace.instructions.is_none() {
            namespace.instructions = connected.instructions.clone();
        }
        drop(namespace);
        *self.pending.lock().expect("MCP 待挂工具") = connected.tools.clone();
        tracing::info!(server = %self.name, tools = count, "MCP 服务器已连上");
    }

    /// 拿一个能用的会话：连着就直接用；断了、上次没连上、或等登录而凭证文件已经变了，就当场
    /// 重连一次（pi 的 `getClient`）。
    async fn ready(&self) -> Result<Arc<McpServer>, String> {
        let mut link = self.link.lock().await;
        let reconnect = match &*link {
            Link::Connected(server) => server.is_closed(),
            Link::NeedsAuth { tokens } => self.stored_tokens() != *tokens,
            Link::Failed(_) => true,
        };
        if reconnect {
            tracing::info!(server = %self.name, "MCP 服务器重连");
            self.connect_into(&mut link).await;
        }
        match &*link {
            Link::Connected(server) => Ok(Arc::clone(server)),
            Link::NeedsAuth { .. } => Err(self.login_hint()),
            Link::Failed(error) => Err(format!("MCP 服务器 {} 连不上：{error}", self.name)),
        }
    }

    /// 调一次工具。凭证被拒时这台服务器转成等登录、这次调用报错，**不在登录后自动重试**
    /// （照 pi）：要不要再调由模型决定。
    pub async fn call(
        &self,
        tool: &str,
        arguments: JsonObject,
    ) -> Result<CallToolResult, CallError> {
        let server = self.ready().await.map_err(CallError::NotSent)?;
        match server.call(tool, arguments).await {
            Ok(result) => Ok(result),
            Err(error) if auth_required(&error) => {
                if self.is_oauth() {
                    self.lost_auth(&server).await;
                    Err(CallError::NotSent(format!(
                        "服务器拒绝了凭证（{error}）。{}",
                        self.login_hint()
                    )))
                } else {
                    Err(CallError::NotSent(format!("服务器拒绝了凭证：{error}")))
                }
            }
            Err(error) => Err(CallError::Sent(error.to_string())),
        }
    }

    /// 连着的会话被拒了凭证：转成等登录。别的调用已经换过会话了就不动它。
    async fn lost_auth(&self, server: &Arc<McpServer>) {
        let mut link = self.link.lock().await;
        if let Link::Connected(current) = &*link
            && Arc::ptr_eq(current, server)
        {
            tracing::warn!(server = %self.name, "MCP 服务器拒绝了凭证，等重新登录");
            *link = Link::NeedsAuth {
                tokens: self.stored_tokens(),
            };
        }
    }

    /// 等登录、而凭证文件已经变了：重连。
    async fn reconnect_if_signed_in(&self) {
        let mut link = self.link.lock().await;
        let Link::NeedsAuth { tokens } = &*link else {
            return;
        };
        if self.stored_tokens() == *tokens {
            return;
        }
        tracing::info!(server = %self.name, "MCP 凭证文件变了，重连");
        self.connect_into(&mut link).await;
    }
}

/// 全部配置过的 MCP 服务器。Gateway 启动时造一份，一直留着。
#[derive(Debug, Default)]
pub struct McpHub {
    connections: Vec<Arc<McpConnection>>,
    /// 已经挂进工具目录的名字 → 属于哪台服务器。工具挂上之后不撤：执行器的目录只追加。
    registered: Mutex<BTreeMap<String, String>>,
}

impl McpHub {
    /// 启动时并行连全部服务器。连不上的不让 Gateway 起不来，也不让别的服务器的工具跟着
    /// 消失：它留在这里，等登录或下一次用到时再连。
    pub async fn start(
        servers: &BTreeMap<String, McpServerConfig>,
        secrets: &Secrets,
        data_dir: &Path,
    ) -> Self {
        let connections: Vec<Arc<McpConnection>> = servers
            .iter()
            .map(|(name, config)| Arc::new(McpConnection::new(name, config, secrets, data_dir)))
            .collect();
        futures_util::future::join_all(connections.iter().map(|connection| async move {
            let mut link = connection.link.lock().await;
            connection.connect_into(&mut link).await;
        }))
        .await;
        McpHub {
            connections,
            registered: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// 受理 Run 之前问一次（pi 在每个 turn 开始时）：等登录的服务器凭证文件变了就重连。交回
    /// 这次新挂得上的工具。只读一下凭证文件，没人登录时几乎不花时间。
    pub async fn refresh(&self) -> Vec<Arc<dyn Tool>> {
        futures_util::future::join_all(
            self.connections
                .iter()
                .map(|connection| connection.reconnect_if_signed_in()),
        )
        .await;
        self.new_tools()
    }

    /// 连上过的服务器里还没挂的工具（启动时连上的、后来登录连上的、断线重连后新列出来的）。
    /// 名字太长、跨服务器撞名的记 warn 跳过。
    pub fn new_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut registered = self.registered.lock().expect("MCP 工具名表");
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for connection in &self.connections {
            let listed = std::mem::take(&mut *connection.pending.lock().expect("MCP 待挂工具"));
            let name = connection.name();
            let config = &connection.config;
            let (selected, _missing) = select(config, &listed);
            for remote in selected {
                let exposure = match config.exposure_of(&remote.name) {
                    McpExposure::Hidden => continue,
                    McpExposure::Codemode => ToolExposure::Codemode,
                    McpExposure::Direct => ToolExposure::Direct,
                };
                let Some(tool_name) = tool_name(name, &remote.name) else {
                    tracing::warn!(server = %name, tool = %remote.name, "工具名太长，放不进模型的工具表");
                    continue;
                };
                match registered.get(&tool_name) {
                    Some(owner) if owner == name => continue,
                    Some(_) => {
                        tracing::warn!(server = %name, tool = %remote.name, %tool_name, "换掉特殊字符之后撞名，跳过");
                        continue;
                    }
                    None => {}
                }
                registered.insert(tool_name.clone(), name.to_string());
                let read_only = config
                    .read_only
                    .iter()
                    .any(|tool| tool == remote.name.as_ref());
                tools.push(Arc::new(McpTool::new(
                    Arc::clone(connection),
                    remote,
                    tool_name,
                    read_only,
                    exposure,
                )));
            }
        }
        tools
    }
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
                oauth: false,
            },
            tools: Some(vec!["b".into(), "ghost".into()]),
            read_only: Vec::new(),
            description: None,
            exposure: Default::default(),
            tool_exposure: Default::default(),
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
