//! 目标与领域经 HTTP 往返（`docs/goals.md` §4、§9 P1a）：全部动词走真 Gateway、真 aux.db；
//! Run 里改承诺要有用户原话，推进不要；`memory = private / off` 的会话读写都被拒。
//!
//! 放在记忆这个测试二进制里：用的是同一套带记忆、带多个 Agent 的配置，少链接一个二进制。

use komo_kernel::test_support::ScriptedLlm;
use komo_kernel::types::ids::{RunId, SessionId};
use serde_json::{Value, json};

use super::{Scripted, a_run, text_round, wait_for_terminal};
use crate::harness::*;

/// 默认的 `assistant`（共享记忆）、`helper`（私有）、`quiet`（关了记忆）。
fn config() -> String {
    memory_config("keyword", false).replace(
        "[agents.assistant]",
        "[agents.assistant]\n\n[agents.helper]\nmemory = \"private\"\n\n\
         [agents.quiet]\nmemory = \"off\"",
    )
}

async fn start(replies: usize) -> TestGateway {
    let scripts = (0..replies)
        .map(|_| vec![text_round(1, "好的。")])
        .collect();
    super::gateway(&config())
        .scripted(ScriptedLlm::new(scripts))
        .start()
        .await
}

/// 一个跑完了的 Run，用户在里面说了一句话。
async fn user_run(gateway: &TestGateway, text: &str) -> (SessionId, RunId) {
    let (session, run) = a_run(gateway, "rk-matter", text).await;
    wait_for_terminal(gateway, &run).await;
    (session, run)
}

async fn post(gateway: &TestGateway, path: &str, body: Value) -> Value {
    let (status, body) = gateway.post_json(path, body).await;
    assert_eq!(status, 200, "{path}：{body}");
    body
}

fn in_run(mut body: Value, session: &SessionId, run: &RunId) -> Value {
    body["session"] = json!(session);
    body["run"] = json!(run);
    body
}

/// §9 P1a：add → next → wait → resume → done → reopen 全走 HTTP；状态不允许的报错并写出
/// 当前状态；清单默认藏结案的；每个响应带 Gateway 的今天。
#[tokio::test]
async fn a_goal_walks_its_whole_life_over_http() {
    let gateway = start(1).await;
    let (session, run) = user_run(&gateway, "开始做 owl 的发布观察，这周做完").await;

    let added = post(
        &gateway,
        "/v1/matters",
        in_run(
            json!({"kind": "goal", "title": "这周把 owl 的发布观察做完",
                   "next": "看 AB 组的 5xx", "due": "2020-01-01"}),
            &session,
            &run,
        ),
    )
    .await;
    let goal = &added["matter"];
    let id = goal["id"].as_str().unwrap().to_string();
    assert_eq!(goal["state"], "active");
    assert_eq!(goal["revision"], 1);
    assert_eq!(goal["due"], "2020-01-01");
    assert_eq!(
        goal["evidence"].as_array().map(Vec::len),
        Some(1),
        "Run 里新建引这个 Run 里用户的那句话：{goal}"
    );
    assert!(added["today"].as_str().is_some(), "{added}");

    let act = |verb: &'static str, body: Value| {
        let gateway = &gateway;
        let path = format!("/v1/matters/{id}/{verb}");
        async move { post(gateway, &path, body).await["matter"].clone() }
    };
    // 推进：Run 里不要求原话（这里的 Run 有原话，也不追加）。
    let next = act(
        "next",
        in_run(json!({"next": "提发布工单"}), &session, &run),
    )
    .await;
    assert_eq!(next["revision"], 2);
    assert_eq!(next["next"], "提发布工单");
    let waiting = act(
        "wait",
        in_run(json!({"waiting_for": "SRE 审批"}), &session, &run),
    )
    .await;
    assert_eq!(waiting["state"], "waiting");
    assert_eq!(waiting["waiting_for"], "SRE 审批");
    let resumed = act("resume", json!({"next": "看审批意见"})).await;
    assert_eq!(resumed["state"], "active");
    assert!(
        resumed.get("waiting_for").is_none(),
        "离开 waiting 清空：{resumed}"
    );
    assert_eq!(resumed["next"], "看审批意见");

    let done = act(
        "done",
        in_run(json!({"expected_revision": 4}), &session, &run),
    )
    .await;
    assert_eq!(done["state"], "done");
    assert_eq!(done["revision"], 5);
    assert!(done["closed_at"].as_str().is_some(), "{done}");

    // 状态不允许：报错、写出当前状态，什么都不改。
    let (status, body) = gateway
        .post_json(&format!("/v1/matters/{id}/next"), json!({"next": "再看看"}))
        .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("done"),
        "{body}"
    );
    // 已经是 done 再 done：原样返回、不升版。
    assert_eq!(act("done", json!({})).await["revision"], 5);
    // 预期版本不符：版本冲突。
    let (status, body) = gateway
        .post_json(
            &format!("/v1/matters/{id}/reopen"),
            json!({"expected_revision": 1}),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "version_conflict", "{body}");

    // 清单默认只给未结案的。
    let (_, open) = gateway.get_json("/v1/matters").await;
    assert_eq!(open["matters"].as_array().unwrap().len(), 0, "{open}");
    let (_, all) = gateway.get_json("/v1/matters?all=true").await;
    assert_eq!(all["matters"].as_array().unwrap().len(), 1, "{all}");

    let reopened = act("reopen", json!({})).await;
    assert_eq!(reopened["state"], "active");
    assert!(reopened.get("closed_at").is_none(), "{reopened}");
    let (status, shown) = gateway.get_json(&format!("/v1/matters/{id}")).await;
    assert_eq!(status, 200, "{shown}");
    assert_eq!(shown["matter"]["revision"], 6);
}

/// 领域：add → edit → retire → reopen；目标的动词用在领域上被拒，种类专属字段也是。
#[tokio::test]
async fn an_area_retires_and_reopens_and_refuses_goal_verbs() {
    let gateway = start(0).await;
    let added = post(
        &gateway,
        "/v1/matters",
        json!({"kind": "area", "title": "告警值班轮换", "standard": "每周一前完成"}),
    )
    .await;
    let id = added["matter"]["id"].as_str().unwrap().to_string();
    assert!(added["matter"]["evidence"].is_null(), "终端里不带证据");

    let (status, body) = gateway
        .post_json(&format!("/v1/matters/{id}/park"), json!({}))
        .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = gateway
        .post_json(
            &format!("/v1/matters/{id}/edit"),
            json!({"due": "2026-11-01"}),
        )
        .await;
    assert_eq!(status, 400, "领域没有截止日期：{body}");
    let (status, body) = gateway
        .post_json(
            "/v1/matters",
            json!({"kind": "area", "title": "家里的设备", "due": "2026-11-01"}),
        )
        .await;
    assert_eq!(status, 400, "{body}");

    let edited = post(
        &gateway,
        &format!("/v1/matters/{id}/edit"),
        json!({"standard": "每周一 10 点前完成", "kind": "area"}),
    )
    .await;
    assert_eq!(edited["matter"]["standard"], "每周一 10 点前完成");
    let retired = post(&gateway, &format!("/v1/matters/{id}/retire"), json!({})).await;
    assert_eq!(retired["matter"]["state"], "retired");
    let reopened = post(&gateway, &format!("/v1/matters/{id}/reopen"), json!({})).await;
    assert_eq!(reopened["matter"]["state"], "active");
    let (_, list) = gateway.get_json("/v1/matters?kind=area").await;
    assert_eq!(list["matters"].as_array().unwrap().len(), 1, "{list}");
    let (_, goals) = gateway.get_json("/v1/matters?kind=goal").await;
    assert_eq!(goals["matters"].as_array().unwrap().len(), 0, "{goals}");
}

/// Run 里没有用户原话：改承诺被拒、推进照常。只给 session 不给 run 也被拒。
#[tokio::test]
async fn a_run_without_user_words_can_progress_but_not_commit() {
    let gateway = start(1).await;
    let (session, _run) = user_run(&gateway, "随便聊聊").await;
    let silent = RunId::from_raw("not-a-run-of-this-session");

    let (status, body) = gateway
        .post_json(
            "/v1/matters",
            in_run(
                json!({"kind": "goal", "title": "发布观察"}),
                &session,
                &silent,
            ),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("找不到用户说的话"),
        "{body}"
    );
    let (status, body) = gateway
        .post_json(
            "/v1/matters",
            json!({"kind": "goal", "title": "发布观察", "session": session}),
        )
        .await;
    assert_eq!(status, 400, "{body}");

    let id = post(
        &gateway,
        "/v1/matters",
        json!({"kind": "goal", "title": "发布观察"}),
    )
    .await["matter"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let next = post(
        &gateway,
        &format!("/v1/matters/{id}/next"),
        in_run(json!({"next": "提工单"}), &session, &silent),
    )
    .await;
    assert_eq!(next["matter"]["revision"], 2);
    let (status, body) = gateway
        .post_json(
            &format!("/v1/matters/{id}/drop"),
            in_run(json!({}), &session, &silent),
        )
        .await;
    assert_eq!(status, 400, "{body}");
}

/// 给别人用的 Agent（`memory = private / off`）的会话：读写一律拒（§2）。
#[tokio::test]
async fn a_private_or_off_session_cannot_see_or_change_matters() {
    let gateway = start(0).await;
    post(
        &gateway,
        "/v1/matters",
        json!({"kind": "goal", "title": "发布观察"}),
    )
    .await;
    let core = komo_gateway::service::test_support::driver_stream::core(&gateway);
    for agent in ["helper", "quiet"] {
        let session = SessionId::from_raw(core.main_session(agent).await.unwrap().0.id);
        let (status, body) = gateway
            .get_json(&format!("/v1/matters?session={session}"))
            .await;
        assert_eq!(status, 403, "{agent}：{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("memory = "),
            "{body}"
        );
        let (status, body) = gateway
            .post_json(
                "/v1/matters",
                in_run(
                    json!({"kind": "goal", "title": "替别人记的事"}),
                    &session,
                    &RunId::from_raw("any"),
                ),
            )
            .await;
        assert_eq!(status, 403, "{agent}：{body}");
    }
    // 共享记忆的默认 Agent 照常看得到。
    let assistant = SessionId::from_raw(core.main_session("assistant").await.unwrap().0.id);
    let (status, body) = gateway
        .get_json(&format!("/v1/matters?session={assistant}"))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["matters"].as_array().unwrap().len(), 1, "{body}");
}
