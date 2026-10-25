//! gateway 要的存储接缝（D5 W0a）：Session 行、conversation 的 session 归属、整组 purge。
//! 都是一次提交里的事务视图操作，所以跑在引擎的 `commit` 上。

use komo_kernel::durable::storage::{ConversationQuery, DocumentQuery};
use komo_kernel::durable::{
    ConversationId, DocumentAddress, DocumentScope, EntryId, SessionKind, SessionRecord, Storage,
};
use serde_json::json;

use super::super::tx::{ConversationOwnership, EntryDraft};
use super::support::*;

#[tokio::test(start_paused = true)]
async fn a_session_is_one_commit_of_a_row_a_root_conversation_and_its_forks_inherit_the_session() {
    let env = Env::new();
    let engine = env.open().await;
    let (session, root, branch, entry) = engine
        .commit(|tx| {
            Box::pin(async move {
                let root = tx
                    .create_conversation_in("sess-1", ConversationOwnership::Ownerless)
                    .await?;
                let entry = tx
                    .append_entry(root.id, EntryDraft::new("komo.user"))
                    .await?;
                let mut record = SessionRecord::new("sess-1", root.id);
                record.kind = SessionKind::Cron;
                record.title = Some("t".into());
                record.created_at = tx.now();
                tx.put_session(record);
                // 本次提交里暂存的 Session 行读得到（Session 行不算表写，不受表读规则限制）。
                let staged = tx.session("sess-1").await?.expect("staged");
                assert_eq!(staged.root_conversation, root.id);
                let branch = tx
                    .fork_conversation(root.id, entry.id, ConversationOwnership::Ownerless)
                    .await?;
                // fork 继承暂存的父 conversation 的 session。
                assert_eq!(branch.session.as_deref(), Some("sess-1"));
                Ok((staged, root.id, branch.id, entry.id))
            })
        })
        .await
        .unwrap();

    let stored = env.storage.session("sess-1").await.unwrap().unwrap();
    assert_eq!(stored, session);
    assert_eq!(stored.current_branch, root, "新建的 Session 当前分支是根");
    assert_eq!(stored.kind, SessionKind::Cron);
    for id in [root, branch] {
        let record = env.storage.conversation(id).await.unwrap().unwrap();
        assert_eq!(record.session.as_deref(), Some("sess-1"));
    }
    // 之后另一次提交里 fork 已提交的父，同样继承；不带 session 的 conversation 没有。
    let later = engine
        .commit(move |tx| {
            Box::pin(async move {
                let plain = tx
                    .create_conversation(ConversationOwnership::Ownerless)
                    .await?;
                assert_eq!(plain.session, None);
                tx.fork_conversation(root, entry, ConversationOwnership::Ownerless)
                    .await
            })
        })
        .await
        .unwrap();
    assert_eq!(later.session.as_deref(), Some("sess-1"));
    let page = env
        .storage
        .scan_conversations(
            &ConversationQuery {
                session: Some("sess-1".into()),
                ..Default::default()
            },
            10,
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 3);
    // 同一提交里再改 Session 行：后写的赢，且已提交的读得到旧值。
    engine
        .commit(move |tx| {
            Box::pin(async move {
                let mut record = tx.session("sess-1").await?.expect("committed");
                assert_eq!(record.title.as_deref(), Some("t"));
                record.title = Some("renamed".into());
                tx.put_session(record.clone());
                record.leaf = Some(EntryId::new(entry.get()));
                tx.put_session(record);
                Ok(())
            })
        })
        .await
        .unwrap();
    let stored = env.storage.session("sess-1").await.unwrap().unwrap();
    assert_eq!(stored.title.as_deref(), Some("renamed"));
    assert_eq!(stored.leaf, Some(entry));
    engine.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn purge_conversations_deletes_every_row_of_the_listed_conversations_only() {
    let env = Env::new();
    let engine = env.open().await;
    let address = |conversation: ConversationId| DocumentAddress {
        kind: "komo.agent".into(),
        scope: DocumentScope::Conversation { conversation },
        key: None,
    };
    let (gone, kept, entry) = engine
        .commit(|tx| {
            Box::pin(async move {
                let gone = tx
                    .create_conversation_in("doomed", ConversationOwnership::Ownerless)
                    .await?
                    .id;
                let kept = tx
                    .create_conversation_in("kept", ConversationOwnership::Ownerless)
                    .await?
                    .id;
                let mut entries = Vec::new();
                for conversation in [gone, kept] {
                    entries.push(
                        tx.append_entry(conversation, EntryDraft::new("komo.user"))
                            .await?
                            .id,
                    );
                    tx.put_document(address(conversation), 1, json!({"c": conversation}))
                        .await?;
                }
                Ok((gone, kept, entries))
            })
        })
        .await
        .unwrap();

    engine
        .commit(move |tx| {
            Box::pin(async move {
                tx.purge_conversations([gone, gone]);
                Ok(())
            })
        })
        .await
        .unwrap();

    let storage = &env.storage;
    assert_eq!(storage.conversation(gone).await.unwrap(), None);
    assert_eq!(storage.entry(entry[0]).await.unwrap(), None);
    assert_eq!(storage.find_document(&address(gone)).await.unwrap(), None);
    let left = storage
        .scan_documents(
            &DocumentQuery {
                scope: DocumentScope::Conversation { conversation: gone },
                kind: None,
            },
            10,
            None,
        )
        .await
        .unwrap();
    assert!(left.items.is_empty());
    assert!(storage.conversation(kept).await.unwrap().is_some());
    assert!(storage.entry(entry[1]).await.unwrap().is_some());
    assert!(
        storage
            .find_document(&address(kept))
            .await
            .unwrap()
            .is_some()
    );
    engine.close().await.unwrap();
}
