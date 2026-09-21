//! 工具的定义、上下文与结果（§4）。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use super::ids::{AttemptId, RunId, SessionId, ToolCallId};
use super::plan::{EnvVersion, PlanSource, Verification};
use super::refs::{ContentRef, ToolResultStatus};
use super::status::ToolCallState;

/// 交给模型的工具 Schema。六个基础工具的这份定义是固定的（§4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema。
    pub parameters: serde_json::Value,
}

/// 工具执行的结果正文（还没有发布成 `output.json`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub status: ToolResultStatus,
    #[serde(default)]
    pub result: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ContentRef>,
    /// 给模型看的简短预览；完整正文在 `output.json` 里。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// 工具失败。普通失败作为结果交给模型修正；只有驱动 / LLM 错误中断整轮（§6）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolError {
    /// 参数不合法，模型可以改了再来。
    #[error("参数不合法：{message}")]
    InvalidArguments { message: String },
    /// 文件版本与预期不符（§4：覆盖现有文件需检查版本）。
    #[error("版本冲突：{path}")]
    VersionConflict { path: String },
    /// `edit` 的匹配内容没找到——不做模糊猜测。
    #[error("没有找到要替换的内容：{path}")]
    NoMatch { path: String },
    /// 被 Policy 拒绝。作为明确结果交回模型（§7.4）。
    #[error("被拒绝：{reason}")]
    Denied { reason: String },
    /// 超过活动执行时限。
    #[error("超时：{after_secs}s")]
    Timeout { after_secs: u64 },
    /// 用户取消。
    #[error("已取消")]
    Cancelled,
    /// 远端写入结果不明：停止自动重试，转为结果核对（§6、§8.6）。
    #[error("结果不明：{message}")]
    Uncertain { message: String },
    /// 其他执行失败。
    #[error("{message}")]
    Failed { message: String },
}

impl ToolError {
    /// 结果不明的失败不能被当成失败重试掉。
    pub fn is_uncertain(&self) -> bool {
        matches!(self, ToolError::Uncertain { .. })
    }
}

/// 取消信号。
///
/// kernel 不依赖 tokio（§13.4），所以这里只有一个 `AtomicBool`：**查询**取消状态在
/// 哪一层都能用，**等待**取消发生是 runtime 的事（它把这个 flag 和自己的 notify
/// 包在一起）。工具在长循环里查询它，子进程的回收由 runtime 负责。
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// 工具执行的上下文。**由 Gateway 创建，不能由模型参数提升**（§4）。
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub session: SessionId,
    pub run: RunId,
    pub call: ToolCallId,
    pub attempt: AttemptId,
    pub source: PlanSource,
    /// 本次执行的工作目录。
    pub cwd: PathBuf,
    /// 已授权的根目录：workspace、artifacts、skills（只读）等。
    pub roots: Vec<WorkspaceRoot>,
    /// 当前 Python 环境版本。
    pub env_version: Option<EnvVersion>,
    /// 这次 `execute` 是不是一次**恢复执行**（§8.4 第 6 / 7 行）。`None` = 首次。
    pub resumed: Option<ResumedCall>,
    pub cancel: CancelToken,
}

/// 这次执行是在接一次没有收尾的调用（§8.4、§8.6）。
///
/// **它在 `ToolContext` 上，不在 `ExecutionPlan` 里**，理由是硬的：审批绑定的是计划的
/// 哈希，而恢复执行用的必须是**同一份**计划——把"这是第二次"写进计划，哈希就变了，原
/// 来那条授权立刻覆盖不到它，重启之后每个等过审批的调用都要重新问一遍人（§7.4 明说
/// "审批无需用户因重启再答一次"）。计划回答"这个动作是什么、它可以怎样恢复"
/// （[`RecoveryMode`](super::plan::RecoveryMode)），上下文回答"这一次是第几次"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumedCall {
    /// 上一次尝试。计划已落盘但 `tool.started` 从未写过时为 `None`——那种情形下
    /// 一次尝试都还没有过。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_attempt: Option<AttemptId>,
    /// 上一次这个调用停在哪个状态。`Planned` = §8.4 第 6 行（**确定尚未执行**）；
    /// `Started` / `Uncertain` = 第 7 行（**先核对外部效果**）。
    pub previous_state: ToolCallState,
    /// 这个调用之前已经尝试过几次。重复尝试计入原任务预算（§8.6），所以它是个数字而
    /// 不是一个布尔。
    #[serde(default)]
    pub attempts_so_far: u32,
    /// 已经做过的核对的结论。
    ///
    /// `None` 表示还没核对过。工具**只有在这里读到
    /// [`Verification::NotPerformed`] 时**才可以把一个有副作用的动作重做一遍；读到
    /// [`Verification::AlreadySatisfied`] 就报告"核对后目标已满足"而不是重跑，读到
    /// `Conflict` / `Unknown` / `Unavailable` 就交给人（§8.6）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
}

impl ResumedCall {
    /// 确定这次动作还没发生过——§8.4 第 6 行，或核对给出了 `NotPerformed`。
    pub fn is_known_not_to_have_run(&self) -> bool {
        matches!(
            (&self.previous_state, &self.verification),
            (ToolCallState::Planned, _) | (_, Some(Verification::NotPerformed { .. }))
        )
    }
}

impl ToolContext {
    /// 这个路径落在哪个已授权的根里。落不进任何一个 → None（由 Policy 决定是 Ask
    /// 还是 Deny，工具自己不放行）。
    pub fn root_for(&self, path: &std::path::Path) -> Option<&WorkspaceRoot> {
        self.roots
            .iter()
            .filter(|root| path.starts_with(&root.path))
            .max_by_key(|root| root.path.as_os_str().len())
    }
}

/// 一个已授权的根目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRoot {
    pub path: PathBuf,
    pub writable: bool,
    /// 这个根是干什么的，只为界面和规则可读。
    pub label: String,
}

/// Python 的两种调用形式（§5.2）。两者属于同一个 `python` 工具。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum PythonJob {
    /// 任意代码。不会因为 import 了已审核模块而自动获得同样授权。
    Code { code: String },
    /// 已保存模块中**明确导出**的函数，按已审核版本执行；不是任意属性查找。
    Call {
        module: String,
        function: String,
        #[serde(default)]
        args: serde_json::Value,
    },
}

/// 一次 Python 执行的结构化返回（§5.1）。脚本的普通 print 单独收集，不混进控制协议。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PythonResult {
    pub status: ToolResultStatus,
    #[serde(default)]
    pub result: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ContentRef>,
    /// stdout 的**尾巴**（最多几 KiB），只给模型看的那份预览用。
    ///
    /// 脚本只 `print` 不返回结构化结果时，预览不能是字面量 `null`——模型看到它会以为工具
    /// 坏了，转头去 `shell` + `python3` 重跑一遍。完整 stdout 照旧在 `stdout.txt` 里。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout_tail: String,
    pub env_version: EnvVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PyError {
    #[error("解释器启动失败：{0}")]
    Spawn(String),
    #[error("协议错误：{0}")]
    Protocol(String),
    #[error("环境版本不匹配：计划 {planned}，当前 {current}")]
    EnvVersionMismatch { planned: String, current: String },
    #[error("已取消")]
    Cancelled,
    #[error("超时：{after_secs}s")]
    Timeout { after_secs: u64 },
    #[error("{0}")]
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancel_token_is_shared_by_its_clones() {
        let token = CancelToken::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
    }

    #[test]
    fn the_longest_matching_root_wins() {
        let ctx = ToolContext {
            session: SessionId::from_raw("s"),
            run: RunId::from_raw("r"),
            call: ToolCallId::from_raw("c"),
            attempt: AttemptId::from_raw("a"),
            source: PlanSource::Interactive {
                session: SessionId::from_raw("s"),
            },
            cwd: PathBuf::from("/home/u/ws"),
            roots: vec![
                WorkspaceRoot {
                    path: PathBuf::from("/home/u"),
                    writable: false,
                    label: "home".into(),
                },
                WorkspaceRoot {
                    path: PathBuf::from("/home/u/ws"),
                    writable: true,
                    label: "workspace".into(),
                },
            ],
            env_version: None,
            resumed: None,
            cancel: CancelToken::new(),
        };
        let root = ctx
            .root_for(std::path::Path::new("/home/u/ws/a.txt"))
            .unwrap();
        assert!(root.writable);
        assert!(ctx.root_for(std::path::Path::new("/etc/passwd")).is_none());
    }

    #[test]
    fn a_first_attempt_has_no_resumed_marker() {
        let json = r#"{"previous_state":"planned"}"#;
        let resumed: ResumedCall = serde_json::from_str(json).unwrap();
        assert_eq!(resumed.attempts_so_far, 0);
        assert!(resumed.previous_attempt.is_none());
        assert!(resumed.verification.is_none());
        assert!(
            resumed.is_known_not_to_have_run(),
            "planned 就是 §8.4 第 6 行的「确定尚未执行」"
        );
    }

    #[test]
    fn a_started_call_is_only_safe_to_redo_after_a_verification_says_so() {
        let mut resumed = ResumedCall {
            previous_attempt: Some(AttemptId::from_raw("attempt-1")),
            previous_state: ToolCallState::Started,
            attempts_so_far: 1,
            verification: None,
        };
        assert!(!resumed.is_known_not_to_have_run(), "还没核对，不能重做");

        resumed.verification = Some(Verification::AlreadySatisfied {
            evidence: "内容哈希已是预期值".into(),
        });
        assert!(
            !resumed.is_known_not_to_have_run(),
            "目标已满足不等于没发生过"
        );

        resumed.verification = Some(Verification::Unknown {
            reason: "远端没给幂等键".into(),
        });
        assert!(!resumed.is_known_not_to_have_run());

        resumed.verification = Some(Verification::NotPerformed {
            evidence: "文件仍是原内容".into(),
        });
        assert!(resumed.is_known_not_to_have_run());
    }

    #[test]
    fn a_python_job_round_trips_by_mode() {
        let job = PythonJob::Call {
            module: "toolbox.ha".into(),
            function: "turn_off".into(),
            args: serde_json::json!({"entity_id": "light.living_room"}),
        };
        let text = serde_json::to_string(&job).unwrap();
        assert!(text.contains("\"mode\":\"call\""), "{text}");
        assert_eq!(serde_json::from_str::<PythonJob>(&text).unwrap(), job);
    }
}
