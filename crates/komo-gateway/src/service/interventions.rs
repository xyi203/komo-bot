//! §7.5 的答复：两类 Intervention 走同一条路。
//!
//! 「统一成一个概念：**Intervention = 一条未完成的 Run 停在某个只有人能回答的问题上**。」
//! 审批那一类已经随 Policy 一起删掉（2026-10-11，§7）。这个文件把两类答复收在一处，
//! 因为 §7.5 第 3 条只允许一条路：**核对 → 按 §8.4 行事**。
//!
//! 两类各自的落点：
//!
//! | 种类 | 都答什么 | 落到哪儿 |
//! |---|---|---|
//! | `verify` | `satisfied` / `not_performed` / `abandon` | 给那次调用补一条结果，再按 §8.4 入队或收尾 |
//! | `blocked` | `resolve` / `abandon` | `resolve` = **重新观察 + 重新决策**，不强行放行 |
//!
//! **没有"我确认副作用已发生"这种结论**（§7.5）：`satisfied` 说的是"核对之后目标已经是
//! 那个样子"。操作者可能看错，而账本一旦这么记就再也纠不回来。

use std::sync::Arc;

use komo_kernel::protocol::http::{
    InterventionAnswerResponse, InterventionDetail, InterventionKind, InterventionListQuery,
    InterventionSummary, InterventionVerdict,
};
use komo_kernel::traits::GatewayError;
use komo_kernel::types::chat::PeerId;
use komo_kernel::types::ids::{AttemptId, RunId, SessionId, ToolCallId};
use komo_kernel::types::status::{RunEnd, RunState};
use komo_runtime::executor::OperatorVerdict;

use super::state::GatewayState;

/// 这一条现在允许答什么（§7.5）。界面照抄，不自己推——推错一个界面就会给出一个按下去
/// 没反应的答案。
pub fn verdicts_of(kind: InterventionKind) -> Vec<InterventionVerdict> {
    kind.verdicts()
}

/// 一条 Intervention 的问题正文（两类各自的"哪里不清楚"）。
pub fn question_of(detail: &InterventionDetail) -> String {
    match detail {
        InterventionDetail::Verify { tool, .. } => {
            format!("上一次那个 `{tool}` 调用**到底发生没有**？")
        }
        InterventionDetail::Blocked { reason, .. } => reason.clone(),
    }
}

impl GatewayState {
    /// 待处理清单（§7.5）。**派生视图**：从 `runs` 查出来，没有另一张表。
    pub async fn interventions(
        &self,
        query: &InterventionListQuery,
    ) -> Result<Vec<InterventionSummary>, GatewayError> {
        Ok(komo_store::repos::interventions::list(&self.db, query).await?)
    }

    /// 按句柄取一条详情。
    pub async fn intervention(
        &self,
        handle: &str,
    ) -> Result<Option<InterventionDetail>, GatewayError> {
        Ok(komo_store::repos::interventions::find(&self.db, handle).await?)
    }

    /// 答复一条 Intervention（§7.5）。按 `kind` 分派，两类共用这一条入口。
    ///
    /// 两件事的一致性由调用方看得到：**结论与种类不符**是一句明确的失败（
    /// [`GatewayError::InvalidRequest`]，HTTP 那一层映射成 422），**已经答过**则回原结论、
    /// 不报错（§11.3）。
    pub async fn answer_intervention(
        self: &Arc<Self>,
        handle: &str,
        verdict: InterventionVerdict,
        by: Option<PeerId>,
    ) -> Result<InterventionAnswerResponse, GatewayError> {
        let Some(found) = self.intervention(handle).await? else {
            return self.answer_settled(handle, verdict).await;
        };
        match found {
            InterventionDetail::Verify {
                summary,
                tool,
                reason,
                ..
            } => {
                if !verdict.allowed_for(InterventionKind::Verify) {
                    return Err(verdict_mismatch(InterventionKind::Verify, verdict));
                }
                self.answer_verify(&summary, &tool, &reason, verdict, by)
                    .await
            }
            InterventionDetail::Blocked { summary, reason } => {
                if !verdict.allowed_for(InterventionKind::Blocked) {
                    return Err(verdict_mismatch(InterventionKind::Blocked, verdict));
                }
                self.answer_blocked(&summary, &reason, verdict, by).await
            }
        }
    }

    /// `verify` 的答复（§7.5）：`satisfied` / `not_performed` 给那次调用补一条结果再入队，
    /// `abandon` 收尾。
    async fn answer_verify(
        self: &Arc<Self>,
        summary: &InterventionSummary,
        tool: &str,
        reason: &str,
        verdict: InterventionVerdict,
        by: Option<PeerId>,
    ) -> Result<InterventionAnswerResponse, GatewayError> {
        let (session, run) = self.summary_run(summary)?;
        if verdict == InterventionVerdict::Abandon {
            return self
                .abandon(
                    &summary.handle,
                    InterventionKind::Verify,
                    &session,
                    &run,
                    &format!("在结果不明上放弃：{reason}"),
                    by,
                )
                .await;
        }

        let call = summary.call.clone().ok_or_else(|| {
            GatewayError::Internal(format!(
                "{} 是 verify，却没记下停在哪一次调用上（§7.5 那张表的句柄）",
                summary.handle
            ))
        })?;
        let attempt = self.started_attempt(&call).await?;
        let operator = match verdict {
            InterventionVerdict::Satisfied => OperatorVerdict::AlreadySatisfied {
                evidence: evidence_of(&summary.handle, tool, verdict),
            },
            InterventionVerdict::NotPerformed => OperatorVerdict::NotPerformed {
                evidence: evidence_of(&summary.handle, tool, verdict),
            },
            other => return Err(verdict_mismatch(InterventionKind::Verify, other)),
        };
        // §7.5：结论只走一条路——**给那次调用补一条结果**。这一步不碰 Run 的状态。
        let status = self
            .handler
            .settle_by_operator(&session, &run, &call, &attempt, operator)
            .await
            .map_err(|error| GatewayError::Internal(error.to_string()))?;
        // 写完结果之后按 §8.4 重新入队：`satisfied` 让这一轮接着跑，`not_performed` 把那
        // 次调用当成一次明确的失败交回模型（**重做是新的一次调用**）。
        self.wake_run(&run).await?;

        Ok(InterventionAnswerResponse {
            handle: summary.handle.clone(),
            kind: InterventionKind::Verify,
            verdict,
            run_state: self.run_state_of(Some(&run)).await?,
            note: match verdict {
                InterventionVerdict::Satisfied => format!(
                    "`{tool}` 按「核对后目标已满足」收尾（{status:?}），这条 Run 回到队列。"
                ),
                _ => format!(
                    "`{tool}` 按「确定没有执行」收尾（{status:?}），这条 Run 回到队列——重做会是一次新调用。"
                ),
            },
            already_answered: false,
        })
    }

    /// `blocked` 的答复（§7.5）：`resolve` **不强行放行**，只要求重新观察 + 重新决策一次。
    async fn answer_blocked(
        self: &Arc<Self>,
        summary: &InterventionSummary,
        _reason: &str,
        verdict: InterventionVerdict,
        by: Option<PeerId>,
    ) -> Result<InterventionAnswerResponse, GatewayError> {
        let (session, run) = self.summary_run(summary)?;
        if verdict == InterventionVerdict::Abandon {
            return self
                .abandon(
                    &summary.handle,
                    InterventionKind::Blocked,
                    &session,
                    &run,
                    "前提没了，操作者放弃",
                    by,
                )
                .await;
        }

        // 「`resolve` 不强行放行，它只要求**重新观察并重新决策**一次」（§7.5 第 3 条）。
        // 这里就是那条路：跑一遍恢复扫描——同一个观察、同一张决策表，只是"我处理过了"
        // 这件事落到了事实那一侧。事实没变，它就还停在等人上，这是对的。
        let scan = self
            .recovery()
            .scan()
            .await
            .map_err(|error| GatewayError::Internal(error.to_string()))?;
        tracing::info!(handle = %summary.handle, runs = scan.outcomes.len(), "按操作者的要求重新观察并重新决策了一次");
        let state = self.run_state_of(Some(&run)).await?;
        let note = match state {
            Some(RunState::Queued) | Some(RunState::Running) => {
                "前提已经处理好了，这条 Run 回到队列。".to_string()
            }
            Some(RunState::Waiting) => {
                "重新看过一遍：前提仍然不成立，这条 Run 还在等人（原因见详情）。".to_string()
            }
            other => format!("重新看过一遍，这条 Run 现在是 {other:?}。"),
        };
        Ok(InterventionAnswerResponse {
            handle: summary.handle.clone(),
            kind: InterventionKind::Blocked,
            verdict: InterventionVerdict::Resolve,
            run_state: state,
            note,
            already_answered: false,
        })
    }

    /// `abandon`：这条 Run 到此为止。**与取消分开记**（§8.4 的四个终态）。
    async fn abandon(
        self: &Arc<Self>,
        handle: &str,
        kind: InterventionKind,
        session: &SessionId,
        run: &RunId,
        why: &str,
        by: Option<PeerId>,
    ) -> Result<InterventionAnswerResponse, GatewayError> {
        let state = self
            .finish_run(
                session,
                run,
                RunEnd::Abandoned {
                    by: Some(match by {
                        Some(peer) => peer.to_string(),
                        None => "operator".to_string(),
                    }),
                    reason: Some(why.to_string()),
                },
            )
            .await?;
        Ok(InterventionAnswerResponse {
            handle: handle.to_string(),
            kind,
            verdict: InterventionVerdict::Abandon,
            run_state: Some(state),
            note: format!("{run} 到此为止（abandoned），不会再有下文。"),
            already_answered: false,
        })
    }

    /// 句柄此刻不在清单里：**已经答过的返回原结论，不报错**（§11.3）。
    ///
    /// `verify` / `blocked` 没有一行"决定"：它们的结论落在**工具结果**与 **Run 的状态**上，
    /// 而句柄就是 Run ID——所以这里如实回答"这条 Run 现在是什么"，并在终态里把
    /// `abandoned` 认成 `abandon`（那是它唯一的终态结论）。
    async fn answer_settled(
        self: &Arc<Self>,
        handle: &str,
        verdict: InterventionVerdict,
    ) -> Result<InterventionAnswerResponse, GatewayError> {
        let run = RunId::from_raw(handle.to_string());
        let Some(record) = komo_store::repos::runs::get(&self.db, &run).await? else {
            return Err(GatewayError::NotFound {
                what: format!("Intervention {handle}"),
            });
        };
        let verdict = if record.state == RunState::Abandoned {
            InterventionVerdict::Abandon
        } else {
            verdict
        };
        Ok(InterventionAnswerResponse {
            handle: handle.to_string(),
            kind: InterventionKind::Verify,
            verdict,
            run_state: Some(record.state),
            note: format!(
                "{run} 已经答复过了（现在 {}），这里是原结论。",
                record.state.as_str()
            ),
            already_answered: true,
        })
    }

    /// 清单里的那一条必须说得出是哪一条 Run。
    fn summary_run(
        &self,
        summary: &InterventionSummary,
    ) -> Result<(SessionId, RunId), GatewayError> {
        let run = summary.run.clone().ok_or_else(|| {
            GatewayError::Internal(format!(
                "{} 是 {}，却没有 Run——清单里的每一条都挂在一条 Run 上（§7.5）",
                summary.handle,
                summary.kind.as_str()
            ))
        })?;
        Ok((summary.session.clone(), run))
    }

    /// 那条 `uncertain` 调用最后落下的尝试。核对结论写在**那一次尝试**上。
    async fn started_attempt(&self, call: &ToolCallId) -> Result<AttemptId, GatewayError> {
        komo_store::repos::calls::attempts_of(&self.db, call)
            .await?
            .into_iter()
            .next_back()
            .map(|row| AttemptId::from_raw(row.id))
            .ok_or_else(|| {
                GatewayError::Internal(format!(
                    "{call} 说结果不明，却一次尝试都没落下来——没有东西可以收尾"
                ))
            })
    }

    /// 这条 Run 现在在哪个状态。
    async fn run_state_of(&self, run: Option<&RunId>) -> Result<Option<RunState>, GatewayError> {
        let Some(run) = run else {
            return Ok(None);
        };
        Ok(komo_store::repos::runs::get(&self.db, run)
            .await?
            .map(|record| record.state))
    }
}

/// 结论文本里那句"凭什么"（§7.5：证据要进正文）。
fn evidence_of(handle: &str, tool: &str, verdict: InterventionVerdict) -> String {
    format!(
        "操作者在 Intervention 清单上答复 `{}`（{handle}，工具 {tool}）",
        verdict.as_str()
    )
}

/// 结论不属于这个种类（§7.5：不混用）。HTTP 那一层把它映射成 422。
fn verdict_mismatch(kind: InterventionKind, verdict: InterventionVerdict) -> GatewayError {
    GatewayError::InvalidRequest(format!(
        "这条是 {}，答不了 `{}`；它此刻能答的是：{}",
        kind.as_str(),
        verdict.as_str(),
        kind.verdicts()
            .iter()
            .map(|one| format!("`{}`", one.as_str()))
            .collect::<Vec<_>>()
            .join(" / ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 结论与种类不符时说得出**能答什么**——回执要能直接递给操作者。
    #[test]
    fn a_mismatched_verdict_names_what_this_kind_accepts() {
        let error = verdict_mismatch(InterventionKind::Verify, InterventionVerdict::Resolve);
        let GatewayError::InvalidRequest(message) = error else {
            panic!("应当是 InvalidRequest");
        };
        assert!(message.contains("verify"), "{message}");
        assert!(message.contains("satisfied"), "{message}");
        assert!(message.contains("not_performed"), "{message}");
        assert!(message.contains("abandon"), "{message}");
    }

    #[test]
    fn every_kind_gets_the_verdicts_the_kernel_lists() {
        assert_eq!(
            verdicts_of(InterventionKind::Verify),
            vec![
                InterventionVerdict::Satisfied,
                InterventionVerdict::NotPerformed,
                InterventionVerdict::Abandon
            ]
        );
        assert_eq!(
            verdicts_of(InterventionKind::Blocked),
            vec![InterventionVerdict::Resolve, InterventionVerdict::Abandon]
        );
    }
}
