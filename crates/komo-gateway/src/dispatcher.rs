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
//! **去重键只管平台重投，不管用户连点**（§11.1）：两条真实消息带着两个不同的
//! `event_id` / `update_id`，是两条合法输入；挡住第二次执行的是答复本身的幂等
//! （已经答过的介入返回原结论）。两层分开，不要用组合键去兼职。
//!
// TODO(decide: 普通文本的去重是**持久**的——`Ledger::accept_input` 按请求键返回原 Run
// （§8.5）。命令没有对应的持久表：`deliveries` 记的是出站，`runs` 记的是输入，而一条
// `/answer` 两者都不是。这里先用一个进程内的有界表挡住平台重投，重启后那个窗口里的
// 重投会再执行一次——**而它的效果是幂等的**（已答过的返回原结论、`/new` 追加一条边界、
// `/pending` 只读），所以这不是一个会产生第二次副作用的缺口。要做成持久的，需要一张
// `inbox` 表或给 `deliveries` 加一个入站方向——那是编排者的决定，见报告。)

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use komo_kernel::protocol::http::{
    InterventionKind, InterventionListQuery, InterventionSummary, InterventionVerdict,
};
use komo_kernel::protocol::{ChatCommand, InboundAck, InboundMessage};
use komo_kernel::traits::{GatewayError, Inbound};
use komo_kernel::types::chat::{ChannelPeer, ChannelPlatform, PeerId, Principal};
use komo_kernel::types::ids::{RequestKey, RunId, SessionId};
use komo_kernel::types::status::{RunState, WaitReason};
use komo_store::models::SessionKind;

use crate::service::state::{GatewayState, HOME_ORIGIN};

/// 进程内记住的去重键上限。
const SEEN_LIMIT: usize = 4_096;

/// 群聊会话在 `sessions.origin` 里的样子。
fn chat_origin(peer: &ChannelPeer) -> String {
    format!("chat:{peer}")
}

/// 访客会话的 origin：与操作者同一话题的 `chat:` 会话分开，两边互不可见。
fn guest_origin(peer: &ChannelPeer) -> String {
    format!("guest:{peer}")
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

    /// 第 2 步：发送者在该渠道 `allow_from` 里（或 `allow_tenant` 且渠道核实了同一组织）
    /// 就是操作者，否则不是。**每条消息现读**
    /// 当前快照（§3 第 2 步、§11.2）。
    fn principal(&self, msg: &InboundMessage) -> Principal {
        let snapshot = self.state.snapshot();
        let operator = match snapshot.channels.get(msg.peer.platform) {
            Some(channel) => channel.admits(&msg.sender, msg.same_org),
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
    /// 操作者的**私聊**（Telegram DM、WeChat、TUI）落到**默认 Agent 的主会话**
    /// ——它们没有"找谁"这一维，所以走 §四 说的 `default_agent`。带话题的私聊（飞书）
    /// 与群一样一个话题一个会话；群聊按 `{platform}:{chat_id}`（有话题再加 `#{thread}`）
    /// 各自一个；机器人所在的群里，操作者 @ 它就响应（@ 的过滤在渠道里，§11.2）。
    async fn conversation(&self, msg: &InboundMessage) -> Result<SessionId, GatewayError> {
        if msg.is_private {
            if msg.peer.thread.is_some() {
                return self.session_for_peer(&msg.peer).await;
            }
            return self.state.default_main_session().await;
        }
        self.session_for_peer(&msg.peer).await
    }

    /// 一个群聊对应的那个 Session。
    ///
    /// 会话 ID 仍然是 UUID（kernel 的 ID 都是），"哪个群"记在 `sessions.origin` 上——
    /// 把 `{platform}:{chat_id}` 直接当 ID 会让每个消费者再去拆一次字符串。
    /// 只认还收输入的会话：被删（或正在关闭）的那个不再接消息，下一条另开新的——
    /// `origin` 上没有唯一约束，同一个 origin 本来就可以有墓碑与活的并存。
    pub async fn session_for_peer(&self, peer: &ChannelPeer) -> Result<SessionId, GatewayError> {
        let origin = chat_origin(peer);
        let existing = komo_store::repos::session::list(&self.state.db, false)
            .await?
            .into_iter()
            .filter(|record| record.origin == origin && record.state.accepts_input())
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

    /// 群里非操作者的那个访客会话：同一个话题里所有访客共用一个，与操作者的 `chat:` 会话
    /// 分开。只认还收输入、且绑的是**当前**访客 Agent 的会话——`guest_agent` 改过之后另开
    /// 新的，旧的那个不会拿着一个已不在配置里的身份继续答。
    ///
    /// 建的时候就把 `agent_id` 与 `kind = Guest` 写死（与任务会话同一个顺序）：不走
    /// `own_session`，否则它会被认给默认 Agent。
    async fn guest_session_for_peer(
        &self,
        peer: &ChannelPeer,
        agent_id: &str,
    ) -> Result<SessionId, GatewayError> {
        let origin = guest_origin(peer);
        let existing = komo_store::repos::session::list(&self.state.db, false)
            .await?
            .into_iter()
            .filter(|record| {
                record.origin == origin
                    && record.kind == SessionKind::Guest
                    && record.agent_id == agent_id
                    && record.state.accepts_input()
            })
            .map(|record| record.session)
            .min();
        if let Some(session) = existing {
            return Ok(session);
        }
        let session = SessionId::new_at(self.state.clock.now());
        let jsonl_path = format!("sessions/{session}/events.jsonl");
        komo_store::repos::session::ensure_owned(
            &self.state.db,
            &session,
            &origin,
            &jsonl_path,
            agent_id,
            SessionKind::Guest,
        )
        .await?;
        self.state.ledgers.open(&session, &origin).await?;
        Ok(session)
    }

    /// 非操作者能不能走访客路径，能就返回访客 Agent。
    ///
    /// 只有群里、渠道配了 `guest_agent`、且不是斜杠命令的消息才行——私聊与命令（含
    /// `/answer`）照旧拒绝。每条消息现读当前快照。
    fn guest_agent_for(&self, msg: &InboundMessage) -> Option<String> {
        if msg.is_private || msg.text.trim_start().starts_with('/') {
            return None;
        }
        self.state
            .snapshot()
            .channels
            .get(msg.peer.platform)?
            .guest_agent
            .clone()
    }

    /// 访客的一句话：开（或接上）访客会话，当普通文本提交。没有命令、没有 `/answer`。
    async fn guest(&self, msg: InboundMessage, agent_id: &str) -> Result<InboundAck, GatewayError> {
        let session = self.guest_session_for_peer(&msg.peer, agent_id).await?;
        self.state.notifier.flush(Some(&msg.peer)).await;
        self.submit_text(&msg, session).await
    }

    /// 第 6 步：普通文本。操作者与访客共用。
    async fn submit_text(
        &self,
        msg: &InboundMessage,
        session: SessionId,
    ) -> Result<InboundAck, GatewayError> {
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
        // （§11.4：终态回到来源会话，需要处理投来源会话 + home chat），所以这里不再
        // 另起一个——两个看客会把同一条介入投两遍。
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
        }
    }
    /// 待处理的 Intervention，两类一起（§7.5）。
    async fn pending(&self) -> Result<Vec<InterventionSummary>, GatewayError> {
        self.state
            .interventions(&InterventionListQuery::default())
            .await
    }

    /// `/answer <handle> <结论>`：答复 `verify` / `blocked`（§7.5）。
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
            .answer_intervention(handle, verdict, Some(by))
            .await?;
        Ok(InboundAck::Replied {
            text: response.note,
        })
    }

    async fn current_run(&self, session: &SessionId) -> Result<Option<RunId>, GatewayError> {
        let mut runs = komo_store::repos::runs::list_for_session(&self.state.db, session).await?;
        runs.retain(|run| run.state.is_unfinished());
        Ok(runs.into_iter().next_back().map(|run| run.run))
    }

    /// `/status`：当前 Run 的状态、在等什么，以及**两类合计**的待处理数（§7.5）。
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
            "{head}\n待处理（共 {} 条）：结果不明 {} · 前提没了 {}",
            pending.len(),
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
        // 记进去重表——重投再被拒一次，代价只是一条固定提示）。唯一的例外：群里的普通
        // 文本、且渠道配了 `guest_agent`——走访客会话（无工具，不碰记忆），见 [`Self::guest`]。
        let principal = self.principal(&msg);
        if !principal.is_operator() {
            return match self.guest_agent_for(&msg) {
                Some(agent) => self.guest(msg, &agent).await,
                None => Ok(reject(&msg)),
            };
        }

        // ③ 哪一个会话。
        let session = self.conversation(&msg).await?;

        // ④ 冲刷这个会话还没送到的投递（§11.1 第 4 步）——**先补上积压的，再处理新的**。
        self.state.notifier.flush(Some(&msg.peer)).await;

        // ⑤ 命令。
        //
        // `/answer <句柄> <结论>` 不在 kernel 的 `ChatCommand` 里（那是三个渠道共用的解析
        // 结果，而"答复介入"这条路只有网关看得到清单）：它在这里当场认，走的是与 HTTP
        // 同一个入口（§7.5：四个界面语义相同）。
        if let Some((handle, verdict)) = parse_answer(&msg.text) {
            // 答复介入只给操作者：上面的拒绝已经挡过一次，这里不靠那一处。
            if !principal.is_operator() {
                return Ok(reject(&msg));
            }
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
        // 已经删掉的审批命令（`/approve` / `/reject`，§7）：说一句它们没了、现在有哪些
        // 命令，而不是把它当普通消息发给模型去跑一轮。
        if let Some(help) = retired_command_help(&msg.text) {
            let ack = InboundAck::Replied { text: help };
            self.remember(&msg.request_key, &ack);
            return Ok(ack);
        }
        if let Some(command) = command {
            let ack = self.command(command, &msg, &principal, &session).await?;
            self.remember(&msg.request_key, &ack);
            return Ok(ack);
        }

        // ⑥ 普通文本。
        self.submit_text(&msg, session).await
    }
}

/// 被拒绝的那条固定提示：带上他在这个平台的 id，操作者抄进 `allow_from` 即可。
fn reject(msg: &InboundMessage) -> InboundAck {
    InboundAck::Rejected {
        hint: format!(
            "这台 komo 只听它的操作者。把 {} 加进 config.toml 的 [channels.{}] allow_from 就可以了（当前会话：{}）。",
            msg.sender,
            msg.peer.platform,
            config_chat(msg)
        ),
    }
}

fn id_reply(msg: &InboundMessage) -> String {
    format!("会话：{}\n发送者：{}", config_chat(msg), msg.sender)
}

/// 抄进 `home_chat` 的那个串：不带话题，那一项只认会话本身。
fn config_chat(msg: &InboundMessage) -> ChannelPeer {
    ChannelPeer::new(msg.peer.platform, msg.peer.chat_id.as_str())
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
        RunState::Absorbed => "已并入当时正在跑的那条",
    }
}

/// **为什么不动**（§8.4 的第二维）。`/status` 与 `/pending` 靠它答得出"在等什么"。
fn wait_text(wait: &WaitReason) -> String {
    match wait {
        // 只在旧数据里（审批 2026-10-11 删除，§7）：启动恢复会按 §8.4 续跑它。
        WaitReason::Approval { .. } => "停在一条旧审批上（审批已删除，恢复时会续跑）".to_string(),
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

/// `/pending`：两类一起，句柄 + 种类 + 问题 + 可答的结论（§11.3）。
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
                .map(|_| "satisfied / not_performed / resolve / abandon")
                .unwrap_or_default()
        )),
        _ => Some(format!(
            "`{}` 不是一个结论。能答的是：satisfied / not_performed / resolve / abandon。",
            parts[2]
        )),
    }
}

/// 已经删掉的审批命令（§7，2026-10-11）：`/approve`、`/reject`、`/deny`。认出来就答一句
/// 帮助——它们不再是命令，也不该被当成普通消息跑一轮模型。`y` / `n` 不在这里：它们就是
/// 普通消息。
fn retired_command_help(text: &str) -> Option<String> {
    let head = text.split_whitespace().next()?;
    let name = head
        .strip_prefix('/')?
        .split('@')
        .next()?
        .to_ascii_lowercase();
    matches!(name.as_str(), "approve" | "reject" | "deny").then(|| {
        format!(
            "`/{name}` 已经不是命令了：审批随 Policy 一起删掉（§7），模型要做的 komo 直接执行。\n\
             现在的命令：/pending · /answer <句柄> <结论> · /status · /cancel · /new · /id"
        )
    })
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
    match name.as_str() {
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
        same_org: false,
        request_key: RequestKey::new(request_key),
    }
}

/// 主会话在 `sessions.origin` 里的前缀，给别处对照用（见
/// [`main_origin`](crate::service::state::main_origin)）。
pub const HOME: &str = HOME_ORIGIN;

#[cfg(test)]
mod tests {
    use super::*;

    /// 审批删掉之后（§7），`y` / `n` 就是普通消息，`/approve` / `/reject` 不再是命令。
    #[test]
    fn approval_words_are_no_longer_commands() {
        for text in [
            "y",
            "n",
            "yes",
            "no",
            "/approve",
            "/approve 7K2M run",
            "/reject all",
        ] {
            assert_eq!(parse_command(text), None, "{text}");
        }
    }

    /// 退役的审批命令答一句帮助，而不是被当成普通消息交给模型。
    #[test]
    fn a_retired_approval_command_gets_help_not_a_run() {
        for text in ["/approve", "/approve 7K2M", "/Reject@komo_bot all", "/deny"] {
            let help = retired_command_help(text).expect(text);
            assert!(help.contains("/pending"), "{help}");
            assert!(help.contains("/answer"), "{help}");
        }
        assert_eq!(retired_command_help("y"), None);
        assert_eq!(retired_command_help("approve 这个方案"), None);
        assert_eq!(retired_command_help("/status"), None);
    }

    #[test]
    fn a_bot_suffix_and_case_do_not_hide_a_command() {
        assert_eq!(parse_command("/Status@komo_bot"), Some(ChatCommand::Status));
        assert_eq!(parse_command("  /id  "), Some(ChatCommand::Id));
    }

    #[test]
    fn id_echoes_the_chat_without_the_thread() {
        let msg = InboundMessage {
            peer: ChannelPeer::new(ChannelPlatform::Feishu, "oc_g").in_thread("om_1"),
            is_private: false,
            sender: PeerId::new("ou_x"),
            text: "/id".into(),
            same_org: false,
            request_key: RequestKey::new("feishu:evt"),
        };
        assert_eq!(id_reply(&msg), "会话：feishu:oc_g\n发送者：ou_x");
        let InboundAck::Rejected { hint } = reject(&msg) else {
            unreachable!()
        };
        assert!(hint.contains("当前会话：feishu:oc_g）"), "{hint}");
    }

    #[test]
    fn plain_text_is_not_a_command() {
        assert_eq!(parse_command("帮我看下这个仓库"), None);
        assert_eq!(parse_command("http://example.com/new"), None);
    }
}
