//! 大结果完整给过几次之后换成短视图（§8.3）：活着的这一段里，**什么时候换**。
//!
//! 换成哪个视图早在投影时就定了（`projection::at_send`，结果上那份 [`PendingDecay`]），
//! 这里只数次数。次数的唯一定义是"这条 Run 在这条结果之后记了几条 `message.assistant`"
//! ——回放（`komo-agent` 的 `to_replay_messages`）从日志里数同一个数，所以这一段里换掉的
//! 那份正文，就是下一段回放时直接给出的那份。
//!
//! 计数跟着 loop 的形状走：一次 `TurnDriver::next` 成功并 `record_round` 之后才算"给过
//! 一次"。请求失败不重发——loop 让出名额去等退避，下一段从日志重新数——所以活着的计数
//! 不会比账本多。

use std::collections::BTreeMap;

use komo_kernel::types::ids::ToolCallId;
use komo_kernel::types::turn::{PendingDecay, ReplayMessage, ToolResultForModel};

use crate::executor::CallRequest;

struct Tracked {
    tool: String,
    provider_call_id: String,
    call_id: ToolCallId,
    full_bytes: usize,
    decay: PendingDecay,
}

#[derive(Default)]
pub(crate) struct DecayTracker {
    tools: BTreeMap<ToolCallId, String>,
    pending: Vec<Tracked>,
}

impl DecayTracker {
    /// 从这一段的回放窗口起步：窗口里还在完整期的结果。`decay` 从消息上拿走——驱动不看它。
    pub(crate) fn seed(messages: &mut [ReplayMessage]) -> Self {
        let mut tracker = Self::default();
        for message in messages.iter() {
            for call in &message.tool_calls {
                tracker
                    .tools
                    .insert(call.call_id.clone(), call.name.clone());
            }
        }
        for message in messages.iter_mut() {
            tracker.track(&mut message.tool_results);
        }
        tracker
    }

    /// 记下这一轮调用各是哪个工具（只为 trace）。
    pub(crate) fn name(&mut self, calls: &[CallRequest]) {
        for call in calls {
            self.tools.insert(call.call.clone(), call.tool.clone());
        }
    }

    /// 刚交给模型的结果里还要换视图的那些。
    pub(crate) fn track(&mut self, results: &mut [ToolResultForModel]) {
        for result in results {
            let Some(decay) = result.decay.take() else {
                continue;
            };
            self.pending.push(Tracked {
                tool: self
                    .tools
                    .get(&result.call_id)
                    .cloned()
                    .unwrap_or_else(|| "?".into()),
                provider_call_id: result.provider_call_id.clone(),
                call_id: result.call_id.clone(),
                full_bytes: result.content.len(),
                decay,
            });
        }
    }

    /// 一次请求带着它们发出去、回复也记下了：每条都少一次。
    pub(crate) fn sent(&mut self) {
        for tracked in &mut self.pending {
            tracked.decay.remaining_full_sends =
                tracked.decay.remaining_full_sends.saturating_sub(1);
        }
    }

    /// 下一次请求之前到点的那些：换成短视图，交给驱动按 `provider_call_id` 就地改掉。
    pub(crate) fn due(&mut self) -> Vec<ToolResultForModel> {
        let (due, pending): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|tracked| tracked.decay.remaining_full_sends == 0);
        self.pending = pending;
        due.into_iter()
            .map(|tracked| {
                tracing::debug!(
                    target: "komo::observation",
                    tool = %tracked.tool,
                    full_bytes = tracked.full_bytes,
                    view_bytes = tracked.decay.view.len(),
                    "observation.decayed"
                );
                ToolResultForModel {
                    provider_call_id: tracked.provider_call_id,
                    call_id: tracked.call_id,
                    content: tracked.decay.view,
                    is_error: false,
                    decay: None,
                }
            })
            .collect()
    }
}
