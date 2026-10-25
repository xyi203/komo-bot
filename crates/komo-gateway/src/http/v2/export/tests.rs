use super::*;
use crate::service::test_support::harness::FakeLlm;
use crate::service::test_support::{TestGateway, config_toml};
use komo_kernel::durable::{ContentBlock, Message, StopReason};
use komo_kernel::protocol::http::{ForkSessionResponse, SessionSummary, SessionTreeResponse};
use komo_kernel::session::SessionTree;
use komo_kernel::types::ids::SessionId;
use komo_runtime::durable::EntryDraft;
use serde_json::json;

async fn setup() -> (TestGateway, SessionId) {
    // 手动压缩只收 `keep_recent_tokens` 之外的旧内容：几十字的测试历史要压得动，调到最小。
    let gw = TestGateway::with(
        &config_toml("[compaction]\nkeep_recent_tokens = 1\n"),
        FakeLlm::finisher("model answer"),
    )
    .await;
    let session = new_session(&gw).await;
    (gw, session)
}

async fn new_session(gw: &TestGateway) -> SessionId {
    let (status, body) = gw.post("/v1/sessions", json!({})).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str::<SessionSummary>(&body)
        .unwrap()
        .session
}

/// 在当前分支末端追加一条消息，返回它的 pi ID。
async fn seed(gw: &TestGateway, session: &SessionId, role: &str, content: ContentBlock) -> String {
    let core = gw.state().core().unwrap();
    let conversation = core.session(session.as_str()).await.unwrap().current_branch;
    let (kind, message) = if role == "assistant" {
        (
            "komo.assistant",
            Message::assistant(vec![content], StopReason::Stop, 1),
        )
    } else {
        ("komo.user", {
            let text = match content {
                ContentBlock::Text { text, .. } => text,
                _ => unreachable!("用户消息只种文字"),
            };
            Message::user(text, 1)
        })
    };
    let id = core
        .engine
        .commit(move |tx| {
            Box::pin(async move {
                Ok(tx
                    .append_entry(
                        conversation,
                        EntryDraft::new(kind).with_model(vec![message]),
                    )
                    .await?
                    .id)
            })
        })
        .await
        .unwrap();
    id.get().to_string()
}

async fn say(gw: &TestGateway, session: &SessionId, role: &str, text: &str) -> String {
    seed(gw, session, role, ContentBlock::text(text)).await
}

async fn post_ok(gw: &TestGateway, session: &SessionId, route: &str, body: Value) -> Value {
    let (status, body) = gw
        .post(&format!("/v1/sessions/{session}/{route}"), body)
        .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// `/tree` 的快照，当前位置移到它说的 `leaf_id`。
async fn snapshot(gw: &TestGateway, session: &SessionId) -> SessionTree {
    let (status, body) = gw.get(&format!("/v1/sessions/{session}/tree")).await;
    assert_eq!(status, 200, "{body}");
    let response: SessionTreeResponse = serde_json::from_str(&body).unwrap();
    let mut tree = SessionTree::from_entries(response.entries).unwrap();
    tree.branch(response.leaf_id.as_deref()).unwrap();
    tree
}

/// 像 pi 那样读导出文件：第一行 header，其余逐行 entry，**最后一条就是当前位置**。
async fn read_export(gw: &TestGateway, session: &SessionId) -> (Value, SessionTree) {
    let (status, body) = gw.get(&format!("/v1/sessions/{session}/export")).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.ends_with('\n'));
    let mut lines = body.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let entries = lines
        .map(|line| serde_json::from_str::<SessionEntry>(line).unwrap())
        .collect();
    (header, SessionTree::from_entries(entries).unwrap())
}

/// 导出与 `/tree` 是同一棵树：entry（除了位置标记）与父子逐条一致，在各自当前位置上推导出的
/// 上下文相同。返回导出的树。
fn assert_same_tree(exported: &SessionTree, live: &SessionTree) {
    let strip = |tree: &SessionTree| {
        let mut lines: Vec<String> = tree
            .entries()
            .iter()
            .filter(|entry| entry.id != LEAF_MARKER_ID)
            .map(|entry| {
                format!(
                    "{}<-{:?} {}",
                    entry.id,
                    entry.parent_id,
                    serde_json::to_string(&entry.kind).unwrap()
                )
            })
            .collect();
        lines.sort();
        lines
    };
    assert_eq!(strip(exported), strip(live));
    assert_eq!(
        exported.project().unwrap().messages,
        live.project().unwrap().messages,
        "同一个位置的上下文"
    );
    // 当前位置：要么就是 live 的 leaf，要么是挂在它下面的标记。
    let at = exported.leaf_id().unwrap();
    match at {
        LEAF_MARKER_ID => {
            let marker = exported.get(LEAF_MARKER_ID).unwrap();
            assert_eq!(marker.parent_id.as_deref(), live.leaf_id());
        }
        other => assert_eq!(Some(other), live.leaf_id()),
    }
}

fn forks(tree: &SessionTree) -> usize {
    tree.entries()
        .iter()
        .filter(|entry| tree.children(Some(&entry.id)).len() > 1)
        .count()
}

#[tokio::test]
async fn export_round_trips_through_session_tree_with_branches_and_leaf() {
    let (gw, session) = setup().await;
    let u1 = say(&gw, &session, "user", "shared task").await;
    let a1 = say(&gw, &session, "assistant", "shared answer").await;
    let u2 = say(&gw, &session, "user", "abandoned direction").await;
    let _a2 = say(&gw, &session, "assistant", "abandoned answer").await;

    // 导航到 u2 前面再提交一条新输入：a1 下面长出第二个分支。
    post_ok(&gw, &session, "navigate", json!({"targetId": u2})).await;
    let (status, body) = gw
        .post(
            &format!("/v1/sessions/{session}/runs"),
            json!({"request_key":"export-branch","text":"replacement direction"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let accepted: komo_kernel::protocol::http::SubmitRunResponse =
        serde_json::from_str(&body).unwrap();
    gw.wait_run(&accepted.run).await;

    // 标签、名字、上下文编辑。
    post_ok(
        &gw,
        &session,
        "label",
        json!({"targetId": a1, "label": "checkpoint"}),
    )
    .await;
    post_ok(&gw, &session, "name", json!({"name": "exported"})).await;
    post_ok(
        &gw,
        &session,
        "context-edit",
        json!({"targetId": u1, "replacement": {"content": "edited task"}}),
    )
    .await;

    // 位置在树中间（a1，它有两个分支）→ 末尾要有一条标记。
    post_ok(&gw, &session, "navigate", json!({"targetId": a1})).await;
    let live = snapshot(&gw, &session).await;
    assert_eq!(live.leaf_id(), Some(a1.as_str()));
    assert_eq!(live.label(&a1), Some("checkpoint"));
    let (header, exported) = read_export(&gw, &session).await;

    assert_eq!(header["type"], "session");
    assert_eq!(header["version"], 3);
    assert_eq!(header["id"], session.as_str());
    assert!(
        header["komoBlobRoot"]
            .as_str()
            .unwrap()
            .ends_with(session.as_str())
    );
    assert_same_tree(&exported, &live);
    assert_eq!(exported.leaf_id(), Some(LEAF_MARKER_ID));
    assert_eq!(exported.label(&a1), Some("checkpoint"));
    assert_eq!(exported.name(), Some("exported"));
    assert_eq!(forks(&exported), 1);
    assert_eq!(forks(&exported), forks(&live));
    let texts = serde_json::to_string(&exported.project().unwrap().messages).unwrap();
    // 上下文编辑只作用在它落的那条路径上（replacement 分支），a1 这条路径看到原文。
    assert!(texts.contains("shared task") && texts.contains("shared answer"));
    assert!(
        exported
            .entries()
            .iter()
            .any(|entry| matches!(&entry.kind, EntryKind::ContextEdit { .. }))
    );
    assert!(!texts.contains("replacement direction"));

    // 位置在一个叶子上（没有后代）→ 挪到文件末尾，不需要标记。
    let tip = live
        .entries()
        .iter()
        .find(|entry| matches!(&entry.kind, EntryKind::Message{message} if message["content"][0]["text"] == "abandoned answer"))
        .unwrap()
        .id
        .clone();
    post_ok(&gw, &session, "navigate", json!({"targetId": tip})).await;
    let live = snapshot(&gw, &session).await;
    assert_eq!(live.leaf_id(), Some(tip.as_str()));
    let (_, exported) = read_export(&gw, &session).await;
    assert_same_tree(&exported, &live);
    assert_eq!(exported.leaf_id(), Some(tip.as_str()));
    assert!(exported.get(LEAF_MARKER_ID).is_none());
    let texts = serde_json::to_string(&exported.project().unwrap().messages).unwrap();
    assert!(texts.contains("abandoned answer") && !texts.contains("replacement direction"));
    gw.running.stop().await;
}

#[tokio::test]
async fn export_carries_branch_summaries_and_compaction() {
    let (gw, session) = setup().await;
    say(&gw, &session, "user", "task").await;
    let a = say(&gw, &session, "assistant", "answer").await;
    let left = say(&gw, &session, "user", "left branch").await;

    post_ok(
        &gw,
        &session,
        "navigate",
        json!({"targetId": a, "summarize": true}),
    )
    .await;
    let live = snapshot(&gw, &session).await;
    let (_, exported) = read_export(&gw, &session).await;
    assert_same_tree(&exported, &live);
    assert!(exported.entries().iter().any(
        |entry| matches!(&entry.kind, EntryKind::BranchSummary { from_id, .. } if *from_id == left)
    ));

    post_ok(
        &gw,
        &session,
        "compact",
        json!({"instructions": "keep decisions"}),
    )
    .await;
    let live = snapshot(&gw, &session).await;
    let (_, exported) = read_export(&gw, &session).await;
    assert_same_tree(&exported, &live);
    // 压缩后的当前位置就是新的末条：文件末尾，没有标记。
    assert_eq!(exported.leaf_id(), live.leaf_id());
    let messages = exported.project().unwrap().messages;
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["role"] == "compactionSummary")
            .count(),
        1
    );
    assert!(
        exported
            .entries()
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
    );
    gw.running.stop().await;
}

#[tokio::test]
async fn a_forked_session_exports_on_its_own_and_blob_references_stay_references() {
    let (gw, session) = setup().await;
    say(&gw, &session, "user", "look at this").await;
    let image = seed(
        &gw,
        &session,
        "assistant",
        serde_json::from_value(json!({
            "type": "image", "mime": "image/png", "path": "images/a.png", "hash": "ab"
        }))
        .unwrap(),
    )
    .await;
    let reply = say(
        &gw,
        &session,
        "assistant",
        "完整输出：artifact://run-1/call-7/attempt-1/result",
    )
    .await;

    let fork = post_ok(&gw, &session, "fork", json!({"targetId": image})).await;
    let fork: ForkSessionResponse = serde_json::from_value(fork).unwrap();
    let (header, exported) = read_export(&gw, &fork.session).await;
    assert_eq!(header["id"], fork.session.as_str());
    // 复制到 `image` 为止：回答（reply）不在副本里，位置在末条。
    assert!(exported.get(&reply).is_none());
    let live = snapshot(&gw, &fork.session).await;
    assert_same_tree(&exported, &live);
    assert_eq!(exported.entries().len(), 2);

    // 图片块仍是 `{mime, path, hash}`，没有字节；`artifact://` 文字原样。
    let (_, source) = read_export(&gw, &session).await;
    let blocks = serde_json::to_string(source.entries()).unwrap();
    assert!(blocks.contains(r#""path":"images/a.png""#), "{blocks}");
    assert!(!blocks.contains(r#""data""#), "{blocks}");
    assert!(blocks.contains("artifact://run-1/call-7/attempt-1/result"));
    gw.running.stop().await;
}

#[tokio::test]
async fn exporting_a_missing_session_is_a_404() {
    let (gw, _) = setup().await;
    let (status, _) = gw.get("/v1/sessions/no-such-session/export").await;
    assert_eq!(status, 404);
    gw.running.stop().await;
}

// ---------------------------------------------------------------- 位置安排（纯函数）

fn message(id: &str, parent: Option<&str>) -> SessionEntry {
    SessionEntry {
        id: id.to_owned(),
        parent_id: parent.map(str::to_owned),
        timestamp: "2026-10-25T00:00:00Z".to_owned(),
        kind: EntryKind::Message {
            message: json!({"role": "user", "content": id, "timestamp": 1}),
        },
    }
}

fn ids(entries: &[SessionEntry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.id.as_str()).collect()
}

#[test]
fn the_leaf_goes_last_when_it_can_and_a_marker_otherwise() {
    let tree = |leaf: Option<&str>| PiTree {
        entries: vec![
            message("1", None),
            message("2", Some("1")),
            message("3", Some("2")),
            message("4", Some("1")),
        ],
        leaf_id: leaf.map(str::to_owned),
    };
    // 已经是末条。
    assert_eq!(ids(&arranged(tree(Some("4")))), ["1", "2", "3", "4"]);
    // 没有后代：挪到最后，其余顺序不变。
    assert_eq!(ids(&arranged(tree(Some("3")))), ["1", "2", "4", "3"]);
    // 在中间（有后代）：追加标记，挂在 leaf 下。
    let middle = arranged(tree(Some("2")));
    assert_eq!(ids(&middle), ["1", "2", "3", "4", LEAF_MARKER_ID]);
    assert_eq!(middle[4].parent_id.as_deref(), Some("2"));
    // 第一条之前：标记是新根。
    let before = arranged(tree(None));
    assert_eq!(before[4].id, LEAF_MARKER_ID);
    assert_eq!(before[4].parent_id, None);
    let restored = SessionTree::from_entries(before).unwrap();
    assert!(restored.project().unwrap().messages.is_empty());
}

#[test]
fn a_label_leaf_is_never_moved() {
    let mut entries = vec![message("1", None), message("2", Some("1"))];
    entries.insert(
        1,
        SessionEntry {
            id: "9".to_owned(),
            parent_id: Some("1".to_owned()),
            timestamp: "2026-10-25T00:00:00Z".to_owned(),
            kind: EntryKind::Label {
                target_id: "1".to_owned(),
                label: Some("x".to_owned()),
            },
        },
    );
    entries[2].parent_id = Some("9".to_owned());
    let arranged = arranged(PiTree {
        entries,
        leaf_id: Some("9".to_owned()),
    });
    assert_eq!(ids(&arranged), ["1", "9", "2", LEAF_MARKER_ID]);
}

#[test]
fn an_empty_session_exports_just_the_header() {
    let session = SessionRecord::new("s1", komo_kernel::durable::ids::ConversationId::new(1));
    let body = jsonl(
        &session,
        "/data/sessions/s1",
        PiTree {
            entries: vec![],
            leaf_id: None,
        },
    )
    .unwrap();
    assert_eq!(body.lines().count(), 1);
    let header: SessionHeader = serde_json::from_str(body.lines().next().unwrap()).unwrap();
    assert_eq!((header.version, header.id.as_str()), (3, "s1"));
}
