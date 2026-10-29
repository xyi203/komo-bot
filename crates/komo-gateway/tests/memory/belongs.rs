//! 记忆归属与结案归档经 HTTP 往返（`docs/goals.md` §6，`docs/memory.md` §9.6「结案归档」）：
//! 真 Gateway、真 aux.db。结案响应带归档条数与仍在生效的约定；`--for` 要事项存在；
//! `memory = private / off` 的会话不能挂归属。

use komo_kernel::test_support::ScriptedLlm;
use komo_kernel::types::ids::{RunId, SessionId};
use serde_json::{Value, json};

use super::Scripted;
use crate::harness::*;

fn config() -> String {
    memory_config("keyword", false).replace(
        "[agents.assistant]",
        "[agents.assistant]\n\n[agents.helper]\nmemory = \"private\"\n\n\
         [agents.quiet]\nmemory = \"off\"",
    )
}

async fn start() -> TestGateway {
    super::gateway(&config())
        .scripted(ScriptedLlm::new(Vec::new()))
        .start()
        .await
}

async fn post(gateway: &TestGateway, path: &str, body: Value) -> Value {
    let (status, body) = gateway.post_json(path, body).await;
    assert_eq!(status, 200, "{path}：{body}");
    body
}

async fn matter(gateway: &TestGateway, kind: &str, title: &str) -> String {
    post(
        gateway,
        "/v1/matters",
        json!({"kind": kind, "title": title}),
    )
    .await["matter"]["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn memory(gateway: &TestGateway, content: &str, belongs: Option<&str>) -> Value {
    let mut body = json!({"content": content, "kind": "fact", "scope": {"kind": "personal"}});
    if let Some(belongs) = belongs {
        body["belongs_to"] = json!(belongs);
    }
    post(gateway, "/v1/memories", body).await["memory"].clone()
}

async fn convention(gateway: &TestGateway, content: &str, belongs: &str) -> Value {
    post(
        gateway,
        "/v1/memories",
        json!({"content": content, "kind": "preference", "scope": {"kind": "personal"},
               "application": "convention", "applies_when": "发布观察时",
               "belongs_to": belongs}),
    )
    .await["memory"]
        .clone()
}

async fn listed(gateway: &TestGateway, query: &str) -> Vec<String> {
    let (status, body) = gateway.get_json(&format!("/v1/memories{query}")).await;
    assert_eq!(status, 200, "{body}");
    body["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|memory| memory["id"].as_str().unwrap().to_string())
        .collect()
}

/// 结案：响应带归档条数与名下仍生效的约定；清单默认藏归档的、`include_archived` 与
/// `state=archived` 查得到；再结一次幂等；重开恢复。
#[tokio::test]
async fn closing_a_goal_reports_the_archive_and_the_conventions_and_reopening_restores() {
    let gateway = start().await;
    let goal = matter(&gateway, "goal", "这周把 owl 的发布观察做完").await;
    let clue = memory(&gateway, "owl 的 AB 组在 canary 集群", Some(&goal)).await;
    let free = memory(&gateway, "owl 的仓库在 gitlab 上", None).await;
    let rule = convention(&gateway, "发布观察先看 5xx", &goal).await;
    let clue_id = clue["id"].as_str().unwrap().to_string();
    assert_eq!(clue["belongs_to"], json!(goal));
    assert!(free.get("belongs_to").is_none(), "{free}");

    let done = post(&gateway, &format!("/v1/matters/{goal}/done"), json!({})).await;
    assert_eq!(done["matter"]["state"], "done");
    let closure = &done["closure"];
    assert_eq!(closure["archived"], 1, "{done}");
    assert_eq!(
        closure["conventions"].as_array().unwrap().len(),
        1,
        "{done}"
    );
    let listed_rule = &closure["conventions"][0];
    assert_eq!(listed_rule["id"], rule["id"]);
    assert_eq!(listed_rule["revision"], 1);
    assert_eq!(listed_rule["content"], "发布观察先看 5xx");
    assert_eq!(listed_rule["applies_when"], "发布观察时");

    let visible = listed(&gateway, "").await;
    assert!(!visible.contains(&clue_id), "默认不列归档的：{visible:?}");
    assert!(visible.contains(&free["id"].as_str().unwrap().to_string()));
    assert!(visible.contains(&rule["id"].as_str().unwrap().to_string()));
    assert!(
        listed(&gateway, "?include_archived=true")
            .await
            .contains(&clue_id)
    );
    assert_eq!(listed(&gateway, "?state=archived").await, [clue_id.clone()]);
    let (_, shown) = gateway.get_json(&format!("/v1/memories/{clue_id}")).await;
    assert_eq!(shown["memory"]["state"], "archived");
    assert_eq!(shown["memory"]["revision"], 2);

    let again = post(&gateway, &format!("/v1/matters/{goal}/done"), json!({})).await;
    assert_eq!(again["closure"]["archived"], 0, "幂等：{again}");
    assert_eq!(again["closure"]["conventions"].as_array().unwrap().len(), 1);

    let reopened = post(&gateway, &format!("/v1/matters/{goal}/reopen"), json!({})).await;
    assert_eq!(reopened["closure"]["restored"], 1, "{reopened}");
    let (_, shown) = gateway.get_json(&format!("/v1/memories/{clue_id}")).await;
    assert_eq!(shown["memory"]["state"], "active");
    assert!(listed(&gateway, "").await.contains(&clue_id));

    // 搁置、推进这类不是结案的动作不带 closure。
    let parked = post(&gateway, &format!("/v1/matters/{goal}/park"), json!({})).await;
    assert!(parked.get("closure").is_none(), "{parked}");
}

/// 领域 retire 同样归档；drop 的目标也一样。
#[tokio::test]
async fn retiring_an_area_and_dropping_a_goal_archive_their_clues_too() {
    let gateway = start().await;
    let area = matter(&gateway, "area", "告警值班轮换").await;
    let goal = matter(&gateway, "goal", "迁移告警规则").await;
    let area_clue = memory(&gateway, "告警值班表在飞书文档里", Some(&area)).await;
    let goal_clue = memory(&gateway, "告警规则迁移到新平台", Some(&goal)).await;

    let retired = post(&gateway, &format!("/v1/matters/{area}/retire"), json!({})).await;
    assert_eq!(retired["closure"]["archived"], 1, "{retired}");
    let dropped = post(&gateway, &format!("/v1/matters/{goal}/drop"), json!({})).await;
    assert_eq!(dropped["closure"]["archived"], 1, "{dropped}");
    let hidden = listed(&gateway, "").await;
    for clue in [&area_clue, &goal_clue] {
        assert!(!hidden.contains(&clue["id"].as_str().unwrap().to_string()));
    }
}

/// `add --for` 与 `classify --for`：事项要存在且未结案；classify 升版、`none` 清空、
/// 两者都给或都不给被拒。
#[tokio::test]
async fn add_for_and_classify_for_validate_the_matter_and_bump_the_revision() {
    let gateway = start().await;
    let goal = matter(&gateway, "goal", "发布观察").await;
    let closed = matter(&gateway, "goal", "已办完的事").await;
    post(&gateway, &format!("/v1/matters/{closed}/done"), json!({})).await;

    for belongs in ["g-nope", closed.as_str()] {
        let (status, body) = gateway
            .post_json(
                "/v1/memories",
                json!({"content": "一条线索", "kind": "fact",
                       "scope": {"kind": "personal"}, "belongs_to": belongs}),
            )
            .await;
        assert_eq!(status, 400, "{belongs}：{body}");
    }

    let item = memory(&gateway, "没有归属的线索", None).await;
    let id = item["id"].as_str().unwrap();
    let path = format!("/v1/memories/{id}/classify");
    let moved = post(
        &gateway,
        &path,
        json!({"expected_revision": 1, "belongs_to": goal}),
    )
    .await;
    assert_eq!(moved["memory"]["revision"], 2);
    assert_eq!(moved["memory"]["belongs_to"], json!(goal));

    let cleared = post(
        &gateway,
        &path,
        json!({"expected_revision": 2, "clear_belongs_to": true}),
    )
    .await;
    assert_eq!(cleared["memory"]["revision"], 3);
    assert!(cleared["memory"].get("belongs_to").is_none(), "{cleared}");

    for (body, why) in [
        (
            json!({"expected_revision": 3, "belongs_to": "g-nope"}),
            "事项不存在",
        ),
        (
            json!({"expected_revision": 3, "belongs_to": closed}),
            "事项已结案",
        ),
        (
            json!({"expected_revision": 3, "belongs_to": goal, "clear_belongs_to": true}),
            "两个都给",
        ),
        (json!({"expected_revision": 3}), "什么都没给"),
    ] {
        let (status, response) = gateway.post_json(&path, body).await;
        assert_eq!(status, 400, "{why}：{response}");
    }
}

/// 给别人用的 Agent（`memory = private / off`）的会话不能把记忆挂到操作者的事项上。
#[tokio::test]
async fn a_private_or_off_session_cannot_attach_memory_to_a_matter() {
    let gateway = start().await;
    let goal = matter(&gateway, "goal", "发布观察").await;
    let item = memory(&gateway, "没有归属的线索", None).await;
    let core = komo_gateway::service::test_support::driver_stream::core(&gateway);
    for agent in ["helper", "quiet"] {
        let session = SessionId::from_raw(core.main_session(agent).await.unwrap().0.id);
        let (status, body) = gateway
            .post_json(
                "/v1/memories",
                json!({"content": "替别人记的线索", "kind": "fact",
                       "scope": {"kind": "personal"}, "belongs_to": goal,
                       "session": session, "run": RunId::from_raw("any")}),
            )
            .await;
        assert_eq!(status, 403, "{agent}：{body}");
        let (status, body) = gateway
            .post_json(
                &format!("/v1/memories/{}/classify", item["id"].as_str().unwrap()),
                json!({"expected_revision": 1, "belongs_to": goal, "session": session}),
            )
            .await;
        assert_eq!(status, 403, "{agent}：{body}");
    }
}
