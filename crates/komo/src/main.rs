//! komo 二进制：clap 分发（docs/komo_bot.md §3 命令表、§13.4）。
//!
//! 这一层只做分发：`komo` / `komo home` / `komo resume` 与操作子命令走 komo-client，
//! `komo gateway` 走 komo-gateway。**没有第二份业务逻辑**——每条命令要么打一个 HTTP
//! 接口，要么读一次配置文件。

mod commands;
mod connect;
mod telemetry;
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
    /// codemode 的脚本 worker：Gateway 为每次 codemode 调用起一个，stdio 上说行 JSON
    /// （`docs/codemode.md` §3）。不给人用，不进 `--help`。
    #[command(name = "__codemode-worker", hide = true)]
    CodemodeWorker,
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
    /// 待处理清单与答复：结果不明、阻塞两类一张表，一次答一条（§7.5）。
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
    /// 在数据目录生成一份带注释的最小 config.toml 与 .env 模板；已有 config.toml 不覆盖（§3）。
    ///
    /// 不经 Gateway。生成后填 `.env` 里的 API key，再 `komo config check`。
    Init,
    /// 校验或重载配置（§3）。
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },
    /// MCP 服务器（docs/mcp.md）。
    Mcp {
        #[command(subcommand)]
        action: McpCommand,
    },
    /// 渠道清单、连通性核对与微信登录；不经 Gateway（§3）。
    Channel {
        #[command(subcommand)]
        action: ChannelCommand,
    },
    /// 账号 OAuth 登录；不经 Gateway，凭证写入数据目录（§3、§13.3）。
    Auth {
        #[command(subcommand)]
        action: AuthCommand,
    },
    /// Skills 目录；只读文件系统，不经 Gateway（§5.6）。
    Skills {
        #[command(subcommand)]
        action: SkillsCommand,
    },
    /// 显示当前生效配置的加载时间与来源文件 mtime，以及上一次校验错误（§3）。
    ///
    /// `--reconcile` 顺带跑一次对账（§8.9：只读观察 + 只写状态，不调工具）。
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
    /// 把一个会话导出成 pi v3 树形 JSONL（带 blob 引用与当前位置）；默认写 stdout。
    Export {
        /// 会话 ID。
        session_id: String,
        /// 写到这个文件，而不是 stdout。
        #[arg(short, long, value_name = "FILE")]
        output: Option<std::path::PathBuf>,
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
    Stop {
        /// 先等这个进程退出再动手（Gateway 自己的 shell 里跑 stop 时由它起的后台进程用）。
        #[arg(long, hide = true)]
        after_pid: Option<u32>,
    },
    /// 重启后台进程。
    Restart {
        /// 同 `stop --after-pid`。
        #[arg(long, hide = true)]
        after_pid: Option<u32>,
    },
}

#[derive(Subcommand)]
enum InterventionCommand {
    /// 待处理清单：结果不明、阻塞两类一张表（§7.5）。
    List {
        /// 只看某个会话的。
        #[arg(long)]
        session: Option<String>,
    },
    /// 单项详情：结果不明多一次调用的名字与计划哈希，阻塞印前提为什么没了。
    Show {
        /// 句柄：Run ID。
        handle: String,
    },
    /// 答复一条。结论按种类分派（§7.5）：结果不明 `satisfied` / `not_performed` /
    /// `abandon`，阻塞 `resolve` / `abandon`。没有批量。
    Answer {
        /// 句柄：Run ID。
        handle: String,
        /// 结论词。
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
    /// 与 `--command` 二选一：交给模型的任务描述。
    #[arg(long, conflicts_with = "command")]
    prompt: Option<String>,
    /// 命令直跑模式：不经模型，触发时固定跑这一条 shell 命令（§10）。与 `--prompt`
    /// 二选一。
    #[arg(long, conflicts_with = "prompt")]
    command: Option<String>,
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
    /// 结果投递：always（默认）/ on_error / never。停着等人（结果不明 / 阻塞）**不受它约束**。
    #[arg(long, default_value = "always")]
    notify: String,
}

#[derive(Subcommand)]
enum MemoryCommand {
    /// 按作用域、状态和查询条件列出记忆。
    List {
        /// 只看这个作用域：personal / project:<id> / environment:<id> / agent:<id>。
        #[arg(long)]
        scope: Option<String>,
        /// 只看这个状态：candidate / active / contested / superseded / forgotten。
        #[arg(long)]
        state: Option<String>,
        /// 最多列多少条。
        #[arg(long)]
        limit: Option<u32>,
        /// 一条一行、制表符分隔、正文不截断：`id@版本 状态 来源 确认 用量 使用方式 条件 正文`。
        #[arg(long)]
        full: bool,
    },
    /// 检索；支持 hybrid / keyword / vector（§9.4）。
    Search {
        /// 查询词。
        query: String,
        /// 检索模式：hybrid（默认，按配置）/ keyword / vector。
        #[arg(long)]
        mode: Option<String>,
        /// 只看这个作用域：personal / project:<id> / environment:<id> / agent:<id>。
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
    /// 手动记一条。在终端里跑记成已确认；Agent 在 Run 里跑（带 KOMO_SESSION / KOMO_RUN）
    /// 记成未确认，证据是这个 Run 里用户说的话，等你 confirm。
    Add {
        /// 一句简短、只说一件事的陈述。
        content: String,
        /// 种类：preference（默认）/ fact / experience。
        #[arg(long, default_value = "preference")]
        kind: String,
        /// 作用域：personal（默认）/ project:<id> / environment:<id>。
        #[arg(long, default_value = "personal")]
        scope: String,
        /// 使用方式：convention（长期约定）/ reference（线索）。省略 = reference；
        /// convention 只给 preference，且必须带 --applies-when。
        #[arg(long)]
        application: Option<String>,
        /// 约定适用的条件，一句能独立理解的话，如「创建发布工单、需要选 Platform 时」。
        #[arg(long)]
        applies_when: Option<String>,
    },
    /// 操作者给指定 revision 定使用方式（约定 / 线索）与条件，写成新 revision；证据与确认不变。
    Classify {
        /// 记忆 ID。
        memory_id: String,
        /// 预期 revision。
        #[arg(long)]
        revision: u32,
        /// 使用方式：convention / reference。convention 只给 preference，且要有条件。
        #[arg(long)]
        application: String,
        /// 条件；省略 = 沿用现有条件。
        #[arg(long)]
        applies_when: Option<String>,
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
    /// 撤回一次遗忘或争议，回到生效；版本号不变。
    Restore {
        /// 记忆 ID。
        memory_id: String,
        #[arg(long)]
        revision: u32,
    },
    /// 按规则遗忘：过期、久未使用、从没用过；路径失效的标为有冲突。已确认的不动。默认只预演。
    Sweep {
        /// 真的执行；不加只列出会做什么。
        #[arg(long)]
        apply: bool,
        /// 入库超过这么多天、一次都没被用过的，遗忘。
        #[arg(long, default_value_t = 60)]
        never_used_days: u32,
        /// 最近这么多天没被用过的，遗忘。
        #[arg(long, default_value_t = 90)]
        idle_days: u32,
    },
    /// 离线整理（§9.8）：对一个作用域里已有的记忆去重、取代、标冲突，不新增正文。默认只预演。
    Consolidate {
        /// 只整理这个作用域：personal / project:<id> / environment:<id> / agent:<id>；不给就逐个作用域跑。
        #[arg(long)]
        scope: Option<String>,
        /// 只整理这个种类：preference / fact / experience。
        #[arg(long)]
        kind: Option<String>,
        /// 给模型的软提示（比如"这两个名字是同一个项目"）；不改变任何授权规则。
        #[arg(long)]
        instruction: Option<String>,
        /// 真的执行；不加只列出现在跑会怎样。
        #[arg(long)]
        apply: bool,
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
enum McpCommand {
    /// 让 Gateway 断开重连 MCP 服务器，等握手完报各自的状态；不用重启 Gateway（docs/mcp.md §2.5）。
    Reconnect {
        /// `[mcp.servers.<name>]` 里的名字；不写就是全部。
        servers: Vec<String>,
    },
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
enum AuthCommand {
    /// Codex 模型走的 ChatGPT 账号 OAuth（§13.3）。
    Codex {
        #[command(subcommand)]
        action: CodexAuthCommand,
    },
    /// `oauth = true` 的 MCP 服务器（docs/mcp.md §2.1）。
    Mcp {
        #[command(subcommand)]
        action: McpAuthCommand,
    },
}

#[derive(Subcommand)]
enum McpAuthCommand {
    /// 打开浏览器授权（授权码 + PKCE），凭证写进数据目录；在跑的 Gateway 下一条消息时自动连上。
    Login {
        /// `[mcp.servers.<name>]` 里的名字。
        server: String,
    },
    /// 各 OAuth 服务器登录了没有、token 何时到期、能否自动刷新；不打印 token。
    Status,
}

#[derive(Subcommand)]
enum CodexAuthCommand {
    /// 设备码登录：显示配对码与网址，轮询直到确认或超时（约 10 分钟）。
    Login,
    /// 账号、邮箱、套餐与过期时间；不打印 token。
    Status,
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
        Some(Command::CodemodeWorker) => std::process::exit(komo_runtime::codemode::worker::run()),
        // `komo`：确保本机 Gateway 就绪，进入 TUI 聊天（§3）。**会话不在这里建**——
        // TUI 里发出第一条消息时才铸：看一眼、一个字没发就退出，不该在账本和磁盘上
        // 留下一个空壳会话。
        None => {
            init_tracing(None, None);
            let client = connect::connect_or_start(home).await?;
            run_tui(client, None, TuiMode::New)
                .await
                .map_err(|error| error.to_string())?;
            Ok(None)
        }
        Some(Command::Home) => {
            init_tracing(None, None);
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
            init_tracing(None, None);
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
        // 不经 Gateway（§3）：Gateway 起来要先有配置，这条命令就是生成那份配置。
        Some(Command::Init) => commands::init(home).map(Some),
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
        // 不经 Gateway（§3、§13.3）：设备码登录直接落盘，同微信登录的先例。
        Some(Command::Auth { action }) => match action {
            AuthCommand::Codex { action } => match action {
                CodexAuthCommand::Login => {
                    let path = komo_gateway::codex_auth::credentials_path(home);
                    let mut out = std::io::stderr();
                    komo_gateway::codex_auth::login(&path, &mut out)
                        .await
                        .map(|creds| {
                            Some(format!(
                                "ChatGPT 登录成功，凭证已写入 {}（账号 {}）",
                                path.display(),
                                creds.account_id
                            ))
                        })
                        .map_err(|error| error.to_string())
                }
                CodexAuthCommand::Status => commands::auth_codex_status(home).map(Some),
            },
            AuthCommand::Mcp { action } => match action {
                McpAuthCommand::Login { server } => {
                    commands::auth_mcp_login(home, &server).await.map(Some)
                }
                McpAuthCommand::Status => commands::auth_mcp_status(home).map(Some),
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
            SessionCommand::Export { session_id, output } => {
                commands::session_export(client, &session_id, output.as_deref()).await
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
            InterventionCommand::Answer { handle, verdict } => {
                commands::intervention_answer(client, &handle, &verdict).await
            }
        },
        Command::Cron { action } => match action {
            CronCommand::Add(args) => {
                let CronAddArgs {
                    name,
                    schedule,
                    timezone,
                    prompt,
                    command,
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
                        command,
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
                full,
            } => {
                commands::memory_list(
                    client,
                    commands::MemoryFilter {
                        query: None,
                        mode: None,
                        scope,
                        state,
                        limit,
                        full,
                        search: false,
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
                        full: false,
                        search: true,
                    },
                )
                .await
            }
            MemoryCommand::Show { memory_id } => commands::memory_show(client, &memory_id).await,
            MemoryCommand::Add {
                content,
                kind,
                scope,
                application,
                applies_when,
            } => {
                commands::memory_add(
                    client,
                    &content,
                    &kind,
                    &scope,
                    application.as_deref(),
                    applies_when.as_deref(),
                )
                .await
            }
            MemoryCommand::Classify {
                memory_id,
                revision,
                application,
                applies_when,
            } => {
                commands::memory_classify(
                    client,
                    &memory_id,
                    revision,
                    &application,
                    applies_when.as_deref(),
                )
                .await
            }
            MemoryCommand::Confirm {
                memory_id,
                revision,
            } => commands::memory_confirm(client, &memory_id, revision).await,
            MemoryCommand::Forget {
                memory_id,
                revision,
            } => commands::memory_forget(client, &memory_id, revision).await,
            MemoryCommand::Restore {
                memory_id,
                revision,
            } => commands::memory_restore(client, &memory_id, revision).await,
            MemoryCommand::Sweep {
                apply,
                never_used_days,
                idle_days,
            } => commands::memory_sweep(client, apply, never_used_days, idle_days).await,
            MemoryCommand::Consolidate {
                scope,
                kind,
                instruction,
                apply,
            } => commands::memory_consolidate(client, scope, kind, instruction, apply).await,
            MemoryCommand::Index { action } => match action {
                MemoryIndexCommand::Status => commands::memory_index(client).await,
                MemoryIndexCommand::Rebuild => commands::memory_rebuild(client).await,
            },
        },
        Command::Config { action } => match action {
            ConfigCommand::Check => commands::config_check(client).await,
            ConfigCommand::Reload => commands::config_reload(client).await,
        },
        Command::Mcp { action } => match action {
            McpCommand::Reconnect { servers } => commands::mcp_reconnect(client, servers).await,
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
            // OTLP 端点是启动期键：在装 tracing 之前读，exporter 随 subscriber 一起建好。
            // 读不出配置不在这里报——`run` 自己会带着原因失败。
            let endpoint =
                komo_gateway::config::load_config(&komo_gateway::config::LoadOptions::at(home))
                    .ok()
                    .and_then(|loaded| loaded.snapshot.start_only.otlp_endpoint.clone());
            let status = std::sync::Arc::new(komo_gateway::TelemetryStatus::new(endpoint.clone()));
            let telemetry = endpoint.as_deref().map(|endpoint| {
                telemetry::Telemetry::build(endpoint, &host_name(), std::sync::Arc::clone(&status))
            });
            let (telemetry, build_error) = match telemetry {
                Some(Ok(telemetry)) => (Some(telemetry), None),
                Some(Err(error)) => (None, Some(error)),
                None => (None, None),
            };
            init_tracing(Some(home), telemetry.as_ref());
            if let Some(error) = build_error {
                tracing::warn!(%error, "遥测导出没建起来，本次运行不上报");
            }
            let outcome = komo_gateway::run(komo_gateway::ServiceOptions {
                home: Some(home.to_path_buf()),
                listen: None,
                channels: komo_gateway::channels::factories(),
                llm: None,
                stream: None,
                embeddings: None,
                // 生产上用当前用户的家目录：`~/.agents/skills` 是与其他本地 agent 共享的
                // 那一份（§5.6）。
                shared_home: None,
                telemetry: telemetry.as_ref().map(|_| status),
            })
            .await
            .map_err(|error| error.to_string());
            if let Some(telemetry) = telemetry {
                // SDK 的线程要 join、blocking reqwest 不能在 async 上下文里 drop。
                let _ = tokio::task::spawn_blocking(move || telemetry.shutdown()).await;
            }
            outcome?;
            Ok(None)
        }
        // 后台启动，等就绪后返回。
        None => {
            init_tracing(None, None);
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
                    "在跑：{}（实例 {}，启动于 {}）\n服务管理器：{}{}",
                    client.base_url(),
                    health.instance_id,
                    health.started_at,
                    managed.trim(),
                    // 单元名是全局的：**单元在服务哪个数据目录**与"这个命令连的是哪一个"
                    // 是两件事，不一致时正是出事时的样子（见 `units::owns_unit`）。
                    units::installed_home(&connect::user_home())
                        .map(|installed| if units::same_home(&installed, home) {
                            format!("（单元服务 {}）", installed.display())
                        } else {
                            format!("（单元服务 {}，**不是**这个数据目录）", installed.display())
                        })
                        .unwrap_or_else(|| "（还没装单元）".to_string())
                )))
            }
            // **不隐式启动服务**（§3）。
            Err(error) => Ok(Some(format!("没在跑：{error}"))),
        },
        Some(GatewayCommand::Stop { after_pid }) => {
            if after_pid.is_none() && units::inside_gateway() {
                let by = units::defer(&connect::user_home(), home, units::Deferred::Stop)
                    .map_err(|error| error.to_string())?;
                return Ok(Some(format!(
                    "这条命令跑在 Gateway 自己里面：等它结束、结果记进会话之后再停（{by}）。\
                     停下之后要在终端里跑 `komo gateway` 才会再起来"
                )));
            }
            wait_for_caller(after_pid);
            units::stop(&connect::user_home(), home).map_err(|error| error.to_string())?;
            Ok(Some("已请服务管理器停止 Gateway".into()))
        }
        Some(GatewayCommand::Restart { after_pid }) => {
            if after_pid.is_none() && units::inside_gateway() {
                let by = units::defer(&connect::user_home(), home, units::Deferred::Restart)
                    .map_err(|error| error.to_string())?;
                return Ok(Some(format!(
                    "这条命令跑在 Gateway 自己里面：等它结束、结果记进会话之后再重启（{by}）。\
                     重启时事件订阅会断开几秒，之后自己重连"
                )));
            }
            wait_for_caller(after_pid);
            units::restart(&connect::user_home(), home).map_err(|error| error.to_string())?;
            let client = connect::wait_ready(home).await?;
            Ok(Some(format!("Gateway 已重启：{}", client.base_url())))
        }
    }
}

/// `--after-pid`：先等发起的那条命令退出，再留一点时间让 Gateway 把那次调用的结果落盘、
/// 把那条 Run 收尾——这正是推迟的目的（`units::defer`）。
fn wait_for_caller(after_pid: Option<u32>) {
    if let Some(pid) = after_pid {
        komo_gateway::service::units::wait_for_exit(pid, std::time::Duration::from_secs(30));
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
}

/// 实例标识：本机主机名（Mac 与 NAS 两个实例分得开）。取不到就是 `unknown`。
fn host_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// 日志：级别看 `KOMO_LOG`。Gateway 只写一份 `logs/gateway.log.<日期>`（按天轮转，留 7 个）；
/// stderr 层在 Gateway 里只有 stderr 是终端时才开——服务管理器会把 stderr 收进它自己的
/// 文件，再开就是同一行写两份。文件开不出来时退回 stderr，不至于一行日志都没有。
/// 文本日志（文件、stderr）的过滤：级别看 `KOMO_LOG`，并且**永远不含** `komo::metrics`
/// 指标事件——它们只给 MetricsLayer（`telemetry::export_filter`）。
fn log_filter() -> tracing_subscriber::EnvFilter {
    let filter = tracing_subscriber::EnvFilter::try_from_env("KOMO_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    exclude_metrics(filter)
}

fn exclude_metrics(filter: tracing_subscriber::EnvFilter) -> tracing_subscriber::EnvFilter {
    filter.add_directive("komo::metrics=off".parse().expect("静态指令"))
}

fn init_tracing(gateway_home: Option<&Path>, telemetry: Option<&telemetry::Telemetry>) {
    use std::io::IsTerminal;
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let file = gateway_home.and_then(|home| {
        let appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("gateway.log")
            .max_log_files(7)
            .build(home.join("logs"))
            .ok()?;
        Some(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(appender)
                .with_filter(log_filter()),
        )
    });
    let stderr = (gateway_home.is_none() || file.is_none() || std::io::stderr().is_terminal())
        .then(|| {
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(log_filter())
        });
    let otel = telemetry.map(|telemetry| {
        telemetry
            .trace_layer()
            .with_filter(telemetry::export_filter())
    });
    let metrics = telemetry.map(|telemetry| {
        telemetry
            .metrics_layer()
            .with_filter(telemetry::export_filter())
    });

    let _ = tracing_subscriber::registry()
        .with(stderr)
        .with(file)
        .with(otel)
        .with(metrics)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// 指标事件（`komo::metrics`，DEBUG）不进文本日志：哪怕 `KOMO_LOG=debug`。
    #[test]
    fn metrics_events_stay_out_of_the_text_log() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::Layer as _;
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let sink = buf.clone();
        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .with_filter(exclude_metrics(tracing_subscriber::EnvFilter::new("debug")));
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            tracing::event!(target: "komo::metrics", tracing::Level::DEBUG, histogram.x_seconds = 0.5_f64, "m");
            tracing::debug!("plain-debug-line");
        });
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(text.contains("plain-debug-line"), "{text}");
        assert!(
            !text.contains("komo::metrics") && !text.contains("x_seconds"),
            "{text}"
        );
    }

    /// clap 的命令树自洽（重名、短选项冲突、必填与默认值打架都在这里挂）。
    #[test]
    fn the_command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    /// `stop` / `restart` 的 `--after-pid` 只给 Gateway 自己的 shell 里起的后台进程用：
    /// 不带它就是平常那一条。
    #[test]
    fn restart_and_stop_take_a_hidden_after_pid() {
        let Some(Command::Gateway {
            action: Some(GatewayCommand::Restart { after_pid }),
            ..
        }) = Cli::parse_from(["komo", "gateway", "restart"]).command
        else {
            panic!("解析不出 restart");
        };
        assert_eq!(after_pid, None);
        let Some(Command::Gateway {
            action: Some(GatewayCommand::Stop { after_pid }),
            ..
        }) = Cli::parse_from(["komo", "gateway", "stop", "--after-pid", "42"]).command
        else {
            panic!("解析不出 stop --after-pid");
        };
        assert_eq!(after_pid, Some(42));
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
        assert!(matches!(
            Cli::parse_from(["komo", "init"]).command,
            Some(Command::Init)
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

    /// `komo mcp reconnect` 不点名就是全部，点名可以点几台。
    #[test]
    fn mcp_reconnect_takes_zero_or_more_servers() {
        let servers = |args: &[&str]| match Cli::parse_from(args).command {
            Some(Command::Mcp {
                action: McpCommand::Reconnect { servers },
            }) => servers,
            _ => panic!("解析不出 mcp reconnect"),
        };
        assert!(servers(&["komo", "mcp", "reconnect"]).is_empty());
        assert_eq!(
            servers(&["komo", "mcp", "reconnect", "devops-mcp-prod", "cloudflare"]),
            vec!["devops-mcp-prod", "cloudflare"]
        );
    }

    /// §10 的 Job 字段在 CLI 上**全都有 flag**：`komo cron add` 是唯一的写入口
    /// （「模型需要管理 Cron 时通过 bash 调这些命令」），少一个 flag 就等于那个字段
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

    /// 命令直跑模式（§10）：`--command` 与 `--prompt` 二选一，clap 在解析这一步就挡住
    /// "两个都给"——不必等到发请求才发现。
    #[test]
    fn cron_add_command_and_prompt_are_mutually_exclusive_at_parse_time() {
        let parsed = Cli::parse_from([
            "komo",
            "cron",
            "add",
            "--name",
            "backup",
            "--schedule",
            "0 3 * * *",
            "--command",
            "tar czf /tmp/backup.tgz /data",
        ]);
        let Some(Command::Cron {
            action: CronCommand::Add(args),
        }) = parsed.command
        else {
            panic!("解析不出 cron add");
        };
        assert_eq!(
            args.command.as_deref(),
            Some("tar czf /tmp/backup.tgz /data")
        );
        assert!(args.prompt.is_none());

        assert!(
            Cli::try_parse_from([
                "komo",
                "cron",
                "add",
                "--name",
                "both",
                "--schedule",
                "0 9 * * *",
                "--prompt",
                "整理",
                "--command",
                "echo hi",
            ])
            .is_err(),
            "两个都给，clap 该在解析这一步就拒绝"
        );
    }
}
