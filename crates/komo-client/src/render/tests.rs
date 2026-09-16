//! 渲染函数的快照测试：协议类型 → 字符串，全是纯函数。

use super::*;
use crate::tui::test_support as fixture;
use komo_kernel::cron::{TimeZone, Trigger};
use komo_kernel::protocol::PROTOCOL_VERSION;
use komo_kernel::protocol::config::{
    ChannelConfig, ChannelsConfig, KeyPath, MemoryConfig, PathsConfig, RetrievalConfig, SourceFile,
    StartOnly,
};
use komo_kernel::protocol::http::{
    ApprovalDecisionRecord, RunSummary, SessionSummary, ToolCallSummary,
};
use komo_kernel::types::chat::{ApprovalScope, PeerId};
use komo_kernel::types::digest::ContentHash;
use komo_kernel::types::ids::{CronJobId, MemoryId, RunId, Seq, SessionId};
use komo_kernel::types::memory::{Evidence, EvidenceRef, ExtractionMetadata, MemoryUsage};
use komo_kernel::types::model::{Effort, ModelConfig};
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
            next_run_at: None,
            last_error: Some("时区 Mars/Olympus 解析不了".into()),
        },
    ];
    let printed = cron_list(&jobs, NOW);
    insta_like(
        &printed,
        &[
            "job-1",
            "启用",
            "跳过",
            "还有 2h0m",
            "早报 · 0 9 * * * @Asia/Shanghai",
            "job-2",
            "暂停",
            "并行",
            "⚠ 时区 Mars/Olympus 解析不了",
        ],
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

#[test]
fn config_check_locates_every_problem_at_a_key() {
    let printed = config_check(&issues());
    insta_like(
        &printed,
        &[
            "错误  memory.embedding.base_url: 缺少 base_url",
            "警告  channels.telegram.allow_from: 已启用但名单为空",
            "1 个错误——这份配置不会被装上",
        ],
    );
}

#[test]
fn config_check_with_only_warnings_does_not_claim_the_config_is_rejected() {
    let only_warning = vec![issues()[1].clone()];
    let printed = config_check(&only_warning);
    assert!(printed.contains("1 个警告，没有错误"), "{printed}");
    assert!(!printed.contains("不会被装上"), "{printed}");
}

#[test]
fn config_check_on_a_clean_config_says_so() {
    assert_eq!(config_check(&[]), "配置校验通过");
}

fn model(name: &str) -> ModelConfig {
    ModelConfig {
        provider: "openai_compatible".into(),
        base_url: "https://llm.example.com/v1".into(),
        model: name.into(),
        api_key_env: "KOMO_LLM_API_KEY".into(),
        effort: None,
        timeout_secs: 120,
    }
}

fn snapshot(loaded_at: OffsetDateTime, mtime: OffsetDateTime) -> ConfigSnapshot {
    ConfigSnapshot {
        start_only: StartOnly {
            data_dir: PathBuf::from("/home/u/.komo"),
            listen: "127.0.0.1:7777".into(),
            db_path: PathBuf::from("/home/u/.komo/state.db"),
            python_env_root: PathBuf::from("/home/u/.komo/python-envs"),
        },
        model: model("chat-a"),
        memory: MemoryConfig {
            enabled: true,
            model: model("memory-a"),
            embedding: None,
            retrieval: RetrievalConfig::default(),
        },
        channels: ChannelsConfig {
            telegram: ChannelConfig {
                enabled: true,
                allow_from: vec![],
                home_chat: None,
                groups: vec![],
            },
            ..ChannelsConfig::default()
        },
        policy: komo_kernel::policy::RuleTable::default(),
        paths: PathsConfig {
            sessions_dir: "/home/u/.komo/sessions".into(),
            toolbox_dir: "/home/u/.komo/toolbox".into(),
            skill_dirs: vec![],
            runtime_dir: "/home/u/.komo/runtime".into(),
            logs_dir: "/home/u/.komo/logs".into(),
            workspaces_dir: "/home/u/.komo/workspaces".into(),
        },
        credentials: Default::default(),
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
fn doctor_calls_out_a_file_that_was_edited_after_the_config_was_loaded() {
    // §3：两者不一致就是「文件改了但没装上」。
    let printed = doctor(
        &health(),
        Some(&snapshot(NOW, NOW + time::Duration::minutes(5))),
        &issues(),
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
        Some(&snapshot(NOW, NOW - time::Duration::minutes(1))),
        &[],
    );
    assert!(!printed.contains("没装上"), "{printed}");
    assert!(!printed.contains("上一次校验"), "{printed}");
}

#[test]
fn doctor_warns_about_a_channel_nobody_can_speak_through() {
    let printed = doctor(&health(), Some(&snapshot(NOW, NOW)), &[]);
    assert!(
        printed.contains("channels.telegram 已启用但 allow_from 为空"),
        "{printed}"
    );
    assert!(printed.contains("没有任何渠道配了 home_chat"), "{printed}");
}

#[test]
fn doctor_without_a_snapshot_says_it_could_not_get_one() {
    let printed = doctor(&health(), None, &[]);
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
