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
//! - **改 `[mcp.servers]` 热重载即生效**（照 pi 的 `mcp_servers_change`，§2.2）：新加的后台
//!   连上，删掉的撤工具、断开，改了的先删后加。正在执行的那一次调用不被掐断：它握着自己那份
//!   会话，返回之后才真的断开。
//! - **启动不等握手**（2026-10-17）：每台服务器后台连，Gateway 立刻就绪。工具在受理 Run 时
//!   挂上（[`McpHub::wait_for_startup`]）；被接续的 Run 不经过受理，调到还没挂上的工具时先等
//!   那台服务器（[`McpHub::await_tools`]）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use komo_kernel::traits::Tool;

use komo_kernel::protocol::config::{McpExposure, McpServerConfig, McpTransport};
use komo_kernel::protocol::http::{McpServerState, McpServerStatus};
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

/// 连接 + 握手 + 列工具的总时限：卡住的服务器到点算连不上，下一次用到时再连。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 等服务器第一次连上的上限（照 pi 的 `startupWaitMs`，默认 10 秒）：受理 Run 时、被接续的
/// Run 调到还没挂上的工具时都按它等。从这台服务器开始连时算起，不是每条 Run 各等一遍：挂住的
/// 服务器至多拖慢这一段，之后不再等它。
pub const STARTUP_WAIT: Duration = Duration::from_secs(10);

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
    let name = format!("mcp__{}__{}", clean(server), clean(tool));
    (name.len() <= MAX_TOOL_NAME).then_some(name)
}

fn clean(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

/// 这个模型侧工具名照 [`tool_name`] 的规则可能出自这台服务器：`mcp__<server>__` 开头、后面还有
/// 东西。工具还没列出来之前只能这么认。
fn claims(server: &str, tool: &str) -> bool {
    tool.strip_prefix("mcp__")
        .and_then(|rest| rest.strip_prefix(clean(server).as_str()))
        .and_then(|rest| rest.strip_prefix("__"))
        .is_some_and(|rest| !rest.is_empty())
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

/// 一台服务器此刻连没连上（pi 的 `connecting / connected / needs-auth / failed / closed`；断开
/// 不单列，会话自己报 [`McpServer::is_closed`]）。
enum Link {
    /// 热重载刚加进来、后台还没轮到连。先用到它的调用当场连，后台那次就不再连。
    Connecting,
    Connected(Arc<McpServer>),
    /// 要人登录。`tokens` 是进入这个状态之前凭证文件的内容：文件变了 = 有人跑过
    /// `komo auth mcp login`，该重连了（pi 的 `tokensAtSignIn`）。只有 `oauth = true` 的服务器
    /// 会到这里——bearer token 在 `.env` 里，改它走配置。
    NeedsAuth {
        tokens: Option<Vec<u8>>,
    },
    /// 连不上，不是登录的问题。下一次用到时再连。
    Failed(String),
    /// 已从配置移除（[`McpConnection::close`]）。hub 手里的会话放掉了，在途调用握着的那份
    /// 用完才断开。
    Closed,
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
    /// 已从配置移除。不进锁：重连可能握着锁好几秒，移除不该等它。
    closed: AtomicBool,
    /// 第一次连接有了结果（连上、要登录、失败都算）。没有就是"连接中"：受理 Run 时等服务器
    /// 看它（pi 的 `server.ready`）。
    settled: tokio::sync::watch::Sender<bool>,
    /// 开始连的时刻：`startup_wait` 从这里算。
    started: Instant,
    /// 等第一次连接的上限，生产里是 [`STARTUP_WAIT`]（测试改短）。
    startup_wait: Duration,
}

impl std::fmt::Debug for McpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnection")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl McpConnection {
    fn new(
        name: &str,
        config: &McpServerConfig,
        secrets: &Secrets,
        data_dir: &Path,
        startup_wait: Duration,
    ) -> Self {
        McpConnection {
            name: name.to_string(),
            config: config.clone(),
            secrets: secrets.clone(),
            data_dir: data_dir.to_path_buf(),
            link: tokio::sync::Mutex::new(Link::Connecting),
            namespace: Mutex::new(ToolNamespace {
                name: namespace_name(name),
                server: name.to_string(),
                description: config.description.clone(),
                instructions: None,
            }),
            pending: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            settled: tokio::sync::watch::Sender::new(false),
            started: Instant::now(),
            startup_wait,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// 后台连第一次（pi 的 `session_start` 不 await `startConnection`）：启动与热重载都不等
    /// 握手。调用先到、已经连过了，这里就什么也不做。
    fn connect_in_background(self: &Arc<Self>) {
        let connection = Arc::clone(self);
        tokio::spawn(async move {
            let mut link = connection.link.lock().await;
            if matches!(*link, Link::Connecting) {
                connection.connect_into(&mut link).await;
            }
        });
    }

    /// 从配置移除（pi 的 `close`，但**不掐断在途的调用**——komo 多会话并发，没有 pi 那种
    /// "只在空闲时重载"）：之后的调用报"已移除"；hub 手里的会话放掉，在途调用握着自己那份
    /// `Arc<McpServer>`，返回之后最后一个引用释放，stdio 子进程这时才退。
    fn close(self: &Arc<Self>) {
        self.closed.store(true, Ordering::SeqCst);
        let connection = Arc::clone(self);
        tokio::spawn(async move {
            // 正在连的那次连完才拿得到锁；它连完看见 `closed` 也会自己放掉。
            *connection.link.lock().await = Link::Closed;
        });
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn removed_message(&self) -> String {
        format!("MCP 服务器 {} 已从配置移除，它的工具不能再调", self.name)
    }

    /// 有没有挂得上的工具（直出或只在脚本里调，不全是 `hidden`）：受理 Run 时只等这样的服务器。
    ///
    /// pi 只等直出的（`hasDirectTools`），因为它的脚本侧工具调用时才按名字找服务器；komo 的能力面
    /// 在受理时按名字冻结，没挂上的工具这条 Run 从头到尾调不到，所以只在脚本里调的也要等。
    fn has_callable_tools(&self) -> bool {
        self.config.exposure != McpExposure::Hidden
            || self
                .config
                .tool_exposure
                .values()
                .any(|exposure| *exposure != McpExposure::Hidden)
    }

    /// 第一次连接还没有结果、离开始连不到 `startup_wait`：等它，到点就不等了。交回第一次连接
    /// 有没有结果。
    async fn wait_startup(&self) -> bool {
        let Some(left) = self.startup_wait.checked_sub(self.started.elapsed()) else {
            return *self.settled.borrow();
        };
        let mut settled = self.settled.subscribe();
        tokio::time::timeout(left, settled.wait_for(|done| *done))
            .await
            .is_ok()
    }

    /// 被接续的 Run 用到这台服务器、它的工具却还没挂上：等第一次连接有结果（同受理时的上限），
    /// 交回没连上的原因。**不重连**：启动时就连不上的服务器受理 Run 时也不重试（`docs/mcp.md`
    /// §2.2），同一个口径；连上过、后来断了的，调用时 [`Self::ready`] 自己重连。
    async fn settle(&self) -> Result<(), String> {
        if self.is_closed() {
            return Err(self.removed_message());
        }
        if !self.wait_startup().await {
            return Err(format!(
                "MCP 服务器 {} 还没连上（等了 {} 秒），这次调用没有执行；稍后再调",
                self.name,
                self.startup_wait.as_secs()
            ));
        }
        // `settled` 在 `connect_into` 写进结果之前就发了，连接的那一方这时可能还握着锁：在剩下的
        // 窗口里真的拿一次锁，读到的才是这次连接的结果。窗口过了还拿不到，是一次调用在重连。
        let left = self.startup_wait.saturating_sub(self.started.elapsed());
        let link = match tokio::time::timeout(left, self.link.lock()).await {
            Ok(link) => Some(link),
            Err(_) => self.link.try_lock().ok(),
        };
        let reconnecting = || {
            format!(
                "MCP 服务器 {} 正在重连，这次调用没有执行；稍后再调",
                self.name
            )
        };
        let Some(link) = link else {
            return Err(reconnecting());
        };
        match &*link {
            Link::Connected(_) => Ok(()),
            Link::Connecting => Err(reconnecting()),
            Link::NeedsAuth { .. } => Err(self.login_hint()),
            Link::Closed => Err(self.removed_message()),
            Link::Failed(error) => Err(format!(
                "MCP 服务器 {} 连接失败（{error}），这次调用没有执行",
                self.name
            )),
        }
    }

    /// 此刻的状态，不等锁：锁被占着就是正在连（第一次连、或一次调用在重连）。
    pub fn status(&self) -> McpServerStatus {
        let (state, detail) = if !*self.settled.borrow() {
            (McpServerState::Connecting, None)
        } else {
            match self.link.try_lock() {
                Err(_) => (McpServerState::Connecting, None),
                Ok(link) => match &*link {
                    Link::Connecting => (McpServerState::Connecting, None),
                    Link::Connected(_) => (McpServerState::Connected, None),
                    Link::NeedsAuth { .. } => (McpServerState::NeedsAuth, Some(self.login_hint())),
                    Link::Failed(error) => (McpServerState::Failed, Some(error.clone())),
                    Link::Closed => (McpServerState::Failed, Some(self.removed_message())),
                },
            }
        };
        McpServerStatus {
            name: self.name.clone(),
            state,
            detail,
        }
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

    /// 连一次，结果写进 `link`。调用方握着锁。连的时候被移除了，连上的会话当场放掉。
    async fn connect_into(&self, link: &mut Link) {
        let tokens = self.stored_tokens();
        let span = tracing::info_span!(
            "komo.mcp.connect",
            komo.mcp.server = %self.name,
            komo.mcp.outcome = tracing::field::Empty,
        );
        let result = tracing::Instrument::instrument(
            connect(&self.name, &self.config, &self.secrets, &self.data_dir),
            span.clone(),
        )
        .await;
        self.settled.send_replace(true);
        let outcome = match &result {
            Ok(_) => "connected",
            Err(error) if error.needs_login() && self.is_oauth() => "needs_auth",
            Err(_) => "failed",
        };
        span.record("komo.mcp.outcome", outcome);
        if outcome == "failed" {
            tracing::event!(
                target: "komo::metrics",
                tracing::Level::DEBUG,
                {
                monotonic_counter.komo.mcp.connect.failures = 1u64,
                komo.mcp.server = %self.name,
                }
            );
        }
        if self.is_closed() {
            *link = Link::Closed;
            return;
        }
        *link = match result {
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
        if self.is_closed() {
            return Err(self.removed_message());
        }
        let mut link = self.link.lock().await;
        let reconnect = match &*link {
            Link::Connecting | Link::Failed(_) => true,
            Link::Connected(server) => server.is_closed(),
            Link::NeedsAuth { tokens } => self.stored_tokens() != *tokens,
            Link::Closed => false,
        };
        // 等锁的时候被移除了：不再连。
        if self.is_closed() {
            return Err(self.removed_message());
        }
        if reconnect {
            tracing::info!(server = %self.name, "MCP 服务器重连");
            self.connect_into(&mut link).await;
        }
        match &*link {
            Link::Connected(server) => Ok(Arc::clone(server)),
            Link::NeedsAuth { .. } => Err(self.login_hint()),
            Link::Failed(error) => Err(format!("MCP 服务器 {} 连不上：{error}", self.name)),
            Link::Closed => Err(self.removed_message()),
            // `connect_into` 总会写一个结果，走不到这里；真走到了也只是这次调用报错。
            Link::Connecting => Err(format!("MCP 服务器 {} 还在连", self.name)),
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
    ///
    /// 锁被占着就是正在连（后台第一次连、或一次调用在重连），不是"等登录"：不等它，下一条 Run
    /// 受理时再比。等的话，一台握手卡住的服务器会把受理拖到 [`CONNECT_TIMEOUT`]。
    async fn reconnect_if_signed_in(&self) {
        let Ok(mut link) = self.link.try_lock() else {
            return;
        };
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

/// 一次 [`McpHub::reconcile`] 做了什么。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct McpChange {
    /// 新连的服务器（新增的、改了配置重连的）。
    pub added: Vec<String>,
    /// 断开的服务器（删掉的、改了配置先删的）。
    pub removed: Vec<String>,
    /// 要从执行器目录撤掉的工具：名字 → 交给还在调它的 Run 的说明（pi 的 `hideTools`）。
    pub withdrawn: Vec<(String, String)>,
}

/// 全部配置过的 MCP 服务器。Gateway 启动时造一份，一直留着；热重载时按配置增删
/// （[`Self::reconcile`]）。
#[derive(Debug)]
pub struct McpHub {
    /// 名字 → 连接。
    connections: Mutex<BTreeMap<String, Arc<McpConnection>>>,
    /// 已经挂进工具目录的名字 → 属于哪台服务器。服务器移除时它的那些一起撤
    /// （[`McpChange::withdrawn`]）。
    registered: Mutex<BTreeMap<String, String>>,
    /// 热重载新加的服务器用它找登录凭证。
    data_dir: PathBuf,
    /// 等第一次连接的上限，生产里是 [`STARTUP_WAIT`]。
    startup_wait: Duration,
}

impl Default for McpHub {
    fn default() -> Self {
        McpHub {
            connections: Mutex::default(),
            registered: Mutex::default(),
            data_dir: PathBuf::new(),
            startup_wait: STARTUP_WAIT,
        }
    }
}

impl McpHub {
    /// 启动时每台服务器各自后台连，**不等握手**：一台卡住的服务器不拖住 Gateway 就绪。连不上
    /// 的也不让别的服务器的工具跟着消失：它留在这里，等登录或下一次用到时再连。工具在受理 Run
    /// 时挂上（[`Self::wait_for_startup`] + [`Self::refresh`]）。
    pub fn start(
        servers: &BTreeMap<String, McpServerConfig>,
        secrets: &Secrets,
        data_dir: &Path,
    ) -> Self {
        Self::start_waiting(servers, secrets, data_dir, STARTUP_WAIT)
    }

    /// 同 [`Self::start`]，等第一次连接的上限自己给（测试用短的）。
    pub fn start_waiting(
        servers: &BTreeMap<String, McpServerConfig>,
        secrets: &Secrets,
        data_dir: &Path,
        startup_wait: Duration,
    ) -> Self {
        let connections: BTreeMap<String, Arc<McpConnection>> = servers
            .iter()
            .map(|(name, config)| {
                let connection = Arc::new(McpConnection::new(
                    name,
                    config,
                    secrets,
                    data_dir,
                    startup_wait,
                ));
                connection.connect_in_background();
                (name.clone(), connection)
            })
            .collect();
        McpHub {
            connections: Mutex::new(connections),
            registered: Mutex::new(BTreeMap::new()),
            data_dir: data_dir.to_path_buf(),
            startup_wait,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.connections.lock().expect("MCP 连接表").is_empty()
    }

    /// 此刻的连接，按名字排。拿的是快照：之后 [`Self::reconcile`] 换掉的不影响它。
    fn connections(&self) -> Vec<Arc<McpConnection>> {
        self.connections
            .lock()
            .expect("MCP 连接表")
            .values()
            .cloned()
            .collect()
    }

    /// 每台服务器此刻的状态，不等握手。
    pub fn statuses(&self) -> Vec<McpServerStatus> {
        self.connections()
            .iter()
            .map(|connection| connection.status())
            .collect()
    }

    /// 热重载换上了新的 `[mcp.servers]`（照 pi 的 `mcp_servers_change`）：
    ///
    /// - 配置里没了的、配置变了的（`McpServerConfig` 不相等）：从表里拿掉、断开，工具撤掉；
    /// - 表里没有的（新增的，和上一步拿掉的改过的）：建连接，**后台**连，不等握手。连上之后
    ///   工具在下一次 [`Self::refresh`] 里交出去。
    ///
    /// 配置一样的服务器原样留着，连接不动。正在执行的调用不受影响（[`McpConnection::close`]）。
    pub fn reconcile(
        &self,
        servers: &BTreeMap<String, McpServerConfig>,
        secrets: &Secrets,
    ) -> McpChange {
        let mut connections = self.connections.lock().expect("MCP 连接表");
        // 与 `new_tools` 同一把锁：一台刚移除的服务器不会在撤完之后又把工具挂回去。
        let mut registered = self.registered.lock().expect("MCP 工具名表");
        let mut change = McpChange::default();
        let stale: Vec<String> = connections
            .iter()
            .filter(|(name, connection)| servers.get(*name) != Some(&connection.config))
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale {
            let Some(connection) = connections.remove(&name) else {
                continue;
            };
            connection.close();
            let reason = if servers.contains_key(&name) {
                format!("MCP 服务器 {name} 的配置改了，正在重连；连上后从下一条 Run 起可用")
            } else {
                connection.removed_message()
            };
            registered.retain(|tool, owner| {
                if *owner == name {
                    change.withdrawn.push((tool.clone(), reason.clone()));
                    false
                } else {
                    true
                }
            });
            tracing::info!(server = %name, "MCP 服务器已断开（配置删了或改了）");
            change.removed.push(name);
        }
        for (name, config) in servers {
            if connections.contains_key(name) {
                continue;
            }
            let connection = Arc::new(McpConnection::new(
                name,
                config,
                secrets,
                &self.data_dir,
                self.startup_wait,
            ));
            connection.connect_in_background();
            connections.insert(name.clone(), connection);
            tracing::info!(server = %name, "MCP 服务器加进来了，后台连接");
            change.added.push(name.clone());
        }
        change
    }

    /// 受理 Run 时先等还在连的服务器（照 pi 的 `waitForDirectServers`）：只等有工具挂得上、
    /// 而且第一次连接还没结果的那些，至多等到开始连之后 [`STARTUP_WAIT`]。到点没连上的这条
    /// Run 先不等它，连上后工具从下一条 Run 起可用。
    pub async fn wait_for_startup(&self) {
        let pending: Vec<Arc<McpConnection>> = self
            .connections()
            .into_iter()
            .filter(|connection| connection.has_callable_tools())
            .collect();
        let settled = futures_util::future::join_all(
            pending.iter().map(|connection| connection.wait_startup()),
        )
        .await;
        for (connection, settled) in pending.iter().zip(settled) {
            if !settled {
                tracing::info!(server = %connection.name, "MCP 服务器还在连，这条 Run 先不等它，连上后工具从下一条 Run 起可用");
            }
        }
    }

    /// 被接续的 Run 不经过受理（`docs/mcp.md` §2.2）：它冻结的能力面里有 `names`，执行器目录里
    /// 却还没有——Gateway 刚重启、服务器还在连。按 [`tool_name`] 的命名规则认出这些名字可能
    /// 出自哪几台服务器，等它们第一次连接有结果（[`McpConnection::settle`]），交回这次挂得上
    /// 的工具，以及仍然没有着落的名字 → 原因。
    ///
    /// 认不出服务器的名字（不是 MCP 的、服务器已从配置删掉的）不在交回的原因里：调用方照常报
    /// "没有这个工具"。认出来的服务器连上了、却没列出这个工具的，同理。
    pub async fn await_tools(
        &self,
        names: &[String],
    ) -> (Vec<Arc<dyn Tool>>, BTreeMap<String, String>) {
        let connections = self.connections();
        let claimed: Vec<(&String, Vec<&Arc<McpConnection>>)> = names
            .iter()
            .map(|name| {
                let owners = connections
                    .iter()
                    .filter(|connection| claims(connection.name(), name))
                    .collect();
                (name, owners)
            })
            .filter(|(_, owners): &(_, Vec<_>)| !owners.is_empty())
            .collect();
        if claimed.is_empty() {
            return (Vec::new(), BTreeMap::new());
        }
        // 一台服务器只等一次，几台一起等。
        let waiting: BTreeMap<&str, &Arc<McpConnection>> = claimed
            .iter()
            .flat_map(|(_, owners)| owners.iter().map(|owner| (owner.name(), *owner)))
            .collect();
        let outcomes: BTreeMap<&str, Result<(), String>> = waiting
            .keys()
            .copied()
            .zip(
                futures_util::future::join_all(
                    waiting.values().map(|connection| connection.settle()),
                )
                .await,
            )
            .collect();
        let tools = self.new_tools();
        let registered = self.registered.lock().expect("MCP 工具名表");
        let mut unresolved = BTreeMap::new();
        for (name, owners) in claimed {
            if registered.contains_key(name.as_str()) {
                continue;
            }
            let reasons: Vec<&str> = owners
                .iter()
                .filter_map(|owner| outcomes.get(owner.name())?.as_ref().err())
                .map(String::as_str)
                .collect();
            if !reasons.is_empty() {
                unresolved.insert(name.clone(), reasons.join("；"));
            }
        }
        (tools, unresolved)
    }

    /// 受理 Run 之前问一次（pi 在每个 turn 开始时）：等登录的服务器凭证文件变了就重连。交回
    /// 这次新挂得上的工具。只读一下凭证文件，没人登录时几乎不花时间。
    pub async fn refresh(&self) -> Vec<Arc<dyn Tool>> {
        futures_util::future::join_all(
            self.connections()
                .iter()
                .map(|connection| connection.reconnect_if_signed_in()),
        )
        .await;
        self.new_tools()
    }

    /// 连上过的服务器里还没挂的工具（启动时连上的、后来登录连上的、断线重连后新列出来的、
    /// 热重载新加的）。名字太长、跨服务器撞名的记 warn 跳过。
    pub fn new_tools(&self) -> Vec<Arc<dyn Tool>> {
        let connections = self.connections();
        let mut registered = self.registered.lock().expect("MCP 工具名表");
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for connection in &connections {
            // 拿完快照之后被移除了：它的工具不挂。
            if connection.is_closed() {
                continue;
            }
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

    /// 热重载测试用的 stdio 服务器：`echo` 立刻回；`slow` 先写 `<prefix>.started`，等到
    /// `<prefix>.release` 出现才回——拿它钉"在途的调用不被掐断"。pid 写进 `<prefix>.pid`。
    const SERVER: &str = r#"
import json, os, sys, time
PREFIX = sys.argv[1]
open(PREFIX + ".pid", "w").write(str(os.getpid()))
TOOLS = [
    {"name": "echo", "inputSchema": {"type": "object", "properties": {}}},
    {"name": "slow", "inputSchema": {"type": "object", "properties": {}}},
]
def reply(msg_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": result}) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    method = msg.get("method")
    if method == "initialize":
        reply(msg["id"], {"protocolVersion": msg["params"]["protocolVersion"],
                          "capabilities": {"tools": {}},
                          "serverInfo": {"name": "t", "version": "0"}})
    elif method == "tools/list":
        reply(msg["id"], {"tools": TOOLS})
    elif method == "tools/call":
        if msg["params"]["name"] == "slow":
            open(PREFIX + ".started", "w").close()
            for _ in range(1500):
                if os.path.exists(PREFIX + ".release"):
                    break
                time.sleep(0.02)
            text = "slow done"
        else:
            text = "echoed"
        reply(msg["id"], {"content": [{"type": "text", "text": text}]})
    else:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg["id"],
                                     "error": {"code": -32601, "message": "no"}}) + "\n")
        sys.stdout.flush()
"#;

    struct Fixture {
        dir: tempfile::TempDir,
        script: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("临时目录");
            let script = dir.path().join("server.py");
            std::fs::write(&script, SERVER).unwrap();
            Fixture { dir, script }
        }

        /// 一台服务器的配置；`tag` 区分进程（pid 与标记文件的前缀）。
        fn server(&self, tag: &str, read_only: &[&str]) -> McpServerConfig {
            McpServerConfig {
                transport: McpTransport::Stdio {
                    command: "python3".into(),
                    args: vec![
                        self.script.display().to_string(),
                        self.prefix(tag).display().to_string(),
                    ],
                    env: vec![],
                },
                tools: None,
                read_only: read_only.iter().map(|tool| tool.to_string()).collect(),
                description: None,
                exposure: McpExposure::Codemode,
                tool_exposure: BTreeMap::new(),
            }
        }

        fn prefix(&self, tag: &str) -> PathBuf {
            self.dir.path().join(tag)
        }

        /// 起一个 hub，等第一次连接有结果（与受理 Run 时同一个等法）。
        async fn hub(&self, servers: &[(&str, McpServerConfig)]) -> McpHub {
            let hub = McpHub::start(&servers_of(servers), &Secrets::new(), self.dir.path());
            hub.wait_for_startup().await;
            hub
        }

        /// 握手永远不回的服务器：进程起着，什么也不答。
        fn stuck(&self) -> McpServerConfig {
            let mut config = self.server("stuck", &[]);
            config.transport = McpTransport::Stdio {
                command: "python3".into(),
                args: vec!["-c".into(), "import time; time.sleep(40)".into()],
                env: vec![],
            };
            config
        }

        /// 起不来的服务器：命令不存在。
        fn broken(&self) -> McpServerConfig {
            let mut config = self.server("broken", &[]);
            config.transport = McpTransport::Stdio {
                command: self.dir.path().join("no-such-server").display().to_string(),
                args: vec![],
                env: vec![],
            };
            config
        }

        /// 标记文件出现为止（至多 20 秒）。
        async fn wait_file(&self, name: &str) {
            let path = self.dir.path().join(name);
            for _ in 0..1000 {
                if path.exists() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("等不到 {}", path.display());
        }

        async fn pid(&self, tag: &str) -> u32 {
            self.wait_file(&format!("{tag}.pid")).await;
            std::fs::read_to_string(self.prefix(tag).with_extension("pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        }
    }

    fn servers_of(servers: &[(&str, McpServerConfig)]) -> BTreeMap<String, McpServerConfig> {
        servers
            .iter()
            .map(|(name, config)| (name.to_string(), config.clone()))
            .collect()
    }

    /// 进程还在（僵尸不算）。Linux 读 `/proc/<pid>/stat`，其他平台（macOS 没有 `/proc`）
    /// 问 `ps -o stat=`，与 `recovery::processes::SysProcessProbe` 同一个分法。
    fn alive(pid: u32) -> bool {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit(')')
                    .next()
                    .is_some_and(|rest| !rest.trim_start().starts_with('Z'))
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .is_ok_and(|output| {
                    let stat = String::from_utf8_lossy(&output.stdout);
                    output.status.success()
                        && !stat.trim().is_empty()
                        && !stat.trim().starts_with('Z')
                })
        }
    }

    async fn wait_exit(pid: u32) {
        for _ in 0..500 {
            if !alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("子进程 {pid} 没退");
    }

    /// 新加的服务器连上、工具交出来为止（至多 20 秒）。
    async fn wait_tools(hub: &McpHub) -> Vec<String> {
        for _ in 0..1000 {
            let tools = hub.new_tools();
            if !tools.is_empty() {
                return tools.iter().map(|tool| tool.definition().name).collect();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("等不到工具");
    }

    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                rmcp::model::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    fn withdrawn_names(change: &McpChange) -> Vec<&str> {
        change
            .withdrawn
            .iter()
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// 新增：建连接、后台连，reconcile 不等握手；连上后工具从 `new_tools` 交出去。配置没变的
    /// 服务器原样留着。
    #[tokio::test]
    async fn reconcile_adds_a_server_in_the_background() {
        let fixture = Fixture::new();
        let hub = fixture.hub(&[]).await;
        let servers = servers_of(&[("demo", fixture.server("a", &[]))]);

        let change = hub.reconcile(&servers, &Secrets::new());
        assert_eq!(change.added, vec!["demo".to_string()]);
        assert!(change.removed.is_empty() && change.withdrawn.is_empty());
        // 不等握手：交回时还在连。
        let statuses = hub.statuses();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].state, McpServerState::Connecting);

        assert_eq!(
            wait_tools(&hub).await,
            vec!["mcp__demo__echo", "mcp__demo__slow"]
        );
        assert_eq!(hub.statuses()[0].state, McpServerState::Connected);

        let before = Arc::clone(&hub.connections()[0]);
        assert_eq!(
            hub.reconcile(&servers, &Secrets::new()),
            McpChange::default()
        );
        assert!(
            Arc::ptr_eq(&before, &hub.connections()[0]),
            "配置没变不该重连"
        );
    }

    /// 每次连接一个 `komo.mcp.connect` span 带服务器名与结果；连不上才记失败计数（维度只有服务器名）。
    #[tokio::test]
    async fn each_connect_has_a_span_and_only_failures_are_counted() {
        let capture = crate::telemetry::capture::Capture::default();
        let _guard = capture.install();
        let fixture = Fixture::new();
        let mut broken = fixture.server("b", &[]);
        broken.transport = McpTransport::Stdio {
            command: "komo-no-such-binary".into(),
            args: vec![],
            env: vec![],
        };
        let hub = fixture
            .hub(&[("good", fixture.server("a", &[])), ("bad", broken)])
            .await;
        drop(hub);

        let outcome = |server: &str| {
            capture
                .spans("komo.mcp.connect")
                .into_iter()
                .find(|span| span.fields["komo.mcp.server"] == server)
                .map(|span| span.fields["komo.mcp.outcome"].clone())
        };
        assert_eq!(outcome("good").as_deref(), Some("connected"));
        assert_eq!(outcome("bad").as_deref(), Some("failed"));
        let failures = capture.metrics("monotonic_counter.komo.mcp.connect.failures");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].fields["komo.mcp.server"], "bad");
        assert_eq!(failures[0].fields.len(), 2);
    }

    /// 删除：从 hub 拿掉、工具撤掉（带原因）、连接断开；之后再调报"已从配置移除"。
    #[tokio::test]
    async fn reconcile_removes_a_server_and_withdraws_its_tools() {
        let fixture = Fixture::new();
        let hub = fixture.hub(&[("demo", fixture.server("a", &[]))]).await;
        assert_eq!(hub.new_tools().len(), 2);
        let pid = fixture.pid("a").await;
        let connection = Arc::clone(&hub.connections()[0]);

        let change = hub.reconcile(&BTreeMap::new(), &Secrets::new());
        assert_eq!(change.removed, vec!["demo".to_string()]);
        assert!(change.added.is_empty());
        assert_eq!(
            withdrawn_names(&change),
            vec!["mcp__demo__echo", "mcp__demo__slow"]
        );
        assert!(
            change
                .withdrawn
                .iter()
                .all(|(_, reason)| reason.contains("MCP 服务器 demo 已从配置移除")),
            "{change:?}"
        );
        assert!(hub.is_empty());
        assert!(hub.new_tools().is_empty());

        match connection.call("echo", JsonObject::new()).await {
            Err(CallError::NotSent(message)) => {
                assert!(message.contains("已从配置移除"), "{message}")
            }
            other => panic!("移除之后不该调得通：{other:?}"),
        }
        wait_exit(pid).await;
    }

    /// 修改：先删后加（照 pi）——旧连接断开、工具撤掉，新连接用新配置后台连上，工具再交出来。
    #[tokio::test]
    async fn reconcile_replaces_a_changed_server() {
        let fixture = Fixture::new();
        let hub = fixture.hub(&[("demo", fixture.server("a", &[]))]).await;
        assert_eq!(hub.new_tools().len(), 2);
        let old_pid = fixture.pid("a").await;
        let old = Arc::clone(&hub.connections()[0]);

        let servers = servers_of(&[("demo", fixture.server("b", &["echo"]))]);
        let change = hub.reconcile(&servers, &Secrets::new());
        assert_eq!(change.removed, vec!["demo".to_string()]);
        assert_eq!(change.added, vec!["demo".to_string()]);
        assert_eq!(
            withdrawn_names(&change),
            vec!["mcp__demo__echo", "mcp__demo__slow"]
        );
        assert!(
            change
                .withdrawn
                .iter()
                .all(|(_, reason)| reason.contains("配置改了")),
            "{change:?}"
        );
        assert!(!Arc::ptr_eq(&old, &hub.connections()[0]));

        assert_eq!(
            wait_tools(&hub).await,
            vec!["mcp__demo__echo", "mcp__demo__slow"]
        );
        let new_pid = fixture.pid("b").await;
        wait_exit(old_pid).await;
        assert!(alive(new_pid));
    }

    /// 在途调用：删除服务器时正在执行的那一次照常返回；子进程活到它返回之后才退。
    #[tokio::test]
    async fn removing_a_server_lets_the_call_in_flight_finish() {
        let fixture = Fixture::new();
        let hub = fixture.hub(&[("demo", fixture.server("a", &[]))]).await;
        let pid = fixture.pid("a").await;
        let connection = Arc::clone(&hub.connections()[0]);
        let call = tokio::spawn(async move { connection.call("slow", JsonObject::new()).await });
        fixture.wait_file("a.started").await;

        hub.reconcile(&BTreeMap::new(), &Secrets::new());
        // 让 `close` 的后台任务跑完：hub 那份会话已经放掉。
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(alive(pid), "在途调用还没返回，子进程不该被杀");

        std::fs::write(fixture.prefix("a").with_extension("release"), "").unwrap();
        let result = call.await.unwrap().expect("在途调用照常返回");
        assert_eq!(text_of(&result), "slow done");
        wait_exit(pid).await;
    }

    /// 在途调用：改了配置（先删后加）时旧连接上正在执行的那一次照常返回，新连接照样连上。
    #[tokio::test]
    async fn changing_a_server_lets_the_call_in_flight_finish() {
        let fixture = Fixture::new();
        let hub = fixture.hub(&[("demo", fixture.server("a", &[]))]).await;
        let old_pid = fixture.pid("a").await;
        let connection = Arc::clone(&hub.connections()[0]);
        let call = tokio::spawn(async move { connection.call("slow", JsonObject::new()).await });
        fixture.wait_file("a.started").await;

        let servers = servers_of(&[("demo", fixture.server("b", &["echo"]))]);
        hub.reconcile(&servers, &Secrets::new());
        assert_eq!(wait_tools(&hub).await.len(), 2);
        assert!(alive(old_pid), "在途调用还没返回，旧子进程不该被杀");

        std::fs::write(fixture.prefix("a").with_extension("release"), "").unwrap();
        let result = call.await.unwrap().expect("在途调用照常返回");
        assert_eq!(text_of(&result), "slow done");
        wait_exit(old_pid).await;
        let fresh = Arc::clone(&hub.connections()[0]);
        assert!(fresh.call("echo", JsonObject::new()).await.is_ok());
    }

    /// 工具还没列出来之前按命名规则认服务器：名字里的特殊字符照 [`tool_name`] 换掉再比。
    #[test]
    fn a_tool_name_is_claimed_by_its_server_prefix() {
        assert!(claims("dev-radius", "mcp__dev-radius__search"));
        assert!(claims("a.b", "mcp__a_b__x"));
        assert!(claims("a", "mcp__a__b__c"));
        assert!(!claims("a", "mcp__ab__x"));
        assert!(!claims("a", "mcp__a__"));
        assert!(!claims("a", "read"));
    }

    /// 启动不等握手：一台握手永远不回的服务器不拖住 `start`，另一台照样后台连上。
    #[tokio::test]
    async fn start_returns_without_waiting_for_a_stuck_server() {
        let fixture = Fixture::new();
        let begun = Instant::now();
        let hub = McpHub::start_waiting(
            &servers_of(&[
                ("demo", fixture.server("a", &[])),
                ("stuck", fixture.stuck()),
            ]),
            &Secrets::new(),
            fixture.dir.path(),
            Duration::from_secs(1),
        );
        assert!(
            begun.elapsed() < Duration::from_millis(500),
            "{:?}",
            begun.elapsed()
        );
        assert!(
            hub.statuses()
                .iter()
                .all(|status| status.state == McpServerState::Connecting)
        );

        // 受理 Run 时的等待到点就放手，连上的那台工具交得出来。
        hub.wait_for_startup().await;
        assert!(
            begun.elapsed() < Duration::from_secs(5),
            "{:?}",
            begun.elapsed()
        );
        assert_eq!(
            hub.new_tools()
                .iter()
                .map(|tool| tool.definition().name)
                .collect::<Vec<_>>(),
            vec!["mcp__demo__echo", "mcp__demo__slow"]
        );
        let stuck = hub
            .statuses()
            .into_iter()
            .find(|status| status.name == "stuck")
            .unwrap();
        assert_eq!(stuck.state, McpServerState::Connecting);
    }

    /// 被接续的 Run 用的那条路：按名字认出服务器、等它，连上的工具交回来；还在连的、连不上的
    /// 报原因；认不出服务器的名字不报（调用方照常说"没有这个工具"）。
    #[tokio::test]
    async fn await_tools_waits_for_the_owner_and_explains_the_rest() {
        let fixture = Fixture::new();
        let hub = McpHub::start_waiting(
            &servers_of(&[
                ("demo", fixture.server("a", &[])),
                ("stuck", fixture.stuck()),
                ("broken", fixture.broken()),
            ]),
            &Secrets::new(),
            fixture.dir.path(),
            Duration::from_secs(1),
        );
        let names: Vec<String> = [
            "mcp__demo__echo",
            "mcp__demo__nope",
            "mcp__stuck__x",
            "mcp__broken__x",
            "mcp__ghost__x",
            "read",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let (tools, unresolved) = hub.await_tools(&names).await;
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.definition().name)
                .collect::<Vec<_>>(),
            vec!["mcp__demo__echo", "mcp__demo__slow"]
        );
        assert_eq!(
            unresolved.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["mcp__broken__x", "mcp__stuck__x"]
        );
        assert!(
            unresolved["mcp__stuck__x"].contains("MCP 服务器 stuck 还没连上"),
            "{unresolved:?}"
        );
        assert!(
            unresolved["mcp__broken__x"].contains("MCP 服务器 broken 连接失败"),
            "{unresolved:?}"
        );

        // 第二次不再等：工具已经挂过，还在连的那台到点了直接报。
        let begun = Instant::now();
        let (tools, unresolved) = hub.await_tools(&names).await;
        assert!(tools.is_empty());
        assert_eq!(unresolved.len(), 2);
        assert!(
            begun.elapsed() < Duration::from_millis(500),
            "{:?}",
            begun.elapsed()
        );
    }

    /// 执行器的顶层调用：能力面里有、目录里还没有的 MCP 工具，先等服务器连上再调通；连不上的
    /// 报那台服务器为什么连不上，不是"没有名为…的工具"。
    #[tokio::test]
    async fn a_call_to_a_tool_not_yet_mounted_waits_for_its_server() {
        use crate::executor::ToolExecutor;
        use crate::executor::harness::Harness;

        let fixture = Fixture::new();
        let hub = Arc::new(McpHub::start_waiting(
            &servers_of(&[
                ("demo", fixture.server("a", &[])),
                ("broken", fixture.broken()),
            ]),
            &Secrets::new(),
            fixture.dir.path(),
            Duration::from_secs(5),
        ));
        let harness = Harness::new();
        let executor = ToolExecutor::new(
            vec![],
            harness.ledger.clone(),
            harness.outputs.clone(),
            Arc::new(harness.clock.clone()),
        )
        .with_mcp(Arc::clone(&hub));
        assert!(executor.tool_names().is_empty());
        let (session, run) = harness.open_run().await;
        let mut env = harness.env(&session, &run);
        env.surface =
            komo_kernel::types::surface::AgentSurface::new(["mcp__demo__echo", "mcp__broken__x"]);

        let calls = harness
            .record_round(
                &run,
                &[
                    ("mcp__demo__echo", serde_json::json!({})),
                    ("mcp__broken__x", serde_json::json!({})),
                ],
            )
            .await;
        let outcome = executor.execute_round(calls, &env).await.unwrap();
        assert!(outcome.stop.is_none());
        let echoed = &outcome.results[0];
        assert!(!echoed.is_error, "{}", echoed.content);
        assert!(echoed.content.contains("echoed"), "{}", echoed.content);
        let broken = &outcome.results[1];
        assert!(broken.is_error);
        assert!(
            broken.content.contains("MCP 服务器 broken 连接失败"),
            "{}",
            broken.content
        );
        assert!(
            executor
                .tool_names()
                .contains(&"mcp__demo__echo".to_string())
        );
    }
}
