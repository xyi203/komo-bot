use super::*;
use crate::durable::tests::support::{Env, ScanPause};
use crate::durable::{ConversationOwnership, DraftHead, Engine, EntryDraft};
use komo_kernel::durable::compaction::{cache_cold, online_state};
use komo_kernel::durable::{
    ContentBlock, ContextEdit, Message, ROOT_CONVERSATION_ID, Sections, StopReason, derive_context,
};
use serde_json::json;

const ROOT: ConversationId = ROOT_CONVERSATION_ID;

async fn append(engine: &Engine, conversation: ConversationId, draft: EntryDraft) -> EntryRecord {
    engine
        .commit(move |tx| Box::pin(async move { tx.append_entry(conversation, draft).await }))
        .await
        .unwrap()
}

fn user(text: &str) -> EntryDraft {
    let mut draft = EntryDraft::new("message");
    draft.model = Some(vec![Message::user(text, 1)]);
    draft
}

fn cache_for(engine: &Engine) -> Arc<ContextCache> {
    let cache = Arc::new(ContextCache::default());
    let observed = cache.clone();
    engine
        .line()
        .subscribe_commits(move |publication| observed.observe(publication));
    cache
}

#[tokio::test]
async fn unchanged_bounds_share_snapshots_and_new_entries_only_scan_the_suffix() {
    let env = Env::new();
    let engine = env.open().await;
    let cache = cache_for(&engine);
    assert!(
        cache
            .view(engine.line(), ROOT, None)
            .await
            .unwrap()
            .messages
            .is_empty()
    );
    let first = append(&engine, ROOT, user("first")).await;
    let one = cache.snapshot(engine.line(), ROOT).await.unwrap();
    env.storage.entry_queries.lock().unwrap().clear();
    let same = cache.snapshot(engine.line(), ROOT).await.unwrap();
    assert!(Arc::ptr_eq(&one.entries, &same.entries));
    assert!(Arc::ptr_eq(&one.view, &same.view));
    assert!(
        env.storage
            .entry_queries
            .lock()
            .unwrap()
            .iter()
            .all(|(_, limit)| *limit == 1)
    );

    let second = append(&engine, ROOT, user("second")).await;
    env.storage.entry_queries.lock().unwrap().clear();
    let two = cache.snapshot(engine.line(), ROOT).await.unwrap();
    assert_eq!(&*two.entries, &[first.clone(), second.clone()]);
    assert_eq!(*two.view, derive_context(&[first.clone(), second], None));
    assert!(
        env.storage
            .entry_queries
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, limit)| *limit > 1)
            .all(|(query, _)| query.min == Some(after(first.id)))
    );
    // 历史请求只截缓存前缀；不能让在线账本随 cutoff 回退。
    let past = cache
        .view(engine.line(), ROOT, Some(first.id))
        .await
        .unwrap();
    assert_eq!(*past, derive_context(&[first], None));
    assert_eq!(
        cache.snapshot(engine.line(), ROOT).await.unwrap().view,
        two.view
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn compaction_keeps_prefix_debt_but_reset_starts_with_a_warm_empty_cache() {
    let env = Env::new();
    let engine = env.open().await;
    let cache = cache_for(&engine);
    let mut all = Vec::new();
    let mut draft = user("q");
    draft.model = Some(vec![Message::assistant(
        vec![ContentBlock::text("a")],
        StopReason::Stop,
        1,
    )]);
    all.push(append(&engine, ROOT, draft).await);
    cache.snapshot(engine.line(), ROOT).await.unwrap();
    let kept = append(&engine, ROOT, user("kept")).await;
    all.push(kept.clone());
    let mut compact = user("summary");
    compact.kind = "komo.compaction".into();
    compact.head = Some(DraftHead::At(kept.id));
    compact.data = Some(json!({"debt": {"debt_tokens": 500.0, "repayment_tokens": 20}}));
    all.push(append(&engine, ROOT, compact).await);
    let context = cache.snapshot(engine.line(), ROOT).await.unwrap();
    assert_eq!(context.online_state(None), online_state(&all, None));
    assert_eq!(context.online_state(None).request_count, 1);
    assert_eq!(context.online_state(None).cache_debt_tokens, 500.0);
    assert!(context.entries.iter().all(|entry| entry.id >= kept.id));

    let mut reset = EntryDraft::new("komo.reset");
    reset.head = Some(DraftHead::SelfHead);
    all.push(append(&engine, ROOT, reset).await);
    let reset = cache.snapshot(engine.line(), ROOT).await.unwrap();
    assert!(!cache_cold(&reset.entries, 1_000_000, 100));
    assert!(
        cache_cold(&all, 1_000_000, 100),
        "old whole-history behavior"
    );
    assert_eq!(reset.online_state(None), OnlineState::default());
    assert_eq!(*reset.view, derive_context(&all, None));
    engine.close().await.unwrap();
}

/// 固定种子的性质测试：任意增量切点、后退 cutoff、head、edits 与系统基线。
#[tokio::test]
async fn incremental_context_matches_full_derivation_at_random_cutoffs_and_forks() {
    for seed in 1..=12u64 {
        let env = Env::new();
        let engine = env.open().await;
        let cache = cache_for(&engine);
        let mut random = seed;
        let mut all = Vec::<EntryRecord>::new();
        let mut conversation = ROOT;
        for n in 0..80 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut draft = user(&format!("{seed}/{n}"));
            let pick = (random >> 32) as usize;
            match pick % 11 {
                0 => {
                    draft.kind = "komo.system".into();
                    draft.model = Some(vec![Message::system(
                        Sections::default().set("base", "x"),
                        n,
                    )]);
                }
                1 | 2 => {
                    draft.model = Some(vec![Message::assistant(
                        vec![ContentBlock::tool_call("call", "read", json!({}))],
                        StopReason::ToolUse,
                        n,
                    )]);
                }
                3 => draft.model = Some(vec![Message::tool_result("call", "read", "out", n)]),
                4 if !all.is_empty() => {
                    let target = all[pick % all.len()].id;
                    draft.edits = vec![ContextEdit::replace(
                        target,
                        vec![Message::user("edited", n)],
                    )];
                }
                5 if !all.is_empty() => {
                    draft.edits = vec![ContextEdit::omit(all[pick % all.len()].id)]
                }
                6 if !all.is_empty() => draft.head = Some(DraftHead::At(all[pick % all.len()].id)),
                7 => draft.head = Some(DraftHead::SelfHead),
                8 => {
                    draft.model = Some(vec![Message::assistant(
                        vec![ContentBlock::text("failed")],
                        StopReason::Error,
                        n,
                    )])
                }
                _ => {}
            }
            all.push(append(&engine, conversation, draft).await);
            let snapshot = cache.snapshot(engine.line(), conversation).await.unwrap();
            assert_eq!(
                *snapshot.view,
                derive_context(&all, None),
                "seed={seed}, n={n}"
            );
            assert_eq!(snapshot.online_state(None), online_state(&all, None));
            if n % 5 == 0 {
                let cutoff = all[pick % all.len()].id;
                let view = cache
                    .view(engine.line(), conversation, Some(cutoff))
                    .await
                    .unwrap();
                assert_eq!(
                    *view,
                    derive_context(&all, Some(cutoff)),
                    "historical seed={seed}, n={n}"
                );
            }
            if n == 39 {
                let cut = all[pick % all.len()].id;
                let parent = conversation;
                conversation = engine
                    .commit(move |tx| {
                        Box::pin(async move {
                            Ok(tx
                                .fork_conversation(parent, cut, ConversationOwnership::Ownerless)
                                .await?
                                .id)
                        })
                    })
                    .await
                    .unwrap();
                all.retain(|entry| entry.id <= cut);
                // 父 conversation 后续追加不应进入 fork。
                append(&engine, parent, user("parent only")).await;
            }
        }
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn a_paused_ten_thousand_entry_scan_does_not_hold_the_commit_line() {
    let env = Env::new();
    let engine = Arc::new(env.open().await);
    let cache = cache_for(&engine);
    engine
        .commit(|tx| {
            Box::pin(async move {
                for _ in 0..10_000 {
                    tx.append_entry(ROOT, user("history")).await?;
                }
                Ok(())
            })
        })
        .await
        .unwrap();
    let pause = ScanPause::new();
    *env.storage.pause_scan.lock().unwrap() = Some(pause.clone());
    let reader = tokio::spawn({
        let engine = engine.clone();
        let cache = cache.clone();
        async move { cache.snapshot(engine.line(), ROOT).await }
    });
    pause.started.notified().await;
    let other = tokio::time::timeout(
        Duration::from_secs(1),
        engine.commit(|tx| {
            Box::pin(async move {
                let other = tx
                    .create_conversation(ConversationOwnership::Ownerless)
                    .await?;
                tx.append_entry(other.id, user("unblocked")).await?;
                tx.append_entry(ROOT, user("beyond captured tail")).await?;
                Ok(other.id)
            })
        }),
    )
    .await
    .expect("unrelated commits must not wait for history scans")
    .unwrap();
    assert_eq!(engine.context(other, None).await.unwrap().messages.len(), 1);
    pause.release.notify_one();
    let snapshot = reader.await.unwrap().unwrap();
    assert_eq!(
        snapshot.entries.len(),
        10_000,
        "captured tail excludes concurrent append"
    );
    assert_eq!(
        cache
            .snapshot(engine.line(), ROOT)
            .await
            .unwrap()
            .entries
            .len(),
        10_001
    );
    engine.close().await.unwrap();
}

#[tokio::test]
async fn purge_invalidates_cached_and_in_flight_contexts() {
    let env = Env::new();
    let engine = Arc::new(env.open().await);
    let cache = cache_for(&engine);
    append(&engine, ROOT, user("cached")).await;
    cache.view(engine.line(), ROOT, None).await.unwrap();
    append(&engine, ROOT, user("new")).await;
    let pause = ScanPause::new();
    *env.storage.pause_scan.lock().unwrap() = Some(pause.clone());
    let reader = tokio::spawn({
        let engine = engine.clone();
        let cache = cache.clone();
        async move { cache.view(engine.line(), ROOT, None).await }
    });
    pause.started.notified().await;
    engine
        .commit(|tx| {
            Box::pin(async move {
                tx.purge_conversations(vec![ROOT]);
                Ok(())
            })
        })
        .await
        .unwrap();
    pause.release.notify_one();
    assert!(reader.await.unwrap().is_err());
    assert!(cache.slots.lock().unwrap().is_empty());
    assert!(cache.view(engine.line(), ROOT, None).await.is_err());
    engine.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn idle_contexts_expire_after_ten_minutes_while_busy_and_in_flight_ones_stay() {
    let cache = ContextCache::default();
    let idle = ConversationId::new(10);
    let busy = ConversationId::new(11);
    let reading = ConversationId::new(12);
    drop(cache.slot(idle));
    drop(cache.slot(busy));
    let held = cache.slot(reading);
    tokio::time::advance(RETENTION + Duration::from_secs(1)).await;
    cache.expire(|id| id == busy);
    let slots = cache.slots.lock().unwrap();
    assert!(!slots.contains_key(&idle));
    assert!(slots.contains_key(&busy));
    assert!(slots.contains_key(&reading));
    drop(slots);
    drop(held);
    cache.expire(|_| false);
    assert!(cache.slots.lock().unwrap().is_empty());
}

#[tokio::test]
async fn off_line_scan_failures_still_fail_the_line() {
    let env = Env::new();
    let engine = env.open().await;
    append(&engine, ROOT, user("q")).await;
    env.storage.fail_scans.store(true, Ordering::SeqCst);
    assert!(engine.context(ROOT, None).await.is_err());
    assert!(engine.failure().is_some());
    engine.close().await.unwrap();
}
