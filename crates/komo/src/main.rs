//! komo 二进制：clap 分发（docs/komo_bot.md §3 命令表、§13.4）。
//!
//! 这一层只做分发：`komo` / `komo web` 与操作子命令走 komo-client，
//! `komo gateway` 走 komo-gateway。**没有第二份业务逻辑**——每条命令要么打一个 HTTP
//! 接口，要么读一次配置文件。

mod commands;
mod connect;
mod login;
mod matters;
mod telemetry;
mod update;

use std::path::Path;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "komo", version, about = "komo：个人 Agent 框架", long_about = None)]
struct Cli {
    /// 不带子命令时：确保本机 Gateway 就绪，再做 `komo web` 做的事（§3）。
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// codemode 的脚本 worker：Gateway 为每次 codemode 调用起一个，stdio 上说行 JSON
    /// （`docs/codemode.md` §3）。不给人用，不进 `--help`。
    #[command(name = "__codemode-worker", hide = true)]
    CodemodeWorker,
    /// 在浏览器里打开网页版：打开 Gateway 的地址，没登录时网页会跳去 OIDC 登录（`docs/redesign.md` §2.9）。
    ///
    /// 不再有配对码。Gateway 没配 `[auth.oidc]` 时网页登不了（`komo config check` 会提示）。
    Web {
        /// 只打印链接，不打开浏览器。
        #[arg(long)]
        no_open: bool,
    },
    /// 登录远程 Gateway：跳浏览器走 OIDC，换一个设备令牌存在数据目录的 `remote.json`；之后的命令连它。
    ///
    /// 机器上连本机 Gateway 不需要登录（本机令牌）。
    Login {
        /// 远程 Gateway 的根地址，如 `https://komo.example.com`。
        url: String,
        /// 在设备清单里显示的名字。
        #[arg(long)]
        name: Option<String>,
        /// 只打印登录链接，不打开浏览器。
        #[arg(long)]
        no_open: bool,
    },
    /// 退出远程 Gateway：吊销设备令牌并删掉 `remote.json`，命令回到连本机。
    Logout,
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
    /// 监听清单（`monitor` 工具挂的、转后台的 bash）；只读，停监听用 monitor_stop 或删会话（docs/monitor.md）。
    Monitor {
        #[command(subcommand)]
        action: MonitorCommand,
    },
    /// 查看自动记忆、来源与确认状态（§9）。
    Memory {
        #[command(subcommand)]
        action: MemoryCommand,
    },
    /// 目标：有结果、会结束的事；下一步、等待、搁置、完成 / 放弃（docs/goals.md §4）。
    Goal {
        #[command(subcommand)]
        action: matters::GoalCommand,
    },
    /// 领域：没有终点、要维持标准的职责（docs/goals.md §4）。
    Area {
        #[command(subcommand)]
        action: matters::AreaCommand,
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
    /// 查看会话列表和状态（最新的在前，一次一页）；`--all` 才列出已逻辑删除的（§8.10）。
    List {
        /// 把已逻辑删除（closing / deleted）的会话也列出来。
        #[arg(long)]
        all: bool,
        /// 一页几条；0 = 不分页，一次列完。
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// 从上一页末尾印出的位置接着列。
        #[arg(long)]
        cursor: Option<String>,
    },
    /// 在各会话的标题与正文里找关键词（空白分开的几个词要在同一条消息里），给出会话、片段与能接着查的 ID。
    Search {
        /// 关键词。
        query: String,
        /// 最多给几个会话（默认 10，上限 50）。
        #[arg(long)]
        limit: Option<u32>,
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
enum MonitorCommand {
    /// 列出还在跑的监听：短号、已跑多久、距截止多久、标题、挂它的会话与脚本。
    List {
        /// 带上已结束的（登记里留着的最近几十条）。
        #[arg(long)]
        all: bool,
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
enum MemoryCommand {
    /// 按作用域、状态和查询条件列出记忆。
    List {
        /// 只看这个作用域：personal / project:<id> / environment:<id> / agent:<id>。
        #[arg(long)]
        scope: Option<String>,
        /// 只看这个状态：candidate / active / contested / superseded / forgotten / archived。
        #[arg(long)]
        state: Option<String>,
        /// 带上随事项结案归档的条目（默认不列；`--state archived` 明确问时不受限）。
        #[arg(long)]
        include_archived: bool,
        /// 最多列多少条。
        #[arg(long)]
        limit: Option<u32>,
        /// 一条一行、制表符分隔、正文不截断：`id@版本 状态 来源 确认 用量 使用方式 条件 正文`。
        #[arg(long)]
        full: bool,
    },
    /// 检索；支持 hybrid / keyword / vector（§9.4）。`id@版本` 后一列是相关度分数：关键词臂对照
    /// `min_keyword_score`，余弦对照 `min_similarity`。
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
        /// 把随事项结案归档的条目也查出来（默认不带）。
        #[arg(long)]
        include_archived: bool,
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
        /// 为哪件事记的：目标或领域的 ID（`komo goal list` / `komo area list`）；必须存在且未结案。
        #[arg(long = "for")]
        belongs_to: Option<String>,
    },
    /// 操作者给指定 revision 定使用方式（约定 / 线索）、条件与归属，写成新 revision；证据与确认不变。
    Classify {
        /// 记忆 ID。
        memory_id: String,
        /// 预期 revision。
        #[arg(long)]
        revision: u32,
        /// 使用方式：convention / reference。convention 只给 preference，且要有条件；
        /// 省略 = 沿用现有分类（只改归属时）。
        #[arg(long)]
        application: Option<String>,
        /// 条件；省略 = 沿用现有条件。
        #[arg(long)]
        applies_when: Option<String>,
        /// 改挂到这件事项（ID，必须存在且未结案），或 `none` 清空归属。归档的条目改挂到进行中
        /// 的事或清空归属后回到生效。
        #[arg(long = "for")]
        belongs_to: Option<String>,
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

/// 裸 `komo` 与 `komo web` 是同一件事：终端里没有对话入口，对话在网页（与渠道）里。
fn command_of(cli: Cli) -> Command {
    cli.command.unwrap_or(Command::Web { no_open: false })
}

/// `komo web`：确保 Gateway 就绪，打开它的地址（登录由网页走 OIDC）。
async fn web(home: &Path, no_open: bool) -> Result<Option<String>, String> {
    init_tracing(None, None);
    let client = connect::connect_or_start(home).await?;
    let url = client.base_url();
    if !no_open && !commands::open_in_browser(&url) {
        eprintln!("没能自动打开浏览器，请手动打开下面的链接。");
    }
    Ok(Some(format!(
        "{url}\n（没登录时网页会跳去身份提供方；机器上没有浏览器就从自己的电脑打开 Gateway 的公开地址）"
    )))
}

/// 跑一条命令。`Ok(None)` = 这条命令自己管输出（前台 Gateway）。
async fn dispatch(cli: Cli, home: &Path) -> Result<Option<String>, String> {
    match command_of(cli) {
        Command::CodemodeWorker => std::process::exit(komo_tools::codemode::worker::run()),
        Command::Web { no_open } => web(home, no_open).await,
        // 不经本机 Gateway：登录与退出都直接对远程那台说话。
        Command::Login { url, name, no_open } => login::login(home, &url, name.as_deref(), no_open)
            .await
            .map(Some),
        Command::Logout => login::logout(home).await.map(Some),
        Command::Gateway { foreground, action } => gateway(home, foreground, action).await,
        // 不经 Gateway（§3）：Gateway 起来要先有配置，这条命令就是生成那份配置。
        Command::Init => commands::init(home).map(Some),
        Command::Doctor { reconcile } => commands::doctor(home, reconcile).await.map(Some),
        // 也不经 Gateway（§3）：它换的是磁盘上那份二进制，与在跑的那个进程无关。
        Command::Update => update::run(home).await.map(Some),
        // 不经 Gateway（§3、§13.3）：设备码登录直接落盘，落盘。
        Command::Auth { action } => match action {
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
        Command::Skills { action } => {
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

async fn operator(command: Command, client: &komo_client::KomoClient) -> Result<String, String> {
    match command {
        Command::Session { action } => match action {
            SessionCommand::List { all, limit, cursor } => {
                commands::session_list(client, all, limit, cursor).await
            }
            SessionCommand::Search { query, limit } => {
                commands::session_search(client, &query, limit).await
            }
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
        Command::Memory { action } => match action {
            MemoryCommand::List {
                scope,
                state,
                include_archived,
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
                        include_archived,
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
                include_archived,
                limit,
            } => {
                commands::memory_list(
                    client,
                    commands::MemoryFilter {
                        query: Some(query),
                        mode,
                        scope,
                        state,
                        include_archived,
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
                belongs_to,
            } => {
                commands::memory_add(
                    client,
                    &content,
                    &kind,
                    &scope,
                    application.as_deref(),
                    applies_when.as_deref(),
                    belongs_to.as_deref(),
                )
                .await
            }
            MemoryCommand::Classify {
                memory_id,
                revision,
                application,
                applies_when,
                belongs_to,
            } => {
                commands::memory_classify(
                    client,
                    &memory_id,
                    revision,
                    application.as_deref(),
                    applies_when.as_deref(),
                    belongs_to.as_deref(),
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
        Command::Monitor { action } => match action {
            MonitorCommand::List { all } => commands::monitor_list(client, all).await,
        },
        Command::Goal { action } => matters::goal(client, action).await,
        Command::Area { action } => matters::area(client, action).await,
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

    /// 终端里没有对话入口：裸 `komo` 与 `komo web` 是同一件事（打开 Gateway 的网页）。
    #[test]
    fn bare_komo_is_komo_web() {
        assert!(matches!(
            command_of(Cli::parse_from(["komo"])),
            Command::Web { no_open: false }
        ));
        assert!(matches!(
            command_of(Cli::parse_from(["komo", "web", "--no-open"])),
            Command::Web { no_open: true }
        ));
        // 已经删掉的子命令不再能解析。
        for gone in ["home", "resume"] {
            assert!(Cli::try_parse_from(["komo", gone, "x"]).is_err(), "{gone}");
        }
    }

    /// `komo web` 连上发现文件指的 Gateway，印出它的地址（没有配对码了）。
    #[tokio::test]
    async fn web_prints_the_gateway_address_from_the_discovery_file() {
        use axum::routing::get;
        use axum::{Json, Router};
        use komo_kernel::protocol::PROTOCOL_VERSION;
        use komo_kernel::protocol::http::HealthResponse;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().expect("临时数据目录");
        let data_dir = dir.path().to_string_lossy().to_string();
        let health = HealthResponse {
            instance_id: "inst-1".into(),
            version: "test".into(),
            protocol_version: PROTOCOL_VERSION,
            started_at: time::OffsetDateTime::now_utc(),
            data_dir,
            telemetry: Default::default(),
        };
        let app = Router::new().route("/healthz", get(move || async move { Json(health) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定 loopback");
        let base_url = format!("http://{}", listener.local_addr().expect("本地地址"));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        std::fs::create_dir_all(dir.path().join("runtime")).expect("runtime 目录");
        std::fs::write(
            dir.path().join("runtime/gateway.json"),
            serde_json::json!({
                "instance_id": "inst-1",
                "base_url": base_url,
                "protocol_version": PROTOCOL_VERSION,
            })
            .to_string(),
        )
        .expect("写发现文件");

        let out = web(dir.path(), true)
            .await
            .expect("komo web")
            .expect("有输出");
        server.abort();
        assert!(out.starts_with(&format!("{base_url}\n")), "{out}");
        assert!(!out.contains("/pair"), "{out}");
    }

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
            Cli::parse_from(["komo", "web", "--no-open"]).command,
            Some(Command::Web { no_open: true })
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

    /// `komo memory add --for` / `classify --for`（`--application` 可省）、`list` / `search` 的
    /// `--include-archived`（docs/goals.md §6）。
    #[test]
    fn memory_commands_parse_the_matter_flags() {
        for args in [
            &["memory", "add", "一条线索", "--for", "g-1"][..],
            &[
                "memory",
                "classify",
                "m-1",
                "--revision",
                "1",
                "--for",
                "g-1",
            ],
            &[
                "memory",
                "classify",
                "m-1",
                "--revision",
                "1",
                "--for",
                "none",
            ],
            &[
                "memory",
                "classify",
                "m-1",
                "--revision",
                "1",
                "--application",
                "reference",
                "--for",
                "a-1",
            ],
            &["memory", "list", "--include-archived", "--full"],
            &["memory", "search", "发布", "--include-archived"],
            &["memory", "list", "--state", "archived"],
        ] {
            let argv = std::iter::once("komo").chain(args.iter().copied());
            assert!(Cli::try_parse_from(argv).is_ok(), "{args:?}");
        }
    }

    /// `komo goal` / `komo area`（docs/goals.md §4）：每个动词都解析得出，`--due` 与
    /// `--no-due` 只能给一个，`--revision` 跟在任何改动后面。
    #[test]
    fn goal_and_area_commands_parse() {
        for args in [
            &[
                "goal",
                "add",
                "发布观察",
                "--next",
                "看 5xx",
                "--due",
                "2026-11-01",
            ][..],
            &["goal", "next", "g-1", "提工单", "--revision", "3"],
            &["goal", "wait", "g-1", "SRE 审批"],
            &["goal", "resume", "g-1", "--next", "看意见"],
            &["goal", "park", "g-1"],
            &["goal", "done", "g-1"],
            &["goal", "drop", "g-1"],
            &["goal", "reopen", "g-1"],
            &["goal", "edit", "g-1", "--title", "新标题", "--no-due"],
            &["goal", "list", "--all"],
            &["goal", "show", "g-1"],
            &["area", "add", "值班轮换", "--standard", "每周一前"],
            &["area", "edit", "a-1", "--standard", "每周一 10 点前"],
            &["area", "retire", "a-1"],
            &["area", "reopen", "a-1"],
            &["area", "list"],
            &["area", "show", "a-1"],
        ] {
            let argv = std::iter::once("komo").chain(args.iter().copied());
            assert!(Cli::try_parse_from(argv).is_ok(), "{args:?}");
        }
        assert!(
            Cli::try_parse_from([
                "komo",
                "goal",
                "edit",
                "g-1",
                "--due",
                "2026-11-01",
                "--no-due"
            ])
            .is_err()
        );
        let Some(Command::Goal {
            action: matters::GoalCommand::Done { .. },
        }) = Cli::parse_from(["komo", "goal", "done", "g-1", "--revision", "2"]).command
        else {
            panic!("解析不出 goal done");
        };
    }

    /// `komo goal|area link|unlink`（docs/goals.md §8）：只挂 monitor；定时不再是挂上去的引用。
    #[test]
    fn link_commands_take_a_monitor_and_no_longer_a_cron_job() {
        for args in [
            &[
                "goal",
                "link",
                "g-1",
                "--monitor",
                "sess-1:#3f2a",
                "--revision",
                "2",
            ][..],
            &["goal", "unlink", "g-1", "--monitor", "sess-1:#3f2a"],
            &["area", "link", "a-1", "--monitor", "sess-1:3f2a"],
            &["area", "unlink", "a-1", "--monitor", "sess-1:3f2a"],
        ] {
            let argv = std::iter::once("komo").chain(args.iter().copied());
            assert!(Cli::try_parse_from(argv).is_ok(), "{args:?}");
        }
        assert!(
            Cli::try_parse_from(["komo", "goal", "link", "g-1", "--cron", "job-1"]).is_err(),
            "定时不再挂在目标上"
        );
        assert!(
            Cli::try_parse_from(["komo", "goal", "link", "g-1"]).is_err(),
            "没给 monitor"
        );
        assert!(
            Cli::try_parse_from(["komo", "cron", "list"]).is_err(),
            "独立的 komo cron 已经没有了"
        );
    }

    /// 定时的全部字段（§10）在 `komo goal|area schedule` 上都有 flag——和 `add` 随目标一起给是同一组。
    #[test]
    fn schedule_covers_every_job_field_for_goals_and_areas() {
        for kind in ["goal", "area"] {
            let parsed = Cli::parse_from([
                "komo",
                kind,
                "schedule",
                "g-1",
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
                "--pause",
            ]);
            let spec = match parsed.command {
                Some(Command::Goal {
                    action: matters::GoalCommand::Schedule { args, .. },
                }) => args.into_spec().unwrap().unwrap(),
                Some(Command::Area {
                    action: matters::AreaCommand::Schedule { args, .. },
                }) => args.into_spec().unwrap().unwrap(),
                _ => panic!("解析不出 {kind} schedule"),
            };
            assert_eq!(spec.schedule.as_deref(), Some("0 9 * * *"));
            assert_eq!(spec.timezone.as_deref(), Some("Asia/Shanghai"));
            assert_eq!(spec.prompt.as_deref(), Some("整理"));
            assert_eq!(spec.workdir.as_deref(), Some("/tmp"));
            assert_eq!(spec.model.as_deref(), Some("job-model"));
            assert_eq!(
                spec.effort.as_ref().map(|e| e.to_string()),
                Some("high".into())
            );
            assert_eq!(
                spec.skills,
                Some(vec!["memos".to_string(), "search".to_string()])
            );
            assert_eq!(spec.max_rounds, Some(12));
            assert_eq!(spec.overlap, Some(komo_kernel::cron::OverlapPolicy::Allow));
            assert_eq!(spec.notify, Some(komo_kernel::cron::NotifyPolicy::OnError));
            assert_eq!(spec.enabled, Some(false));
        }
    }

    /// 命令直跑模式（§10）：`--command` 与 `--prompt` 二选一，clap 在解析这一步就挡住
    /// "两个都给"；打错的 `--notify` / `--overlap` 在发请求之前被拒，不当默认。
    #[test]
    fn schedule_flags_are_checked_before_any_request_goes_out() {
        assert!(
            Cli::try_parse_from([
                "komo",
                "goal",
                "schedule",
                "g-1",
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
        let parsed = Cli::parse_from([
            "komo",
            "goal",
            "schedule",
            "g-1",
            "--schedule",
            "0 3 * * *",
            "--command",
            "tar czf /tmp/backup.tgz /data",
            "--notify",
            "nerver",
        ]);
        let Some(Command::Goal {
            action: matters::GoalCommand::Schedule { args, .. },
        }) = parsed.command
        else {
            panic!("解析不出 goal schedule");
        };
        assert!(args.into_spec().is_err(), "写错的 notify 不能当默认");
        // 一个定时字段都没给：`schedule` 没东西可改。
        let parsed = Cli::parse_from(["komo", "goal", "schedule", "g-1"]);
        let Some(Command::Goal {
            action: matters::GoalCommand::Schedule { args, .. },
        }) = parsed.command
        else {
            panic!("解析不出 goal schedule");
        };
        assert_eq!(args.into_spec().unwrap(), None);
        for cmd in [["goal", "unschedule", "g-1"], ["area", "run", "a-1"]] {
            let argv = std::iter::once("komo").chain(cmd);
            assert!(Cli::try_parse_from(argv).is_ok(), "{cmd:?}");
        }
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
}
