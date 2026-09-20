//! §13.1 接口的 HTTP 客户端与幂等请求键。
//!
//! 一个接口一个方法，请求与响应**就是** `komo_kernel::protocol::http` 里的类型——这样
//! 客户端与 Gateway 不可能各自演化出一个形状。
//!
//! **幂等请求键由调用方给。**提交输入、审批、Cron 与 Memory 变更的请求类型里都有
//! `request_key` 字段（§13.1），这里不代生成：客户端重发同一次操作要带同一个键，而
//! "同一次操作"是调用方才知道的事。[`RequestKeys`] 是给调用方用的构造器，不是默认值。

use komo_kernel::protocol::http::*;
use komo_kernel::types::ids::{CronJobId, MemoryId, RequestKey, RunId, SessionId, uuid_v7_at};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{ClientError, ClientResult};

/// 一个已解析的 Gateway 地址与它的认证令牌。
#[derive(Clone)]
pub struct KomoClient {
    http: reqwest::Client,
    /// 发现文件所在的数据目录。`None` = 地址与令牌是**直接给的**（测试替身、或者调用方
    /// 自己知道连哪），没有可刷新之处。
    home: Option<std::path::PathBuf>,
    /// 连接的那一半。**可换**：网关重启会换令牌（发现文件里那个每次启动重新生成），
    /// 有时还换端口——`refresh()` 按发现文件把它换掉，见那里的注释。
    endpoint: std::sync::Arc<std::sync::RwLock<Endpoint>>,
}

/// 一次连接要的全部东西：地址、令牌、以及**它是哪个实例**。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    base: String,
    token: Option<String>,
    instance: Option<String>,
}

impl Endpoint {
    fn of(base_url: &str, token: Option<String>, instance: Option<String>) -> Endpoint {
        Endpoint {
            base: base_url.trim().trim_end_matches('/').to_string(),
            token,
            instance,
        }
    }
}

impl std::fmt::Debug for KomoClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 令牌不进日志。
        let endpoint = self.endpoint();
        f.debug_struct("KomoClient")
            .field("base", &endpoint.base)
            .field("token", &endpoint.token.as_ref().map(|_| "<set>"))
            .finish()
    }
}

/// 这次失败像不像"网关换了实例"：401（令牌不对）或者根本连不上。
///
/// 401 有两种形状：带统一错误体的 [`ClientError::Api`]（服务端照 §13.1 答的）与裸的
/// [`ClientError::Http`]（中间有代理、或者服务端没按格式答）。两种都算。
fn is_stale_connection(error: &ClientError) -> bool {
    match error {
        ClientError::Transport(_) => true,
        ClientError::Api { status, .. } | ClientError::Http { status, .. } => *status == 401,
        _ => false,
    }
}

impl KomoClient {
    pub fn new(base_url: &str, token: Option<String>) -> ClientResult<Self> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        Self::with_http(http, base_url, token)
    }

    /// 用一个已经建好的 `reqwest::Client`（复用连接池、或测试里换超时）。
    pub fn with_http(
        http: reqwest::Client,
        base_url: &str,
        token: Option<String>,
    ) -> ClientResult<Self> {
        let base = base_url.trim().trim_end_matches('/').to_string();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            return Err(ClientError::BadUrl(format!(
                "地址要以 http:// 或 https:// 开头，收到 {base_url}"
            )));
        }
        Ok(KomoClient {
            http,
            home: None,
            endpoint: std::sync::Arc::new(std::sync::RwLock::new(Endpoint::of(
                base_url, token, None,
            ))),
        })
    }

    /// 同 [`KomoClient::with_http`]，但记住数据目录：这样 [`KomoClient::refresh`] 能按
    /// 发现文件核对当前实例（§3 第 1–2 步）。
    pub fn with_home(
        http: reqwest::Client,
        base_url: &str,
        token: Option<String>,
        home: impl Into<std::path::PathBuf>,
        instance: Option<String>,
    ) -> ClientResult<Self> {
        let mut client = Self::with_http(http, base_url, token)?;
        let current = client.endpoint().token;
        *client.endpoint.write().expect("连接信息") = Endpoint::of(base_url, current, instance);
        client.home = Some(home.into());
        Ok(client)
    }

    /// 按发现文件核对当前实例：**实例换了（重启会换令牌）就把地址与令牌一起换掉**。
    ///
    /// 返回是否真的换了。发现文件读不出来、或者那个实例这会儿连不上，就是"没换"——错误
    /// 由调用方照常处理，刷新不是错误路径。
    ///
    /// 这条存在的理由是一次真实的故障：网关重启之后，客户端手里还攥着**上一个实例的
    /// 令牌**，于是每一次请求都 401、SSE 永远重连不上，界面停在"重连中"直到人手动重开。
    /// 重启本来是最常见的操作（改配置、换二进制），客户端必须能自己跟过去。
    pub async fn refresh(&self) -> bool {
        let Some(home) = self.home.as_ref() else {
            return false;
        };
        let Ok(found) = crate::discovery::discover(home).await else {
            return false;
        };
        let next = Endpoint::of(
            &found.discovery.base_url,
            found.discovery.token.clone(),
            Some(found.discovery.instance_id.clone()),
        );
        {
            let mut endpoint = self.endpoint.write().expect("连接信息");
            if *endpoint == next {
                return false;
            }
            *endpoint = next;
        }
        true
    }

    pub fn base_url(&self) -> String {
        self.endpoint().base
    }

    pub(crate) fn token(&self) -> Option<String> {
        self.endpoint().token
    }

    /// 当前连接信息的一份拷贝。**每次用之前现取**：`refresh()` 可能刚换过它。
    fn endpoint(&self) -> Endpoint {
        self.endpoint.read().expect("连接信息").clone()
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.endpoint().base)
    }

    fn authed(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.endpoint().token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    /// 请求。**401 / 传输失败时按发现文件刷新一次，然后用新的地址与令牌重试。**
    ///
    /// 这两种失败最常见的原因是网关重启过：令牌每次启动重新生成，地址也可能变。刷新只做
    /// 一次，第二次还失败就是真的失败（网关没起来、或者确实没权限）。
    ///
    /// `build` 拿的是**当前**的连接信息——重试要重新拼一遍请求，不能拿刷新之前那份地址
    /// 与令牌再发一次（那正是第一次为什么失败）。
    async fn send<T: DeserializeOwned, F>(&self, build: F) -> ClientResult<T>
    where
        F: Fn(&KomoClient) -> reqwest::RequestBuilder,
    {
        match self.send_once(build(self)).await {
            Err(error) if is_stale_connection(&error) => {
                if !self.refresh().await {
                    return Err(error);
                }
                self.send_once(build(self)).await
            }
            other => other,
        }
    }

    async fn send_once<T: DeserializeOwned>(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> ClientResult<T> {
        let response = self.authed(builder).send().await?;
        let status = response.status().as_u16();
        let body = response.text().await?;
        if !(200..300).contains(&status) {
            return Err(ClientError::from_body(status, &body));
        }
        // 204 之类的空体：让 `()` 这样的目标类型也能用。
        let text = if body.trim().is_empty() {
            "null"
        } else {
            &body
        };
        serde_json::from_str(text).map_err(|e| ClientError::Decode(format!("{e}（{body}）")))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> ClientResult<T> {
        self.send(|client| client.http.get(client.url(path))).await
    }

    async fn get_with<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> ClientResult<T> {
        let suffix = query_string(query);
        self.send(|client| client.http.get(format!("{}{suffix}", client.url(path))))
            .await
    }

    async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ClientResult<T> {
        self.send(|client| client.http.post(client.url(path)).json(body))
            .await
    }

    // ---- GET /healthz ----

    /// **唯一不认证的接口**（§13.1）。
    pub async fn health(&self) -> ClientResult<HealthResponse> {
        let response = self.http.get(self.url("/healthz")).send().await?;
        let status = response.status().as_u16();
        let body = response.text().await?;
        if !(200..300).contains(&status) {
            return Err(ClientError::from_body(status, &body));
        }
        serde_json::from_str(&body).map_err(|e| ClientError::Decode(format!("{e}（{body}）")))
    }

    // ---- /v1/sessions ----

    pub async fn create_session(
        &self,
        request: &CreateSessionRequest,
    ) -> ClientResult<SessionSummary> {
        self.post("/v1/sessions", request).await
    }

    /// 列出会话。默认**不含**已逻辑删除的（§8.10）；`SessionListQuery { all: true }` 才
    /// 把 `closing` / `deleted` 的也列出来（`purged` 只在显式查看单个会话时可见）。
    pub async fn list_sessions(
        &self,
        query: &SessionListQuery,
    ) -> ClientResult<SessionListResponse> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if query.all {
            params.push(("all", "true".into()));
        }
        self.get_with("/v1/sessions", &params).await
    }

    pub async fn session(&self, session: &SessionId) -> ClientResult<SessionDetail> {
        self.get(&format!("/v1/sessions/{session}")).await
    }

    /// 按游标读一页事件。SSE 订阅见 [`crate::sse`]。
    pub async fn events(&self, session: &SessionId, query: &EventQuery) -> ClientResult<EventPage> {
        let mut params = vec![("from", query.from.0.to_string())];
        if let Some(limit) = query.limit {
            params.push(("limit", limit.to_string()));
        }
        self.get_with(&format!("/v1/sessions/{session}/events"), &params)
            .await
    }

    pub async fn submit_run(
        &self,
        session: &SessionId,
        request: &SubmitRunRequest,
    ) -> ClientResult<SubmitRunResponse> {
        self.post(&format!("/v1/sessions/{session}/runs"), request)
            .await
    }

    pub async fn resume(
        &self,
        session: &SessionId,
        request: &ResumeRequest,
    ) -> ClientResult<ResumeResponse> {
        self.post(&format!("/v1/sessions/{session}/resume"), request)
            .await
    }

    /// `/new`：追加 `conversation.boundary`，**不切 Session**（§13.1）。
    ///
    /// TUI 不带 `by`——那个字段是给聊天渠道填发送者的，终端前的人没有平台身份。
    pub async fn boundary(
        &self,
        session: &SessionId,
        request: &BoundaryRequest,
    ) -> ClientResult<BoundaryResponse> {
        self.post(&format!("/v1/sessions/{session}/boundary"), request)
            .await
    }

    /// `komo session delete`：逻辑删除（§8.10）——进 `closing`，**不碰内容**。
    ///
    /// `now` 是「立刻走完」：把未完成的 Run 各写一条明确取消（回执里的 `cancelled`）再进
    /// `deleted`。不带 `now` 时，`closing → deleted` 由 reconcile 在"已无未完成 Run"时
    /// 推进——那是判定，不是时钟。
    pub async fn delete_session(
        &self,
        session: &SessionId,
        now: bool,
    ) -> ClientResult<SessionLifecycleResponse> {
        self.post(
            &format!("/v1/sessions/{session}/delete"),
            &DeleteSessionRequest {
                now,
                request_key: None,
            },
        )
        .await
    }

    /// `komo session purge`：回收内容进 `purged`（§8.10）。
    ///
    /// 引用检查不过时是 **409，正文是 [`PurgeBlocked`]**（列出要先处置什么），而它不是
    /// [`crate::error::ClientError::Api`] 的统一错误体——这里落成
    /// [`crate::error::ClientError::Http`]，`body` 原样带着那份 JSON。**不假装成功**。
    ///
    /// 墓碑先落、内容后删，所以重跑是幂等的：目录已不在时 `removed_bytes` 是 0。
    pub async fn purge_session(&self, session: &SessionId) -> ClientResult<PurgeSessionResponse> {
        self.post(
            &format!("/v1/sessions/{session}/purge"),
            &PurgeSessionRequest::default(),
        )
        .await
    }

    /// `POST /v1/reconcile`：立刻跑一次对账（§8.9）。**幂等**——同一批输入跑十遍与跑一遍
    /// 结果相同，所以它没有请求键。
    pub async fn reconcile(&self) -> ClientResult<ReconcileResponse> {
        self.post("/v1/reconcile", &serde_json::json!({})).await
    }

    // ---- /v1/runs ----

    pub async fn run(&self, run: &RunId) -> ClientResult<RunDetail> {
        self.get(&format!("/v1/runs/{run}")).await
    }

    pub async fn cancel_run(
        &self,
        run: &RunId,
        request: &CancelRunRequest,
    ) -> ClientResult<CancelRunResponse> {
        self.post(&format!("/v1/runs/{run}/cancel"), request).await
    }

    // ---- /v1/interventions（§7.5）----

    /// 待处理清单：**审批、结果不明、阻塞三类一张表**（§7.5）。
    ///
    /// 它是 `runs` 与 `approval_requests` 的并集查询——派生视图，不是第四张表。所以这里
    /// 每一个查询条件都只是过滤，不改变"权威在哪"。
    pub async fn interventions(
        &self,
        query: &InterventionListQuery,
    ) -> ClientResult<InterventionListResponse> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(session) = &query.session {
            params.push(("session", session.to_string()));
        }
        if let Some(run) = &query.run {
            params.push(("run", run.to_string()));
        }
        if let Some(kind) = &query.kind {
            params.push(("kind", enum_param(kind)?));
        }
        self.get_with("/v1/interventions", &params).await
    }

    /// 单项详情。`handle` 就是 [`InterventionSummary::handle`]：审批是短 ID（§11.3），
    /// `verify` / `blocked` 是 Run ID。
    pub async fn intervention(&self, handle: &str) -> ClientResult<InterventionDetail> {
        self.get(&format!("/v1/interventions/{}", path_segment(handle)))
            .await
    }

    /// 答复一条。**已答复的返回原结论，不报错**（§11.3）——`already_answered` 说明这次
    /// 什么都没改。
    ///
    /// 结论按种类分派（§7.5）：`approve` / `reject` 是审批的，`satisfied` /
    /// `not_performed` 是 `verify` 的，`resolve` 是 `blocked` 的，`abandon` 三类共有。
    /// 范围（`scope`）只有 `approve` 用得上。
    pub async fn answer_intervention(
        &self,
        handle: &str,
        request: &InterventionAnswerRequest,
    ) -> ClientResult<InterventionAnswerResponse> {
        self.post(
            &format!("/v1/interventions/{}/answer", path_segment(handle)),
            request,
        )
        .await
    }

    /// 一次答一批（§11.3 的 `/approve all`）。名单由**发起方**列出——协议里没有"全部"，
    /// 服务端不替操作者决定哪些算全部。
    ///
    /// **只答审批，而且范围固定为本次调用**：范围授权绑的是一份具体的计划，一批互不相干
    /// 的计划共用一个范围，只能是替操作者猜一个他没看过的答复。
    pub async fn answer_interventions(
        &self,
        request: &InterventionBatchAnswerRequest,
    ) -> ClientResult<InterventionBatchAnswerResponse> {
        self.post("/v1/interventions/answers", request).await
    }

    // ---- /v1/approvals（只有审计这一个用途）----

    /// 与这个 Run 相关的审批记录，**含已经决定过的**（§7.4 的审计面）。
    ///
    /// 这是审批这一族里**唯一还活着的读**，而它存在的理由只有一个：`komo run inspect`
    /// 要答「这一步是谁放行的」——那条审批早就不在待处理集合里了，§7.5 的统一清单
    /// （只列待处理）按定义查不到它。
    ///
    /// 答复不在这里：`approve` / `reject` 走 [`KomoClient::answer_intervention`]，与另外两类
    /// 同一条路。也没有单条 `GET /v1/approvals/{id}`——详情走
    /// [`KomoClient::intervention`]。
    pub async fn approvals_of_run(&self, run: &RunId) -> ClientResult<Vec<ApprovalRecord>> {
        let response: ApprovalListResponse = self
            .get_with(
                "/v1/approvals",
                &[("run", run.to_string()), ("include_decided", "true".into())],
            )
            .await?;
        Ok(response.approvals)
    }

    // ---- /v1/cron ----

    pub async fn cron_list(&self) -> ClientResult<CronListResponse> {
        self.get("/v1/cron").await
    }

    pub async fn cron_create(
        &self,
        request: &CreateCronRequest,
    ) -> ClientResult<komo_kernel::cron::CronJob> {
        self.post("/v1/cron", request).await
    }

    /// 手动触发。**独立请求幂等键，不冒充定时触发**（§10）。
    pub async fn cron_run(
        &self,
        job: &CronJobId,
        request: &ManualCronRunRequest,
    ) -> ClientResult<ManualCronRunResponse> {
        self.post(&format!("/v1/cron/{job}/run"), request).await
    }

    pub async fn cron_update(
        &self,
        job: &CronJobId,
        request: &UpdateCronRequest,
    ) -> ClientResult<komo_kernel::cron::CronJob> {
        self.send(|client| {
            client
                .http
                .patch(client.url(&format!("/v1/cron/{job}")))
                .json(request)
        })
        .await
    }

    pub async fn cron_delete(&self, job: &CronJobId) -> ClientResult<CronDeleteResponse> {
        self.send(|client| client.http.delete(client.url(&format!("/v1/cron/{job}"))))
            .await
    }

    // ---- /v1/memories ----

    pub async fn memories(&self, query: &MemoryListQuery) -> ClientResult<MemoryListResponse> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(scope) = &query.scope {
            // `MemoryScope` 自己的 query string 写法（kernel 的 `Display` / `FromStr`），
            // 两边共用一份，不在这里再编一种。
            params.push(("scope", scope.to_string()));
        }
        if let Some(state) = &query.state {
            params.push(("state", enum_param(state)?));
        }
        if let Some(text) = &query.query {
            params.push(("query", text.clone()));
        }
        if let Some(mode) = &query.mode {
            params.push(("mode", enum_param(mode)?));
        }
        if let Some(limit) = query.limit {
            params.push(("limit", limit.to_string()));
        }
        self.get_with("/v1/memories", &params).await
    }

    pub async fn memory(&self, memory: &MemoryId) -> ClientResult<MemoryDetail> {
        self.get(&format!("/v1/memories/{memory}")).await
    }

    pub async fn memory_confirm(
        &self,
        memory: &MemoryId,
        request: &MemoryRevisionRequest,
    ) -> ClientResult<MemoryDetail> {
        self.post(&format!("/v1/memories/{memory}/confirm"), request)
            .await
    }

    pub async fn memory_forget(
        &self,
        memory: &MemoryId,
        request: &MemoryRevisionRequest,
    ) -> ClientResult<MemoryDetail> {
        self.post(&format!("/v1/memories/{memory}/forget"), request)
            .await
    }

    pub async fn memory_index(&self) -> ClientResult<MemoryIndexStatus> {
        self.get("/v1/memory-index").await
    }

    pub async fn rebuild_memory_index(
        &self,
        request: &RebuildIndexRequest,
    ) -> ClientResult<RebuildIndexResponse> {
        self.post("/v1/memory-index/rebuild", request).await
    }

    // ---- /v1/models、/v1/config ----

    /// 这个 Gateway 能切到哪些模型，每个支持哪几档 effort（§13.3）。
    pub async fn models(&self) -> ClientResult<ModelsResponse> {
        self.get("/v1/models").await
    }

    /// `komo config check`：只读，**不改变运行中的 Gateway**（§3 命令表）。
    ///
    /// 校验不过**不是**这个调用的错误——问题在 `issues` 里，因为 check 本来就是去问
    /// 「现在这份配置有没有毛病」的。
    pub async fn config_check(&self) -> ClientResult<ConfigCheckResponse> {
        self.get("/v1/config/check").await
    }

    /// `komo config reload`：校验通过才装。
    ///
    /// **校验不过走错误体**——[`ErrorCode::ConfigInvalid`] 带 `keys` 定位，旧快照原样
    /// 保留（§3 第 1 步）。所以这里的 `Err` 是一个正常结果，调用方要把 `keys` 印出来。
    pub async fn config_reload(&self) -> ClientResult<ConfigReloadResponse> {
        self.post("/v1/config/reload", &serde_json::json!({})).await
    }
}

/// 把参数拼成 `?a=1&b=2`。
///
/// reqwest 的 `query()` 要 `urlencoded` 特性（serde_urlencoded），而 §13.4 把 reqwest 的
/// 特性写死成四个，客户端不去动它——一个百分号编码的字符表是十几行，不值得多一条
/// 依赖边。
pub(crate) fn query_string(params: &[(&str, String)]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let mut out = String::from("?");
    for (index, (key, value)) in params.iter().enumerate() {
        if index > 0 {
            out.push('&');
        }
        percent_encode(key, &mut out);
        out.push('=');
        percent_encode(value, &mut out);
    }
    out
}

/// 一个放进路径里的片段（`/v1/interventions/{handle}`）。句柄可能是 Run ID，客户端的
/// 调用方也可能转手把用户输入递进来——编码一次，不让它拼出另一条路径。
fn path_segment(raw: &str) -> String {
    let mut out = String::new();
    percent_encode(raw, &mut out);
    out
}

/// RFC 3986 的 unreserved 原样，其余按字节百分号编码。
fn percent_encode(raw: &str, out: &mut String) {
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
}

/// 一个 `rename_all = "snake_case"` 的单元枚举在 query string 里的写法：它的线格式。
fn enum_param<T: Serialize>(value: &T) -> ClientResult<String> {
    match serde_json::to_value(value).map_err(|e| ClientError::Decode(e.to_string()))? {
        serde_json::Value::String(s) => Ok(s),
        other => Err(ClientError::BadUrl(format!("{other} 不是一个查询参数"))),
    }
}

/// 幂等请求键的构造器。调用方**自己**决定什么算"同一次操作"。
pub struct RequestKeys;

impl RequestKeys {
    /// 一次新的操作：UUIDv7，前 48 位就是这一刻。重发同一次操作要**复用**返回的键，
    /// 不是再调一次这个函数。
    pub fn fresh(now: time::OffsetDateTime) -> RequestKey {
        RequestKey::new(uuid_v7_at(now).to_string())
    }

    /// 由一个稳定的名字派生，例如 TUI 里"这次提交的这段文本"。
    pub fn named(prefix: &str, id: &str) -> RequestKey {
        RequestKey::new(format!("{prefix}:{id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::memory::{MemoryState, RetrievalMode};

    #[test]
    fn a_base_url_must_name_a_scheme() {
        crate::test_server::install_crypto_provider();
        assert!(KomoClient::new("127.0.0.1:7777", None).is_err());
        assert!(KomoClient::new("http://127.0.0.1:7777", None).is_ok());
    }

    #[test]
    fn a_trailing_slash_does_not_double_up_in_paths() {
        crate::test_server::install_crypto_provider();
        let client = KomoClient::new("http://127.0.0.1:7777/", None).unwrap();
        assert_eq!(client.url("/healthz"), "http://127.0.0.1:7777/healthz");
    }

    #[test]
    fn the_token_never_shows_up_in_debug_output() {
        crate::test_server::install_crypto_provider();
        let client = KomoClient::new("http://127.0.0.1:1", Some("secret-token".into())).unwrap();
        let printed = format!("{client:?}");
        assert!(!printed.contains("secret-token"), "{printed}");
    }

    #[test]
    fn a_scope_goes_on_the_wire_in_kernels_own_spelling() {
        // 两边共用 kernel 的 `Display` / `FromStr`，这里不再编一种。
        use komo_kernel::types::memory::MemoryScope;
        assert_eq!(MemoryScope::Personal.to_string(), "personal");
        assert_eq!(
            query_string(&[(
                "scope",
                MemoryScope::Project {
                    project_id: "komo".into()
                }
                .to_string()
            )]),
            "?scope=project%3Akomo"
        );
    }

    #[test]
    fn a_unit_enum_query_value_is_its_wire_name() {
        assert_eq!(enum_param(&MemoryState::Candidate).unwrap(), "candidate");
        assert_eq!(enum_param(&RetrievalMode::Hybrid).unwrap(), "hybrid");
    }

    #[test]
    fn a_query_string_percent_encodes_everything_outside_unreserved() {
        assert_eq!(query_string(&[]), "");
        assert_eq!(
            query_string(&[("from", "7".into()), ("limit", "20".into())]),
            "?from=7&limit=20"
        );
        // 中文与 `:` 都要编码，否则 `project:komo bot` 会把查询串截断。
        assert_eq!(
            query_string(&[("query", "深色 主题".into())]),
            "?query=%E6%B7%B1%E8%89%B2%20%E4%B8%BB%E9%A2%98"
        );
        assert_eq!(
            query_string(&[("scope", "project:komo".into())]),
            "?scope=project%3Akomo"
        );
    }

    #[test]
    fn a_handle_cannot_add_another_path_segment() {
        // `/v1/interventions/{handle}`：句柄里出现 `/` 时必须编码，否则一段输入就能指向
        // 另一个端点。
        assert_eq!(path_segment("7K2M"), "7K2M");
        assert_eq!(path_segment("run-1"), "run-1");
        assert_eq!(path_segment("../x y"), "..%2Fx%20y");
    }

    #[test]
    fn two_fresh_request_keys_are_different() {
        let now = time::OffsetDateTime::now_utc();
        assert_ne!(RequestKeys::fresh(now), RequestKeys::fresh(now));
        // 派生的键则是稳定的：重发同一次操作带同一个键。
        assert_eq!(
            RequestKeys::named("tui", "abc"),
            RequestKeys::named("tui", "abc")
        );
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use crate::error::ClientError;
    use crate::test_server::{FakeGateway, Reply};
    use komo_kernel::protocol::PROTOCOL_VERSION;
    use komo_kernel::types::chat::ApprovalScope;
    use komo_kernel::types::ids::Seq;

    fn health_body() -> serde_json::Value {
        serde_json::json!({
            "instance_id": "inst-1",
            "version": "0.8.0",
            "protocol_version": PROTOCOL_VERSION,
            "started_at": "2026-09-16T08:00:00Z",
            "data_dir": "/home/u/.komo"
        })
    }

    /// 发现文件那一行的样子（`runtime/gateway.json`）。
    fn write_discovery(home: &std::path::Path, base_url: &str, instance: &str, token: &str) {
        let dir = home.join("runtime");
        std::fs::create_dir_all(&dir).expect("建 runtime/");
        let body = serde_json::json!({
            "instance_id": instance,
            "base_url": base_url,
            "protocol_version": PROTOCOL_VERSION,
            "version": "0.8.0",
            "pid": 4242,
            "data_dir": home.to_string_lossy(),
            "token": token,
            "started_at": "2026-09-19T00:00:00Z"
        });
        std::fs::write(dir.join("gateway.json"), body.to_string()).expect("写发现文件");
    }

    /// 一个"只认当时那个令牌"的假实例：`/healthz` 照常答，其余裸奔就 401。
    ///
    /// 令牌可以随时换（`Arc<Mutex<String>>`）——那就是网关重启之后的样子：**同一个端口**，
    /// 换了一份新令牌。
    /// 假实例的身份：**实例 id 与令牌一起换**——重启换的就是这两样。
    #[derive(Clone)]
    struct Identity(std::sync::Arc<std::sync::Mutex<(String, String)>>);

    impl Identity {
        fn new(instance: &str, token: &str) -> Identity {
            Identity(std::sync::Arc::new(std::sync::Mutex::new((
                instance.to_string(),
                token.to_string(),
            ))))
        }

        fn restart_as(&self, instance: &str, token: &str) {
            *self.0.lock().expect("身份") = (instance.to_string(), token.to_string());
        }

        fn get(&self) -> (String, String) {
            self.0.lock().expect("身份").clone()
        }
    }

    fn guarding(
        identity: Identity,
        data_dir: String,
    ) -> impl Fn(&crate::test_server::Recorded, usize) -> Reply + Send + Sync + 'static {
        move |request, _| {
            let (instance, token) = identity.get();
            if request.path.contains("/healthz") {
                // `/healthz` 要报**这个数据目录**：发现文件里那一行与它对不上会被判成
                // "不是同一个实例"（§3 第 2 步的第三条核对）。
                return Reply::ok(serde_json::json!({
                    "instance_id": instance,
                    "version": "0.8.0",
                    "protocol_version": PROTOCOL_VERSION,
                    "started_at": "2026-09-19T00:00:00Z",
                    "data_dir": data_dir
                }));
            }
            match request.header("authorization") {
                Some(value) if value == format!("Bearer {token}") => {
                    Reply::ok(serde_json::json!({"sessions": []}))
                }
                _ => Reply::error(401, "unauthorized", "令牌不对"),
            }
        }
    }

    /// 同一个端口、换了一份令牌（网关重启最常见的样子）：**401 一次，自己换过来。**
    ///
    /// 线上表现就是这条要挡的：客户端攥着上一个实例的令牌，每一次请求都 401、SSE 永远
    /// 重连不上，界面停在"重连中"直到人手动重开。
    #[tokio::test]
    async fn a_request_retries_with_the_new_token_after_a_restart() {
        let home = tempfile::tempdir().expect("临时数据目录");
        let data_dir = home.path().to_string_lossy().to_string();
        let identity = Identity::new("inst-1", "old-token");
        let gateway = FakeGateway::spawn(guarding(identity.clone(), data_dir.clone())).await;
        write_discovery(home.path(), &gateway.base_url(), "inst-1", "old-token");
        let client = crate::discovery::discover(home.path())
            .await
            .expect("发现得了")
            .client()
            .expect("建得出客户端");

        // 只数 API 那一路：刷新顺带会打一次 `/healthz` 核对实例身份（§3 第 2 步），
        // 那不是请求本身的往返。
        let api_calls = |gateway: &FakeGateway| {
            gateway
                .requests()
                .into_iter()
                .filter(|request| request.path.contains("/v1/sessions"))
                .count()
        };
        client
            .list_sessions(&SessionListQuery::default())
            .await
            .expect("重启之前连得上");
        let before = api_calls(&gateway);

        // 重启：同一个地址，新实例身份与新令牌。
        identity.restart_as("inst-2", "new-token");
        write_discovery(home.path(), &gateway.base_url(), "inst-2", "new-token");

        client
            .list_sessions(&SessionListQuery::default())
            .await
            .expect("401 之后要自己换令牌再试一次");
        assert_eq!(
            api_calls(&gateway),
            before + 2,
            "第一次 401、换过令牌之后一次成功——不该有第三次"
        );

        // 换过来之后就不再 401 了：一次请求一次往返。
        let after = api_calls(&gateway);
        client
            .list_sessions(&SessionListQuery::default())
            .await
            .expect("接着连得上");
        assert_eq!(api_calls(&gateway), after + 1);
    }

    /// 换了端口（新实例在另一个地址上）：按发现文件**跟过去**。
    #[tokio::test]
    async fn a_client_follows_a_gateway_that_moved_to_another_port() {
        let home = tempfile::tempdir().expect("临时数据目录");
        let data_dir = home.path().to_string_lossy().to_string();
        let old = FakeGateway::spawn(guarding(
            Identity::new("inst-1", "old-token"),
            data_dir.clone(),
        ))
        .await;
        let new = FakeGateway::spawn(guarding(
            Identity::new("inst-2", "new-token"),
            data_dir.clone(),
        ))
        .await;

        write_discovery(home.path(), &old.base_url(), "inst-1", "old-token");
        let client = crate::discovery::discover(home.path())
            .await
            .expect("发现得了")
            .client()
            .expect("建得出客户端");
        assert_eq!(client.base_url(), old.base_url());

        // 老实例下线、新实例在另一个端口起来，发现文件跟着改。
        drop(old);
        write_discovery(home.path(), &new.base_url(), "inst-2", "new-token");

        client
            .list_sessions(&SessionListQuery::default())
            .await
            .expect("地址换了也要自己跟过去");
        assert_eq!(client.base_url(), new.base_url(), "地址要跟着换");
    }

    /// 连不上（网关还没起来）时刷新不会假装成功，错误照旧报出来。
    #[tokio::test]
    async fn refresh_does_not_invent_an_instance_when_nothing_is_listening() {
        let home = tempfile::tempdir().expect("临时数据目录");
        write_discovery(home.path(), "http://127.0.0.1:1", "inst-gone", "token");
        let client = KomoClient::with_home(
            reqwest::Client::new(),
            "http://127.0.0.1:1",
            Some("token".into()),
            home.path().to_path_buf(),
            Some("inst-gone".into()),
        )
        .expect("建得出");
        assert!(!client.refresh().await, "读不出/连不上时不算刷新成功");
        assert!(matches!(
            client.list_sessions(&SessionListQuery::default()).await,
            Err(ClientError::Transport(_))
        ));
    }

    #[tokio::test]
    async fn healthz_is_the_one_call_that_carries_no_token() {
        let server = FakeGateway::spawn(|_, _| Reply::ok(health_body())).await;
        let client = server.client();
        let health = client.health().await.unwrap();
        assert_eq!(health.instance_id, "inst-1");
        let request = &server.requests()[0];
        assert_eq!(request.path, "/healthz");
        assert!(
            request.header("authorization").is_none(),
            "健康检查不认证（§13.1）"
        );
    }

    #[tokio::test]
    async fn every_other_call_carries_the_bearer_token() {
        let server =
            FakeGateway::spawn(|_, _| Reply::ok(serde_json::json!({"sessions": []}))).await;
        server
            .client()
            .list_sessions(&SessionListQuery::default())
            .await
            .unwrap();
        assert_eq!(
            server.requests()[0].header("authorization"),
            Some("Bearer test-token")
        );
    }

    #[tokio::test]
    async fn each_error_code_comes_back_as_itself() {
        use komo_kernel::protocol::http::ErrorCode::*;
        for (status, code, expected) in [
            (404u16, "not_found", NotFound),
            (401, "unauthorized", Unauthorized),
            (409, "request_key_conflict", RequestKeyConflict),
            (409, "version_conflict", VersionConflict),
            (400, "invalid_request", InvalidRequest),
            (403, "denied", Denied),
            (409, "conflict", Conflict),
            (400, "config_invalid", ConfigInvalid),
            (503, "vector_unavailable", VectorUnavailable),
            (500, "corrupt", Corrupt),
            (500, "internal", Internal),
        ] {
            let owned = code.to_string();
            let server =
                FakeGateway::spawn(move |_, _| Reply::error(status, &owned, "出事了")).await;
            let error = server
                .client()
                .list_sessions(&SessionListQuery::default())
                .await
                .unwrap_err();
            assert_eq!(error.code(), Some(expected), "{code}");
            assert!(error.to_string().contains("出事了"), "{error}");
        }
    }

    #[tokio::test]
    async fn a_non_json_failure_does_not_invent_an_error_code() {
        let server = FakeGateway::spawn(|_, _| Reply::Json {
            status: 502,
            body: "<html>bad gateway</html>".into(),
        })
        .await;
        let error = server
            .client()
            .list_sessions(&SessionListQuery::default())
            .await
            .unwrap_err();
        assert_eq!(error.code(), None);
        assert!(matches!(error, ClientError::Http { status: 502, .. }));
    }

    #[tokio::test]
    async fn a_config_error_carries_the_key_it_is_about() {
        let server = FakeGateway::spawn(|_, _| Reply::Json {
            status: 400,
            body: r#"{"error":{"code":"config_invalid","message":"缺少 base_url","keys":["memory.embedding.base_url"]}}"#.into(),
        })
        .await;
        let error = server
            .client()
            .list_sessions(&SessionListQuery::default())
            .await
            .unwrap_err();
        assert_eq!(
            error.keys().iter().map(|k| k.as_str()).collect::<Vec<_>>(),
            vec!["memory.embedding.base_url"]
        );
    }

    #[tokio::test]
    async fn an_unreachable_gateway_is_a_transport_failure_not_a_code() {
        // 监听一下拿到一个没人用的端口，然后立刻放掉。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        crate::test_server::FakeGateway::spawn(|_, _| Reply::Hangup).await; // 装 provider
        let client = KomoClient::new(&format!("http://{addr}"), None).unwrap();
        let error = client
            .list_sessions(&SessionListQuery::default())
            .await
            .unwrap_err();
        assert!(matches!(error, ClientError::Transport(_)), "{error}");
        assert_eq!(error.code(), None);
    }

    #[tokio::test]
    async fn the_idempotency_key_the_caller_gave_is_the_one_on_the_wire() {
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({
                "run": "run-1", "session": "sess-1", "seq": 1, "state": "queued"
            }))
        })
        .await;
        let request = SubmitRunRequest {
            request_key: RequestKeys::named("tui", "abc"),
            text: "你好".into(),
            model: None,
            effort: None,
        };
        let response = server
            .client()
            .submit_run(&SessionId::from_raw("sess-1"), &request)
            .await
            .unwrap();
        assert!(!response.deduplicated);
        let sent = &server.requests()[0];
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.path, "/v1/sessions/sess-1/runs");
        assert!(sent.body.contains("tui:abc"), "{}", sent.body);
    }

    #[tokio::test]
    async fn an_event_page_is_fetched_by_cursor() {
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({
                "session": "sess-1", "events": [], "next": 41, "more": false
            }))
        })
        .await;
        let page = server
            .client()
            .events(
                &SessionId::from_raw("sess-1"),
                &EventQuery {
                    from: Seq(7),
                    limit: Some(500),
                },
            )
            .await
            .unwrap();
        assert_eq!(page.next, Seq(41));
        let sent = &server.requests()[0];
        assert_eq!(sent.param("from").as_deref(), Some("7"));
        assert_eq!(sent.param("limit").as_deref(), Some("500"));
    }

    #[tokio::test]
    async fn an_intervention_query_names_the_session_the_run_and_the_kind() {
        let server =
            FakeGateway::spawn(|_, _| Reply::ok(serde_json::json!({"interventions": []}))).await;
        server
            .client()
            .interventions(&InterventionListQuery {
                session: Some(SessionId::from_raw("sess-1")),
                run: Some(RunId::from_raw("run-1")),
                kind: Some(InterventionKind::Verify),
            })
            .await
            .unwrap();
        let sent = &server.requests()[0];
        assert_eq!(sent.path, "/v1/interventions");
        assert_eq!(sent.param("session").as_deref(), Some("sess-1"));
        assert_eq!(sent.param("run").as_deref(), Some("run-1"));
        // 单元素枚举走它的线格式，与 kernel 的 serde 表现一致。
        assert_eq!(sent.param("kind").as_deref(), Some("verify"));
    }

    #[tokio::test]
    async fn the_default_intervention_query_asks_for_all_three_kinds() {
        let server =
            FakeGateway::spawn(|_, _| Reply::ok(serde_json::json!({"interventions": []}))).await;
        server
            .client()
            .interventions(&InterventionListQuery::default())
            .await
            .unwrap();
        // 不带 `kind`：三类一起要——清单不是"审批清单"（§7.5）。
        assert_eq!(server.requests()[0].query, "");
    }

    #[tokio::test]
    async fn a_boundary_from_the_tui_names_nobody() {
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({"session": "sess-1", "seq": 10}))
        })
        .await;
        server
            .client()
            .boundary(&SessionId::from_raw("sess-1"), &BoundaryRequest::default())
            .await
            .unwrap();
        let sent = &server.requests()[0];
        assert_eq!(sent.path, "/v1/sessions/sess-1/boundary");
        // 终端前的人没有平台身份，所以 `by` 根本不上线。
        assert!(!sent.body.contains("\"by\""), "{}", sent.body);
    }

    #[tokio::test]
    async fn a_manual_cron_run_carries_its_own_key() {
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({
                "job": "job-1", "session": "sess-1", "run": "run-9",
                "request_key": "cli:manual-1"
            }))
        })
        .await;
        let response = server
            .client()
            .cron_run(
                &CronJobId::from_raw("job-1"),
                &ManualCronRunRequest {
                    request_key: Some(RequestKey::new("cli:manual-1")),
                },
            )
            .await
            .unwrap();
        // §10：手动触发不冒充定时触发。
        assert_eq!(response.request_key.as_str(), "cli:manual-1");
        assert!(server.requests()[0].body.contains("cli:manual-1"));
    }

    #[tokio::test]
    async fn the_model_menu_says_which_efforts_each_model_takes() {
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({"models": [
                {"id": "chat-a", "provider": "openai_compatible",
                 "efforts": ["low", "high"], "default": true},
                {"id": "chat-b", "provider": "anthropic"}
            ]}))
        })
        .await;
        let response = server.client().models().await.unwrap();
        assert_eq!(server.requests()[0].path, "/v1/models");
        assert_eq!(response.models[0].efforts.len(), 2);
        assert!(response.models[0].default);
        // 老行没有这两个字段：空表 + 非默认。
        assert!(response.models[1].efforts.is_empty());
        assert!(!response.models[1].default);
    }

    #[tokio::test]
    async fn config_check_reports_problems_without_failing_the_call() {
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({
                "issues": [{"key": "model.base_url", "severity": "error", "message": "缺"}],
                "loaded_at": "2026-09-16T08:00:00Z",
                "sources": [{"path": "/home/u/.komo/config.toml",
                             "mtime": "2026-09-16T08:05:00Z"}]
            }))
        })
        .await;
        // check 就是去问「有没有毛病」的——有毛病不是这次调用失败。
        let response = server.client().config_check().await.unwrap();
        assert_eq!(server.requests()[0].path, "/v1/config/check");
        assert_eq!(response.issues.len(), 1);
        assert!(response.sources[0].mtime > response.loaded_at);
    }

    #[tokio::test]
    async fn a_reload_that_only_touched_start_only_keys_says_they_did_not_take_effect() {
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({
                "changed": ["start_only.listen"],
                "start_only": ["start_only.listen"]
            }))
        })
        .await;
        let response = server.client().config_reload().await.unwrap();
        assert_eq!(server.requests()[0].method, "POST");
        assert_eq!(response.start_only.len(), 1);
    }

    #[tokio::test]
    async fn a_rejected_reload_comes_back_as_config_invalid_with_its_keys() {
        let server = FakeGateway::spawn(|_, _| Reply::Json {
            status: 400,
            body: r#"{"error":{"code":"config_invalid","message":"缺少 base_url","keys":["memory.embedding.base_url"]}}"#.into(),
        })
        .await;
        let error = server.client().config_reload().await.unwrap_err();
        assert!(error.is(komo_kernel::protocol::http::ErrorCode::ConfigInvalid));
        assert_eq!(error.keys()[0].as_str(), "memory.embedding.base_url");
    }

    #[tokio::test]
    async fn an_answer_that_changed_nothing_says_so_rather_than_failing() {
        // §11.3：已答复的返回原结论，不报错。
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({
                "handle": "7K2M",
                "kind": "approval",
                "verdict": "approve",
                "decision": {
                    "approved": true,
                    "scope": "once",
                    "decided_at": "2026-09-16T08:00:00Z",
                    "consumed": false
                },
                "note": "已批准（本次调用）",
                "already_answered": true
            }))
        })
        .await;
        let response = server
            .client()
            .answer_intervention(
                "7K2M",
                &InterventionAnswerRequest {
                    verdict: InterventionVerdict::Approve,
                    scope: Some(ApprovalScope::Once),
                    request_key: None,
                },
            )
            .await
            .unwrap();
        assert!(response.already_answered);
        // 回执那句话由服务端给，界面直接用它（`note`）。
        assert_eq!(response.note, "已批准（本次调用）");
        let sent = &server.requests()[0];
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.path, "/v1/interventions/7K2M/answer");
    }

    #[tokio::test]
    async fn a_verify_answer_carries_no_scope() {
        // §7.5：范围只有审批用得上；`satisfied` 之类的结论不带它。
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({
                "handle": "run-1", "kind": "verify", "verdict": "satisfied",
                "note": "已补一条结果", "already_answered": false
            }))
        })
        .await;
        server
            .client()
            .answer_intervention(
                "run-1",
                &InterventionAnswerRequest {
                    verdict: InterventionVerdict::Satisfied,
                    scope: None,
                    request_key: None,
                },
            )
            .await
            .unwrap();
        assert!(
            !server.requests()[0].body.contains("scope"),
            "{}",
            server.requests()[0].body
        );
    }

    #[tokio::test]
    async fn a_batch_answer_names_every_handle_it_was_told_to() {
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({"answered": [], "missing": ["9QRS"]}))
        })
        .await;
        let response = server
            .client()
            .answer_interventions(&InterventionBatchAnswerRequest {
                handles: vec!["7K2M".into(), "9QRS".into()],
                approved: true,
                request_key: None,
            })
            .await
            .unwrap();
        // 点了名却已经不在的那些单独列出，不让整批失败（§11.3）。
        assert_eq!(response.missing, vec!["9QRS".to_string()]);
        let sent = &server.requests()[0];
        assert_eq!(sent.path, "/v1/interventions/answers");
        assert!(sent.body.contains("7K2M"), "{}", sent.body);
    }

    #[tokio::test]
    async fn the_run_scoped_approval_read_is_only_for_audit() {
        // §7.4：`komo run inspect` 要答「这一步是谁放行的」，而那条审批早就不在待处理
        // 集合里了——所以这一读必须带 `include_decided`，否则永远查不到它。
        let server =
            FakeGateway::spawn(|_, _| Reply::ok(serde_json::json!({"approvals": []}))).await;
        let records = server
            .client()
            .approvals_of_run(&RunId::from_raw("run-1"))
            .await
            .unwrap();
        assert!(records.is_empty());
        let sent = &server.requests()[0];
        assert_eq!(sent.method, "GET");
        assert_eq!(sent.path, "/v1/approvals");
        assert_eq!(sent.param("run").as_deref(), Some("run-1"));
        assert_eq!(sent.param("include_decided").as_deref(), Some("true"));
    }

    #[tokio::test]
    async fn a_session_list_asks_for_the_deleted_ones_only_when_told_to() {
        let server =
            FakeGateway::spawn(|_, _| Reply::ok(serde_json::json!({"sessions": []}))).await;
        server
            .client()
            .list_sessions(&SessionListQuery::default())
            .await
            .unwrap();
        // 默认那一份不带参数：已逻辑删除的不列（§8.10）。
        assert_eq!(server.requests()[0].query, "");

        server
            .client()
            .list_sessions(&SessionListQuery { all: true })
            .await
            .unwrap();
        assert_eq!(server.requests()[1].param("all").as_deref(), Some("true"));
    }

    #[tokio::test]
    async fn a_logical_delete_says_whether_to_wait_for_unfinished_runs() {
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({
                "session": "sess-1", "state": "deleted",
                "changed_at": "2026-09-16T08:00:00Z", "cancelled": ["run-1"]
            }))
        })
        .await;
        let response = server
            .client()
            .delete_session(&SessionId::from_raw("sess-1"), true)
            .await
            .unwrap();
        assert_eq!(
            response.state,
            komo_kernel::types::status::SessionState::Deleted
        );
        assert_eq!(response.cancelled.len(), 1);
        let sent = &server.requests()[0];
        assert_eq!(sent.path, "/v1/sessions/sess-1/delete");
        assert!(sent.body.contains("\"now\":true"), "{}", sent.body);
    }

    #[tokio::test]
    async fn a_purge_reports_how_much_it_reclaimed() {
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({
                "session": "sess-1", "state": "purged", "removed_bytes": 4096
            }))
        })
        .await;
        let response = server
            .client()
            .purge_session(&SessionId::from_raw("sess-1"))
            .await
            .unwrap();
        assert_eq!(response.removed_bytes, 4096);
        assert_eq!(server.requests()[0].path, "/v1/sessions/sess-1/purge");
    }

    #[tokio::test]
    async fn a_blocked_purge_comes_back_with_what_to_settle_first() {
        // §8.10：引用检查不过就 409 并列出要先处置什么，**不假装成功**。
        let server = FakeGateway::spawn(|_, _| Reply::Json {
            status: 409,
            body: r#"{"session":"sess-1","blockers":[{"what":"memory_evidence","detail":"3 条记忆还引用这里的事件"}]}"#.into(),
        })
        .await;
        let error = server
            .client()
            .purge_session(&SessionId::from_raw("sess-1"))
            .await
            .unwrap_err();
        // 409 的正文是 PurgeBlocked，不是统一错误体：原样带着它，不伪造一个 ErrorCode。
        let ClientError::Http { status, body } = error else {
            panic!("{error:?}");
        };
        assert_eq!(status, 409);
        assert!(body.contains("memory_evidence"), "{body}");
    }

    #[tokio::test]
    async fn reconcile_returns_what_this_pass_decided() {
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({
                "checked": 7, "resumed": 2, "blocked": 1, "reclaimed": 1,
                "closed": 3, "purged": 0, "finished_at": "2026-09-16T08:00:00Z"
            }))
        })
        .await;
        let response = server.client().reconcile().await.unwrap();
        assert_eq!(response.checked, 7);
        assert_eq!(response.blocked, 1);
        let sent = &server.requests()[0];
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.path, "/v1/reconcile");
    }
}
