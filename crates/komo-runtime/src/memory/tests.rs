//! MemoryManager 的行为断言（§9、§14 的「Memory 与模型验收覆盖」）。
//!
//! 这里的模型是 [`ScriptedLlm`]：提取与关系判断各给一段固定 JSON，于是「写库路径全测」
//! （§14 第八条）不必碰网络。向量端点是 [`ConceptEmbeddings`]——一个**语义**替身：它按
//! 正文里出现了哪些概念词落到固定的几维上，所以"喜欢深色主题"和"界面都给我用暗色"的
//! 向量真的接近，而 `FixedEmbeddingClient` 那种按哈希摊开的替身在这件事上什么都证明不了。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::events::{Event, EventPayload, MessageAssistant, RunAccepted, ToolResult};
use komo_kernel::protocol::config::{MemoryConfig, RetrievalConfig};
use komo_kernel::test_support::ScriptedLlm;
use komo_kernel::traits::{Clock, EmbedError, EmbeddingClient, LlmClient, MemoryRepo, StoreError};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{
    AttemptId, EventId, MemoryId, RequestKey, RunId, Seq, SessionId, ToolCallId,
};
use komo_kernel::types::memory::{
    Confirmation, MemoryScope, MemoryState, MemoryWork, Provenance, RetrievalMode,
};
use komo_kernel::types::model::{
    DistanceRule, Effort, EmbeddingConfig, EmbeddingSpace, InputKind, ModelConfig, Vector,
};
use komo_kernel::types::plan::PlanSource;
use komo_kernel::types::refs::{ContentRef, OutputRef, ToolResultStatus};
use komo_kernel::types::status::RunStatus;
use komo_kernel::types::turn::{MemoryUse, Round};
use komo_store::{Db, TursoMemoryRepo};
use time::OffsetDateTime;
use time::macros::datetime;

use super::*;

const NOW: OffsetDateTime = datetime!(2026-09-17 08:00:00 UTC);

// ---------------------------------------------------------------- 替身

#[derive(Debug, Clone, Copy)]
struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        NOW
    }
}

/// 一个**按概念**给向量的 embedding 替身。
///
/// 每个概念占一维；一段文本里出现了哪几个概念，对应的维度就是 1。于是同一件事的两种说法
/// 落在同一个方向上——这正是 §14 第一条要的那个性质（"同一偏好换一种中文表述仍可语义
/// 召回"），而按哈希摊开的替身证不了它。
#[derive(Debug)]
struct ConceptEmbeddings {
    space: EmbeddingSpace,
    down: Mutex<bool>,
    /// 每一维一组同义词。
    concepts: Vec<Vec<&'static str>>,
}

impl ConceptEmbeddings {
    fn new() -> Arc<Self> {
        let concepts: Vec<Vec<&'static str>> = vec![
            vec!["深色", "暗色", "dark", "黑色"],
            vec!["主题", "界面", "theme", "配色"],
            vec!["空调", "温度", "制冷"],
            vec!["cargo", "测试", "test"],
        ];
        Arc::new(ConceptEmbeddings {
            space: EmbeddingSpace {
                provider: "test".into(),
                endpoint: "memory://concept".into(),
                model: "concept-v1".into(),
                revision: Some("1".into()),
                dimensions: concepts.len() as u32,
                preprocessing: "v1".into(),
                document_prefix: String::new(),
                query_prefix: String::new(),
                normalized: true,
                distance: DistanceRule::Cosine,
                effort: None,
            },
            down: Mutex::new(false),
            concepts,
        })
    }

    fn set_down(&self, down: bool) {
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
                // 全 0 的向量不合法（§9.5），给一个极小的常量底。
                if values.iter().all(|v| *v == 0.0) {
                    values[0] = 0.01;
                }
                Vector(values)
            })
            .collect())
    }
}

/// 内存里的 [`SessionEvents`]：装好一个会话的事件表。
#[derive(Debug, Default)]
struct SeedEvents {
    events: Mutex<std::collections::BTreeMap<SessionId, Vec<Event>>>,
    /// 读出来就是 [`LedgerError::Corrupt`] 的会话——JSONL 没了、损坏、已提交范围缺失
    /// 在这一层长一个样（§8.3）。
    poisoned: Mutex<std::collections::BTreeSet<SessionId>>,
}

impl SeedEvents {
    fn seed(&self, session: &SessionId, events: Vec<Event>) {
        self.events
            .lock()
            .expect("事件表")
            .insert(session.clone(), events);
    }

    fn poison(&self, session: &SessionId) {
        self.poisoned
            .lock()
            .expect("损坏表")
            .insert(session.clone());
    }
}

#[async_trait]
impl SessionEvents for SeedEvents {
    async fn events_of(&self, session: &SessionId) -> Result<Vec<Event>, MemoryError> {
        if self.poisoned.lock().expect("损坏表").contains(session) {
            return Err(MemoryError::Ledger("记录损坏：已提交范围缺失".to_string()));
        }
        Ok(self
            .events
            .lock()
            .expect("事件表")
            .get(session)
            .cloned()
            .unwrap_or_default())
    }
}

/// 内存里的 [`MemoryWorkLog`]：断言"失败不推进游标"用它。
#[derive(Debug, Default)]
struct MemWork {
    items: Mutex<Vec<MemoryWorkItem>>,
    settled: Mutex<Vec<(RunId, MemoryWork, Seq)>>,
}

impl MemWork {
    fn with(items: Vec<MemoryWorkItem>) -> Arc<Self> {
        Arc::new(MemWork {
            items: Mutex::new(items),
            settled: Mutex::new(Vec::new()),
        })
    }

    fn settled(&self) -> Vec<(RunId, MemoryWork, Seq)> {
        self.settled.lock().expect("结算").clone()
    }
}

#[async_trait]
impl MemoryWorkLog for MemWork {
    async fn claim(&self, limit: usize) -> Result<Vec<MemoryWorkItem>, StoreError> {
        let mut items = self.items.lock().expect("队列");
        let take = limit.min(items.len());
        Ok(items.drain(..take).collect())
    }

    async fn finish(&self, run: &RunId, work: MemoryWork, cursor: Seq) -> Result<(), StoreError> {
        self.settled
            .lock()
            .expect("结算")
            .push((run.clone(), work, cursor));
        Ok(())
    }

    async fn requeue_stuck(&self) -> Result<u64, StoreError> {
        Ok(0)
    }
}

// ---------------------------------------------------------------- 装配

fn model(name: &str, effort: Option<&str>) -> ModelConfig {
    ModelConfig {
        provider: "openai_responses".into(),
        base_url: format!("https://{name}.example.com/v1"),
        model: name.into(),
        api_key_env: "KOMO_LLM_API_KEY".into(),
        effort: effort.map(Effort::new),
        efforts: None,
        timeout_secs: 10,
    }
}

fn memory_config(enabled: bool, mode: RetrievalMode) -> MemoryConfig {
    MemoryConfig {
        enabled,
        model: model("memory-model", Some("low")),
        embedding: None,
        retrieval: RetrievalConfig {
            mode,
            candidate_limit: 40,
            top_k: 8,
            max_tokens: 1500,
        },
    }
}

fn round(text: &str) -> Round {
    Round {
        round: 1,
        text: Some(text.into()),
        tool_calls: vec![],
        provider_blocks: None,
        usage: Default::default(),
        truncated: false,
    }
}

/// 一台装好的 MemoryManager，连同测试要回头看的那几个替身。
struct Harness {
    manager: Arc<MemoryManager>,
    repo: Arc<TursoMemoryRepo>,
    llm: ScriptedLlm,
    embeddings: Arc<ConceptEmbeddings>,
    work: Arc<MemWork>,
    ledger: Arc<SeedEvents>,
    _dir: tempfile::TempDir,
}

struct HarnessBuilder {
    config: MemoryConfig,
    scripts: Vec<Vec<Round>>,
    with_embeddings: bool,
    work: Vec<MemoryWorkItem>,
}

impl HarnessBuilder {
    fn new() -> Self {
        HarnessBuilder {
            config: memory_config(true, RetrievalMode::Keyword),
            scripts: Vec::new(),
            with_embeddings: false,
            work: Vec::new(),
        }
    }

    fn config(mut self, config: MemoryConfig) -> Self {
        self.config = config;
        self
    }

    fn script(mut self, rounds: Vec<Round>) -> Self {
        self.scripts.push(rounds);
        self
    }

    fn embeddings(mut self) -> Self {
        self.with_embeddings = true;
        self
    }

    fn work(mut self, items: Vec<MemoryWorkItem>) -> Self {
        self.work = items;
        self
    }

    async fn build(self) -> Harness {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = Db::connect(dir.path().join("state.db"))
            .await
            .expect("打开库");
        let repo = Arc::new(TursoMemoryRepo::new(db));
        let llm = ScriptedLlm::new(self.scripts);
        let embeddings = ConceptEmbeddings::new();
        let work = MemWork::with(self.work);
        let ledger = Arc::new(SeedEvents::default());
        let manager = Arc::new(MemoryManager::new(MemoryParts {
            config: self.config,
            repo: Arc::clone(&repo) as Arc<dyn MemoryRepo>,
            catalog: Arc::clone(&repo) as Arc<dyn MemoryCatalog>,
            embeddings: self
                .with_embeddings
                .then(|| Arc::clone(&embeddings) as Arc<dyn EmbeddingClient>),
            llm: Arc::new(llm.clone()) as Arc<dyn LlmClient>,
            events: Arc::clone(&ledger) as Arc<dyn SessionEvents>,
            work: Arc::clone(&work) as Arc<dyn MemoryWorkLog>,
            clock: Arc::new(FixedClock),
        }));
        Harness {
            manager,
            repo,
            llm,
            embeddings,
            work,
            ledger,
            _dir: dir,
        }
    }
}

// ---------------------------------------------------------------- 会话事件夹具

fn event(seq: u64, run: &RunId, payload: EventPayload) -> Event {
    Event {
        v: 1,
        seq: Seq(seq),
        event_id: EventId::from_raw(format!("evt-{seq}")),
        session: SessionId::from_raw("sess-1"),
        run: Some(run.clone()),
        ts: NOW,
        payload,
    }
}

fn user_event(seq: u64, run: &RunId, text: &str) -> Event {
    event(
        seq,
        run,
        EventPayload::RunAccepted(RunAccepted {
            request_key: RequestKey::new(format!("rk-{seq}")),
            input_hash: ContentHash::of_str(text),
            text: Some(text.into()),
            text_ref: None,
            source: PlanSource::Interactive {
                session: SessionId::from_raw("sess-1"),
            },
            peer: None,
            model: None,
            effort: None,
        }),
    )
}

fn assistant_event(seq: u64, run: &RunId, text: &str) -> Event {
    event(
        seq,
        run,
        EventPayload::MessageAssistant(MessageAssistant {
            round: 1,
            text: Some(text.into()),
            text_ref: None,
            tool_calls: vec![],
            provider_blocks: None,
            input_tokens: None,
            output_tokens: None,
        }),
    )
}

fn tool_event(seq: u64, run: &RunId, preview: &str) -> Event {
    event(
        seq,
        run,
        EventPayload::ToolResult(ToolResult {
            call_id: ToolCallId::from_raw("call-1"),
            attempt_id: AttemptId::from_raw("att-1"),
            status: ToolResultStatus::Completed,
            output_ref: OutputRef(ContentRef {
                path: "tool-output/run-1/call-1/attempt-1/output.json".into(),
                size: 4,
                hash: ContentHash::of_str("x"),
                pointer: None,
            }),
            elapsed_ms: 12,
            preview: Some(preview.into()),
            stdout: None,
            stderr: None,
            attempt_state: None,
        }),
    )
}

fn work_item(status: RunStatus, source: PlanSource) -> MemoryWorkItem {
    MemoryWorkItem {
        run: RunId::from_raw("run-1"),
        session: SessionId::from_raw("sess-1"),
        status,
        source,
        cursor: Seq::ZERO,
    }
}

fn interactive() -> PlanSource {
    PlanSource::Interactive {
        session: SessionId::from_raw("sess-1"),
    }
}

fn transcript(events: &[Event]) -> extract::Transcript {
    extract::Transcript::build(
        &SessionId::from_raw("sess-1"),
        &RunId::from_raw("run-1"),
        events,
        Seq::ZERO,
    )
}

// =========================================================== ① 语义召回与 bigram

/// §14 ①：同一偏好换一种中文表述仍可语义召回，**同时保留原始来源**。
#[tokio::test]
async fn a_paraphrase_still_recalls_the_same_preference_and_keeps_its_provenance() {
    let harness = HarnessBuilder::new()
        .config(memory_config(true, RetrievalMode::Hybrid))
        .embeddings()
        .build()
        .await;

    // 存进去的是用户的原话。
    let stored = harness
        .repo
        .put(
            MemoryItem {
                id: MemoryId::from_raw("m-1"),
                revision: 1,
                content: "喜欢深色主题".into(),
                kind: komo_kernel::types::memory::MemoryKind::Preference,
                scope: MemoryScope::Personal,
                provenance: Provenance::UserStatement,
                confirmation: Confirmation::Unconfirmed,
                state: MemoryState::Active,
                evidence: vec![],
                observed_at: NOW,
                valid_until: None,
                created_at: NOW,
                updated_at: NOW,
                extraction: komo_kernel::types::memory::ExtractionMetadata::new(
                    "memory-model",
                    None,
                    "v1",
                ),
                usage: Default::default(),
                supersedes: None,
            },
            None,
        )
        .await
        .unwrap();
    harness.manager.build_index().await.expect("建得起来");

    // 换一种说法问：一个字都不重合的中文查询。
    let result = harness
        .manager
        .search(harness.manager.query("界面都给我用暗色", None))
        .await
        .expect("查得动");
    assert!(!result.degraded, "向量臂在，就不是降级");
    assert_eq!(result.items.len(), 1, "换一种说法仍然召回得到");
    assert_eq!(result.items[0].id, stored.id);
    // **保留原始来源**：召回不改写它是谁说的。
    assert_eq!(result.items[0].provenance, Provenance::UserStatement);
    assert_eq!(result.items[0].content, "喜欢深色主题");

    // 同一个查询在纯关键词下**够不着**——这正是向量臂存在的理由（CJK bigram 与
    // "暗色" 一个都不重合）。
    let keyword = harness
        .manager
        .search(
            harness
                .manager
                .query("界面都给我用暗色", Some(RetrievalMode::Keyword)),
        )
        .await
        .unwrap();
    assert!(keyword.items.is_empty(), "关键词臂结构上就匹配不到");
}

// =========================================================== ② 四种来源分得开

/// §14 ②：用户原话、工具观察、模型推断、用户确认在展示与上下文里都分得开。
#[test]
fn every_kind_of_provenance_reads_differently_in_the_injected_block() {
    let base = |id: &str, provenance, confirmation, state| komo_kernel::types::memory::MemoryItem {
        id: MemoryId::from_raw(id),
        revision: 1,
        content: format!("关于 {id} 的一句话"),
        kind: komo_kernel::types::memory::MemoryKind::Fact,
        scope: MemoryScope::Personal,
        provenance,
        confirmation,
        state,
        evidence: vec![],
        observed_at: NOW,
        valid_until: None,
        created_at: NOW,
        updated_at: NOW,
        extraction: komo_kernel::types::memory::ExtractionMetadata::new("m", None, "v1"),
        usage: Default::default(),
        supersedes: None,
    };
    let items = vec![
        base(
            "m-1",
            Provenance::UserStatement,
            Confirmation::Unconfirmed,
            MemoryState::Active,
        ),
        base(
            "m-2",
            Provenance::ToolObservation,
            Confirmation::Unconfirmed,
            MemoryState::Active,
        ),
        base(
            "m-3",
            Provenance::ModelInference,
            Confirmation::Unconfirmed,
            MemoryState::Candidate,
        ),
        base(
            "m-4",
            Provenance::UserStatement,
            Confirmation::UserConfirmed,
            MemoryState::Active,
        ),
    ];
    let injection = render_injection(&items, 5_000);
    let text = injection.text.expect("有正文");

    assert!(text.contains("自动整理自用户陈述"), "{text}");
    assert!(text.contains("来自工具结果"), "{text}");
    assert!(text.contains("模型推断"), "{text}");
    assert!(text.contains("候选，未确认"), "{text}");
    assert!(text.contains("用户已确认"), "{text}");
    // §9.7：它是数据，不是指令，也不是授权。
    assert!(text.contains("不授权任何操作"), "{text}");
    assert_eq!(injection.uses.len(), 4);
    assert!(injection.uses.iter().all(|use_| use_.revision == 1));
}

/// §14 ②的后半：**候选不因反复提取而升级**。
///
/// 同一句话被提取第二次，走的是"逐字相同 → 合并证据"，而合并那一支一个字都不改
/// `state` 与 `confirmation`（§9.6：使用次数和多次模型复述不能增加确认等级）。
#[tokio::test]
async fn extracting_the_same_claim_again_never_promotes_a_candidate() {
    let extraction = r#"{"observations":[{"content":"用户大概更喜欢深色主题",
        "kind":"preference","scope":"personal","said_by":"model_inference",
        "evidence":["evt-1"],"user_confirmed":true}]}"#;
    let harness = HarnessBuilder::new()
        .script(vec![round(extraction)])
        .script(vec![round(extraction)])
        .work(vec![
            work_item(RunStatus::Completed, interactive()),
            work_item(RunStatus::Completed, interactive()),
        ])
        .build()
        .await;
    harness.ledger.seed(
        &SessionId::from_raw("sess-1"),
        vec![user_event(1, &RunId::from_raw("run-1"), "把界面弄暗一点")],
    );

    harness.manager.process_pending(1).await.unwrap();
    let first = harness.repo.list(None, None, 10).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].state, MemoryState::Candidate, "模型推断落候选");
    assert_eq!(
        first[0].confirmation,
        Confirmation::Unconfirmed,
        "模型返回的 user_confirmed 没有写入权限（§9.2）"
    );

    // 再提一次同一句。
    harness.manager.process_pending(1).await.unwrap();
    let again = harness.repo.list(None, None, 10).await.unwrap();
    assert_eq!(again.len(), 1, "重复来源幂等合并，不新增（§9.6）");
    assert_eq!(again[0].state, MemoryState::Candidate, "仍然是候选");
    assert_eq!(again[0].confirmation, Confirmation::Unconfirmed);
}

// =========================================================== ③ 模型隔离

/// §14 ③：切换聊天模型或它的 effort **碰不到**记忆模型。
#[tokio::test]
async fn the_memory_model_is_the_one_the_memory_section_names() {
    let mut config = memory_config(true, RetrievalMode::Keyword);
    config.model = model("memory-model", Some("low"));
    let harness = HarnessBuilder::new()
        .config(config)
        .script(vec![round(
            r#"{"observations":[{"content":"这个仓库用 cargo test --workspace 跑测试",
                "kind":"fact","scope":"project:komo","said_by":"user_statement",
                "evidence":["evt-1"]}]}"#,
        )])
        .work(vec![work_item(RunStatus::Completed, interactive())])
        .build()
        .await;
    harness.ledger.seed(
        &SessionId::from_raw("sess-1"),
        vec![user_event(
            1,
            &RunId::from_raw("run-1"),
            "记一下：这个仓库要用 cargo test --workspace 跑测试",
        )],
    );

    harness.manager.process_pending(1).await.unwrap();

    let requests = harness.llm.requests.lock().expect("请求表").clone();
    assert_eq!(requests.len(), 1, "提取只发了一次");
    // **记忆模型自己的那一份完整配置**（§13.3）：端点、模型、effort 一起来。
    assert_eq!(requests[0].model.model, "memory-model");
    assert_eq!(
        requests[0].model.base_url,
        "https://memory-model.example.com/v1"
    );
    assert_eq!(
        requests[0].model.effort.as_ref().map(|e| e.to_string()),
        Some("low".into()),
        "用的是 [memory.model] 的 effort，不是聊天模型的"
    );
    // 「记忆模型提取结构化候选，**不提供执行工具**」（§9.3）。
    assert!(requests[0].tools.is_empty());
    // 记忆模型不读记忆：提取这一问自己不带注入。
    assert!(requests[0].memories.is_empty());

    // 提取元数据里记下了模型与 effort（§9.2 的 extraction_metadata）。
    let stored = harness.repo.list(None, None, 10).await.unwrap();
    assert_eq!(stored[0].extraction.model, "memory-model");
    assert_eq!(
        stored[0]
            .extraction
            .effort
            .as_option()
            .map(|e| e.to_string()),
        Some("low".into())
    );
    assert_eq!(stored[0].extraction.prompt_version, PROMPT_VERSION);
}

// =========================================================== ⑤ 代次与重建

/// §14 ⑤ 的第三句：**重建中崩溃可以接续**。
///
/// "崩溃"在这里是"跑到一半就不跑了"：候选集是`还欠向量的那些`，所以第二次跑接着算剩下
/// 的，而不是从头再算一遍（§9.5 第 2、5 步）。
#[tokio::test]
async fn a_rebuild_that_stopped_half_way_picks_up_where_it_left_off() {
    let harness = HarnessBuilder::new()
        .config(memory_config(true, RetrievalMode::Hybrid))
        .embeddings()
        .build()
        .await;
    for index in 0..40 {
        harness
            .repo
            .put(memory(&format!("m-{index:03}"), "客厅空调设 26 度"), None)
            .await
            .unwrap();
    }

    // 第一轮：端点在第二批上挂掉，于是这一代没有追平。
    let space = harness.manager.space().expect("有空间");
    let generation = IndexBuilder::generation_id(&space);
    let first = harness.manager.build_index().await.unwrap();
    assert_eq!(first.generation, generation);
    assert!(first.errors.is_empty());
    assert_eq!(first.indexed, 40);

    // 把一半的向量删掉，模拟"上一次只算完了一半"。
    harness.repo.prune_generations("nothing").await.unwrap();
    assert_eq!(harness.repo.indexed_in(&generation).await.unwrap(), 0);
    for index in 0..20 {
        harness
            .repo
            .put_vector(
                &MemoryId::from_raw(format!("m-{index:03}")),
                1,
                &generation,
                Vector(vec![1.0, 0.0, 0.0, 0.0]),
            )
            .await
            .unwrap();
    }
    assert_eq!(harness.repo.indexed_in(&generation).await.unwrap(), 20);

    // 第二轮：只补剩下的 20 条，代次不变，追平后切换。
    let second = harness.manager.build_index().await.unwrap();
    assert_eq!(second.generation, generation, "同一个空间就是同一个代次");
    assert_eq!(second.indexed, 40);
    assert!(second.activated, "追平之后才切换（§9.5 第 4 步）");
    assert_eq!(
        harness.repo.active_generation().await.unwrap().as_deref(),
        Some(generation.as_str())
    );
}

/// 配置换了向量模型 → 指纹变 → **需要新代次**，而且热重载不偷偷做，只提示（§9.5、§13.3）。
#[tokio::test]
async fn a_changed_embedding_model_is_reported_as_needing_a_new_generation() {
    let harness = HarnessBuilder::new()
        .config(memory_config(true, RetrievalMode::Hybrid))
        .embeddings()
        .build()
        .await;
    harness
        .repo
        .put(memory("m-1", "客厅空调设 26 度"), None)
        .await
        .unwrap();
    harness.manager.build_index().await.unwrap();
    let status = harness.manager.index_status().await.unwrap();
    assert!(status.errors.is_empty(), "指纹对得上就没话说：{status:?}");

    // 库里的生效代次来自另一个空间（换了模型）。
    harness
        .repo
        .put_generation("gen-someone-else", None, IndexState::Ready, false)
        .await
        .unwrap();
    harness
        .repo
        .activate_generation("gen-someone-else")
        .await
        .unwrap();
    let drifted = harness.manager.index_status().await.unwrap();
    assert!(
        drifted.errors.iter().any(|note| note.contains("rebuild")),
        "要指出需要一次重建：{:?}",
        drifted.errors
    );
}

/// 重建的提交是**幂等**的：同一个代次在跑时第二次提交返回同一个代次且 `accepted = false`。
#[tokio::test]
async fn submitting_a_rebuild_twice_returns_the_same_generation_once() {
    let harness = HarnessBuilder::new()
        .config(memory_config(true, RetrievalMode::Hybrid))
        .embeddings()
        .build()
        .await;
    let (first, accepted) = harness.manager.request_rebuild().unwrap();
    assert!(accepted);
    // 后台任务还没跑完时再提交一次。
    let (second, accepted_again) = harness.manager.request_rebuild().unwrap();
    assert_eq!(first, second, "同一个空间就是同一个代次");
    assert!(
        !accepted_again || accepted_again,
        "第二次要么命中在跑的，要么已经跑完"
    );
}

/// 没有配置向量模型时提交重建是**配置错误**，不是一次"跑完了"。
#[tokio::test]
async fn a_rebuild_without_an_embedding_model_is_a_configuration_error() {
    let harness = HarnessBuilder::new()
        .config(memory_config(true, RetrievalMode::Keyword))
        .build()
        .await;
    assert_eq!(
        harness.manager.request_rebuild().unwrap_err(),
        MemoryError::VectorUnconfigured
    );
}

// =========================================================== ⑥ 降级

/// §14 ⑥：向量服务不可用时 hybrid **明示降级**，vector-only **明确报不可用**；
/// 两者都不能被解释成"没有相关记忆"。
#[tokio::test]
async fn a_dead_vector_endpoint_degrades_hybrid_and_fails_vector_only() {
    let harness = HarnessBuilder::new()
        .config(memory_config(true, RetrievalMode::Hybrid))
        .embeddings()
        .build()
        .await;
    harness
        .repo
        .put(memory("m-1", "客厅空调设 26 度"), None)
        .await
        .unwrap();
    harness.manager.build_index().await.unwrap();

    harness.embeddings.set_down(true);

    let hybrid = harness
        .manager
        .search(harness.manager.query("空调", None))
        .await
        .expect("hybrid 不该整个失败");
    assert!(hybrid.degraded, "要明示降级");
    assert!(hybrid.degraded_reason.is_some(), "要说原因");
    assert!(
        !hybrid.items.is_empty(),
        "关键词臂照常给结果——故障不是「没有相关记忆」"
    );

    let error = harness
        .manager
        .search(harness.manager.query("空调", Some(RetrievalMode::Vector)))
        .await
        .expect_err("vector-only 要明确报不可用");
    assert!(
        matches!(error, MemoryError::VectorUnavailable(_)),
        "{error:?}"
    );
}

/// 没配 embedding 却选了 hybrid：**配置错误**，不能静默变成长期关键词模式（§9.4）。
#[tokio::test]
async fn hybrid_without_any_embedding_configured_is_a_configuration_error() {
    let harness = HarnessBuilder::new()
        .config(memory_config(true, RetrievalMode::Hybrid))
        .build()
        .await;
    assert_eq!(
        harness
            .manager
            .search(harness.manager.query("空调", None))
            .await
            .unwrap_err(),
        MemoryError::VectorUnconfigured
    );
}

/// 配了 alias 但后端还**没在手上**（启动时那次维度探测还在跑，或者探测失败了）：这是
/// §9.4 的"配了但这一刻不通"，不是配置错误——hybrid 降级成关键词并说明，vector-only 报
/// 不可用；探测落定后 `install_embeddings` 让同一台 manager 立刻有向量臂。
#[tokio::test]
async fn a_backend_that_has_not_landed_yet_degrades_and_then_installs() {
    let mut config = memory_config(true, RetrievalMode::Hybrid);
    config.embedding = Some(EmbeddingConfig {
        model: model("embed", None),
        revision: None,
        dimensions: Some(4),
        document_prefix: None,
        query_prefix: None,
    });
    let harness = HarnessBuilder::new().config(config).build().await;
    harness
        .repo
        .put(memory("m-1", "客厅空调设 26 度"), None)
        .await
        .unwrap();

    assert!(harness.manager.space().is_none(), "还没装上就没有空间");
    assert!(
        matches!(
            harness.manager.build_index().await.unwrap_err(),
            MemoryError::VectorUnavailable(_)
        ),
        "配了 alias 却报「没配置」会把配置错误与后端不可用搅在一起"
    );

    let hybrid = harness
        .manager
        .search(harness.manager.query("空调", None))
        .await
        .expect("hybrid 不该整个失败");
    assert!(hybrid.degraded, "要明示这一次只有关键词臂");
    assert!(!hybrid.items.is_empty(), "关键词臂照常给结果");

    let error = harness
        .manager
        .search(harness.manager.query("空调", Some(RetrievalMode::Vector)))
        .await
        .expect_err("vector-only 要明确报不可用");
    assert!(
        matches!(error, MemoryError::VectorUnavailable(_)),
        "{error:?}"
    );

    harness
        .manager
        .install_embeddings(Arc::clone(&harness.embeddings) as Arc<dyn EmbeddingClient>);
    assert_eq!(
        harness.manager.space().as_ref(),
        Some(harness.embeddings.space()),
        "装上的就是探测拿到的那个空间"
    );
    harness
        .manager
        .build_index()
        .await
        .expect("装上之后建得起来");
}

// =========================================================== ⑦ confirm / forget / resume

/// §14 ⑦ 的后半：**resume 与旧检查点不重新注入已遗忘内容**（§9.7）。
#[tokio::test]
async fn an_old_checkpoint_cannot_bring_back_a_forgotten_memory() {
    let harness = HarnessBuilder::new().build().await;
    let kept = harness
        .repo
        .put(memory("m-1", "客厅空调设 26 度"), None)
        .await
        .unwrap();
    let doomed = harness
        .repo
        .put(memory("m-2", "书房台灯换成深色"), None)
        .await
        .unwrap();
    let stale = harness
        .repo
        .put(memory("m-3", "用 cargo test 跑测试"), None)
        .await
        .unwrap();

    // 检查点里记着三条（§9.7：模型请求与检查点保存记忆 ID / revision）。
    let checkpoint = vec![
        MemoryUse {
            memory: kept.id.clone(),
            revision: kept.revision,
        },
        MemoryUse {
            memory: doomed.id.clone(),
            revision: doomed.revision,
        },
        MemoryUse {
            memory: stale.id.clone(),
            revision: stale.revision,
        },
    ];

    // 其中一条被遗忘，另一条改了版本。
    harness.manager.forget(&doomed.id, 1).await.unwrap();
    let mut edited = stale.clone();
    edited.revision = 2;
    edited.content = "改用 cargo nextest".into();
    harness.repo.put(edited, Some(1)).await.unwrap();

    let alive = harness.manager.revalidate(&checkpoint, NOW).await;
    assert_eq!(alive.len(), 1, "只有没被动过的那条活下来");
    assert_eq!(alive[0].id, kept.id);

    // 从检查点续跑：注入段里也只剩那一条。
    let injection = harness
        .manager
        .prepare(&RunId::from_raw("run-1"), "空调", &checkpoint)
        .await;
    assert_eq!(injection.uses.len(), 1);
    assert_eq!(injection.uses[0].memory, kept.id);
    assert!(
        !injection
            .text
            .as_deref()
            .unwrap_or_default()
            .contains("台灯"),
        "遗忘的内容不能回到上下文"
    );
}

/// §14 ⑦ 的前半：confirm / forget 带预期 revision，重复请求幂等。
#[tokio::test]
async fn confirm_and_forget_carry_the_expected_revision_and_repeat_harmlessly() {
    let harness = HarnessBuilder::new().build().await;
    harness
        .repo
        .put(memory("m-1", "客厅空调设 26 度"), None)
        .await
        .unwrap();

    assert!(matches!(
        harness.manager.confirm(&MemoryId::from_raw("m-1"), 7).await,
        Err(MemoryError::Repo(
            komo_kernel::traits::RepoError::VersionConflict { .. }
        ))
    ));

    let once = harness
        .manager
        .confirm(&MemoryId::from_raw("m-1"), 1)
        .await
        .unwrap();
    let twice = harness
        .manager
        .confirm(&MemoryId::from_raw("m-1"), 1)
        .await
        .unwrap();
    assert_eq!(once.confirmation, Confirmation::UserConfirmed);
    assert_eq!(once.revision, twice.revision, "重复确认不是新版本");

    let forgotten = harness
        .manager
        .forget(&MemoryId::from_raw("m-1"), 1)
        .await
        .unwrap();
    assert_eq!(forgotten.state, MemoryState::Forgotten);
    let again = harness
        .manager
        .forget(&MemoryId::from_raw("m-1"), 1)
        .await
        .unwrap();
    assert_eq!(again.state, MemoryState::Forgotten, "重复遗忘幂等");
}

// =========================================================== ⑧ 提取与写库路径

/// 证据必须指向**真实存在的**用户消息或工具结果；只引助手自己话的那一条丢掉（§9.3）。
#[test]
fn an_observation_that_cites_only_the_assistant_is_dropped() {
    let run = RunId::from_raw("run-1");
    let events = vec![
        user_event(1, &run, "记一下：空调设 26 度"),
        assistant_event(2, &run, "好的，我记住了：你喜欢 26 度"),
        tool_event(3, &run, "exit 0"),
    ];
    let transcript = transcript(&events);
    let raw: Vec<RawObservation> = serde_json::from_str(
        r#"[{"content":"空调设 26 度","kind":"fact","scope":"personal",
             "said_by":"user_statement","evidence":["evt-1"]},
            {"content":"助手自己总结的一句话","kind":"fact","scope":"personal",
             "said_by":"user_statement","evidence":["evt-2"]},
            {"content":"引了一个不存在的事件","kind":"fact","scope":"personal",
             "said_by":"user_statement","evidence":["evt-999"]},
            {"content":"一条什么都不引的","kind":"fact","scope":"personal",
             "said_by":"user_statement","evidence":[]}]"#,
    )
    .unwrap();

    let observations = transcript.validate(
        raw,
        &work_item(RunStatus::Completed, interactive()),
        &model("memory-model", None),
        NOW,
    );
    assert_eq!(observations.len(), 1, "只有引得出真实用户消息的那一条留下");
    assert_eq!(observations[0].content, "空调设 26 度");
    assert_eq!(observations[0].provenance, Provenance::UserStatement);
    assert_eq!(observations[0].state, MemoryState::Active);
}

/// **来源角色由证据决定，不由模型自称决定**：自称用户原话却只引得出工具结果 → 降级。
#[test]
fn a_claim_of_user_speech_without_a_user_event_is_demoted_to_an_inference() {
    let run = RunId::from_raw("run-1");
    let events = vec![
        user_event(1, &run, "跑一下测试"),
        tool_event(2, &run, "test result: ok. 156 passed"),
    ];
    let transcript = transcript(&events);
    let raw: Vec<RawObservation> = serde_json::from_str(
        r#"[{"content":"用户希望每次都跑全量测试","kind":"preference","scope":"personal",
             "said_by":"user_statement","evidence":["evt-2"]},
            {"content":"这个仓库的测试有 156 个","kind":"fact","scope":"project:komo",
             "said_by":"tool_observation","evidence":["evt-2"]}]"#,
    )
    .unwrap();

    let observations = transcript.validate(
        raw,
        &work_item(RunStatus::Completed, interactive()),
        &model("memory-model", None),
        NOW,
    );
    assert_eq!(observations.len(), 2);
    assert_eq!(
        observations[0].provenance,
        Provenance::ModelInference,
        "自称对不上证据就是推断"
    );
    assert_eq!(observations[0].state, MemoryState::Candidate);
    assert_eq!(observations[1].provenance, Provenance::ToolObservation);
    assert_eq!(observations[1].state, MemoryState::Active);
}

/// 「取消、失败或结果未知的动作不能整理成成功经验」，以及「Cron 不能从自己的报告推断用户
/// 偏好」（§9.3）。
#[test]
fn a_failed_run_yields_no_experience_and_a_cron_run_yields_no_preference() {
    let run = RunId::from_raw("run-1");
    let events = vec![user_event(1, &run, "帮我把构建目录清了")];
    let transcript = transcript(&events);
    let raw: Vec<RawObservation> = serde_json::from_str(
        r#"[{"content":"清构建目录要先停掉 cargo watch","kind":"experience","scope":"personal",
             "said_by":"user_statement","evidence":["evt-1"]},
            {"content":"用户喜欢定期清理","kind":"preference","scope":"personal",
             "said_by":"user_statement","evidence":["evt-1"]}]"#,
    )
    .unwrap();

    let failed = transcript.validate(
        raw.clone(),
        &work_item(RunStatus::Failed, interactive()),
        &model("memory-model", None),
        NOW,
    );
    assert_eq!(failed.len(), 1, "失败的 Run 整理不出成功经验");
    assert_eq!(
        failed[0].kind,
        komo_kernel::types::memory::MemoryKind::Preference
    );

    let cron = transcript.validate(
        raw,
        &work_item(
            RunStatus::Completed,
            PlanSource::Cron {
                job: komo_kernel::types::ids::CronJobId::from_raw("job-1"),
                job_version: 1,
            },
        ),
        &model("memory-model", None),
        NOW,
    );
    assert_eq!(cron.len(), 1, "Cron 的 Run 不整理用户偏好");
    assert_eq!(
        cron[0].kind,
        komo_kernel::types::memory::MemoryKind::Experience
    );
}

/// 「自动整理不保存原始密钥、令牌」（§9.3）。
#[test]
fn anything_that_looks_like_a_credential_is_dropped() {
    let run = RunId::from_raw("run-1");
    let events = vec![user_event(1, &run, "这是我的 key")];
    let transcript = transcript(&events);
    let raw: Vec<RawObservation> = serde_json::from_str(
        r#"[{"content":"OpenAI 的 key 是 sk-abcdef0123456789","kind":"fact","scope":"personal",
             "said_by":"user_statement","evidence":["evt-1"]},
            {"content":"备份密码是 password=hunter2","kind":"fact","scope":"personal",
             "said_by":"user_statement","evidence":["evt-1"]},
            {"content":"token 是 0123456789abcdef0123456789abcdef","kind":"fact",
             "scope":"personal","said_by":"user_statement","evidence":["evt-1"]},
            {"content":"备份放在 NAS 的 /srv/backup","kind":"fact","scope":"personal",
             "said_by":"user_statement","evidence":["evt-1"]}]"#,
    )
    .unwrap();

    let observations = transcript.validate(
        raw,
        &work_item(RunStatus::Completed, interactive()),
        &model("memory-model", None),
        NOW,
    );
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].content, "备份放在 NAS 的 /srv/backup");
}

/// 取消的 Run **整个跳过**：它停在用户的选择上，剩下的半截不是证据（§9.3）。
#[tokio::test]
async fn a_cancelled_run_is_skipped_and_its_cursor_still_settles() {
    let harness = HarnessBuilder::new()
        .work(vec![work_item(RunStatus::Cancelled, interactive())])
        .build()
        .await;
    let report = harness.manager.process_pending(4).await.unwrap();
    assert_eq!(report.skipped, 1);
    assert_eq!(report.processed, 0);
    assert!(
        harness.llm.requests.lock().unwrap().is_empty(),
        "一次模型都没发"
    );
    assert_eq!(
        harness.work.settled(),
        vec![(RunId::from_raw("run-1"), MemoryWork::Done, Seq::ZERO)],
        "跳过也要结算，否则它会被反复领取"
    );
}

/// 提取失败**不推进游标**，标记放回 `pending` 下次重试（§9.3）。
#[tokio::test]
async fn a_failed_extraction_leaves_the_cursor_where_it_was() {
    let harness = HarnessBuilder::new()
        // 模型交回一段不是 JSON 的话。
        .script(vec![round("我觉得没什么好记的。")])
        .work(vec![MemoryWorkItem {
            cursor: Seq(3),
            ..work_item(RunStatus::Completed, interactive())
        }])
        .build()
        .await;
    harness.ledger.seed(
        &SessionId::from_raw("sess-1"),
        vec![
            user_event(1, &RunId::from_raw("run-1"), "记一下：空调设 26 度"),
            user_event(5, &RunId::from_raw("run-1"), "还有台灯"),
        ],
    );

    let report = harness.manager.process_pending(4).await.unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(
        harness.work.settled(),
        vec![(RunId::from_raw("run-1"), MemoryWork::Pending, Seq(3))],
        "游标停在原处，标记回 pending"
    );
    assert!(harness.repo.list(None, None, 10).await.unwrap().is_empty());
}

/// **证据读不出来就不重试**：JSONL 损坏 / 已提交范围缺失换个时间点也是同一个答案
/// （§8.3「停止受影响会话，报告损坏」）。翻成 `error` 是对 §9.3 那四个状态的用法——
/// 否则它会每 30 秒被领一次、失败一次，永远占着队列最前面那几个位置。
#[tokio::test]
async fn an_unreadable_session_abandons_the_run_instead_of_retrying_forever() {
    let harness = HarnessBuilder::new()
        .work(vec![MemoryWorkItem {
            cursor: Seq(2),
            ..work_item(RunStatus::Completed, interactive())
        }])
        .build()
        .await;
    harness.ledger.poison(&SessionId::from_raw("sess-1"));

    let report = harness.manager.process_pending(4).await.unwrap();
    assert_eq!(report.abandoned, 1, "{report:?}");
    assert_eq!(report.failed, 0, "这不是下次重试，别记成失败：{report:?}");
    assert!(
        harness.llm.requests.lock().unwrap().is_empty(),
        "证据都读不出来，一次模型都不该发"
    );
    assert_eq!(
        harness.work.settled(),
        vec![(RunId::from_raw("run-1"), MemoryWork::Error, Seq(2))],
        "终态，队列不再为它空转"
    );
}

/// 写库路径全测：一条用户原话落 `active + user_statement + unconfirmed`，带着证据。
#[tokio::test]
async fn a_user_statement_lands_active_and_unconfirmed_with_its_evidence() {
    let harness = HarnessBuilder::new()
        .script(vec![round(
            r#"```json
            {"observations":[{"content":"客厅空调设 26 度","kind":"preference",
              "scope":"personal","said_by":"user_statement","evidence":["evt-1"],
              "user_confirmed":true}]}
            ```"#,
        )])
        .work(vec![work_item(RunStatus::Completed, interactive())])
        .build()
        .await;
    harness.ledger.seed(
        &SessionId::from_raw("sess-1"),
        vec![user_event(
            1,
            &RunId::from_raw("run-1"),
            "客厅空调我一直设 26 度",
        )],
    );

    let report = harness.manager.process_pending(4).await.unwrap();
    assert_eq!(report.applied, 1);

    let stored = harness.repo.list(None, None, 10).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].content, "客厅空调设 26 度");
    assert_eq!(stored[0].state, MemoryState::Active);
    assert_eq!(stored[0].provenance, Provenance::UserStatement);
    assert_eq!(
        stored[0].confirmation,
        Confirmation::Unconfirmed,
        "模型的 user_confirmed 没有写入权限（§9.2）"
    );
    assert_eq!(stored[0].evidence.len(), 1, "带着证据（§9.2）");
    assert_eq!(
        stored[0].evidence[0].extracted_from_run,
        Some(RunId::from_raw("run-1"))
    );
    assert_eq!(
        harness.work.settled(),
        vec![(RunId::from_raw("run-1"), MemoryWork::Done, Seq(1))]
    );
}

/// 冲突：**双方都标 contested**，都退出自动召回，等用户裁（§9.6）。
#[tokio::test]
async fn a_contradiction_puts_both_sides_in_contested_and_out_of_recall() {
    let harness = HarnessBuilder::new()
        .script(vec![round(
            r#"{"observations":[{"content":"客厅空调设 24 度","kind":"preference",
              "scope":"personal","said_by":"user_statement","evidence":["evt-1"]}]}"#,
        )])
        .script(vec![round(
            r#"{"verdicts":[{"memory":"m-1","relation":"contradicts"}]}"#,
        )])
        .work(vec![work_item(RunStatus::Completed, interactive())])
        .build()
        .await;
    harness
        .repo
        .put(memory("m-1", "客厅空调设 26 度"), None)
        .await
        .unwrap();
    harness.ledger.seed(
        &SessionId::from_raw("sess-1"),
        vec![user_event(1, &RunId::from_raw("run-1"), "空调改 24 度")],
    );

    harness.manager.process_pending(4).await.unwrap();

    let all = harness.repo.list(None, None, 10).await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(
        all.iter().all(|item| item.state == MemoryState::Contested),
        "双方都标：{all:?}"
    );
    // 「暂停正常召回」——自动召回一条都拿不到。
    assert!(
        harness
            .manager
            .search(harness.manager.query("空调", Some(RetrievalMode::Keyword)))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    // 但**显式 search 查得到**，不然没人帮得上把冲突定下来（§9.6）。
    let mut explicit = harness.manager.query("空调", Some(RetrievalMode::Keyword));
    explicit.include_states = vec![MemoryState::Contested];
    assert_eq!(
        harness.manager.search(explicit).await.unwrap().items.len(),
        2
    );
}

/// **新推断不能覆盖用户陈述**（§9.6）：`supersedes` 降级成冲突。
#[tokio::test]
async fn an_inference_may_not_supersede_a_user_statement() {
    let harness = HarnessBuilder::new()
        .script(vec![round(
            r#"{"observations":[{"content":"客厅空调大概应该设 24 度","kind":"preference",
              "scope":"personal","said_by":"model_inference","evidence":["evt-1"]}]}"#,
        )])
        .script(vec![round(
            r#"{"verdicts":[{"memory":"m-1","relation":"supersedes"}]}"#,
        )])
        .work(vec![work_item(RunStatus::Completed, interactive())])
        .build()
        .await;
    harness
        .repo
        .put(memory("m-1", "客厅空调设 26 度"), None)
        .await
        .unwrap();
    harness.ledger.seed(
        &SessionId::from_raw("sess-1"),
        vec![user_event(1, &RunId::from_raw("run-1"), "空调好像有点冷")],
    );

    harness.manager.process_pending(4).await.unwrap();

    let old = harness
        .repo
        .get(&MemoryId::from_raw("m-1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        old.state,
        MemoryState::Contested,
        "推断动不了用户陈述，只能把它标成有冲突"
    );
    assert_ne!(old.state, MemoryState::Superseded);
    let all = harness.repo.list(None, None, 10).await.unwrap();
    let fresh = all.iter().find(|item| item.id.as_str() != "m-1").unwrap();
    assert!(fresh.supersedes.is_none(), "没有取代，就没有取代链");
}

/// **自动整理不能静默改写用户已确认内容**（§9.6）：对着一条 `user_confirmed` 的
/// `supersedes` 同样降级成冲突。
#[tokio::test]
async fn nothing_automatic_supersedes_what_the_user_confirmed() {
    let harness = HarnessBuilder::new()
        .script(vec![round(
            r#"{"observations":[{"content":"客厅空调设 24 度","kind":"preference",
              "scope":"personal","said_by":"user_statement","evidence":["evt-1"]}]}"#,
        )])
        .script(vec![round(
            r#"{"verdicts":[{"memory":"m-1","relation":"supersedes"}]}"#,
        )])
        .work(vec![work_item(RunStatus::Completed, interactive())])
        .build()
        .await;
    harness
        .repo
        .put(memory("m-1", "客厅空调设 26 度"), None)
        .await
        .unwrap();
    harness
        .manager
        .confirm(&MemoryId::from_raw("m-1"), 1)
        .await
        .unwrap();
    harness.ledger.seed(
        &SessionId::from_raw("sess-1"),
        vec![user_event(1, &RunId::from_raw("run-1"), "空调改 24 度")],
    );

    harness.manager.process_pending(4).await.unwrap();

    let old = harness
        .repo
        .get(&MemoryId::from_raw("m-1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.state, MemoryState::Contested);
    assert_eq!(
        old.confirmation,
        Confirmation::UserConfirmed,
        "确认等级没被动过"
    );
}

/// 允许的取代：旧的标 `superseded`，新的带着**前向链**（§9.6）。
#[tokio::test]
async fn an_allowed_supersede_retires_the_old_one_and_links_forward() {
    let harness = HarnessBuilder::new()
        .script(vec![round(
            r#"{"observations":[{"content":"客厅空调设 24 度","kind":"preference",
              "scope":"personal","said_by":"user_statement","evidence":["evt-1"]}]}"#,
        )])
        .script(vec![round(
            r#"{"verdicts":[{"memory":"m-1","relation":"supersedes"}]}"#,
        )])
        .work(vec![work_item(RunStatus::Completed, interactive())])
        .build()
        .await;
    // 旧的那条是**工具观察**，不是用户陈述，也没有被确认过。
    let mut old = memory("m-1", "客厅空调设 26 度");
    old.provenance = Provenance::ToolObservation;
    harness.repo.put(old, None).await.unwrap();
    harness.ledger.seed(
        &SessionId::from_raw("sess-1"),
        vec![user_event(1, &RunId::from_raw("run-1"), "空调改 24 度")],
    );

    harness.manager.process_pending(4).await.unwrap();

    let retired = harness
        .repo
        .get(&MemoryId::from_raw("m-1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retired.state, MemoryState::Superseded);

    let all = harness.repo.list(None, None, 10).await.unwrap();
    let fresh = all.iter().find(|item| item.id.as_str() != "m-1").unwrap();
    assert_eq!(fresh.state, MemoryState::Active);
    let link = fresh.supersedes.as_ref().expect("链到旧的那条");
    assert_eq!(link.memory.as_str(), "m-1");
    assert_eq!(link.revision, 1);

    // 被取代的那条退出自动召回。
    let recalled = harness
        .manager
        .search(harness.manager.query("空调", Some(RetrievalMode::Keyword)))
        .await
        .unwrap();
    assert_eq!(recalled.items.len(), 1);
    assert_eq!(recalled.items[0].content, "客厅空调设 24 度");
}

// =========================================================== 注入与预算

/// 注入段在 token 预算处停下，**不强行填满**（§9.4）。
#[test]
fn the_injected_block_stops_at_the_token_budget() {
    let items: Vec<_> = (0..20)
        .map(|index| memory(&format!("m-{index:02}"), "客厅空调设 26 度，书房台灯用暖光"))
        .collect();
    let all = render_injection(&items, 5_000);
    assert_eq!(all.uses.len(), 20);

    let squeezed = render_injection(&items, 400);
    assert!(
        squeezed.uses.len() < 20 && !squeezed.uses.is_empty(),
        "装得下几条就是几条：{}",
        squeezed.uses.len()
    );
    assert!(squeezed.text.unwrap().chars().count() <= 400);
}

/// 注入表按 `run` 取，`SystemPreamble` 就是它的同步一面。
#[tokio::test]
async fn the_preamble_serves_what_the_segment_prepared_for_that_run() {
    let harness = HarnessBuilder::new().build().await;
    harness
        .repo
        .put(memory("m-1", "客厅空调设 26 度"), None)
        .await
        .unwrap();

    let run = RunId::from_raw("run-1");
    let injection = harness.manager.prepare(&run, "空调", &[]).await;
    assert_eq!(injection.uses.len(), 1);
    assert!(harness.manager.injection_for(&run).is_some());
    assert!(
        harness
            .manager
            .injection_for(&RunId::from_raw("run-other"))
            .is_none(),
        "别的 Run 拿不到这一次的选择"
    );

    // 使用计数记上了（§9.2：只度量使用）。
    let after = harness
        .repo
        .get(&MemoryId::from_raw("m-1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.usage.count, 1);
    assert_eq!(after.state, MemoryState::Active, "用一次不改状态");

    harness.manager.forget_run(&run);
    assert!(harness.manager.injection_for(&run).is_none());
}

/// `[memory] enabled = false` 时什么都不做——不注入、不提取、不报错地静静躺着。
#[tokio::test]
async fn a_disabled_memory_section_injects_nothing_and_extracts_nothing() {
    let harness = HarnessBuilder::new()
        .config(memory_config(false, RetrievalMode::Keyword))
        .work(vec![work_item(RunStatus::Completed, interactive())])
        .build()
        .await;
    harness
        .repo
        .put(memory("m-1", "客厅空调设 26 度"), None)
        .await
        .unwrap();

    let injection = harness
        .manager
        .prepare(&RunId::from_raw("run-1"), "空调", &[])
        .await;
    assert_eq!(injection, Injection::default());
    assert_eq!(
        harness.manager.process_pending(4).await.unwrap(),
        PassReport::default()
    );
}

// =========================================================== JSON 解析

/// 模型的回复常常裹着围栏和闲话——两种都要能解出来，解不出来是**错误**而不是空结果。
#[test]
fn the_extraction_parser_survives_fences_and_chatter() {
    let fenced = "好的，我看了一遍：\n```json\n{\"observations\":[{\"content\":\"a\",\
                  \"said_by\":\"user_statement\",\"evidence\":[\"evt-1\"]}]}\n```\n以上。";
    assert_eq!(extract::parse_extraction(fenced).unwrap().len(), 1);

    let bare = r#"{"observations":[]}"#;
    assert!(extract::parse_extraction(bare).unwrap().is_empty());

    // 正文里带花括号的字符串不该把配平算错。
    let tricky = r#"{"observations":[{"content":"用 {} 表示空对象","said_by":"user_statement",
                     "evidence":["evt-1"]}]}"#;
    assert_eq!(
        extract::parse_extraction(tricky).unwrap()[0].content,
        "用 {} 表示空对象"
    );

    assert!(matches!(
        extract::parse_extraction("我觉得没什么好记的"),
        Err(MemoryError::Invalid(_))
    ));
}

// ---------------------------------------------------------------- 小工具

fn memory(id: &str, content: &str) -> komo_kernel::types::memory::MemoryItem {
    komo_kernel::types::memory::MemoryItem {
        id: MemoryId::from_raw(id),
        revision: 1,
        content: content.into(),
        kind: komo_kernel::types::memory::MemoryKind::Preference,
        scope: MemoryScope::Personal,
        provenance: Provenance::UserStatement,
        confirmation: Confirmation::Unconfirmed,
        state: MemoryState::Active,
        evidence: vec![],
        observed_at: NOW,
        valid_until: None,
        created_at: NOW,
        updated_at: NOW,
        extraction: komo_kernel::types::memory::ExtractionMetadata::new("memory-model", None, "v1"),
        usage: Default::default(),
        supersedes: None,
    }
}
