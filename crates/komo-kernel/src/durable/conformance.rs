//! `Storage` 的 conformance 套件（移植 pi-durable 的 `testing/storage-conformance.ts`）。
//!
//! 每个用例拿一个**全新、空的** `&dyn Storage`，失败就 panic。内存实现用
//! [`run_all`] 跑；D2 的 Turso 实现同样调它，换一个建库的闭包即可。document 部分只覆盖
//! 整值模型（无历史回看、无 delta、无 copy）。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use super::ids::{
    ConversationId, DocumentId, EntryId, ROOT_CONVERSATION_ID, Seq, SubmissionId, TaskId,
};
use super::records::{
    ConversationOwner, ConversationRecord, DocumentAddress, DocumentScope, EntryRecord, InputState,
    JoinPolicy, SubmissionKind, SubmissionRecord, SubmissionStatus, TaskOutcome, TaskOutcomeError,
    TaskRecord, TaskState, TaskStatus, WriteState,
};
use super::storage::{
    ConversationQuery, Cursor, DocumentCreate, DocumentQuery, EntryQuery, MAX_ID, ScanOrder,
    Storage, StorageError, StorageExt, StorageWrite, SubmissionQuery, TaskQuery,
};

pub type CaseFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// 一个用例：名字加一个吃 `&dyn Storage` 的异步函数。
pub struct Case {
    pub name: &'static str,
    pub run: for<'a> fn(&'a dyn Storage) -> CaseFuture<'a>,
}

macro_rules! case {
    ($name:ident) => {
        Case {
            name: stringify!($name),
            run: |storage| Box::pin($name(storage)),
        }
    };
}

/// 全部用例。
pub fn cases() -> Vec<Case> {
    vec![
        case!(root_id_is_reserved_and_conversations_are_immutable),
        case!(mixed_commit_is_atomic_and_rolls_back_on_failure),
        case!(reads_are_detached_copies),
        case!(entries_committed_out_of_id_order_are_indexed),
        case!(entry_cursor_continues_below_its_last_item_after_a_newer_commit),
        case!(conversations_paginate_by_cursor_in_ascending_order),
        case!(conversations_filter_by_owner_edges),
        case!(deep_fork_history_scans_through_every_ancestor_cap),
        case!(tables_scan_in_either_order_and_a_cursor_keeps_its_direction),
        case!(task_records_replace_whole_and_filter),
        case!(owned_waiting_and_completing_tasks_scan_by_status),
        case!(request_ids_index_per_conversation_and_replace_whole_submissions),
        case!(request_ids_are_unique_within_a_conversation),
        case!(write_submissions_store_without_input_only_states),
        case!(documents_store_whole_values_and_read_only_live_incarnations),
        case!(documents_enforce_one_live_incarnation_per_address),
        case!(document_scans_page_by_scope_and_kind),
        case!(document_identities_stay_lossless),
        case!(record_ids_share_one_namespace_and_minting_stops_at_the_limit),
        case!(invalid_reads_are_request_errors_without_effects),
        case!(every_operation_is_rejected_after_close),
    ]
}

/// 对每个用例建一个全新的存储并跑它。`new_storage` 每次返回一个空库。
pub async fn run_all<F, Fut>(new_storage: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn Storage>>,
{
    for case in cases() {
        eprintln!("storage conformance: {}", case.name);
        let storage = new_storage().await;
        (case.run)(&*storage).await;
    }
}

// ------------------------------------------------------------------- helpers

const ROOT: ConversationId = ROOT_CONVERSATION_ID;

async fn commit(storage: &dyn Storage, writes: Vec<StorageWrite>) -> Seq {
    storage.commit(&writes).await.expect("commit")
}

async fn create_root(storage: &dyn Storage) -> ConversationId {
    commit(
        storage,
        vec![StorageWrite::Conversation(ConversationRecord::root(ROOT))],
    )
    .await;
    ROOT
}

async fn mint<I: super::ids::EntityId + Send>(storage: &dyn Storage) -> I {
    storage.mint::<I>().await.expect("mint")
}

fn entry(id: EntryId, conversation: ConversationId) -> EntryRecord {
    EntryRecord::new(id, conversation, "message")
}

fn put(record: EntryRecord) -> StorageWrite {
    StorageWrite::Entry(record)
}

fn pending_task(id: TaskId, conversation: ConversationId, phase: &str) -> TaskRecord {
    TaskRecord::pending(
        id,
        conversation,
        "test.task",
        json!({"value": id.get()}),
        json!({"phase": phase}),
    )
}

fn ids<T>(items: &[T], id: impl Fn(&T) -> u64) -> Vec<u64> {
    items.iter().map(id).collect()
}

fn eids(items: &[EntryRecord]) -> Vec<u64> {
    ids(items, |entry| entry.id.get())
}

fn input(
    id: SubmissionId,
    conversation: ConversationId,
    request: Option<&str>,
) -> SubmissionRecord {
    SubmissionRecord::queued_input(id, conversation, request)
}

fn with_state(mut record: SubmissionRecord, state: InputState) -> SubmissionRecord {
    record.kind = SubmissionKind::Input { state };
    record
}

/// cursor 经过一次 JSON 来回：调用方可以把它存起来再交回。
fn round_trip(cursor: &Cursor) -> Cursor {
    serde_json::from_str(&serde_json::to_string(cursor).unwrap()).unwrap()
}

fn expect_rejected<T: std::fmt::Debug>(result: Result<T, StorageError>) {
    match result {
        Err(StorageError::Rejected(_)) => {}
        other => panic!("expected a rejected commit, got {other:?}"),
    }
}

fn expect_request<T: std::fmt::Debug>(result: Result<T, StorageError>) {
    match result {
        Err(StorageError::Request(_)) => {}
        other => panic!("expected an invalid-request error, got {other:?}"),
    }
}

fn doc_address(kind: &str, scope: DocumentScope, key: Option<&str>) -> DocumentAddress {
    DocumentAddress {
        kind: kind.to_owned(),
        scope,
        key: key.map(str::to_owned),
    }
}

fn doc_create(id: DocumentId, address: DocumentAddress, value: Value) -> StorageWrite {
    StorageWrite::DocumentCreate(DocumentCreate {
        id,
        address,
        version: 1,
        value,
    })
}

// --------------------------------------------------------------------- cases

async fn root_id_is_reserved_and_conversations_are_immutable(storage: &dyn Storage) {
    assert_eq!(
        storage.mint_id().await.unwrap(),
        2,
        "ID 1 belongs to the root"
    );
    create_root(storage).await;
    assert_eq!(
        storage.conversation(ROOT).await.unwrap(),
        Some(ConversationRecord::root(ROOT))
    );
    expect_rejected(
        storage
            .commit(&[StorageWrite::Conversation(ConversationRecord::root(ROOT))])
            .await,
    );
    assert_eq!(
        storage.conversation(ConversationId::new(99)).await.unwrap(),
        None
    );
}

async fn mixed_commit_is_atomic_and_rolls_back_on_failure(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let entry_id: EntryId = mint(storage).await;
    let task_id: TaskId = mint(storage).await;
    let submission_id: SubmissionId = mint(storage).await;
    let task = pending_task(task_id, root, "ready");
    let placed = with_state(
        input(submission_id, root, Some("request-1")),
        InputState::Placed { entry: entry_id },
    );
    let first = commit(
        storage,
        vec![
            put(entry(entry_id, root).with_data(json!({"text": "hello"}))),
            StorageWrite::Task(task.clone()),
            StorageWrite::Submission(placed.clone()),
        ],
    )
    .await;

    let stored = storage.entry(entry_id).await.unwrap().unwrap();
    assert_eq!(
        stored.entry,
        entry(entry_id, root).with_data(json!({"text": "hello"}))
    );
    assert_eq!(stored.commit_seq, first);
    assert_eq!(storage.task(task_id).await.unwrap(), Some(task.clone()));
    assert_eq!(
        storage.submission(submission_id).await.unwrap(),
        Some(placed.clone())
    );

    // 最后一条写入非法（重复建根）：前面的写入一条都不能留下。
    let transient: EntryId = mint(storage).await;
    let mut running = task.clone();
    running.state = TaskState::Running {
        checkpoint: json!({"phase": "effect"}),
    };
    let done = with_state(
        placed.clone(),
        InputState::Done {
            entry: entry_id,
            answer: transient,
        },
    );
    expect_rejected(
        storage
            .commit(&[
                StorageWrite::Task(running),
                StorageWrite::Submission(done),
                put(entry(transient, root)),
                StorageWrite::Conversation(ConversationRecord::root(ROOT)),
            ])
            .await,
    );
    assert_eq!(storage.task(task_id).await.unwrap(), Some(task));
    assert_eq!(
        storage.submission(submission_id).await.unwrap(),
        Some(placed)
    );
    assert_eq!(storage.entry(transient).await.unwrap(), None);

    let next: EntryId = mint(storage).await;
    let after = commit(storage, vec![put(entry(next, root))]).await;
    assert!(
        after > first,
        "sequences strictly increase across a rolled-back batch"
    );
}

async fn reads_are_detached_copies(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let entry_id: EntryId = mint(storage).await;
    let task_id: TaskId = mint(storage).await;
    let submission_id: SubmissionId = mint(storage).await;
    let document_id: DocumentId = mint(storage).await;
    let address = doc_address("test.doc", DocumentScope::Session, None);
    commit(
        storage,
        vec![
            put(entry(entry_id, root).with_data(json!({"nested": [1, 2]}))),
            StorageWrite::Task(pending_task(task_id, root, "ready")),
            StorageWrite::Submission(with_state(
                input(submission_id, root, None),
                InputState::Unanswered {
                    entry: None,
                    reason: "failed".into(),
                    detail: Some(json!({"codes": ["initial"]})),
                },
            )),
            doc_create(document_id, address, json!({"count": 1})),
        ],
    )
    .await;

    // 改读出来的值，再读一遍：存的不受影响。
    let mut read = storage.entry(entry_id).await.unwrap().unwrap().entry;
    read.data = Some(json!("mutated"));
    let mut task = storage.task(task_id).await.unwrap().unwrap();
    task.state = TaskState::Terminal {
        outcome: TaskOutcome::Completed { result: json!(1) },
    };
    let mut submission = storage.submission(submission_id).await.unwrap().unwrap();
    submission.request_id = Some("mutated".into());
    let mut document = storage.document(document_id).await.unwrap().unwrap();
    document.value = json!({"count": 99});

    assert_eq!(
        storage.entry(entry_id).await.unwrap().unwrap().entry.data,
        Some(json!({"nested": [1, 2]}))
    );
    assert_eq!(
        storage.task(task_id).await.unwrap().unwrap().state.status(),
        TaskStatus::Pending
    );
    assert_eq!(
        storage
            .submission(submission_id)
            .await
            .unwrap()
            .unwrap()
            .request_id,
        None
    );
    assert_eq!(
        storage.document(document_id).await.unwrap().unwrap().value,
        json!({"count": 1})
    );
}

async fn entries_committed_out_of_id_order_are_indexed(storage: &dyn Storage) {
    let root = create_root(storage).await;
    commit(
        storage,
        vec![
            put(entry(EntryId::new(30), root)),
            put(entry(EntryId::new(10), root)),
            put(entry(EntryId::new(20), root).with_head(EntryId::new(10))),
        ],
    )
    .await;
    let page = storage
        .scan_entries(&EntryQuery::new(root), 10, None)
        .await
        .unwrap();
    assert_eq!(eids(&page.items), [30, 20, 10]);
    let marker = storage
        .find_latest_head_marker(root, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(marker.id, EntryId::new(20));
    assert_eq!(marker.head, Some(EntryId::new(10)));
}

async fn entry_cursor_continues_below_its_last_item_after_a_newer_commit(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let oldest: EntryId = mint(storage).await;
    let middle: EntryId = mint(storage).await;
    let newest: EntryId = mint(storage).await;
    commit(
        storage,
        vec![
            put(entry(oldest, root)),
            put(entry(middle, root)),
            put(entry(newest, root)),
        ],
    )
    .await;
    let query = EntryQuery::new(root);
    let first = storage.scan_entries(&query, 2, None).await.unwrap();
    assert_eq!(eids(&first.items), [newest.get(), middle.get()]);
    let appended: EntryId = mint(storage).await;
    commit(storage, vec![put(entry(appended, root))]).await;
    let second = storage
        .scan_entries(&query, 2, first.next.as_ref())
        .await
        .unwrap();
    assert_eq!(eids(&second.items), [oldest.get()]);
    assert_eq!(second.next, None);
}

async fn conversations_paginate_by_cursor_in_ascending_order(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let second: ConversationId = mint(storage).await;
    let third: ConversationId = mint(storage).await;
    commit(
        storage,
        vec![
            StorageWrite::Conversation(ConversationRecord::root(third)),
            StorageWrite::Conversation(ConversationRecord::root(second)),
        ],
    )
    .await;
    let query = ConversationQuery::default();
    let first = storage.scan_conversations(&query, 2, None).await.unwrap();
    assert_eq!(
        ids(&first.items, |c| c.id.get()),
        [root.get(), second.get()]
    );
    let cursor = round_trip(first.next.as_ref().expect("a second page"));
    let rest = storage
        .scan_conversations(&query, 2, Some(&cursor))
        .await
        .unwrap();
    assert_eq!(ids(&rest.items, |c| c.id.get()), [third.get()]);
    assert_eq!(rest.next, None);
}

async fn conversations_filter_by_owner_edges(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let other_owner: ConversationId = mint(storage).await;
    let first_task: TaskId = mint(storage).await;
    let second_task: TaskId = mint(storage).await;
    let first: ConversationId = mint(storage).await;
    let second: ConversationId = mint(storage).await;
    let third: ConversationId = mint(storage).await;
    let owned = |id, conversation, task| {
        StorageWrite::Conversation(ConversationRecord {
            id,
            parent: None,
            owner: Some(ConversationOwner { conversation, task }),
        })
    };
    commit(
        storage,
        vec![
            StorageWrite::Conversation(ConversationRecord::root(other_owner)),
            owned(first, root, first_task),
            owned(second, root, second_task),
            owned(third, other_owner, first_task),
        ],
    )
    .await;

    let by_root = ConversationQuery {
        owner_conversation: Some(root),
        ..Default::default()
    };
    let page = storage.scan_conversations(&by_root, 1, None).await.unwrap();
    assert_eq!(ids(&page.items, |c| c.id.get()), [first.get()]);
    let page = storage
        .scan_conversations(&by_root, 1, page.next.as_ref())
        .await
        .unwrap();
    assert_eq!(ids(&page.items, |c| c.id.get()), [second.get()]);
    assert_eq!(page.next, None);

    let by_task = ConversationQuery {
        owner_task: Some(first_task),
        ..Default::default()
    };
    let page = storage
        .scan_conversations(&by_task, 10, None)
        .await
        .unwrap();
    assert_eq!(ids(&page.items, |c| c.id.get()), [first.get(), third.get()]);

    let both = ConversationQuery {
        owner_conversation: Some(root),
        owner_task: Some(first_task),
        ..Default::default()
    };
    let page = storage.scan_conversations(&both, 10, None).await.unwrap();
    assert_eq!(ids(&page.items, |c| c.id.get()), [first.get()]);
}

async fn deep_fork_history_scans_through_every_ancestor_cap(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let root_first: EntryId = mint(storage).await;
    let root_fork_point: EntryId = mint(storage).await;
    let root_excluded_same_commit: EntryId = mint(storage).await;
    let root_seq = commit(
        storage,
        vec![
            put(entry(root_first, root)),
            put(EntryRecord::new(root_fork_point, root, "marker").with_head(root_first)),
            put(entry(root_excluded_same_commit, root)),
        ],
    )
    .await;
    let child: ConversationId = mint(storage).await;
    commit(
        storage,
        vec![StorageWrite::Conversation(ConversationRecord::fork(
            child,
            root,
            root_fork_point,
        ))],
    )
    .await;
    let child_fork_point: EntryId = mint(storage).await;
    let child_excluded: EntryId = mint(storage).await;
    commit(
        storage,
        vec![
            put(EntryRecord::new(child_fork_point, child, "note")),
            put(entry(child_excluded, child)),
        ],
    )
    .await;
    let root_excluded_later: EntryId = mint(storage).await;
    commit(storage, vec![put(entry(root_excluded_later, root))]).await;
    let grandchild: ConversationId = mint(storage).await;
    commit(
        storage,
        vec![StorageWrite::Conversation(ConversationRecord::fork(
            grandchild,
            child,
            child_fork_point,
        ))],
    )
    .await;
    let grandchild_head: EntryId = mint(storage).await;
    let grandchild_tail: EntryId = mint(storage).await;
    let grandchild_seq = commit(
        storage,
        vec![
            put(EntryRecord::new(grandchild_head, grandchild, "marker").with_head(grandchild_head)),
            put(entry(grandchild_tail, grandchild)),
        ],
    )
    .await;
    let child_excluded_later: EntryId = mint(storage).await;
    commit(storage, vec![put(entry(child_excluded_later, child))]).await;

    // 降序：沿每个祖先的上限一页页往上读。
    let query = EntryQuery::new(grandchild);
    let first = storage.scan_entries(&query, 2, None).await.unwrap();
    assert_eq!(
        eids(&first.items),
        [grandchild_tail.get(), grandchild_head.get()]
    );
    let second = storage
        .scan_entries(&query, 2, first.next.as_ref())
        .await
        .unwrap();
    assert_eq!(
        eids(&second.items),
        [child_fork_point.get(), root_fork_point.get()]
    );
    let third = storage
        .scan_entries(&query, 2, second.next.as_ref())
        .await
        .unwrap();
    assert_eq!(eids(&third.items), [root_first.get()]);
    assert_eq!(third.next, None);

    // 升序：先读根一段，再读各 fork 的，每段截在下一个 fork 点。
    let ascending = EntryQuery {
        order: Some(ScanOrder::Ascending),
        ..EntryQuery::new(grandchild)
    };
    let first_up = storage.scan_entries(&ascending, 2, None).await.unwrap();
    assert_eq!(
        eids(&first_up.items),
        [root_first.get(), root_fork_point.get()]
    );
    let second_up = storage
        .scan_entries(&ascending, 2, first_up.next.as_ref())
        .await
        .unwrap();
    assert_eq!(
        eids(&second_up.items),
        [child_fork_point.get(), grandchild_head.get()]
    );
    // 续扫时查询可以不写方向。
    let third_up = storage
        .scan_entries(&query, 2, second_up.next.as_ref())
        .await
        .unwrap();
    assert_eq!(eids(&third_up.items), [grandchild_tail.get()]);
    assert_eq!(third_up.next, None);
    let bounded = EntryQuery {
        min: Some(root_fork_point),
        max: Some(child_fork_point),
        ..ascending.clone()
    };
    let page = storage.scan_entries(&bounded, 10, None).await.unwrap();
    assert_eq!(
        eids(&page.items),
        [root_fork_point.get(), child_fork_point.get()]
    );
    // 方向和 cursor 对不上：请求无效。
    let descending = EntryQuery {
        order: Some(ScanOrder::Descending),
        ..EntryQuery::new(grandchild)
    };
    expect_request(
        storage
            .scan_entries(&descending, 2, first_up.next.as_ref())
            .await,
    );

    // head 标记。
    let current = storage
        .find_latest_head_marker(grandchild, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (current.id, current.head),
        (grandchild_head, Some(grandchild_head))
    );
    let historical = storage
        .find_latest_head_marker(grandchild, Some(child_fork_point))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (historical.id, historical.head),
        (root_fork_point, Some(root_first))
    );
    assert_eq!(
        storage
            .find_latest_head_marker(grandchild, Some(root_first))
            .await
            .unwrap(),
        None
    );

    // 从标记的 head 起扫当前上下文，以及停在 child_fork_point 的历史上下文。
    let active = EntryQuery {
        min: current.head,
        ..EntryQuery::new(grandchild)
    };
    let page = storage.scan_entries(&active, 1, None).await.unwrap();
    assert_eq!(eids(&page.items), [grandchild_tail.get()]);
    let page = storage
        .scan_entries(&active, 1, page.next.as_ref())
        .await
        .unwrap();
    assert_eq!(eids(&page.items), [grandchild_head.get()]);
    assert_eq!(page.next, None);
    let historical_range = EntryQuery {
        min: historical.head,
        max: Some(child_fork_point),
        ..EntryQuery::new(grandchild)
    };
    let page = storage
        .scan_entries(&historical_range, 10, None)
        .await
        .unwrap();
    assert_eq!(
        eids(&page.items),
        [
            child_fork_point.get(),
            root_fork_point.get(),
            root_first.get()
        ]
    );

    // 按 ID 全局查，和带 conversation 的可见性查询。
    let stored = storage.entry(root_first).await.unwrap().unwrap();
    assert_eq!(
        (stored.entry, stored.commit_seq),
        (entry(root_first, root), root_seq)
    );
    assert_eq!(
        storage
            .entry(root_fork_point)
            .await
            .unwrap()
            .unwrap()
            .commit_seq,
        root_seq
    );
    assert_eq!(
        storage
            .entry(grandchild_head)
            .await
            .unwrap()
            .unwrap()
            .commit_seq,
        grandchild_seq
    );
    assert_eq!(storage.entry(EntryId::new(999_999)).await.unwrap(), None);
    let visible = storage
        .visible_entry(grandchild, root_first)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (visible.entry, visible.commit_seq),
        (entry(root_first, root), root_seq)
    );
    assert_eq!(
        storage
            .visible_entry(grandchild, child_fork_point)
            .await
            .unwrap()
            .unwrap()
            .entry
            .conversation,
        child
    );
    assert_eq!(
        storage
            .visible_entry(grandchild, grandchild_tail)
            .await
            .unwrap()
            .unwrap()
            .commit_seq,
        grandchild_seq
    );
    for hidden in [
        root_excluded_same_commit,
        root_excluded_later,
        child_excluded,
        child_excluded_later,
        EntryId::new(999_999),
    ] {
        assert_eq!(
            storage.visible_entry(grandchild, hidden).await.unwrap(),
            None,
            "{hidden}"
        );
    }
    assert_eq!(
        storage.visible_entry(root, grandchild_head).await.unwrap(),
        None
    );
    expect_request(
        storage
            .visible_entry(ConversationId::new(999_999), root_first)
            .await,
    );
    expect_request(
        storage
            .scan_entries(&EntryQuery::new(ConversationId::new(999_999)), 10, None)
            .await,
    );
    expect_request(
        storage
            .find_latest_head_marker(ConversationId::new(999_999), None)
            .await,
    );
}

async fn tables_scan_in_either_order_and_a_cursor_keeps_its_direction(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let mut conversations = vec![root.get()];
    let mut tasks = Vec::new();
    let mut submissions = Vec::new();
    for _ in 0..3 {
        let conversation: ConversationId = mint(storage).await;
        let task: TaskId = mint(storage).await;
        let submission: SubmissionId = mint(storage).await;
        commit(
            storage,
            vec![
                StorageWrite::Conversation(ConversationRecord::root(conversation)),
                StorageWrite::Task(pending_task(task, root, "ready")),
                StorageWrite::Submission(input(submission, root, None)),
            ],
        )
        .await;
        conversations.push(conversation.get());
        tasks.push(task.get());
        submissions.push(submission.get());
    }

    // 三张表各自：升序、降序、省略方向都按 ID 次序翻完，续扫时 cursor 自带方向。
    for table in 0..3 {
        let expected = [&conversations, &tasks, &submissions][table];
        let reversed: Vec<u64> = expected.iter().rev().copied().collect();
        let scan = |order: Option<ScanOrder>, cursor: Option<Cursor>| async move {
            let cursor = cursor.as_ref();
            match table {
                0 => {
                    let query = ConversationQuery {
                        order,
                        ..Default::default()
                    };
                    let page = storage.scan_conversations(&query, 2, cursor).await;
                    page.map(|p| (ids(&p.items, |c| c.id.get()), p.next))
                }
                1 => {
                    let query = TaskQuery {
                        order,
                        ..Default::default()
                    };
                    let page = storage.scan_tasks(&query, 2, cursor).await;
                    page.map(|p| (ids(&p.items, |t| t.id.get()), p.next))
                }
                _ => {
                    let query = SubmissionQuery {
                        order,
                        ..Default::default()
                    };
                    let page = storage.scan_submissions(&query, 2, cursor).await;
                    page.map(|p| (ids(&p.items, |s| s.id.get()), p.next))
                }
            }
        };
        let all = |order: Option<ScanOrder>| {
            let scan = &scan;
            async move {
                let mut found = Vec::new();
                let (mut items, mut next) = scan(order, None).await.unwrap();
                found.append(&mut items);
                while let Some(cursor) = next {
                    // cursor 带着方向，续扫的查询可以不写。
                    (items, next) = scan(None, Some(round_trip(&cursor))).await.unwrap();
                    found.append(&mut items);
                }
                found
            }
        };
        assert_eq!(all(None).await, *expected, "table {table} default");
        assert_eq!(
            all(Some(ScanOrder::Ascending)).await,
            *expected,
            "table {table} ascending"
        );
        assert_eq!(
            all(Some(ScanOrder::Descending)).await,
            reversed,
            "table {table} descending"
        );
        let (_, next) = scan(Some(ScanOrder::Descending), None).await.unwrap();
        let cursor = next.unwrap();
        let (second, _) = scan(Some(ScanOrder::Descending), Some(cursor.clone()))
            .await
            .unwrap();
        assert_eq!(second, reversed[2..reversed.len().min(4)]);
        expect_request(scan(Some(ScanOrder::Ascending), Some(cursor)).await);
    }
}

async fn task_records_replace_whole_and_filter(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let first: TaskId = mint(storage).await;
    let second: TaskId = mint(storage).await;
    let third: TaskId = mint(storage).await;
    let mut first_task = pending_task(first, root, "ready");
    first_task.memos = Some([("winner".to_owned(), json!("first"))].into());
    let mut second_task = pending_task(second, root, "ready");
    second_task.background = true;
    let mut third_task = pending_task(third, root, "ready");
    third_task.abort_requested = true;
    commit(
        storage,
        vec![
            StorageWrite::Task(first_task.clone()),
            StorageWrite::Task(second_task),
            StorageWrite::Task(third_task),
        ],
    )
    .await;
    assert_eq!(storage.task(first).await.unwrap(), Some(first_task.clone()));

    let mut running = first_task.clone();
    running.state = TaskState::Running {
        checkpoint: json!({"phase": "effect", "attempt": 1}),
    };
    running.abort_requested = true;
    commit(storage, vec![StorageWrite::Task(running.clone())]).await;
    assert_eq!(storage.task(first).await.unwrap(), Some(running.clone()));

    // 整条替换：终态不带 memos，旧的 checkpoint 也不残留。
    let mut terminal = pending_task(first, root, "ready");
    terminal.abort_requested = true;
    terminal.state = TaskState::Terminal {
        outcome: TaskOutcome::Completed {
            result: json!({"entryId": 99}),
        },
    };
    commit(storage, vec![StorageWrite::Task(terminal.clone())]).await;
    assert_eq!(storage.task(first).await.unwrap(), Some(terminal.clone()));

    let pending = TaskQuery {
        status: Some(TaskStatus::Pending),
        ..Default::default()
    };
    let page = storage.scan_tasks(&pending, 1, None).await.unwrap();
    assert_eq!(ids(&page.items, |t| t.id.get()), [second.get()]);
    let page = storage
        .scan_tasks(&pending, 1, page.next.as_ref())
        .await
        .unwrap();
    assert_eq!(ids(&page.items, |t| t.id.get()), [third.get()]);
    let aborted = TaskQuery {
        status: Some(TaskStatus::Terminal),
        abort_requested: Some(true),
        ..Default::default()
    };
    assert_eq!(
        storage.scan_tasks(&aborted, 10, None).await.unwrap().items,
        [terminal]
    );
    let background = TaskQuery {
        background: Some(true),
        ..Default::default()
    };
    let page = storage.scan_tasks(&background, 10, None).await.unwrap();
    assert_eq!(ids(&page.items, |t| t.id.get()), [second.get()]);
    let by_kind = TaskQuery {
        kind: Some("test.task".into()),
        conversation: Some(root),
        ..Default::default()
    };
    assert_eq!(
        storage
            .scan_tasks(&by_kind, 10, None)
            .await
            .unwrap()
            .items
            .len(),
        3
    );
    let other_kind = TaskQuery {
        kind: Some("other".into()),
        ..Default::default()
    };
    assert!(
        storage
            .scan_tasks(&other_kind, 10, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

async fn owned_waiting_and_completing_tasks_scan_by_status(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let owner_id: TaskId = mint(storage).await;
    let waiting_id: TaskId = mint(storage).await;
    let completing_id: TaskId = mint(storage).await;
    let owner = pending_task(owner_id, root, "ready");
    let mut waiting = pending_task(waiting_id, root, "ready");
    waiting.owner = Some(owner_id);
    waiting.state = TaskState::Waiting {
        checkpoint: json!({"phase": "next"}),
        on: vec![owner_id],
        policy: JoinPolicy::AllSettled,
    };
    waiting.memos = Some([("kept".to_owned(), json!(true))].into());
    let mut completing = pending_task(completing_id, root, "ready");
    completing.owner = Some(owner_id);
    completing.state = TaskState::Completing {
        outcome: TaskOutcome::Failed {
            error: TaskOutcomeError {
                message: "held".into(),
                detail: Some(json!({"why": "children live"})),
            },
            result: None,
        },
    };
    commit(
        storage,
        [&owner, &waiting, &completing]
            .map(|task| StorageWrite::Task(task.clone()))
            .to_vec(),
    )
    .await;
    assert_eq!(
        storage.task(waiting_id).await.unwrap(),
        Some(waiting.clone())
    );
    assert_eq!(
        storage.task(completing_id).await.unwrap(),
        Some(completing.clone())
    );
    let scan = |status| async move {
        let query = TaskQuery {
            status: Some(status),
            ..Default::default()
        };
        storage.scan_tasks(&query, 10, None).await.unwrap().items
    };
    assert_eq!(scan(TaskStatus::Waiting).await, [waiting]);
    assert_eq!(scan(TaskStatus::Completing).await, [completing.clone()]);
    assert_eq!(
        ids(&scan(TaskStatus::Pending).await, |t| t.id.get()),
        [owner_id.get()]
    );

    let TaskState::Completing { outcome } = completing.state.clone() else {
        unreachable!()
    };
    completing.state = TaskState::Terminal { outcome };
    commit(storage, vec![StorageWrite::Task(completing.clone())]).await;
    assert!(scan(TaskStatus::Completing).await.is_empty());
    assert_eq!(scan(TaskStatus::Terminal).await, [completing]);
}

async fn request_ids_index_per_conversation_and_replace_whole_submissions(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let other: ConversationId = mint(storage).await;
    commit(
        storage,
        vec![StorageWrite::Conversation(ConversationRecord::root(other))],
    )
    .await;
    let first_id: SubmissionId = mint(storage).await;
    let second_id: SubmissionId = mint(storage).await;
    let other_id: SubmissionId = mint(storage).await;
    let first = input(first_id, root, Some("same"));
    let second = input(second_id, root, Some("other"));
    let on_other = input(other_id, other, Some("same"));
    commit(
        storage,
        [&first, &second, &on_other]
            .map(|record| StorageWrite::Submission(record.clone()))
            .to_vec(),
    )
    .await;
    // 同一个 request_id 在不同 conversation 里是两条。
    assert_eq!(
        storage.submission_by_request(root, "same").await.unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        storage.submission_by_request(other, "same").await.unwrap(),
        Some(on_other.clone())
    );
    assert_eq!(
        storage
            .submission_by_request(root, "missing")
            .await
            .unwrap(),
        None
    );

    let entry_id: EntryId = mint(storage).await;
    let placed = with_state(second.clone(), InputState::Placed { entry: entry_id });
    commit(storage, vec![StorageWrite::Submission(placed.clone())]).await;
    assert_eq!(
        storage.submission(second_id).await.unwrap(),
        Some(placed.clone())
    );
    assert_eq!(
        storage.submission_by_request(root, "other").await.unwrap(),
        Some(placed.clone())
    );

    let all = |query: SubmissionQuery| async move {
        let mut found = Vec::new();
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = storage
                .scan_submissions(&query, 1, cursor.as_ref())
                .await
                .unwrap();
            found.extend(ids(&page.items, |s| s.id.get()));
            match page.next {
                Some(next) => cursor = Some(next),
                None => return found,
            }
        }
    };
    let query = |conversation, status| SubmissionQuery {
        conversation,
        status,
        order: None,
    };
    assert_eq!(
        all(query(None, None)).await,
        [first_id.get(), second_id.get(), other_id.get()]
    );
    assert_eq!(
        all(query(Some(root), None)).await,
        [first_id.get(), second_id.get()]
    );
    // 状态变了，在状态扫描里换了地方。
    assert_eq!(
        all(query(None, Some(SubmissionStatus::Queued))).await,
        [first_id.get(), other_id.get()]
    );
    assert_eq!(
        all(query(None, Some(SubmissionStatus::Placed))).await,
        [second_id.get()]
    );
    assert_eq!(
        all(query(Some(other), Some(SubmissionStatus::Queued))).await,
        [other_id.get()]
    );
    assert!(
        all(query(Some(other), Some(SubmissionStatus::Placed)))
            .await
            .is_empty()
    );
}

async fn request_ids_are_unique_within_a_conversation(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let first: SubmissionId = mint(storage).await;
    let second: SubmissionId = mint(storage).await;
    commit(
        storage,
        vec![StorageWrite::Submission(input(first, root, Some("dup")))],
    )
    .await;
    expect_rejected(
        storage
            .commit(&[StorageWrite::Submission(input(second, root, Some("dup")))])
            .await,
    );
    // 同一批里两条也不行；被拒的整批不留痕。
    expect_rejected(
        storage
            .commit(&[
                StorageWrite::Submission(input(second, root, Some("fresh"))),
                StorageWrite::Submission(input(
                    SubmissionId::new(second.get() + 100),
                    root,
                    Some("fresh"),
                )),
            ])
            .await,
    );
    assert_eq!(storage.submission(second).await.unwrap(), None);
    assert_eq!(
        storage.submission_by_request(root, "fresh").await.unwrap(),
        None
    );
    // 自己替换自己（同一个 ID 带同一个 request_id）是正常的状态推进。
    let mut done = input(first, root, Some("dup"));
    done.kind = SubmissionKind::Input {
        state: InputState::Unanswered {
            entry: None,
            reason: "x".into(),
            detail: None,
        },
    };
    commit(storage, vec![StorageWrite::Submission(done.clone())]).await;
    assert_eq!(
        storage.submission_by_request(root, "dup").await.unwrap(),
        Some(done)
    );
}

async fn write_submissions_store_without_input_only_states(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let done_id: SubmissionId = mint(storage).await;
    let failed_id: SubmissionId = mint(storage).await;
    let entry_id: EntryId = mint(storage).await;
    let queued_done = SubmissionRecord::queued_write(done_id, root, Some("passive-done"));
    let queued_failed = SubmissionRecord::queued_write(failed_id, root, Some("passive-failed"));
    commit(
        storage,
        vec![
            StorageWrite::Submission(queued_done.clone()),
            StorageWrite::Submission(queued_failed.clone()),
        ],
    )
    .await;
    let mut done = queued_done;
    done.kind = SubmissionKind::Write {
        state: WriteState::Done { entry: entry_id },
    };
    let mut failed = queued_failed;
    failed.kind = SubmissionKind::Write {
        state: WriteState::Unanswered {
            reason: "rejected".into(),
            detail: Some(json!({"code": 7})),
        },
    };
    commit(
        storage,
        vec![
            StorageWrite::Submission(done.clone()),
            StorageWrite::Submission(failed.clone()),
        ],
    )
    .await;
    assert_eq!(
        storage.submission(done_id).await.unwrap(),
        Some(done.clone())
    );
    assert_eq!(
        storage.submission(failed_id).await.unwrap(),
        Some(failed.clone())
    );
    assert_eq!(
        storage
            .submission_by_request(root, "passive-failed")
            .await
            .unwrap(),
        Some(failed.clone())
    );
    let query = SubmissionQuery {
        status: Some(SubmissionStatus::Done),
        ..Default::default()
    };
    assert_eq!(
        storage
            .scan_submissions(&query, 10, None)
            .await
            .unwrap()
            .items,
        [done]
    );
    let query = SubmissionQuery {
        status: Some(SubmissionStatus::Unanswered),
        ..Default::default()
    };
    assert_eq!(
        storage
            .scan_submissions(&query, 10, None)
            .await
            .unwrap()
            .items,
        [failed]
    );
}

async fn documents_store_whole_values_and_read_only_live_incarnations(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let scope = DocumentScope::Conversation { conversation: root };
    let address = doc_address("komo.live", scope, None);
    let id: DocumentId = mint(storage).await;
    let created = commit(
        storage,
        vec![doc_create(
            id,
            address.clone(),
            json!({"rows": [1], "n": 1}),
        )],
    )
    .await;
    let found = storage.find_document(&address).await.unwrap().unwrap();
    assert_eq!(found.id, id);
    assert_eq!(
        (found.version, found.created_at, found.changed_at),
        (1, created, created)
    );
    assert_eq!(found.value, json!({"rows": [1], "n": 1}));
    assert_eq!(storage.document(id).await.unwrap(), Some(found));

    // change 整值替换，连同定义版本。
    let changed = StorageWrite::DocumentChange {
        id,
        version: 2,
        value: json!({"n": 2}),
    };
    let changed_at = commit(storage, vec![changed]).await;
    let doc = storage.document(id).await.unwrap().unwrap();
    assert_eq!(
        (doc.version, doc.created_at, doc.changed_at),
        (2, created, changed_at)
    );
    assert_eq!(doc.value, json!({"n": 2}), "旧值里的 rows 不能残留");
    assert!(changed_at > created);

    // 退役：读不到；同地址可以重建，新的一世换新 ID。
    let retired_at = commit(storage, vec![StorageWrite::DocumentRetire { id }]).await;
    assert!(retired_at > changed_at);
    assert_eq!(storage.document(id).await.unwrap(), None);
    assert_eq!(storage.find_document(&address).await.unwrap(), None);
    let reborn: DocumentId = mint(storage).await;
    commit(
        storage,
        vec![doc_create(reborn, address.clone(), json!({"n": 10}))],
    )
    .await;
    let found = storage.find_document(&address).await.unwrap().unwrap();
    assert_eq!((found.id, found.value), (reborn, json!({"n": 10})));
    assert_eq!(
        storage.document(id).await.unwrap(),
        None,
        "旧 ID 不会跟着地址走"
    );

    // 对退役或不存在的 document 写入被拒。
    expect_rejected(
        storage
            .commit(&[StorageWrite::DocumentChange {
                id,
                version: 1,
                value: json!(null),
            }])
            .await,
    );
    expect_rejected(
        storage
            .commit(&[StorageWrite::DocumentRetire {
                id: DocumentId::new(424_242),
            }])
            .await,
    );
}

async fn documents_enforce_one_live_incarnation_per_address(storage: &dyn Storage) {
    let address = doc_address("komo.agent", DocumentScope::Session, Some("k"));
    let first: DocumentId = mint(storage).await;
    let second: DocumentId = mint(storage).await;
    commit(storage, vec![doc_create(first, address.clone(), json!(1))]).await;
    // 第二个活的 incarnation 落在同一地址：整批被拒。
    expect_rejected(
        storage
            .commit(&[doc_create(second, address.clone(), json!(2))])
            .await,
    );
    assert_eq!(storage.document(second).await.unwrap(), None);
    // 同一批里先退役旧的再建新的，顺序无关；新的在这次提交当前。
    let seq = commit(
        storage,
        vec![
            doc_create(second, address.clone(), json!(2)),
            StorageWrite::DocumentRetire { id: first },
        ],
    )
    .await;
    let current = storage.find_document(&address).await.unwrap().unwrap();
    assert_eq!((current.id, current.created_at), (second, seq));
    assert_eq!(storage.document(first).await.unwrap(), None);

    // 同一批对一个 document 只能有一条内容命令。
    expect_rejected(
        storage
            .commit(&[
                StorageWrite::DocumentChange {
                    id: second,
                    version: 1,
                    value: json!(3),
                },
                StorageWrite::DocumentChange {
                    id: second,
                    version: 1,
                    value: json!(4),
                },
            ])
            .await,
    );
    // 内容 + 退役可以同批（先内容后退役）。
    commit(
        storage,
        vec![
            StorageWrite::DocumentRetire { id: second },
            StorageWrite::DocumentChange {
                id: second,
                version: 1,
                value: json!(5),
            },
        ],
    )
    .await;
    assert_eq!(storage.find_document(&address).await.unwrap(), None);

    // 建了立刻退役：从来没有当前值。
    let ephemeral: DocumentId = mint(storage).await;
    commit(
        storage,
        vec![
            doc_create(ephemeral, address.clone(), json!("blink")),
            StorageWrite::DocumentRetire { id: ephemeral },
        ],
    )
    .await;
    assert_eq!(storage.find_document(&address).await.unwrap(), None);
    assert_eq!(storage.document(ephemeral).await.unwrap(), None);
}

async fn document_scans_page_by_scope_and_kind(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let scope = DocumentScope::Conversation { conversation: root };
    let other_scope = DocumentScope::Task {
        task: TaskId::new(7),
    };
    let mut created = Vec::new();
    for (kind, key, scope) in [
        ("a", None, scope),
        ("b", None, scope),
        ("a", Some("x"), scope),
        ("a", None, other_scope),
        ("a", None, DocumentScope::Session),
    ] {
        let id: DocumentId = mint(storage).await;
        commit(
            storage,
            vec![doc_create(
                id,
                doc_address(kind, scope, key),
                json!(id.get()),
            )],
        )
        .await;
        created.push(id);
    }
    let retired: DocumentId = mint(storage).await;
    commit(
        storage,
        vec![doc_create(retired, doc_address("c", scope, None), json!(0))],
    )
    .await;
    commit(storage, vec![StorageWrite::DocumentRetire { id: retired }]).await;

    let scan_all = |query: DocumentQuery| async move {
        let mut found = Vec::new();
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = storage
                .scan_documents(&query, 1, cursor.as_ref())
                .await
                .unwrap();
            found.extend(ids(&page.items, |d| d.id.get()));
            match page.next {
                Some(next) => cursor = Some(next),
                None => return found,
            }
        }
    };
    assert_eq!(
        scan_all(DocumentQuery { scope, kind: None }).await,
        [created[0].get(), created[1].get(), created[2].get()]
    );
    assert_eq!(
        scan_all(DocumentQuery {
            scope,
            kind: Some("a".into())
        })
        .await,
        [created[0].get(), created[2].get()]
    );
    assert_eq!(
        scan_all(DocumentQuery {
            scope: other_scope,
            kind: None
        })
        .await,
        [created[3].get()]
    );
    assert_eq!(
        scan_all(DocumentQuery {
            scope: DocumentScope::Session,
            kind: None
        })
        .await,
        [created[4].get()]
    );
    // document 扫描只有升序；拿降序 cursor 来续扫是请求无效。
    let backwards = Cursor::new(1, ScanOrder::Descending);
    expect_request(
        storage
            .scan_documents(&DocumentQuery { scope, kind: None }, 1, Some(&backwards))
            .await,
    );
}

async fn document_identities_stay_lossless(storage: &dyn Storage) {
    // 只在标点、大小写、空串与缺省上不同的 kind / key，各自是独立的地址。
    let session = DocumentScope::Session;
    let addresses = [
        doc_address("a/b", session, None),
        doc_address("a", session, Some("b")),
        doc_address("A/B", session, None),
        doc_address("a", session, Some("")),
        doc_address("a", session, None),
        doc_address("a\u{0}b", session, None),
        doc_address("\"quoted\"", session, Some("[\"x\"]")),
    ];
    let mut created = Vec::new();
    for address in &addresses {
        let id: DocumentId = mint(storage).await;
        commit(
            storage,
            vec![doc_create(id, address.clone(), json!(id.get()))],
        )
        .await;
        created.push(id);
    }
    for (address, id) in addresses.iter().zip(&created) {
        let found = storage.find_document(address).await.unwrap().unwrap();
        assert_eq!(found.id, *id, "{address:?}");
        assert_eq!(&found.address, address);
    }
    let by_kind = DocumentQuery {
        scope: session,
        kind: Some("a".into()),
    };
    let page = storage.scan_documents(&by_kind, 10, None).await.unwrap();
    assert_eq!(
        ids(&page.items, |d| d.id.get()),
        [created[1].get(), created[3].get(), created[4].get()]
    );
    // 其余实体里同样：kind / request_id 逐字节比较。
    let root = create_root(storage).await;
    let (lower, upper): (TaskId, TaskId) = (mint(storage).await, mint(storage).await);
    let mut a = pending_task(lower, root, "p");
    a.kind = "x.y".into();
    let mut b = pending_task(upper, root, "p");
    b.kind = "X.Y".into();
    commit(storage, vec![StorageWrite::Task(a), StorageWrite::Task(b)]).await;
    let query = TaskQuery {
        kind: Some("X.Y".into()),
        ..Default::default()
    };
    assert_eq!(
        ids(
            &storage.scan_tasks(&query, 10, None).await.unwrap().items,
            |t| t.id.get()
        ),
        [upper.get()]
    );
}

async fn record_ids_share_one_namespace_and_minting_stops_at_the_limit(storage: &dyn Storage) {
    let root = create_root(storage).await;
    let explicit = EntryId::new(100);
    commit(storage, vec![put(entry(explicit, root))]).await;
    assert_eq!(
        storage.mint_id().await.unwrap(),
        101,
        "minting steps over stored IDs"
    );
    // 同一个数字不能同时是 entry 和 task。
    expect_rejected(
        storage
            .commit(&[StorageWrite::Task(pending_task(
                TaskId::new(100),
                root,
                "p",
            ))])
            .await,
    );
    // entry 不可变：同 ID 再写也不行。
    expect_rejected(storage.commit(&[put(entry(explicit, root))]).await);
    // 同一批里同一个数字当两种实体。
    expect_rejected(
        storage
            .commit(&[
                put(entry(EntryId::new(200), root)),
                StorageWrite::Submission(input(SubmissionId::new(200), root, None)),
            ])
            .await,
    );
    assert_eq!(storage.entry(EntryId::new(200)).await.unwrap(), None);

    // task 与 submission 同表重写没问题；ID 空间的上限是库的有符号 64 位整数。
    commit(
        storage,
        vec![put(EntryRecord::new(EntryId::new(MAX_ID), root, "last-id"))],
    )
    .await;
    assert_eq!(storage.mint_id().await, Err(StorageError::IdSpaceExhausted));
    assert_eq!(storage.mint_id().await, Err(StorageError::IdSpaceExhausted));
    expect_rejected(
        storage
            .commit(&[put(entry(EntryId::new(MAX_ID + 1), root))])
            .await,
    );
}

async fn invalid_reads_are_request_errors_without_effects(storage: &dyn Storage) {
    let root = create_root(storage).await;
    for limit in [0usize] {
        expect_request(
            storage
                .scan_entries(&EntryQuery::new(root), limit, None)
                .await,
        );
        expect_request(
            storage
                .scan_conversations(&ConversationQuery::default(), limit, None)
                .await,
        );
        expect_request(storage.scan_tasks(&TaskQuery::default(), limit, None).await);
        expect_request(
            storage
                .scan_submissions(&SubmissionQuery::default(), limit, None)
                .await,
        );
    }
    // 请求无效不让存储失败：之后照常能读写。
    let id: EntryId = mint(storage).await;
    commit(storage, vec![put(entry(id, root))]).await;
    assert_eq!(
        eids(
            &storage
                .scan_entries(&EntryQuery::new(root), 5, None)
                .await
                .unwrap()
                .items
        ),
        [id.get()]
    );
    assert!(StorageError::request("x").is_request());
    assert!(!StorageError::Closed.is_request());
}

async fn every_operation_is_rejected_after_close(storage: &dyn Storage) {
    let root = create_root(storage).await;
    storage.close().await.unwrap();
    assert_eq!(storage.conversation(root).await, Err(StorageError::Closed));
    assert_eq!(storage.commit(&[]).await, Err(StorageError::Closed));
    assert_eq!(storage.mint_id().await, Err(StorageError::Closed));
    assert_eq!(
        storage.scan_entries(&EntryQuery::new(root), 1, None).await,
        Err(StorageError::Closed)
    );
    assert_eq!(
        storage.task(TaskId::new(2)).await,
        Err(StorageError::Closed)
    );
    assert_eq!(
        storage
            .find_document(&doc_address("k", DocumentScope::Session, None))
            .await,
        Err(StorageError::Closed)
    );
}
