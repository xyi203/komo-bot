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
use komo_kernel::cron::JobStatus;
use komo_kernel::protocol::http::{
    ApprovalDecisionRequest, ApprovalListQuery, CancelRunRequest, CreateCronRequest,
    MemoryListQuery, MemoryRevisionRequest, RebuildIndexRequest, UpdateCronRequest,
};
use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::ids::{ApprovalId, CronJobId, MemoryId, RunId, ShortId};

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
        &response.jobs,
        time::OffsetDateTime::now_utc(),
    ))
}

pub async fn cron_add(
    client: &KomoClient,
    name: &str,
    schedule: &str,
    timezone: &str,
    prompt: &str,
    workdir: Option<&str>,
) -> Outcome {
    let job = client
        .cron_create(&CreateCronRequest {
            name: name.to_string(),
            schedule: schedule.to_string(),
            timezone: timezone.to_string(),
            prompt: prompt.to_string(),
            workdir: workdir.map(str::to_string),
            model: None,
            effort: None,
            skills: Vec::new(),
            overlap: Default::default(),
            max_rounds: None,
            request_key: Some(RequestKeys::fresh(time::OffsetDateTime::now_utc())),
        })
        .await
        .map_err(failed)?;
    Ok(format!("已创建 {}（{}）", job.name, job.id))
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
    Ok(format!(
        "已手动触发：run {}（会话 {}）",
        response.run, response.session
    ))
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
    Ok(format!("{} 现在是 {:?}", job.name, job.status))
}

pub async fn cron_remove(client: &KomoClient, job: &str) -> Outcome {
    let response = client
        .cron_delete(&CronJobId::from_raw(job))
        .await
        .map_err(failed)?;
    Ok(format!(
        "{}：{}（保留了 {} 条触发历史）",
        response.job,
        if response.removed {
            "已移除"
        } else {
            "本来就没有"
        },
        response.firings_kept
    ))
}

// ---------------------------------------------------------------- Memory

pub async fn memory_list(client: &KomoClient, query: Option<&str>) -> Outcome {
    let response = client
        .memories(&MemoryListQuery {
            query: query.map(str::to_string),
            ..Default::default()
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
