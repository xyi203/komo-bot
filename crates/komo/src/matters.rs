//! `komo goal …` / `komo area …`（`docs/goals.md` §4）：clap 定义与分发。
//!
//! 谁在调由环境说：bash 工具的子进程带着 `KOMO_SESSION` 与 `KOMO_RUN`，CLI 原样带上，
//! Gateway 据此要求改承诺的操作有这个 Run 里用户的原话（同 `memory add`）；`list` / `show`
//! 带上会话，给别人用的 Agent 的会话被拒。

use clap::{Args, Subcommand};
use komo_client::KomoClient;
use komo_client::api::RequestKeys;
use komo_client::render;
use komo_kernel::protocol::http::{MatterActionRequest, MatterAddRequest, MatterListQuery};
use komo_kernel::types::ids::{MatterId, RunId, SessionId};
use komo_kernel::types::matter::{MatterKind, MatterVerb, parse_due};

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
    /// 改标题、截止日期（未结案）。
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
    },
    /// 改名称、标准（active）；标准给空串清空。
    Edit {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        standard: Option<String>,
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
        GoalCommand::Add { title, next, due } => {
            add(
                client,
                MatterAddRequest {
                    kind,
                    title,
                    next,
                    due: due.as_deref().map(parse_due).transpose()?,
                    standard: None,
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
        } => {
            let request = MatterActionRequest {
                title,
                due: due.as_deref().map(parse_due).transpose()?,
                clear_due: no_due,
                ..blank()
            };
            act(client, kind, target, MatterVerb::Edit, request).await
        }
        GoalCommand::List { all } => list(client, kind, all).await,
        GoalCommand::Show { id } => show(client, &id).await,
    }
}

pub async fn area(client: &KomoClient, action: AreaCommand) -> Outcome {
    let kind = MatterKind::Area;
    let blank = MatterActionRequest::default;
    match action {
        AreaCommand::Add { title, standard } => {
            add(
                client,
                MatterAddRequest {
                    kind,
                    title,
                    next: None,
                    due: None,
                    standard,
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
        } => {
            let request = MatterActionRequest {
                title,
                standard,
                ..blank()
            };
            act(client, kind, target, MatterVerb::Edit, request).await
        }
        AreaCommand::Retire { target } => {
            act(client, kind, target, MatterVerb::Retire, blank()).await
        }
        AreaCommand::Reopen { target } => {
            act(client, kind, target, MatterVerb::Reopen, blank()).await
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
