//! komo 二进制：clap 分发（docs/komo_bot.md §3 命令表、§13.4）。
//!
//! 这一层只做分发：`komo` / `komo resume` 与操作子命令走 komo-client，
//! `komo gateway` 走 komo-gateway。命令体在 W4 填。

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "komo", version, about = "komo：个人 Agent 框架", long_about = None)]
struct Cli {
    /// 不带子命令时：确保本机 Gateway 就绪，创建新 Session，进入 TUI 聊天（§3）。
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
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
    /// 查看和处理待审核操作；等价于聊天里的 /approve 与 /reject（§11.3）。
    Approval {
        #[command(subcommand)]
        action: ApprovalCommand,
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
    /// 显示当前生效配置的加载时间与来源文件 mtime，以及上一次校验错误（§3）。
    Doctor,
}

#[derive(Subcommand)]
enum SessionCommand {
    /// 查看会话列表和状态。
    List,
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
enum ApprovalCommand {
    /// 待审核列表，列表项包含短 ID、具体动作与计划。
    List,
    /// 单项审批详情。
    Show {
        /// 审批短 ID。
        approval_id: String,
    },
    /// 批准。
    Approve {
        /// 审批短 ID。
        approval_id: String,
    },
    /// 拒绝。
    Reject {
        /// 审批短 ID。
        approval_id: String,
    },
}

#[derive(Subcommand)]
enum CronCommand {
    /// 创建定时任务。
    Add,
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

#[derive(Subcommand)]
enum MemoryCommand {
    /// 按作用域、状态和查询条件列出记忆。
    List,
    /// 检索；支持 hybrid / keyword / vector（§9.4）。
    Search {
        /// 查询词。
        query: String,
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
    },
    /// 停用指定 revision 并失效索引；不删除 Memos 原文。
    Forget {
        /// 记忆 ID。
        memory_id: String,
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

fn main() {
    // §13.4 reqwest 行：reqwest 用 `rustls-no-provider`，进程里必须由我们自己装上
    // ring provider。这一步必须发生在任何网络线程之前——特别是飞书 ws 线程启动之前，
    // 否则第一次握手会因为没有默认 provider 而 panic。
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("安装 rustls ring provider");

    let cli = Cli::parse();
    match cli.command {
        // `komo`：确保本机 Gateway 就绪，创建新 Session，进入 TUI 聊天。
        None => todo!("W4"),
        Some(Command::Resume { .. }) => todo!("W4"),
        Some(Command::Session { action }) => match action {
            SessionCommand::List => todo!("W4"),
        },
        Some(Command::Run { action }) => match action {
            RunCommand::Inspect { .. } => todo!("W4"),
            RunCommand::Cancel { .. } => todo!("W4"),
        },
        Some(Command::Gateway { action, .. }) => match action {
            None => todo!("W4"),
            Some(GatewayCommand::Status) => todo!("W4"),
            Some(GatewayCommand::Stop) => todo!("W4"),
            Some(GatewayCommand::Restart) => todo!("W4"),
        },
        Some(Command::Approval { action }) => match action {
            ApprovalCommand::List => todo!("W4"),
            ApprovalCommand::Show { .. } => todo!("W4"),
            ApprovalCommand::Approve { .. } => todo!("W4"),
            ApprovalCommand::Reject { .. } => todo!("W4"),
        },
        Some(Command::Cron { action }) => match action {
            CronCommand::Add => todo!("W4"),
            CronCommand::List => todo!("W4"),
            CronCommand::Run { .. } => todo!("W4"),
            CronCommand::Pause { .. } => todo!("W4"),
            CronCommand::Resume { .. } => todo!("W4"),
            CronCommand::Remove { .. } => todo!("W4"),
        },
        Some(Command::Memory { action }) => match action {
            MemoryCommand::List => todo!("W4"),
            MemoryCommand::Search { .. } => todo!("W4"),
            MemoryCommand::Show { .. } => todo!("W4"),
            MemoryCommand::Confirm { .. } => todo!("W4"),
            MemoryCommand::Forget { .. } => todo!("W4"),
            MemoryCommand::Index { action } => match action {
                MemoryIndexCommand::Status => todo!("W4"),
                MemoryIndexCommand::Rebuild => todo!("W4"),
            },
        },
        Some(Command::Config { action }) => match action {
            ConfigCommand::Check => todo!("W4"),
            ConfigCommand::Reload => todo!("W4"),
        },
        Some(Command::Channel { action }) => match action {
            ChannelCommand::List => todo!("W4"),
            ChannelCommand::Probe => todo!("W4"),
            ChannelCommand::Wechat { action } => match action {
                WechatCommand::Login => todo!("W4"),
            },
        },
        Some(Command::Skills { action }) => match action {
            SkillsCommand::List => todo!("W4"),
            SkillsCommand::Inspect { .. } => todo!("W4"),
            SkillsCommand::Enable { .. } => todo!("W4"),
            SkillsCommand::Disable { .. } => todo!("W4"),
        },
        Some(Command::Doctor) => todo!("W4"),
    }
}
