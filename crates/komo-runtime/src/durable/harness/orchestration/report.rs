//! 交回结果（`docs/background-tasks.md` §2；`docs/storage-durable.md` §4.6）：子 conversation 的
//! run 收尾的**同一次提交**里，往父 conversation submit 一条输入。
//!
//! - **原子**：交回与 run 的结局（输入 settle、`run` 移除、任务终结）同在一次提交里，不会出现
//!   "终态了、交回之前崩溃"那个旧窗口（`docs/background-tasks.md` §3 / §7）。
//! - **幂等**：请求键 `task-result:{子 conversation}:{这条 run 的首个输入}`，在父 conversation 里
//!   去重。run 的收尾只发生一次（`end_run` 只在 `komo.live.run` 还指着这个任务时才结束它），
//!   键是第二道保险：重放同一笔收尾拿回原来那条，不会交两次。
//! - **父忙时**按收件箱规则排队：交回的输入是 steer，在父的 postTools boundary 并进当前 run
//!   （旧实现的"插话并入"），父空闲就直接放置、起 run。父的 `Conversation.abort()` 会撤回排队中
//!   的 steer / followUp——包括还没放置的交回；这是已知的缺口（见 §13.3）。
//!
//! 收尾的每个出口（回答、模型失败、预算、abort、调度器写的 faulted / orphaned）都经
//! [`end_run`]，所以任务无论怎么结束父都会收到一条（"已完成"或"没有完成"）。

use komo_kernel::durable::{
    Content, ConversationId, EntryRecord, Message, SubmissionSettlement, TaskId,
};
use komo_kernel::types::delegate::DelegateContract;
use serde_json::{Value, json};

use super::{LastRun, TASK_RESULT_ENTRY, load_task_doc, store_task_doc};
use crate::durable::error::DurableError;
use crate::durable::harness::generation::GenerationTask;
use crate::durable::harness::inbox::{self, InputEntry, SubmissionDraft, WhenBusy};
use crate::durable::harness::live::{self, LiveState};
use crate::durable::tx::Tx;

/// 一条任务 run 怎么收尾的（交回消息的标题那一半）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Completed,
    NotCompleted,
}

fn answer_text(entry: &EntryRecord) -> String {
    entry
        .model
        .iter()
        .flatten()
        .find_map(|message| match message {
            Message::Assistant(message) => Some(
                message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        komo_kernel::durable::ContentBlock::Text { text, .. } => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

fn detail_text(detail: Option<&Value>) -> String {
    match detail {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// 没有完成的原因，给父会话的模型看。
fn failure_text(reason: &str, detail: Option<&Value>) -> String {
    let detail = detail_text(detail);
    let with = |head: &str| {
        if detail.is_empty() {
            head.to_owned()
        } else {
            format!("{head}：{detail}")
        }
    };
    match reason {
        "aborted" => "已取消".into(),
        "no_model" => with("没有可用的模型"),
        "model_error" => with("模型调用失败"),
        "budget" => with("超出预算"),
        "faulted" => with("任务出错"),
        "reset" => "对话被重置，没有给出回复".into(),
        other => with(&format!("没有完成（{other}）")),
    }
}

/// 复验结论：与旧委派的父侧收尾同一份取 JSON + 校验（`executor::check_contract`）。
fn contract_verdict(contract: &DelegateContract, text: &str) -> String {
    match crate::executor::check_contract(contract, text) {
        Ok((_, validation)) if validation.ignored.is_empty() => "（结果契约复验：符合。）".into(),
        Ok((_, validation)) => format!(
            "（结果契约复验：符合；这些关键字没有检查：{}。）",
            validation.ignored.join("、")
        ),
        Err(report) => format!("（结果契约复验：不符合——{report}。）"),
    }
}

/// 结束 `task` 负责的 run（同 [`live::end_run`]），并在这是一个后台任务的 conversation 时把结果
/// 交回父 conversation。调用方随后把 `live` 写回。
pub(crate) async fn end_run(
    tx: &mut Tx,
    generation: &GenerationTask,
    conversation: ConversationId,
    live: &mut LiveState,
    task: TaskId,
    settlement: &SubmissionSettlement,
) -> Result<(), DurableError> {
    let Some(run) = live::end_run(tx, live, task, settlement).await? else {
        return Ok(());
    };
    let Some(mut doc) = load_task_doc(tx, conversation).await? else {
        return Ok(());
    };
    let Some(first) = run.inputs.first().copied() else {
        return Ok(());
    };

    let (outcome, text, last) = match settlement {
        SubmissionSettlement::Done { answer } => {
            let text = tx
                .staged_entry(*answer)
                .map(answer_text)
                .unwrap_or_default();
            let last = LastRun {
                state: "completed".into(),
                ended_at: tx.now(),
                final_message: Some(text.clone()),
                reason: None,
            };
            (Outcome::Completed, text, last)
        }
        SubmissionSettlement::Unanswered { reason, detail } => {
            let text = failure_text(reason, detail.as_ref());
            let last = LastRun {
                state: if reason == "aborted" {
                    "cancelled".into()
                } else {
                    "failed".into()
                },
                ended_at: tx.now(),
                final_message: None,
                reason: Some(text.clone()),
            };
            (Outcome::NotCompleted, text, last)
        }
    };
    let contract = doc.contracts.remove(&first.to_string());
    doc.last_run = Some(last);
    store_task_doc(tx, conversation, &doc).await?;

    // 父不在了就没有可交回的对象；不能因此让这次收尾失败（失败路径会再走一遍这里）。
    let parent = doc.parent;
    if tx.conversation(parent).await?.is_none() {
        return Ok(());
    }
    let headline = match outcome {
        Outcome::Completed => "已完成",
        Outcome::NotCompleted => "没有完成",
    };
    let verdict = match (outcome, &contract) {
        (Outcome::Completed, Some(contract)) => {
            format!("{}\n\n", contract_verdict(contract, &text))
        }
        _ => String::new(),
    };
    let input = format!(
        "[后台任务 #{handle}「{title}」{headline}]\n\n{text}\n\n{verdict}\
         （这是后台任务交回的结果，不是用户发来的消息；用户看不到上面这段，请把结论转告用户。）",
        handle = conversation,
        title = doc.title,
    );
    let draft = SubmissionDraft::Input {
        content: Content::Text(input),
        entry: Some(InputEntry::new(
            TASK_RESULT_ENTRY,
            Some(json!({
                "task": {
                    "conversation": conversation,
                    "handle": conversation.to_string(),
                    "title": doc.title,
                    "outcome": match outcome {
                        Outcome::Completed => "completed",
                        Outcome::NotCompleted => "notCompleted",
                    },
                }
            })),
        )),
        when_busy: WhenBusy::Steer,
        request_id: Some(format!("task-result:{conversation}:{first}")),
    };
    inbox::admit(tx, generation, parent, &draft, generation.modes()).await?;
    Ok(())
}
