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
//!   非正常收尾（取消、模型失败、预算、fault、orphan）时它也不卡在收件箱，而是同一次提交里随
//!   收件箱里排着的其余输入一起放置、起后继 run（[`end_run_abnormal`]），父照旧跑一轮把结论转告
//!   用户——旧 komo 取消 home 的 Run 不动任务结果的行为。这是对 pi 的有意偏离（pi 的子 agent
//!   交回是普通 followUp，会被撤）。
//!
//! 收尾的每个出口（回答、模型失败、预算、abort、调度器写的 faulted / orphaned）都经
//! [`end_run`] / [`end_run_abnormal`]，所以任务无论怎么结束父都会收到一条（"已完成"或"没有
//! 完成"）。

use komo_kernel::durable::{
    Content, ConversationId, EntryRecord, Message, SessionLifecycle, SubmissionSettlement, TaskId,
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
/// [`end_run`]，之后在**同一次提交**里把收件箱里排着的东西按 final boundary 放置，放了输入就起
/// 后继 run——"前一轮怎么结束都不挡后面的消息"（D5 决定 8，旧 komo 的行为）。
///
/// - `boundary` 由调用方在闭包开头（第一次表写之前）[`inbox::prepare_boundary`] 备好——放置要读
///   最新的 head 标记。
/// - 放置顺序同 final boundary：先排队的 write，再 steer 与 followUp（用户的与内部的一起，各按
///   ID 序，队列模式照旧：`OneAtATime` 每种取第一项，余下的留给后继 run 自己的 boundary）。
///   没有输入可放（只有 write 或收件箱空）就不起 run，write 照样放了。
/// - **取消时排队的用户输入**去留不在这里决定：`Harness::abort_conversation` 先撤回它们再 abort，
///   收件箱里就只剩内部输入；`Harness::abort_run` 不撤，它们和别的一起在这里放置。后继 run 里
///   放进来的用户 steer 是这条 run 的**起始输入**（没有"当前 run"可并），不是插话；之后新来的
///   steer 才在后继 run 的 postTools boundary 并入。
/// - **取消与放置的竞争**：abort 标记、run 自己的 boundary 提交、本函数所在的收尾提交都串在同一条
///   提交线上。run 自己的 boundary 提交（放 steer、交棒下一个生成任务、`answer` 的 final boundary）
///   要么先于标记——那条 run 已走到下一个任务或已正常收尾，`abort_run` 重读 `komo.live` 会追着同一条
///   run 的新任务再 abort；要么晚于标记——被 abort fence 拒掉，什么都没放到一半。放置只发生在本函数
///   这一次提交里，所以一个排队项不会"既放了又还在收件箱"；标记之后、收尾提交之前受理的输入因为
///   `live.run` 还在而排队，同一次提交里被放进来；收尾提交之后受理的输入看到 run 已移除，照常处理
///   （空闲且收件箱空就直接起 run，否则排在后继 run 后面）。
/// - **不会死循环**：放置的输入就是后继 run 的输入，随这条 run settle，不在收件箱里了，不会被
///   重放；后继 run 再被取消 / 再失败，只会放那时收件箱里新有的东西，每个后继 run 至少消耗一个
///   排队的输入，没有新输入就不再起 run。
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
    if boundary.inbox.items.is_empty() {
        return Ok(());
    }
    let now = tx.now();
    let result = inbox::apply_boundary(tx, &mut boundary, At::Final, now).await?;
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
    let Some(parent_record) = tx.conversation(parent).await? else {
        return Ok(());
    };
    // 父所在的 Session 已经在删（`closing` / `deleted` / `purged`）：不再交回——交回的输入是内部
    // 输入，会在父里起后继 run，删除流程撤过一遍之后晚到的结果不能再让已删的会话动起来。
    if let Some(session) = parent_record.session.as_deref()
        && tx
            .session(session)
            .await?
            .is_some_and(|record| record.lifecycle != SessionLifecycle::Active)
    {
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
