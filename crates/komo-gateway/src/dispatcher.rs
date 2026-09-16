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
use komo_kernel::protocol::{ChatCommand, InboundAck, InboundMessage};
use komo_kernel::traits::{GatewayError, Inbound};
use komo_kernel::types::chat::{
    ApprovalScope, ChannelPeer, ChannelPlatform, DeliveryTarget, Outbound, PeerId, Principal,
};
use komo_kernel::types::ids::{RequestKey, RunId, SessionId, ShortId};
use komo_kernel::types::status::RunStatus;

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
    /// 操作者的**私聊**（飞书 DM、Telegram DM、WeChat、TUI）全部落到同一个 home
    /// session；群聊按 `{platform}:{chat_id}` 各自一个，且只有 `groups` 列出的群会被
    /// 响应（§11.2）。
    async fn conversation(&self, msg: &InboundMessage) -> Result<Option<SessionId>, GatewayError> {
        if msg.is_private {
            return self.state.home_session().await.map(Some);
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
        let existing = komo_store::repos::session::list(&self.state.db)
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
                let pending = self.state.approval_repo.list_pending(None).await?;
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
                let status = self.state.cancel_run(&run).await?;
                Ok(InboundAck::Replied {
                    text: format!("{run} 现在是 {}。", status_text(status)),
                })
            }
            ChatCommand::Approve { short_id, scope } => {
                self.decide(short_id, scope, true, principal, &msg.peer)
                    .await
            }
            ChatCommand::Reject { short_id } => {
                self.decide(short_id, ApprovalScope::Once, false, principal, &msg.peer)
                    .await
            }
        }
    }

    /// `/approve` / `/reject`：**审批命令只接受操作者**（上面已经挡过）。
    async fn decide(
        &self,
        short_id: Option<ShortId>,
        scope: ApprovalScope,
        approved: bool,
        principal: &Principal,
        from: &ChannelPeer,
    ) -> Result<InboundAck, GatewayError> {
        let record = match short_id {
            // **`find_latest_by_short_id`，不是 `find_by_short_id`**：后者只看待处理集合
            // （§11.3 的短 ID 就活在那个集合里），于是第二次点击查到 `None`，得到的是
            // 「没有这条」而不是「已决定」。§11.3 的命令表要求「已决定的返回原决定，
            // 不报错」，§14 的验证列逐字要求「同一人连点两次第二次得到『已决定』」。
            Some(short) => {
                self.state
                    .approval_repo
                    .find_latest_by_short_id(&short)
                    .await?
            }
            None => {
                // 「无 ID 时只有**恰好一个**待处理请求才生效；多于一个则列出并要求指明」
                let pending = self.state.approval_repo.list_pending(None).await?;
                match pending.len() {
                    0 => {
                        return Ok(InboundAck::Replied {
                            text: "现在没有待处理的审批。".into(),
                        });
                    }
                    1 => Some(pending.into_iter().next().expect("刚数过")),
                    _ => {
                        return Ok(InboundAck::Replied {
                            text: format!(
                                "有 {} 条待处理，请指明是哪一条：\n{}",
                                pending.len(),
                                render_pending(&pending)
                            ),
                        });
                    }
                }
            }
        };
        let Some(record) = record else {
            return Ok(InboundAck::Replied {
                text: "没有这条审批——这个短 ID 从来没有出现过。".into(),
            });
        };

        // 已经有结论了：回原决定，**不报错、也不再决定一次**（§11.3）。走
        // `decide_approval` 也答得出同样的话（它是幂等的），但那要多一次写事务，而这里
        // 手上已经有那条记录了。
        if let Some(decision) = &record.decision {
            return Ok(InboundAck::Replied {
                text: decided_text(&record, decision),
            });
        }

        let response = self
            .state
            .decide_approval(
                &record.approval,
                approved,
                scope,
                Some(principal.id().clone()),
            )
            .await?;

        // 结论投回**当初投过这条审批的每一个会话**，那一段在
        // `GatewayState::decide_approval` 里（四个界面共用）——这里不再另投一份，否则
        // 下命令的这个会话会收到两条。
        let _ = from;

        Ok(InboundAck::Replied {
            text: if response.already_decided {
                decided_text(&record, &response.decision)
            } else {
                format!(
                    "{} {}。",
                    record.short_id,
                    decision_text(response.decision.approved)
                )
            },
        })
    }

    async fn current_run(&self, session: &SessionId) -> Result<Option<RunId>, GatewayError> {
        let mut runs = komo_store::repos::runs::list_for_session(&self.state.db, session).await?;
        runs.retain(|run| !run.status.is_terminal());
        Ok(runs.into_iter().next_back().map(|run| run.run))
    }

    async fn status_text(&self, session: &SessionId) -> Result<String, GatewayError> {
        let runs = komo_store::repos::runs::list_for_session(&self.state.db, session).await?;
        let pending = self.state.approval_repo.list_pending(None).await?.len();
        let current = runs.iter().rev().find(|run| !run.status.is_terminal());
        let head = match current {
            Some(run) => format!("当前任务 {}：{}", run.run, status_text(run.status)),
            None => "没有在跑的任务。".to_string(),
        };
        Ok(format!("{head}\n待处理审批：{pending} 条"))
    }

    /// 第 6 步之后：订阅这个 Session 的事件流，Run 有终态时把最终回复发回来源会话。
    fn watch_reply(&self, session: SessionId, run: RunId, peer: ChannelPeer) {
        let state = Arc::clone(&self.state);
        let mut events = state.hub.subscribe(&session);
        tokio::spawn(async move {
            loop {
                let frame = match events.recv().await {
                    Ok(frame) => frame,
                    // 落后了就重读一次账本：内存通知丢了不丢数据（§13.1）。
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                let komo_kernel::protocol::sse::SseEvent::Event(event) = &frame.event else {
                    continue;
                };
                if event.run.as_ref() != Some(&run) {
                    continue;
                }
                let text = match &event.payload {
                    komo_kernel::events::EventPayload::RunCompleted(body) => body
                        .final_message
                        .clone()
                        .unwrap_or_else(|| "（这一轮没有文字回复）".to_string()),
                    komo_kernel::events::EventPayload::RunFailed(body) => {
                        format!("任务失败：{}", body.reason)
                    }
                    komo_kernel::events::EventPayload::RunCancelled(_) => "任务已取消。".into(),
                    _ => continue,
                };
                let target = DeliveryTarget::to_peer(peer.clone());
                if let Err(error) = state
                    .notifier
                    .log()
                    .deliver(&target, Outbound::Text { text })
                    .await
                {
                    tracing::warn!(%error, run = %run, "回复投不出去");
                }
                return;
            }
        });
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
        if let Some(command) = command {
            let ack = self.command(command, &msg, &principal, &session).await?;
            self.remember(&msg.request_key, &ack);
            return Ok(ack);
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
            .await?;
        let ack = if submitted.deduplicated {
            InboundAck::Duplicate {
                run: Some(submitted.run.clone()),
            }
        } else {
            self.watch_reply(session.clone(), submitted.run.clone(), msg.peer.clone());
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

/// 「已决定：原决定 · 谁 · 何时」。
///
/// §11.3：「已决定的返回原决定，**不报错**」——所以这句话要说得出是谁、什么时候决定的，
/// 否则第二个人只知道"轮不到我了"，不知道轮到了谁。
fn decided_text(
    record: &komo_kernel::protocol::http::ApprovalRecord,
    decision: &komo_kernel::protocol::http::ApprovalDecisionRecord,
) -> String {
    let by = decision
        .by
        .as_ref()
        .map(|peer| peer.to_string())
        .unwrap_or_else(|| "操作者".to_string());
    format!(
        "{} 已经决定过了：{} · {} · {}",
        record.short_id,
        decision_text(decision.approved),
        by,
        stamp(decision.decided_at)
    )
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

fn decision_text(approved: bool) -> &'static str {
    if approved { "已批准" } else { "已拒绝" }
}

fn status_text(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Ingesting => "正在接收",
        RunStatus::Queued => "排队中",
        RunStatus::Running => "执行中",
        RunStatus::WaitingApproval => "等待审批",
        RunStatus::WaitingRetry => "等待重试",
        RunStatus::Interrupted => "被中断",
        RunStatus::NeedsAttention => "需要你处理",
        RunStatus::Completed => "已完成",
        RunStatus::Failed => "失败",
        RunStatus::Cancelled => "已取消",
    }
}

fn render_pending(pending: &[komo_kernel::protocol::http::ApprovalRecord]) -> String {
    if pending.is_empty() {
        return "现在没有待处理的审批。".to_string();
    }
    pending
        .iter()
        .map(|record| {
            format!(
                "{} · {} · {}",
                record.short_id, record.plan.tool, record.reason
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 三个渠道都认的那几条命令（§11.3）。解析在这里，渲染在渠道。
pub fn parse_command(text: &str) -> Option<ChatCommand> {
    let trimmed = text.trim();
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
            let short_id = rest.first().and_then(|raw| ShortId::parse(raw));
            let scope = if rest.iter().any(|word| word.eq_ignore_ascii_case("run")) {
                ApprovalScope::Run
            } else {
                ApprovalScope::Once
            };
            Some(ChatCommand::Approve { short_id, scope })
        }
        "reject" | "deny" => Some(ChatCommand::Reject {
            short_id: rest.first().and_then(|raw| ShortId::parse(raw)),
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

/// home session 的 origin 串，给别处对照用。
pub const HOME: &str = HOME_ORIGIN;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approve_parses_its_short_id_and_scope() {
        assert_eq!(
            parse_command("/approve 7K2M"),
            Some(ChatCommand::Approve {
                short_id: ShortId::parse("7K2M"),
                scope: ApprovalScope::Once,
            })
        );
        assert_eq!(
            parse_command("/approve 7K2M run"),
            Some(ChatCommand::Approve {
                short_id: ShortId::parse("7K2M"),
                scope: ApprovalScope::Run,
            })
        );
        assert_eq!(
            parse_command("/approve"),
            Some(ChatCommand::Approve {
                short_id: None,
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
