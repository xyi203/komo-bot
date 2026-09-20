//! Job 的定义面（§10 的字段表）与它的版本语义（§7.2）。

use std::sync::Arc;

use komo_kernel::cron::JobStatus;

use crate::harness::*;

fn daily(prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "name": "morning-summary",
        "schedule": "0 9 * * *",
        "timezone": "Asia/Shanghai",
        "prompt": prompt,
    })
}

#[tokio::test]
async fn model_menu_and_job_selection_use_catalog_aliases() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::new(vec![]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;

    let (status, body) = gw.get("/v1/models").await;
    assert_eq!(status, 200, "{body}");
    let menu: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(menu["models"][0]["id"], "job");
    assert_eq!(menu["models"][0]["model"], "job-model");
    assert_eq!(menu["models"][0]["provider"], "standalone");
    assert_eq!(menu["models"][0]["api_backend"], "responses");
    assert_eq!(menu["models"][1]["id"], "main");
    assert_eq!(menu["models"][1]["default"], true);

    let (status, body) = gw
        .post(
            "/v1/cron",
            serde_json::json!({
                "name": "x", "schedule": "0 9 * * *", "timezone": "UTC", "prompt": "p",
                "model": "not-configured",
            }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("job") && body.contains("main"), "{body}");

    gw.stop().await;
}

/// §10 的字段表**全部**过得去一次 `POST /v1/cron`，并且原样读得回来。
///
/// 一个写得进去但读不回来的字段，最坏的形态是 03:00 才发现——那时人已经走了。
#[tokio::test]
async fn every_field_in_the_job_definition_round_trips() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::new(vec![]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;

    let workdir = home.workspace();
    let job = gw
        .add_job(serde_json::json!({
            "name": "morning-summary",
            "schedule": "0 9 * * *",
            "timezone": "Asia/Shanghai",
            "prompt": "搜索今天关注的技术动态，整理后保存到 Memos",
            "workdir": workdir.display().to_string(),
            "model": "job",
            "effort": "high",
            "skills": ["memos", "search"],
            "overlap": "allow",
            "max_rounds": 12,
            "notify": "on_error",
        }))
        .await;

    assert_eq!(job.version, 1);
    assert_eq!(job.status, JobStatus::Active);
    assert_eq!(job.prompt, "搜索今天关注的技术动态，整理后保存到 Memos");
    assert_eq!(
        job.workdir,
        Some(workdir.canonicalize().expect("工作目录在")),
        "工作目录**创建时就核实**并解析成真实路径"
    );
    assert_eq!(job.skills, vec!["memos".to_string(), "search".to_string()]);
    assert_eq!(job.max_rounds, Some(12));
    assert_eq!(job.overlap, komo_kernel::cron::OverlapPolicy::Allow);
    assert_eq!(job.notify, komo_kernel::cron::NotifyPolicy::OnError);
    assert_eq!(job.effort.as_ref().map(|e| e.as_str()), Some("high"));
    assert!(job.next_run_at.is_some(), "创建就算出了下一个槽位");

    // 读回来是同一份。
    let again = gw.job(&job.id).await;
    assert_eq!(again, job);

    gw.stop().await;
}

/// **覆盖按完整模型配置解析，不能影响记忆整理或向量模型**（§10）。
#[tokio::test]
async fn a_model_override_is_a_whole_config_and_leaves_memory_alone() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::new(vec![]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;

    let job = gw
        .add_job(serde_json::json!({
            "name": "x", "schedule": "0 9 * * *", "timezone": "UTC", "prompt": "p",
            "model": "job",
        }))
        .await;

    // alias 解析成一份**完整**配置；不是在主模型配置上只换上游 model id。
    let model = job.model.expect("有模型覆盖");
    let main = gw.state().snapshot().model.clone();
    assert_eq!(model.model, "job-model");
    assert_eq!(model.base_url, "https://jobs.example.com/v1");
    assert_ne!(model.base_url, main.base_url);
    assert_eq!(model.provider, main.provider);
    assert_eq!(model.api_key_env, main.api_key_env);

    // 记忆与向量模型是另外两个角色，一个字都没动（§13.3）。
    let after = gw.state().snapshot();
    assert_eq!(
        after.memory.model.model,
        gw.state().snapshot().memory.model.model
    );
    assert_eq!(after.model.model, main.model, "主模型也没被改");

    gw.stop().await;
}

/// **不支持的 effort 在请求前拒绝，并指出支持值**（§13.3）。
#[tokio::test]
async fn an_unsupported_effort_is_refused_before_the_request_with_its_supported_values() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::new(vec![]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;

    let (status, body) = gw
        .post(
            "/v1/cron",
            serde_json::json!({
                "name": "x", "schedule": "0 9 * * *", "timezone": "UTC", "prompt": "p",
                "effort": "ultra",
            }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("ultra"), "说出是哪一档：{body}");
    assert!(
        body.contains("low") || body.contains("high") || body.contains("支持"),
        "指出支持值：{body}"
    );

    // 一个 Job 都没有留下来。
    assert!(gw.cron_list().await.jobs.is_empty());
    gw.stop().await;
}

/// 时区不认识：**当场拒绝**，并说出该写什么（§10：时区明确保存，IANA 名）。
#[tokio::test]
async fn an_unknown_timezone_is_refused_with_what_to_write() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::new(vec![]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;

    let (status, body) = gw
        .post(
            "/v1/cron",
            serde_json::json!({
                "name": "x", "schedule": "0 9 * * *", "timezone": "Mars/Olympus", "prompt": "p",
            }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("Mars/Olympus"), "{body}");
    assert!(body.contains("Asia/Shanghai"), "指出该写什么：{body}");
    assert!(gw.cron_list().await.jobs.is_empty());
    gw.stop().await;
}

/// 六字段表达式不被静默读成"带秒"：那会让"每天 9 点"变成"每分钟的第 9 秒"。
#[tokio::test]
async fn a_six_field_expression_is_refused_rather_than_read_as_seconds() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::new(vec![]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let (status, body) = gw
        .post(
            "/v1/cron",
            serde_json::json!({
                "name": "x", "schedule": "0 0 9 * * *", "timezone": "UTC", "prompt": "p",
            }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    gw.stop().await;
}

/// **改定义 → 版本 +1（授权失效）；`pause` / `resume` 不动版本**（§7.2 / §10）。
///
/// 混成一条路的代价是具体的：操作者每暂停一次就要把这个 Job 的授权重批一遍。
#[tokio::test]
async fn changing_the_definition_bumps_the_version_but_pausing_does_not() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::new(vec![]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let job = gw.add_job(daily("原来的话")).await;
    assert_eq!(job.version, 1);

    // pause / resume：只改 status。
    let (status, body) = gw
        .patch(
            &format!("/v1/cron/{}", job.id),
            serde_json::json!({"status": "paused"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let paused: komo_kernel::cron::CronJob = serde_json::from_str(&body).unwrap();
    assert_eq!(paused.status, JobStatus::Paused);
    assert_eq!(paused.version, 1, "暂停不是定义变更");

    let (_, body) = gw
        .patch(
            &format!("/v1/cron/{}", job.id),
            serde_json::json!({"status": "active"}),
        )
        .await;
    let resumed: komo_kernel::cron::CronJob = serde_json::from_str(&body).unwrap();
    assert_eq!(resumed.status, JobStatus::Active);
    assert_eq!(resumed.version, 1, "恢复也不是");
    assert!(resumed.next_run_at.is_some(), "恢复之后重新算上了槽位");

    // 改 prompt：这是定义变更，绑定这个 Job 的授权在这一刻失效。
    let (_, body) = gw
        .patch(
            &format!("/v1/cron/{}", job.id),
            serde_json::json!({"prompt": "换一句话"}),
        )
        .await;
    let edited: komo_kernel::cron::CronJob = serde_json::from_str(&body).unwrap();
    assert_eq!(edited.version, 2);
    assert_eq!(edited.prompt, "换一句话");

    // 每一个定义字段都算。
    for patch in [
        serde_json::json!({"schedule": "0 10 * * *"}),
        serde_json::json!({"timezone": "Europe/Berlin"}),
        serde_json::json!({"max_rounds": 3}),
        serde_json::json!({"overlap": "allow"}),
        serde_json::json!({"notify": "never"}),
        serde_json::json!({"skills": ["memos"]}),
        serde_json::json!({"name": "别的名字"}),
    ] {
        let before = gw.job(&job.id).await.version;
        let (status, body) = gw
            .patch(&format!("/v1/cron/{}", job.id), patch.clone())
            .await;
        assert_eq!(status, 200, "{patch} → {body}");
        let after: komo_kernel::cron::CronJob = serde_json::from_str(&body).unwrap();
        assert_eq!(after.version, before + 1, "{patch} 是定义变更");
    }

    gw.stop().await;
}

/// **`@at` 是一次性：claim 时完成，行留着当可查询的记录**（§10）。
/// 完成之后 `resume` / `run` 都不再受理它——那不是一个还能按的按钮。
#[tokio::test]
async fn a_one_shot_is_done_after_it_fires_and_refuses_to_run_again() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::always(vec![text_round(1, "做完了")])
            as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;

    let at = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let job = gw
        .add_job(serde_json::json!({
            "name": "一次性",
            "schedule": format!("@at {:04}-{:02}-{:02} {:02}:{:02}",
                at.year(), u8::from(at.month()), at.day(), at.hour(), at.minute()),
            "timezone": "UTC",
            "prompt": "提醒我一下",
        }))
        .await;

    gw.make_due(&job.id).await;
    let tick = gw.tick().await;
    assert_eq!(tick.fired.len(), 1, "{tick:?}");

    let done = gw.job(&job.id).await;
    assert_eq!(done.status, JobStatus::Done);
    assert!(done.next_run_at.is_none(), "没有下一槽就不会再被找到");
    assert_eq!(done.version, 1, "完成不是定义变更");

    // 再扫一轮：一次性触发不会响第二次。
    assert!(gw.tick().await.fired.is_empty());

    // 手动 run 也不受理。
    let (status, body) = gw
        .post(&format!("/v1/cron/{}/run", job.id), serde_json::json!({}))
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("一次性"), "{body}");

    // 但**行留着**：`komo cron list` 还看得见它，触发历史也在。
    assert!(gw.cron_list().await.jobs.iter().any(|j| j.id == job.id));
    assert_eq!(gw.firings(&job.id).await.len(), 1);

    gw.stop().await;
}

/// 过去的 `@at` **当场拒绝**——趁打字的人还在（§10）。
#[tokio::test]
async fn an_at_in_the_past_is_refused_while_the_person_is_still_there() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::new(vec![]) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let (status, body) = gw
        .post(
            "/v1/cron",
            serde_json::json!({
                "name": "x", "schedule": "@at 2020-01-01 09:00", "timezone": "UTC", "prompt": "p",
            }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    gw.stop().await;
}

/// **手动 run 使用独立请求幂等键，不冒充定时触发**（§10）：不写触发记录，不推进槽位。
#[tokio::test]
async fn a_manual_run_writes_no_firing_and_does_not_advance_the_slot() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::always(vec![text_round(1, "做完了")])
            as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let job = gw.add_job(daily("整理")).await;
    let slot = job.next_run_at.expect("有槽位");

    let (status, body) = gw
        .post(&format!("/v1/cron/{}/run", job.id), serde_json::json!({}))
        .await;
    assert_eq!(status, 200, "{body}");
    let response: komo_kernel::protocol::http::ManualCronRunResponse =
        serde_json::from_str(&body).unwrap();

    assert!(
        gw.firings(&job.id).await.is_empty(),
        "手动 run 不写触发记录"
    );
    assert_eq!(
        gw.job(&job.id).await.next_run_at,
        Some(slot),
        "也不推进槽位"
    );
    assert!(response.request_key.as_str().contains("cron-manual"));

    // 那个 Run 真的在账本上，来源是 Cron（§11.2 最后一条：来源仍是 Cron）。
    let record = komo_store::repos::runs::get(&gw.state().db, &response.run)
        .await
        .expect("读得到")
        .expect("有这一行");
    assert!(matches!(
        record.source,
        komo_kernel::types::plan::PlanSource::Cron { .. }
    ));

    gw.stop().await;
}

/// 移除：后续调度没了，**已有执行历史保留**（§13.1）。
#[tokio::test]
async fn removing_a_job_keeps_its_firings() {
    let home = Home::new();
    let gw = home
        .start(FakeLlm::always(vec![text_round(1, "做完了")])
            as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;
    let job = gw.add_job(daily("整理")).await;
    gw.make_due(&job.id).await;
    gw.tick().await;
    assert_eq!(gw.firings(&job.id).await.len(), 1);

    let (status, body) = gw
        .request(
            reqwest::Method::DELETE,
            &format!("/v1/cron/{}", job.id),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let response: komo_kernel::protocol::http::CronDeleteResponse =
        serde_json::from_str(&body).unwrap();
    assert!(response.removed);
    assert_eq!(response.firings_kept, 1);

    assert!(gw.cron_list().await.jobs.is_empty());
    assert_eq!(gw.firings(&job.id).await.len(), 1, "历史保留");

    gw.stop().await;
}

/// **执行预算是 Job 自己的**（§10）：`max_rounds = 1` 的 Job 跑一轮就被预算叫停，
/// 而不是用 Gateway 那个全局值。
///
/// 一个存得进去但从不生效的预算，等于没有预算——而它的用处正是"别让一个 Job 在凌晨
/// 把配额跑光"。
#[tokio::test]
async fn a_job_runs_on_its_own_round_budget() {
    let home = Home::new();
    // 根内读取是 Allow（§7.1 第一行），所以这一串调用不会停在审批上——停住它的只能
    // 是预算。
    //
    // 先放一份文件进去：数据目录里已经有东西，这个目录当然也在（Gateway 起过一次就有
    // 它了，§12 的骨架）。
    std::fs::create_dir_all(home.workspace()).expect("工作目录");
    let readable = home.workspace().join("notes.md");
    std::fs::write(&readable, "一点东西\n").expect("写得下");
    let path = readable.display().to_string();
    // 模型每一轮都要求再调一次工具，永远不收尾。
    let llm = FakeLlm::always(vec![
        call_round(1, "pc-1", "read", serde_json::json!({"path": path})),
        call_round(2, "pc-2", "read", serde_json::json!({"path": path})),
        call_round(3, "pc-3", "read", serde_json::json!({"path": path})),
    ]);
    let gw = home
        .start(Arc::clone(&llm) as Arc<dyn komo_kernel::traits::LlmClient>)
        .await;

    let mut body = daily("读点东西");
    body["max_rounds"] = serde_json::json!(1);
    let job = gw.add_job(body).await;
    gw.make_due(&job.id).await;
    let run = gw.tick().await.fired[0].run.clone();

    gw.wait_state(&run, |s| s.is_terminal(), "终态").await;
    let record = komo_store::repos::runs::get(&gw.state().db, &run)
        .await
        .expect("读得到")
        .expect("有这一行");
    assert_eq!(
        record.state,
        komo_kernel::types::status::RunState::Failed,
        "预算用完是一个明确的终态，不是无限循环"
    );

    // 触发记录记成 error，并且按 `notify` 投了出去（默认 always）。
    let mut settled = false;
    for _ in 0..100 {
        if gw
            .firings(&job.id)
            .await
            .first()
            .map(|f| f.status)
            .is_some_and(|s| s == komo_kernel::cron::FiringStatus::Error)
        {
            settled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(settled, "失败的那一次触发应当记成 error");

    gw.stop().await;
}
