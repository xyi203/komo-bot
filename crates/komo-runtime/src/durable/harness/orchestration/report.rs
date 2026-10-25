//! 交回结果（`docs/background-tasks.md` §2；`docs/storage-durable.md` §4.6）：子 conversation 的
//! run 收尾的**同一次提交**里，往父 conversation submit 一条输入。
//!
//! - **原子**：交回与 run 的结局（输入 settle、`run` 移除、任务终结）同在一次提交里，不会出现
//!   "终态了、交回之前崩溃"那个旧窗口（`docs/background-tasks.md` §3 / §7）。
//! - **幂等**：请求键 `task-result:{子 conversation}:{这条 run 的首个输入}`，在父 conversation 里
//!   去重。run 的收尾只发生一次（`end_run` 只在 `komo.live.run` 还指着这个任务时才结束它），
//!   键是第二道保险：重放同一笔收尾拿回原来那条，不会交两次。
//! - **父忙时**按收件箱规则排队：交回的输入是 steer，在父的 postTools boundary 并进当前 run
//!   （旧实现的"插话并入"），父空闲就直接放置、起 run。
//! - **内部输入**：交回的输入带 `internal` 标记，父的 `Conversation.abort()` 不撤它；父的 run
//!   非正常收尾（取消、模型失败、预算、fault、orphan）时它也不卡在收件箱，而是同一次提交里用它
//!   起后继 run（[`end_run_abnormal`]），父照旧跑一轮把结论转告用户——旧 komo 取消 home 的 Run
//!   不动任务结果的行为。这是对 pi 的有意偏离（pi 的子 agent 交回是普通 followUp，会被撤）。
//!
//! 收尾的每个出口（回答、模型失败、预算、abort、调度器写的 faulted / orphaned）都经
//! [`end_run`] / [`end_run_abnormal`]，所以任务无论怎么结束父都会收到一条（"已完成"或"没有
//! 完成"）。

use komo_kernel::durable::{
    Content, ConversationId, EntryRecord, Message, SubmissionSettlement, TaskId,
};
use komo_kernel::types::delegate::DelegateContract;
use serde_json::{Value, json};

use super::{LastRun, TASK_RESULT_ENTRY, load_task_doc, store_task_doc};
use crate::durable::error::DurableError;
use crate::durable::harness::generation::GenerationTask;
use crate::durable::harness::inbox::{self, At, Boundary, InputEntry, SubmissionDraft, WhenBusy};
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

/// 结束 `task` 负责的 run，run 非正常收尾（取消、模型失败、预算、fault、orphan）时用：同
/// [`end_run`]，之后收件箱里还有内部输入就在**同一次提交**里起后继 run。
///
/// - `boundary` 由调用方在闭包开头（第一次表写之前）[`inbox::prepare_boundary`] 备好——放置要读
///   最新的 head 标记。收件箱里没有内部输入就什么都不做：write 与用户输入照 pi 留着，等下一条
///   输入的 boundary（失败后用户输入也留着；取消时它们已被 `withdraw_queued_inputs` 撤掉）。
/// - 放置顺序同 boundary：先排队的 write，再内部输入，各按 ID 序；用户输入不放。
/// - **不会死循环**：内部输入一旦放置就成了后继 run 的输入，随这条 run settle，不在收件箱里了，
///   不会被重放。后继 run 再被取消 / 再失败，收件箱里没有新的内部输入就不再起 run。
/// - 不是 `task` 负责的 run（已经结束过，重放同一笔收尾）什么都不做。
/// - 调用方随后把 `live` 写回。
pub(crate) async fn end_run_abnormal(
    tx: &mut Tx,
    generation: &GenerationTask,
    conversation: ConversationId,
    live: &mut LiveState,
    task: TaskId,
    settlement: &SubmissionSettlement,
    mut boundary: Boundary,
) -> Result<(), DurableError> {
    if !end_run(tx, generation, conversation, live, task, settlement).await? {
        return Ok(());
    }
    if !boundary.has_internal() {
        return Ok(());
    }
    let now = tx.now();
    let result = inbox::apply_boundary(tx, &mut boundary, At::Successor, now).await?;
    if !result.users.is_empty() {
        inbox::start_run(tx, generation, conversation, live, result.users).await?;
    }
    Ok(())
}

/// 结束 `task` 负责的 run（同 [`live::end_run`]），并在这是一个后台任务的 conversation 时把结果
/// 交回父 conversation。返回这次有没有结束一个 run。调用方随后把 `live` 写回。
pub(crate) async fn end_run(
    tx: &mut Tx,
    generation: &GenerationTask,
    conversation: ConversationId,
    live: &mut LiveState,
    task: TaskId,
    settlement: &SubmissionSettlement,
) -> Result<bool, DurableError> {
    let Some(run) = live::end_run(tx, live, task, settlement).await? else {
        return Ok(false);
    };
    report(tx, generation, conversation, &run, settlement).await?;
    Ok(true)
}

/// run 结束了，往父交回结果（这不是后台任务的 conversation、父不在了、没有输入都什么都不做）。
async fn report(
    tx: &mut Tx,
    generation: &GenerationTask,
    conversation: ConversationId,
    run: &live::RunControl,
    settlement: &SubmissionSettlement,
) -> Result<(), DurableError> {
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
        // 交回的结果是内部输入：父被取消时不撤，非正常收尾后用它起后继 run。
        internal: true,
    };
    inbox::admit(tx, generation, parent, &draft, generation.modes()).await?;
    Ok(())
}
