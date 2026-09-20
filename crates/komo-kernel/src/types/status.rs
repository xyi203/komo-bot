//! Run / ToolCall / Attempt 的状态机（§6、§8.1、§8.4）。

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::ids::{ApprovalId, EventId, InterventionId, RunId};

/// Run 的生命周期状态（§8.4）。
///
/// **状态回答"能不能跑"，[`WaitReason`] 回答"为什么不能跑"。** 两个维度分开，是因为它们
/// 混在一起时的症状正是"排队 20 分钟不知道为什么"：一个 `waiting_approval`、一个
/// `needs_attention`，都答不出"它在等什么、等到什么时候"。
///
/// ```text
///                       ┌─────────────┐
///                       │  Accepted   │  已受理：ID 预留好了，输入正文可能还没写完（§8.5）
///                       └──────┬──────┘
///                              │ 正文落盘 + queued 提交
///                       ┌──────▼──────┐
///                       │   Queued    │  现在就能跑，只缺 worker
///                       └──────┬──────┘
///                              │ claim
///                       ┌──────▼──────┐
///                  ┌────│   Running   │────┐
///                  │    └──────┬──────┘    │
///              停在外因         │ 正常收尾   │ 出错 / 取消 / 放弃
///                  │           │           │
///                  ▼           ▼           ▼
///            ┌───────────┐  Completed   Failed / Cancelled / Abandoned
///            │  Waiting  │  （+ `WaitReason` 说出在等什么）
///            └─────┬─────┘
///                  │ 条件满足：审批答复 / 到点 / 干预答复 / 前一条 Run 终态
///                  ▼
///                Queued
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// 已受理：请求键预留了 Run ID，输入正文**可能还没写完**（§8.5）。**不可领取**——
    /// 少了这一档，"正文还没落盘"会被当成"可以跑了"，而模型会拿到一个半截的输入。
    Accepted,
    /// **现在就能跑，只缺 worker。** `WaitReason` 对它没有意义。
    ///
    /// 这是这套状态机唯一的验收口径：`Queued` 的 Run 一定有活干，等着的 Run 一定说得出
    /// 在等什么。查"它怎么还不跑"时，这两句话各回答一半。
    Queued,
    /// 正在执行。`claimed_by IS NULL` 的 `Running` 是**已经被回收、还没判完**的孤儿
    /// （§8.9）：它不可领取，等 reconcile 按 §8.4 判成 `Queued` 或 `Waiting`。
    Running,
    /// 当前不能跑，且必须说得出在等什么（[`WaitReason`]）。让出执行名额，但**仍然挡住
    /// 同 Session 后面的 Run**（§8.4）。
    Waiting,
    /// 正常结束。
    Completed,
    /// 有明确的错误终态。
    Failed,
    /// 用户明确取消，不自动复活（§8.4）。
    Cancelled,
    /// 操作者在 Intervention 清单上放弃（§7.5 的 `abandon`）。与 `Cancelled` 分开记：
    /// 它不是"用户不想跑了"，而是"这件事不会再有下文了"，事后统计要分得开。
    Abandoned,
}

impl RunState {
    /// 终态：重启不会让它再开一轮。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunState::Completed | RunState::Failed | RunState::Cancelled | RunState::Abandoned
        )
    }

    /// 未完成状态。§8.4：同一 Session 的后续 Run 不越过它。
    pub fn is_unfinished(self) -> bool {
        !self.is_terminal()
    }

    /// 还在等领取这一段：`Accepted` 的输入还没落全，`Queued` 才是"缺 worker"。
    ///
    /// 回放窗口要跳过这一种的用户消息。照搬日志位置发出去，provider 看到的是"助手要了
    /// 一次调用、紧接着另一个 Run 的用户消息、最后才是那次调用的输出"，直接 400
    /// （`No tool output found for tool call …`）。
    pub fn awaits_claim(self) -> bool {
        matches!(self, RunState::Accepted | RunState::Queued)
    }

    /// 停在等待上——让出执行名额，但**仍然挡住同 Session 后面的 Run**。
    pub fn is_waiting(self) -> bool {
        matches!(self, RunState::Waiting)
    }

    /// 可以被调度器领取。
    pub fn is_claimable(self) -> bool {
        matches!(self, RunState::Queued)
    }

    /// 数据库那一列（`runs.state`）与日志里的写法。
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Accepted => "accepted",
            RunState::Queued => "queued",
            RunState::Running => "running",
            RunState::Waiting => "waiting",
            RunState::Completed => "completed",
            RunState::Failed => "failed",
            RunState::Cancelled => "cancelled",
            RunState::Abandoned => "abandoned",
        }
    }

    /// 认不出就 `None`——调用方决定那是损坏还是用法错误，这里不替它挑一个默认值
    /// （把"未来的状态"读成 `queued` 会让一条不该跑的 Run 被领走）。
    pub fn parse(raw: &str) -> Option<RunState> {
        match raw.trim() {
            "accepted" => Some(RunState::Accepted),
            "queued" => Some(RunState::Queued),
            "running" => Some(RunState::Running),
            "waiting" => Some(RunState::Waiting),
            "completed" => Some(RunState::Completed),
            "failed" => Some(RunState::Failed),
            "cancelled" => Some(RunState::Cancelled),
            "abandoned" => Some(RunState::Abandoned),
            _ => None,
        }
    }
}

/// Session 的生命周期（§8.10）。**删内容只能是最后一步，而且必须有人明确下令**——
/// 删会话今天等于 `rm -rf sessions/{id}`：数据库那一行还在、未完成的 Run 还在队列里、
/// 下一轮模型请求带着空上下文就跑起来了，而操作者手上没有任何一条命令能做对这件事。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// 默认。接受新输入，Run 正常排队。
    Active,
    /// 逻辑删除已受理：不再接受新输入，未完成的 Run 照 §8.4 走完或停在等待，**内容不动**。
    /// 由 `komo session delete` 置入；由 reconcile 在"已无未完成 Run"时推进到 `Deleted`。
    Closing,
    /// 逻辑删除已完成：内容仍在，但不再服务——列出默认隐藏、resume 拒绝、输入拒绝。
    Deleted,
    /// 内容已回收，只剩墓碑行。`jsonl_path` 这类列保留原值，但那个目录不该再被创建或读取。
    Purged,
}

impl SessionState {
    /// 还能不能接受新输入。只有 `Active` 可以（§8.10）。
    pub fn accepts_input(self) -> bool {
        matches!(self, SessionState::Active)
    }

    /// 还在服务范围里——reconcile 用它决定"这条 Run 能不能按 §8.4 继续"（§8.9）。
    /// `Closing` 仍然服务：它只是不再收新活，手里的活要跑完。
    pub fn serves(self) -> bool {
        matches!(self, SessionState::Active | SessionState::Closing)
    }

    /// 内容还应该在吗。`Purged` 之外都在：逻辑删除不碰内容（§8.10）。
    pub fn content_expected(self) -> bool {
        !matches!(self, SessionState::Purged)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Active => "active",
            SessionState::Closing => "closing",
            SessionState::Deleted => "deleted",
            SessionState::Purged => "purged",
        }
    }

    /// 线格式 / CLI 上的写法。**认不出就 None**——调用方决定那是损坏还是用法错误，
    /// 这里不替它挑一个默认值（"未来的状态"降级成 `active` 会把墓碑读成活的）。
    pub fn parse(raw: &str) -> Option<SessionState> {
        match raw.trim() {
            "active" => Some(SessionState::Active),
            "closing" => Some(SessionState::Closing),
            "deleted" => Some(SessionState::Deleted),
            "purged" => Some(SessionState::Purged),
            _ => None,
        }
    }
}

/// ToolCall 的状态（§8.4）。`uncertain` 是这里的状态，不是 Run 的——副作用可能已经
/// 发生而完整输出没有落盘，这段窗口必须保留，不能用"重试成功"盖掉（§8.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallState {
    /// 计划已持久保存，确定尚未执行。
    Planned,
    /// `tool.started` 已提交；此后才允许产生真实副作用。
    Started,
    Completed,
    Failed,
    /// 结果不明：先核对目标状态，再决定是否重试（§8.6）。
    Uncertain,
}

impl ToolCallState {
    pub fn is_terminal(self) -> bool {
        matches!(self, ToolCallState::Completed | ToolCallState::Failed)
    }

    /// 需要走 §8.6 的核对流程。
    pub fn needs_verification(self) -> bool {
        matches!(self, ToolCallState::Started | ToolCallState::Uncertain)
    }
}

/// 一次实际执行尝试的状态（`tool_attempts`）。一次重试沿用 ToolCall ID，新增一条
/// attempt（§8.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Started,
    Completed,
    Failed,
    /// 执行实例没有收尾就消失了（停机 / 崩溃）。
    Interrupted,
}

impl AttemptState {
    pub fn is_terminal(self) -> bool {
        matches!(self, AttemptState::Completed | AttemptState::Failed)
    }
}

/// **为什么现在不能跑**（§8.4）。与 [`RunState::Waiting`] 成对出现。
///
/// 它落进 `runs` 的三列——`wait_kind` / `wait_ref` / `wake_at`——而不是一个序列化的大
/// 枚举。这样"哪些 Run 在等人"、"哪些 Run 到点了"都是普通查询，reconcile、CLI 与
/// `/v1/interventions` 都不必反序列化一堆状态才能问出一句话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitReason {
    /// 等一份执行计划的答复（§7.4、§7.5）。**释放执行名额，不挂着 future 等人**——
    /// 挂在内存里等，进程一重启这次的等待就没了，还会白占一个并发槽。
    Approval { approval: ApprovalId },
    /// 等一次有界退避到期。
    ///
    /// **只用于模型 / 驱动侧那些可以安全重试的失败**（限流、连不上、5xx、写争用）。
    /// 工具那一侧"结果不明"**不能**用它：那是副作用有没有发生都不知道，必须进
    /// [`WaitReason::Intervention`] 让人核对（§8.6）——两者混起来就是"重试成功"掩盖
    /// 掉窗口，正好是那一条要禁止的事。
    Retry {
        /// 已经用掉的次数。**重启不重置预算**（§8.5）。
        attempts: u32,
        #[serde(with = "time::serde::rfc3339")]
        not_before: OffsetDateTime,
        cause: RetryCause,
    },
    /// 停在一条 Intervention 上等人答复（§7.5）：结果不明，或者前提没了。
    Intervention { intervention: InterventionId },
    /// 在等同 Session 里**更早**的那条 Run（§8.4：后面的 Run 不越过前面的）。
    ///
    /// 它**不是"等人"**，所以不进 Intervention 清单；但它必须被说出来，否则就是
    /// "排队 20 分钟不知道为什么"。前一条一进终态，这一条就回 `Queued`。
    Dependency { run: RunId },
}

/// 一次可安全重试的退避是**哪一种**失败（§8.5）。
///
/// 分类不是为了好看：`RateLimited` 要尊重服务端给的 `Retry-After`，`Contended` 是本地
/// 数据库写争用（退避几毫秒就够），而"服务端 5xx"要退得比它久。三者混成一个数字，就
/// 只能取最保守的那个，每次 429 都白等。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryCause {
    /// 服务端限流（429 / `Retry-After`）。
    RateLimited,
    /// 连不上 / 连接中断 / 响应没收全。
    Transport,
    /// 5xx 一类的服务端错误。
    Server,
    /// 本地写争用（MVCC 冲突重试超限，§8.2）。
    Contended,
}

impl RetryCause {
    pub fn as_str(self) -> &'static str {
        match self {
            RetryCause::RateLimited => "rate_limited",
            RetryCause::Transport => "transport",
            RetryCause::Server => "server",
            RetryCause::Contended => "contended",
        }
    }

    pub fn parse(raw: &str) -> Option<RetryCause> {
        match raw.trim() {
            "rate_limited" | "rate-limited" => Some(RetryCause::RateLimited),
            "transport" => Some(RetryCause::Transport),
            "server" => Some(RetryCause::Server),
            "contended" => Some(RetryCause::Contended),
            _ => None,
        }
    }
}

impl WaitReason {
    /// `runs.wait_kind` 的那一段。
    pub fn kind(&self) -> &'static str {
        match self {
            WaitReason::Approval { .. } => "approval",
            WaitReason::Retry { .. } => "retry",
            WaitReason::Intervention { .. } => "intervention",
            WaitReason::Dependency { .. } => "dependency",
        }
    }

    /// `runs.wait_ref` 的那一段：`retry` 没有引用（它的进度是次数 + 到点时刻）。
    pub fn reference(&self) -> Option<String> {
        match self {
            WaitReason::Approval { approval } => Some(approval.to_string()),
            WaitReason::Intervention { intervention } => Some(intervention.to_string()),
            WaitReason::Dependency { run } => Some(run.to_string()),
            WaitReason::Retry { .. } => None,
        }
    }

    /// `runs.wake_at`：到点才能再跑的那一种等待。
    pub fn wake_at(&self) -> Option<OffsetDateTime> {
        match self {
            WaitReason::Retry { not_before, .. } => Some(*not_before),
            _ => None,
        }
    }

    /// 这一种等待**要不要人**。它决定这条 Run 进不进 §7.5 的清单：`Approval` 与
    /// `Intervention` 进，`Retry`（等时钟）与 `Dependency`（等前一条 Run）不进。
    pub fn needs_a_person(&self) -> bool {
        matches!(
            self,
            WaitReason::Approval { .. } | WaitReason::Intervention { .. }
        )
    }

    /// 它把 Run 放进哪个状态。**永远只有一个答案**——这正是把它拆出来的意义：状态机
    /// 那一层不必再枚举"等审批 / 等重试 / 等干预"三种停法。
    pub fn state(&self) -> RunState {
        RunState::Waiting
    }
}

/// `Ledger::complete` 的参数：Run 怎么结束的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunEnd {
    /// 正常结束，带最终回复正文（大正文由调用方先外置）。
    Completed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_message: Option<String>,
        /// 这个 Run 一共跑了几轮模型。`run inspect` 与预算核对读它——`RunCompleted.rounds`
        /// 原先恒为 0，因为账本接口上根本没地方把这个数交进来。
        #[serde(default)]
        rounds: u32,
    },
    Failed {
        reason: String,
    },
    /// 用户明确取消。
    Cancelled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    /// 操作者在 Intervention 清单上放弃（§7.5）：**不是**用户取消，而是"这件事不再
    /// 推进了"——比如一条停在结果不明上的 Run，操作者查过之后决定不追究。分开记，
    /// 事后统计"多少人放弃了什么"才有意义。
    Abandoned {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl RunEnd {
    pub fn state(&self) -> RunState {
        match self {
            RunEnd::Completed { .. } => RunState::Completed,
            RunEnd::Failed { .. } => RunState::Failed,
            RunEnd::Cancelled { .. } => RunState::Cancelled,
            RunEnd::Abandoned { .. } => RunState::Abandoned,
        }
    }
}

/// 调度器领取到的一个 Run（§8.7：条件更新 + 递增代次）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claimed {
    pub run: RunId,
    /// 领取代次。JSONL 追加与状态提交都校验它；旧代次不能继续提交新状态。
    pub generation: u64,
}

/// Run 的最终事件引用，用于"结果已保存但客户端没收到"的补读（§8.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalEventRef {
    pub event: EventId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::ToolCallId;
    use time::macros::datetime;

    /// 终态**恰好四个**，而且状态机里没有"停在外面却说不出在等什么"的那种状态。
    #[test]
    fn exactly_four_run_states_are_terminal_and_only_one_waits() {
        let all = [
            RunState::Accepted,
            RunState::Queued,
            RunState::Running,
            RunState::Waiting,
            RunState::Completed,
            RunState::Failed,
            RunState::Cancelled,
            RunState::Abandoned,
        ];
        let terminal: Vec<_> = all.into_iter().filter(|s| s.is_terminal()).collect();
        assert_eq!(
            terminal,
            vec![
                RunState::Completed,
                RunState::Failed,
                RunState::Cancelled,
                RunState::Abandoned
            ]
        );
        assert!(all.iter().all(|s| s.is_terminal() != s.is_unfinished()));
        let waiting: Vec<_> = all.into_iter().filter(|s| s.is_waiting()).collect();
        assert_eq!(waiting, vec![RunState::Waiting], "停着等人的只有这一档");
        let claimable: Vec<_> = all.into_iter().filter(|s| s.is_claimable()).collect();
        assert_eq!(
            claimable,
            vec![RunState::Queued],
            "Accepted 的正文还没落全，不许领"
        );
    }

    /// §8.4：`Queued` = 现在就能跑；`Waiting` = 现在不能跑**且说得出在等什么**。
    /// 两种等待不进清单（等时钟、等前一条 Run），两种进（等人）。
    #[test]
    fn a_wait_reason_says_who_is_being_waited_on() {
        let approval = WaitReason::Approval {
            approval: ApprovalId::from_raw("ap-1"),
        };
        assert_eq!(approval.state(), RunState::Waiting);
        assert_eq!(approval.kind(), "approval");
        assert_eq!(approval.reference().as_deref(), Some("ap-1"));
        assert!(approval.wake_at().is_none(), "审批没有到点时刻，只有答复");
        assert!(approval.needs_a_person());

        let retry = WaitReason::Retry {
            attempts: 2,
            not_before: datetime!(2026-09-15 08:05:00 UTC),
            cause: RetryCause::RateLimited,
        };
        assert!(!retry.needs_a_person(), "等时钟不是等人");
        assert_eq!(retry.reference(), None, "退避的进度是次数 + 到点时刻");
        assert_eq!(retry.wake_at(), Some(datetime!(2026-09-15 08:05:00 UTC)));

        let dependency = WaitReason::Dependency {
            run: RunId::from_raw("run-1"),
        };
        assert!(!dependency.needs_a_person(), "等前一条 Run 不是等人");
        assert_eq!(dependency.reference().as_deref(), Some("run-1"));

        let intervention = WaitReason::Intervention {
            intervention: InterventionId::for_run(&RunId::from_raw("run-2")),
        };
        assert!(intervention.needs_a_person());
        assert_eq!(intervention.reference().as_deref(), Some("run-2"));
        assert_eq!(intervention.state(), RunState::Waiting);
    }

    /// 一次退避是**哪一种**失败要说得出来——`RateLimited` 该尊重服务端给的时刻，
    /// 本地写争用退几毫秒就够（§8.5）。
    #[test]
    fn a_retry_cause_survives_the_round_trip() {
        let retry = WaitReason::Retry {
            attempts: 1,
            not_before: datetime!(2026-09-15 08:00:30 UTC),
            cause: RetryCause::Contended,
        };
        let text = serde_json::to_string(&retry).unwrap();
        assert!(text.contains("contended"), "{text}");
        assert_eq!(serde_json::from_str::<WaitReason>(&text).unwrap(), retry);
        assert_eq!(
            RetryCause::parse("rate-limited"),
            Some(RetryCause::RateLimited)
        );
        assert_eq!(RetryCause::parse("teapot"), None);
    }

    #[test]
    fn started_and_uncertain_calls_both_need_verification() {
        assert!(ToolCallState::Started.needs_verification());
        assert!(ToolCallState::Uncertain.needs_verification());
        assert!(!ToolCallState::Completed.needs_verification());
        assert!(!ToolCallState::Planned.needs_verification());
    }

    /// 放弃与取消分得开，而且四个终态各自说得出自己叫什么。
    #[test]
    fn giving_up_is_not_the_same_as_being_cancelled() {
        let abandon = RunEnd::Abandoned {
            by: Some("operator".into()),
            reason: Some("查过了，不再追究".into()),
        };
        assert_eq!(abandon.state(), RunState::Abandoned);
        assert_ne!(RunEnd::Cancelled { by: None }.state(), abandon.state());

        let old = r#"{"kind":"completed","final_message":"好了"}"#;
        assert_eq!(
            serde_json::from_str::<RunEnd>(old).unwrap(),
            RunEnd::Completed {
                final_message: Some("好了".into()),
                rounds: 0
            },
            "老行没有 rounds，读为默认"
        );

        // 认不出的状态词不许默认成"能跑"。
        assert_eq!(RunState::parse("needs_attention"), None);
        assert_eq!(RunState::parse(" waiting "), Some(RunState::Waiting));
        assert_eq!(RunState::Waiting.as_str(), "waiting");
        assert_eq!(
            serde_json::to_string(&RunState::Abandoned).unwrap(),
            "\"abandoned\""
        );
    }

    /// 一个 Run 上最多一条要人判断的 Intervention，所以 Run ID 就是它的句柄（§7.5）。
    #[test]
    fn an_intervention_handle_falls_back_to_its_run() {
        let run = RunId::from_raw("run-9");
        assert_eq!(InterventionId::for_run(&run).as_str(), "run-9");
        assert_eq!(
            InterventionId::from_raw("7K2M").as_str(),
            "7K2M",
            "审批类用短 ID"
        );
        let _ = ToolCallId::from_raw("call-1");
    }

    /// §8.10：只有 `active` 收新输入；`closing` 仍然服务（手里的活要跑完）；
    /// **只有 `purged` 才意味着内容不该还在**——逻辑删除不碰内容。
    #[test]
    fn the_session_ladder_separates_input_service_and_content() {
        use SessionState::*;
        let all = [Active, Closing, Deleted, Purged];
        let accepts: Vec<_> = all.iter().copied().filter(|s| s.accepts_input()).collect();
        assert_eq!(accepts, vec![Active], "只有活着的会话收新输入");
        let serves: Vec<_> = all.iter().copied().filter(|s| s.serves()).collect();
        assert_eq!(serves, vec![Active, Closing], "closing 手里的活要跑完");
        let content: Vec<_> = all
            .iter()
            .copied()
            .filter(|s| s.content_expected())
            .collect();
        assert_eq!(
            content,
            vec![Active, Closing, Deleted],
            "逻辑删除不碰内容，回收才碰"
        );

        assert_eq!(SessionState::parse("closing"), Some(Closing));
        assert_eq!(SessionState::parse(" archived "), None, "认不出就是认不出");
        assert_eq!(serde_json::to_string(&Purged).unwrap(), "\"purged\"");
        assert_eq!(
            serde_json::from_str::<SessionState>("\"deleted\"").unwrap(),
            Deleted
        );
    }
}
