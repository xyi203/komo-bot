//! `Dispatcher::handle`：渠道与 HTTP 共用的那个入口（§11.1 的六步）。
//!
//! ```text
//! 1. request_key 去重（平台重投）
//! 2. 解析 Principal（config.toml 的 allow_from，每条消息现读）
//! 3. 解析 Conversation → SessionId
//! 4. 冲刷该会话 pending 的投递（§11.4）
//! 5. 命令？→ 直接处理并 ack
//! 6. 普通文本 → Ledger::accept_input → 排队
//! ```
//!
//! **去重键只管平台重投，不管用户连点**（§11.1）：两次真实点击带着两个不同的
//! `event_id` / `update_id`，是两条合法输入；挡住第二次执行的是审批本身的幂等
//! （同一 `approval_id` 的第二次决定返回原决定）。两层分开，不要用组合键去兼职。
//!
// TODO(decide: 普通文本的去重是**持久**的——`Ledger::accept_input` 按请求键返回原 Run
// （§8.5）。命令没有对应的持久表：`deliveries` 记的是出站，`runs` 记的是输入，而一条
// `/approve` 两者都不是。这里先用一个进程内的有界表挡住平台重投，重启后那个窗口里的
// 重投会再执行一次——**而它的效果是幂等的**（已决定的返回原决定、`/new` 追加一条边界、
// `/pending` 只读），所以这不是一个会产生第二次副作用的缺口。要做成持久的，需要一张
// `inbox` 表或给 `deliveries` 加一个入站方向——那是编排者的决定，见报告。)

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::protocol::http::{
    InterventionKind, InterventionListQuery, InterventionSummary, InterventionVerdict,
};
use komo_kernel::protocol::{ApprovalTarget, ChatCommand, InboundAck, InboundMessage};
use komo_kernel::traits::{GatewayError, Inbound};
use komo_kernel::types::chat::{ApprovalScope, ChannelPeer, ChannelPlatform, PeerId, Principal};
use komo_kernel::types::ids::{RequestKey, RunId, SessionId, ShortId};
use komo_kernel::types::status::{RunState, WaitReason};

use crate::service::state::{GatewayState, HOME_ORIGIN};

/// 进程内记住的去重键上限。
const SEEN_LIMIT: usize = 4_096;

/// 群聊会话在 `sessions.origin` 里的样子。
fn chat_origin(peer: &ChannelPeer) -> String {
    format!("chat:{peer}")
}

pub struct Dispatcher {
    state: Arc<GatewayState>,
    seen: Mutex<VecDeque<(RequestKey, InboundAck)>>,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher").finish_non_exhaustive()
    }
}

impl Dispatcher {
    pub fn new(state: Arc<GatewayState>) -> Self {
        Dispatcher {
            state,
            seen: Mutex::new(VecDeque::new()),
        }
    }

    pub fn state(&self) -> &Arc<GatewayState> {
        &self.state
    }

    fn remember(&self, key: &RequestKey, ack: &InboundAck) {
        let mut seen = self.seen.lock().expect("去重表");
        if seen.len() >= SEEN_LIMIT {
            seen.pop_front();
        }
        seen.push_back((key.clone(), ack.clone()));
    }

    fn recall(&self, key: &RequestKey) -> Option<InboundAck> {
        self.seen
            .lock()
            .expect("去重表")
            .iter()
            .rev()
            .find(|(seen, _)| seen == key)
            .map(|(_, ack)| match ack {
                InboundAck::Queued { session: _, run } => InboundAck::Duplicate {
                    run: Some(run.clone()),
                },
                other => other.clone(),
            })
    }

    /// 第 2 步：发送者在该渠道 `allow_from` 里就是操作者，否则不是。**每条消息现读**
    /// 当前快照（§3 第 2 步、§11.2）。
    fn principal(&self, msg: &InboundMessage) -> Principal {
        let snapshot = self.state.snapshot();
        let operator = match snapshot.channels.get(msg.peer.platform) {
            Some(channel) => channel.is_operator(&msg.sender),
            // HTTP / TUI 的入口自己有认证（§13.1 的 Bearer），到这里就是操作者。
            None => msg.peer.platform == ChannelPlatform::Api,
        };
        if operator {
            Principal::Operator {
                platform: msg.peer.platform,
                id: msg.sender.clone(),
            }
        } else {
            Principal::Stranger {
                platform: msg.peer.platform,
                id: msg.sender.clone(),
            }
        }
    }

    /// 第 3 步：哪一个会话。
    ///
    /// 操作者的**私聊**（飞书 DM、Telegram DM、WeChat、TUI）落到**默认 Agent 的主会话**
    /// ——它们没有"找谁"这一维，所以走 §四 说的 `default_agent`；群聊按
    /// `{platform}:{chat_id}` 各自一个，且只有 `groups` 列出的群会被响应（§11.2）。
    async fn conversation(&self, msg: &InboundMessage) -> Result<Option<SessionId>, GatewayError> {
        if msg.is_private {
            return self.state.default_main_session().await.map(Some);
        }
        let snapshot = self.state.snapshot();
        let responds = snapshot
            .channels
            .get(msg.peer.platform)
            .is_some_and(|channel| channel.responds_in_group(&msg.peer.chat_id));
        if !responds {
            return Ok(None);
        }
        self.session_for_peer(&msg.peer).await.map(Some)
    }

    /// 一个群聊对应的那个 Session。
    ///
    /// 会话 ID 仍然是 UUID（kernel 的 ID 都是），"哪个群"记在 `sessions.origin` 上——
    /// 把 `{platform}:{chat_id}` 直接当 ID 会让每个消费者再去拆一次字符串。
    pub async fn session_for_peer(&self, peer: &ChannelPeer) -> Result<SessionId, GatewayError> {
        let origin = chat_origin(peer);
        let existing = komo_store::repos::session::list(&self.state.db, true)
            .await?
            .into_iter()
            .filter(|record| record.origin == origin)
            .map(|record| record.session)
            .min();
        if let Some(session) = existing {
            return Ok(session);
        }
        let session = SessionId::new_at(self.state.clock.now());
        self.state.ledgers.open(&session, &origin).await?;
        self.state.own_session(&session).await?;
        Ok(session)
    }

    /// 第 5 步。
    async fn command(
        &self,
        command: ChatCommand,
        msg: &InboundMessage,
        principal: &Principal,
        session: &SessionId,
    ) -> Result<InboundAck, GatewayError> {
        // `/id` 是**唯一不要求操作者身份**的命令，它在 handle 里就答过了。
        if !principal.is_operator() {
            return Ok(reject(msg));
        }
        match command {
            ChatCommand::Id => Ok(InboundAck::Replied {
                text: id_reply(msg),
            }),
            ChatCommand::Pending => {
                let pending = self.pending().await?;
                Ok(InboundAck::Replied {
                    text: render_pending(&pending),
                })
            }
            ChatCommand::New => {
                let seq = self.state.routed_boundary(session).await?;
                Ok(InboundAck::Replied {
                    text: format!("好，从这里开始新的一段（seq {seq}）。"),
                })
            }
            ChatCommand::Status => {
                let text = self.status_text(session).await?;
                Ok(InboundAck::Replied { text })
            }
            ChatCommand::Cancel => {
                let Some(run) = self.current_run(session).await? else {
                    return Ok(InboundAck::Replied {
                        text: "这个会话没有在跑的任务。".into(),
                    });
                };
                let state = self.state.cancel_run(&run).await?;
                Ok(InboundAck::Replied {
                    text: format!("{run} 现在是 {}。", state_text(state)),
                })
            }
            ChatCommand::Approve { target, scope } => {
                self.decide(target, scope, true, principal, &msg.peer).await
            }
            ChatCommand::Reject { target } => {
                self.decide(target, ApprovalScope::Once, false, principal, &msg.peer)
                    .await
            }
            // 待处理只有一条时的最短答复（`handle` 已经确认过真的有人在等）。语义就是
            // `Only`：多于一条时它会列出清单让人挑（§11.3）。
            ChatCommand::BareVerdict { approved } => {
                self.decide(
                    ApprovalTarget::Only,
                    ApprovalScope::Once,
                    approved,
                    principal,
                    &msg.peer,
                )
                .await
            }
        }
    }

    /// 待处理的 Intervention，三类一起（§7.5）。
    async fn pending(&self) -> Result<Vec<InterventionSummary>, GatewayError> {
        self.state
            .interventions(&InterventionListQuery::default())
            .await
    }

    /// `/approve` / `/reject`：**审批命令只接受操作者**（上面已经挡过）。
    ///
    /// 它们打的是 `POST /v1/interventions/{handle}/answer`（§11.3）——审批就是 §7.5 的
    /// 三类之一，`approve` / `reject` 就是它那两个结论，没有再走一条自己的路。
    async fn decide(
        &self,
        target: ApprovalTarget,
        scope: ApprovalScope,
        approved: bool,
        principal: &Principal,
        from: &ChannelPeer,
    ) -> Result<InboundAck, GatewayError> {
        if target == ApprovalTarget::All {
            return self.decide_all(scope, approved, principal).await;
        }
        let handle = match target {
            // 短 ID 是审批的句柄（§11.3 那张表）；已经答过的那条也定位得到，答复侧回原结论。
            ApprovalTarget::One(short) => short.to_string(),
            ApprovalTarget::Only => {
                // 「无 ID 时只有**恰好一个**待处理请求才生效；多于一个则列出并要求指明」
                let pending = self.pending().await?;
                match pending.as_slice() {
                    [] => {
                        return Ok(InboundAck::Replied {
                            text: "现在没有待处理的 Intervention。".into(),
                        });
                    }
                    [only] if only.kind == InterventionKind::Approval => only.handle.clone(),
                    // 只有一条、但它不是审批：`y` / `n` 只作用于审批（§11.3），所以这里
                    // 说清楚它是哪一类、该用哪条命令答——而不是把它当审批答下去。
                    [only] => {
                        return Ok(InboundAck::Replied {
                            text: format!(
                                "现在待处理的这一条是 {}（`{}`）：{}\n它不是审批，用 `/answer {} {}` 答。",
                                only.kind.as_str(),
                                only.handle,
                                only.question,
                                only.handle,
                                answer_hint(only.kind)
                            ),
                        });
                    }
                    many => {
                        return Ok(InboundAck::Replied {
                            text: format!(
                                "有 {} 条待处理，请指明是哪一条（`/approve all` 是全答审批）：\n{}",
                                many.len(),
                                render_pending(many)
                            ),
                        });
                    }
                }
            }
            ApprovalTarget::All => unreachable!("`all` 在上面就分流了"),
        };

        let response = self
            .state
            .answer_intervention(
                &handle,
                if approved {
                    InterventionVerdict::Approve
                } else {
                    InterventionVerdict::Reject
                },
                Some(if approved { scope } else { ApprovalScope::Once }),
                Some(principal.id().clone()),
            )
            .await?;

        // 结论投回**当初投过这条审批的每一个会话**，那一段在
        // `GatewayState::decide_approval` 里（四个界面共用）——这里不再另投一份，否则
        // 下命令的这个会话会收到两条。
        let _ = from;
        Ok(InboundAck::Replied {
            text: response.note,
        })
    }

    /// `/answer <handle> <结论>`：答复**另外两类**（§7.5），也认审批。
    ///
    /// 结论按种类分派：拼错了就把这一类**能答的**列出来（[`InterventionSummary::verdicts`]
    /// 是权威），而不是猜一个。
    async fn answer(
        &self,
        handle: &str,
        verdict: InterventionVerdict,
        by: PeerId,
    ) -> Result<InboundAck, GatewayError> {
        // 先看这一条的结论拼对了没有：`InterventionVerdict::parse` 认几个手滑得不算离谱
        // 的拼法，认不出来的到不了这里。
        let detail = self.state.intervention(handle).await?;
        if let Some(detail) = detail {
            let kind = match &detail {
                komo_kernel::protocol::http::InterventionDetail::Approval(_) => {
                    InterventionKind::Approval
                }
                komo_kernel::protocol::http::InterventionDetail::Verify { .. } => {
                    InterventionKind::Verify
                }
                komo_kernel::protocol::http::InterventionDetail::Blocked { .. } => {
                    InterventionKind::Blocked
                }
            };
            if !verdict.allowed_for(kind) {
                return Ok(InboundAck::Replied {
                    text: format!(
                        "`{}` 答不了 `{}`——这一条是 {}，能答的是：{}。",
                        handle,
                        verdict.as_str(),
                        kind.as_str(),
                        kind.verdicts()
                            .iter()
                            .map(|one| one.as_str())
                            .collect::<Vec<_>>()
                            .join(" / ")
                    ),
                });
            }
        }
        let response = self
            .state
            .answer_intervention(handle, verdict, Some(ApprovalScope::Once), Some(by))
            .await?;
        Ok(InboundAck::Replied {
            text: response.note,
        })
    }

    /// `/approve all` / `/reject all`：一次答一批（§11.3）。
    ///
    /// 名单**在这里列**——协议里没有"全部"这个词（见 `InterventionBatchAnswerRequest`
    /// 的注释：那会在答复到达之前，把这之后新出现的请求也一起答掉）。范围固定成"本次
    /// 调用"：一条命令替一批互不相干的计划选一个范围，是在替操作者猜一件他没看过的事。
    /// 他说了范围时，回执要**说出来它没被采纳**，而不是静默降级。
    ///
    /// **只答审批**：另外两类各有各的结论，一次按键替它们选不了（§7.5）。
    async fn decide_all(
        &self,
        scope: ApprovalScope,
        approved: bool,
        principal: &Principal,
    ) -> Result<InboundAck, GatewayError> {
        let pending = self.pending().await?;
        let approvals: Vec<String> = pending
            .iter()
            .filter(|one| one.kind == InterventionKind::Approval)
            .map(|one| one.handle.clone())
            .collect();
        if approvals.is_empty() {
            return Ok(InboundAck::Replied {
                text: "现在没有待处理的审批。".into(),
            });
        }
        let response = self
            .state
            .answer_approvals(&approvals, approved, Some(principal.id().clone()))
            .await?;
        let names: Vec<String> = response
            .answered
            .iter()
            .map(|answer| answer.handle.clone())
            .collect();
        let mut text = format!(
            "{} {} 条（各按本次调用）：{}。",
            if approved { "已批准" } else { "已拒绝" },
            names.len(),
            names.join(" ")
        );
        if scope != ApprovalScope::Once {
            text.push_str(
                "\n批量答复不带范围授权——范围绑的是单份计划，要范围请逐条 `/approve <短ID> run`。",
            );
        }
        if !response.missing.is_empty() {
            text.push_str(&format!(
                "\n{} 条在答复前已经不在待处理集合里，跳过。",
                response.missing.len()
            ));
        }
        let others = pending
            .iter()
            .filter(|one| one.kind != InterventionKind::Approval)
            .count();
        if others > 0 {
            text.push_str(&format!(
                "\n另有 {others} 条不是审批（结果不明 / 前提没了），用 `/answer <句柄> <结论>` 逐条答。"
            ));
        }
        Ok(InboundAck::Replied { text })
    }

    async fn current_run(&self, session: &SessionId) -> Result<Option<RunId>, GatewayError> {
        let mut runs = komo_store::repos::runs::list_for_session(&self.state.db, session).await?;
        runs.retain(|run| run.state.is_unfinished());
        Ok(runs.into_iter().next_back().map(|run| run.run))
    }

    /// `/status`：当前 Run 的状态、在等什么，以及**三类合计**的待处理数。
    ///
    /// 计数是三类合计（§7.5）：单看审批数会正好落回"清单为空而会话停着"那个老毛病——
    /// 一条 `verify` 挂在那里，审批数是 0。
    async fn status_text(&self, session: &SessionId) -> Result<String, GatewayError> {
        let runs = komo_store::repos::runs::list_for_session(&self.state.db, session).await?;
        let pending = self.pending().await?;
        let by_kind =
            |kind: InterventionKind| pending.iter().filter(|one| one.kind == kind).count();
        let current = runs.iter().rev().find(|run| run.state.is_unfinished());
        let head = match current {
            Some(run) => format!(
                "当前任务 {}：{}",
                run.run,
                state_line(run.state, run.wait.as_ref())
            ),
            None => "没有在跑的任务。".to_string(),
        };
        Ok(format!(
            "{head}\n待处理（共 {} 条）：审批 {} · 结果不明 {} · 前提没了 {}",
            pending.len(),
            by_kind(InterventionKind::Approval),
            by_kind(InterventionKind::Verify),
            by_kind(InterventionKind::Blocked),
        ))
    }
}

impl GatewayState {
    /// `/new`：当前 Session 追加 `conversation.boundary`，**不切 Session**（§11.3）。
    pub async fn routed_boundary(
        &self,
        session: &SessionId,
    ) -> Result<komo_kernel::types::ids::Seq, GatewayError> {
        use komo_kernel::traits::Ledger;
        Ok(self.routed.boundary(session).await?)
    }
}

#[async_trait]
impl Inbound for Dispatcher {
    async fn handle(&self, msg: InboundMessage) -> Result<InboundAck, GatewayError> {
        // ① 去重：平台重投命中同一个键。
        if let Some(ack) = self.recall(&msg.request_key) {
            return Ok(ack);
        }

        let command = parse_command(&msg.text);

        // `/id` 对**任何人**可用，也是唯一不要求操作者身份的命令（§11.2）。
        if matches!(command, Some(ChatCommand::Id)) {
            let ack = InboundAck::Replied {
                text: id_reply(&msg),
            };
            self.remember(&msg.request_key, &ack);
            return Ok(ack);
        }

        // ② Principal。不在名单里 → 拒绝，**不留任何记录**（不进 Run、不写投递、也不
        // 记进去重表——重投再被拒一次，代价只是一条固定提示）。
        let principal = self.principal(&msg);
        if !principal.is_operator() {
            return Ok(reject(&msg));
        }

        // ③ 哪一个会话。
        let Some(session) = self.conversation(&msg).await? else {
            return Ok(InboundAck::Ignored);
        };

        // ④ 冲刷这个会话还没送到的投递（§11.1 第 4 步）——**先补上积压的，再处理新的**。
        self.state.notifier.flush(Some(&msg.peer)).await;

        // ⑤ 命令。
        //
        // `/answer <句柄> <结论>` 不在 kernel 的 `ChatCommand` 里（那是三个渠道共用的解析
        // 结果，而"答复另外两类"这条路只有网关看得到清单）：它在这里当场认，走的是与
        // `/approve` 同一个入口（§7.5：四个界面语义相同）。
        if let Some((handle, verdict)) = parse_answer(&msg.text) {
            let ack = self
                .answer(&handle, verdict, principal.id().clone())
                .await?;
            self.remember(&msg.request_key, &ack);
            return Ok(ack);
        }
        // 写歪了的 `/answer`（少了结论、或者结论拼错），**列出来**：一句"我不知道"比
        // 悄悄把它当普通消息发出去好得多——那条消息会真的跑一轮模型。
        if let Some(help) = parse_answer_help(&msg.text) {
            let ack = InboundAck::Replied { text: help };
            self.remember(&msg.request_key, &ack);
            return Ok(ack);
        }
        if let Some(command) = command {
            // 裸 `y` / `n` 只在**真有一条在等**的时候才算答复：没有待处理的 Intervention
            // 时它落到下面那条普通消息的路（模型问"要不要…"，回个 `n` 不该被读成"拒绝
            // 一条不存在的审批"）。§11.3 的判据是"没有待处理 Intervention 时它不是命令"
            // ——三类都算，不只是审批：一条 `verify` 挂在那里时 `y` 仍然该被当成答复
            // 来对待（只是它答不了那一类，会得到一句说明）。
            let bare = matches!(command, ChatCommand::BareVerdict { .. });
            if !bare || !self.pending().await?.is_empty() {
                let ack = self.command(command, &msg, &principal, &session).await?;
                self.remember(&msg.request_key, &ack);
                return Ok(ack);
            }
        }

        // ⑥ 普通文本。
        let submitted = self
            .state
            .submit(
                &session,
                msg.request_key.clone(),
                msg.text.clone(),
                Some(msg.peer.clone()),
                None,
            )
            .await
            .map_err(|error| match error {
                // 「已逻辑删除的会话拒绝新输入」（§8.10）：聊天这一侧也要说清楚是哪种
                // 状态、以及该怎么做——HTTP 那侧是 409 加同一句话。
                GatewayError::InvalidRequest(message) => GatewayError::InvalidRequest(format!(
                    "{message}。要接着聊就在 TUI 里新建一个会话（`/new` 只是划一段边界）。"
                )),
                other => other,
            })?;
        // 谁在看这个 Run：`GatewayState::submit` 已经按来源会话起了一个看客
        // （§11.4：终态回到来源会话，等待审批投来源会话 + home chat），所以这里不再
        // 另起一个——两个看客会把同一条审批投两遍。
        let ack = if submitted.deduplicated {
            InboundAck::Duplicate {
                run: Some(submitted.run.clone()),
            }
        } else {
            InboundAck::Queued {
                session,
                run: submitted.run.clone(),
            }
        };
        self.remember(&msg.request_key, &ack);
        Ok(ack)
    }
}

/// 被拒绝的那条固定提示：带上他在这个平台的 id，操作者抄进 `allow_from` 即可。
fn reject(msg: &InboundMessage) -> InboundAck {
    InboundAck::Rejected {
        hint: format!(
            "这台 komo 只听它的操作者。把 {} 加进 config.toml 的 [channels.{}] allow_from 就可以了（当前会话：{}）。",
            msg.sender, msg.peer.platform, msg.peer
        ),
    }
}

fn id_reply(msg: &InboundMessage) -> String {
    format!("会话：{}\n发送者：{}", msg.peer, msg.sender)
}

/// 决定时刻，按本地可读的样子。
fn stamp(at: time::OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute()
    )
}

/// 一个 Run 状态的中文说法。
fn state_text(state: RunState) -> &'static str {
    match state {
        RunState::Accepted => "正在接收",
        RunState::Queued => "排队中",
        RunState::Running => "执行中",
        RunState::Waiting => "在等一个条件",
        RunState::Completed => "已完成",
        RunState::Failed => "失败",
        RunState::Cancelled => "已取消",
        RunState::Abandoned => "已放弃（不会再有下文）",
    }
}

/// **为什么不动**（§8.4 的第二维）。`/status` 与 `/pending` 靠它答得出"在等什么"。
fn wait_text(wait: &WaitReason) -> String {
    match wait {
        WaitReason::Approval { approval } => format!("等你批一份执行计划（{approval}）"),
        WaitReason::Retry {
            attempts,
            not_before,
            cause,
        } => format!(
            "等一次退避到点（第 {attempts} 次，{}，原因 {}）",
            stamp(*not_before),
            cause.as_str()
        ),
        WaitReason::Intervention { .. } => {
            "等你答一条 Intervention（`/pending` 看清单）".to_string()
        }
        WaitReason::Dependency { run } => format!("等同会话里更早的那条 Run（{run}）"),
    }
}

/// 一行"状态 + 在等什么"。
fn state_line(state: RunState, wait: Option<&WaitReason>) -> String {
    match (state, wait) {
        (RunState::Waiting, Some(wait)) => format!("{}：{}", state_text(state), wait_text(wait)),
        (RunState::Waiting, None) => "在等一个条件（没说清在等什么）".to_string(),
        _ => state_text(state).to_string(),
    }
}

/// 这一类该用哪条命令答——列一条**按下去有反应**的路（§11.3 的 TUI 菜单同一条理由）。
fn answer_hint(kind: InterventionKind) -> &'static str {
    match kind {
        InterventionKind::Approval => "approve",
        InterventionKind::Verify => "satisfied",
        InterventionKind::Blocked => "resolve",
    }
}

/// `/pending`：三类一起，句柄 + 种类 + 问题 + 可答的结论（§11.3）。
fn render_pending(pending: &[InterventionSummary]) -> String {
    if pending.is_empty() {
        return "现在没有待处理的 Intervention。".to_string();
    }
    pending
        .iter()
        .map(|one| {
            format!(
                "{} · {} · {}\n  可答：{}",
                one.handle,
                kind_text(one.kind),
                one.question,
                one.verdicts
                    .iter()
                    .map(|verdict| verdict.as_str())
                    .collect::<Vec<_>>()
                    .join(" / ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn kind_text(kind: InterventionKind) -> &'static str {
    match kind {
        InterventionKind::Approval => "审批",
        InterventionKind::Verify => "结果不明",
        InterventionKind::Blocked => "前提没了",
    }
}

/// `/answer <句柄> <结论>`（§11.3）。
///
/// 认不出结论就**列可选项**，不猜：`/answer run-1 done` 与 `/answer run-1 notperformed`
/// 都认（`InterventionVerdict::parse` 的手滑表），别的到不了这里。句柄缺失时也要说清楚。
fn parse_answer(text: &str) -> Option<(String, InterventionVerdict)> {
    let trimmed = text.trim();
    let mut parts = trimmed.split_whitespace();
    let head = parts.next()?;
    let name = head
        .trim_start_matches('/')
        .split('@')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name != "answer" {
        return None;
    }
    let handle = parts.next()?;
    // 结论拼错：返回 `None` 让调用方落进"说一句它接受哪些写法"那条路
    // （见 `parse_answer_help`），不在这里编一个"答不了"的结论。
    parts
        .next()
        .and_then(InterventionVerdict::parse)
        .map(|verdict| (handle.to_string(), verdict))
}

/// `/answer` 拼错了什么，以及它接受哪些写法。
fn parse_answer_help(text: &str) -> Option<String> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let head = parts.first()?.trim_start_matches('/').split('@').next()?;
    if !head.eq_ignore_ascii_case("answer") {
        return None;
    }
    match parts.len() {
        1 => Some("用法：`/answer <句柄> <结论>`（句柄见 `/pending`）。".to_string()),
        2 => Some(format!(
            "`/answer {} …` 还要一个结论：{}。",
            parts[1],
            InterventionVerdict::parse("satisfied")
                .map(|_| "satisfied / not_performed / resolve / abandon（审批是 approve / reject）")
                .unwrap_or_default()
        )),
        _ => Some(format!(
            "`{}` 不是一个结论。能答的是：satisfied / not_performed / resolve / abandon（审批用 approve / reject，也认 y / n）。",
            parts[2]
        )),
    }
}

/// `/approve` / `/reject` 后面那一段说的是**哪一条**。
///
/// `all` 是个词，不是短 ID：`ShortId::parse("all")` 会失败，按老规矩"认不出来的词当没
/// 写"就落进 `Only`——那在三条待处理时会回一句"请指明"，而不是把它们全答了。所以这个
/// 词要先认出来。
fn parse_target(rest: &[&str]) -> ApprovalTarget {
    if rest.iter().any(|word| word.eq_ignore_ascii_case("all")) {
        return ApprovalTarget::All;
    }
    match rest.first().and_then(|raw| ShortId::parse(raw)) {
        Some(short) => ApprovalTarget::One(short),
        None => ApprovalTarget::Only,
    }
}

/// 三个渠道都认的那几条命令（§11.3）。解析在这里，渲染在渠道。
pub fn parse_command(text: &str) -> Option<ChatCommand> {
    let trimmed = text.trim();
    // 最短的那条路：待处理只有一条时，`y` / `n` 连短 ID 都不用抄（§11.3）。**只认单个词**
    // ——`y 7K2M` 这种半懂不懂的写法宁可当普通消息，也不猜他想批哪一条。
    if !trimmed.contains(char::is_whitespace) {
        match trimmed.to_ascii_lowercase().as_str() {
            "y" | "yes" => return Some(ChatCommand::BareVerdict { approved: true }),
            "n" | "no" => return Some(ChatCommand::BareVerdict { approved: false }),
            _ => {}
        }
    }
    let mut parts = trimmed.split_whitespace();
    let head = parts.next()?;
    if !head.starts_with('/') {
        return None;
    }
    // Telegram 的 `/status@mybot`。
    let name = head
        .trim_start_matches('/')
        .split('@')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let rest: Vec<&str> = parts.collect();
    match name.as_str() {
        "approve" => {
            let target = parse_target(&rest);
            // `/approve <id> run` 是 §11.3 那张表里的一行；`cron` 是 §7.2 第三种范围
            // 的同一个形状——三个渠道的渲染里已经这么写了（`render::*::approval`）。
            // 认不出来的词一律当**没写**，也就是 `Once`：把一个不认识的词理解成一个
            // 更宽的范围是最糟的那一种宽容。
            // TODO(decide: 文档的命令表只写了 `run`。`cron` 的写法是按 `run` 的形状定
            // 的，等文档收口。)
            let scope = if rest.iter().any(|word| word.eq_ignore_ascii_case("cron")) {
                ApprovalScope::CronJob
            } else if rest.iter().any(|word| word.eq_ignore_ascii_case("run")) {
                ApprovalScope::Run
            } else {
                ApprovalScope::Once
            };
            Some(ChatCommand::Approve { target, scope })
        }
        "reject" | "deny" => Some(ChatCommand::Reject {
            target: parse_target(&rest),
        }),
        "pending" => Some(ChatCommand::Pending),
        "new" => Some(ChatCommand::New),
        "cancel" => Some(ChatCommand::Cancel),
        "status" => Some(ChatCommand::Status),
        "id" => Some(ChatCommand::Id),
        _ => None,
    }
}

/// 一条入站消息的便捷构造（渠道与测试都用得上）。
pub fn inbound(
    platform: ChannelPlatform,
    chat: &str,
    sender: &str,
    text: &str,
    request_key: &str,
    is_private: bool,
) -> InboundMessage {
    InboundMessage {
        peer: ChannelPeer::new(platform, chat),
        is_private,
        sender: PeerId::new(sender),
        text: text.to_string(),
        request_key: RequestKey::new(request_key),
    }
}

/// 主会话在 `sessions.origin` 里的前缀，给别处对照用（见
/// [`main_origin`](crate::service::state::main_origin)）。
pub const HOME: &str = HOME_ORIGIN;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approve_parses_its_short_id_and_scope() {
        assert_eq!(
            parse_command("/approve 7K2M"),
            Some(ChatCommand::Approve {
                target: ApprovalTarget::One(ShortId::parse("7K2M").expect("短 ID")),
                scope: ApprovalScope::Once,
            })
        );
        assert_eq!(
            parse_command("/approve 7K2M run"),
            Some(ChatCommand::Approve {
                target: ApprovalTarget::One(ShortId::parse("7K2M").expect("短 ID")),
                scope: ApprovalScope::Run,
            })
        );
        assert_eq!(
            parse_command("/approve"),
            Some(ChatCommand::Approve {
                target: ApprovalTarget::Only,
                scope: ApprovalScope::Once,
            })
        );
    }

    /// 待处理只有一条时最短的那条路：`y` / `n`（§11.3）。**只认单个词**。
    #[test]
    fn a_bare_yes_or_no_is_the_shortest_answer() {
        for text in ["y", "Y", " y ", "yes", "YES"] {
            assert_eq!(
                parse_command(text),
                Some(ChatCommand::BareVerdict { approved: true }),
                "{text}"
            );
        }
        for text in ["n", "N", "no", "No"] {
            assert_eq!(
                parse_command(text),
                Some(ChatCommand::BareVerdict { approved: false }),
                "{text}"
            );
        }
        // 带别的词就不猜了：`y 7K2M` 是半懂不懂的写法，宁可当普通消息。
        assert_eq!(parse_command("y 7K2M"), None);
        assert_eq!(parse_command("yes please"), None);
        // 别的单字/单词不能变成答复（中文里"好"太常见，绝不能绑）。
        assert_eq!(parse_command("好"), None);
        assert_eq!(parse_command("可以"), None);
        assert_eq!(parse_command("ok"), None);
        assert_eq!(parse_command(""), None);
    }

    /// `/approve all` 是**可以批量答**（§11.3），不是"这条叫 all 的短 ID 不存在"。
    ///
    /// 少了 `all` 这个词的识别，`ShortId::parse("all")` 失败 → 按"当没写"落进 `Only`，
    /// 于是它回的是"有 3 条待处理，请指明"——一句既不说明能批量、也不说 `all` 拼错了
    /// 的话，而它正是操作者看见三条待审批时最会打的那一句。
    #[test]
    fn approve_all_is_a_batch_not_an_unreadable_short_id() {
        assert_eq!(
            parse_command("/approve all"),
            Some(ChatCommand::Approve {
                target: ApprovalTarget::All,
                scope: ApprovalScope::Once,
            })
        );
        assert_eq!(
            parse_command("/reject all"),
            Some(ChatCommand::Reject {
                target: ApprovalTarget::All,
            })
        );
        // 大小写与位置都不重要——它是个词，不是位置参数。
        assert_eq!(
            parse_command("/approve ALL"),
            Some(ChatCommand::Approve {
                target: ApprovalTarget::All,
                scope: ApprovalScope::Once,
            })
        );
    }

    #[test]
    fn a_bot_suffix_and_case_do_not_hide_a_command() {
        assert_eq!(parse_command("/Status@komo_bot"), Some(ChatCommand::Status));
        assert_eq!(parse_command("  /id  "), Some(ChatCommand::Id));
    }

    #[test]
    fn plain_text_is_not_a_command() {
        assert_eq!(parse_command("帮我看下这个仓库"), None);
        assert_eq!(parse_command("http://example.com/new"), None);
    }
}
