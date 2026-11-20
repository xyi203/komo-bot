//! Hindsight（vectorize-io/hindsight，自建服务）作为外部记忆提供方（`docs/memory.md` 附录 G）。
//!
//! 用到它的四个接口（按 1.2.1 的 HTTP API，本机实测过形状）：
//!
//! - 召回 `POST /v1/default/banks/{bank}/memories/recall`：`results[]` 里有 `id / text / tags / scores.final`；
//! - 现状 `GET …/memories/{id}`：`state = invalidated` 是作废，404 是删了；
//! - 清单 `GET …/memories/list?state=valid&limit&offset&q`：`items[]`（多带 `fact_type / date`）与 `total`，记忆页只读展示；
//!   `q` 是正文子串筛选，它自己说是扫全 bank 的，只给人翻看用；
//! - 写入 `POST …/memories`：交一段窗口正文，`document_id` 是幂等键（同一个键再交是覆盖），`async = true`
//!   让它排队提取、立刻返回。bank 不存在时先 `PUT …/banks/{bank}` 建一个（建过的再 PUT 只是更新名字）。
//!
//! 作用域用标签表示：`scope:personal`、`scope:agent:<id>`……写入时打一个；召回时按放行的作用域转成标签、任一命中
//! 即可。放行列表为空（共享、没写 `memory_scope`）时只查 `scope:personal`——共享提取写入的都是它，`agent:*` 本来
//! 就不该被看到。

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use komo_kernel::protocol::config::HindsightConfig;
use komo_kernel::types::memory::MemoryScope;
use komo_memory::provider::{
    EvidenceBatch, MemoryProvider, ProviderError, ProviderHit, ProviderListed, ProviderPage,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::OnceCell;

/// 单次请求的上限；核心另有召回 5 秒、写入 15 秒的时限。
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// 召回给它的 token 预算：只取前几条，不必让它多拼。
const RECALL_TOKENS: u32 = 2048;

pub struct Hindsight {
    http: reqwest::Client,
    /// `{url}/v1/default/banks/{bank}`。
    base: String,
    bank: String,
    bank_ready: OnceCell<()>,
    /// 召回的重排分数下限（`HindsightConfig::min_reranker`）。
    min_reranker: f64,
}

impl std::fmt::Debug for Hindsight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hindsight")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Hindsight {
    pub fn new(config: &HindsightConfig) -> Self {
        Hindsight {
            http: reqwest::Client::new(),
            base: format!(
                "{}/v1/default/banks/{}",
                config.url.trim_end_matches('/'),
                config.bank
            ),
            bank: config.bank.clone(),
            bank_ready: OnceCell::new(),
            min_reranker: config.min_reranker,
        }
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ProviderError> {
        request
            .timeout(CALL_TIMEOUT)
            .send()
            .await
            .map_err(|error| ProviderError(format!("hindsight 连不上：{error}")))
    }

    async fn json(response: reqwest::Response) -> Result<Value, ProviderError> {
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| ProviderError(format!("hindsight 响应读不完：{error}")))?;
        if !status.is_success() {
            return Err(ProviderError(format!(
                "hindsight 回了 {status}：{}",
                String::from_utf8_lossy(&body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )));
        }
        serde_json::from_slice(&body)
            .map_err(|error| ProviderError(format!("hindsight 响应解析不了：{error}")))
    }

    /// bank 不存在就建；建过的 PUT 只是更新名字。每个进程只做一次。
    async fn ensure_bank(&self) -> Result<(), ProviderError> {
        self.bank_ready
            .get_or_try_init(|| async {
                let response = self
                    .send(
                        self.http
                            .put(&self.base)
                            .json(&json!({ "name": format!("komo {}", self.bank) })),
                    )
                    .await?;
                Self::json(response).await.map(|_| ())
            })
            .await
            .map(|_| ())
    }
}

#[derive(Debug, Deserialize)]
struct Fact {
    id: String,
    text: String,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    scores: Option<Scores>,
    /// 只有 `GET memory` 带：`valid` / `invalidated`。
    #[serde(default)]
    state: Option<String>,
    /// 清单带：`world` / `experience` / `observation`。
    #[serde(default)]
    fact_type: Option<String>,
    /// 清单带：它记下的事发生的时刻（RFC 3339）。
    #[serde(default)]
    date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Scores {
    #[serde(default, rename = "final")]
    total: Option<f64>,
}

impl Fact {
    /// 写入时打的作用域标签；没有或读不出来按 Personal。
    fn scope(&self) -> MemoryScope {
        self.tags
            .iter()
            .flatten()
            .find_map(|tag| tag.strip_prefix("scope:"))
            .and_then(|raw| MemoryScope::from_str(raw).ok())
            .unwrap_or(MemoryScope::Personal)
    }

    fn into_hit(self) -> ProviderHit {
        ProviderHit {
            scope: self.scope(),
            id: self.id,
            text: self.text,
            score: self.scores.and_then(|scores| scores.total),
        }
    }

    fn into_listed(self) -> ProviderListed {
        ProviderListed {
            scope: self.scope(),
            at: self
                .date
                .as_deref()
                .and_then(|date| date.parse::<Timestamp>().ok()),
            id: self.id,
            text: self.text,
            kind: self.fact_type,
        }
    }
}

fn scope_tag(scope: &MemoryScope) -> String {
    format!("scope:{scope}")
}

/// 放行的作用域 → 召回时的标签（任一命中）。空 = 共享召回，只有 `scope:personal`。
fn recall_tags(scopes: &[MemoryScope]) -> Vec<String> {
    if scopes.is_empty() {
        return vec![scope_tag(&MemoryScope::Personal)];
    }
    scopes.iter().map(scope_tag).collect()
}

#[async_trait]
impl MemoryProvider for Hindsight {
    fn name(&self) -> &str {
        "hindsight"
    }

    async fn recall(
        &self,
        text: &str,
        scopes: &[MemoryScope],
        limit: u32,
    ) -> Result<Vec<ProviderHit>, ProviderError> {
        #[derive(Deserialize)]
        struct Recalled {
            results: Vec<Fact>,
        }
        let response = self
            .send(
                self.http
                    .post(format!("{}/memories/recall", self.base))
                    .json(&json!({
                        "query": text,
                        "max_tokens": RECALL_TOKENS,
                        "tags": recall_tags(scopes),
                        "tags_match": "any",
                        "min_scores": { "reranker": self.min_reranker },
                    })),
            )
            .await?;
        let recalled: Recalled = serde_json::from_value(Self::json(response).await?)
            .map_err(|error| ProviderError(format!("hindsight 召回结果解析不了：{error}")))?;
        Ok(recalled
            .results
            .into_iter()
            .take(limit as usize)
            .map(Fact::into_hit)
            .collect())
    }

    async fn current(&self, ids: &[String]) -> Result<Vec<ProviderHit>, ProviderError> {
        let mut alive = Vec::new();
        for id in ids {
            let response = self
                .send(self.http.get(format!("{}/memories/{id}", self.base)))
                .await?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                continue;
            }
            let fact: Fact = serde_json::from_value(Self::json(response).await?)
                .map_err(|error| ProviderError(format!("hindsight 条目解析不了：{error}")))?;
            if fact.state.as_deref() == Some("invalidated") {
                continue;
            }
            alive.push(fact.into_hit());
        }
        Ok(alive)
    }

    async fn ingest(&self, batch: &EvidenceBatch<'_>) -> Result<(), ProviderError> {
        self.ensure_bank().await?;
        let timestamp = batch.at.to_string();
        let response = self
            .send(
                self.http
                    .post(format!("{}/memories", self.base))
                    .json(&json!({
                        "items": [{
                            "content": batch.text,
                            "timestamp": timestamp,
                            "context": "komo 会话的一段：用户的话、工具结果与助手的回答",
                            "document_id": batch.key,
                            "tags": [scope_tag(&batch.scope)],
                            "metadata": { "session": batch.session.as_str() },
                        }],
                        "async": true,
                    })),
            )
            .await?;
        Self::json(response).await.map(|_| ())
    }

    async fn list(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<ProviderPage, ProviderError> {
        #[derive(Deserialize)]
        struct Listed {
            items: Vec<Fact>,
            #[serde(default)]
            total: u64,
        }
        let mut params = vec![
            ("state", "valid".to_string()),
            ("limit", limit.to_string()),
            ("offset", offset.to_string()),
        ];
        if !query.trim().is_empty() {
            params.push(("q", query.trim().to_string()));
        }
        let url = reqwest::Url::parse_with_params(&format!("{}/memories/list", self.base), &params)
            .map_err(|error| ProviderError(format!("hindsight 地址拼不成：{error}")))?;
        let response = self.send(self.http.get(url)).await?;
        let listed: Listed = serde_json::from_value(Self::json(response).await?)
            .map_err(|error| ProviderError(format!("hindsight 清单解析不了：{error}")))?;
        Ok(ProviderPage {
            items: listed.items.into_iter().map(Fact::into_listed).collect(),
            total: listed.total,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Json;
    use axum::Router;
    use axum::extract::{Path, Query, State};
    use axum::http::StatusCode;
    use axum::routing::{get, post, put};
    use komo_kernel::protocol::config::ProviderMode;
    use komo_kernel::types::ids::SessionId;

    use super::*;

    /// 假的 Hindsight：记下请求体，按固定数据回话。
    #[derive(Clone, Default)]
    struct Fake {
        seen: Arc<Mutex<Vec<(String, Value)>>>,
    }

    async fn start() -> (Fake, Hindsight) {
        crate::service::test_support::harness::install_crypto();
        let fake = Fake::default();
        let base = "/v1/default/banks/komo";
        let app = Router::new()
            .route(base, put(bank))
            .route(&format!("{base}/memories"), post(retain))
            .route(&format!("{base}/memories/recall"), post(recall))
            .route(&format!("{base}/memories/list"), get(list))
            .route(&format!("{base}/memories/{{id}}"), get(memory))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let hindsight = Hindsight::new(&HindsightConfig {
            url,
            bank: "komo".into(),
            mode: ProviderMode::Shadow,
            min_reranker: 0.4,
        });
        (fake, hindsight)
    }

    async fn bank(State(fake): State<Fake>, Json(body): Json<Value>) -> Json<Value> {
        fake.seen.lock().unwrap().push(("bank".into(), body));
        Json(json!({ "bank_id": "komo" }))
    }

    async fn retain(State(fake): State<Fake>, Json(body): Json<Value>) -> Json<Value> {
        fake.seen.lock().unwrap().push(("retain".into(), body));
        Json(json!({ "success": true, "bank_id": "komo", "items_count": 1, "async": true }))
    }

    async fn recall(State(fake): State<Fake>, Json(body): Json<Value>) -> Json<Value> {
        fake.seen.lock().unwrap().push(("recall".into(), body));
        Json(json!({ "results": [
            { "id": "a", "text": "发布工单 Platform 选 saas", "type": "world",
              "tags": ["scope:personal"], "scores": { "final": 1.2 }, "brand_new": 1 },
            { "id": "b", "text": "私有的", "tags": ["scope:agent:helper"] },
            { "id": "c", "text": "第三条" },
        ]}))
    }

    async fn list(
        State(fake): State<Fake>,
        Query(params): Query<std::collections::HashMap<String, String>>,
    ) -> Json<Value> {
        fake.seen
            .lock()
            .unwrap()
            .push(("list".into(), json!(params)));
        Json(json!({ "items": [
            { "id": "a", "text": "发布工单 Platform 选 saas", "fact_type": "world",
              "date": "2026-11-18T08:08:00.030000+00:00", "tags": ["scope:personal"], "state": "valid" },
            { "id": "b", "text": "助手的私事", "fact_type": "observation", "date": "不是时间",
              "tags": ["scope:agent:helper"] },
        ], "total": 42, "limit": 2, "offset": 10 }))
    }

    async fn memory(Path(id): Path<String>) -> (StatusCode, Json<Value>) {
        match id.as_str() {
            "a" => (
                StatusCode::OK,
                Json(json!({ "id": "a", "text": "发布工单 Platform 选 saas", "state": "valid" })),
            ),
            "b" => (
                StatusCode::OK,
                Json(json!({ "id": "b", "text": "私有的", "state": "invalidated" })),
            ),
            _ => (
                StatusCode::NOT_FOUND,
                Json(json!({ "detail": "not found" })),
            ),
        }
    }

    #[tokio::test]
    async fn recall_maps_scopes_to_tags_and_reads_scores_and_scopes_back() {
        let (fake, hindsight) = start().await;
        let hits = hindsight.recall("发布工单", &[], 2).await.unwrap();
        assert_eq!(hits.len(), 2, "按 limit 截");
        assert_eq!(hits[0].id, "a");
        assert_eq!(hits[0].score, Some(1.2));
        assert_eq!(hits[0].scope, MemoryScope::Personal);
        assert_eq!(
            hits[1].scope,
            MemoryScope::Agent {
                agent_id: "helper".into()
            }
        );
        let helper = MemoryScope::Agent {
            agent_id: "helper".into(),
        };
        hindsight.recall("x", &[helper], 5).await.unwrap();
        let seen = fake.seen.lock().unwrap().clone();
        assert_eq!(
            seen[0].1["tags"],
            json!(["scope:personal"]),
            "共享召回只查 personal"
        );
        assert_eq!(seen[1].1["tags"], json!(["scope:agent:helper"]));
        assert_eq!(seen[1].1["tags_match"], "any");
        assert_eq!(
            seen[1].1["min_scores"]["reranker"],
            json!(0.4),
            "重排下限取自配置"
        );
    }

    #[tokio::test]
    async fn current_drops_invalidated_and_deleted_facts() {
        let (_fake, hindsight) = start().await;
        let alive = hindsight
            .current(&["a".into(), "b".into(), "gone".into()])
            .await
            .unwrap();
        let ids: Vec<&str> = alive.iter().map(|hit| hit.id.as_str()).collect();
        assert_eq!(ids, vec!["a"]);
    }

    #[tokio::test]
    async fn ingest_creates_the_bank_once_and_keys_the_window() {
        let (fake, hindsight) = start().await;
        let session = SessionId::from_raw("sess-1");
        for _ in 0..2 {
            hindsight
                .ingest(&EvidenceBatch {
                    key: "sess-1:run-1".into(),
                    session: &session,
                    scope: MemoryScope::Personal,
                    text: "[用户] 以后都选 saas",
                    at: jiff::Timestamp::UNIX_EPOCH,
                })
                .await
                .unwrap();
        }
        let seen = fake.seen.lock().unwrap().clone();
        let kinds: Vec<&str> = seen.iter().map(|(kind, _)| kind.as_str()).collect();
        assert_eq!(kinds, vec!["bank", "retain", "retain"], "bank 只建一次");
        let item = &seen[1].1["items"][0];
        assert_eq!(item["document_id"], "sess-1:run-1");
        assert_eq!(item["tags"], json!(["scope:personal"]));
        assert_eq!(item["content"], "[用户] 以后都选 saas");
        assert_eq!(seen[1].1["async"], true);
    }

    #[tokio::test]
    async fn list_asks_for_valid_facts_and_reads_kind_time_and_scope() {
        let (fake, hindsight) = start().await;
        let page = hindsight.list(" saas ", 2, 10).await.unwrap();
        assert_eq!(page.total, 42, "总数是全部符合的，不只这一页");
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].kind.as_deref(), Some("world"));
        assert_eq!(
            page.items[0].at.map(|at| at.as_second()),
            Some(1_794_989_280)
        );
        assert_eq!(page.items[1].at, None, "读不出来的时间不编");
        assert_eq!(
            page.items[1].scope,
            MemoryScope::Agent {
                agent_id: "helper".into()
            }
        );
        hindsight.list("  ", 50, 0).await.unwrap();
        let seen = fake.seen.lock().unwrap().clone();
        assert_eq!(
            seen[0].1,
            json!({ "state": "valid", "limit": "2", "offset": "10", "q": "saas" })
        );
        assert!(seen[1].1.get("q").is_none(), "空查询不带 q");
    }

    /// 对一台真的 Hindsight 走一遍：`KOMO_TEST_HINDSIGHT_URL=http://localhost:8888 cargo test -p komo-gateway
    /// --lib hindsight -- --ignored`。用一个临时 bank，跑完删掉。写入是排队提取的，召回最多等一分钟。
    #[tokio::test]
    #[ignore = "要一台在跑的 Hindsight"]
    async fn against_a_real_server() {
        crate::service::test_support::harness::install_crypto();
        let Ok(url) = std::env::var("KOMO_TEST_HINDSIGHT_URL") else {
            return;
        };
        let bank = format!("komo-test-{}", std::process::id());
        let hindsight = Hindsight::new(&HindsightConfig {
            url: url.clone(),
            bank: bank.clone(),
            mode: ProviderMode::Shadow,
            min_reranker: 0.0,
        });
        let session = SessionId::from_raw("sess-live");
        hindsight
            .ingest(&EvidenceBatch {
                key: "sess-live:run-1".into(),
                session: &session,
                scope: MemoryScope::Personal,
                text: "[用户] 以后发布工单 Platform 都选 saas。\n[助手] 好的。",
                at: jiff::Timestamp::now(),
            })
            .await
            .unwrap();
        let mut hits = Vec::new();
        for _ in 0..60 {
            hits = hindsight
                .recall("发布工单选哪个平台", &[], 5)
                .await
                .unwrap();
            if !hits.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let ids: Vec<String> = hits.iter().map(|hit| hit.id.clone()).collect();
        let current = hindsight.current(&ids).await.unwrap();
        let private = hindsight
            .recall(
                "发布工单选哪个平台",
                &[MemoryScope::Agent {
                    agent_id: "helper".into(),
                }],
                5,
            )
            .await
            .unwrap();
        let _ = reqwest::Client::new()
            .delete(format!(
                "{}/v1/default/banks/{bank}",
                url.trim_end_matches('/')
            ))
            .send()
            .await;
        assert!(!hits.is_empty(), "一分钟内没召回到");
        assert!(hits[0].text.contains("saas"), "{hits:?}");
        assert_eq!(hits[0].scope, MemoryScope::Personal);
        assert_eq!(current.len(), hits.len(), "刚写入的都还有效");
        assert!(private.is_empty(), "别的作用域查不到：{private:?}");
    }
}
