//! 离线整理（§9.8）的验收。模型是 [`ScriptedLlm`]：每组一段固定 JSON，一次 `begin_turn`
//! 取一段，所以"这一轮调了几次模型"可以直接数 `requests`。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::protocol::config::{ConsolidateConfig, MemoryConfig, RetrievalConfig};
use komo_kernel::protocol::http::{
    ConsolidateAction, ConsolidateGroupStatus, ConsolidateOutcome, MemoryConsolidateRequest,
    MemoryConsolidateResponse,
};
use komo_kernel::test_support::ScriptedLlm;
use komo_kernel::traits::{Clock, LlmClient, MemoryRepo};
use komo_kernel::types::ids::{EventId, MemoryId, Seq, SessionId};
use komo_kernel::types::memory::{
    Confirmation, Evidence, EvidenceRef, ExtractionMetadata, Injection, MemoryApplication,
    MemoryItem, MemoryKind, MemoryScope, MemoryState, Provenance, RetrievalMode,
};
use komo_kernel::types::model::{Effort, ModelConfig};
use komo_kernel::types::turn::{Round, TurnRequest};
use komo_store::{Db, TursoMemoryRepo};
use time::OffsetDateTime;
use time::macros::datetime;

use super::super::consolidate::Consolidator;
use super::super::{
    MemoryBatch, MemoryCatalog, MemoryError, MemoryManager, MemoryParts, Observation, SessionEvents,
};
use super::*;

const NOW: OffsetDateTime = datetime!(2026-10-19 08:00:00 UTC);
const LATER: OffsetDateTime = datetime!(2026-10-19 09:00:00 UTC);
const NOTHING: &str = r#"{"actions":[]}"#;

// ---------------------------------------------------------------- 替身

#[derive(Debug, Clone)]
struct TestClock(Arc<Mutex<OffsetDateTime>>);

impl TestClock {
    fn set(&self, to: OffsetDateTime) {
        *self.0.lock().unwrap() = to;
    }
}

impl Clock for TestClock {
    fn now(&self) -> OffsetDateTime {
        *self.0.lock().unwrap()
    }
}

#[derive(Debug)]
struct NoEvents;

#[async_trait]
impl SessionEvents for NoEvents {
    async fn events_of(
        &self,
        _session: &SessionId,
    ) -> Result<Vec<komo_kernel::events::Event>, MemoryError> {
        Ok(vec![])
    }
}

/// 离线整理不碰提取队列：一个永远是空的。
#[derive(Debug)]
struct NoWork;

#[async_trait]
impl super::super::MemoryWorkLog for NoWork {
    async fn claim(
        &self,
        _idle: time::Duration,
        _limit: usize,
    ) -> Result<Vec<MemoryBatch>, komo_kernel::traits::StoreError> {
        Ok(vec![])
    }

    async fn finish(
        &self,
        _run: &komo_kernel::types::ids::RunId,
        _work: komo_kernel::types::memory::MemoryWork,
        _cursor: komo_kernel::types::ids::Seq,
    ) -> Result<(), komo_kernel::traits::StoreError> {
        Ok(())
    }

    async fn requeue_stuck(&self) -> Result<u64, komo_kernel::traits::StoreError> {
        Ok(0)
    }
}

/// 第一次被问到时，先把一条记忆改掉再交给脚本：模拟"判定期间后台提取动了它"。
struct Meddling {
    inner: ScriptedLlm,
    repo: Arc<TursoMemoryRepo>,
    victim: MemoryId,
    done: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl LlmClient for Meddling {
    async fn begin_turn(
        &self,
        req: TurnRequest,
    ) -> Result<Box<dyn komo_kernel::traits::TurnDriver>, komo_kernel::types::turn::LlmError> {
        if !self.done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let mut item = self.repo.get(&self.victim).await.unwrap().unwrap();
            let expected = item.revision;
            item.revision += 1;
            item.content = format!("{}（改过）", item.content);
            self.repo.put(item, Some(expected)).await.unwrap();
        }
        self.inner.begin_turn(req).await
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

fn model() -> ModelConfig {
    ModelConfig {
        provider: "openai_responses".into(),
        base_url: "https://memory.example.com/v1".into(),
        model: "memory-model".into(),
        api_key_env: "KOMO_LLM_API_KEY".into(),
        auth: None,
        effort: Some(Effort::new("low")),
        efforts: None,
        timeout_secs: 10,
        context_window: None,
    }
}

fn render(_: &[MemoryItem], _: &[MemoryItem], _: u32, _: u32) -> Injection {
    Injection::default()
}

struct Rig {
    manager: Arc<MemoryManager>,
    repo: Arc<TursoMemoryRepo>,
    llm: ScriptedLlm,
    clock: TestClock,
    _dir: tempfile::TempDir,
}

impl Rig {
    async fn new(scripts: &[&str]) -> Rig {
        Rig::with(scripts, ConsolidateConfig::default()).await
    }

    async fn with(scripts: &[&str], consolidate: ConsolidateConfig) -> Rig {
        Rig::build(scripts, consolidate, |_, llm| Arc::new(llm)).await
    }

    async fn build(
        scripts: &[&str],
        consolidate: ConsolidateConfig,
        wrap: impl FnOnce(&Arc<TursoMemoryRepo>, ScriptedLlm) -> Arc<dyn LlmClient>,
    ) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::connect(dir.path().join("state.db")).await.unwrap();
        let repo = Arc::new(TursoMemoryRepo::new(db.clone()));
        let llm = ScriptedLlm::new(scripts.iter().map(|text| vec![round(text)]).collect());
        let clock = TestClock(Arc::new(Mutex::new(LATER)));
        let client = wrap(&repo, llm.clone());
        let manager = Arc::new(MemoryManager::new(MemoryParts {
            config: MemoryConfig {
                enabled: true,
                model: model(),
                embedding: None,
                retrieval: RetrievalConfig {
                    mode: RetrievalMode::Keyword,
                    ..RetrievalConfig::default()
                },
                consolidate,
                extract_idle_secs: 600,
                max_observations: 8,
            },
            repo: Arc::clone(&repo) as Arc<dyn MemoryRepo>,
            catalog: Arc::clone(&repo) as Arc<dyn MemoryCatalog>,
            embeddings: None,
            reranker: None,
            llm: client,
            events: Arc::new(NoEvents),
            work: Arc::new(NoWork),
            clock: Arc::new(clock.clone()),
            render,
        }));
        Rig {
            manager,
            repo,
            llm,
            clock,
            _dir: dir,
        }
    }

    async fn seed(&self, spec: Seed) -> MemoryItem {
        self.repo.put(spec.build(), None).await.unwrap()
    }

    async fn get(&self, id: &str) -> MemoryItem {
        self.repo
            .get(&MemoryId::from_raw(id))
            .await
            .unwrap()
            .unwrap()
    }

    async fn mark(&self, id: &str) -> Option<OffsetDateTime> {
        self.repo
            .consolidation_marks(&[MemoryId::from_raw(id)])
            .await
            .unwrap()[0]
            .1
    }

    async fn run(&self, apply: bool) -> MemoryConsolidateResponse {
        self.manager
            .consolidate(&MemoryConsolidateRequest {
                apply,
                ..Default::default()
            })
            .await
            .unwrap()
    }

    fn model_calls(&self) -> usize {
        self.llm.requests.lock().unwrap().len()
    }
}

struct Seed {
    id: &'static str,
    content: String,
    kind: MemoryKind,
    scope: MemoryScope,
    provenance: Provenance,
    confirmation: Confirmation,
    state: MemoryState,
    application: Option<MemoryApplication>,
    applies_when: Option<String>,
}

impl Seed {
    fn new(id: &'static str, content: &str) -> Seed {
        Seed {
            id,
            content: content.into(),
            kind: MemoryKind::Fact,
            scope: MemoryScope::Personal,
            provenance: Provenance::UserStatement,
            confirmation: Confirmation::Unconfirmed,
            state: MemoryState::Active,
            application: None,
            applies_when: None,
        }
    }
    fn usage(mut self, application: MemoryApplication, when: Option<&str>) -> Seed {
        self.application = Some(application);
        self.applies_when = when.map(str::to_string);
        self
    }
    fn kind(mut self, kind: MemoryKind) -> Seed {
        self.kind = kind;
        self
    }
    fn scope(mut self, scope: MemoryScope) -> Seed {
        self.scope = scope;
        self
    }
    fn by(mut self, provenance: Provenance) -> Seed {
        self.provenance = provenance;
        self
    }
    fn confirmed(mut self) -> Seed {
        self.confirmation = Confirmation::UserConfirmed;
        self
    }
    fn state(mut self, state: MemoryState) -> Seed {
        self.state = state;
        self
    }

    fn build(self) -> MemoryItem {
        MemoryItem {
            id: MemoryId::from_raw(self.id),
            revision: 1,
            content: self.content,
            kind: self.kind,
            scope: self.scope,
            provenance: self.provenance,
            confirmation: self.confirmation,
            state: self.state,
            evidence: vec![Evidence {
                reference: EvidenceRef::Event {
                    session: SessionId::from_raw("sess-1"),
                    event: EventId::from_raw(format!("evt-{}", self.id)),
                    seq: Seq(1),
                },
                provenance: self.provenance,
                observed_at: NOW,
                extracted_from_run: None,
            }],
            observed_at: NOW,
            valid_until: None,
            created_at: NOW,
            updated_at: NOW,
            extraction: ExtractionMetadata::new("memory-model", None, "v1"),
            usage: Default::default(),
            supersedes: None,
            superseded_by: None,
            application: self.application,
            applies_when: self.applies_when,
        }
    }
}

/// 同义的两句，共用 `深色` `主题` 两个词元，检索得到彼此。
const DARK_A: &str = "喜欢深色主题界面";
const DARK_B: &str = "偏好深色的主题配色";

fn merge_json(keep: usize, absorb: &[usize]) -> String {
    format!(
        r#"{{"actions":[{{"action":"merge","keep":{keep},"absorb":{absorb:?},"reason":"同一件事"}}]}}"#
    )
}

// ---------------------------------------------------------------- 验收

#[tokio::test]
async fn two_synonymous_actives_merge_and_a_preview_writes_nothing() {
    let merge = merge_json(0, &[1]);
    let rig = Rig::new(&[&merge, &merge]).await;
    rig.seed(Seed::new("a-1", DARK_A)).await;
    rig.seed(Seed::new("a-2", DARK_B)).await;

    let preview = rig.run(false).await;
    assert!(!preview.applied);
    assert_eq!(preview.groups.len(), 1);
    assert_eq!(
        preview.groups[0].actions[0].outcome,
        ConsolidateOutcome::Executed
    );
    assert_eq!(
        rig.get("a-2").await.state,
        MemoryState::Active,
        "预演不改库"
    );
    assert_eq!(rig.get("a-1").await.evidence.len(), 1);
    assert_eq!(rig.mark("a-1").await, None, "预演不盖游标");

    let applied = rig.run(true).await;
    assert_eq!(applied.groups[0].status, ConsolidateGroupStatus::Judged);
    let keep = rig.get("a-1").await;
    assert_eq!(keep.evidence.len(), 2, "证据是并集");
    assert_eq!(keep.content, DARK_A, "正文一个字不改");
    assert_eq!(keep.state, MemoryState::Active);
    assert_eq!(keep.confirmation, Confirmation::Unconfirmed);
    let gone = rig.get("a-2").await;
    assert_eq!(gone.state, MemoryState::Superseded);
    assert_eq!(gone.superseded_by.unwrap().memory.as_str(), "a-1");
    assert_eq!(rig.mark("a-1").await, Some(LATER));
    assert_eq!(rig.mark("a-2").await, Some(LATER));
}

/// `komo memory consolidate --scope agent:<id>`：私有作用域与别的作用域一样按组整理，
/// 而且只碰它自己。
#[tokio::test]
async fn an_agent_scope_consolidates_on_its_own_and_leaves_personal_alone() {
    let merge = merge_json(0, &[1]);
    let rig = Rig::new(&[&merge]).await;
    let helper = MemoryScope::Agent {
        agent_id: "helper".into(),
    };
    rig.seed(Seed::new("g-1", DARK_A).scope(helper.clone()))
        .await;
    rig.seed(Seed::new("g-2", DARK_B).scope(helper.clone()))
        .await;
    rig.seed(Seed::new("p-1", DARK_A)).await;
    rig.seed(Seed::new("p-2", DARK_B)).await;

    let response = rig
        .manager
        .consolidate(&MemoryConsolidateRequest {
            apply: true,
            scope: Some(helper),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(response.groups.len(), 1);
    assert_eq!(rig.get("g-2").await.state, MemoryState::Superseded);
    assert_eq!(rig.get("p-1").await.state, MemoryState::Active);
    assert_eq!(rig.get("p-2").await.state, MemoryState::Active);
}

/// §9.8：合并的成员 application 与 applies_when 必须逐字相同，否则不执行、标"需要人看"——
/// 合并既不能把线索抬成约定，也不能把"这个任务"的条件扩成"所有任务"。
#[tokio::test]
async fn members_with_a_different_condition_or_application_are_never_merged() {
    use MemoryApplication::{Convention, Reference};
    for (first, second) in [
        (
            Seed::new("a-1", DARK_A).usage(Convention, Some("创建发布工单时")),
            Seed::new("a-2", DARK_B).usage(Convention, Some("写周报时")),
        ),
        (
            Seed::new("a-1", DARK_A).usage(Convention, Some("创建发布工单时")),
            Seed::new("a-2", DARK_B).usage(Reference, Some("创建发布工单时")),
        ),
        (
            Seed::new("a-1", DARK_A).usage(Reference, None),
            Seed::new("a-2", DARK_B).usage(Reference, Some("只在这个任务里")),
        ),
    ] {
        let rig = Rig::new(&[&merge_json(0, &[1])]).await;
        rig.seed(first).await;
        rig.seed(second).await;

        let response = rig.run(true).await;
        let entry = &response.groups[0].actions[0];
        assert_eq!(entry.outcome, ConsolidateOutcome::Skipped);
        assert!(entry.note.as_deref().unwrap().contains("需要人看"));
        assert_eq!(rig.get("a-1").await.state, MemoryState::Active);
        assert_eq!(
            rig.get("a-2").await.state,
            MemoryState::Active,
            "没有被吸收"
        );
    }
}

/// 使用方式与条件都相同时照常合并（没分类的旧条目按 reference 比）。
#[tokio::test]
async fn members_with_the_same_usage_still_merge() {
    use MemoryApplication::{Convention, Reference};
    for (first, second) in [
        (
            Seed::new("a-1", DARK_A).usage(Convention, Some("创建发布工单时")),
            Seed::new("a-2", DARK_B).usage(Convention, Some("创建发布工单时")),
        ),
        (
            Seed::new("a-1", DARK_A),
            Seed::new("a-2", DARK_B).usage(Reference, None),
        ),
    ] {
        let rig = Rig::new(&[&merge_json(0, &[1])]).await;
        rig.seed(first).await;
        rig.seed(second).await;
        let response = rig.run(true).await;
        assert_eq!(
            response.groups[0].actions[0].outcome,
            ConsolidateOutcome::Executed
        );
        assert_eq!(rig.get("a-2").await.state, MemoryState::Superseded);
    }
}

/// §9.8：old 是 convention、by 不是 → 降级为 contest，不让一条线索悄悄停掉约定。
#[tokio::test]
async fn a_reference_cannot_supersede_a_convention() {
    let json = r#"{"actions":[{"action":"supersede","old":0,"by":1,"reason":"新答案"}]}"#;
    let rig = Rig::new(&[json]).await;
    rig.seed(Seed::new("a-1", DARK_A).usage(MemoryApplication::Convention, Some("选 Platform 时")))
        .await;
    rig.seed(Seed::new("a-2", DARK_B).usage(MemoryApplication::Reference, None))
        .await;

    let response = rig.run(true).await;
    let entry = &response.groups[0].actions[0];
    assert_eq!(entry.outcome, ConsolidateOutcome::Downgraded);
    assert_eq!(rig.get("a-1").await.state, MemoryState::Contested);
    assert_eq!(rig.get("a-2").await.state, MemoryState::Contested);

    let requests = rig.llm.requests.lock().unwrap();
    let prompt = requests[0].messages[0].text.clone().unwrap();
    assert!(
        prompt.contains("使用方式=convention 条件：选 Platform 时"),
        "{prompt}"
    );
    assert!(prompt.contains("使用方式=reference"), "{prompt}");
}

/// 组内编号行带上使用方式与条件，提示词是 `memory-consolidate/v2`。
#[test]
fn the_consolidation_prompt_version_is_v2() {
    assert_eq!(PROMPT_VERSION, "memory-consolidate/v2");
}

#[tokio::test]
async fn an_inference_absorbing_a_user_statement_is_not_executed() {
    let rig = Rig::new(&[&merge_json(0, &[1])]).await;
    rig.seed(Seed::new("a-1", DARK_A).by(Provenance::ModelInference))
        .await;
    rig.seed(Seed::new("a-2", DARK_B)).await;

    let response = rig.run(true).await;
    let entry = &response.groups[0].actions[0];
    assert_eq!(entry.outcome, ConsolidateOutcome::Skipped);
    assert!(entry.note.as_deref().unwrap().contains("需要人看"));
    assert_eq!(rig.get("a-2").await.state, MemoryState::Active);
    assert_eq!(rig.get("a-1").await.evidence.len(), 1);
    assert_eq!(rig.mark("a-1").await, Some(LATER), "判过就盖游标，不论动作");
}

#[tokio::test]
async fn superseding_a_user_confirmed_memory_contests_both() {
    let json = r#"{"actions":[{"action":"supersede","old":0,"by":1,"reason":"新答案"}]}"#;
    let rig = Rig::new(&[json]).await;
    rig.seed(Seed::new("a-1", DARK_A).confirmed()).await;
    rig.seed(Seed::new("a-2", DARK_B).by(Provenance::ToolObservation))
        .await;

    let response = rig.run(true).await;
    let entry = &response.groups[0].actions[0];
    assert_eq!(entry.outcome, ConsolidateOutcome::Downgraded);
    assert!(matches!(
        entry.executed_as,
        Some(ConsolidateAction::Contest { .. })
    ));
    assert_eq!(rig.get("a-1").await.state, MemoryState::Contested);
    assert_eq!(rig.get("a-2").await.state, MemoryState::Contested);
    assert_eq!(
        rig.get("a-1").await.confirmation,
        Confirmation::UserConfirmed
    );
}

#[tokio::test]
async fn an_inference_superseding_a_user_statement_contests_both() {
    let json = r#"{"actions":[{"action":"supersede","old":0,"by":1,"reason":"新答案"}]}"#;
    let rig = Rig::new(&[json]).await;
    rig.seed(Seed::new("a-1", DARK_A)).await;
    rig.seed(
        Seed::new("a-2", DARK_B)
            .by(Provenance::ModelInference)
            .state(MemoryState::Candidate),
    )
    .await;

    let response = rig.run(true).await;
    assert_eq!(
        response.groups[0].actions[0].outcome,
        ConsolidateOutcome::Downgraded
    );
    assert_eq!(rig.get("a-1").await.state, MemoryState::Contested);
    assert_eq!(rig.get("a-2").await.state, MemoryState::Contested);
}

#[tokio::test]
async fn a_plain_supersede_marks_only_the_old_one_and_records_who_replaced_it() {
    let json = r#"{"actions":[{"action":"supersede","old":0,"by":1,"reason":"新答案"}]}"#;
    let rig = Rig::new(&[json]).await;
    rig.seed(Seed::new("a-1", DARK_A)).await;
    rig.seed(Seed::new("a-2", DARK_B)).await;

    rig.run(true).await;
    let old = rig.get("a-1").await;
    assert_eq!(old.state, MemoryState::Superseded);
    assert_eq!(old.superseded_by.unwrap().memory.as_str(), "a-2");
    assert_eq!(rig.get("a-2").await.state, MemoryState::Active);
}

#[tokio::test]
async fn a_group_whose_member_changed_after_judging_is_skipped_without_a_cursor() {
    let merge = merge_json(0, &[1]);
    let rig = Rig::build(&[&merge], ConsolidateConfig::default(), |repo, llm| {
        Arc::new(Meddling {
            inner: llm,
            repo: Arc::clone(repo),
            victim: MemoryId::from_raw("a-2"),
            done: Default::default(),
        })
    })
    .await;
    rig.seed(Seed::new("a-1", DARK_A)).await;
    rig.seed(Seed::new("a-2", DARK_B)).await;

    let response = rig.run(true).await;
    assert_eq!(response.groups[0].status, ConsolidateGroupStatus::Conflict);
    assert_eq!(rig.get("a-2").await.state, MemoryState::Active);
    assert_eq!(rig.get("a-2").await.revision, 2);
    assert_eq!(rig.get("a-1").await.evidence.len(), 1);
    assert_eq!(rig.mark("a-1").await, None);
    assert_eq!(rig.mark("a-2").await, None);
}

#[tokio::test]
async fn rereadable_never_touches_confirmed_or_preference_and_a_forgotten_source_stays_forgotten() {
    let json = r#"{"actions":[
        {"action":"rereadable","item":0,"reason":"目录结构随时能读"},
        {"action":"rereadable","item":1,"reason":"也是目录结构"},
        {"action":"rereadable","item":2,"reason":"也是目录结构"}]}"#;
    let rig = Rig::new(&[json]).await;
    rig.seed(Seed::new("b-1", "仓库目录结构：src 下是 crates"))
        .await;
    rig.seed(Seed::new("b-2", "仓库目录结构：docs 放设计").confirmed())
        .await;
    rig.seed(Seed::new("b-3", "仓库目录结构我希望保持扁平").kind(MemoryKind::Preference))
        .await;

    let response = rig.run(true).await;
    let outcomes: Vec<_> = response.groups[0]
        .actions
        .iter()
        .map(|entry| entry.outcome)
        .collect();
    assert_eq!(
        outcomes,
        [
            ConsolidateOutcome::Executed,
            ConsolidateOutcome::Skipped,
            ConsolidateOutcome::Skipped
        ]
    );
    assert_eq!(rig.get("b-1").await.state, MemoryState::Forgotten);
    assert_eq!(rig.get("b-2").await.state, MemoryState::Active);
    assert_eq!(rig.get("b-3").await.state, MemoryState::Active);

    // 同一句话被后台再提取一次：撞上那条已遗忘的，只补证据，不新增、不复活。
    let observation = Observation {
        content: "仓库目录结构：src 下是 crates".into(),
        kind: MemoryKind::Fact,
        scope: MemoryScope::Personal,
        provenance: Provenance::UserStatement,
        state: MemoryState::Active,
        evidence: vec![],
        observed_at: NOW,
        valid_until: None,
        extraction: ExtractionMetadata::new("memory-model", None, "v3"),
        relations: vec![],
        application: None,
        applies_when: None,
    };
    Consolidator::new(&rig.manager)
        .apply(&[observation])
        .await
        .unwrap();
    assert_eq!(rig.get("b-1").await.state, MemoryState::Forgotten);
    let all = rig
        .manager
        .catalog()
        .list(None, None, usize::MAX)
        .await
        .unwrap();
    assert_eq!(all.len(), 3, "没有新条目");
}

#[tokio::test]
async fn items_in_different_scopes_are_never_grouped() {
    let rig = Rig::new(&[]).await;
    rig.seed(Seed::new("a-1", DARK_A)).await;
    rig.seed(Seed::new("a-2", DARK_A).scope(MemoryScope::Project {
        project_id: "proj".into(),
    }))
    .await;

    let response = rig.run(true).await;
    assert!(response.groups.is_empty());
    assert_eq!(rig.model_calls(), 0);
}

#[tokio::test]
async fn a_group_left_as_is_goes_last_next_time_and_unchanged_groups_skip_the_model() {
    let rig = Rig::with(
        &[NOTHING, NOTHING, NOTHING],
        ConsolidateConfig {
            max_groups: 1,
            ..Default::default()
        },
    )
    .await;
    rig.seed(Seed::new("a-1", DARK_A)).await;
    rig.seed(Seed::new("a-2", DARK_B)).await;
    rig.seed(Seed::new("b-1", "客厅空调设二十六度")).await;
    rig.seed(Seed::new("b-2", "客厅空调温度设置二十六度")).await;

    let first = rig.run(true).await;
    assert_eq!(first.groups[0].members[0].memory.as_str(), "a-1");
    assert_eq!(first.deferred_groups, 1);

    // a 组之后有人动过它（updated_at 晚于游标）：它仍然排在从没看过的 b 组之后。
    let mut touched = rig.get("a-1").await;
    touched.updated_at = LATER + time::Duration::minutes(1);
    rig.repo.put(touched, Some(1)).await.unwrap();
    let second = rig.run(true).await;
    assert_eq!(second.groups.len(), 1);
    assert_eq!(second.groups[0].members[0].memory.as_str(), "b-1");

    rig.clock.set(LATER + time::Duration::hours(1));
    let third = rig.run(true).await;
    assert_eq!(third.groups.len(), 1, "只剩被动过的 a 组");
    assert_eq!(third.groups[0].members[0].memory.as_str(), "a-1");
    assert_eq!(third.unchanged_groups, 1);

    let calls = rig.model_calls();
    let fourth = rig.run(true).await;
    assert!(fourth.groups.is_empty());
    assert_eq!(fourth.unchanged_groups, 2);
    assert_eq!(rig.model_calls(), calls, "成员都没变：不调模型");
}

#[tokio::test]
async fn the_fifty_first_group_is_handled_in_the_second_run() {
    let scripts = vec![NOTHING; 51];
    let rig = Rig::new(&scripts).await;
    for n in 0..=50 {
        let (a, b) = (
            Box::leak(format!("p{n:02}-1").into_boxed_str()) as &'static str,
            Box::leak(format!("p{n:02}-2").into_boxed_str()) as &'static str,
        );
        rig.seed(Seed::new(a, &format!("zq{n:02}a zq{n:02}b")))
            .await;
        rig.seed(Seed::new(b, &format!("zq{n:02}a zq{n:02}c")))
            .await;
    }

    let first = rig.run(true).await;
    assert_eq!(first.groups.len(), 50);
    assert_eq!(first.deferred_groups, 1);
    assert_eq!(rig.mark("p50-1").await, None);

    let second = rig.run(true).await;
    assert_eq!(second.groups.len(), 1);
    assert_eq!(second.groups[0].members[0].memory.as_str(), "p50-1");
    assert_eq!(second.unchanged_groups, 50);
    assert_eq!(rig.mark("p50-1").await, Some(LATER));
}

#[tokio::test]
async fn a_candidate_cannot_absorb_an_active_one() {
    let rig = Rig::new(&[&merge_json(0, &[1])]).await;
    rig.seed(
        Seed::new("a-1", DARK_A)
            .by(Provenance::ModelInference)
            .state(MemoryState::Candidate),
    )
    .await;
    rig.seed(Seed::new("a-2", DARK_B).by(Provenance::ModelInference))
        .await;

    let response = rig.run(true).await;
    let entry = &response.groups[0].actions[0];
    assert_eq!(entry.outcome, ConsolidateOutcome::Skipped);
    assert!(entry.note.as_deref().unwrap().contains("需要人看"));
    assert_eq!(rig.get("a-2").await.state, MemoryState::Active);
}

#[tokio::test]
async fn a_merge_with_a_contested_member_is_not_executed() {
    let rig = Rig::new(&[&merge_json(0, &[1])]).await;
    rig.seed(Seed::new("a-1", DARK_A)).await;
    rig.seed(Seed::new("a-2", DARK_B).state(MemoryState::Contested))
        .await;

    let response = rig.run(true).await;
    assert_eq!(
        response.groups[0].actions[0].outcome,
        ConsolidateOutcome::Skipped
    );
    assert_eq!(rig.get("a-2").await.state, MemoryState::Contested);
}

#[tokio::test]
async fn a_model_failure_leaves_the_group_untouched_and_unstamped() {
    let rig = Rig::new(&[]).await;
    rig.seed(Seed::new("a-1", DARK_A)).await;
    rig.seed(Seed::new("a-2", DARK_B)).await;

    let response = rig.run(true).await;
    assert_eq!(response.groups[0].status, ConsolidateGroupStatus::Failed);
    assert_eq!(rig.mark("a-1").await, None);
}

#[tokio::test]
async fn the_prompt_lists_candidates_by_index_and_carries_the_instruction() {
    let rig = Rig::new(&[NOTHING]).await;
    rig.seed(Seed::new("a-1", DARK_A)).await;
    rig.seed(Seed::new("a-2", DARK_B)).await;

    rig.manager
        .consolidate(&MemoryConsolidateRequest {
            instruction: Some("两个名字是同一个项目".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let requests = rig.llm.requests.lock().unwrap();
    let prompt = requests[0].messages[0].text.clone().unwrap();
    assert!(prompt.contains("[0] 状态=") && prompt.contains("[1] 状态="));
    assert!(prompt.contains("两个名字是同一个项目"));
    assert!(
        !prompt.contains("a-1") && !prompt.contains("a-2"),
        "提示词里没有真实 id"
    );
}

// ---------------------------------------------------------------- 纯函数

fn mem(id: &str, content: &str) -> MemoryItem {
    Seed {
        id: Box::leak(id.to_string().into_boxed_str()),
        ..Seed::new("x", content)
    }
    .build()
}

#[test]
fn an_oversized_group_is_split_by_similarity_into_chunks_of_at_most_the_cap() {
    let items: Vec<MemoryItem> = (0..10)
        .map(|n| mem(&format!("m-{n:02}"), &format!("alpha beta gamma w{n}")))
        .collect();
    let neighbors = items
        .iter()
        .map(|item| {
            (
                item.id.clone(),
                items.iter().map(|other| other.id.clone()).collect(),
            )
        })
        .collect();
    let groups = group_items(&items, &neighbors, 8, 0.0);
    assert!(groups.iter().all(|group| (2..=8).contains(&group.len())));
    assert_eq!(groups.iter().map(Vec::len).sum::<usize>(), 10);
}

#[test]
fn a_neighbor_below_the_similarity_floor_does_not_join_the_group() {
    let items = vec![
        mem("m-1", "alpha beta gamma delta"),
        mem("m-2", "alpha beta gamma epsilon"),
        mem("m-3", "zeta eta theta alpha"),
    ];
    let neighbors = items
        .iter()
        .map(|item| {
            (
                item.id.clone(),
                items.iter().map(|other| other.id.clone()).collect(),
            )
        })
        .collect();
    let groups = group_items(&items, &neighbors, 8, 0.5);
    assert_eq!(groups, vec![vec![0, 1]]);
}

#[test]
fn out_of_range_and_non_integer_indexes_are_dropped_and_numeric_strings_accepted() {
    let text = r#"{"actions":[
        {"action":"merge","keep":"0","absorb":[1],"reason":"x"},
        {"action":"supersede","old":0,"by":9,"reason":"越界"},
        {"action":"contest","a":0.5,"b":1,"reason":"非整数"},
        {"action":"rewrite","item":0,"reason":"没有这个动作"}]}"#;
    let parsed = parse_actions(text, 2).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(
        parsed[0].kind,
        RawKind::Merge {
            keep: 0,
            absorb: vec![1]
        }
    );
}
