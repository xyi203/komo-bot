//! Session 生命周期与对账（§8.9、§8.10）。
//!
//! 两件事合在一个文件里，因为它们是同一个问题的两半：**删一个会话**（逻辑删除 → 回收
//! 内容）与**把"数据库说有、内容说没有"的那些半个事实收拾干净**（reconcile）。§8.10 的
//! 状态阶梯只有三个入口（`delete`、`delete --now`、`purge`），而 `closing → deleted` 与
//! "墓碑已落而内容还在"这两步**不是命令**，是对账按事实推进的（§8.10 第 2、3 条）。
//!
//! 一条贯穿全模块的规则：**数据库先提交，内容后删**（§8.10 第 3 条）。`purged` 这个墓碑
//! 一旦落下就声明了"这个目录要没了"，所以删一半被杀不会留下说不清的状态——重跑一次
//! 就收尾（幂等）。

use std::sync::Arc;

use komo_kernel::protocol::http::{
    PurgeBlocker, PurgeSessionResponse, ReconcileResponse, SessionLifecycleResponse,
};
use komo_kernel::traits::{GatewayError, Ledger, StoreError};
use komo_kernel::types::ids::{EventId, RunId, SessionId};
use komo_kernel::types::status::{RunEnd, RunState, SessionState};
use komo_runtime::recovery::Applied;
use time::OffsetDateTime;

use super::state::GatewayState;

/// 一次对账判定了什么。与 [`ReconcileResponse`] 一一对应，日志里也用它。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// 看了多少条非终态 Run。
    pub checked: u32,
    /// 交给 §8.4 决策表继续的（补了索引、放回队列）。
    pub resumed: u32,
    /// 停成 `blocked` Intervention 的。
    pub blocked: u32,
    /// 没主人的 `running` 回收了几条（只交还领取权）。
    pub reclaimed: u32,
    /// `closing → deleted` 推进的会话数。
    pub closed: u32,
    /// 墓碑已落、内容还没删完，这次补齐的会话数。
    pub purged: u32,
}

impl ReconcileReport {
    pub fn response(self, finished_at: OffsetDateTime) -> ReconcileResponse {
        ReconcileResponse {
            checked: self.checked,
            resumed: self.resumed,
            blocked: self.blocked,
            reclaimed: self.reclaimed,
            closed: self.closed,
            purged: self.purged,
            finished_at,
        }
    }
}

impl GatewayState {
    /// 立刻跑一次对账（§8.9）。**幂等**：同一批输入跑十遍与跑一遍结果相同。
    ///
    /// 六个动作，对应 §8.9 那张表与 §8.10 的两步状态推进：
    ///
    /// 1. **回收没有主人的 `running`**（只交还领取权，不判状态）：别的实例遗留的一次性
    ///    交还；自己手里的那一批必须先过**存活判定**——租约过期只是信号，`InFlight` 里
    ///    还在的那些（一次二十分钟的调用）一条都不动，否则那不是恢复，是重复副作用（§8.6）。
    /// 2. **会话可服务 + 内容可读**：不行的不许领，停成 `waiting + intervention`。这一维由
    ///    恢复扫描的观察给出（`RecoveryIndex::session_state` + 扫描自己读 JSONL），决策仍
    ///    是 kernel 那张表（`WaitingForOperator`）。
    /// 3. **`retry` 到点 → 回 `queued`**、**`dependency` 的前一条进终态 → 回 `queued`**。
    ///    两条都不是"等人"，所以它们不进 §7.5 的清单，得有这条机械的放行路径。
    /// 4. **`closing` 且已无未完成 Run → `deleted`**：这是判定，不是时钟（§8.10 第 2 条）。
    /// 5. **墓碑已落、内容还在 → 补齐回收**（幂等；"删一半被杀"就靠它收尾）。
    ///
    /// 它**永远不做四件事**：不调用工具、不发送外部请求、不消费授权、不改写任何内容。
    pub async fn reconcile(self: &Arc<Self>) -> Result<ReconcileReport, GatewayError> {
        Ok(self.reconcile_reporting().await?.0)
    }

    /// 跑一次对账，并把恢复扫描那份逐 Run 的报告一并交出来。
    ///
    /// 只有**启动**要这一份：§8.7 的启动顺序在扫描之后还有两步"不影响调度事实"的事——
    /// 补发上一次没送到的结果、把损坏的会话报给操作者（§8.4 第 10 行、§8.5）。对账本身
    /// **不发任何外部请求**（§8.9），所以那两步留在调用方，不塞进 [`Self::reconcile`]。
    pub async fn reconcile_reporting(
        self: &Arc<Self>,
    ) -> Result<(ReconcileReport, komo_runtime::recovery::RecoveryReport), GatewayError> {
        let now = self.clock.now();
        let mut report = ReconcileReport {
            reclaimed: self.reclaim_orphans(now).await?,
            ..Default::default()
        };

        let scan = self
            .recovery()
            .scan()
            .await
            .map_err(|error| GatewayError::Internal(error.to_string()))?;
        report.checked = scan.outcomes.len() as u32;
        report.blocked = scan
            .outcomes
            .iter()
            .filter(|outcome| outcome.applied == Applied::NeedsOperator)
            .count() as u32;
        report.resumed = scan
            .outcomes
            .iter()
            .filter(|outcome| outcome.applied == Applied::Requeued)
            .count() as u32;

        // 到点的退避与放得开的依赖。
        report.resumed += self.release_due_waits(now).await?;
        let released =
            komo_store::repos::queue::release_satisfied_dependencies(&self.db, now).await? as u32;
        report.resumed += released;

        // `closing → deleted`（§8.10 第 2 条）。
        for session in komo_store::repos::reconcile::closing_sessions_to_close(&self.db).await? {
            if self
                .set_session_state(&session, SessionState::Closing, SessionState::Deleted, now)
                .await?
            {
                report.closed += 1;
            }
        }

        // 墓碑已落而内容还在：补齐回收（幂等）。
        let root = self.snapshot().paths.sessions_dir.clone();
        for session in
            komo_store::repos::reconcile::purged_sessions_with_content(&self.db, &root).await?
        {
            let paths = komo_store::SessionPaths::new(&root, &session);
            match komo_store::repos::reconcile::remove_session_content(&paths).await {
                Ok(_) => report.purged += 1,
                // 「删不掉就说清楚，不假装成功」（§8.8）。下一拍还会再来。
                Err(error) => {
                    tracing::warn!(%error, %session, "墓碑已落但内容删不掉，下一拍再试")
                }
            }
        }

        // 一条汇总日志：**看了多少、判了什么**。它是对账唯一的可观察面（另一面是返回值）。
        tracing::info!(
            checked = report.checked,
            resumed = report.resumed,
            blocked = report.blocked,
            reclaimed = report.reclaimed,
            closed = report.closed,
            purged = report.purged,
            "对账完成"
        );
        Ok((report, scan))
    }

    /// `POST /v1/reconcile` 的返回值。
    pub async fn reconcile_response(self: &Arc<Self>) -> Result<ReconcileResponse, GatewayError> {
        let at = self.clock.now();
        Ok(self.reconcile().await?.response(at))
    }

    /// 回收**没有主人**的 `running`（§8.7 / §8.9）。返回回收了几条。
    ///
    /// 两个来源，一个判据：
    ///
    /// - 别的实例遗留的（`claimed_by <> 我`）：交还领取权即可——那个实例已经不在（数据
    ///   目录的进程锁是我们拿着的，§3）。
    /// - 自己持有的而租约过期了：**逐条过存活判定**——还在 [`InFlight`] 里的那些（一次
    ///   二十分钟的调用）一条都不动。少这一道，对账会把一个活着的长调用当成孤儿，那不是
    ///   恢复，是真的重复副作用（§8.6）。
    async fn reclaim_orphans(&self, now: OffsetDateTime) -> Result<u32, GatewayError> {
        let mut reclaimed =
            komo_store::repos::queue::reclaim_unowned(&self.db, &self.executor).await? as u32;

        let expired = komo_store::repos::queue::expired_lease_running(&self.db, now).await?;
        for run in expired {
            if self.in_flight.holds(&run) {
                // 还在本进程手里：它的租约没在续（心跳停了、handler 卡住了）。**不动它**
                // ——把一条活着的长调用交还领取权，下一次领取就是第二次副作用。
                tracing::warn!(%run, "这条 Run 还在本进程手里但租约过期了：不回收");
                continue;
            }
            if komo_store::repos::queue::reclaim_lease(&self.db, &run, &self.executor, now).await? {
                reclaimed += 1;
            }
        }
        Ok(reclaimed)
    }

    /// 到点的退避放回 `queued`（§8.9 第三行）。返回放了几条。
    ///
    /// 领取语句本来也认"`waiting + retry` 且 `wake_at <= now`"，所以这不是它能不能跑的
    /// 前提，而是**它现在是什么**的问题：§8.4 的验收口径要求 `Queued` 的 Run 一定说得出
    /// "现在就能跑"，而一条到点还写着 `waiting` 的 Run 在 `session list` 上要看两列才答得
    /// 出这件事。
    async fn release_due_waits(&self, now: OffsetDateTime) -> Result<u32, GatewayError> {
        let mut released = 0;
        for run in komo_store::repos::runs::unfinished(&self.db).await? {
            let due = matches!(
                &run.wait,
                Some(komo_kernel::types::status::WaitReason::Retry { not_before, .. })
                    if *not_before <= now
            );
            if !due {
                continue;
            }
            self.recovery_store.requeue(&run.run).await?;
            released += 1;
        }
        if released > 0 {
            self.waker().wake();
        }
        Ok(released)
    }

    /// `POST /v1/sessions/{id}/delete`（§8.10）。
    ///
    /// 逻辑删除**不碰内容**。`now = false` 进 `closing`：未完成的 Run 照 §8.4 跑完或停在
    /// 等待，只是**不新开**；`now = true` 则先把未完成的 Run 各写一条明确的取消，再进
    /// `deleted`。
    ///
    /// 已经 `deleted` / `purged` 的会话再删一次是**幂等**的：返回它现在的状态、`cancelled`
    /// 为空，不报错也不倒退状态（CAS 天然如此，§8.10 第 1 条）。
    pub async fn delete_session(
        self: &Arc<Self>,
        session: &SessionId,
        now: bool,
    ) -> Result<SessionLifecycleResponse, GatewayError> {
        let at = self.clock.now();
        let record = self.session_record(session).await?;
        let mut cancelled = Vec::new();

        if now {
            for run in self.unfinished_of(session).await? {
                self.finish_run(
                    session,
                    &run,
                    RunEnd::Cancelled {
                        by: Some("session delete --now".to_string()),
                    },
                )
                .await?;
                cancelled.push(run);
            }
            // `--now` 把未完成的都写成了取消，于是"已无未完成 Run"当场成立：直接进
            // `deleted`，不必等下一拍对账。
            if !matches!(record.state, SessionState::Deleted | SessionState::Purged) {
                self.set_session_state(session, record.state, SessionState::Deleted, at)
                    .await?;
            }
            self.audit_session(session, "session.deleted", SessionState::Deleted, at)
                .await;
        } else if record.state == SessionState::Active {
            self.set_session_state(session, SessionState::Active, SessionState::Closing, at)
                .await?;
            self.audit_session(session, "session.closing", SessionState::Closing, at)
                .await;
        }

        let state = self.session_state(session).await?;
        Ok(SessionLifecycleResponse {
            session: session.clone(),
            state,
            changed_at: at,
            cancelled,
        })
    }

    /// `POST /v1/sessions/{id}/purge`（§8.10 第 3 条）：**先算引用，再落墓碑，最后删内容**。
    ///
    /// 引用检查不过就把它交给调用方去回 409——正文里列出要先处置什么，**不假装成功**。
    pub async fn purge_session(
        self: &Arc<Self>,
        session: &SessionId,
    ) -> Result<Result<PurgeSessionResponse, Vec<PurgeBlocker>>, GatewayError> {
        let at = self.clock.now();
        let record = self.session_record(session).await?;

        let blockers = komo_store::repos::reconcile::purge_blockers(&self.db, session).await?;
        if !blockers.is_empty() {
            return Ok(Err(blockers));
        }

        let root = self.snapshot().paths.sessions_dir.clone();
        let paths = komo_store::SessionPaths::new(&root, session);

        // 墓碑先落（§8.10 第 3 条）。`purge` 只从 `deleted` 起步：`active` / `closing` 的
        // 会话先走一次逻辑删除的受理——不然一个还在收输入的会话会被从底下抽掉目录。
        if record.state != SessionState::Purged {
            if record.state != SessionState::Deleted {
                self.set_session_state(session, record.state, SessionState::Deleted, at)
                    .await?;
                self.audit_session(session, "session.deleted", SessionState::Deleted, at)
                    .await;
            }
            let marked = self.mark_purged(session, at).await?;
            if marked {
                self.audit_session(session, "session.purged", SessionState::Purged, at)
                    .await;
            }
        }

        // 内容后删。**目录不在 = 0 字节**：上一次删了一半被杀，这里收尾，重跑与跑一遍结果
        // 相同（幂等）。
        let removed_bytes = komo_store::repos::reconcile::remove_session_content(&paths).await?;
        Ok(Ok(PurgeSessionResponse {
            session: session.clone(),
            state: SessionState::Purged,
            removed_bytes,
        }))
    }

    /// 这个会话现在接不接受新输入，以及不接受时那**一种**状态（§8.10）。
    ///
    /// 调用方拿它写 409 的正文：`closing` / `deleted` / `purged` 三句话不一样，因为操作者
    /// 下一步该做的事不一样。
    pub async fn input_refusal(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionState>, GatewayError> {
        let state = self.session_state(session).await?;
        if state.accepts_input() {
            return Ok(None);
        }
        Ok(Some(state))
    }

    /// 一个会话的元数据；行不在就是"找不到这个会话"。
    pub async fn session_record(
        &self,
        session: &SessionId,
    ) -> Result<komo_store::repos::session::SessionRecord, GatewayError> {
        komo_store::repos::session::get(&self.db, session)
            .await?
            .ok_or_else(|| GatewayError::NotFound {
                what: format!("会话 {session}"),
            })
    }

    /// 一个会话现在处在哪一格。
    pub async fn session_state(&self, session: &SessionId) -> Result<SessionState, GatewayError> {
        komo_store::repos::session::state(&self.db, session)
            .await?
            .ok_or_else(|| GatewayError::NotFound {
                what: format!("会话 {session}"),
            })
    }

    /// 这个会话还没进终态的 Run（按创建序）。
    pub async fn unfinished_of(&self, session: &SessionId) -> Result<Vec<RunId>, GatewayError> {
        Ok(komo_store::repos::runs::list_for_session(&self.db, session)
            .await?
            .into_iter()
            .filter(|record| record.state.is_unfinished())
            .map(|record| record.run)
            .collect())
    }

    /// 条件推进会话状态（`from` 是 CAS，§8.10 第 1 条）。返回这次推进了没有。
    async fn set_session_state(
        &self,
        session: &SessionId,
        from: SessionState,
        to: SessionState,
        at: OffsetDateTime,
    ) -> Result<bool, GatewayError> {
        let session = session.clone();
        let changed = self
            .db
            .with_write_retry(move |ex| {
                let session = session.clone();
                Box::pin(async move {
                    komo_store::repos::session::set_state_in(ex, &session, from, to, at).await
                }) as komo_store::db::BoxFuture<'_, Result<bool, StoreError>>
            })
            .await?;
        Ok(changed)
    }

    /// 落 `purged` 墓碑。
    async fn mark_purged(
        &self,
        session: &SessionId,
        at: OffsetDateTime,
    ) -> Result<bool, GatewayError> {
        Ok(komo_store::repos::reconcile::mark_purged(&self.db, session, at).await?)
    }

    /// 把一个未完成的 Run 收在终态上（取消 / 放弃共用）。
    ///
    /// 会话那一侧写不进去时（目录被手工删过、日志中间损坏）**终态照样落在 state.db 上**：
    /// 那是调度事实，不是会话内容（§8.2），而操作者手上只有这一把。缺的那条会话副本如实
    /// 报到日志里。
    pub async fn finish_run(
        self: &Arc<Self>,
        session: &SessionId,
        run: &RunId,
        end: RunEnd,
    ) -> Result<RunState, GatewayError> {
        self.segments.cancel(run);
        let wanted = end.state();
        let outcome = match self.ledgers.open(session, "agent").await {
            Ok(entry) => entry
                .ledger
                .complete(run, end)
                .await
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        if let Err(error) = outcome {
            tracing::warn!(%error, %run, %session, "会话写不进去，终态只落在 state.db 上");
            komo_store::repos::runs::stop_without_event(
                &self.db,
                session,
                run,
                wanted,
                Some(format!("收尾时这个会话读不出来：{error}")),
                self.clock.now(),
            )
            .await?;
        }
        Ok(wanted)
    }

    /// 追加一条会话生命周期的审计副本（§8.10 第 1 条）。
    ///
    /// **它不产生状态**：权威是 `sessions.state` 那一列，数据库先提交、事件随后补写
    /// （与审批同向）。写不进去只影响审计，状态已经落库了。
    async fn audit_session(
        &self,
        session: &SessionId,
        kind: &str,
        state: SessionState,
        at: OffsetDateTime,
    ) {
        let event_id = EventId::new_at(at);
        let payload = komo_kernel::events::EventPayload::Unknown {
            event_type: kind.to_string(),
            raw: serde_json::json!({
                "session": session.as_str(),
                "state": state.as_str(),
                "at": at.to_string(),
            }),
        };
        let target = session.clone();
        let result = self
            .db
            .with_write_retry(move |ex| {
                let (session, event_id, payload) =
                    (target.clone(), event_id.clone(), payload.clone());
                Box::pin(async move {
                    komo_store::repos::outbox::enqueue_in(ex, &session, &event_id, payload, at)
                        .await
                }) as komo_store::db::BoxFuture<'_, Result<(), StoreError>>
            })
            .await;
        match result {
            Ok(()) => self.audit_wake.notify_one(),
            Err(error) => {
                tracing::warn!(%error, %session, kind, "会话生命周期的审计事件排不进 outbox")
            }
        }
    }
}
