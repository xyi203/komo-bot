//! `ExecutionPlan` 与它的类型状态封印（§4、§7.2）。
//!
//! 计划由工具的 `prepare` 填写，模型参数不能指定 `recovery`，也不能指定任何版本
//! 字段。`plan_hash` 是计划的规范化哈希，审批绑定的就是它。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::delegate::DelegateSpec;
use super::digest::ContentHash;
use super::ids::{ApprovalId, GrantId, OperationId, RunId, SessionId, ToolCallId};

/// 计划的规范化哈希。审批、授权与恢复都按它核对。
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
/// Gateway 发起就取得交互操作者的权限（§8.8）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanSource {
    /// 操作者在聊天 / TUI 里的交互请求。
    Interactive { session: SessionId },
    /// 定时任务触发。
    Cron {
        job: super::ids::CronJobId,
        /// Job 定义的版本；Job 改了，旧授权失效（§10）。
        job_version: u64,
    },
    /// MemoryManager 的内部变更：走同一决策入口，但不是第六个 LLM 工具（§7.1）。
    Memory { session: Option<SessionId> },
    /// 恢复流程发起的核对调用（§8.6）。核对本身仍经过 Policy。
    Verification { of: ToolCallId },
}

impl PlanSource {
    /// 无人值守：没有操作者在等着回答。新增危险操作不能因为无人值守自动放行（§10）。
    pub fn is_unattended(&self) -> bool {
        matches!(self, PlanSource::Cron { .. } | PlanSource::Memory { .. })
    }

    pub fn kind(&self) -> SourceKind {
        match self {
            PlanSource::Interactive { .. } => SourceKind::Interactive,
            PlanSource::Cron { .. } => SourceKind::Cron,
            PlanSource::Memory { .. } => SourceKind::Memory,
            PlanSource::Verification { .. } => SourceKind::Verification,
        }
    }
}

/// [`PlanSource`] 去掉负载后的判别式，规则表用它匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Interactive,
    Cron,
    Memory,
    Verification,
}

/// 这次执行**做的是什么**——§7.1 那张表逐行要分辨的那个维度。
///
/// 它由 `prepare` 归类，不是模型给的字符串：规则表按它匹配，所以让模型来命名它就
/// 等于让模型给自己定风险等级。
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
    /// 修改启用中的 toolbox（写候选、启用新版本）。
    ToolboxChange { module: String },
    /// 修改 Python 环境（依赖安装、环境版本切换）。
    PythonEnvChange,
    /// 自动记忆的内部变更与索引生成。
    MemoryChange,
    /// 权限扩大或修改 Policy。模型自行放宽是不被允许的（§7.1 最后一行）。
    PolicyChange,
    /// 把一个自包含子任务交给一条子 Run（§4）。
    ///
    /// **它不是第七个基础工具**：模型看不见任何新能力，子代理用的是同一套六个工具，而它
    /// 自己的每一次调用照常过 Policy（§7.1）。所以要审的不是"模型能不能做这件事"，而是
    /// "允不允许它把这件事派出去"——规则表按这一条匹配。
    ///
    /// **整份 [`DelegateSpec`] 都在计划里**（含任务正文与结果契约）：计划是审批绑定的对象，
    /// 也是父 Run 续跑时手里唯一那份东西——它要拿同一份契约去复验子代理的结果（§8.6）。
    Delegate { spec: DelegateSpec },
}

impl Operation {
    /// 任意代码：一旦启动就具有其运行账号的操作系统权限（§7.3）。
    pub fn is_arbitrary_code(&self) -> bool {
        matches!(self, Operation::ShellCommand { .. } | Operation::PythonCode)
    }

    /// 只读动作。
    pub fn is_read_only(&self) -> bool {
        matches!(self, Operation::ReadFile)
    }
}

/// 计划触及的一个真实目标（已解析符号链接后的路径）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTarget {
    /// 真实路径。规则的路径匹配只看这里，不看模型给的原始参数。
    pub path: PathBuf,
    pub access: TargetAccess,
    /// 覆盖现有文件时的预期版本（§4：`write` / `edit` 的版本检查）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<ContentHash>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetAccess {
    Read,
    Write,
}

/// 计划绑定的各种版本（§7.2）。版本变化使旧授权失效（§5.4）。
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

/// §8.6 的四种恢复方式。由 `prepare` 填写，模型不能随意填"可重试"。
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
    /// 可以核对目标状态：用审核过的核对逻辑判断已达到 / 确定未执行 / 冲突 / 未知。
    VerifyTarget,
    /// 无可靠恢复方式：停在 `needs_attention`，不自动从头执行整个脚本。
    NoSafeRecovery,
}

/// 一份不可变的执行计划（§7.2）。
///
/// 审批绑定的就是这个对象；[`ExecutionPlan::plan_hash`] 是它的规范化哈希。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPlan {
    /// 这次被审核的操作的 ID。Memory 内部变更也有自己的 operation_id（§7.1）。
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

    /// 计划触及的真实路径。
    pub fn paths(&self) -> impl Iterator<Item = &std::path::Path> {
        self.targets.iter().map(|t| t.path.as_path())
    }
}

/// 一次执行被允许的证明。
///
/// **字段私有，没有公开构造函数**，所以 kernel 之外无法凭空造一个。两个来源：
/// [`crate::policy::PolicyDecision::into_proof`]（只有 `Allow` 换得出）和
/// [`ConsumedApproval::into_proof`]（只有消费成功的审批换得出）。测试用
/// `test_support::proof()`，feature 门控，不进正式构建（§4）。
///
/// 外部 crate 造不出 `Proof`，所以也调不到 `Tool::execute`——这是**编译期**的保证，
/// 不是约定（§14 阶段 2 的验收项）：
///
/// ```compile_fail
/// # use komo_kernel::types::Proof;
/// // 元组字段是私有的，构造不出来。
/// let forged = Proof(unimplemented!());
/// ```
///
/// ```compile_fail
/// # use komo_kernel::types::Proof;
/// // 构造函数是 pub(crate) 的，外面看不见。
/// let forged = Proof::policy_allow();
/// ```
///
/// ```compile_fail
/// # use komo_kernel::types::{ApprovedPlan, ExecutionPlan};
/// // 于是 ApprovedPlan 也没法凭空造：第二个参数拿不到。
/// fn forge(plan: ExecutionPlan) -> ApprovedPlan {
///     ApprovedPlan::new(plan, komo_kernel::types::Proof::policy_allow())
/// }
/// ```
///
/// 唯一合法的两条路都在 kernel 里换：
///
/// ```
/// # use komo_kernel::policy::PolicyDecision;
/// assert!(PolicyDecision::allow("在已授权根内").into_proof().is_some());
/// assert!(PolicyDecision::ask("需要人看一眼").into_proof().is_none());
/// assert!(PolicyDecision::deny("命中禁用规则").into_proof().is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof(ProofKind);

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProofKind {
    /// Policy 直接答 Allow。
    PolicyAllow,
    /// 消费了一条已批准的审批 / 授权。
    Approval {
        approval: ApprovalId,
        grant: Option<GrantId>,
    },
}

impl Proof {
    pub(crate) fn policy_allow() -> Self {
        Proof(ProofKind::PolicyAllow)
    }

    pub(crate) fn approval(approval: ApprovalId, grant: Option<GrantId>) -> Self {
        Proof(ProofKind::Approval { approval, grant })
    }

    /// 这次放行来自哪条审批（如果不是 Policy 直接 Allow）。写进账本用。
    pub fn approval_id(&self) -> Option<&ApprovalId> {
        match &self.0 {
            ProofKind::PolicyAllow => None,
            ProofKind::Approval { approval, .. } => Some(approval),
        }
    }

    /// 这次放行消费了哪条范围授权。
    pub fn grant_id(&self) -> Option<&GrantId> {
        match &self.0 {
            ProofKind::PolicyAllow => None,
            ProofKind::Approval { grant, .. } => grant.as_ref(),
        }
    }
}

/// 这次消费是**第一次跑这个调用**，还是恢复流程已经核对过、确定原动作没发生。
///
/// §7.4 在这里分了两句话：「已取消或已完成调用不能再次执行」和「恢复时若确定原动作未
/// 发生……可在原授权范围内继续；**已经消费授权本身不是重试依据**」。把这件事交给
/// `ApprovalRepo` 用一个参数问出来，而不是让每个调用点自己记得判断——两个语境说的是
/// 同一条授权，只有"这一次算不算重来"不同。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumeIntent {
    /// 这个调用的第一次执行。一次性授权已经用过了就拒绝——那意味着这个动作已经发生过。
    #[default]
    First,
    /// 恢复流程核对过了：§8.4 第 6 行的"确定尚未执行"，或 §8.6 的核对给出
    /// [`Verification::NotPerformed`]。这时候才准重用一条已消费的一次性授权。
    KnownNotToHaveRun,
}

/// 一条被成功消费的审批。只有执行器从 `ApprovalRepo` 消费成功才拿得到它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumedApproval {
    approval: ApprovalId,
    grant: Option<GrantId>,
    plan_hash: PlanHash,
}

impl ConsumedApproval {
    /// `ApprovalRepo` 的实现在确认「这条审批已批准、仍有效、绑定的正是这个计划哈希」
    /// 之后调用它。
    pub fn new(approval: ApprovalId, grant: Option<GrantId>, plan_hash: PlanHash) -> Self {
        Self {
            approval,
            grant,
            plan_hash,
        }
    }

    pub fn plan_hash(&self) -> &PlanHash {
        &self.plan_hash
    }

    pub fn into_proof(self) -> Proof {
        Proof::approval(self.approval, self.grant)
    }
}

/// 允许执行的计划。`Tool::execute` 只接受它（§4），所以「没有经过 Policy 或审批就
/// 执行」在类型上就写不出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedPlan {
    plan: ExecutionPlan,
    proof: Proof,
}

impl ApprovedPlan {
    pub fn new(plan: ExecutionPlan, proof: Proof) -> Self {
        Self { plan, proof }
    }

    pub fn plan(&self) -> &ExecutionPlan {
        &self.plan
    }

    pub fn proof(&self) -> &Proof {
        &self.proof
    }

    pub fn into_parts(self) -> (ExecutionPlan, Proof) {
        (self.plan, self.proof)
    }
}

/// 核对的结论（§8.6）。默认实现返回 [`Verification::Unavailable`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verification {
    /// 目标已达到预期。
    AlreadySatisfied { evidence: String },
    /// 确定未执行，且前提仍成立，可以重新执行同一原子修改。
    NotPerformed { evidence: String },
    /// 出现了第三种状态：既不是原内容也不是预期内容。
    Conflict { evidence: String },
    /// 核对不出结论。
    Unknown { reason: String },
    /// 这个工具没有可用的核对方式 → `needs_attention`。
    Unavailable,
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
            targets: vec![PlanTarget {
                path: PathBuf::from("/tmp/ws/a.txt"),
                access: TargetAccess::Write,
                expected_version: None,
            }],
            versions: PlanVersions::default(),
            resources: vec![],
            recovery: RecoveryMode::VerifyTarget,
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
        b.targets[0].path = PathBuf::from("/etc/passwd");
        assert_ne!(a.plan_hash(), b.plan_hash());
    }

    #[test]
    fn an_approved_plan_keeps_the_proof_it_was_built_with() {
        let approved = ApprovedPlan::new(plan(), Proof::policy_allow());
        assert!(approved.proof().approval_id().is_none());

        let consumed = ConsumedApproval::new(
            ApprovalId::from_raw("ap-1"),
            Some(GrantId::from_raw("g-1")),
            plan().plan_hash(),
        );
        let approved = ApprovedPlan::new(plan(), consumed.into_proof());
        assert_eq!(
            approved.proof().approval_id().map(|a| a.as_str()),
            Some("ap-1")
        );
        assert_eq!(approved.proof().grant_id().map(|g| g.as_str()), Some("g-1"));
    }

    #[test]
    fn cron_and_memory_plans_are_unattended() {
        assert!(
            PlanSource::Cron {
                job: crate::types::ids::CronJobId::from_raw("job-1"),
                job_version: 1
            }
            .is_unattended()
        );
        assert!(
            !PlanSource::Interactive {
                session: SessionId::from_raw("s")
            }
            .is_unattended()
        );
    }
}
