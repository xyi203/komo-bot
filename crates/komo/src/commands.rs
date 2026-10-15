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
use komo_gateway::skills::{OfferContext, SkillRegistry, one_line};
use komo_kernel::cron::{JobStatus, NotifyPolicy, OverlapPolicy};
use komo_kernel::protocol::http::{
    CancelRunRequest, CreateCronRequest, InterventionAnswerRequest, InterventionListQuery,
    InterventionVerdict, MemoryListQuery, MemoryRevisionRequest, MemorySweepRequest,
    RebuildIndexRequest, SessionListQuery, UpdateCronRequest,
};
use komo_kernel::types::ids::{CronJobId, MemoryId, RunId, SessionId};
use komo_kernel::types::memory::{MemoryScope, MemoryState, RetrievalMode};
use komo_kernel::types::model::Effort;

pub type Outcome = Result<String, String>;

fn failed(error: impl std::fmt::Display) -> String {
    error.to_string()
}

// ---------------------------------------------------------------- 会话与运行

pub async fn session_list(client: &KomoClient, all: bool) -> Outcome {
    let response = client
        .list_sessions(&SessionListQuery { all })
        .await
        .map_err(failed)?;
    Ok(render::session_list(
        &response,
        time::OffsetDateTime::now_utc(),
    ))
}

/// `komo session delete`：**逻辑删除**（§8.10）。
///
/// 它不碰内容，也不"等一会儿再说"：数据库那一行进 `closing`（`--now` 直接进
/// `deleted`），未完成的 Run 照 §8.4 走完或停在等待。要真的删内容得 `purge`。
pub async fn session_delete(client: &KomoClient, session: &str, now: bool) -> Outcome {
    let session = SessionId::from_raw(session);
    let response = client.delete_session(&session, now).await.map_err(failed)?;
    let mut line = format!(
        "{} 现在是 {}（{}）",
        response.session,
        response.state.as_str(),
        render::stamp(response.changed_at)
    );
    if !response.cancelled.is_empty() {
        line.push_str(&format!(
            "；已取消 {} 条未完成的 Run：{}",
            response.cancelled.len(),
            response
                .cancelled
                .iter()
                .map(RunId::as_str)
                .collect::<Vec<_>>()
                .join("、")
        ));
    }
    line.push_str("。内容一个字节都没动——回收要显式 `komo session purge`。");
    Ok(line)
}

/// `komo session purge`：**回收内容**（§8.10）。引用没处置完时服务端 409 并列出
/// 要先处理什么——这里把那句话原样交给操作者，不假装成功。
pub async fn session_purge(client: &KomoClient, session: &str) -> Outcome {
    let session = SessionId::from_raw(session);
    match client.purge_session(&session).await {
        Ok(response) => Ok(format!(
            "{} 已回收（{}）；删掉 {} 字节，墓碑行还在",
            response.session,
            response.state.as_str(),
            response.removed_bytes
        )),
        Err(error) => Err(failed(error)),
    }
}

pub async fn run_inspect(client: &KomoClient, run: &str) -> Outcome {
    let run = RunId::from_raw(run);
    let detail = client.run(&run).await.map_err(failed)?;
    Ok(render::run_inspect(
        &detail,
        time::OffsetDateTime::now_utc(),
    ))
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
    Ok(format!(
        "{} 现在是 {}",
        response.run,
        response.state.as_str()
    ))
}

// ---------------------------------------------------------------- Intervention（§7.5）

/// `komo intervention list`：**两类一张表**（结果不明 / 阻塞）。
///
/// 它取代了 `komo approval list`——那个清单空着而会话卡住，正是这套东西要消灭的现象。
pub async fn intervention_list(client: &KomoClient, session: Option<&str>) -> Outcome {
    let response = client
        .interventions(&InterventionListQuery {
            session: session.map(SessionId::from_raw),
            ..Default::default()
        })
        .await
        .map_err(failed)?;
    Ok(render::interventions(&response))
}

pub async fn intervention_show(client: &KomoClient, handle: &str) -> Outcome {
    let detail = client.intervention(handle).await.map_err(failed)?;
    Ok(render::intervention_detail(&detail))
}

/// `komo intervention answer <handle> <结论>`（§7.5）。一次答一条。
///
/// **结论在发请求之前按口语校验**：写错一个词就让服务端去猜种类，只会得到一句
/// "这个结论对这一条不适用"。这里先看词表，再让服务端按权威判定（种类不符时它照样
/// 会拒，那是最后一道）。
pub async fn intervention_answer(client: &KomoClient, handle: &str, verdict: &str) -> Outcome {
    let verdict = InterventionVerdict::parse(verdict).ok_or_else(|| {
        format!("不认识这个结论：{verdict}；能写的是 satisfied / not_performed / resolve / abandon")
    })?;
    let response = client
        .answer_intervention(
            handle,
            &InterventionAnswerRequest {
                verdict,
                request_key: Some(RequestKeys::named("cli-answer", handle)),
            },
        )
        .await
        .map_err(failed)?;
    Ok(format!(
        "{} {}{}{}",
        response.handle,
        answer_word(response.verdict),
        if response.already_answered {
            "（这次什么都没改，返回的是之前那个结论）"
        } else {
            ""
        },
        match &response.run_state {
            Some(state) => format!("；这条 Run 现在是 {}", state.as_str()),
            None => String::new(),
        }
    ))
}

/// 结论词在人读的那一面是中文，在命令里是线格式词。
fn answer_word(verdict: InterventionVerdict) -> &'static str {
    match verdict {
        InterventionVerdict::Satisfied => "已记下：核对后目标已满足",
        InterventionVerdict::NotPerformed => "已记下：确定没有执行",
        InterventionVerdict::Resolve => "已记下：前提已处理，重新核对",
        InterventionVerdict::Abandon => "已放弃这条 Run",
    }
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
    /// 与 `command` 二选一（§10）。
    pub prompt: Option<String>,
    pub command: Option<String>,
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
    // `--prompt` 与 `--command` 二选一——clap 的 `conflicts_with` 挡了"两个都给"，
    // 这里补上"一个都没给"（第二道校验，HTTP 那边是第三道，两处都校验，§10）。
    let (prompt, command) = prompt_or_command(add.prompt, add.command)?;
    let job = client
        .cron_create(&CreateCronRequest {
            name: add.name,
            schedule: add.schedule,
            timezone: add.timezone,
            prompt,
            command,
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

/// `--prompt` 与 `--command` 二选一（§10）：clap 的 `conflicts_with` 已经挡住"两个都
/// 给"，这里补上"一个都没给"，产出发请求要的形状——`CreateCronRequest.prompt` 仍是
/// 必填的 `String`，命令 Job 那一列写空串（§8.2 的惯例）。
fn prompt_or_command(
    prompt: Option<String>,
    command: Option<String>,
) -> Result<(String, Option<String>), String> {
    match (prompt, command) {
        (Some(prompt), None) => Ok((prompt, None)),
        (None, Some(command)) => Ok((String::new(), Some(command))),
        (None, None) => Err("--prompt 与 --command 必须给一个（§10）".into()),
        (Some(_), Some(_)) => Err("--prompt 与 --command 二选一，不能同时给（§10）".into()),
    }
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
/// 印出版本号，因为这条命令**不递增它**（§10：暂停不是定义变更）——看得见才说得清。
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
    /// 正文不截断、一条一行（[`render::memory_list_full`]）。
    pub full: bool,
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
    if filter.full {
        return Ok(render::memory_list_full(&response));
    }
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

/// forget / restore 用一次一新的键：两者能来回切，`忘 → 恢复 → 再忘` 的第三步若带着与
/// 第一步相同的命名键和相同的 revision，会命中幂等缓存、拿回旧响应而什么都不做。
pub async fn memory_forget(client: &KomoClient, id: &str, revision: u32) -> Outcome {
    let detail = client
        .memory_forget(
            &MemoryId::from_raw(id),
            &MemoryRevisionRequest {
                expected_revision: revision,
                request_key: Some(RequestKeys::fresh(time::OffsetDateTime::now_utc())),
            },
        )
        .await
        .map_err(failed)?;
    Ok(render::memory_show(&detail.memory))
}

pub async fn memory_restore(client: &KomoClient, id: &str, revision: u32) -> Outcome {
    let detail = client
        .memory_restore(
            &MemoryId::from_raw(id),
            &MemoryRevisionRequest {
                expected_revision: revision,
                request_key: Some(RequestKeys::fresh(time::OffsetDateTime::now_utc())),
            },
        )
        .await
        .map_err(failed)?;
    Ok(render::memory_show(&detail.memory))
}

pub async fn memory_sweep(
    client: &KomoClient,
    apply: bool,
    never_used_days: u32,
    idle_days: u32,
) -> Outcome {
    let response = client
        .memory_sweep(&MemorySweepRequest {
            apply,
            never_used_days,
            idle_days,
        })
        .await
        .map_err(failed)?;
    Ok(render::memory_sweep(&response))
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

pub async fn doctor(home: &Path, reconcile: bool) -> Outcome {
    let client = match crate::connect::connect(home).await {
        Ok(client) => client,
        Err(error) => return Err(format!("Gateway 没在跑：{error}")),
    };
    let health = client.health().await.map_err(failed)?;
    let config = client.config_check().await.ok();
    let mut out = render::doctor(&health, config.as_ref());
    if reconcile {
        // §8.9：对账只读观察 + 只写状态。印出这一趟判定了什么——"没事"也要看得见
        // 它看过了多少条，否则"没输出"和"没跑"分不开。
        let report = client.reconcile().await.map_err(failed)?;
        out.push_str(&format!(
            "\n对账（{}）：看了 {} 条未完成 Run；{} 条继续，{} 条停在等人，{} 条没主人的 running 已回收；{} 个会话推进到 deleted，{} 个会话的内容已补齐回收",
            render::stamp(report.finished_at),
            report.checked,
            report.resumed,
            report.blocked,
            report.reclaimed,
            report.closed,
            report.purged
        ));
    }
    Ok(out)
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

/// `komo auth codex status`：账号、邮箱、套餐、过期时间——**不打印 token**，
/// 不经 Gateway（§3、§13.3）。
pub fn auth_codex_status(home: &Path) -> Outcome {
    let path = komo_gateway::codex_auth::credentials_path(home);
    let status = komo_gateway::codex_auth::status(&path).map_err(failed)?;
    let mut line = format!("账号 {}", status.account_id);
    if let Some(email) = &status.email {
        line.push_str(&format!("  邮箱 {email}"));
    }
    if let Some(plan) = &status.plan {
        line.push_str(&format!("  套餐 {plan}"));
    }
    line.push_str(&format!("  过期 {}", render::stamp(status.expires_at)));
    Ok(line)
}

/// `oauth = true` 的那个 MCP 服务器的地址；没配或不是 OAuth 就说清楚。
fn oauth_server_url(home: &Path, server: &str) -> Result<String, String> {
    use komo_kernel::protocol::config::McpTransport;
    let loaded = load_config(&LoadOptions::at(home)).map_err(failed)?;
    match loaded
        .snapshot
        .start_only
        .mcp
        .get(server)
        .map(|c| &c.transport)
    {
        Some(McpTransport::Http {
            url, oauth: true, ..
        }) => Ok(url.clone()),
        Some(_) => Err(format!("[mcp.servers.{server}] 没写 oauth = true")),
        None => Err(format!("config.toml 里没有 [mcp.servers.{server}]")),
    }
}

/// `komo auth mcp login <server>`：浏览器授权，凭证直接落盘，不经 Gateway（同
/// `komo auth codex login`）。
pub async fn auth_mcp_login(home: &Path, server: &str) -> Outcome {
    let url = oauth_server_url(home, server)?;
    let path = komo_gateway::mcp_auth::credentials_path(home, server);
    let mut out = std::io::stderr();
    komo_gateway::mcp_auth::login(&url, &path, &mut out)
        .await
        .map_err(failed)?;
    Ok(format!(
        "{server} 登录成功，凭证已写入 {}；跑 `komo gateway restart` 让 Gateway 连上它",
        path.display()
    ))
}

/// `komo auth mcp status`：每个 `oauth = true` 的服务器一行，不打印 token。
pub fn auth_mcp_status(home: &Path) -> Outcome {
    use komo_kernel::protocol::config::McpTransport;
    let loaded = load_config(&LoadOptions::at(home)).map_err(failed)?;
    let mut lines = Vec::new();
    for (name, config) in &loaded.snapshot.start_only.mcp {
        if !matches!(config.transport, McpTransport::Http { oauth: true, .. }) {
            continue;
        }
        let path = komo_gateway::mcp_auth::credentials_path(home, name);
        let line = match komo_gateway::mcp_auth::status(&path).map_err(failed)? {
            None => format!("{name:<12} 未登录（komo auth mcp login {name}）"),
            Some(status) => {
                let expiry = status
                    .expires_at
                    .and_then(|secs| time::OffsetDateTime::from_unix_timestamp(secs as i64).ok())
                    .map_or("未给过期时间".to_string(), |at| {
                        format!("过期 {}", render::stamp(at))
                    });
                let refresh = if status.refreshable {
                    "可自动刷新"
                } else {
                    "不能自动刷新，过期要重新登录"
                };
                format!("{name:<12} 已登录  {expiry}  {refresh}")
            }
        };
        lines.push(line);
    }
    if lines.is_empty() {
        return Ok("没有 oauth = true 的 MCP 服务器".into());
    }
    Ok(lines.join("\n"))
}

/// `komo skills ...`：只读文件系统，不经 Gateway（§5.6）。
pub fn skills(home: &Path, action: SkillsAction<'_>) -> Outcome {
    let loaded = load_config(&LoadOptions::at(home)).map_err(failed)?;
    // 与 Gateway 同一个搜索路径（`from_snapshot`：配置里的目录 + `~/.komo/skills` +
    // `~/.agents/skills`）。列表要是与系统提示里的目录行对不上，人就没法回答"我的 skill
    // 为什么没进提示"。
    let registry = SkillRegistry::from_snapshot(
        &loaded.snapshot,
        komo_gateway::config::user_home().ok().as_deref(),
    );
    let offer = offer_context();
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
                    // 门控不过 / 被 disable 的也在列表里——但要说清楚它**不进系统提示**，
                    // 否则"我明明写了它"与"模型看不见它"对不上。
                    let mark = if registry.is_disabled(&skill.name) {
                        "（已停用）"
                    } else if skill.offered(&offer) {
                        ""
                    } else {
                        "（不进提示：平台或工具不满足）"
                    };
                    format!("{}{}  {}", skill.name, mark, one_line(&skill.description))
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

/// `komo skills` 的门控上下文：与 Gateway 的目录行同一个口径（本机平台 + 5 个基础工具）。
pub fn offer_context() -> OfferContext {
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
            command: None,
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
        assert!(line.contains(&render::stamp(NOW)), "{line}");
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

    /// `--prompt` 与 `--command` 二选一（§10）：CLI 这一侧的第二道校验（第一道是
    /// clap 的 `conflicts_with`，见 `main.rs` 的解析测试）。
    #[test]
    fn prompt_and_command_are_mutually_exclusive_and_one_is_required() {
        assert_eq!(
            prompt_or_command(Some("整理今天的动态".into()), None).unwrap(),
            ("整理今天的动态".to_string(), None)
        );
        assert_eq!(
            prompt_or_command(None, Some("echo hi".into())).unwrap(),
            (String::new(), Some("echo hi".to_string()))
        );
        let neither = prompt_or_command(None, None).unwrap_err();
        assert!(neither.contains("必须给一个"), "{neither}");
        let both = prompt_or_command(Some("x".into()), Some("y".into())).unwrap_err();
        assert!(both.contains("二选一"), "{both}");
    }
}
