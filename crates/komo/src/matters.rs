//! `komo goal …` / `komo area …`（`docs/goals.md` §4）：clap 定义与分发。
//!
//! 谁在调由环境说：bash 工具的子进程带着 `KOMO_SESSION` 与 `KOMO_RUN`，CLI 原样带上，
//! Gateway 据此要求改承诺的操作有这个 Run 里用户的原话（同 `memory add`）；`list` / `show`
//! 带上会话，给别人用的 Agent 的会话被拒。

use clap::{Args, Subcommand};
use komo_client::KomoClient;
use komo_client::api::RequestKeys;
use komo_client::render;
use komo_kernel::cron::{NotifyPolicy, OverlapPolicy};
use komo_kernel::protocol::http::{
    MatterActionRequest, MatterAddRequest, MatterListQuery, ScheduleSpec,
};
use komo_kernel::types::ids::{MatterId, RunId, SessionId};
use komo_kernel::types::model::Effort;
use komo_kernel::types::matter::{MatterKind, MatterTrigger, MatterVerb, parse_due};

use crate::commands::Outcome;

/// 改一条事项都要的：哪一条、预期哪一版。
#[derive(Args)]
pub struct Target {
    /// 事项 ID。
    id: String,
    /// 预期版本；不符报冲突。不带就按当前版本改。
    #[arg(long)]
    revision: Option<u32>,
}

/// 要挂 / 摘的触发器：monitor。定时不再是挂在目标上的引用，而是目标自己的配置（`schedule`）。
#[derive(Args)]
pub struct TriggerArg {
    /// monitor：挂它的会话 ID 与 monitor ID，写成 SESSION_ID:MONITOR_ID（`#` 可省）。
    #[arg(long, value_name = "SESSION_ID:MONITOR_ID")]
    monitor: String,
}

impl TriggerArg {
    fn into_request(self) -> Result<MatterActionRequest, String> {
        let trigger = MatterTrigger::parse_monitor(&self.monitor)?;
        trigger.check_shape()?;
        Ok(MatterActionRequest {
            trigger: Some(trigger),
            ..MatterActionRequest::default()
        })
    }
}

/// 目标上的定时（§10 的 Job 字段全在这里）：创建时随 `add` 一起给，之后用 `schedule`，
/// 给出的字段才改。
#[derive(Args, Default)]
pub struct ScheduleArgs {
    /// 五字段 cron 表达式，或 `@at YYYY-MM-DD HH:MM`。
    #[arg(long)]
    schedule: Option<String>,
    /// IANA 时区名；新设置时不给就是 UTC。
    #[arg(long)]
    timezone: Option<String>,
    /// 到点交给模型做的事（与 `--command` 二选一；都不给取目标的说明）。
    #[arg(long, conflicts_with = "command")]
    prompt: Option<String>,
    /// 命令直跑模式：不经模型，到点固定跑这一条 shell 命令（§10）。
    #[arg(long, conflicts_with = "prompt")]
    command: Option<String>,
    /// 工作目录；**设置时就核实**（§10）。
    #[arg(long)]
    workdir: Option<String>,
    /// 这个定时的主模型：模型目录里的 alias。**按完整模型配置解析**，记忆整理与向量模型不动。
    #[arg(long)]
    model: Option<String>,
    /// 这个定时的 effort。不支持的档位**在请求前**被拒绝并指出支持值（§13.3）。
    #[arg(long)]
    effort: Option<String>,
    /// 触发时预载进首轮上下文的 SKILL.md（§5.6）。可重复。
    #[arg(long = "skill")]
    skills: Vec<String>,
    /// 执行预算：一次触发最多跑几轮模型。
    #[arg(long)]
    max_rounds: Option<u32>,
    /// 上一次还没结束时：skip（默认）/ allow。
    #[arg(long)]
    overlap: Option<String>,
    /// 结果投递：always（默认）/ on_error / never。停着等人**不受它约束**。
    #[arg(long)]
    notify: Option<String>,
    /// 暂停定时（不改定义）。
    #[arg(long, conflicts_with = "resume")]
    pause: bool,
    /// 恢复定时。
    #[arg(long)]
    resume: bool,
}

impl ScheduleArgs {
    /// 一个字段都没给 = `None`。重叠策略与 notify **在发请求之前**校验：它们是打字打出来的，
    /// 而"不认识就当默认"会让一个写错的 `--notify nerver` 变成"每次都投"。
    pub fn into_spec(self) -> Result<Option<ScheduleSpec>, String> {
        let overlap = self
            .overlap
            .as_deref()
            .map(|raw| match raw.trim().to_ascii_lowercase().as_str() {
                "skip" => Ok(OverlapPolicy::Skip),
                "allow" => Ok(OverlapPolicy::Allow),
                other => Err(format!("--overlap 只接受 skip / allow，收到：{other}")),
            })
            .transpose()?;
        let notify = self
            .notify
            .as_deref()
            .map(|raw| {
                NotifyPolicy::parse(raw).ok_or_else(|| {
                    format!("--notify 只接受 always / on_error / never，收到：{raw}")
                })
            })
            .transpose()?;
        let spec = ScheduleSpec {
            schedule: self.schedule,
            timezone: self.timezone,
            prompt: self.prompt,
            command: self.command,
            workdir: self.workdir,
            model: self.model,
            model_config: None,
            effort: self.effort.as_deref().map(Effort::new),
            skills: (!self.skills.is_empty()).then_some(self.skills),
            overlap,
            max_rounds: self.max_rounds,
            notify,
            enabled: match (self.pause, self.resume) {
                (true, _) => Some(false),
                (_, true) => Some(true),
                _ => None,
            },
        };
        Ok((spec != ScheduleSpec::default()).then_some(spec))
    }
}

#[derive(Subcommand)]
pub enum GoalCommand {
    /// 新建一个目标（active）。
    Add {
        /// 一句能独立看懂的话，≤ 80 字。
        title: String,
        /// 下一步。
        #[arg(long)]
        next: Option<String>,
        /// 截止日期，YYYY-MM-DD。
        #[arg(long)]
        due: Option<String>,
        /// 说明：这件事是什么、到点要做什么。
        #[arg(long)]
        description: Option<String>,
        #[command(flatten)]
        schedule: ScheduleArgs,
    },
    /// 改下一步（active / waiting）；给空串清空。
    Next {
        #[command(flatten)]
        target: Target,
        /// 下一步，一句话。
        next: String,
    },
    /// 转等待，写明在等谁、等什么（active / waiting）。
    Wait {
        #[command(flatten)]
        target: Target,
        /// 在等谁、等什么。
        waiting_for: String,
    },
    /// 回到进行中（waiting / parked）。
    Resume {
        #[command(flatten)]
        target: Target,
        /// 顺带改下一步。
        #[arg(long)]
        next: Option<String>,
    },
    /// 搁置（active / waiting）。
    Park {
        #[command(flatten)]
        target: Target,
    },
    /// 完成（active / waiting / parked）。
    Done {
        #[command(flatten)]
        target: Target,
    },
    /// 放弃（active / waiting / parked）。
    Drop {
        #[command(flatten)]
        target: Target,
    },
    /// 重开回进行中（done / dropped）。
    Reopen {
        #[command(flatten)]
        target: Target,
    },
    /// 改标题、截止日期、说明（未结案）。
    Edit {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        title: Option<String>,
        /// 截止日期，YYYY-MM-DD。
        #[arg(long, conflicts_with = "no_due")]
        due: Option<String>,
        /// 去掉截止日期。
        #[arg(long)]
        no_due: bool,
        /// 说明；给空串清空。
        #[arg(long)]
        description: Option<String>,
    },
    /// 设置或修改定时（未结案）：给出的字段才改；还没有定时时要给 `--schedule`。
    Schedule {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        args: ScheduleArgs,
    },
    /// 去掉定时；目标与触发历史保留。
    Unschedule {
        #[command(flatten)]
        target: Target,
    },
    /// 立刻触发一次定时（不冒充计划触发，不推进槽位）。
    Run {
        #[command(flatten)]
        target: Target,
    },
    /// 挂一个触发器：monitor（未结案）；已挂着的无变化。
    Link {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        trigger: TriggerArg,
    },
    /// 摘掉一个触发器（未结案）；没挂着的报错。
    Unlink {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        trigger: TriggerArg,
    },
    /// 清单：一条一行；默认只列未结案的。
    List {
        /// 带上已完成、已放弃的。
        #[arg(long)]
        all: bool,
    },
    /// 详情。
    Show {
        /// 事项 ID。
        id: String,
    },
}

#[derive(Subcommand)]
pub enum AreaCommand {
    /// 新建一个领域（active）。
    Add {
        /// 名称，一句话，≤ 80 字。
        title: String,
        /// 要维持的标准。
        #[arg(long)]
        standard: Option<String>,
        /// 说明：这件事是什么、到点要做什么。
        #[arg(long)]
        description: Option<String>,
        #[command(flatten)]
        schedule: ScheduleArgs,
    },
    /// 改名称、标准、说明（active）；标准、说明给空串清空。
    Edit {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        standard: Option<String>,
        /// 说明；给空串清空。
        #[arg(long)]
        description: Option<String>,
    },
    /// 设置或修改定时（在管）：给出的字段才改；还没有定时时要给 `--schedule`。
    Schedule {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        args: ScheduleArgs,
    },
    /// 去掉定时；领域与触发历史保留。
    Unschedule {
        #[command(flatten)]
        target: Target,
    },
    /// 立刻触发一次定时（不冒充计划触发，不推进槽位）。
    Run {
        #[command(flatten)]
        target: Target,
    },
    /// 不再管（active）。
    Retire {
        #[command(flatten)]
        target: Target,
    },
    /// 重开（retired）。
    Reopen {
        #[command(flatten)]
        target: Target,
    },
    /// 挂一个触发器：monitor（在管）；已挂着的无变化。
    Link {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        trigger: TriggerArg,
    },
    /// 摘掉一个触发器（在管）；没挂着的报错。
    Unlink {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        trigger: TriggerArg,
    },
    /// 清单：一条一行；默认只列在管的。
    List {
        /// 带上不再管的。
        #[arg(long)]
        all: bool,
    },
    /// 详情。
    Show {
        /// 事项 ID。
        id: String,
    },
}

pub async fn goal(client: &KomoClient, action: GoalCommand) -> Outcome {
    let kind = MatterKind::Goal;
    let blank = MatterActionRequest::default;
    match action {
        GoalCommand::Add {
            title,
            next,
            due,
            description,
            schedule,
        } => {
            add(
                client,
                MatterAddRequest {
                    kind,
                    title,
                    next,
                    due: due.as_deref().map(parse_due).transpose()?,
                    standard: None,
                    description,
                    schedule: schedule.into_spec()?,
                    session: None,
                    run: None,
                    request_key: None,
                },
            )
            .await
        }
        GoalCommand::Next { target, next } => {
            let request = MatterActionRequest {
                next: Some(next),
                ..blank()
            };
            act(client, kind, target, MatterVerb::Next, request).await
        }
        GoalCommand::Wait {
            target,
            waiting_for,
        } => {
            let request = MatterActionRequest {
                waiting_for: Some(waiting_for),
                ..blank()
            };
            act(client, kind, target, MatterVerb::Wait, request).await
        }
        GoalCommand::Resume { target, next } => {
            let request = MatterActionRequest { next, ..blank() };
            act(client, kind, target, MatterVerb::Resume, request).await
        }
        GoalCommand::Park { target } => act(client, kind, target, MatterVerb::Park, blank()).await,
        GoalCommand::Done { target } => act(client, kind, target, MatterVerb::Done, blank()).await,
        GoalCommand::Drop { target } => act(client, kind, target, MatterVerb::Drop, blank()).await,
        GoalCommand::Reopen { target } => {
            act(client, kind, target, MatterVerb::Reopen, blank()).await
        }
        GoalCommand::Edit {
            target,
            title,
            due,
            no_due,
            description,
        } => {
            let request = MatterActionRequest {
                title,
                due: due.as_deref().map(parse_due).transpose()?,
                clear_due: no_due,
                description,
                ..blank()
            };
            act(client, kind, target, MatterVerb::Edit, request).await
        }
        GoalCommand::Schedule { target, args } => schedule(client, kind, target, args).await,
        GoalCommand::Unschedule { target } => {
            act(client, kind, target, MatterVerb::Unschedule, blank()).await
        }
        GoalCommand::Run { target } => act(client, kind, target, MatterVerb::Run, blank()).await,
        GoalCommand::Link { target, trigger } => {
            act(
                client,
                kind,
                target,
                MatterVerb::Link,
                trigger.into_request()?,
            )
            .await
        }
        GoalCommand::Unlink { target, trigger } => {
            act(
                client,
                kind,
                target,
                MatterVerb::Unlink,
                trigger.into_request()?,
            )
            .await
        }
        GoalCommand::List { all } => list(client, kind, all).await,
        GoalCommand::Show { id } => show(client, &id).await,
    }
}

pub async fn area(client: &KomoClient, action: AreaCommand) -> Outcome {
    let kind = MatterKind::Area;
    let blank = MatterActionRequest::default;
    match action {
        AreaCommand::Add {
            title,
            standard,
            description,
            schedule,
        } => {
            add(
                client,
                MatterAddRequest {
                    kind,
                    title,
                    next: None,
                    due: None,
                    standard,
                    description,
                    schedule: schedule.into_spec()?,
                    session: None,
                    run: None,
                    request_key: None,
                },
            )
            .await
        }
        AreaCommand::Edit {
            target,
            title,
            standard,
            description,
        } => {
            let request = MatterActionRequest {
                title,
                standard,
                description,
                ..blank()
            };
            act(client, kind, target, MatterVerb::Edit, request).await
        }
        AreaCommand::Schedule { target, args } => schedule(client, kind, target, args).await,
        AreaCommand::Unschedule { target } => {
            act(client, kind, target, MatterVerb::Unschedule, blank()).await
        }
        AreaCommand::Run { target } => act(client, kind, target, MatterVerb::Run, blank()).await,
        AreaCommand::Retire { target } => {
            act(client, kind, target, MatterVerb::Retire, blank()).await
        }
        AreaCommand::Reopen { target } => {
            act(client, kind, target, MatterVerb::Reopen, blank()).await
        }
        AreaCommand::Link { target, trigger } => {
            act(
                client,
                kind,
                target,
                MatterVerb::Link,
                trigger.into_request()?,
            )
            .await
        }
        AreaCommand::Unlink { target, trigger } => {
            act(
                client,
                kind,
                target,
                MatterVerb::Unlink,
                trigger.into_request()?,
            )
            .await
        }
        AreaCommand::List { all } => list(client, kind, all).await,
        AreaCommand::Show { id } => show(client, &id).await,
    }
}

fn from_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn session() -> Option<SessionId> {
    from_env("KOMO_SESSION").map(SessionId::from_raw)
}

fn run() -> Option<RunId> {
    from_env("KOMO_RUN").map(RunId::from_raw)
}

fn fresh_key() -> Option<komo_kernel::types::ids::RequestKey> {
    Some(RequestKeys::fresh(time::OffsetDateTime::now_utc()))
}

async fn add(client: &KomoClient, mut request: MatterAddRequest) -> Outcome {
    request.session = session();
    request.run = run();
    request.request_key = fresh_key();
    let detail = client
        .matter_add(&request)
        .await
        .map_err(|e| e.to_string())?;
    Ok(render::matter_changed(&detail))
}

async fn act(
    client: &KomoClient,
    kind: MatterKind,
    target: Target,
    verb: MatterVerb,
    mut request: MatterActionRequest,
) -> Outcome {
    request.kind = Some(kind);
    request.expected_revision = target.revision;
    request.session = session();
    request.run = run();
    request.request_key = fresh_key();
    let detail = client
        .matter_act(&MatterId::from_raw(target.id), verb, &request)
        .await
        .map_err(|e| e.to_string())?;
    Ok(render::matter_changed(&detail))
}

async fn schedule(
    client: &KomoClient,
    kind: MatterKind,
    target: Target,
    args: ScheduleArgs,
) -> Outcome {
    let spec = args
        .into_spec()?
        .ok_or("至少要给一个定时的字段（--schedule、--pause …）")?;
    let request = MatterActionRequest {
        schedule: Some(spec),
        ..MatterActionRequest::default()
    };
    act(client, kind, target, MatterVerb::Schedule, request).await
}

async fn list(client: &KomoClient, kind: MatterKind, all: bool) -> Outcome {
    let response = client
        .matters(&MatterListQuery {
            kind: Some(kind),
            all,
            session: session(),
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(render::matter_list(&response))
}

async fn show(client: &KomoClient, id: &str) -> Outcome {
    let detail = client
        .matter(&MatterId::from_raw(id), session().as_ref())
        .await
        .map_err(|e| e.to_string())?;
    Ok(render::matter_show(&detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trigger_argument_becomes_one_monitor_trigger_or_an_error() {
        let monitor = TriggerArg {
            monitor: "sess-1:#3f2a".into(),
        }
        .into_request()
        .unwrap();
        assert_eq!(
            monitor.trigger,
            Some(MatterTrigger::parse_monitor("sess-1:3f2a").unwrap())
        );
        for bad in ["没有冒号", "", "sess-1:"] {
            let arg = TriggerArg {
                monitor: bad.into(),
            };
            assert!(arg.into_request().is_err(), "{bad:?}");
        }
    }
}
