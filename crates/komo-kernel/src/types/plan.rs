//! `ExecutionPlan`：一次工具调用要做什么（§4、§7.2）。
//!
//! 计划由工具的 `prepare` 填写，模型参数不能指定 `recovery`，也不能指定任何版本
//! 字段。`plan_hash` 是计划的规范化哈希，账本与恢复按它核对。

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::delegate::DelegateSpec;
use super::digest::ContentHash;
use super::ids::{OperationId, RunId, SessionId, ToolCallId};
use super::resource::{ResourceUri, TargetRef};

/// 计划的规范化哈希。账本与恢复都按它核对。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanHash(ContentHash);

impl PlanHash {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self(ContentHash::from_raw(raw))
    }
}

impl std::fmt::Display for PlanHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_str())
    }
}

/// 请求这次执行的是谁。恢复后来源不变——Cron 恢复后仍是 Cron，不会因为重启由本机
/// Gateway 发起就变成交互请求（§8.8）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanSource {
    /// 操作者在聊天 / TUI 里的交互请求。
    Interactive { session: SessionId },
    /// 定时任务触发。
    Cron {
        job: super::ids::CronJobId,
        /// Job 定义的版本（§10）。
        job_version: u64,
    },
    /// MemoryManager 的内部变更：走同一执行入口，但不是第六个 LLM 工具（§7.1）。
    Memory { session: Option<SessionId> },
    /// 恢复流程发起的核对调用。核对梯子 2026-10-13 删掉（§8.6），只为解码旧计划保留。
    Verification { of: ToolCallId },
}

/// 这次执行**做的是什么**。
///
/// 它由 `prepare` 归类，不是模型给的字符串：恢复方式、只读判断与界面文案都按它分辨。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    /// 读取普通文件。
    ReadFile,
    /// 写入或修改文件（覆盖时检查版本）。
    WriteFile,
    /// 任意 shell 命令。
    ShellCommand { command: String },
    /// 任意 Python 代码（`mode = "code"`）。
    PythonCode,
    /// 调用 toolbox 中已导出的函数（`mode = "call"`）。
    PythonCall { module: String, function: String },
    /// 修改 Python 环境（依赖安装、环境版本切换）。
    PythonEnvChange,
    /// 自动记忆的内部变更与索引生成。
    MemoryChange,
    /// 把一个自包含子任务交给一条子 Run（§4）。
    ///
    /// **它不是第七个基础工具**：模型看不见任何新能力，子代理用的是同一套六个工具，而它
    /// 自己的每一次调用照常走 executor。
    ///
    /// **整份 [`DelegateSpec`] 都在计划里**（含任务正文与结果契约）：计划是父 Run 续跑时手里唯一那份东西——它要拿同一份契约去复验子代理的结果（§8.6）。
    Delegate { spec: DelegateSpec },
    /// 建一个**独立的任务会话**并提交第一条输入（`docs/background-tasks.md`）。
    ///
    /// 与 `Delegate` 同一类操作——不是第七个基础工具，模型看不见任何新能力，任务会话
    /// 里的每一次调用照常走 executor——但**不等它跑完**：executor 放行之后立刻收尾（不
    /// 返回 `RoundStop::Dependency`），home 不会被一个慢任务堵住。`task` / `title` 都
    /// 进计划：计划是幂等重放时唯一能对上的那份东西。
    Dispatch { task: String, title: String },
    /// 把一句话提交进一个**已有的**任务会话（`docs/background-tasks.md`）。
    ///
    /// `task_id` 是模型给的任务短号原文，解析成具体会话在执行侧做（只有 Gateway 查得到
    /// "这个会话派出去过哪些任务"）。
    Follow { task_id: String, text: String },
    /// 挂一个长时间跑的监听脚本，stdout 的行交回这个会话（`docs/monitor.md`）。
    ///
    /// 与 `Dispatch` 同一类编排操作：执行器放行之后交给 Gateway 起进程、立刻收尾，不等
    /// 脚本跑完。命令与时限都进计划——重放时唯一能对上的就是这份计划。
    Monitor {
        command: String,
        title: String,
        timeout_minutes: u64,
    },
    /// 停掉这个会话挂着的一个监听。`monitor_id` 是模型给的短号原文。
    MonitorStop { monitor_id: String },
    /// 更新这条 Run 的工作计划（`update_plan`）。
    ///
    /// 与 `Delegate` / `Dispatch` 同一类编排操作，不是第七个基础工具：它不碰文件、不碰
    /// 进程、没有副作用，只是让模型报一次进度。整份计划只在 `args` 里——这里不再存一
    /// 份，免得同一件事有两个落点。
    UpdatePlan,
    /// 调一个 MCP 服务器上的工具（`docs/mcp.md`）。
    ///
    /// 副作用由服务器决定，komo 看不见。`read_only` 是**操作者**在配置里声明的
    /// （`[mcp.servers.<name>] read_only`），不是服务器自报的 `readOnlyHint`——自称安全
    /// 的东西不算数。
    McpCall {
        server: String,
        tool: String,
        #[serde(default)]
        read_only: bool,
    },
    /// 一段在操作系统沙箱里跑的 Python 脚本（`docs/codemode.md`）。
    ///
    /// 脚本自己碰不到文件、网络与进程；工具调用经 executor 的 durable journal。
    /// 允许只读操作及 MCP（包括写操作），所以整段是调度屏障；恢复接回已记录子结果。
    Codemode { code: String },
}

impl Operation {
    /// 只读动作。
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Operation::ReadFile
                | Operation::McpCall {
                    read_only: true,
                    ..
                }
        )
    }
}

/// 计划触及的一个目标（§六）：**本地路径，或者一个资源 URI**。
///
/// 两种目标在计划里带着不同的东西：本地路径只有路径；资源入口带着**逻辑** URI（界面与
/// 审计回答的是"读的是哪个逻辑资源"）与解析出来的**真实**目标（执行看它）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTarget {
    /// 目标本身。**序列化上扁平**：本地路径那条仍然是 `{"path": "…"}`，与旧行逐字
    /// 兼容——旧计划的哈希不会因为多了资源这一维就变（§八）。
    #[serde(flatten)]
    pub target: TargetRef,
    pub access: TargetAccess,
    /// 覆盖现有文件时的预期版本（§4：`write` / `edit` 的版本检查）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<ContentHash>,
}

impl PlanTarget {
    /// 一个本地路径目标（已解析符号链接的真实路径）。
    pub fn local(path: impl Into<PathBuf>, access: TargetAccess) -> Self {
        Self {
            target: TargetRef::local(path),
            access,
            expected_version: None,
        }
    }

    /// 一个资源目标：逻辑 URI + 计划那一刻解析出来的真实目标。
    pub fn resource(uri: ResourceUri, resolved: Option<PathBuf>, access: TargetAccess) -> Self {
        Self {
            target: TargetRef::resource(uri, resolved),
            access,
            expected_version: None,
        }
    }

    /// 解析出来的真实目标——**按路径判断只看这里**，不看模型给的原始参数。
    ///
    /// 虚拟入口（`tool://`）没有磁盘目标，返回 `None`。
    pub fn path(&self) -> Option<&Path> {
        self.target.path()
    }

    /// 逻辑资源入口（本地路径没有）。
    pub fn uri(&self) -> Option<&ResourceUri> {
        self.target.uri()
    }

    /// 没有磁盘目标的那一类。
    pub fn is_virtual(&self) -> bool {
        self.target.is_virtual()
    }
}

impl fmt::Display for PlanTarget {
    /// 界面与日志用同一份拼法：资源印**逻辑** URI，本地印路径。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.target)
    }
}

impl PlanTarget {
    /// 展示那一行：资源印逻辑 URI（有真实目标就跟在后面——"读哪个逻辑资源"与
    /// "实际读的是哪个文件"是两件事，看的人要同时看到），本地路径照旧。
    pub fn describe(&self) -> String {
        if self.uri().is_none() {
            return self.to_string();
        }
        match self.path() {
            Some(path) => format!("{}（{}）", self, path.display()),
            None => self.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetAccess {
    Read,
    Write,
}

/// 计划绑定的各种版本（§7.2）。版本变化改变计划哈希（§5.4）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanVersions {
    /// 代码内容哈希（任意 code 模式、shell 命令正文的快照）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<ContentHash>,
    /// 已保存模块的版本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// Python 环境版本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<EnvVersion>,
}

/// 受管理的 Python 环境版本。依赖升级需要新的环境版本（§5.1）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnvVersion(pub String);

/// 计划引用到的资源：HA / Memos / 搜索服务的端点与**凭证引用**。凭证的值不进计划、
/// 不进提示词、不进日志（§5.3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRef {
    /// 资源名，例如 `memos`、`homeassistant`。
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// 凭证所在的环境变量名——不是凭证本身。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_env: Option<String>,
}

/// 计划里记下的恢复方式（§8.6）。由 `prepare` 填写，模型不能随意填"可重试"。
///
/// 重启续跑只问它一件事：[`RecoveryMode::replay`]。四个变体都要留着——它进计划哈希，
/// 旧 JSONL 与旧库里的计划必须照原样解码、哈希不变。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryMode {
    /// 可安全重做的读取。重做要记录这次重新读取的实际时间，不冒充重启前的观察。
    SafeReread,
    /// 外部接口确实保证去重；所有尝试复用同一逻辑操作的键。
    IdempotencyKey {
        key: String,
        /// 键的有效期（秒）；None 表示外部接口没有给出有效期。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        valid_for_secs: Option<u64>,
    },
    /// 2026-10-13 之前 write / edit / 带核对函数的 python 计划写的是它（那时有核对梯子）。
    /// 只为解码旧计划保留，新计划不再产生；重启时按 [`Replay::Unsafe`]。
    VerifyTarget,
    /// 无可靠恢复方式：重启时不重跑，中断结果交给模型判断。
    NoSafeRecovery,
}

impl RecoveryMode {
    /// 计划受理时记下的重放策略（pi-durable 的 `replay`）。
    pub fn replay(&self) -> Replay {
        match self {
            RecoveryMode::SafeReread | RecoveryMode::IdempotencyKey { .. } => Replay::Safe,
            RecoveryMode::VerifyTarget | RecoveryMode::NoSafeRecovery => Replay::Unsafe,
        }
    }
}

/// 一次 started 而没有结果的调用，重启后能不能带原计划再跑一遍（§8.6）。
///
/// 规则借自 pi-durable：计划里记下的策略与工具**现在**声明的（[`crate::traits::Tool::replay`]）
/// 都是 `Safe` 才重跑；现在的 `Unsafe` 能否决记下的 `Safe`，反过来不行。其余一律把中断
/// 结果（带中断前已落盘的部分输出）交给模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Replay {
    Safe,
    Unsafe,
}

impl Replay {
    /// 两份策略都说 `Safe` 才算 `Safe`。
    pub fn and(self, other: Replay) -> Replay {
        match (self, other) {
            (Replay::Safe, Replay::Safe) => Replay::Safe,
            _ => Replay::Unsafe,
        }
    }
}

/// 一份不可变的执行计划（§7.2）。
///
/// `Tool::execute` 收的就是它（按值）；[`ExecutionPlan::plan_hash`] 是它的规范化哈希。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPlan {
    /// 这次操作的 ID。Memory 内部变更也有自己的 operation_id（§7.1）。
    pub operation_id: OperationId,
    pub source: PlanSource,
    /// 工具名。
    pub tool: String,
    pub operation: Operation,
    /// 关联的 Run / ToolCall（如有）。Memory 变更可能两者都没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<ToolCallId>,
    /// 规范化后的参数。
    pub args: serde_json::Value,
    /// 工作目录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// 真实目标路径与它们的预期版本。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<PlanTarget>,
    #[serde(default)]
    pub versions: PlanVersions,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<ResourceRef>,
    pub recovery: RecoveryMode,
    /// 文件改动之后紧接着跑的那条命令（`edit` / `write` 的 `then_run`）。
    ///
    /// 两步是**一份**计划：计划哈希盖住两步（§7.2）。只许一层、只能是 shell 命令——见
    /// [`ExecutionPlan::validate_steps`]。没有它时不序列化，旧计划的哈希逐字不变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then_run: Option<Box<ExecutionPlan>>,
}

/// 组合计划的形状不对（[`ExecutionPlan::validate_steps`]）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StepsError {
    #[error("then_run 只许一层：它自己不能再带 then_run")]
    Nested,
    #[error("then_run 只能是一条 shell 命令")]
    NotAShellCommand,
    /// 第二步的来源、Run 或 ToolCall 与第一步不同：账本按这几样记这次调用，两步记在
    /// 不同的 Run 下就对不上同一个 ToolCall。
    #[error("then_run 必须与改动同一来源、同一 Run、同一 ToolCall")]
    ForeignContext,
}

impl ExecutionPlan {
    /// 计划的规范化哈希。
    ///
    /// 先转成 [`serde_json::Value`]，对象的键因此排序，**同一份计划序列化两次得到同一
    /// 个哈希，字段在结构体里的声明顺序也不影响结果**。哈希不作为字段存在计划里，
    /// 免得计划被改过而哈希还是旧的。
    pub fn plan_hash(&self) -> PlanHash {
        let hash = ContentHash::of_json(self).expect("ExecutionPlan 的每个字段都可序列化");
        PlanHash(hash)
    }

    /// 计划触及的真实路径（虚拟入口没有路径，不在其中）。
    pub fn paths(&self) -> impl Iterator<Item = &std::path::Path> {
        self.targets.iter().filter_map(|target| target.path())
    }

    /// 这份计划的各步：自己，然后是 `then_run`（只有一层）。
    pub fn steps(&self) -> impl Iterator<Item = &ExecutionPlan> {
        std::iter::once(self).chain(self.then_run.as_deref())
    }

    /// 组合计划的形状：`then_run` 只许一层、只能是 shell 命令、与改动同一来源 / Run /
    /// ToolCall。工具在 `prepare` 里调它，executor 执行前再核
    /// 一遍——形状不对的组合计划不执行，不指望每个工具都记得。
    pub fn validate_steps(&self) -> Result<(), StepsError> {
        let Some(next) = self.then_run.as_deref() else {
            return Ok(());
        };
        if next.then_run.is_some() {
            return Err(StepsError::Nested);
        }
        if !matches!(next.operation, Operation::ShellCommand { .. }) {
            return Err(StepsError::NotAShellCommand);
        }
        if next.source != self.source || next.run != self.run || next.tool_call != self.tool_call {
            return Err(StepsError::ForeignContext);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::OperationId;

    fn plan() -> ExecutionPlan {
        ExecutionPlan {
            operation_id: OperationId::from_raw("op-1"),
            source: PlanSource::Interactive {
                session: SessionId::from_raw("sess-1"),
            },
            tool: "write".into(),
            operation: Operation::WriteFile,
            run: Some(RunId::from_raw("run-1")),
            tool_call: Some(ToolCallId::from_raw("call-7")),
            args: serde_json::json!({"path": "a.txt", "content": "hi"}),
            cwd: Some(PathBuf::from("/tmp/ws")),
            targets: vec![PlanTarget::local("/tmp/ws/a.txt", TargetAccess::Write)],
            versions: PlanVersions::default(),
            resources: vec![],
            recovery: RecoveryMode::VerifyTarget,
            then_run: None,
        }
    }

    fn fused(command: &str) -> ExecutionPlan {
        let mut step = plan();
        step.tool = "shell".into();
        step.operation = Operation::ShellCommand {
            command: command.into(),
        };
        step.args = serde_json::json!({ "command": command });
        step.targets = vec![];
        ExecutionPlan {
            then_run: Some(Box::new(step)),
            ..plan()
        }
    }

    /// 这份计划在 `then_run` 出现之前序列化成的样子——旧 JSONL 与
    /// 旧库里存的就是这种行。
    const OLD_ROW: &str = r#"{"args":{"content":"hi","path":"a.txt"},"cwd":"/tmp/ws","operation":{"kind":"write_file"},"operation_id":"op-1","recovery":{"kind":"verify_target"},"run":"run-1","source":{"kind":"interactive","session":"sess-1"},"targets":[{"access":"write","path":"/tmp/ws/a.txt"}],"tool":"write","tool_call":"call-7","versions":{}}"#;

    #[test]
    fn only_rereads_and_idempotency_keys_are_replay_safe() {
        assert_eq!(RecoveryMode::SafeReread.replay(), Replay::Safe);
        let keyed = RecoveryMode::IdempotencyKey {
            key: "k".into(),
            valid_for_secs: None,
        };
        assert_eq!(keyed.replay(), Replay::Safe);
        // 旧计划里的 verify_target 还解得出来，重启时按 unsafe。
        assert_eq!(
            serde_json::from_value::<RecoveryMode>(serde_json::json!({"kind": "verify_target"}))
                .unwrap()
                .replay(),
            Replay::Unsafe
        );
        assert_eq!(RecoveryMode::NoSafeRecovery.replay(), Replay::Unsafe);
    }

    #[test]
    fn a_current_unsafe_declaration_vetoes_a_stored_safe_one() {
        assert_eq!(Replay::Safe.and(Replay::Safe), Replay::Safe);
        assert_eq!(Replay::Safe.and(Replay::Unsafe), Replay::Unsafe);
        assert_eq!(Replay::Unsafe.and(Replay::Safe), Replay::Unsafe);
    }

    #[test]
    fn a_plan_without_then_run_hashes_exactly_as_before() {
        assert_eq!(
            plan().plan_hash().as_str(),
            "b70f7d86d16c050098cc34320cb5493959931536b31366d746996e521f63aa96"
        );
        let canonical = serde_json::to_value(plan()).unwrap();
        assert_eq!(serde_json::to_string(&canonical).unwrap(), OLD_ROW);
    }

    #[test]
    fn an_old_stored_plan_reads_back_without_then_run() {
        let read: ExecutionPlan = serde_json::from_str(OLD_ROW).unwrap();
        assert_eq!(read.then_run, None);
        assert_eq!(read, plan());
    }

    #[test]
    fn the_composite_hash_covers_the_command() {
        let a = fused("cargo test");
        assert_ne!(a.plan_hash(), plan().plan_hash());
        assert_ne!(a.plan_hash(), fused("cargo test --release").plan_hash());
        assert_eq!(a.plan_hash(), fused("cargo test").plan_hash());
    }

    #[test]
    fn steps_are_the_plan_then_its_then_run() {
        let single = plan();
        assert_eq!(single.steps().count(), 1);

        let composite = fused("cargo test");
        let tools: Vec<&str> = composite.steps().map(|step| step.tool.as_str()).collect();
        assert_eq!(tools, ["write", "shell"]);
        assert_eq!(composite.validate_steps(), Ok(()));
    }

    #[test]
    fn then_run_is_one_shell_command_in_the_same_call() {
        let mut nested = fused("cargo test");
        nested.then_run.as_mut().unwrap().then_run = Some(Box::new(plan()));
        assert_eq!(nested.validate_steps(), Err(StepsError::Nested));

        let not_shell = ExecutionPlan {
            then_run: Some(Box::new(plan())),
            ..plan()
        };
        assert_eq!(
            not_shell.validate_steps(),
            Err(StepsError::NotAShellCommand)
        );

        let changes: [fn(&mut ExecutionPlan); 3] = [
            |step| step.run = Some(RunId::from_raw("run-2")),
            |step| step.tool_call = None,
            |step| step.source = PlanSource::Memory { session: None },
        ];
        for change in changes {
            let mut foreign = fused("cargo test");
            change(foreign.then_run.as_mut().unwrap());
            assert_eq!(foreign.validate_steps(), Err(StepsError::ForeignContext));
        }
    }

    #[test]
    fn hashing_the_same_plan_twice_gives_the_same_hash() {
        let p = plan();
        assert_eq!(p.plan_hash(), p.plan_hash());
        assert_eq!(p.plan_hash(), plan().plan_hash());
    }

    #[test]
    fn argument_key_order_does_not_change_the_hash() {
        let mut a = plan();
        let mut b = plan();
        a.args = serde_json::from_str(r#"{"path":"a.txt","content":"hi"}"#).unwrap();
        b.args = serde_json::from_str(r#"{"content":"hi","path":"a.txt"}"#).unwrap();
        assert_eq!(a.plan_hash(), b.plan_hash());
    }

    #[test]
    fn changing_a_bound_version_changes_the_hash() {
        let a = plan();
        let mut b = plan();
        b.versions.env = Some(EnvVersion("py-2".into()));
        assert_ne!(a.plan_hash(), b.plan_hash());
    }

    #[test]
    fn changing_the_real_target_path_changes_the_hash() {
        let a = plan();
        let mut b = plan();
        b.targets[0].target = TargetRef::local("/etc/passwd");
        assert_ne!(a.plan_hash(), b.plan_hash());
    }
}
