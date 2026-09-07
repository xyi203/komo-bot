use super::*;
use komo_core::domain::cron::{CronJobStatus, NotifyPolicy, RoutineRunStatus};

/// A `komo.db` in a home of this test's own — a home holds transcripts
/// beside the db, and two tests sharing a directory would read each
/// other's conversations.
fn turso_url(name: &str) -> String {
    let home = std::env::temp_dir().join(format!("komo-cron-{name}"));
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    format!("turso:{}", home.join("komo.db").display())
}

#[tokio::test]
async fn job_roundtrip_update_and_delete() {
    let db = Db::connect(&turso_url("komo_cron_repo_test.db"))
        .await
        .unwrap();
    let job = CronJob::new(
        "weekly",
        Trigger::cron("0 14 * * 5"),
        CronAction::Command {
            command: "/opt/rotate.py".into(),
            args: vec!["--push".into(), "第二个".into()],
            workdir: Some("/opt".into()),
            timeout_secs: 600,
        },
        1234,
    );

    db.save(&job).await.unwrap();
    let listed = db.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "weekly");
    let CronAction::Command {
        command,
        args,
        workdir,
        timeout_secs,
    } = &listed[0].action
    else {
        panic!("command job");
    };
    assert_eq!(command, "/opt/rotate.py");
    assert_eq!(args, &vec!["--push".to_string(), "第二个".to_string()]);
    assert_eq!(workdir.as_deref(), Some("/opt"));
    assert_eq!(*timeout_secs, 600);
    assert_eq!(listed[0].next_run_at, 1234);
    assert_eq!(listed[0].status, CronJobStatus::Active);
    assert_eq!(listed[0].trigger, Trigger::cron("0 14 * * 5"));
    assert!(listed[0].runs.is_empty());
    assert_eq!(listed[0].notify, NotifyPolicy::Always);

    let mut updated = listed[0].clone();
    updated.status = CronJobStatus::Paused;
    updated.next_run_at = 9999;
    updated.notify = NotifyPolicy::OnError;
    updated.last_error = "exit status: 3".into();
    let run = updated.begin_run(5000);
    updated.finish_run(
        &run,
        RoutineRunStatus::Error,
        "boom\n",
        Some("cron:weekly:5000".into()),
    );
    db.update(&updated).await.unwrap();

    let found = db.find_by_name("weekly").await.unwrap().unwrap();
    assert_eq!(found.status, CronJobStatus::Paused);
    assert_eq!(found.next_run_at, 9999);
    assert_eq!(found.notify, NotifyPolicy::OnError);
    assert_eq!(found.last_error, "exit status: 3");
    let last = found.last_run().expect("one recorded run");
    assert_eq!(last.status, RoutineRunStatus::Error);
    assert_eq!(last.started_at, 5000);
    assert_eq!(last.output, "boom\n");
    assert_eq!(last.session_id.as_deref(), Some("cron:weekly:5000"));

    assert!(db.delete("weekly").await.unwrap());
    assert!(
        !db.delete("weekly").await.unwrap(),
        "second delete is a no-op"
    );
    assert!(db.list().await.unwrap().is_empty());
}

/// A routine stored with a trigger this build no longer has — one that
/// fired on an event rather than a clock — is skipped and named, never
/// read as some clock trigger and never failing the listing for the jobs
/// beside it. It stays deletable, because "get rid of it" is the only
/// thing left to do with it.
///
/// Its run history still loads: `event` is an unknown field on
/// `RoutineRun` now, and serde ignores those.
#[tokio::test]
async fn a_job_whose_trigger_no_longer_reads_is_skipped_not_fatal() {
    let home = std::env::temp_dir().join("komo-cron-retired-trigger");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("test home");
    let path = home.join("komo.db");
    let db = Db::connect(&format!("turso:{}", path.display()))
        .await
        .unwrap();

    let mut nightly =
        CronJob::new_command("nightly", Trigger::cron("0 3 * * *"), "/bin/true", 3000);
    let run = nightly.begin_run(2000);
    nightly.finish_run(&run, RoutineRunStatus::Ok, "ok", None);
    db.save(&nightly).await.unwrap();

    // The row a `@webhook ci` routine left behind, run history and all.
    let mut hooked = CronJob::new_command("on-ci", Trigger::cron("0 3 * * *"), "/bin/true", 0);
    hooked.id = "id-hooked".into();
    db.save(&hooked).await.unwrap();
    {
        let mut conn = db.inner.connection().await.unwrap();
        let mut record = CronJobRecord::get_by_id(&mut conn, "id-hooked")
            .await
            .unwrap();
        record
                .update()
                .trigger(r#"{"kind":"webhook","name":"ci"}"#.to_string())
                .runs(
                    r#"[{"id":"r1","status":"ok","started_at":10,"event":"webhook `ci`","output":"done"}]"#
                        .to_string(),
                )
                .exec(&mut conn)
                .await
                .unwrap();
    }

    let listed = db.list().await.unwrap();
    assert_eq!(
        listed.iter().map(|j| j.name.as_str()).collect::<Vec<_>>(),
        vec!["nightly"],
        "the readable routine survives its neighbour"
    );
    assert_eq!(
        listed[0].last_run().unwrap().output,
        "ok",
        "a run written with an `event` field still deserializes"
    );
    assert!(db.find_by_name("on-ci").await.unwrap().is_none());
    assert!(db.delete("on-ci").await.unwrap(), "and it can be removed");
    assert!(!db.delete("on-ci").await.unwrap());
}

#[tokio::test]
async fn agent_job_roundtrips() {
    let db = Db::connect(&turso_url("komo_cron_agent_test.db"))
        .await
        .unwrap();
    let job = CronJob::new(
        "brief",
        Trigger::cron("0 8 * * *"),
        CronAction::Agent {
            prompt: "总结我今天的日程".into(),
            skills: vec!["calendar".into(), "weather".into()],
            workspace: None,
        },
        42,
    );
    db.save(&job).await.unwrap();
    let found = db.find_by_name("brief").await.unwrap().unwrap();
    let CronAction::Agent { prompt, skills, .. } = &found.action else {
        panic!("agent job");
    };
    assert_eq!(prompt, "总结我今天的日程");
    assert_eq!(skills, &vec!["calendar".to_string(), "weather".to_string()]);
    assert_eq!(found.next_run_at, 42);
}

/// An agent job's workspace and a command job's workdir share one column,
/// discriminated by `kind`. Both are read back here, in one store, so a
/// mapping that crossed the two would surface as a job confined to another
/// job's directory rather than as a compile error.
#[tokio::test]
async fn an_agent_workspace_and_a_command_workdir_share_a_column_without_crossing() {
    let db = Db::connect(&turso_url("komo_cron_workspace_test.db"))
        .await
        .unwrap();
    db.save(&CronJob::new(
        "tidy",
        Trigger::cron("0 8 * * *"),
        CronAction::Agent {
            prompt: "tidy".into(),
            skills: vec![],
            workspace: Some("/srv/notes".into()),
        },
        42,
    ))
    .await
    .unwrap();
    db.save(&CronJob::new(
        "backup",
        Trigger::cron("0 9 * * *"),
        CronAction::Command {
            command: "/bin/true".into(),
            args: vec![],
            workdir: Some("/srv/backups".into()),
            timeout_secs: 60,
        },
        42,
    ))
    .await
    .unwrap();

    let agent = db.find_by_name("tidy").await.unwrap().unwrap();
    let CronAction::Agent { workspace, .. } = &agent.action else {
        panic!("agent job");
    };
    assert_eq!(workspace.as_deref(), Some("/srv/notes"));

    let command = db.find_by_name("backup").await.unwrap().unwrap();
    let CronAction::Command { workdir, .. } = &command.action else {
        panic!("command job");
    };
    assert_eq!(workdir.as_deref(), Some("/srv/backups"));
}

/// Grants survive save → read → update → read, field for field. An
/// approval the operator gave once must not quietly widen or narrow because
/// the job's `last_run_at` was stamped.
#[tokio::test]
async fn job_grants_roundtrip_through_save_and_update() {
    let db = Db::connect(&turso_url("komo_cron_grants_test.db"))
        .await
        .unwrap();
    let grant = RuleSpec {
        category: "homeassistant".into(),
        matcher: "exact".into(),
        value: "climate.set_temperature".into(),
        access: None,
        channels: None,
        effect: "allow".into(),
        include_dangerous: false,
        unattended: true,
    };
    let job = CronJob::new(
        "ac-temp",
        Trigger::cron("0 22 * * *"),
        CronAction::Agent {
            prompt: "设到 26 度".into(),
            skills: vec![],
            workspace: None,
        },
        0,
    )
    .with_grants(vec![grant]);
    db.save(&job).await.unwrap();

    let found = db.find_by_name("ac-temp").await.unwrap().unwrap();
    assert_eq!(found.grants.len(), 1);
    assert_eq!(found.grants[0].value, "climate.set_temperature");
    assert!(found.grants[0].unattended);
    // And it parses into the rule the policy engine will match on.
    let rules = found.granted_rules();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].value, "climate.set_temperature");

    let mut updated = found;
    let run = updated.begin_run(999);
    updated.finish_run(&run, RoutineRunStatus::Ok, "26", None);
    db.update(&updated).await.unwrap();
    let again = db.find_by_name("ac-temp").await.unwrap().unwrap();
    assert_eq!(again.grants.len(), 1, "update must not drop grants");
    assert_eq!(again.last_run().unwrap().started_at, 999);
}

/// A job without grants writes the empty column, so it is indistinguishable
/// from a pre-column row — no `'[]'` noise for an operator reading the db.
#[tokio::test]
async fn a_job_without_grants_stores_nothing() {
    assert_eq!(encode_grants(&[]).unwrap(), "");
}

/// **Revocation.** Deleting a job takes its permissions with it. This is
/// the whole advantage over a global `unattended = true` config rule, which
/// outlives whatever it was written for, so it gets its own test rather
/// than being left as a property of `delete` nobody checks.
#[tokio::test]
async fn removing_a_job_revokes_its_grants() {
    let db = Db::connect(&turso_url("komo_cron_revoke_test.db"))
        .await
        .unwrap();
    let job = CronJob::new(
        "ac-temp",
        Trigger::cron("0 22 * * *"),
        CronAction::Agent {
            prompt: "设到 26 度".into(),
            skills: vec![],
            workspace: None,
        },
        0,
    )
    .with_grants(vec![RuleSpec {
        category: "homeassistant".into(),
        matcher: "exact".into(),
        value: "climate.set_temperature".into(),
        access: None,
        channels: None,
        effect: "allow".into(),
        include_dangerous: false,
        unattended: true,
    }]);
    db.save(&job).await.unwrap();
    assert!(db.delete("ac-temp").await.unwrap());

    // Nothing anywhere in the store still grants it — no orphaned rule.
    let remaining: Vec<_> = db
        .list()
        .await
        .unwrap()
        .iter()
        .flat_map(|j| j.granted_rules())
        .collect();
    assert!(remaining.is_empty());
}

#[tokio::test]
async fn find_by_name_returns_none_for_unknown() {
    let db = Db::connect(&turso_url("komo_cron_find_test.db"))
        .await
        .unwrap();
    assert!(db.find_by_name("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn list_orders_by_name() {
    let db = Db::connect(&turso_url("komo_cron_order_test.db"))
        .await
        .unwrap();
    for name in ["zeta", "alpha", "mid"] {
        db.save(&CronJob::new_command(
            name,
            Trigger::cron("* * * * *"),
            "/bin/true",
            0,
        ))
        .await
        .unwrap();
    }
    let names: Vec<String> = db
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|j| j.name)
        .collect();
    assert_eq!(names, vec!["alpha", "mid", "zeta"]);
}
