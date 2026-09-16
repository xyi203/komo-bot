//! §13.1 接口的 HTTP 客户端与幂等请求键。
//!
//! 一个接口一个方法，请求与响应**就是** `komo_kernel::protocol::http` 里的类型——这样
//! 客户端与 Gateway 不可能各自演化出一个形状。
//!
//! **幂等请求键由调用方给。**提交输入、审批、Cron 与 Memory 变更的请求类型里都有
//! `request_key` 字段（§13.1），这里不代生成：客户端重发同一次操作要带同一个键，而
//! "同一次操作"是调用方才知道的事。[`RequestKeys`] 是给调用方用的构造器，不是默认值。

use komo_kernel::protocol::http::*;
use komo_kernel::types::ids::{
    ApprovalId, CronJobId, MemoryId, RequestKey, RunId, SessionId, uuid_v7_at,
};
use komo_kernel::types::memory::MemoryScope;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{ClientError, ClientResult};

/// 一个已解析的 Gateway 地址与它的认证令牌。
#[derive(Clone)]
pub struct KomoClient {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl std::fmt::Debug for KomoClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 令牌不进日志。
        f.debug_struct("KomoClient")
            .field("base", &self.base)
            .field("token", &self.token.as_ref().map(|_| "<set>"))
            .finish()
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
        Ok(KomoClient { http, base, token })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub(crate) fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn authed(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    async fn send<T: DeserializeOwned>(&self, builder: reqwest::RequestBuilder) -> ClientResult<T> {
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
        self.send(self.http.get(self.url(path))).await
    }

    async fn get_with<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> ClientResult<T> {
        let url = format!("{}{}", self.url(path), query_string(query));
        self.send(self.http.get(url)).await
    }

    async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ClientResult<T> {
        self.send(self.http.post(self.url(path)).json(body)).await
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
        // TODO(decide: §13.1 只说 "POST /v1/sessions 创建会话"，没有指定响应类型。
        // 取 `SessionSummary`——它是列表与详情共用的那一个。)
        self.post("/v1/sessions", request).await
    }

    pub async fn list_sessions(&self) -> ClientResult<SessionListResponse> {
        self.get("/v1/sessions").await
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
    pub async fn boundary(&self, session: &SessionId) -> ClientResult<BoundaryResponse> {
        self.post(
            &format!("/v1/sessions/{session}/boundary"),
            &serde_json::json!({}),
        )
        .await
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

    // ---- /v1/approvals ----

    /// 待审核列表（§13.1）。
    // TODO(decide: §13.1 没有给这个接口的查询参数，所以也没有"列出已决定的"。
    // `run inspect` 想印 "allowed by …" 时只能由调用方另行取 `GET /v1/approvals/{id}`。)
    pub async fn approvals(&self) -> ClientResult<ApprovalListResponse> {
        self.get("/v1/approvals").await
    }

    pub async fn approval(&self, approval: &ApprovalId) -> ClientResult<ApprovalRecord> {
        self.get(&format!("/v1/approvals/{approval}")).await
    }

    /// 批准或拒绝。**已决定的返回原决定，不报错**（§11.3）——`already_decided` 说明这次
    /// 什么都没改。
    pub async fn decide_approval(
        &self,
        approval: &ApprovalId,
        request: &ApprovalDecisionRequest,
    ) -> ClientResult<ApprovalDecisionResponse> {
        self.post(&format!("/v1/approvals/{approval}/decision"), request)
            .await
    }

    // ---- /v1/cron ----

    pub async fn cron_list(&self) -> ClientResult<CronListResponse> {
        self.get("/v1/cron").await
    }

    pub async fn cron_create(
        &self,
        request: &CreateCronRequest,
    ) -> ClientResult<komo_kernel::cron::CronJob> {
        // TODO(decide: §13.1 未指定响应类型；取 `CronJob`——创建完就该看见它的 next_run_at。)
        self.post("/v1/cron", request).await
    }

    /// 手动触发。**独立请求幂等键，不冒充定时触发**（§10）。
    pub async fn cron_run(
        &self,
        job: &CronJobId,
        request_key: Option<RequestKey>,
    ) -> ClientResult<ManualCronRunResponse> {
        // TODO(decide: kernel 有 `ManualCronRunResponse` 而没有对应的请求类型。
        // 这里就地拼一个只有 request_key 的对象；kernel 补了类型就换过去。)
        self.post(
            &format!("/v1/cron/{job}/run"),
            &serde_json::json!({ "request_key": request_key }),
        )
        .await
    }

    pub async fn cron_update(
        &self,
        job: &CronJobId,
        request: &UpdateCronRequest,
    ) -> ClientResult<komo_kernel::cron::CronJob> {
        self.send(
            self.http
                .patch(self.url(&format!("/v1/cron/{job}")))
                .json(request),
        )
        .await
    }

    pub async fn cron_delete(&self, job: &CronJobId) -> ClientResult<CronDeleteResponse> {
        self.send(self.http.delete(self.url(&format!("/v1/cron/{job}"))))
            .await
    }

    // ---- /v1/memories ----

    pub async fn memories(&self, query: &MemoryListQuery) -> ClientResult<MemoryListResponse> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(scope) = &query.scope {
            params.push(("scope", scope_param(scope)));
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

/// `MemoryScope` 是个带负载的枚举，query string 装不下嵌套对象。
// TODO(decide: §13.1 只说 "GET /v1/memories 按作用域、状态和查询条件列出"，没给写法。
// 取 `personal` / `project:<id>` / `environment:<id>` 这个扁平写法；Gateway 侧要认同一个。)
pub fn scope_param(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::Personal => "personal".to_string(),
        MemoryScope::Project { project_id } => format!("project:{project_id}"),
        MemoryScope::Environment { instance_id } => format!("environment:{instance_id}"),
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
    fn a_scope_flattens_into_one_query_value() {
        assert_eq!(scope_param(&MemoryScope::Personal), "personal");
        assert_eq!(
            scope_param(&MemoryScope::Project {
                project_id: "komo".into()
            }),
            "project:komo"
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
        server.client().list_sessions().await.unwrap();
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
            let error = server.client().list_sessions().await.unwrap_err();
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
        let error = server.client().list_sessions().await.unwrap_err();
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
        let error = server.client().list_sessions().await.unwrap_err();
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
        let error = client.list_sessions().await.unwrap_err();
        assert!(matches!(error, ClientError::Transport(_)), "{error}");
        assert_eq!(error.code(), None);
    }

    #[tokio::test]
    async fn the_idempotency_key_the_caller_gave_is_the_one_on_the_wire() {
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({
                "run": "run-1", "session": "sess-1", "seq": 1, "status": "queued"
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
    async fn a_decision_that_changed_nothing_says_so_rather_than_failing() {
        // §11.3：已决定的返回原决定，不报错。
        let server = FakeGateway::spawn(|_, _| {
            Reply::ok(serde_json::json!({
                "approval": "appr-1",
                "short_id": "7K2M",
                "decision": {
                    "approved": true,
                    "scope": "once",
                    "decided_at": "2026-09-16T08:00:00Z",
                    "consumed": false
                },
                "already_decided": true
            }))
        })
        .await;
        let response = server
            .client()
            .decide_approval(
                &ApprovalId::from_raw("appr-1"),
                &ApprovalDecisionRequest {
                    approved: true,
                    scope: komo_kernel::types::chat::ApprovalScope::Once,
                    request_key: None,
                },
            )
            .await
            .unwrap();
        assert!(response.already_decided);
        assert!(response.decision.approved);
    }
}
