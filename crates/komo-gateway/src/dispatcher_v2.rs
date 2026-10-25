//! `Dispatcher::handle` 的 Durable 版（`docs/storage-durable-d5.md` §4 W3a / §8）。
//!
//! 六步与 Legacy 相同（去重 → 认人 → 会话 → 冲刷投递 → 命令 → 普通文本），认人（`allow_from` 等）、
//! 命令解析、渲染一字不改——Legacy 的那些纯函数 `pub(crate)` 出来直接用。不同的只有落存储的那几步：
//!
//! - **会话**：操作者的私聊（没有话题）落到默认 Agent 的主会话（`kind = main`）；群、话题、带话题的
//!   私聊按 `source = chat:{peer}` 各一个普通会话（被删或正在关闭的不再接消息，下一条另开新的）。
//! - **普通文本**：`request_id` = 平台消息键（持久去重：重投返回原 run），输入 entry 是 `chat.input`，
//!   `data {peer, principal}`；宿主 run 是**同一个 peer** 起的、还在忙时 `WhenBusy::Steer`（并进去），
//!   别的 peer 的话排在后面（§6：对端与宿主不同就不并）。
//! - **命令**：`/new` = `boundary`；`/cancel` = 只停当前这一轮（决定 8：`abort_run`，不撤回收件箱；
//!   只有排队的那一条才 `withdraw`）；`/status` 带当前 run 与这个会话派出的后台任务
//!   （`TaskLine::render`）；`/pending` 与 `/answer` 读的 intervention 清单 W4a 才接新存储（faulted 任务），
//!   在那之前是空的。
//!
//! 谁在看这条 run：渠道回复由 [`watch`](crate::service::durable::watch) 的看客投回，这里只在**提交之前**
//! 保证这个会话有看客。

use komo_kernel::durable::{SessionKind, SessionRecord, legacy_ids};
use komo_kernel::protocol::{ChatCommand, InboundAck, InboundMessage};
use komo_kernel::traits::GatewayError;
use komo_kernel::types::chat::{ChannelPeer, Principal};
use komo_kernel::types::ids::SessionId;
use komo_kernel::types::status::RunState;
use komo_runtime::durable::harness::{Harness, InputEntry, WhenBusy};
use serde_json::json;

use crate::Dispatcher;
use crate::dispatcher::{
    chat_origin, id_reply, parse_answer, parse_answer_help, parse_command, reject, render_pending,
    retired_command_help, state_line, state_text, with_context,
};
use crate::service::durable::watch::{self, CHAT_INPUT, Source};
use crate::service::durable::{DurableCore, SessionError};
use komo_kernel::protocol::http::InterventionKind;

/// `SessionError` → 渠道看得懂的错误。
fn gateway_error(error: SessionError) -> GatewayError {
    match error {
        SessionError::NotFound(what) => GatewayError::NotFound { what },
        // 「已逻辑删除的会话拒绝新输入」（§8.10）：聊天这一侧也要说清楚是哪种状态、该怎么做。
        SessionError::NotOpen(_) => GatewayError::InvalidRequest(format!(
            "{error}。要接着聊就在 TUI 里新建一个会话（`/new` 只是划一段边界）。"
        )),
        SessionError::Invalid(message)
        | SessionError::Busy(message)
        | SessionError::Conflict(message) => GatewayError::InvalidRequest(message),
        SessionError::RequestKeyConflict(_) => GatewayError::InvalidRequest(error.to_string()),
        SessionError::Internal(message) => GatewayError::Internal(message),
    }
}

impl Dispatcher {
    /// 第 3 步：哪一个会话（规则同 Legacy 的 `conversation`，落在新存储上）。
    async fn session_v2(
        &self,
        core: &DurableCore,
        msg: &InboundMessage,
    ) -> Result<SessionRecord, GatewayError> {
        let agent = self.state().snapshot().agent.default_agent.clone();
        let private_main = msg.is_private && msg.peer.thread.is_none();
        let (record, _) = if private_main {
            core.main_session(&agent).await
        } else {
            core.session_for_source(&chat_origin(&msg.peer), &agent)
                .await
        }
        .map_err(gateway_error)?;
        Ok(record)
    }

    /// 宿主 run 是不是这个 peer 起的（或者它的结果本来就要回给这个 peer）。会话空闲时无所谓，答 `true`。
    async fn host_is_peer(
        &self,
        core: &DurableCore,
        record: &SessionRecord,
        peer: &ChannelPeer,
    ) -> bool {
        let Ok(live) = Harness::live(&*core.storage, record.current_branch).await else {
            return false;
        };
        let Some(first) = live
            .run
            .as_ref()
            .and_then(|run| run.inputs.first().copied())
        else {
            return true;
        };
        match watch::source_of(core, first).await {
            (Source::Peer(host), _) => &host == peer,
            (Source::Internal, Some(entry)) => {
                watch::peer_on_path(core, &record.id, entry).await.as_ref() == Some(peer)
            }
            _ => false,
        }
    }

    pub(crate) async fn handle_v2(&self, msg: InboundMessage) -> Result<InboundAck, GatewayError> {
        // ① 去重：平台重投命中同一个键（命令用；普通文本的去重是持久的，在请求键上）。
        if let Some(ack) = self.recall(&msg.request_key) {
            return Ok(ack);
        }
        let command = parse_command(&msg.text);

        // `/id` 对**任何人**可用。
        if matches!(command, Some(ChatCommand::Id)) {
            let ack = InboundAck::Replied {
                text: id_reply(&msg),
            };
            self.remember(&msg.request_key, &ack);
            return Ok(ack);
        }

        // ② Principal。不在名单里 → 拒绝，不留任何记录。
        let principal = self.principal(&msg);
        if !principal.is_operator() {
            return Ok(reject(&msg));
        }

        // ③ 哪一个会话。
        let core = self.state().core()?.clone();
        let record = self.session_v2(&core, &msg).await?;
        let session = record.id.clone();

        // ④ 冲刷这个会话还没送到的投递——先补上积压的，再处理新的。
        self.state().notifier.flush(Some(&msg.peer)).await;

        // ⑤ 命令（与 Legacy 同一张表）。
        if let Some((handle, verdict)) = parse_answer(&msg.text) {
            let ack = self
                .answer(&handle, verdict, principal.id().clone())
                .await?;
            self.remember(&msg.request_key, &ack);
            return Ok(ack);
        }
        if let Some(help) = parse_answer_help(&msg.text) {
            let ack = InboundAck::Replied { text: help };
            self.remember(&msg.request_key, &ack);
            return Ok(ack);
        }
        if let Some(help) = retired_command_help(&msg.text) {
            let ack = InboundAck::Replied { text: help };
            self.remember(&msg.request_key, &ack);
            return Ok(ack);
        }
        if let Some(command) = command {
            let ack = self
                .command_v2(command, &msg, &principal, &core, &record)
                .await?;
            self.remember(&msg.request_key, &ack);
            return Ok(ack);
        }

        // ⑥ 普通文本。**先挂看客再提交**：订阅之后才发生的终态帧才收得到。
        watch::ensure(self.state(), &session);
        let when_busy = if self.host_is_peer(&core, &record, &msg.peer).await {
            WhenBusy::Steer
        } else {
            WhenBusy::FollowUp
        };
        let entry = InputEntry::new(
            CHAT_INPUT,
            Some(json!({
                "peer": msg.peer.to_string(),
                "principal": {
                    "platform": principal.platform().as_str(),
                    "id": principal.id().as_str(),
                },
            })),
        );
        let accepted = core
            .submit_input(
                &session,
                crate::service::durable::submit::InputRequest::new(with_context(&msg))
                    .request_id(msg.request_key.as_str())
                    .entry(entry)
                    .when_busy(when_busy),
            )
            .await
            .map_err(gateway_error)?;
        let ack = if accepted.deduplicated {
            InboundAck::Duplicate {
                run: Some(accepted.run.clone()),
            }
        } else {
            InboundAck::Queued {
                session: SessionId::from_raw(session),
                run: accepted.run.clone(),
            }
        };
        self.remember(&msg.request_key, &ack);
        Ok(ack)
    }

    async fn command_v2(
        &self,
        command: ChatCommand,
        msg: &InboundMessage,
        principal: &Principal,
        core: &DurableCore,
        record: &SessionRecord,
    ) -> Result<InboundAck, GatewayError> {
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
                let submission = core
                    .boundary(&record.id, None)
                    .await
                    .map_err(gateway_error)?;
                let seq = legacy_ids::seq(submission, 0);
                Ok(InboundAck::Replied {
                    text: format!("好，从这里开始新的一段（seq {seq}）。"),
                })
            }
            ChatCommand::Status => Ok(InboundAck::Replied {
                text: self.status_v2(core, record).await?,
            }),
            ChatCommand::Cancel => {
                let summary = core.session_summary(record).await.map_err(gateway_error)?;
                let Some(run) = summary.current_run else {
                    return Ok(InboundAck::Replied {
                        text: "这个会话没有在跑的任务。".into(),
                    });
                };
                // 排队的那一条 withdraw，在跑的那一条只停这一轮（决定 8）。
                let mut state = core.cancel_run_id(&run).await.map_err(gateway_error)?;
                if !state.is_terminal() {
                    // 取消是异步的：等它落下来（有限时），好把真实状态告诉操作者。
                    if let Some(first) = legacy_ids::submission_of(&run) {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            core.wait_submission(first),
                        )
                        .await;
                        state = core.submission_state(first).await.unwrap_or(state);
                    }
                }
                let text = if state == RunState::Cancelled || state.is_terminal() {
                    format!("{run} 现在是 {}。", state_text(state))
                } else {
                    format!("{run} 已发出取消，这一轮停下后即为已取消。")
                };
                Ok(InboundAck::Replied { text })
            }
        }
    }

    /// `/status`：当前 run、待处理数（两类合计）、这个会话派出的后台任务。
    async fn status_v2(
        &self,
        core: &DurableCore,
        record: &SessionRecord,
    ) -> Result<String, GatewayError> {
        let summary = core.session_summary(record).await.map_err(gateway_error)?;
        let pending = self.pending().await?;
        let by_kind =
            |kind: InterventionKind| pending.iter().filter(|one| one.kind == kind).count();
        let head = match (&summary.current_run, summary.current_state) {
            (Some(run), Some(state)) => format!(
                "当前任务 {run}：{}",
                state_line(state, summary.current_wait.as_ref())
            ),
            _ => "没有在跑的任务。".to_string(),
        };
        let mut text = format!(
            "{head}\n待处理（共 {} 条）：结果不明 {} · 前提没了 {}",
            pending.len(),
            by_kind(InterventionKind::Verify),
            by_kind(InterventionKind::Blocked),
        );
        if record.kind != SessionKind::Task {
            let tasks = core
                .task_sessions(Some(&record.id))
                .await
                .map_err(gateway_error)?;
            if !tasks.is_empty() {
                text.push_str("\n后台任务：");
                for line in tasks {
                    text.push_str("\n  ");
                    text.push_str(&line.render());
                }
            }
        }
        Ok(text)
    }
}
