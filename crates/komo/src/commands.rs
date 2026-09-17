//! 每条操作命令的实现：拿一个客户端、调一个接口、用 `render` 印出来。
//!
//! 「CLI 通过 HTTP 发命令」（§13.1）——所以这里没有任何业务判断，除了三条**不经
//! Gateway** 的（`channel list|probe`、`skills *`、`gateway start|stop|status`，§3）。

use std::path::Path;

use komo_client::KomoClient;
use komo_client::api::RequestKeys;
use komo_client::render;
use komo_gateway::channels;
use komo_gateway::config::{LoadOptions, load_config};
use komo_gateway::skills::{OfferContext, SkillRegistry};
use komo_kernel::cron::{JobStatus, NotifyPolicy, OverlapPolicy};
use komo_kernel::protocol::http::{
    ApprovalDecisionRequest, ApprovalListQuery, CancelRunRequest, CreateCronRequest,
    MemoryListQuery, MemoryRevisionRequest, RebuildIndexRequest, UpdateCronRequest,
};
use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::ids::{ApprovalId, CronJobId, MemoryId, RunId, ShortId};
use komo_kernel::types::memory::{MemoryScope, MemoryState, RetrievalMode};
use komo_kernel::types::model::Effort;

pub type Outcome = Result<String, String>;

fn failed(error: impl std::fmt::Display) -> String {
    error.to_string()
}

// ---------------------------------------------------------------- 会话与运行

pub async fn session_list(client: &KomoClient) -> Outcome {
    let response = client.list_sessions().await.map_err(failed)?;
    Ok(render::session_list(&response))
}

pub async fn run_inspect(client: &KomoClient, run: &str) -> Outcome {
    let run = RunId::from_raw(run);
    let detail = client.run(&run).await.map_err(failed)?;
    // 「这一步是谁放行的」——那条审批早就不在待处理集合里了（§7.4 的审计面）。
    let approvals = client
        .approvals(&ApprovalListQuery {
            run: Some(run.clone()),
            session: None,
            include_decided: true,
        })
        .await
        .map(|response| response.approvals)
        .unwrap_or_default();
    Ok(render::run_inspect(&detail, &approvals))
}

pub async fn run_cancel(client: &KomoClient, run: &str) -> Outcome {
    let run = RunId::from_raw(run);
    let response = client
        .cancel_run(
            &run,
            &CancelRunRequest {
                request_key: Some(RequestKeys::fresh(time::OffsetDateTime::now_utc())),
            },
        )
        .await
        .map_err(failed)?;
    Ok(format!("{} 现在是 {:?}", response.run, response.status))
}

// ---------------------------------------------------------------- 审批

pub async fn approval_list(client: &KomoClient) -> Outcome {
    let response = client
        .approvals(&ApprovalListQuery::default())
        .await
        .map_err(failed)?;
    Ok(render::approval_list(&response))
}

pub async fn approval_show(client: &KomoClient, id: &str) -> Outcome {
    let record = resolve_approval(client, id).await?;
    Ok(render::approval_show(&record))
}

pub async fn approval_decide(client: &KomoClient, id: &str, approved: bool) -> Outcome {
    let record = resolve_approval(client, id).await?;
    let response = client
        .decide_approval(
            &record.approval,
            &ApprovalDecisionRequest {
                approved,
                scope: ApprovalScope::Once,
                request_key: Some(RequestKeys::named("cli-decision", record.approval.as_str())),
            },
        )
        .await
        .map_err(failed)?;
    Ok(format!(
        "{} {}{}",
        response.short_id,
        if response.decision.approved {
            "已批准"
        } else {
            "已拒绝"
        },
        if response.already_decided {
            "（这次什么都没改，返回的是之前那个决定）"
        } else {
            ""
        }
    ))
}

/// 命令行上给的可能是 4 位短 ID，也可能是完整 ID。
async fn resolve_approval(
    client: &KomoClient,
    id: &str,
) -> Result<komo_kernel::protocol::http::ApprovalRecord, String> {
    if let Some(short) = ShortId::parse(id) {
        let pending = client
            .approvals(&ApprovalListQuery::default())
            .await
            .map_err(failed)?;
        if let Some(found) = pending
            .approvals
            .into_iter()
            .find(|record| record.short_id == short)
        {
            return Ok(found);
        }
    }
    client
        .approval(&ApprovalId::from_raw(id))
        .await
        .map_err(failed)
}

// ---------------------------------------------------------------- Cron

pub async fn cron_list(client: &KomoClient) -> Outcome {
    let response = client.cron_list().await.map_err(failed)?;
    Ok(render::cron_list(
        &response,
        time::OffsetDateTime::now_utc(),
    ))
}

/// `komo cron add` 的全部 flag（§10 的 Job 字段）。
///
/// 一个结构体而不是十一个参数：这些字段会随 §10 继续长，而一串同类型的
/// `Option<String>` 位置参数是一个等着发生的错配。
#[derive(Debug, Clone, Default)]
pub struct CronAdd {
    pub name: String,
    pub schedule: String,
    pub timezone: String,
    pub prompt: String,
    pub workdir: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub skills: Vec<String>,
    pub max_rounds: Option<u32>,
    pub overlap: String,
    pub notify: String,
}

pub async fn cron_add(client: &KomoClient, add: CronAdd) -> Outcome {
    // 重叠策略与 notify **在发请求之前**校验：它们是打字打出来的，而"不认识就当默认"
    // 会让一个写错的 `--notify nerver` 变成"每次都投"。
    let overlap = parse_overlap(&add.overlap)?;
    let notify = NotifyPolicy::parse(&add.notify).ok_or_else(|| {
        format!(
            "--notify 只接受 always / on_error / never，收到：{}",
            add.notify
        )
    })?;
    let job = client
        .cron_create(&CreateCronRequest {
            name: add.name,
            schedule: add.schedule,
            timezone: add.timezone,
            prompt: add.prompt,
            workdir: add.workdir,
            model: add.model,
            effort: add.effort.as_deref().map(Effort::new),
            skills: add.skills,
            overlap,
            max_rounds: add.max_rounds,
            notify: Some(notify),
            model_config: None,
            request_key: Some(RequestKeys::fresh(time::OffsetDateTime::now_utc())),
        })
        .await
        .map_err(failed)?;
    Ok(added_line(&job))
}

/// `komo cron add` 的回执。
///
/// 一个 Job 创建之后操作者要能立刻答出两件事：**它是哪一个**（ID，后面每条命令都要
/// 它），以及**下一次什么时候**——一个写错了时区的 Job 与一个写对了的长得一样，
/// 只有下一次的时刻不一样。
pub fn added_line(job: &komo_kernel::cron::CronJob) -> String {
    let next = match job.next_run_at {
        Some(next) => render::stamp(next),
        None => "—".to_string(),
    };
    format!("已创建 {}（{}）；下一次 {next}", job.name, job.id)
}

fn parse_overlap(raw: &str) -> Result<OverlapPolicy, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "skip" => Ok(OverlapPolicy::Skip),
        "allow" => Ok(OverlapPolicy::Allow),
        other => Err(format!("--overlap 只接受 skip / allow，收到：{other}")),
    }
}

pub async fn cron_run(client: &KomoClient, job: &str) -> Outcome {
    let response = client
        .cron_run(
            &CronJobId::from_raw(job),
            &komo_kernel::protocol::http::ManualCronRunRequest {
                request_key: Some(RequestKeys::fresh(time::OffsetDateTime::now_utc())),
            },
        )
        .await
        .map_err(failed)?;
    Ok(manual_run_line(&response))
}

/// `komo cron run` 的回执。**会话号要印出来**：「Cron 的结果去原 Session 查看」
/// （§10），而手动触发不写触发记录，清单上事后找不到它。
pub fn manual_run_line(response: &komo_kernel::protocol::http::ManualCronRunResponse) -> String {
    format!(
        "已手动触发：run {}（会话 {}）；这一次不算定时触发，不推进槽位",
        response.run, response.session
    )
}

pub async fn cron_status(client: &KomoClient, job: &str, status: JobStatus) -> Outcome {
    let job = client
        .cron_update(
            &CronJobId::from_raw(job),
            &UpdateCronRequest {
                status: Some(status),
                ..Default::default()
            },
        )
        .await
        .map_err(failed)?;
    Ok(status_line(&job))
}

/// `komo cron pause` / `resume` 的回执。
///
/// 印出版本号，因为这条命令**不递增它**（§7.2 / §10：暂停不是定义变更，绑定这个 Job
/// 的授权不该因为暂停一次就失效）——看得见才说得清。
pub fn status_line(job: &komo_kernel::cron::CronJob) -> String {
    let state = match job.status {
        JobStatus::Active => "启用",
        JobStatus::Paused => "暂停",
        JobStatus::Done => "已完成",
    };
    let next = match job.next_run_at {
        Some(next) => render::stamp(next),
        None => "—".to_string(),
    };
    format!(
        "{} 现在是{state}（v{}）；下一次 {next}",
        job.name, job.version
    )
}

pub async fn cron_remove(client: &KomoClient, job: &str) -> Outcome {
    let response = client
        .cron_delete(&CronJobId::from_raw(job))
        .await
        .map_err(failed)?;
    Ok(removed_line(&response))
}

/// `komo cron remove` 的回执。「移除后续调度，**已有执行历史保留**」（§13.1）——
/// 保留了多少条要说出来，否则"移除"听起来像把历史也删了。
pub fn removed_line(response: &komo_kernel::protocol::http::CronDeleteResponse) -> String {
    format!(
        "{}：{}（保留了 {} 条触发历史）",
        response.job,
        if response.removed {
            "已移除后续调度"
        } else {
            "本来就没有"
        },
        response.firings_kept
    )
}

// ---------------------------------------------------------------- Memory

/// `komo memory list` / `search` 的筛选条件。
///
/// **模式、作用域、状态都是显式的**：§9.4 说 hybrid / keyword / vector「供明确选择与
/// 诊断」，而 §9.6 说 contested 暂停自动召回但用户要查得到——两句话都要求这几个开关在
/// 命令行上存在，不能只有一个默认。
#[derive(Debug, Clone, Default)]
pub struct MemoryFilter {
    pub query: Option<String>,
    pub mode: Option<String>,
    pub scope: Option<String>,
    pub state: Option<String>,
    pub limit: Option<u32>,
}

pub async fn memory_list(client: &KomoClient, filter: MemoryFilter) -> Outcome {
    // 参数在**发出去之前**就要判——打错一个模式名，答案该是"支持这三种"而不是一次
    // 沉默的默认值。
    let mode = match filter.mode.as_deref() {
        None => None,
        Some("hybrid") => Some(RetrievalMode::Hybrid),
        Some("keyword") => Some(RetrievalMode::Keyword),
        Some("vector") => Some(RetrievalMode::Vector),
        Some(other) => {
            return Err(format!(
                "不认识的检索模式 `{other}`：支持 hybrid / keyword / vector"
            ));
        }
    };
    let scope = match filter.scope.as_deref() {
        None => None,
        Some(raw) => Some(
            raw.parse::<MemoryScope>()
                .map_err(|error| error.to_string())?,
        ),
    };
    let state = match filter.state.as_deref() {
        None => None,
        Some("candidate") => Some(MemoryState::Candidate),
        Some("active") => Some(MemoryState::Active),
        Some("contested") => Some(MemoryState::Contested),
        Some("superseded") => Some(MemoryState::Superseded),
        Some("forgotten") => Some(MemoryState::Forgotten),
        Some(other) => {
            return Err(format!(
                "不认识的状态 `{other}`：支持 candidate / active / contested / superseded / forgotten"
            ));
        }
    };

    let response = client
        .memories(&MemoryListQuery {
            query: filter.query,
            mode,
            scope,
            state,
            limit: filter.limit,
        })
        .await
        .map_err(failed)?;
    Ok(render::memory_list(&response))
}

pub async fn memory_show(client: &KomoClient, id: &str) -> Outcome {
    let detail = client
        .memory(&MemoryId::from_raw(id))
        .await
        .map_err(failed)?;
    Ok(render::memory_show(&detail.memory))
}

pub async fn memory_confirm(client: &KomoClient, id: &str, revision: u32) -> Outcome {
    let detail = client
        .memory_confirm(
            &MemoryId::from_raw(id),
            &MemoryRevisionRequest {
                expected_revision: revision,
                request_key: Some(RequestKeys::named("cli-confirm", id)),
            },
        )
        .await
        .map_err(failed)?;
    Ok(render::memory_show(&detail.memory))
}

pub async fn memory_forget(client: &KomoClient, id: &str, revision: u32) -> Outcome {
    let detail = client
        .memory_forget(
            &MemoryId::from_raw(id),
            &MemoryRevisionRequest {
                expected_revision: revision,
                request_key: Some(RequestKeys::named("cli-forget", id)),
            },
        )
        .await
        .map_err(failed)?;
    Ok(render::memory_show(&detail.memory))
}

pub async fn memory_index(client: &KomoClient) -> Outcome {
    let status = client.memory_index().await.map_err(failed)?;
    Ok(render::memory_index(&status))
}

pub async fn memory_rebuild(client: &KomoClient) -> Outcome {
    let response = client
        .rebuild_memory_index(&RebuildIndexRequest {
            request_key: Some(RequestKeys::fresh(time::OffsetDateTime::now_utc())),
        })
        .await
        .map_err(failed)?;
    Ok(format!(
        "代次 {}：{}",
        response.generation,
        if response.accepted {
            "已提交重建"
        } else {
            "没有新提交（命中了正在跑的同一个代次，或者重建还没接上）"
        }
    ))
}

// ---------------------------------------------------------------- 配置与诊断

pub async fn config_check(client: &KomoClient) -> Outcome {
    let response = client.config_check().await.map_err(failed)?;
    Ok(render::config_check(&response))
}

pub async fn config_reload(client: &KomoClient) -> Outcome {
    match client.config_reload().await {
        Ok(response) => Ok(render::config_reload(&response)),
        // 「校验不过走错误体，旧快照原样保留」——`keys` 要印出来。
        Err(error) => Err(render::config_error(&error)),
    }
}

pub async fn doctor(home: &Path) -> Outcome {
    let client = match crate::connect::connect(home).await {
        Ok(client) => client,
        Err(error) => return Err(format!("Gateway 没在跑：{error}")),
    };
    let health = client.health().await.map_err(failed)?;
    let config = client.config_check().await.ok();
    Ok(render::doctor(&health, config.as_ref()))
}

// ---------------------------------------------------------------- 不经 Gateway

/// `komo channel list`：渠道清单，只读配置（§3、§11.5）。
pub fn channel_list(home: &Path) -> Outcome {
    let loaded = load_config(&LoadOptions::at(home)).map_err(failed)?;
    let mut lines = Vec::new();
    for platform in [
        komo_kernel::types::chat::ChannelPlatform::Feishu,
        komo_kernel::types::chat::ChannelPlatform::Telegram,
        komo_kernel::types::chat::ChannelPlatform::Wechat,
    ] {
        let Some(config) = loaded.snapshot.channels.get(platform) else {
            continue;
        };
        let mut line = format!(
            "{platform:<9} {}",
            if config.enabled {
                "enabled "
            } else {
                "disabled"
            }
        );
        line.push_str(&format!("  allow_from {}", config.allow_from.len()));
        if let Some(home_chat) = &config.home_chat {
            line.push_str(&format!("  home_chat {home_chat}"));
        }
        if !config.groups.is_empty() {
            line.push_str(&format!("  groups {}", config.groups.len()));
        }
        // 「某渠道 enabled 但 allow_from 为空」是一条警告（§11.2）。
        if config.is_outbound_only() {
            line.push_str("  ⚠ 只出不进（allow_from 为空，没人能通过它下指令）");
        }
        lines.push(line);
    }
    Ok(lines.join("\n"))
}

/// `komo channel probe`：连通性核对，**不经 Gateway**（§3）。
pub async fn channel_probe(home: &Path) -> Outcome {
    let loaded = load_config(&LoadOptions::at(home)).map_err(failed)?;
    let mut lines = Vec::new();
    for factory in channels::factories() {
        let platform = factory.platform();
        let enabled = loaded
            .snapshot
            .channels
            .get(platform)
            .is_some_and(|config| config.enabled);
        if !enabled {
            lines.push(format!("{platform:<9} 未启用"));
            continue;
        }
        match factory.probe(&loaded.snapshot, &loaded.secrets).await {
            Ok(message) => lines.push(format!("{platform:<9} ✓ {message}")),
            Err(error) => lines.push(format!("{platform:<9} ✗ {error}")),
        }
    }
    Ok(lines.join("\n"))
}

/// `komo skills ...`：只读文件系统，不经 Gateway（§5.6）。
pub fn skills(home: &Path, action: SkillsAction<'_>) -> Outcome {
    let loaded = load_config(&LoadOptions::at(home)).map_err(failed)?;
    let registry = SkillRegistry::new(loaded.snapshot.paths.skill_dirs.clone());
    match action {
        SkillsAction::List => {
            let skills = registry.list();
            if skills.is_empty() {
                return Ok(
                    "没有 skills（放一个 SKILL.md 到 ~/.komo/skills/<name>/ 就有了）".into(),
                );
            }
            Ok(skills
                .iter()
                .map(|skill| {
                    format!(
                        "{}{}  {}",
                        skill.name,
                        if registry.is_disabled(&skill.name) {
                            "（已停用）"
                        } else {
                            ""
                        },
                        skill.description
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        SkillsAction::Inspect(name) => registry
            .inspect(name)
            .map(|document| document.body)
            .ok_or_else(|| format!("没有叫 {name} 的 skill")),
        SkillsAction::Enable(name) => registry
            .enable(name)
            .map(|()| format!("{name} 已启用"))
            .map_err(failed),
        SkillsAction::Disable(name) => registry
            .disable(name)
            .map(|()| format!("{name} 已停用（只是从目录行里隐藏，文件没删）"))
            .map_err(failed),
    }
}

pub enum SkillsAction<'a> {
    List,
    Inspect(&'a str),
    Enable(&'a str),
    Disable(&'a str),
}

/// 让编译器盯着这个没用到的导入，免得 `OfferContext` 哪天要用时找不到。
#[allow(dead_code)]
fn offer_context() -> OfferContext {
    OfferContext::here(["read", "write", "edit", "shell", "python"])
}

#[cfg(test)]
mod cron_render_tests {
    //! `komo cron` 各子命令的回执。**纯函数**，所以测得起来：一句话里该有哪些东西是
    //! 一个能出错的决定（少印一个 ID、少印一个版本号），而它出错的时候没人会崩。

    use super::*;
    use komo_kernel::cron::{CronJob, JobStatus, OverlapPolicy, TimeZone, Trigger};
    use komo_kernel::types::ids::{RunId, SessionId};
    use time::macros::datetime;

    const NOW: time::OffsetDateTime = datetime!(2026-09-16 01:00:00 UTC);

    fn job() -> CronJob {
        CronJob {
            id: CronJobId::from_raw("job-1"),
            name: "早报".into(),
            version: 3,
            trigger: Trigger::Cron {
                expr: "0 9 * * *".into(),
                tz: TimeZone::new("Asia/Shanghai"),
            },
            prompt: "整理".into(),
            workdir: None,
            status: JobStatus::Active,
            overlap: OverlapPolicy::Skip,
            model: None,
            effort: None,
            skills: vec![],
            max_rounds: None,
            notify: Default::default(),
            next_run_at: Some(NOW),
            last_error: None,
        }
    }

    #[test]
    fn add_says_which_job_it_made_and_when_it_will_next_run() {
        let line = added_line(&job());
        assert!(line.contains("早报"), "{line}");
        assert!(line.contains("job-1"), "后面每条命令都要这个 ID：{line}");
        assert!(line.contains("2026-09-16 01:00:00"), "{line}");
    }

    /// 一个算不出槽位的 Job（区名解析不了一类）不该印成"下一次 1970"。
    #[test]
    fn add_says_dash_when_there_is_no_next_slot() {
        let mut job = job();
        job.next_run_at = None;
        assert!(added_line(&job).contains("—"), "{}", added_line(&job));
    }

    /// `pause` / `resume` **不递增版本**（§7.2 / §10）——印出来才说得清。
    #[test]
    fn pause_prints_the_version_it_did_not_bump() {
        let mut job = job();
        job.status = JobStatus::Paused;
        let line = status_line(&job);
        assert!(line.contains("暂停"), "{line}");
        assert!(line.contains("v3"), "版本没动，而这句话要说得出：{line}");

        let mut done = job.clone();
        done.status = JobStatus::Done;
        done.next_run_at = None;
        let line = status_line(&done);
        assert!(line.contains("已完成"), "{line}");
        assert!(line.contains("—"), "{line}");
    }

    /// 手动触发要印会话号——「Cron 的结果去原 Session 查看」（§10），而它不写触发
    /// 记录，事后在清单上找不到。
    #[test]
    fn a_manual_run_prints_the_session_to_look_at() {
        let line = manual_run_line(&komo_kernel::protocol::http::ManualCronRunResponse {
            job: CronJobId::from_raw("job-1"),
            session: SessionId::from_raw("sess-9"),
            run: RunId::from_raw("run-9"),
            request_key: RequestKeys::fresh(NOW),
        });
        assert!(line.contains("sess-9"), "{line}");
        assert!(line.contains("run-9"), "{line}");
        assert!(line.contains("不推进槽位"), "{line}");
    }

    /// 「移除后续调度，**已有执行历史保留**」（§13.1）：保留了多少条要说出来。
    #[test]
    fn remove_says_how_much_history_it_kept() {
        let line = removed_line(&komo_kernel::protocol::http::CronDeleteResponse {
            job: CronJobId::from_raw("job-1"),
            removed: true,
            firings_kept: 7,
        });
        assert!(line.contains("已移除后续调度"), "{line}");
        assert!(line.contains("7"), "{line}");

        let line = removed_line(&komo_kernel::protocol::http::CronDeleteResponse {
            job: CronJobId::from_raw("job-2"),
            removed: false,
            firings_kept: 0,
        });
        assert!(line.contains("本来就没有"), "{line}");
    }

    /// `--overlap` / `--notify` 写错**当场拒绝**，并说出接受哪几个词——不静默当默认。
    #[test]
    fn a_misspelled_flag_is_refused_with_the_accepted_spellings() {
        let err = parse_overlap("skipp").unwrap_err();
        assert!(err.contains("skip") && err.contains("allow"), "{err}");
        assert_eq!(parse_overlap(" Skip ").unwrap(), OverlapPolicy::Skip);
        assert_eq!(parse_overlap("ALLOW").unwrap(), OverlapPolicy::Allow);

        assert!(NotifyPolicy::parse("nerver").is_none());
        assert_eq!(NotifyPolicy::parse("never"), Some(NotifyPolicy::Never));
    }
}
