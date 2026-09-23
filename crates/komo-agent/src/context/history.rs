//! Ledger 事件与模型消息之间那一层：`Surface` 已经把"发生了什么"折好了（`fold`），这里
//! 只回答**回放哪些、按哪种方式**（`docs/agent.md` §8）。
//!
//! 三步纯逻辑 + 一步 I/O，交替进行：
//!
//! ```text
//! Gateway: events → fold → Surface                      I/O（读账本）
//! entries(&Surface, scope) -> Vec<Entry>                 纯：选窗口
//! Gateway: 对选中的消息读正文与 output.json              I/O（只读被选中的）
//! to_replay_messages(Vec<ResolvedMessage>) -> ..         纯：投影成模型消息
//! ```
//!
//! **只对 `entries` 选中的条目读正文**：一条落在窗口外、已经损坏的 payload 不该让这一段
//! `halt_if_corrupt`（`docs/agent.md` §8 的"为什么要先选窗口再读正文"）。

use std::collections::BTreeMap;

use komo_kernel::fold::{Surface, SurfaceMessage};
use komo_kernel::projection::{ProjectionContext, ToolResultFacts, project};
use komo_kernel::types::ids::{RunId, Seq, ToolCallId};
use komo_kernel::types::refs::{ContentRef, ToolResultStatus};
use komo_kernel::types::turn::{ReplayMessage, Role, ToolCallRequest, ToolResultForModel};

/// 这一段回放给模型的是哪一块（`docs/agent.md` §8）。
#[derive(Debug, Clone, Copy)]
pub enum ReplayScope<'a> {
    /// 主对话：最新一个 `conversation.boundary` 之后**属于主 Run** 的消息，跨 Run 聚合。
    ///
    /// 带的是**正在跑的那条 Run**：它那几句是完整协议，其余 Run 只发布"用户说了什么、
    /// 它最后答了什么"。
    Conversation(&'a RunId),
    /// 只这一条 Run（子代理）：它拿不到父的对话历史，父的窗口里也没有它的过程。
    Run(&'a RunId),
}

/// 一条消息该按哪种方式回放。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// 历史 Run（或不属于任何 Run 的消息）：只要正文，不要调用、结果与原生块。
    Transcript,
    /// 正在跑的那条 Run：完整协议。
    Protocol,
}

/// 一条要回放的消息，以及它按哪种方式回放。`entries` 的输出——Gateway 只对这里出现的
/// 消息去读正文与 `output.json`。
#[derive(Debug, Clone, Copy)]
pub struct Entry<'s> {
    pub message: &'s SurfaceMessage,
    pub kind: EntryKind,
}

/// Gateway 读回来的东西：`entry` 选中的那条消息，正文与（`Protocol` 才有的）工具输出。
pub struct ResolvedMessage<'s> {
    pub entry: Entry<'s>,
    /// 内联正文，或外置正文按引用读回来的结果。
    pub text: Option<String>,
    /// 与 `entry.message.tool_results` 一一对应；`None` = `output.json` 没读成
    /// （没有输出存储，或者读不回来）。只有 `Protocol` 条目才会去读，`Transcript` 条目
    /// 这里恒为空。
    pub outputs: Vec<Option<StoredOutput>>,
}

/// 一次工具尝试的落盘输出：`output.json` 里那份完整正文与这一轮产出的文件。
#[derive(Debug, Clone)]
pub struct StoredOutput {
    pub preview: Option<String>,
    pub artifacts: Vec<ContentRef>,
}

/// 纯函数：选窗口，并且已经筛掉历史 Run 里"不是用户正文、也不是最终回复"的那些
/// （`docs/agent.md` §8）。
///
/// **还没有决定读不读正文**：历史 Run 条目的正文是不是空、能不能读到，那是读回来之后才
/// 知道的事（§8 的"skipping empty transcript text"在 [`to_replay_messages`] 里做）。这里
/// 只按**结构**（角色、seq、是不是这条 Run）判断要不要这一条。
pub fn entries<'s>(surface: &'s Surface, scope: ReplayScope<'_>) -> Vec<Entry<'s>> {
    let only = match scope {
        ReplayScope::Conversation(run) | ReplayScope::Run(run) => run,
    };
    let kept = window(surface, Some(scope));

    // 历史 Run 的"它最后答了什么"是哪一条：同一个 Run 里**最后**一条带正文的 assistant。
    let mut finals: BTreeMap<&RunId, Seq> = BTreeMap::new();
    for message in &kept {
        let (Some(run), Role::Assistant) = (&message.run, message.role) else {
            continue;
        };
        if message.text.is_none() && message.text_ref.is_none() {
            continue;
        }
        let entry = finals.entry(run).or_insert(message.seq);
        *entry = (*entry).max(message.seq);
    }

    kept.into_iter()
        .filter_map(|message| {
            if message.run.as_ref() != Some(only) {
                // 历史 Run：用户正文与最终回复这两句，别的都不要。
                let last = message
                    .run
                    .as_ref()
                    .and_then(|run| finals.get(run))
                    .copied();
                if message.role != Role::User && last != Some(message.seq) {
                    return None;
                }
                return Some(Entry {
                    message,
                    kind: EntryKind::Transcript,
                });
            }
            Some(Entry {
                message,
                kind: EntryKind::Protocol,
            })
        })
        .collect()
}

/// 回放窗口里属于**已经开跑过**的那些 Run，**按 Run 分组、Run 之间先来后到**。
///
/// 日志的顺序是**接收**的顺序（输入落盘的时机，§8.5 内容权威），不是执行的顺序：一个
/// Run 停在半轮上时，后一个 Run 的输入会落在它的调用与调用结果之间。照搬这个位置发出去，
/// provider 看到的是"助手要了一次调用、紧接着另一个 Run 的用户消息、最后才是那次调用的
/// 输出"，直接 400（`No tool output found for tool call …`）。§8.4 的次序本来就是 Run
/// 之间不越过，转写按它排：每个 Run 的那几句连在一起，Run 之间按先来后到。
///
/// 还没被领走的 Run 整个不进转写（`awaits_claim`）：它的输入已经落盘，但这一轮还没轮到
/// 它。
///
/// `scope` 挑的是**哪些 Run**；`None` = 全都要，[`latest_user_text`] 用它——用户最后说的
/// 那句不该被另一个 Run 的输入顶掉。
fn window<'s>(surface: &'s Surface, scope: Option<ReplayScope<'_>>) -> Vec<&'s SurfaceMessage> {
    let kept: Vec<&SurfaceMessage> = surface
        .replay()
        .iter()
        .filter(|message| match scope {
            None => true,
            // 子代理与父在同一份日志里，但它们不是同一段对话：子代理跑过的那几轮不能进
            // 父的窗口，父的也没进过子代理的。
            Some(ReplayScope::Run(run)) => message.run.as_ref() == Some(run),
            Some(ReplayScope::Conversation(_)) => message
                .run
                .as_ref()
                .and_then(|run| surface.runs.get(run))
                .is_none_or(|view| view.delegate.is_none()),
        })
        .filter(|message| {
            message
                .run
                .as_ref()
                .and_then(|run| surface.runs.get(run))
                .is_none_or(|view| !view.status.awaits_claim())
        })
        .collect();

    // 每个 Run 的第一个 `seq` 就是它在对话里的位置；不在 Run 里的消息（`message.user`）
    // 按自己的 `seq` 排。
    let mut starts: BTreeMap<&RunId, Seq> = BTreeMap::new();
    for message in &kept {
        if let Some(run) = &message.run {
            let entry = starts.entry(run).or_insert(message.seq);
            *entry = (*entry).min(message.seq);
        }
    }

    let mut keyed: Vec<((Seq, Seq), &SurfaceMessage)> = kept
        .into_iter()
        .map(|message| {
            let key = match &message.run {
                Some(run) => (starts.get(run).copied().unwrap_or(Seq::ZERO), message.seq),
                None => (message.seq, message.seq),
            };
            (key, message)
        })
        .collect();
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, message)| message).collect()
}

/// 回放面上最后一条用户消息的正文——记忆召回的查询文本。
pub fn latest_user_text(surface: &Surface) -> Option<String> {
    window(surface, None)
        .into_iter()
        .rfind(|message| message.role == Role::User)
        .and_then(|message| message.text.clone())
}

/// 纯函数：把已解析正文的回放条目投影成交给模型的消息（`docs/agent.md` §8）。
///
/// **只有一处调用 `kernel::projection::project`**：工具结果的模型视图不在这里重新拼。
/// `Transcript` 条目正文为空（读不到、或者本来就是空字符串）就跳过——那句话本来就没有
/// 值得回放的内容。
///
/// `pub(crate)`：Gateway 不该绕过 [`super::assemble`] 直接拿到 `Vec<ReplayMessage>`——
/// **唯一的 Context Assembly 入口**只有那一个（§5）。
pub(crate) fn to_replay_messages(
    history: Vec<ResolvedMessage<'_>>,
    model_result_bytes: usize,
) -> Vec<ReplayMessage> {
    // 结果落在**另一条**消息上（`Role::Tool` 的节点），工具名与 provider 的 call_id 要靠
    // 这份索引从原始请求里找回来——先把这一段窗口里出现过的调用都记一遍。
    let mut requests: BTreeMap<&ToolCallId, &ToolCallRequest> = BTreeMap::new();
    for resolved in &history {
        for call in &resolved.entry.message.tool_calls {
            requests.insert(&call.call_id, call);
        }
    }

    let mut out = Vec::new();
    for resolved in history {
        let message = resolved.entry.message;
        match resolved.entry.kind {
            EntryKind::Transcript => {
                let Some(text) = resolved.text else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                out.push(ReplayMessage {
                    role: message.role,
                    seq: message.seq,
                    text: Some(text),
                    tool_calls: Vec::new(),
                    tool_results: Vec::new(),
                    provider_blocks: None,
                });
            }
            EntryKind::Protocol => {
                let mut tool_results = Vec::new();
                for (result, stored) in message.tool_results.iter().zip(resolved.outputs.iter()) {
                    // 完整正文在 `output.json` 里；没读回来（没有输出存储、或者读不回来）
                    // 时退回账本里那份 ≤1 KiB 的预览——那是"我们至少还有这些"，不是
                    // "本该如此"。
                    let text = stored
                        .as_ref()
                        .and_then(|stored| stored.preview.as_deref())
                        .map(str::to_string)
                        .or_else(|| result.preview.clone());
                    let artifacts: &[ContentRef] = stored
                        .as_ref()
                        .map(|stored| stored.artifacts.as_slice())
                        .unwrap_or_default();
                    let request = requests.get(&result.call).copied();
                    let facts = ToolResultFacts {
                        tool: request.map(|call| call.name.as_str()).unwrap_or("?"),
                        status: result.status,
                        elapsed_ms: result.elapsed_ms,
                        text: text.as_deref(),
                        output: &result.output,
                        stdout: result.stdout.as_ref(),
                        stderr: result.stderr.as_ref(),
                        artifacts,
                    };
                    tool_results.push(ToolResultForModel {
                        provider_call_id: request
                            .map(|call| call.provider_call_id.clone())
                            .unwrap_or_else(|| result.call.to_string()),
                        call_id: result.call.clone(),
                        content: project(&facts, &ProjectionContext { model_result_bytes }),
                        is_error: !matches!(result.status, ToolResultStatus::Completed),
                    });
                }
                out.push(ReplayMessage {
                    role: message.role,
                    seq: message.seq,
                    text: resolved.text,
                    tool_calls: message.tool_calls.clone(),
                    tool_results,
                    provider_blocks: message.provider_blocks.clone(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
