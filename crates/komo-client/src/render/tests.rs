//! 渲染函数的快照测试：协议类型 → 字符串，全是纯函数。

use super::*;
use crate::tui::test_support as fixture;
use komo_kernel::cron::{CronJob, TimeZone, Trigger};
use komo_kernel::protocol::PROTOCOL_VERSION;
use komo_kernel::protocol::config::{KeyPath, SourceFile};
use komo_kernel::protocol::http::{
    ApprovalDecisionRecord, ModelMenuEntry, RunSummary, SessionSummary, ToolCallSummary,
};
use komo_kernel::types::chat::{ApprovalScope, PeerId};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{CronJobId, MemoryId, RunId, Seq, SessionId};
use komo_kernel::types::memory::{Evidence, EvidenceRef, ExtractionMetadata, MemoryUsage};
use komo_kernel::types::model::Effort;
use komo_kernel::types::refs::{ContentRef, OutputRef};
use komo_kernel::types::turn::MemoryUse;
use std::path::PathBuf;
use time::macros::datetime;

const NOW: OffsetDateTime = datetime!(2026-09-16 08:00:00 UTC);

// ---- session list ----

#[test]
fn a_session_list_shows_status_and_title() {
    let response = SessionListResponse {
        sessions: vec![
            SessionSummary {
                session: SessionId::from_raw("sess-1"),
                title: "清理构建目录".into(),
                workdir: Some("/home/u/project".into()),
                current_run: Some(RunId::from_raw("run-1")),
                current_status: Some(RunStatus::WaitingApproval),
                applied_seq: Seq(9),
                created_at: NOW,
                updated_at: NOW,
            },
            SessionSummary {
                session: SessionId::from_raw("sess-2"),
                title: String::new(),
                workdir: None,
                current_run: None,
                current_status: None,
                applied_seq: Seq(0),
                created_at: NOW,
                updated_at: NOW,
            },
        ],
    };
    let printed = session_list(&response);
    insta_like(
        &printed,
        &[
            "SESSION",
            "sess-1",
            "等待审批",
            "清理构建目录 · /home/u/project",
            "sess-2",
            "空闲",
            "（无标题）",
        ],
    );
}

#[test]
fn an_empty_session_list_says_so() {
    assert_eq!(session_list(&SessionListResponse::default()), "没有会话");
}

// ---- run inspect ----

fn run_detail(state: ToolCallState) -> RunDetail {
    RunDetail {
        summary: RunSummary {
            run: RunId::from_raw("run-1"),
            session: SessionId::from_raw("sess-1"),
            status: RunStatus::Completed,
            source: komo_kernel::types::plan::PlanSource::Interactive {
                session: SessionId::from_raw("sess-1"),
            },
            rounds: 2,
            created_at: NOW,
            ended_at: Some(NOW + time::Duration::seconds(9)),
        },
        calls: vec![ToolCallSummary {
            call: fixture::call(),
            tool: "shell".into(),
            state,
            attempts: 2,
            plan_hash: Some(fixture::plan().plan_hash()),
            output: Some(OutputRef(ContentRef {
                path: "tool-output/run-1/call-7/attempt-2/output.json".into(),
                size: 12,
                hash: ContentHash::of_str("x"),
                pointer: None,
            })),
            preview: Some("removed 12 files".into()),
        }],
        final_message: Some("清完了。".into()),
        memories: vec![MemoryUse {
            memory: MemoryId::from_raw("mem-1"),
            revision: 3,
        }],
    }
}

#[test]
fn run_inspect_prints_two_question_marks_for_an_uncertain_call() {
    let printed = run_inspect(&run_detail(ToolCallState::Uncertain), &[]);
    assert!(printed.contains("?? shell"), "{printed}");
    // §8.6：不能装成失败，也不能用重试成功盖掉。
    assert!(printed.contains("结果不明"), "{printed}");
    assert!(printed.contains("尝试 2 次"), "{printed}");
    assert!(printed.contains("mem-1@3"), "§9.7 的审计证据：{printed}");
}

#[test]
fn run_inspect_marks_each_call_state_differently() {
    let markers: Vec<&str> = [
        ToolCallState::Planned,
        ToolCallState::Started,
        ToolCallState::Completed,
        ToolCallState::Failed,
        ToolCallState::Uncertain,
    ]
    .into_iter()
    .map(call_marker)
    .collect();
    assert_eq!(markers, vec!["··", "▶ ", "ok", "!!", "??"]);
}

#[test]
fn run_inspect_says_who_allowed_a_call_when_the_record_is_at_hand() {
    let mut record = fixture::approval_record();
    record.decision = Some(ApprovalDecisionRecord {
        approved: true,
        scope: ApprovalScope::Run,
        by: Some(PeerId::new("ou_xxx")),
        decided_at: NOW,
        grant: None,
        consumed: true,
    });
    let printed = run_inspect(&run_detail(ToolCallState::Completed), &[record]);
    assert!(printed.contains("allowed by ou_xxx"), "{printed}");
    assert!(printed.contains("本次 Run 范围"), "{printed}");
    assert!(printed.contains("7K2M"), "{printed}");
}

#[test]
fn run_inspect_says_nothing_about_provenance_when_it_has_no_record() {
    let printed = run_inspect(&run_detail(ToolCallState::Completed), &[]);
    assert!(
        !printed.contains("allowed by"),
        "猜的放行来源不如不印：{printed}"
    );
}

#[test]
fn run_inspect_explains_an_interrupted_run_is_not_a_cancellation() {
    let mut detail = run_detail(ToolCallState::Started);
    detail.summary.status = RunStatus::Interrupted;
    let printed = run_inspect(&detail, &[]);
    assert!(printed.contains("不等于用户取消"), "{printed}");
}

// ---- approval ----

#[test]
fn an_approval_list_gives_the_short_ids_and_how_to_answer() {
    let printed = approval_list(&ApprovalListResponse {
        approvals: vec![fixture::approval_record()],
    });
    insta_like(
        &printed,
        &[
            "短ID",
            "7K2M",
            "shell",
            "run-1",
            "命中 shell 规则",
            "komo approval approve",
        ],
    );
}

#[test]
fn an_approval_show_carries_the_same_five_items_as_the_tui_popup() {
    let printed = approval_show(&fixture::approval_record());
    insta_like(
        &printed,
        &[
            "短 ID",
            "7K2M",
            "动作",
            "shell",
            "rm -rf build",
            "/home/u/project/build",
            "cwd",
            "版本",
            "改动",
            "+新的一行",
            "已有验证结果",
            "原因",
            "范围",
            "本次 Run 范围",
        ],
    );
}

// ---- cron ----

#[test]
fn a_cron_list_shows_the_next_slot_and_how_far_off_it_is() {
    let jobs = vec![
        CronJob {
            id: CronJobId::from_raw("job-1"),
            name: "早报".into(),
            version: 3,
            trigger: Trigger::Cron {
                expr: "0 9 * * *".into(),
                tz: TimeZone::new("Asia/Shanghai"),
            },
            prompt: "把昨天的事说一遍".into(),
            workdir: None,
            status: JobStatus::Active,
            overlap: OverlapPolicy::Skip,
            model: None,
            effort: None,
            skills: vec![],
            max_rounds: None,
            notify: Default::default(),
            next_run_at: Some(NOW + time::Duration::hours(2)),
            last_error: None,
        },
        CronJob {
            id: CronJobId::from_raw("job-2"),
            name: "坏掉的".into(),
            version: 1,
            trigger: Trigger::At { at: NOW },
            prompt: "x".into(),
            workdir: None,
            status: JobStatus::Paused,
            overlap: OverlapPolicy::Allow,
            model: None,
            effort: None,
            skills: vec![],
            max_rounds: None,
            notify: Default::default(),
            next_run_at: None,
            last_error: Some("时区 Mars/Olympus 解析不了".into()),
        },
    ];
    let response = komo_kernel::protocol::http::CronListResponse {
        status: vec![
            // 上一次跑成了，会话号在这一行上——「Cron 的结果去原 Session 查看」（§10）。
            komo_kernel::protocol::http::CronJobStatus {
                job: CronJobId::from_raw("job-1"),
                next_run_at: jobs[0].next_run_at,
                last: Some(komo_kernel::cron::CronFiring {
                    job: CronJobId::from_raw("job-1"),
                    job_version: 3,
                    scheduled_at: NOW - time::Duration::hours(22),
                    prompt: "把昨天的事说一遍".into(),
                    session: Some(SessionId::from_raw("sess-9")),
                    run: Some(RunId::from_raw("run-9")),
                    status: komo_kernel::cron::FiringStatus::Ok,
                    error: None,
                }),
            },
            // 上一次被跳过了，**原因在同一行**：一个天天被跳过的 Job 不该和一个天天
            // 跑成的 Job 长得一样。
            komo_kernel::protocol::http::CronJobStatus {
                job: CronJobId::from_raw("job-2"),
                next_run_at: None,
                last: Some(komo_kernel::cron::CronFiring {
                    job: CronJobId::from_raw("job-2"),
                    job_version: 1,
                    scheduled_at: NOW - time::Duration::days(1),
                    prompt: "x".into(),
                    session: None,
                    run: None,
                    status: komo_kernel::cron::FiringStatus::Skipped,
                    error: Some("上一次触发还没结束".into()),
                }),
            },
        ],
        jobs,
    };
    let printed = cron_list(&response, NOW);
    insta_like(
        &printed,
        &[
            "job-1",
            "启用",
            "跳过",
            "总是",
            "还有 2h0m",
            "早报 · 0 9 * * * @Asia/Shanghai",
            "上次",
            "ok",
            "会话 sess-9",
            "job-2",
            "暂停",
            "并行",
            "skipped",
            "上一次触发还没结束",
            "⚠ 时区 Mars/Olympus 解析不了",
        ],
    );
}

/// `notify` 与触发状态在清单里都看得见——`waiting` 尤其：那是任务在**问**，
/// 而一条没人看见的提问等于这个 Job 从此停在那里（§10）。
#[test]
fn a_waiting_firing_says_it_is_waiting_for_a_person() {
    let jobs = vec![CronJob {
        id: CronJobId::from_raw("job-3"),
        name: "夜间整理".into(),
        version: 1,
        trigger: Trigger::Cron {
            expr: "0 3 * * *".into(),
            tz: TimeZone::new("Europe/Berlin"),
        },
        prompt: "整理".into(),
        workdir: None,
        status: JobStatus::Active,
        overlap: OverlapPolicy::Skip,
        model: None,
        effort: None,
        skills: vec![],
        max_rounds: None,
        notify: komo_kernel::cron::NotifyPolicy::OnError,
        next_run_at: Some(NOW + time::Duration::hours(5)),
        last_error: None,
    }];
    let response = komo_kernel::protocol::http::CronListResponse {
        status: vec![komo_kernel::protocol::http::CronJobStatus {
            job: CronJobId::from_raw("job-3"),
            next_run_at: jobs[0].next_run_at,
            last: Some(komo_kernel::cron::CronFiring {
                job: CronJobId::from_raw("job-3"),
                job_version: 1,
                scheduled_at: NOW - time::Duration::hours(19),
                prompt: "整理".into(),
                session: Some(SessionId::from_raw("sess-3")),
                run: Some(RunId::from_raw("run-3")),
                status: komo_kernel::cron::FiringStatus::Waiting,
                error: None,
            }),
        }],
        jobs,
    };
    insta_like(
        &cron_list(&response, NOW),
        &["仅出错", "waiting（在等人）", "会话 sess-3"],
    );
}

// ---- memory ----

fn memory_item() -> MemoryItem {
    MemoryItem {
        id: MemoryId::from_raw("mem-1"),
        revision: 3,
        content: "喜欢深色主题".into(),
        kind: MemoryKind::Preference,
        scope: MemoryScope::Project {
            project_id: "komo".into(),
        },
        provenance: Provenance::ModelInference,
        confirmation: Confirmation::Unconfirmed,
        state: MemoryState::Candidate,
        evidence: vec![Evidence {
            reference: EvidenceRef::Event {
                session: SessionId::from_raw("sess-1"),
                event: komo_kernel::types::ids::EventId::from_raw("evt-4"),
                seq: Seq(4),
            },
            provenance: Provenance::UserStatement,
            observed_at: NOW,
            extracted_from_run: Some(RunId::from_raw("run-1")),
        }],
        observed_at: NOW - time::Duration::days(3),
        valid_until: None,
        created_at: NOW,
        updated_at: NOW,
        extraction: ExtractionMetadata::new("mem-model", Some(Effort::new("low")), "v1"),
        usage: MemoryUsage {
            count: 5,
            last_used_at: Some(NOW),
        },
        supersedes: None,
    }
}

#[test]
fn a_memory_list_keeps_provenance_and_confirmation_apart() {
    // §9.2：把「模型从用户原话整理」写成「用户确认了模型摘要」是这组类型要挡的事。
    let printed = memory_list(&MemoryListResponse {
        memories: vec![memory_item()],
        degraded: false,
        degraded_reason: None,
    });
    insta_like(
        &printed,
        &["mem-1", "候选", "模型推断", "未确认", "喜欢深色主题"],
    );
}

#[test]
fn a_degraded_retrieval_says_so_out_loud() {
    let printed = memory_list(&MemoryListResponse {
        memories: vec![],
        degraded: true,
        degraded_reason: Some("向量后端不可用".into()),
    });
    assert!(
        printed.contains("⚠ 检索已降级为关键词：向量后端不可用"),
        "{printed}"
    );
}

#[test]
fn a_memory_show_separates_when_it_was_observed_from_when_it_was_stored() {
    let printed = memory_show(&memory_item());
    insta_like(
        &printed,
        &[
            "mem-1",
            "revision 3",
            "偏好 · 项目 komo",
            "模型推断",
            "未确认",
            "观察    2026-09-13",
            "入库 2026-09-16",
            "mem-model",
            "prompt v1",
            "使用    5 次",
            "sess-1#4",
        ],
    );
}

#[test]
fn an_index_status_without_a_vector_model_says_keyword_only() {
    let printed = memory_index(&MemoryIndexStatus {
        space: None,
        generation: None,
        state: IndexState::Unconfigured,
        coverage: 0.0,
        indexed: 0,
        total: 120,
        errors: vec![],
    });
    assert!(printed.contains("未配置向量模型"), "{printed}");
    assert!(printed.contains("0.0%（0 / 120）"), "{printed}");
}

// ---- config check / doctor ----

fn issues() -> Vec<ConfigIssue> {
    vec![
        ConfigIssue {
            key: KeyPath::new("memory.embedding.base_url"),
            severity: IssueSeverity::Error,
            message: "缺少 base_url".into(),
        },
        ConfigIssue {
            key: KeyPath::new("channels.telegram.allow_from"),
            severity: IssueSeverity::Warning,
            message: "已启用但名单为空".into(),
        },
    ]
}

fn config_response(
    issues: Vec<ConfigIssue>,
    loaded_at: OffsetDateTime,
    mtime: OffsetDateTime,
) -> ConfigCheckResponse {
    ConfigCheckResponse {
        issues,
        loaded_at,
        sources: vec![SourceFile {
            path: PathBuf::from("/home/u/.komo/config.toml"),
            mtime,
        }],
    }
}

fn health() -> HealthResponse {
    HealthResponse {
        instance_id: "inst-1".into(),
        version: "0.8.0".into(),
        protocol_version: PROTOCOL_VERSION,
        started_at: NOW,
        data_dir: "/home/u/.komo".into(),
    }
}

#[test]
fn config_check_locates_every_problem_at_a_key() {
    let printed = config_check(&config_response(issues(), NOW, NOW));
    insta_like(
        &printed,
        &[
            "错误  memory.embedding.base_url: 缺少 base_url",
            "警告  channels.telegram.allow_from: 已启用但名单为空",
            "1 个错误——这份配置不会被装上",
            "当前配置装载于 2026-09-16 08:00:00",
        ],
    );
}

#[test]
fn config_check_with_only_warnings_does_not_claim_the_config_is_rejected() {
    let printed = config_check(&config_response(vec![issues()[1].clone()], NOW, NOW));
    assert!(printed.contains("1 个警告，没有错误"), "{printed}");
    assert!(!printed.contains("不会被装上"), "{printed}");
}

#[test]
fn config_check_on_a_clean_config_says_so() {
    let printed = config_check(&config_response(vec![], NOW, NOW));
    assert!(printed.starts_with("配置校验通过"), "{printed}");
}

#[test]
fn config_check_calls_out_a_file_edited_after_the_config_was_loaded() {
    let printed = config_check(&config_response(
        vec![],
        NOW,
        NOW + time::Duration::minutes(5),
    ));
    assert!(printed.contains("⚠ 文件改了但没装上"), "{printed}");
}

#[test]
fn a_reload_names_the_keys_that_changed_without_their_values() {
    let printed = config_reload(&ConfigReloadResponse {
        changed: vec![
            KeyPath::new("model.model"),
            KeyPath::new("start_only.listen"),
        ],
        start_only: vec![KeyPath::new("start_only.listen")],
        warnings: vec![issues()[1].clone()],
    });
    insta_like(
        &printed,
        &[
            "2 个键变化",
            "model.model",
            // §3 第 4 步：不静默忽略，也不假装已生效。
            "只在启动时生效",
            "komo gateway restart",
            "警告  channels.telegram.allow_from",
        ],
    );
    // 只出键名，不出值。
    assert!(!printed.contains("127.0.0.1"), "{printed}");
}

#[test]
fn a_reload_that_changed_nothing_says_so() {
    let printed = config_reload(&ConfigReloadResponse::default());
    assert_eq!(printed, "配置已重新装载，没有键变化");
}

#[test]
fn a_rejected_reload_prints_the_keys_and_says_the_old_config_stands() {
    let error = crate::error::ClientError::from_body(
        400,
        r#"{"error":{"code":"config_invalid","message":"缺少 base_url","keys":["memory.embedding.base_url"]}}"#,
    );
    let printed = config_error(&error);
    insta_like(
        &printed,
        &[
            "缺少 base_url",
            "memory.embedding.base_url",
            "运行中的 Gateway 保留原配置",
        ],
    );
}

#[test]
fn a_model_list_says_which_efforts_each_model_takes() {
    let printed = model_list(&ModelsResponse {
        models: vec![
            ModelMenuEntry {
                id: "chat-a".into(),
                provider: "openai_compatible".into(),
                efforts: vec![Effort::new("low"), Effort::new("high")],
                default: true,
                ..ModelMenuEntry::default()
            },
            ModelMenuEntry {
                id: "chat-b".into(),
                provider: "anthropic".into(),
                efforts: vec![],
                default: false,
                ..ModelMenuEntry::default()
            },
        ],
    });
    insta_like(
        &printed,
        &[
            "chat-a",
            "low · high",
            "← 当前",
            "chat-b",
            // 空表 = 一档都不支持，不是「还不知道」。
            "（不接受显式 effort）",
        ],
    );
}

#[test]
fn doctor_calls_out_a_file_that_was_edited_after_the_config_was_loaded() {
    // §3：两者不一致就是「文件改了但没装上」。
    let printed = doctor(
        &health(),
        Some(&config_response(
            issues(),
            NOW,
            NOW + time::Duration::minutes(5),
        )),
    );
    insta_like(
        &printed,
        &[
            "实例      inst-1",
            "配置装载  2026-09-16 08:00:00",
            "mtime 2026-09-16 08:05:00",
            "⚠ 文件改了但没装上：/home/u/.komo/config.toml",
            "komo config reload",
            // 上一次校验错误一并印出来。
            "上一次校验",
            "memory.embedding.base_url",
        ],
    );
}

#[test]
fn doctor_is_quiet_about_staleness_when_the_file_has_not_moved() {
    let printed = doctor(
        &health(),
        Some(&config_response(
            vec![],
            NOW,
            NOW - time::Duration::minutes(1),
        )),
    );
    assert!(!printed.contains("没装上"), "{printed}");
    assert!(!printed.contains("上一次校验"), "{printed}");
}

#[test]
fn doctor_without_a_snapshot_says_it_could_not_get_one() {
    let printed = doctor(&health(), None);
    assert!(printed.contains("取不到当前快照"), "{printed}");
}

/// 逐条断言输出里该出现的片段——比整段字面量的快照耐改，坏掉时也说得清哪一条没了。
fn insta_like(printed: &str, fragments: &[&str]) {
    for fragment in fragments {
        assert!(
            printed.contains(fragment),
            "少了「{fragment}」：\n{printed}"
        );
    }
}
