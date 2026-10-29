//! 记忆的归属、结案归档与按归属分层（`docs/goals.md` §6、§7；`docs/memory.md` §9.2、§9.4、§9.6）。

use komo_kernel::traits::MatterRepo;
use komo_kernel::types::ids::MatterId;
use komo_kernel::types::matter::{Matter, MatterKind, MatterState, NewMatter};
use komo_store::TursoMatterRepo;

use super::*;
use crate::memory::consolidate::Consolidator;

struct Rig {
    manager: Arc<MemoryManager>,
    repo: Arc<TursoMemoryRepo>,
    matters: Arc<TursoMatterRepo>,
    _dir: tempfile::TempDir,
}

async fn rig(top_k: u32, push_unattached: bool) -> Rig {
    let dir = tempfile::tempdir().expect("临时目录");
    let db = Db::connect(dir.path().join("aux.db"))
        .await
        .expect("打开库");
    let repo = Arc::new(TursoMemoryRepo::new(db.clone()));
    let matters = Arc::new(TursoMatterRepo::new(db));
    let mut config = memory_config(true, RetrievalMode::Keyword);
    config.retrieval.top_k = top_k;
    config.retrieval.push_unattached = push_unattached;
    let manager = Arc::new(MemoryManager::new(MemoryParts {
        config,
        repo: Arc::clone(&repo) as Arc<dyn MemoryRepo>,
        catalog: Arc::clone(&repo) as Arc<dyn MemoryCatalog>,
        embeddings: None,
        reranker: None,
        llm: Arc::new(ScriptedLlm::new(Vec::new())) as Arc<dyn LlmClient>,
        events: Arc::new(SeedEvents::default()) as Arc<dyn SessionEvents>,
        work: MemWork::with(Vec::new()) as Arc<dyn MemoryWorkLog>,
        clock: Arc::new(FixedClock),
        render: test_render,
        matters: Some(Arc::clone(&matters) as Arc<dyn MatterRepo>),
    }));
    Rig {
        manager,
        repo,
        matters,
        _dir: dir,
    }
}

impl Rig {
    async fn matter(&self, id: &str, kind: MatterKind, state: MatterState) -> Matter {
        let mut matter = NewMatter {
            kind,
            title: format!("事项 {id}"),
            next: None,
            due: None,
            standard: None,
        }
        .build(MatterId::from_raw(id), NOW)
        .unwrap();
        matter.state = state;
        self.matters.put(matter, None).await.unwrap()
    }

    /// 把事项改到另一个状态（模拟 `done` / `reopen` 已经落库）。
    async fn move_matter(&self, id: &str, state: MatterState) {
        let id = MatterId::from_raw(id);
        let mut matter = self.matters.get(&id).await.unwrap().unwrap();
        let expected = matter.revision;
        matter.state = state;
        matter.revision += 1;
        self.matters.put(matter, Some(expected)).await.unwrap();
    }

    async fn clue(&self, content: &str, matter: Option<&str>) -> MemoryItem {
        let (item, _) = self
            .manager
            .add(&add_request(content, matter, None))
            .await
            .unwrap();
        item
    }

    async fn convention(&self, content: &str, matter: &str) -> MemoryItem {
        let (item, _) = self
            .manager
            .add(&add_request(
                content,
                Some(matter),
                Some(MemoryApplication::Convention),
            ))
            .await
            .unwrap();
        item
    }

    async fn get(&self, id: &MemoryId) -> MemoryItem {
        self.repo.get(id).await.unwrap().unwrap()
    }

    async fn pushed(&self, query: &str) -> Vec<MemoryId> {
        self.manager
            .recall_for_run(&RunId::from_raw("run-1"), query, &[], &[], None)
            .await
            .uses
            .into_iter()
            .map(|use_| use_.memory)
            .collect()
    }
}

fn add_request(
    content: &str,
    matter: Option<&str>,
    application: Option<MemoryApplication>,
) -> MemoryAddRequest {
    MemoryAddRequest {
        content: content.into(),
        kind: MemoryKind::Preference,
        scope: MemoryScope::Personal,
        session: None,
        run: None,
        request_key: None,
        application,
        applies_when: application.map(|_| "空调相关的事".into()),
        belongs_to: matter.map(MatterId::from_raw),
    }
}

// ---------------------------------------------------------------- 分层

/// 搁置的目标名下的线索不推，也不占 `top_k` 的名额：名额 2、三条都命中，搁置的那条排在哪都
/// 不能挤掉另外两条。
#[tokio::test]
async fn a_parked_goals_clue_is_not_pushed_and_takes_no_top_k_slot() {
    let rig = rig(2, true).await;
    rig.matter("g-active", MatterKind::Goal, MatterState::Active)
        .await;
    rig.matter("g-parked", MatterKind::Goal, MatterState::Parked)
        .await;
    let live = rig.clue("空调 进行中的目标的线索", Some("g-active")).await;
    let free = rig.clue("空调 没有归属的线索", None).await;
    let parked = rig.clue("空调 搁置的目标的线索", Some("g-parked")).await;

    let pushed = rig.pushed("空调").await;
    assert!(!pushed.contains(&parked.id), "搁置的不推：{pushed:?}");
    assert!(pushed.contains(&live.id), "{pushed:?}");
    assert!(pushed.contains(&free.id), "{pushed:?}");
    // 搁置不归档：线索原样在库里，重启时回来。
    assert_eq!(rig.get(&parked.id).await.state, MemoryState::Active);
}

/// `push_unattached = false`：没有归属的线索降到只能查；有归属且进行中的照推。
#[tokio::test]
async fn unattached_clues_are_withheld_when_push_unattached_is_off() {
    let rig = rig(8, false).await;
    rig.matter("g-active", MatterKind::Goal, MatterState::Active)
        .await;
    rig.matter("a-area", MatterKind::Area, MatterState::Active)
        .await;
    let attached = rig.clue("空调 目标的线索", Some("g-active")).await;
    let area = rig.clue("空调 领域的线索", Some("a-area")).await;
    let free = rig.clue("空调 没有归属的线索", None).await;

    let pushed = rig.pushed("空调").await;
    assert!(pushed.contains(&attached.id) && pushed.contains(&area.id));
    assert!(!pushed.contains(&free.id), "{pushed:?}");
}

/// 约定不按归属分层：归属搁置的目标、`push_unattached` 关着，约定照常进每个 Run。
#[tokio::test]
async fn a_convention_is_not_layered_by_its_matter() {
    let rig = rig(8, false).await;
    rig.matter("g-parked", MatterKind::Goal, MatterState::Parked)
        .await;
    let convention = rig.convention("空调一律设 26 度", "g-parked").await;
    assert!(rig.pushed("毫不相干的查询").await.contains(&convention.id));
}

// ---------------------------------------------------------------- 结案与重开

/// 结案：线索（active、candidate）归档并升版，contested 不动，约定不动且被列出来；
/// 再跑一遍什么都不多做（幂等）；归档的不再被召回、`include_states` 明说时仍查得到。
/// 重开：归档的恢复成 active。
#[tokio::test]
async fn closing_archives_the_clues_lists_the_conventions_and_reopening_restores() {
    let rig = rig(8, true).await;
    rig.matter("g-1", MatterKind::Goal, MatterState::Active)
        .await;
    rig.matter("g-2", MatterKind::Goal, MatterState::Active)
        .await;
    let clue = rig.clue("空调 线索一", Some("g-1")).await;
    let other = rig.clue("空调 别的事项的线索", Some("g-2")).await;
    let convention = rig.convention("空调一律设 26 度", "g-1").await;
    let mut candidate = rig.clue("空调 候选线索", Some("g-1")).await;
    candidate.state = MemoryState::Candidate;
    let candidate = rig
        .repo
        .put(candidate.clone(), Some(candidate.revision))
        .await
        .unwrap();
    let mut contested = rig.clue("空调 有争议的线索", Some("g-1")).await;
    contested.state = MemoryState::Contested;
    let contested = rig
        .repo
        .put(contested.clone(), Some(contested.revision))
        .await
        .unwrap();

    rig.move_matter("g-1", MatterState::Done).await;
    let closed = rig
        .manager
        .archive_for(&MatterId::from_raw("g-1"))
        .await
        .unwrap();
    assert_eq!(closed.archived, 2, "active 与 candidate 的线索");
    assert_eq!(closed.conventions.len(), 1);
    assert_eq!(closed.conventions[0].id, convention.id);

    let archived = rig.get(&clue.id).await;
    assert_eq!(archived.state, MemoryState::Archived);
    assert_eq!(archived.revision, clue.revision + 1, "升版，展示过的行失效");
    assert_eq!(rig.get(&candidate.id).await.state, MemoryState::Archived);
    assert_eq!(rig.get(&contested.id).await.state, MemoryState::Contested);
    assert_eq!(rig.get(&convention.id).await.state, MemoryState::Active);
    assert_eq!(rig.get(&other.id).await.state, MemoryState::Active);

    let again = rig
        .manager
        .archive_for(&MatterId::from_raw("g-1"))
        .await
        .unwrap();
    assert_eq!(again.archived, 0, "幂等");
    assert_eq!(again.conventions.len(), 1, "约定仍然列出来");

    let pushed = rig.pushed("空调").await;
    assert!(!pushed.contains(&clue.id) && !pushed.contains(&candidate.id));
    assert!(pushed.contains(&other.id) && pushed.contains(&convention.id));
    let mut query = rig.manager.query("线索一", None);
    query.include_states = vec![MemoryState::Archived];
    let found = rig.manager.search(query).await.unwrap();
    assert!(
        found.items.iter().any(|item| item.id == clue.id),
        "明说才查得到"
    );
    assert!(
        found
            .items
            .iter()
            .all(|item| item.state == MemoryState::Archived),
        "只给归档的"
    );

    rig.move_matter("g-1", MatterState::Active).await;
    let reopened = rig
        .manager
        .restore_for(&MatterId::from_raw("g-1"))
        .await
        .unwrap();
    assert_eq!(reopened.restored, 2);
    assert_eq!(rig.get(&clue.id).await.state, MemoryState::Active);
    assert_eq!(rig.get(&clue.id).await.revision, clue.revision + 2);
    assert!(rig.pushed("空调").await.contains(&clue.id));
    assert_eq!(rig.get(&contested.id).await.state, MemoryState::Contested);
}

/// 事项已结案、归档还没跑（事项与记忆不在同一个事务里）：线索不会继续被推。
#[tokio::test]
async fn a_closed_matters_unarchived_clue_is_not_pushed_either() {
    let rig = rig(8, true).await;
    rig.matter("g-1", MatterKind::Goal, MatterState::Active)
        .await;
    let clue = rig.clue("空调 线索一", Some("g-1")).await;
    rig.move_matter("g-1", MatterState::Dropped).await;
    assert!(!rig.pushed("空调").await.contains(&clue.id));
}

// ---------------------------------------------------------------- add / classify

/// `add --for`：事项要存在且未结案；归属写进条目。
#[tokio::test]
async fn add_for_a_matter_validates_that_it_exists_and_is_open() {
    let rig = rig(8, true).await;
    rig.matter("g-1", MatterKind::Goal, MatterState::Active)
        .await;
    rig.matter("g-done", MatterKind::Goal, MatterState::Done)
        .await;

    let item = rig.clue("空调 线索", Some("g-1")).await;
    assert_eq!(item.belongs_to, Some(MatterId::from_raw("g-1")));
    for (matter, needle) in [("g-nope", "没有事项"), ("g-done", "已经")] {
        let error = rig
            .manager
            .add(&add_request("空调 另一条", Some(matter), None))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(needle), "{matter}：{error}");
    }
}

/// `classify --for`：升版、保留证据与确认；`none` 清空；指向不存在的事项被拒；不带任何改动被拒。
#[tokio::test]
async fn classify_for_bumps_the_revision_and_validates_the_matter() {
    let rig = rig(8, true).await;
    rig.matter("g-1", MatterKind::Goal, MatterState::Active)
        .await;
    rig.matter("g-2", MatterKind::Goal, MatterState::Active)
        .await;
    let item = rig.clue("空调 线索", None).await;

    let moved = rig
        .manager
        .classify(
            &item.id,
            item.revision,
            None,
            None,
            BelongsChange::Set(MatterId::from_raw("g-1")),
        )
        .await
        .unwrap();
    assert_eq!(moved.revision, item.revision + 1);
    assert_eq!(moved.belongs_to, Some(MatterId::from_raw("g-1")));
    assert_eq!(moved.application, item.application, "没给就沿用");
    assert_eq!(moved.confirmation, item.confirmation);

    // 同一个归属再来一次：不升版。
    let same = rig
        .manager
        .classify(
            &item.id,
            moved.revision,
            None,
            None,
            BelongsChange::Set(MatterId::from_raw("g-1")),
        )
        .await
        .unwrap();
    assert_eq!(same.revision, moved.revision);

    // 也可以和 application 一起改。
    let both = rig
        .manager
        .classify(
            &item.id,
            moved.revision,
            Some(MemoryApplication::Reference),
            None,
            BelongsChange::Set(MatterId::from_raw("g-2")),
        )
        .await
        .unwrap();
    assert_eq!(both.belongs_to, Some(MatterId::from_raw("g-2")));
    assert_eq!(both.revision, moved.revision + 1);

    let cleared = rig
        .manager
        .classify(&item.id, both.revision, None, None, BelongsChange::Clear)
        .await
        .unwrap();
    assert_eq!(cleared.belongs_to, None);
    assert_eq!(cleared.revision, both.revision + 1);

    for (belongs, application) in [
        (BelongsChange::Set(MatterId::from_raw("g-nope")), None),
        (BelongsChange::Keep, None),
    ] {
        let error = rig
            .manager
            .classify(&item.id, cleared.revision, application, None, belongs)
            .await
            .unwrap_err();
        assert!(matches!(error, MemoryError::Invalid(_)), "{error}");
    }
}

/// 归档的条目：改挂到进行中的事（或清空）回到 active；只改分类不改归属仍是归档的。
#[tokio::test]
async fn an_archived_clue_comes_back_only_when_it_is_rehomed() {
    let rig = rig(8, true).await;
    rig.matter("g-1", MatterKind::Goal, MatterState::Active)
        .await;
    rig.matter("a-1", MatterKind::Area, MatterState::Active)
        .await;
    let clue = rig.clue("空调 线索", Some("g-1")).await;
    rig.move_matter("g-1", MatterState::Done).await;
    rig.manager
        .archive_for(&MatterId::from_raw("g-1"))
        .await
        .unwrap();
    let archived = rig.get(&clue.id).await;

    let kept = rig
        .manager
        .classify(
            &archived.id,
            archived.revision,
            Some(MemoryApplication::Reference),
            Some("某个条件"),
            BelongsChange::Keep,
        )
        .await
        .unwrap();
    assert_eq!(kept.state, MemoryState::Archived);

    let rehomed = rig
        .manager
        .classify(
            &kept.id,
            kept.revision,
            None,
            None,
            BelongsChange::Set(MatterId::from_raw("a-1")),
        )
        .await
        .unwrap();
    assert_eq!(rehomed.state, MemoryState::Active);
    assert!(rig.pushed("空调").await.contains(&clue.id));

    // 再说一遍同一句话、挂回活的事：同样回来；不带归属则提示去重开或改挂。
    rig.move_matter("a-1", MatterState::Retired).await;
    rig.manager
        .archive_for(&MatterId::from_raw("a-1"))
        .await
        .unwrap();
    let error = rig
        .manager
        .add(&add_request("空调 线索", None, None))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("归档"), "{error}");
}

/// 没接事项的管理器：`--for` 报清楚，不是悄悄忽略。
#[tokio::test]
async fn add_for_without_a_matter_store_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::connect(dir.path().join("aux.db")).await.unwrap();
    let repo = Arc::new(TursoMemoryRepo::new(db));
    let manager = MemoryManager::new(MemoryParts {
        config: memory_config(true, RetrievalMode::Keyword),
        repo: Arc::clone(&repo) as Arc<dyn MemoryRepo>,
        catalog: Arc::clone(&repo) as Arc<dyn MemoryCatalog>,
        embeddings: None,
        reranker: None,
        llm: Arc::new(ScriptedLlm::new(Vec::new())) as Arc<dyn LlmClient>,
        events: Arc::new(SeedEvents::default()) as Arc<dyn SessionEvents>,
        work: MemWork::with(Vec::new()) as Arc<dyn MemoryWorkLog>,
        clock: Arc::new(FixedClock),
        render: test_render,
        matters: None,
    });
    let error = manager
        .add(&add_request("空调 线索", Some("g-1"), None))
        .await
        .unwrap_err();
    assert!(matches!(error, MemoryError::Invalid(_)), "{error}");
}

// ---------------------------------------------------------------- 提取（P1d）

fn matter_table(rig_matters: &[(&str, MatterKind, MatterState)]) -> Vec<Matter> {
    rig_matters
        .iter()
        .map(|(id, kind, state)| {
            let mut matter = NewMatter {
                kind: *kind,
                title: format!("事项 {id}"),
                next: None,
                due: None,
                standard: None,
            }
            .build(MatterId::from_raw(*id), NOW)
            .unwrap();
            matter.state = *state;
            matter
        })
        .collect()
}

fn belongs_raw(content: &str, belongs: &str) -> String {
    format!(
        r#"{{"content":"{content}","kind":"fact","scope":"personal","said_by":"user_statement",
            "evidence":["evt-1"]{belongs}}}"#
    )
}

/// 归属建议是编号：在表里就映射成 id（`g1` 与整数 `0` 都收）；越界、读不懂、没给的都当作没有
/// 归属，观察本身一条不丢。
#[test]
fn a_suggested_matter_number_is_validated_against_the_table() {
    let run = RunId::from_raw("run-1");
    let events = vec![user_event(1, &run, "我在做发布观察，空调这条记一下")];
    let table = matter_table(&[
        ("g-0", MatterKind::Goal, MatterState::Active),
        ("a-1", MatterKind::Area, MatterState::Active),
    ]);
    let raw: Vec<RawObservation> = serde_json::from_str(&format!(
        "[{}]",
        [
            belongs_raw("字符串编号", r#","belongs_to":"g1""#),
            belongs_raw("整数编号", r#","belongs_to":0"#),
            belongs_raw("越界编号", r#","belongs_to":"g9""#),
            belongs_raw("读不懂", r#","belongs_to":"owl 发布观察""#),
            belongs_raw("空值", r#","belongs_to":null"#),
            belongs_raw("没给", ""),
        ]
        .join(",")
    ))
    .unwrap();

    let observations =
        transcript(&events).validate(raw, 8, &model("memory-model", None), &[], &table, NOW);
    assert_eq!(observations.len(), 6, "坏编号不拖累观察");
    let belongs: Vec<Option<&str>> = observations
        .iter()
        .map(|observation| observation.belongs_to.as_ref().map(MatterId::as_str))
        .collect();
    assert_eq!(
        belongs,
        [Some("a-1"), Some("g-0"), None, None, None, None],
        "{belongs:?}"
    );
}

/// 没有事项表（私有记忆的批次、没有进行中的事）时，任何编号都不生效。
#[test]
fn a_matter_number_means_nothing_without_a_table() {
    let run = RunId::from_raw("run-1");
    let events = vec![user_event(1, &run, "记一下")];
    let raw: Vec<RawObservation> = serde_json::from_str(&format!(
        "[{}]",
        belongs_raw("一条", r#","belongs_to":"g0""#)
    ))
    .unwrap();
    let observations =
        transcript(&events).validate(raw, 8, &model("memory-model", None), &[], &[], NOW);
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].belongs_to, None);
}

/// 新建的条目带建议的归属；撞上已有条目（同一句话）走合并，不改已有条目的归属。
#[tokio::test]
async fn extraction_sets_belongs_on_new_items_and_never_changes_an_existing_one() {
    let rig = rig(8, true).await;
    rig.matter("g-0", MatterKind::Goal, MatterState::Active)
        .await;
    rig.matter("g-1", MatterKind::Goal, MatterState::Active)
        .await;
    let existing = rig.clue("已有的一句话", Some("g-1")).await;
    let run = RunId::from_raw("run-1");
    let events = vec![user_event(1, &run, "说了两件事")];
    let table = matter_table(&[
        ("g-0", MatterKind::Goal, MatterState::Active),
        ("g-1", MatterKind::Goal, MatterState::Active),
    ]);
    let raw: Vec<RawObservation> = serde_json::from_str(&format!(
        "[{},{}]",
        belongs_raw("新的一句话", r#","belongs_to":"g0""#),
        belongs_raw("已有的一句话", r#","belongs_to":"g0""#),
    ))
    .unwrap();
    let observations =
        transcript(&events).validate(raw, 8, &model("memory-model", None), &[], &table, NOW);
    Consolidator::new(&rig.manager)
        .apply(&observations)
        .await
        .unwrap();

    let all = rig.repo.list(None, None, 10).await.unwrap();
    let fresh = all
        .iter()
        .find(|item| item.content == "新的一句话")
        .unwrap();
    assert_eq!(fresh.belongs_to, Some(MatterId::from_raw("g-0")));
    assert_eq!(
        rig.get(&existing.id).await.belongs_to,
        Some(MatterId::from_raw("g-1")),
        "提取不改已有条目的归属"
    );
}

/// 提示词给模型一张编号的事项表（自己的命名空间 g0、g1…），没有事项就整段省掉。
#[test]
fn the_extraction_prompt_numbers_the_open_matters_in_their_own_namespace() {
    let run = RunId::from_raw("run-1");
    let events = vec![user_event(1, &run, "记一下")];
    let transcript = transcript(&events);
    let table = matter_table(&[
        ("g-0", MatterKind::Goal, MatterState::Active),
        ("g-1", MatterKind::Goal, MatterState::Parked),
        ("a-2", MatterKind::Area, MatterState::Active),
    ]);
    let prompt = extract::user_prompt(&transcript, &[], &[], &table, NOW);
    assert!(prompt.contains("--- 进行中的事项 ---"), "{prompt}");
    assert!(prompt.contains("[g0] 目标：事项 g-0"), "{prompt}");
    assert!(prompt.contains("[g1] 目标（parked）：事项 g-1"), "{prompt}");
    assert!(prompt.contains("[g2] 领域：事项 a-2"), "{prompt}");
    assert!(
        prompt.find("进行中的事项").unwrap() < prompt.find("--- 会话记录 ---").unwrap(),
        "表在记录之前"
    );

    let none = extract::user_prompt(&transcript, &[], &[], &[], NOW);
    assert!(!none.contains("进行中的事项"), "{none}");
}
