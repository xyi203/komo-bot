//! 目标上的定时、详情、运行历史、绑定支线（`docs/redesign.md` §2.2、§2.3、P22），走真 Gateway、真 aux.db：
//! schedule / unschedule / run 三个动词的规矩、调度器只触发还在追踪的目标且不动目标的版本、
//! `GET /v1/matters/{id}` 的 overview、`/runs` 分页、`/chat` 绑定与解除、定时结果与停住的提问投到
//! 绑定的支线（没绑定投主对话）、`GET /v1/chats` 的 `goal` 与 `GET /v1/matters` 的 `waiting`。

use komo_kernel::cron::{CronFiring, FiringStatus};
use komo_kernel::events::EventPayload;
use komo_kernel::types::ids::{CronJobId, SessionId};
use serde_json::{Value, json};

use crate::confirm::{bump_then, outward_home};
use crate::harness::*;

fn command_schedule() -> Value {
    json!({ "schedule": "0 9 * * *", "timezone": "UTC", "command": "true" })
}

fn prompt_schedule(notify: &str) -> Value {
    json!({ "schedule": "0 9 * * *", "timezone": "UTC", "prompt": "整理今天的技术动态",
            "notify": notify })
}

async fn ok(gw: &Gw, path: &str, body: Value) -> Value {
    let (status, body) = gw.post_json(path, body).await;
    assert_eq!(status, 200, "{path}：{body}");
    body
}

/// 新建一个目标；`schedule` 给了就同时设定时。返回 id。
async fn goal(gw: &Gw, title: &str, schedule: Option<Value>) -> String {
    let mut body = json!({ "kind": "goal", "title": title });
    if let Some(schedule) = schedule {
        body["schedule"] = schedule;
    }
    let created = ok(gw, "/v1/matters", body).await;
    created["matter"]["id"].as_str().unwrap().to_string()
}

async fn detail(gw: &Gw, id: &str) -> Value {
    let (status, body) = gw.get_json(&format!("/v1/matters/{id}")).await;
    assert_eq!(status, 200, "{body}");
    body
}

async fn chats(gw: &Gw) -> Value {
    let (status, body) = gw.get_json("/v1/chats").await;
    assert_eq!(status, 200, "{body}");
    body
}

fn row<'a>(chats: &'a Value, session: &str) -> Option<&'a Value> {
    chats["chats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == session)
}

/// 等某段对话里 Komo 说了满足条件的话。
async fn wait_line_in(gw: &Gw, session: &str, wanted: impl Fn(&str) -> bool) -> String {
    let id = SessionId::from_raw(session);
    for _ in 0..200 {
        let lines = gw
            .events(&id)
            .await
            .into_iter()
            .filter_map(|e| match e.payload {
                EventPayload::LineReply(line) => Some(line.text),
                _ => None,
            });
        if let Some(line) = lines.into_iter().find(|line| wanted(line)) {
            return line;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("等了 10 秒，{session} 里没有想要的话");
}

async fn fire(gw: &Gw, id: &str) -> komo_kernel::types::ids::RunId {
    let job = CronJobId::from_raw(id);
    gw.make_due(&job).await;
    let run = gw.tick().await.fired[0].run.clone();
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;
    run
}

// ---------------------------------------------------------------- 动词

/// schedule / unschedule / run：定时是目标的配置，不升目标的版本；结案的目标不给设、不给触发，去掉随时可以；
/// 没有定时的目标 run / unschedule 都是 400。
#[tokio::test]
async fn the_schedule_verbs_follow_the_goals_state() {
    let gw = Home::new().start(FakeLlm::finisher("好")).await;
    let id = goal(&gw, "每日巡检", None).await;
    let post = |verb: &'static str, body: Value| {
        let (gw, id) = (&gw, id.clone());
        async move {
            gw.post_json(&format!("/v1/matters/{id}/{verb}"), body)
                .await
        }
    };

    for verb in ["run", "unschedule"] {
        let (status, body) = post(verb, json!({})).await;
        assert_eq!(status, 400, "没有定时：{verb} {body}");
    }
    // 设定时至少要给一个字段；设了以后可以只改一个。
    let (status, body) = post("schedule", json!({})).await;
    assert_eq!(status, 400, "{body}");
    let set = ok(
        &gw,
        &format!("/v1/matters/{id}/schedule"),
        json!({ "schedule": command_schedule() }),
    )
    .await;
    assert_eq!(set["schedule"]["job"]["id"], id.as_str());
    assert_eq!(set["matter"]["revision"], 1, "定时不升目标的版本");

    let run = ok(&gw, &format!("/v1/matters/{id}/run"), json!({})).await;
    assert_eq!(run["fired"]["job"], id.as_str(), "{run}");
    assert_eq!(run["matter"]["revision"], 1);

    // 搁置的目标还能设、还能手动跑（只是调度器不触发，见下一条）。
    ok(&gw, &format!("/v1/matters/{id}/park"), json!({})).await;
    ok(&gw, &format!("/v1/matters/{id}/run"), json!({})).await;

    // 结案：不给设、不给触发（409）；去掉随时可以，且只能去一次。
    ok(&gw, &format!("/v1/matters/{id}/drop"), json!({})).await;
    for verb in ["schedule", "run"] {
        let body = if verb == "schedule" {
            json!({ "schedule": command_schedule() })
        } else {
            json!({})
        };
        let (status, body) = post(verb, body).await;
        assert_eq!(status, 409, "结案的目标 {verb}：{body}");
        assert_eq!(body["error"]["code"], "conflict", "{body}");
    }
    let gone = ok(&gw, &format!("/v1/matters/{id}/unschedule"), json!({})).await;
    assert!(gone.get("schedule").is_none(), "{gone}");
    let (status, _) = post("unschedule", json!({})).await;
    assert_eq!(status, 400);
    // 别的动词不认 schedule 字段，schedule 动词也不改目标的内容。
    let other = goal(&gw, "另一个", None).await;
    let (status, body) = gw
        .post_json(
            &format!("/v1/matters/{other}/next"),
            json!({ "next": "x", "schedule": command_schedule() }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = gw
        .post_json(
            &format!("/v1/matters/{other}/schedule"),
            json!({ "title": "改名", "schedule": command_schedule() }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    gw.stop().await;
}

// ---------------------------------------------------------------- 调度器

/// 调度器读目标：只触发还在追踪（进行中 / 等待中）的；搁置与结案的到了点也不响。
/// 触发之后槽位推进、定时版本与**目标的 revision** 都不动。
#[tokio::test]
async fn the_scheduler_fires_tracked_goals_only_and_leaves_the_goals_revision_alone() {
    let gw = Home::new().start(FakeLlm::finisher("好")).await;
    let id = goal(&gw, "每日巡检", Some(command_schedule())).await;
    let job_id = CronJobId::from_raw(&id);
    let before = detail(&gw, &id).await;
    let revision = before["matter"]["revision"].clone();
    let version = before["schedule"]["job"]["version"].clone();

    // 搁置：到点也不触发。
    ok(&gw, &format!("/v1/matters/{id}/park"), json!({})).await;
    let parked_revision = detail(&gw, &id).await["matter"]["revision"].clone();
    gw.make_due(&job_id).await;
    assert!(gw.tick().await.fired.is_empty(), "搁置的目标不响");
    assert!(gw.firings(&job_id).await.is_empty());

    // 恢复：同一个到期的槽位这就触发。
    ok(
        &gw,
        &format!("/v1/matters/{id}/resume"),
        json!({ "next": "看告警" }),
    )
    .await;
    let resumed = detail(&gw, &id).await["matter"]["revision"].clone();
    let tick = gw.tick().await;
    assert_eq!(tick.fired.len(), 1, "恢复后触发");
    gw.wait_state(&tick.fired[0].run.clone(), |s| s.is_terminal(), "终态")
        .await;
    let after = detail(&gw, &id).await;
    assert_eq!(after["matter"]["revision"], resumed, "触发不升目标的版本");
    assert_ne!(resumed, parked_revision);
    assert_ne!(resumed, revision, "（状态变更各升了版）");
    assert_eq!(
        after["schedule"]["job"]["version"], version,
        "也不升定时的版本"
    );
    let next = after["schedule"]["job"]["next_run_at"].as_str().unwrap();
    let next =
        time::OffsetDateTime::parse(next, &time::format_description::well_known::Rfc3339).unwrap();
    assert!(
        next > time::OffsetDateTime::now_utc(),
        "槽位推进到未来：{after}"
    );
    assert!(after["schedule"]["job"]["last_error"].is_null(), "{after}");

    // 等待中的目标仍在追踪：照样触发（跨过 1 秒，免得与上一槽同一计划时间被判重）。
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    ok(
        &gw,
        &format!("/v1/matters/{id}/wait"),
        json!({ "waiting_for": "SRE 回复" }),
    )
    .await;
    gw.make_due(&job_id).await;
    assert_eq!(gw.tick().await.fired.len(), 1, "waiting 仍触发");

    // 结案：不响；去掉定时之后目标与历史都在。
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    ok(&gw, &format!("/v1/matters/{id}/drop"), json!({})).await;
    gw.make_due(&job_id).await;
    assert!(gw.tick().await.fired.is_empty(), "结案的目标不响");
    assert_eq!(gw.firings(&job_id).await.len(), 2);
    gw.stop().await;
}

// ---------------------------------------------------------------- 详情与历史

/// overview：没绑定、没触发时是空的；触发之后活动里有这次运行，带目标的标题；`/runs` 分页新的在前，
/// 游标接得上，limit 被夹在 1..=100，没有这个目标是 404。
#[tokio::test]
async fn the_detail_overview_and_the_run_history_page() {
    let gw = Home::new()
        .start(FakeLlm::always(vec![text_round(1, "整理好了")]))
        .await;
    let id = goal(&gw, "每日巡检", Some(prompt_schedule("never"))).await;
    let shown = detail(&gw, &id).await;
    let overview = &shown["overview"];
    assert!(overview.get("chat").is_none_or(Value::is_null), "{shown}");
    for key in ["monitors", "activity", "memories"] {
        assert!(overview.get(key).is_none(), "空的不带：{key} {shown}");
    }

    let run = fire(&gw, &id).await;
    let shown = detail(&gw, &id).await;
    let activity = shown["overview"]["activity"].as_array().unwrap();
    assert_eq!(activity.len(), 1, "{shown}");
    let task = &activity[0]["task"];
    assert!(task["session"].is_string(), "{shown}");
    assert_eq!(task["goals"][0]["id"], id.as_str());
    assert_eq!(task["goals"][0]["title"], "每日巡检");
    assert!(activity[0].get("waiting").is_none_or(Value::is_null));

    // 再塞四条更早的历史（跳过 / 出错 / 排队），共五条。
    let job = gw
        .state()
        .cron
        .get(&CronJobId::from_raw(&id))
        .await
        .unwrap()
        .unwrap();
    let now = time::OffsetDateTime::now_utc();
    for (days, status, error) in [
        (1, FiringStatus::Skipped, Some("上一次还没结束")),
        (2, FiringStatus::Error, Some("模型后端坏了")),
        (3, FiringStatus::Ok, None),
        (4, FiringStatus::Queued, None),
    ] {
        let at = now - time::Duration::days(days);
        let mut firing = CronFiring::claimed(&job, at);
        assert!(gw.state().cron.claim_firing(firing.clone()).await.unwrap());
        firing.status = status;
        firing.error = error.map(str::to_string);
        assert!(gw.state().cron.update_firing(firing).await.unwrap());
    }
    let runs = |query: &str| {
        let gw = &gw;
        let path = format!("/v1/matters/{id}/runs{query}");
        async move {
            let (status, body) = gw.get_json(&path).await;
            assert_eq!(status, 200, "{path}：{body}");
            body
        }
    };
    let first = runs("?limit=2").await;
    let page = first["runs"].as_array().unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0]["run"], run.as_str(), "最新的在前：{first}");
    assert!(page[0]["fired_at"].is_string(), "触发过的有实际时刻");
    assert_eq!(page[0]["status"], "ok");
    assert_eq!(page[1]["status"], "skipped");
    assert_eq!(page[1]["error"], "上一次还没结束");
    assert!(
        page[1].get("fired_at").is_none_or(Value::is_null),
        "跳过的没有"
    );
    let cursor = first["next_before"].as_str().unwrap().to_string();
    assert_eq!(cursor, page[1]["scheduled_at"].as_str().unwrap());

    let second = runs(&format!("?limit=2&before={}", cursor.replace('+', "%2B"))).await;
    let page = second["runs"].as_array().unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0]["status"], "error");
    assert_eq!(page[1]["status"], "ok");
    let last = runs(&format!(
        "?limit=2&before={}",
        second["next_before"].as_str().unwrap().replace('+', "%2B")
    ))
    .await;
    assert_eq!(last["runs"].as_array().unwrap().len(), 1, "{last}");
    assert!(
        last.get("next_before").is_none_or(Value::is_null),
        "到头了：{last}"
    );

    assert_eq!(
        runs("?limit=0").await["runs"].as_array().unwrap().len(),
        1,
        "下限 1"
    );
    assert_eq!(
        runs("?limit=500").await["runs"].as_array().unwrap().len(),
        5
    );
    assert_eq!(runs("").await["runs"].as_array().unwrap().len(), 5);

    for path in ["/v1/matters/nope/runs", "/v1/matters/nope"] {
        let (status, body) = gw.get_json(path).await;
        assert_eq!(status, 404, "{path}：{body}");
    }
    gw.stop().await;
}

// ---------------------------------------------------------------- 绑定支线

/// 绑定：新开一段支线、重复不再开、解除幂等、`GET /v1/chats` 的行带 `goal`；结案的目标不给新绑；
/// 支线归档之后读成没绑定，可以再绑一段新的。
#[tokio::test]
async fn binding_a_side_chat_to_a_goal() {
    let gw = Home::new().start(FakeLlm::finisher("好")).await;
    let id = goal(&gw, "发布观察", None).await;
    let url = format!("/v1/matters/{id}/chat");

    let first = ok(&gw, &url, json!({})).await;
    assert_eq!(first["created"], true);
    assert_eq!(first["chat"]["title"], "发布观察");
    let session = first["chat"]["session"].as_str().unwrap().to_string();
    let again = ok(&gw, &url, json!({})).await;
    assert_eq!(again["created"], false, "重复绑定返回原来那段");
    assert_eq!(again["chat"]["session"], session.as_str());

    let list = chats(&gw).await;
    let side = row(&list, &session).expect("支线在对话列表里");
    assert_eq!(
        side["goal"],
        json!({ "id": id, "title": "发布观察" }),
        "{list}"
    );
    assert_eq!(side["main"], false);
    assert!(list["chats"][0]["goal"].is_null(), "主对话没有：{list}");
    assert_eq!(
        detail(&gw, &id).await["overview"]["chat"]["session"],
        session.as_str()
    );

    // 解除：支线留着，变回普通支线；再解除一次也成功。
    for _ in 0..2 {
        let (status, body) = gw.request(reqwest::Method::DELETE, &url, None).await;
        assert_eq!(status, 200, "{body}");
        assert!(serde_json::from_str::<Value>(&body).unwrap()["chat"].is_null());
    }
    let list = chats(&gw).await;
    assert!(
        row(&list, &session).expect("支线还在")["goal"].is_null(),
        "{list}"
    );
    assert!(
        detail(&gw, &id).await["overview"]
            .get("chat")
            .is_none_or(Value::is_null)
    );

    // 绑定的支线被归档（删除）：视为未绑定，读不出旗，再绑是新开一段。
    let rebound = ok(&gw, &url, json!({})).await;
    assert_eq!(rebound["created"], true);
    let second = rebound["chat"]["session"].as_str().unwrap().to_string();
    assert_ne!(second, session);
    ok(&gw, &format!("/v1/sessions/{second}/delete"), json!({})).await;
    assert!(
        detail(&gw, &id).await["overview"]
            .get("chat")
            .is_none_or(Value::is_null),
        "归档的支线读成未绑定"
    );
    assert!(row(&chats(&gw).await, &second).is_none());
    let third = ok(&gw, &url, json!({})).await;
    assert_eq!(third["created"], true);
    assert_ne!(third["chat"]["session"], second.as_str());

    // 结案的目标：没绑定时不给新绑（409）；解除照常。
    let closed = goal(&gw, "已办完", None).await;
    ok(&gw, &format!("/v1/matters/{closed}/done"), json!({})).await;
    let (status, body) = gw
        .post_json(&format!("/v1/matters/{closed}/chat"), json!({}))
        .await;
    assert_eq!(status, 409, "{body}");
    let (status, _) = gw
        .request(
            reqwest::Method::DELETE,
            &format!("/v1/matters/{closed}/chat"),
            None,
        )
        .await;
    assert_eq!(status, 200);
    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
        let (status, body) = gw.request(method, "/v1/matters/nope/chat", None).await;
        assert_eq!(status, 404, "{body}");
    }
    gw.stop().await;
}

/// 定时结果投到绑定的支线；没绑定（或解除之后）投主对话。
#[tokio::test]
async fn a_scheduled_result_lands_in_the_bound_chat_or_else_the_main_chat() {
    let gw = Home::new()
        .start(FakeLlm::always(vec![text_round(1, "整理好了")]))
        .await;
    let id = goal(&gw, "每日巡检", Some(prompt_schedule("always"))).await;
    let bound = ok(&gw, &format!("/v1/matters/{id}/chat"), json!({})).await;
    let side = bound["chat"]["session"].as_str().unwrap().to_string();

    fire(&gw, &id).await;
    let line = wait_line_in(&gw, &side, |line| line.contains("整理好了")).await;
    assert!(line.starts_with("定时任务「每日巡检」"), "{line}");
    let main = crate::notify::main_lines(&gw).await;
    assert!(main.is_empty(), "绑定了就不投主对话：{main:?}");

    // 解除之后，下一次回主对话。
    let (status, _) = gw
        .request(
            reqwest::Method::DELETE,
            &format!("/v1/matters/{id}/chat"),
            None,
        )
        .await;
    assert_eq!(status, 200);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    fire(&gw, &id).await;
    crate::notify::wait_line(&gw, "回到主对话", |line| line.contains("整理好了")).await;
    gw.stop().await;
}

/// 停住的提问投到绑定的支线：`GET /v1/chats` 里那段支线在等你、主对话不在；`GET /v1/matters` 的 `waiting`
/// 指向那段支线与介入句柄；详情的活动与运行历史也标出来；答复之后都消失。
#[tokio::test]
async fn a_stopped_ask_waits_in_the_bound_chat_and_the_goal_shows_it() {
    let (home, _counter) = outward_home("");
    let gw = home.start(bump_then("加好了")).await;
    let id = goal(
        &gw,
        "每日加一",
        Some(json!({ "schedule": "0 9 * * *", "timezone": "UTC", "prompt": "加一" })),
    )
    .await;
    let side = ok(&gw, &format!("/v1/matters/{id}/chat"), json!({})).await["chat"]["session"]
        .as_str()
        .unwrap()
        .to_string();
    let other = goal(&gw, "不相干", None).await;

    gw.make_due(&CronJobId::from_raw(&id)).await;
    let run = gw.tick().await.fired[0].run.clone();
    let pending = gw.wait_intervention(&run).await;
    wait_line_in(&gw, &side, |line| line.contains("mcp__demo__bump")).await;

    let list = chats(&gw).await;
    assert_eq!(row(&list, &side).unwrap()["waiting"], true, "{list}");
    assert_eq!(list["chats"][0]["waiting"], false, "主对话没有在等：{list}");
    assert_eq!(list["waitingCount"], 1);

    let (_, matters) = gw.get_json("/v1/matters").await;
    let waiting = &matters["waiting"];
    assert_eq!(waiting[&id]["chat"], side.as_str(), "{matters}");
    assert_eq!(waiting[&id]["handle"], pending.handle.as_str());
    assert!(
        waiting.get(&other).is_none(),
        "没在等的目标不出现：{matters}"
    );
    let shown = detail(&gw, &id).await;
    assert_eq!(
        shown["overview"]["activity"][0]["waiting"]["handle"],
        pending.handle.as_str(),
        "{shown}"
    );
    let (_, runs) = gw.get_json(&format!("/v1/matters/{id}/runs")).await;
    assert_eq!(runs["runs"][0]["waiting"]["chat"], side.as_str(), "{runs}");

    gw.answer(&pending.handle, "approve").await;
    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;
    let (_, matters) = gw.get_json("/v1/matters").await;
    assert!(matters.get("waiting").is_none(), "答完不再等：{matters}");
    assert_eq!(chats(&gw).await["waitingCount"], 0);
    gw.stop().await;
}
