//! 命令直跑模式的固定出牌驱动（§10）。
//!
//! Cron 的命令 Job 不经模型：`AgentLoop` 平时靠 `LlmClient::begin_turn` 拿一个
//! `TurnDriver`，命令 Job 触发的 Run 用这个固定脚本代替它——第一轮固定给一个 `shell`
//! 工具调用（命令 = Job 的 `command`，工作目录由这一段的执行环境给出），第二轮拿到
//! 工具结果后收尾，最终答复就是那条工具结果的正文。选哪个驱动在 `AgentLoop::run`
//! 里按 Run 的来源判（§10 设计的"选驱动的接缝"）：不是把命令伪装成另一个 `ModelConfig`
//! / provider，是压根不经过 `LlmClient` 这一层——`self.llm` 一次都不会被摸到。
//!
//! 于是 Policy / Proof / 审批 / JSONL / tool-output / 恢复 / 投递全部原样复用：那一次
//! `shell` 调用照样要过 `prepare` → Policy → （必要时）审批 → `execute`，跟模型发起的
//! 调用走的是同一条路。
//!
//! **恢复安全**：如果上一次已经把这条 `shell` 调用的结果记进了账本（收到结果之后、
//! 收尾之前进程重启了），那条调用在回放窗口里已经"收尾"了，不再进 `resumed.pending`
//! （`Surface::open_calls`，§8.4）——于是这一段会带着一个**空的** `resume`、从
//! `RoundInput::First` 重新起步。普通模型驱动不怕这个：它靠 `TurnRequest.messages`
//! 里那条已经落盘的调用与结果认出"这件事做过了"。`CommandDriver` 不读
//! `messages`（它压根不构造模型请求），所以要自己在构造时查一遍这一段的回放窗口：
//! 已经有结果就直接收尾，没有才第一次发那个调用——不然重启一次会把同一条命令再跑
//! 一遍，正是 §8.6 要求不能发生的事。

use async_trait::async_trait;

use komo_kernel::traits::TurnDriver;
use komo_kernel::types::model::TokenUsage;
use komo_kernel::types::turn::{LlmError, ProviderToolCall, ReplayMessage, Round, RoundInput};

/// 那次 `shell` 调用固定用这个 `provider_call_id`——回放窗口里认哪条结果是它的，靠的
/// 就是这个常量，不是猜"最后一条工具结果"。
const CALL_ID: &str = "cmd-1";

/// 一条命令 Job 交给 `CommandDriver` 的那一条命令。工作目录不在这里：它是这一段执行
/// 环境的 `cwd`（Session 的 workdir，受理时已经从 Job 落到会话上，§10），`shell` 工具
/// 不给 `cwd` 参数时就用它——不必在这里重复一份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub command: String,
}

/// 固定出牌的 [`TurnDriver`]：第一轮发那个 `shell` 调用，第二轮把它的结果当最终答复。
pub struct CommandDriver {
    command: String,
    /// 回放窗口里已经有的那条结果（重启安全，见模块文档）。`Some` 时第一轮直接用它
    /// 收尾，不再发一次调用。
    already: Option<String>,
}

impl CommandDriver {
    /// `messages` 是这一段的回放窗口（`TurnRequest.messages`）——构造时查一遍，不是
    /// 每次 `next` 都查。
    pub fn new(spec: CommandSpec, messages: &[ReplayMessage]) -> Self {
        let already = messages.iter().rev().find_map(|message| {
            message
                .tool_results
                .iter()
                .find(|result| result.provider_call_id == CALL_ID)
                .map(|result| result.content.clone())
        });
        CommandDriver {
            command: spec.command,
            already,
        }
    }
}

#[async_trait]
impl TurnDriver for CommandDriver {
    async fn next(&mut self, input: RoundInput) -> Result<Round, LlmError> {
        match input {
            RoundInput::First => match self.already.take() {
                // 上一次已经有结果了：不再重复那次调用，直接收尾（见模块文档的恢复
                // 安全那一段）。
                Some(content) => Ok(finish(content)),
                None => Ok(Round {
                    round: 1,
                    text: None,
                    tool_calls: vec![ProviderToolCall {
                        provider_call_id: CALL_ID.into(),
                        name: "shell".into(),
                        arguments: serde_json::json!({ "command": self.command }),
                    }],
                    provider_blocks: None,
                    usage: TokenUsage::default(),
                    truncated: false,
                }),
            },
            RoundInput::ToolResults { results } => {
                let content = results
                    .into_iter()
                    .find(|result| result.provider_call_id == CALL_ID)
                    .map(|result| result.content)
                    .unwrap_or_default();
                Ok(finish(content))
            }
        }
    }

    fn usage(&self) -> TokenUsage {
        // 命令 Run 全程零模型请求（§10）：没有用量可言，不是"这次没测到"。
        TokenUsage::default()
    }
}

/// 收尾那一轮：没有调用了，`AgentLoop` 见到空的 `tool_calls` 就会正常结束这个 Run。
fn finish(text: String) -> Round {
    Round {
        round: 2,
        text: Some(text),
        tool_calls: vec![],
        provider_blocks: None,
        usage: TokenUsage::default(),
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::ids::ToolCallId;
    use komo_kernel::types::turn::{Role, ToolResultForModel};

    fn spec(command: &str) -> CommandSpec {
        CommandSpec {
            command: command.into(),
        }
    }

    #[tokio::test]
    async fn the_first_round_asks_for_exactly_one_shell_call_of_the_configured_command() {
        let mut driver = CommandDriver::new(spec("echo hi"), &[]);
        let round = driver.next(RoundInput::First).await.unwrap();
        assert!(round.text.is_none());
        assert_eq!(round.tool_calls.len(), 1);
        let call = &round.tool_calls[0];
        assert_eq!(call.name, "shell");
        assert_eq!(call.provider_call_id, CALL_ID);
        assert_eq!(call.arguments, serde_json::json!({ "command": "echo hi" }));
    }

    #[tokio::test]
    async fn the_second_round_finishes_with_the_tool_results_content_and_no_more_calls() {
        let mut driver = CommandDriver::new(spec("echo hi"), &[]);
        driver.next(RoundInput::First).await.unwrap();
        let round = driver
            .next(RoundInput::ToolResults {
                results: vec![ToolResultForModel {
                    provider_call_id: CALL_ID.into(),
                    call_id: ToolCallId::from_raw("call-1"),
                    content: "hi".into(),
                    is_error: false,
                }],
            })
            .await
            .unwrap();
        assert_eq!(round.text.as_deref(), Some("hi"));
        assert!(round.tool_calls.is_empty(), "没有调用了，这一段该结束");
    }

    /// 恢复安全：回放窗口里已经有这条调用的结果时（上一次结果落盘之后、收尾之前
    /// 进程重启了），第一轮不再重发那个调用，直接用已有结果收尾。
    #[tokio::test]
    async fn a_result_already_in_the_replay_window_is_not_re_run() {
        let messages = vec![ReplayMessage {
            role: Role::Tool,
            seq: komo_kernel::types::ids::Seq(3),
            text: None,
            tool_calls: vec![],
            tool_results: vec![ToolResultForModel {
                provider_call_id: CALL_ID.into(),
                call_id: ToolCallId::from_raw("call-1"),
                content: "hi".into(),
                is_error: false,
            }],
            provider_blocks: None,
        }];
        let mut driver = CommandDriver::new(spec("echo hi"), &messages);
        let round = driver.next(RoundInput::First).await.unwrap();
        assert_eq!(round.text.as_deref(), Some("hi"));
        assert!(
            round.tool_calls.is_empty(),
            "已经有结果了，不该再发一次调用"
        );
    }
}
