//! W6 toolbox 验收的脚手架。
//!
//! 一台真 Gateway 用共用的 [`komo_gateway::service::test_support::harness`]；这里只加
//! 两样这一组测试独有的东西：
//!
//! - 一个 **loopback 假 Memos**（axum），§5.5 那几条验收对着它跑——本机没有 Memos 实例，
//!   而"写入返回 ID / 链接、查询回到原文"要的是一次真的 HTTP 往返，不是一个替身对象。
//! - toolbox 的几个 HTTP 助手。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use komo_gateway::service::test_support::harness::Gw;
use komo_runtime::toolbox::{ModuleInfo, TestReport};
use serde_json::{Value, json};

// ---------------------------------------------------------------- 假 Memos

/// 一个够 §5.5 用的 Memos：创建、读取、列出、改、删，外加一道 Bearer 校验。
///
/// **令牌真的校验**：模块"凭证只进 Authorization 头"这句话，没有一个会拒绝的服务端就
/// 证不出来。
#[derive(Clone, Default)]
pub struct FakeMemos {
    memos: Arc<Mutex<BTreeMap<u64, String>>>,
    next: Arc<Mutex<u64>>,
}

pub struct RunningMemos {
    pub base_url: String,
    pub state: FakeMemos,
    shutdown: tokio_util::sync::CancellationToken,
}

impl RunningMemos {
    /// 服务端现在存着的东西——断言"真的写进去了"用。
    pub fn contents(&self) -> Vec<String> {
        self.state
            .memos
            .lock()
            .expect("假 Memos")
            .values()
            .cloned()
            .collect()
    }

    /// 直接往服务端塞一条，绕过 HTTP——模拟"写入已经发生但响应丢了"。
    pub fn plant(&self, text: &str) -> u64 {
        let mut next = self.state.next.lock().expect("假 Memos");
        *next += 1;
        let id = *next;
        self.state
            .memos
            .lock()
            .expect("假 Memos")
            .insert(id, text.to_string());
        id
    }
}

impl Drop for RunningMemos {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

pub const MEMOS_TOKEN: &str = "fake-memos-token";

pub async fn start_memos() -> RunningMemos {
    let state = FakeMemos::default();
    let app = axum::Router::new()
        .route("/api/v1/memos", get(list).post(create))
        .route("/api/v1/memos/{id}", get(show).patch(update).delete(remove))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("假 Memos 绑得上");
    let addr = listener.local_addr().expect("地址");
    let shutdown = tokio_util::sync::CancellationToken::new();
    let stop = shutdown.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { stop.cancelled().await })
            .await;
    });
    RunningMemos {
        base_url: format!("http://{addr}"),
        state,
        shutdown,
    }
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(&format!("Bearer {MEMOS_TOKEN}"))
}

fn memo(id: u64, content: &str) -> Value {
    json!({ "name": format!("memos/{id}"), "content": content })
}

async fn create(
    State(state): State<FakeMemos>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"message": "no"})));
    }
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut next = state.next.lock().expect("假 Memos");
    *next += 1;
    let id = *next;
    state
        .memos
        .lock()
        .expect("假 Memos")
        .insert(id, content.clone());
    (StatusCode::OK, Json(memo(id, &content)))
}

async fn list(State(state): State<FakeMemos>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    if !authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"message": "no"})));
    }
    // **故意不实现 filter**：模块因此必须自己在本地再筛一遍，那正是 §5.5「不能把
    // latest 文档当成该实例的接口保证」要的那份谨慎。
    let memos: Vec<Value> = state
        .memos
        .lock()
        .expect("假 Memos")
        .iter()
        .map(|(id, content)| memo(*id, content))
        .collect();
    (StatusCode::OK, Json(json!({ "memos": memos })))
}

async fn show(
    State(state): State<FakeMemos>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    if !authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"message": "no"})));
    }
    let key: u64 = match id.parse() {
        Ok(key) => key,
        Err(_) => return (StatusCode::NOT_FOUND, Json(json!({"message": "not found"}))),
    };
    match state.memos.lock().expect("假 Memos").get(&key) {
        Some(content) => (StatusCode::OK, Json(memo(key, content))),
        None => (StatusCode::NOT_FOUND, Json(json!({"message": "not found"}))),
    }
}

async fn update(
    State(state): State<FakeMemos>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"message": "no"})));
    }
    let key: u64 = match id.parse() {
        Ok(key) => key,
        Err(_) => return (StatusCode::NOT_FOUND, Json(json!({"message": "not found"}))),
    };
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut memos = state.memos.lock().expect("假 Memos");
    if !memos.contains_key(&key) {
        return (StatusCode::NOT_FOUND, Json(json!({"message": "not found"})));
    }
    memos.insert(key, content.clone());
    (StatusCode::OK, Json(memo(key, &content)))
}

async fn remove(
    State(state): State<FakeMemos>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    if !authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"message": "no"})));
    }
    let key: u64 = id.parse().unwrap_or(0);
    match state.memos.lock().expect("假 Memos").remove(&key) {
        Some(_) => (StatusCode::OK, Json(json!({}))),
        None => (StatusCode::NOT_FOUND, Json(json!({"message": "not found"}))),
    }
}

// ---------------------------------------------------------------- toolbox 助手

/// 这台 Gateway 的 toolbox 目录。
pub fn toolbox_dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join("toolbox")
}

/// 写一个候选（模型走的是 `write` 工具，验收这一侧直接写盘——测的是启用流程，
/// 不是 `write`）。
pub fn write_candidate(home: &std::path::Path, module: &str, code: &str, tests: Option<&str>) {
    let staging = toolbox_dir(home).join(".staging");
    std::fs::create_dir_all(&staging).expect("候选目录");
    std::fs::write(staging.join(format!("{module}.py")), code).expect("写候选");
    if let Some(tests) = tests {
        std::fs::write(staging.join(format!("test_{module}.py")), tests).expect("写候选测试");
    }
}

pub async fn toolbox_list(gw: &Gw) -> Vec<ModuleInfo> {
    let (code, body) = gw.get("/v1/toolbox").await;
    assert_eq!(code, 200, "{body}");
    let parsed: Value = serde_json::from_str(&body).expect("清单");
    serde_json::from_value(parsed["modules"].clone()).expect("模块")
}

pub async fn toolbox_show(gw: &Gw, module: &str) -> ModuleInfo {
    let (code, body) = gw.get(&format!("/v1/toolbox/{module}")).await;
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).expect("模块")
}

pub async fn toolbox_test(gw: &Gw, module: &str) -> TestReport {
    let (code, body) = gw
        .post(&format!("/v1/toolbox/{module}/test"), json!({}))
        .await;
    assert_eq!(code, 200, "{body}");
    let parsed: Value = serde_json::from_str(&body).expect("测试结果");
    serde_json::from_value(parsed["report"].clone()).expect("报告")
}

/// 等一个条件成立——启用是批准之后由后台任务装上的，所以断言要等。
pub async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("等了 20 秒也没等到「{what}」");
}

pub fn have_python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}
