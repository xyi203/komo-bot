//! komo 二进制：clap 分发（docs/komo_bot.md §3 命令表、§13.4）。
//!
//! 这一层只做分发：`komo` / `komo home` / `komo resume` 与操作子命令走 komo-client，
//! `komo gateway` 走 komo-gateway。**没有第二份业务逻辑**——每条命令要么打一个 HTTP
//! 接口，要么读一次配置文件。

mod commands;
mod connect;
mod update;

use std::path::Path;

use clap::{Parser, Subcommand};
use komo_client::{TuiMode, run_tui};
use komo_kernel::types::ids::SessionId;

#[derive(Parser)]
#[command(name = "komo", version, about = "komo：个人 Agent 框架", long_about = None)]
struct Cli {
    /// 不带子命令时：确保本机 Gateway 就绪，创建新 Session，进入 TUI 聊天（§3）。
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 操作者那一个常驻会话：早上在微信说的话，回到终端接着说（§11.2）。
    Home,
    /// 连接原会话并查看进度或处理待办；恢复调度由 Gateway 自动进行（§3）。
    Resume {
        /// 会话 ID。
        session_id: String,
    },
    /// 会话列表与状态。
    Session {
        #[command(subcommand)]
        action: SessionCommand,
    },
    /// 查看或取消运行。
    Run {
        #[command(subcommand)]
        action: RunCommand,
    },
    /// 启动后台 Gateway，等待就绪后返回；子命令管理后台进程（§3）。
    Gateway {
        /// 前台运行 Gateway，供服务管理器及调试使用。
        #[arg(long)]
        foreground: bool,
        #[command(subcommand)]
        action: Option<GatewayCommand>,
    },
    /// 待处理清单与答复：审批、结果不明、阻塞三类一张表（§7.5）。
    ///
    /// 等价于聊天里的 `/pending` 与 `/answer`（§11.3）。
    Intervention {
        #[command(subcommand)]
        action: InterventionCommand,
    },
    /// 管理定时任务（§10）。
    Cron {
        #[command(subcommand)]
        action: CronCommand,
    },
    /// 查看自动记忆、来源与确认状态（§9）。
    Memory {
        #[command(subcommand)]
        action: MemoryCommand,
    },
    /// 校验或重载配置（§3）。
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },
    /// 渠道清单、连通性核对与微信登录；不经 Gateway（§3）。
    Channel {
        #[command(subcommand)]
        action: ChannelCommand,
    },
    /// Skills 目录；只读文件系统，不经 Gateway（§5.6）。
    Skills {
        #[command(subcommand)]
        action: SkillsCommand,
    },
    /// 已保存的 Python 能力：候选、测试、启用与停用（§5.3、§5.4）。
    ///
    // TODO(decide: §3 的命令表里没有它。toolbox 是文件系统上的东西，但**启用要审批**，
    // 而审批在 state.db 里、只有 Gateway 打得开（§12）——所以整组走 Gateway，不像
    // `komo skills` 那样直接读盘。见报告。)
    Toolbox {
        #[command(subcommand)]
        action: ToolboxCommand,
    },
    /// 显示当前生效配置的加载时间与来源文件 mtime，以及上一次校验错误（§3）。
    ///
    /// `--reconcile` 顺带跑一次对账（§8.9：只读观察 + 只写状态，不调工具、不消费授权）。
    Doctor {
        /// 立刻跑一次对账，印出这一趟判定了什么。
        #[arg(long)]
        reconcile: bool,
    },
    /// 从 GitHub release 换掉这个可执行文件（§3、§13.6）。
    ///
    /// 不经 Gateway：换的是**磁盘上那份二进制**，不是在跑的那个进程。最后一步是同目录
    /// `rename`，校验和与试跑都在旧文件还在时做——失败不会留下一个跑不起来的 komo。
    Update,
}

#[derive(Subcommand)]
enum SessionCommand {
    /// 查看会话列表和状态；`--all` 才列出已逻辑删除的（§8.10）。
    List {
        /// 把已逻辑删除（closing / deleted）的会话也列出来。
        #[arg(long)]
        all: bool,
    },
    /// 逻辑删除一个会话：进 `closing`，不再接受新输入，**内容一个字节都不动**（§8.10）。
    Delete {
        /// 会话 ID。
        session_id: String,
        /// 不等待未完成的 Run：立刻把它们各写一条明确取消，再进 `deleted`。
        #[arg(long)]
        now: bool,
    },
    /// 回收一个会话的内容（`purged`）；引用未处置完会列出要先处理什么（§8.10）。
    Purge {
        /// 会话 ID。
        session_id: String,
    },
}

#[derive(Subcommand)]
enum RunCommand {
    /// 查看执行过程、工具结果与产物。
    Inspect {
        /// 运行 ID。
        run_id: String,
    },
    /// 请求取消运行。
    Cancel {
        /// 运行 ID。
        run_id: String,
    },
}

#[derive(Subcommand)]
enum GatewayCommand {
    /// 查询后台进程状态；不隐式启动服务（§3）。
    Status,
    /// 停止后台进程；不隐式启动服务（§3）。
    Stop,
    /// 重启后台进程。
    Restart,
}

#[derive(Subcommand)]
enum InterventionCommand {
    /// 待处理清单：审批、结果不明、阻塞三类一张表（§7.5）。
    List {
        /// 只看某个会话的。
        #[arg(long)]
        session: Option<String>,
    },
    /// 单项详情；审批类是 §7.2 要展示的那一份（计划、改动、原因、范围）。
    Show {
        /// 句柄：审批是短 ID，结果不明与阻塞是 Run ID。
        handle: String,
    },
    /// 答复。结论按种类分派（§7.5）：审批 `approve` / `reject`（可带 `--scope`），
    /// 结果不明 `satisfied` / `not_performed` / `abandon`，阻塞 `resolve` / `abandon`。
    Answer {
        /// 句柄：审批是短 ID，结果不明与阻塞是 Run ID。
        handle: String,
        /// 结论词。
        verdict: String,
        /// 只对 `approve` 有意义的范围：`once`（默认）/ `run` / `cron`（§7.2）。
        #[arg(long)]
        scope: Option<String>,
    },
    /// 一次答一批**审批**（等价于聊天里的 `/approve all`）。
    AnswerAll {
        /// 结论词：只接受 `approve` / `reject`。
        verdict: String,
    },
}

#[derive(Subcommand)]
enum CronCommand {
    /// 创建定时任务。
    ///
    /// **装箱**：§10 的 Job 字段还会长，而 clap 的子命令枚举按最大变体定大小——一个
    /// 只在启动时解析一次的枚举不值得让其余每个变体都跟着它变胖。
    Add(Box<CronAddArgs>),
    /// 列出定时任务。
    List,
    /// 手动触发一次。
    Run {
        /// 定时任务 ID。
        job_id: String,
    },
    /// 暂停调度。
    Pause {
        /// 定时任务 ID。
        job_id: String,
    },
    /// 恢复调度。
    Resume {
        /// 定时任务 ID。
        job_id: String,
    },
    /// 移除后续调度，已有执行历史保留。
    Remove {
        /// 定时任务 ID。
        job_id: String,
    },
}

/// `komo cron add` 的 flag（§10 的 Job 字段表）。
#[derive(clap::Args)]
struct CronAddArgs {
    #[arg(long)]
    name: String,
    /// 五字段 cron 表达式，或 `@at YYYY-MM-DD HH:MM`。
    #[arg(long)]
    schedule: String,
    /// IANA 时区名。
    #[arg(long, default_value = "UTC")]
    timezone: String,
    #[arg(long)]
    prompt: String,
    /// 这个 Job 的工作目录；**创建时就核实**（§10）。
    #[arg(long)]
    workdir: Option<String>,
    /// 这个 Job 的主模型。**按完整模型配置解析**：只换模型名，端点与凭证仍是
    /// 主模型那一份；记忆整理与向量模型一个字都不动（§10、§13.3）。
    #[arg(long)]
    model: Option<String>,
    /// 这个 Job 的 effort。不支持的档位**在请求前**被拒绝并指出支持值（§13.3）。
    #[arg(long)]
    effort: Option<String>,
    /// 触发时预载进首轮上下文的 SKILL.md（§5.6）。可重复。
    #[arg(long = "skill")]
    skills: Vec<String>,
    /// 执行预算：一次触发最多跑几轮模型。
    #[arg(long)]
    max_rounds: Option<u32>,
    /// 上一次还没结束时：skip（默认）/ allow。
    #[arg(long, default_value = "skip")]
    overlap: String,
    /// 结果投递：always（默认）/ on_error / never。等待审批**不受它约束**。
    #[arg(long, default_value = "always")]
    notify: String,
}

#[derive(Subcommand)]
enum MemoryCommand {
    /// 按作用域、状态和查询条件列出记忆。
    List {
        /// 只看这个作用域：personal / project:<id> / environment:<id>。
        #[arg(long)]
        scope: Option<String>,
        /// 只看这个状态：candidate / active / contested / superseded / forgotten。
        #[arg(long)]
        state: Option<String>,
        /// 最多列多少条。
        #[arg(long)]
        limit: Option<u32>,
    },
    /// 检索；支持 hybrid / keyword / vector（§9.4）。
    Search {
        /// 查询词。
        query: String,
        /// 检索模式：hybrid（默认，按配置）/ keyword / vector。
        #[arg(long)]
        mode: Option<String>,
        /// 只看这个作用域：personal / project:<id> / environment:<id>。
        #[arg(long)]
        scope: Option<String>,
        /// 只看这个状态。**明确写出来才查得到 contested**（§9.6）。
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// 内容、版本、证据与确认记录。
    Show {
        /// 记忆 ID。
        memory_id: String,
    },
    /// 操作者确认指定 revision。
    Confirm {
        /// 记忆 ID。
        memory_id: String,
        /// 预期 revision（§9.6：confirm / forget 都带着它来）。
        #[arg(long)]
        revision: u32,
    },
    /// 停用指定 revision 并失效索引；不删除 Memos 原文。
    Forget {
        /// 记忆 ID。
        memory_id: String,
        #[arg(long)]
        revision: u32,
    },
    /// 向量索引（§9.5）。
    Index {
        #[command(subcommand)]
        action: MemoryIndexCommand,
    },
}

#[derive(Subcommand)]
enum MemoryIndexCommand {
    /// 当前空间、进度、覆盖率和错误。
    Status,
    /// 幂等提交重建任务。
    Rebuild,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// 校验各模型、effort、向量参数、渠道名单及其他配置；只读。
    Check,
    /// 让 Gateway 立即重载配置；校验失败则保留旧配置并返回错误。
    Reload,
}

#[derive(Subcommand)]
enum ChannelCommand {
    /// 渠道清单。
    List,
    /// 连通性核对（飞书 tenant token、Telegram getMe、微信凭证文件）。
    Probe,
    /// 微信。
    Wechat {
        #[command(subcommand)]
        action: WechatCommand,
    },
}

#[derive(Subcommand)]
enum WechatCommand {
    /// 终端显示二维码完成微信登录，凭证写入数据目录。
    Login,
}

#[derive(Subcommand)]
enum ToolboxCommand {
    /// 列出模块：已启用的、只有候选的，都算。
    List,
    /// 看一个模块：当前版本、导出函数、候选与它的测试结果。
    Inspect {
        /// 模块名，`memos` 或 `toolbox.memos` 都收。
        module: String,
    },
    /// 跑候选自带的测试，结果记进候选的元数据（§5.4）。
    Test { module: String },
    /// 启用候选版本。**产生一条审批**，消息里带版本差异与测试结果（§7.1 第 4 行）。
    Enable {
        module: String,
        /// 只启用这一版；与当前候选对不上就拒绝（§5.4「校验候选哈希与已测版本一致」）。
        #[arg(long)]
        version: Option<String>,
    },
    /// 停用一个模块。正文移出 toolbox/，快照与元数据保留。
    Disable { module: String },
}

#[derive(Subcommand)]
enum SkillsCommand {
    /// 列出 Skills。
    List,
    /// 查看某个 SKILL.md。
    Inspect {
        /// Skill 名字。
        name: String,
    },
    /// 启用。
    Enable {
        /// Skill 名字。
        name: String,
    },
    /// 停用。
    Disable {
        /// Skill 名字。
        name: String,
    },
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // §13.4 reqwest 行：reqwest 用 `rustls-no-provider`，进程里必须由我们自己装上
    // ring provider。这一步必须发生在任何网络线程之前——特别是飞书 ws 线程启动之前，
    // 否则第一次握手会因为没有默认 provider 而 panic。
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("安装 rustls ring provider");

    let cli = Cli::parse();
    let home = connect::komo_home();

    let code = match dispatch(cli, &home).await {
        Ok(Some(text)) => {
            if !text.is_empty() {
                println!("{text}");
            }
            0
        }
        Ok(None) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    };
    std::process::exit(code);
}

/// 跑一条命令。`Ok(None)` = 这条命令自己管输出（TUI、前台 Gateway）。
async fn dispatch(cli: Cli, home: &Path) -> Result<Option<String>, String> {
    match cli.command {
        // `komo`：确保本机 Gateway 就绪，进入 TUI 聊天（§3）。**会话不在这里建**——
        // TUI 里发出第一条消息时才铸：看一眼、一个字没发就退出，不该在账本和磁盘上
        // 留下一个空壳会话。
        None => {
            init_tracing(None);
            let client = connect::connect_or_start(home).await?;
            run_tui(client, None, TuiMode::New)
                .await
                .map_err(|error| error.to_string())?;
            Ok(None)
        }
        Some(Command::Home) => {
            init_tracing(None);
            let client = connect::connect_or_start(home).await?;
            let discovery = komo_client::discovery::read_discovery_file(home)
                .map_err(|error| error.to_string())?;
            let base_url = client.base_url();
            let session =
                komo_gateway::http::fetch_home_session(&base_url, discovery.token.as_deref())
                    .await?;
            run_tui(client, Some(session.session), TuiMode::Home)
                .await
                .map_err(|error| error.to_string())?;
            Ok(None)
        }
        Some(Command::Resume { session_id }) => {
            init_tracing(None);
            let client = connect::connect_or_start(home).await?;
            run_tui(
                client,
                Some(SessionId::from_raw(session_id)),
                TuiMode::Resume,
            )
            .await
            .map_err(|error| error.to_string())?;
            Ok(None)
        }
        Some(Command::Gateway { foreground, action }) => gateway(home, foreground, action).await,
        Some(Command::Doctor { reconcile }) => commands::doctor(home, reconcile).await.map(Some),
        // 也不经 Gateway（§3）：它换的是磁盘上那份二进制，与在跑的那个进程无关。
        Some(Command::Update) => update::run(home).await.map(Some),
        // 三条不经 Gateway 的（§3）。
        Some(Command::Channel { action }) => match action {
            ChannelCommand::List => commands::channel_list(home).map(Some),
            ChannelCommand::Probe => commands::channel_probe(home).await.map(Some),
            ChannelCommand::Wechat { action } => match action {
                // 不经 Gateway（§3）：二维码打到 stderr，凭证写入数据目录 `wechat/credentials.json`
                // （0600）。返回值里不带 token / userId。
                WechatCommand::Login => {
                    let path = komo_gateway::channels::wechat::credentials_path(home);
                    let mut out = std::io::stderr();
                    komo_gateway::channels::wechat::login(&path, &mut out)
                        .await
                        .map(|_| Some(format!("微信登录成功，凭证已写入 {}", path.display())))
                        .map_err(|error| error.to_string())
                }
            },
        },
        Some(Command::Skills { action }) => {
            use commands::SkillsAction;
            let action = match &action {
                SkillsCommand::List => SkillsAction::List,
                SkillsCommand::Inspect { name } => SkillsAction::Inspect(name),
                SkillsCommand::Enable { name } => SkillsAction::Enable(name),
                SkillsCommand::Disable { name } => SkillsAction::Disable(name),
            };
            commands::skills(home, action).map(Some)
        }
        Some(Command::Toolbox { action }) => {
            let client = connect::connect_or_start(home).await?;
            // 这组端点不在 §13.1 的接口表里，所以 `KomoClient` 上没有它们；地址与令牌
            // 从发现文件读一次（`komo home` 已经是这个形状）。
            let discovery = komo_client::discovery::read_discovery_file(home)
                .map_err(|error| error.to_string())?;
            let base_url = client.base_url();
            let at = (base_url.as_str(), discovery.token.as_deref());
            match action {
                ToolboxCommand::List => commands::toolbox_list(at).await,
                ToolboxCommand::Inspect { module } => commands::toolbox_inspect(at, &module).await,
                ToolboxCommand::Test { module } => commands::toolbox_test(at, &module).await,
                ToolboxCommand::Enable { module, version } => {
                    commands::toolbox_enable(at, &module, version).await
                }
                ToolboxCommand::Disable { module } => commands::toolbox_disable(at, &module).await,
            }
            .map(Some)
        }
        // 其余都要一个在跑的 Gateway。
        other => {
            let client = connect::connect_or_start(home).await?;
            operator(other, &client).await.map(Some)
        }
    }
}

async fn operator(
    command: Option<Command>,
    client: &komo_client::KomoClient,
) -> Result<String, String> {
    let Some(command) = command else {
        return Err("没有这条命令".into());
    };
    match command {
        Command::Session { action } => match action {
            SessionCommand::List { all } => commands::session_list(client, all).await,
            SessionCommand::Delete { session_id, now } => {
                commands::session_delete(client, &session_id, now).await
            }
            SessionCommand::Purge { session_id } => {
                commands::session_purge(client, &session_id).await
            }
        },
        Command::Run { action } => match action {
            RunCommand::Inspect { run_id } => commands::run_inspect(client, &run_id).await,
            RunCommand::Cancel { run_id } => commands::run_cancel(client, &run_id).await,
        },
        Command::Intervention { action } => match action {
            InterventionCommand::List { session } => {
                commands::intervention_list(client, session.as_deref()).await
            }
            InterventionCommand::Show { handle } => {
                commands::intervention_show(client, &handle).await
            }
            InterventionCommand::Answer {
                handle,
                verdict,
                scope,
            } => commands::intervention_answer(client, &handle, &verdict, scope.as_deref()).await,
            InterventionCommand::AnswerAll { verdict } => {
                commands::intervention_answer_all(client, &verdict).await
            }
        },
        Command::Cron { action } => match action {
            CronCommand::Add(args) => {
                let CronAddArgs {
                    name,
                    schedule,
                    timezone,
                    prompt,
                    workdir,
                    model,
                    effort,
                    skills,
                    max_rounds,
                    overlap,
                    notify,
                } = *args;
                commands::cron_add(
                    client,
                    commands::CronAdd {
                        name,
                        schedule,
                        timezone,
                        prompt,
                        workdir,
                        model,
                        effort,
                        skills,
                        max_rounds,
                        overlap,
                        notify,
                    },
                )
                .await
            }
            CronCommand::List => commands::cron_list(client).await,
            CronCommand::Run { job_id } => commands::cron_run(client, &job_id).await,
            CronCommand::Pause { job_id } => {
                commands::cron_status(client, &job_id, komo_kernel::cron::JobStatus::Paused).await
            }
            CronCommand::Resume { job_id } => {
                commands::cron_status(client, &job_id, komo_kernel::cron::JobStatus::Active).await
            }
            CronCommand::Remove { job_id } => commands::cron_remove(client, &job_id).await,
        },
        Command::Memory { action } => match action {
            MemoryCommand::List {
                scope,
                state,
                limit,
            } => {
                commands::memory_list(
                    client,
                    commands::MemoryFilter {
                        query: None,
                        mode: None,
                        scope,
                        state,
                        limit,
                    },
                )
                .await
            }
            MemoryCommand::Search {
                query,
                mode,
                scope,
                state,
                limit,
            } => {
                commands::memory_list(
                    client,
                    commands::MemoryFilter {
                        query: Some(query),
                        mode,
                        scope,
                        state,
                        limit,
                    },
                )
                .await
            }
            MemoryCommand::Show { memory_id } => commands::memory_show(client, &memory_id).await,
            MemoryCommand::Confirm {
                memory_id,
                revision,
            } => commands::memory_confirm(client, &memory_id, revision).await,
            MemoryCommand::Forget {
                memory_id,
                revision,
            } => commands::memory_forget(client, &memory_id, revision).await,
            MemoryCommand::Index { action } => match action {
                MemoryIndexCommand::Status => commands::memory_index(client).await,
                MemoryIndexCommand::Rebuild => commands::memory_rebuild(client).await,
            },
        },
        Command::Config { action } => match action {
            ConfigCommand::Check => commands::config_check(client).await,
            ConfigCommand::Reload => commands::config_reload(client).await,
        },
        _ => Err("没有这条命令".into()),
    }
}

/// `komo gateway [--foreground] [status|stop|restart]`（§3）。
async fn gateway(
    home: &Path,
    foreground: bool,
    action: Option<GatewayCommand>,
) -> Result<Option<String>, String> {
    use komo_gateway::service::units;

    match action {
        // 前台运行：服务管理器起的就是这一种。
        None if foreground => {
            init_tracing(Some(home));
            komo_gateway::run(komo_gateway::ServiceOptions {
                home: Some(home.to_path_buf()),
                listen: None,
                channels: komo_gateway::channels::factories(),
                llm: None,
                embeddings: None,
            })
            .await
            .map_err(|error| error.to_string())?;
            Ok(None)
        }
        // 后台启动，等就绪后返回。
        None => {
            init_tracing(None);
            connect::request_start(home)?;
            let client = connect::wait_ready(home).await?;
            let health = client.health().await.map_err(|error| error.to_string())?;
            Ok(Some(format!(
                "Gateway 就绪：{}（实例 {}）",
                client.base_url(),
                health.instance_id
            )))
        }
        Some(GatewayCommand::Status) => match connect::connect(home).await {
            Ok(client) => {
                let health = client.health().await.map_err(|error| error.to_string())?;
                let managed = units::status().unwrap_or_else(|error| format!("（{error}）"));
                Ok(Some(format!(
                    "在跑：{}（实例 {}，启动于 {}）\n服务管理器：{}",
                    client.base_url(),
                    health.instance_id,
                    health.started_at,
                    managed.trim()
                )))
            }
            // **不隐式启动服务**（§3）。
            Err(error) => Ok(Some(format!("没在跑：{error}"))),
        },
        Some(GatewayCommand::Stop) => {
            units::stop().map_err(|error| error.to_string())?;
            Ok(Some("已请服务管理器停止 Gateway".into()))
        }
        Some(GatewayCommand::Restart) => {
            units::restart(&connect::user_home(), home).map_err(|error| error.to_string())?;
            let client = connect::wait_ready(home).await?;
            Ok(Some(format!("Gateway 已重启：{}", client.base_url())))
        }
    }
}

/// 日志：级别看 `KOMO_LOG`，前台 Gateway 同时写 `logs/gateway.log`。
fn init_tracing(gateway_home: Option<&Path>) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_env("KOMO_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let file = gateway_home.and_then(|home| {
        let dir = home.join("logs");
        std::fs::create_dir_all(&dir).ok()?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("gateway.log"))
            .ok()?;
        Some(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Arc::new(file)),
        )
    });

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr)
        .with(file)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// clap 的命令树自洽（重名、短选项冲突、必填与默认值打架都在这里挂）。
    #[test]
    fn the_command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    /// `komo toolbox ...` 的五条（§5.3、§5.4；**不在 §3 的命令表里**，见上面的 TODO）。
    #[test]
    fn the_toolbox_commands_parse() {
        assert!(matches!(
            Cli::parse_from(["komo", "toolbox", "list"]).command,
            Some(Command::Toolbox {
                action: ToolboxCommand::List
            })
        ));
        let Some(Command::Toolbox {
            action: ToolboxCommand::Inspect { module },
        }) = Cli::parse_from(["komo", "toolbox", "inspect", "memos"]).command
        else {
            panic!("解析不出 inspect");
        };
        assert_eq!(module, "memos");

        // `--version` 是"只启用这一版"，不是 clap 的版本号——它必须真的到得了参数里。
        let Some(Command::Toolbox {
            action: ToolboxCommand::Enable { module, version },
        }) = Cli::parse_from(["komo", "toolbox", "enable", "memos", "--version", "abc-1"]).command
        else {
            panic!("解析不出 enable");
        };
        assert_eq!(module, "memos");
        assert_eq!(version.as_deref(), Some("abc-1"));

        assert!(matches!(
            Cli::parse_from(["komo", "toolbox", "test", "memos"]).command,
            Some(Command::Toolbox {
                action: ToolboxCommand::Test { .. }
            })
        ));
        assert!(matches!(
            Cli::parse_from(["komo", "toolbox", "disable", "memos"]).command,
            Some(Command::Toolbox {
                action: ToolboxCommand::Disable { .. }
            })
        ));
        // 模块名是必填的：`komo toolbox enable` 不该对着"某个模块"生效。
        assert!(Cli::try_parse_from(["komo", "toolbox", "enable"]).is_err());
    }

    /// §3 的命令表里那几条要能解析出来。
    #[test]
    fn the_documented_commands_parse() {
        assert!(Cli::parse_from(["komo"]).command.is_none());
        assert!(matches!(
            Cli::parse_from(["komo", "home"]).command,
            Some(Command::Home)
        ));
        assert!(matches!(
            Cli::parse_from(["komo", "resume", "sess-1"]).command,
            Some(Command::Resume { .. })
        ));
        assert!(matches!(
            Cli::parse_from(["komo", "gateway", "--foreground"]).command,
            Some(Command::Gateway {
                foreground: true,
                action: None
            })
        ));
        assert!(matches!(
            Cli::parse_from(["komo", "gateway", "status"]).command,
            Some(Command::Gateway {
                action: Some(GatewayCommand::Status),
                ..
            })
        ));
        assert!(matches!(
            Cli::parse_from(["komo", "config", "check"]).command,
            Some(Command::Config {
                action: ConfigCommand::Check
            })
        ));
        assert!(matches!(
            Cli::parse_from(["komo", "update"]).command,
            Some(Command::Update)
        ));
    }

    /// `komo cron add` 的四个必填项。
    #[test]
    fn cron_add_requires_a_schedule_and_a_prompt() {
        let parsed = Cli::parse_from([
            "komo",
            "cron",
            "add",
            "--name",
            "morning",
            "--schedule",
            "0 9 * * *",
            "--timezone",
            "Asia/Shanghai",
            "--prompt",
            "整理今天的动态",
        ]);
        let Some(Command::Cron {
            action: CronCommand::Add(args),
        }) = parsed.command
        else {
            panic!("解析不出 cron add");
        };
        assert_eq!(args.name, "morning");
        assert_eq!(args.schedule, "0 9 * * *");
        assert_eq!(args.timezone, "Asia/Shanghai");
        // 没写的那些是默认值，不是空——`notify` 缺省是 `always`（§10）。
        assert_eq!(args.notify, "always");
        assert_eq!(args.overlap, "skip");
        assert!(args.model.is_none());
        assert!(args.skills.is_empty());

        // 少一个必填项就该失败，而不是用一个猜出来的默认值跑起来。
        assert!(Cli::try_parse_from(["komo", "cron", "add", "--name", "x"]).is_err());
    }

    /// §10 的 Job 字段在 CLI 上**全都有 flag**：`komo cron add` 是唯一的写入口
    /// （「模型需要管理 Cron 时通过 shell 调这些命令」），少一个 flag 就等于那个字段
    /// 只能改配置文件——而它根本不在配置文件里。
    #[test]
    fn cron_add_covers_every_job_field() {
        let parsed = Cli::parse_from([
            "komo",
            "cron",
            "add",
            "--name",
            "morning",
            "--schedule",
            "0 9 * * *",
            "--timezone",
            "Asia/Shanghai",
            "--prompt",
            "整理",
            "--workdir",
            "/tmp",
            "--model",
            "job-model",
            "--effort",
            "high",
            "--skill",
            "memos",
            "--skill",
            "search",
            "--max-rounds",
            "12",
            "--overlap",
            "allow",
            "--notify",
            "on_error",
        ]);
        let Some(Command::Cron {
            action: CronCommand::Add(args),
        }) = parsed.command
        else {
            panic!("解析不出 cron add");
        };
        assert_eq!(args.workdir.as_deref(), Some("/tmp"));
        assert_eq!(args.model.as_deref(), Some("job-model"));
        assert_eq!(args.effort.as_deref(), Some("high"));
        assert_eq!(args.skills, vec!["memos".to_string(), "search".to_string()]);
        assert_eq!(args.max_rounds, Some(12));
        assert_eq!(args.overlap, "allow");
        assert_eq!(args.notify, "on_error");
    }
}
