//! `task`：派出、接着做、查进度（工具任务的 `call` 阶段）。结构见 [`super`] 的模块文档。

use std::collections::BTreeMap;

use komo_kernel::durable::{
    Content, ContentBlock, ConversationId, EntryId, EntryRecord, Message, StopReason,
    ToolResultMessage,
};
use komo_kernel::types::delegate::DelegateContract;
use komo_kernel::types::ids::{RunId, ToolCallId};
use komo_kernel::types::model::Effort;
use komo_kernel::types::status::RunState;
use komo_kernel::types::task::{RunProgress, RunningCall, TaskProgress};
use serde_json::json;
use time::OffsetDateTime;

use super::{
    BackgroundTask, Cx, FORK_CALL_RESULT, FORK_LATER_CALL_RESULT, FORK_NOTICE, Reply, TaskDoc,
    load_task_doc, store_task_doc, task_doc_address,
};
use crate::durable::error::{DurableError, TaskError};
use crate::durable::harness::entries::TOOL_RESULT;
use crate::durable::harness::generation::GenerationTask;
use crate::durable::harness::inbox::{self, Admitted, QueueModes, SubmissionDraft, WhenBusy};
use crate::durable::harness::live::{self, LiveState, SlotStatus, live_address};
use crate::durable::tx::{ConversationOwnership, EntryDraft, Ownership, TaskOptions};

/// 模型给的、`prepare` 验过的参数。
pub(super) struct Request {
    pub prompt: String,
    pub title: Option<String>,
    pub task_id: Option<String>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub contract: Option<DelegateContract>,
    pub fork: bool,
}

pub(super) async fn run(cx: &Cx<'_>, request: Request) -> Result<(), TaskError> {
    match request.task_id.clone() {
        None => spawn(cx, request).await,
        Some(id) => follow(cx, &id, request).await,
    }
}

fn queue_modes(cx: &Cx<'_>) -> QueueModes {
    let settings = cx.tool.deps().settings();
    QueueModes {
        steering: settings.steering_mode,
        follow_up: settings.follow_up_mode,
    }
}

fn generation(cx: &Cx<'_>) -> GenerationTask {
    GenerationTask::new(cx.tool.deps().clone(), cx.tool.clone())
}

/// 任务那条输入的正文：任务，带契约时后面补契约段。
fn input_text(cx: &Cx<'_>, prompt: &str, contract: Option<&DelegateContract>) -> String {
    let mut text = prompt.to_owned();
    if let Some(contract) = contract {
        text.push_str("\n\n");
        text.push_str(&cx.hooks().contract_block(contract));
    }
    text
}

// ---------------------------------------------------------------- 派出

/// fork 的切口：子 conversation 从父的这条 entry 起，加上要补结果的调用。
struct ForkSeed {
    at: EntryId,
    /// `(call_id, tool, 结果正文)`：发起这次调用的那条 assistant 消息里还没有结果的调用。
    synth: Vec<(String, String, &'static str)>,
}

/// fork 切在父**此刻**的最新 entry 上（含这一轮里排在前面、已经收尾的调用结果——编排调用
/// 是顺序组，它们先于这次调用落账），而不是 assistant entry 本身：那样同一轮前面的结果会
/// 落在切口之外，在 fork 里变成"结果缺失"。发起调用的那条消息里还没有结果的调用在 fork 里
/// 补一条结果，只进 C，不进父。
async fn fork_seed(cx: &Cx<'_>) -> Result<Result<ForkSeed, String>, TaskError> {
    let entries = cx.rt.entries(cx.conversation(), None).await?;
    let Some(at) = entries.last().map(|entry| entry.id) else {
        return Ok(Err("派任务的会话里还没有任何内容".into()));
    };
    let Some(assistant) = entries.iter().find(|entry| entry.id == cx.assistant) else {
        return Ok(Err("发起这次调用的 assistant 消息不在派任务的会话里".into()));
    };
    let calls: Vec<(String, String)> = assistant
        .model
        .iter()
        .flatten()
        .filter_map(|message| match message {
            Message::Assistant(message) => Some(
                message
                    .tool_calls()
                    .map(|(id, name)| (id.to_owned(), name.to_owned()))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    if !calls.iter().any(|(id, _)| *id == cx.call.id) {
        return Ok(Err(
            "派出这个 fork 的调用不在发起它的 assistant 消息里".into()
        ));
    }
    let answered: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.id > cx.assistant)
        .flat_map(|entry| entry.model.iter().flatten())
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    let synth = calls
        .iter()
        .filter(|(id, _)| !answered.contains(&id.as_str()))
        .map(|(id, name)| {
            let text = if *id == cx.call.id {
                FORK_CALL_RESULT
            } else {
                FORK_LATER_CALL_RESULT
            };
            (id.clone(), name.clone(), text)
        })
        .collect();
    Ok(Ok(ForkSeed { at, synth }))
}

async fn spawn(cx: &Cx<'_>, request: Request) -> Result<(), TaskError> {
    let parent = cx.conversation();
    // 任务会话里不能再派任务：结果只交回一层，再往下派就成了一棵没人看得见的树。
    if cx
        .rt
        .find_document(&task_doc_address(parent))
        .await?
        .is_some()
    {
        return cx
            .reply(&Reply::error(
                "派任务失败：这里已经是一个后台任务了，不能再派任务：自己把这件事做完",
            ))
            .await;
    }
    if let Some(effort) = &request.effort
        && let Err(problem) = cx
            .guarded(
                cx.hooks()
                    .check_effort(parent, request.model.as_deref(), effort),
            )
            .await?
    {
        return cx
            .reply(&Reply::error(format!(
                "派任务失败：effort 立不住：{problem}"
            )))
            .await;
    }
    let seed = if request.fork {
        match fork_seed(cx).await? {
            Ok(seed) => Some(seed),
            Err(reason) => {
                return cx
                    .reply(&Reply::error(format!("派任务失败：fork 不出来：{reason}")))
                    .await;
            }
        }
    } else {
        None
    };

    let title = request.title.clone().unwrap_or_default();
    let mut text = input_text(cx, &request.prompt, request.contract.as_ref());
    if request.fork {
        text = format!("{FORK_NOTICE}\n\n{text}");
    }
    let generation = generation(cx);
    let Request {
        model,
        effort,
        contract,
        fork,
        ..
    } = request;
    cx.commit_effect(move |tx, task| {
        Box::pin(async move {
            let now = tx.now();
            let anchor = tx
                .create_task(
                    &BackgroundTask,
                    json!({ "parent": parent, "title": title }),
                    Ownership::Conversation,
                    TaskOptions {
                        conversation: Some(parent),
                        background: true,
                        abandon_on_restart: false,
                    },
                )
                .await?;
            let ownership = ConversationOwnership::Task(anchor);
            let child = match &seed {
                Some(seed) => tx.fork_conversation(parent, seed.at, ownership).await?,
                None => tx.create_conversation(ownership).await?,
            }
            .id;
            if let Some(seed) = &seed {
                for (call_id, tool, text) in &seed.synth {
                    let message = ToolResultMessage {
                        tool_call_id: call_id.clone(),
                        tool_name: tool.clone(),
                        content: vec![ContentBlock::text(*text)],
                        is_error: false,
                        details: Some(json!({ "reason": "fork" })),
                        timestamp: now,
                        extra: Default::default(),
                    };
                    tx.append_entry(
                        child,
                        EntryDraft::new(TOOL_RESULT)
                            .with_model(vec![Message::ToolResult(message)])
                            .with_data(json!({ "harness": "fork" })),
                    )
                    .await?;
                }
            }
            let mut live = LiveState::default();
            let first = inbox::place_idle_input(
                tx,
                &generation,
                child,
                &mut live,
                &Content::Text(text),
                None,
                Some(&format!("task:{}", task.id())),
                now,
            )
            .await?;
            live::store(tx, child, &live).await?;
            let mut doc = TaskDoc {
                parent,
                anchor,
                title: title.clone(),
                fork,
                model,
                effort,
                contracts: BTreeMap::new(),
                last_run: None,
            };
            if let Some(contract) = contract {
                doc.contracts.insert(first.to_string(), contract);
            }
            store_task_doc(tx, child, &doc).await?;
            let reply = Reply::ok(
                format!("已派出 #{child}：{title}"),
                json!({
                    "conversation": child,
                    "handle": child.to_string(),
                    "title": title,
                }),
            );
            Ok(reply)
        })
    })
    .await
}

// ---------------------------------------------------------------- 找到任务

/// `task_id`（`#12` 或 `12`）解析成调用方派出去的那个子 conversation。
async fn resolve(
    cx: &Cx<'_>,
    raw: &str,
) -> Result<Result<(ConversationId, TaskDoc), Reply>, TaskError> {
    let unknown = || {
        Reply::error(format!(
            "#{} 不是这个会话派出去的任务",
            raw.trim_start_matches('#')
        ))
    };
    let Ok(id) = raw.trim().trim_start_matches('#').parse::<u64>() else {
        return Ok(Err(unknown()));
    };
    let child = ConversationId::new(id);
    let Some(record) = cx.rt.find_document(&task_doc_address(child)).await? else {
        return Ok(Err(unknown()));
    };
    let doc: TaskDoc = serde_json::from_value(record.value)
        .map_err(|e| TaskError::new(format!("komo.task is malformed: {e}")))?;
    if doc.parent != cx.conversation() {
        return Ok(Err(unknown()));
    }
    Ok(Ok((child, doc)))
}

// ---------------------------------------------------------------- 接着做

async fn follow(cx: &Cx<'_>, raw: &str, request: Request) -> Result<(), TaskError> {
    let (child, doc) = match resolve(cx, raw).await? {
        Ok(found) => found,
        Err(reply) => {
            return cx
                .reply(&Reply::error(format!("转不过去：{}", reply.text)))
                .await;
        }
    };
    if let Some(effort) = &request.effort {
        let model = request.model.as_deref().or(doc.model.as_deref());
        if let Err(problem) = cx
            .guarded(cx.hooks().check_effort(child, model, effort))
            .await?
        {
            return cx
                .reply(&Reply::error(format!("转不过去：effort 立不住：{problem}")))
                .await;
        }
    }
    let text = input_text(cx, &request.prompt, request.contract.as_ref());
    // 追问以插话受理：任务正忙时在它下一步之前并进当前 run；带契约的照旧排队——并入沿用宿主的
    // 契约，新契约会丢。
    let when_busy = if request.contract.is_none() {
        WhenBusy::Steer
    } else {
        WhenBusy::FollowUp
    };
    let generation = generation(cx);
    let modes = queue_modes(cx);
    let Request {
        model,
        effort,
        contract,
        ..
    } = request;
    cx.commit_effect(move |tx, task| {
            Box::pin(async move {
                // 表读（请求键、head）都在 `admit` 里，先于这次提交的第一次表写。
                let busy = live::load(tx, child).await?.is_busy();
                let draft = SubmissionDraft::Input {
                    content: Content::Text(text),
                    entry: None,
                    when_busy,
                    request_id: Some(format!("task:{}", task.id())),
                };
                let Admitted::Id(id) = inbox::admit(tx, &generation, child, &draft, modes).await?
                else {
                    return Err(DurableError::rejected("the follow-up was not admitted"));
                };
                let mut doc = load_task_doc(tx, child)
                    .await?
                    .ok_or_else(|| DurableError::rejected("komo.task disappeared"))?;
                // 给了 model：连同它的 effort 换掉；只给 effort：模型沿用，档位换掉；都没给：沿用。
                if let Some(model) = model {
                    doc.model = Some(model);
                    doc.effort = effort;
                } else if effort.is_some() {
                    doc.effort = effort;
                }
                if let Some(contract) = contract {
                    doc.contracts.insert(id.to_string(), contract);
                }
                store_task_doc(tx, child, &doc).await?;
                let text = if busy {
                    // 受理那一刻分不出是并进去还是排在后面，两句都说。
                    format!(
                        "#{child} 正在跑，这句会在它下一步之前并进去；并不进去的（带结果契约等）排在它后面"
                    )
                } else {
                    format!("已转给 #{child}")
                };
                let reply = Reply::ok(
                    text,
                    json!({ "conversation": child, "handle": child.to_string(), "queued_behind": busy }),
                );
                Ok(reply)
            })
        })
        .await
}

// ---------------------------------------------------------------- 查进度

fn at(ms: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn message_time(message: &Message) -> i64 {
    match message {
        Message::User(m) => m.timestamp,
        Message::Assistant(m) => m.timestamp,
        Message::ToolResult(m) => m.timestamp,
        Message::System(m) => m.timestamp,
    }
}

fn assistant_text(entry: &EntryRecord) -> Option<String> {
    entry
        .model
        .iter()
        .flatten()
        .find_map(|message| match message {
            Message::Assistant(message) => {
                let text = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (!text.trim().is_empty()).then_some(text)
            }
            _ => None,
        })
}

fn is_final_answer(entry: &EntryRecord) -> bool {
    entry.model.iter().flatten().any(
        |message| matches!(message, Message::Assistant(m) if m.stop_reason == StopReason::Stop),
    )
}

/// 输出尾巴只要最后几行。
fn tail_lines(text: &str, n: usize) -> Option<String> {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    let from = lines.len().saturating_sub(n);
    Some(lines[from..].join("\n"))
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…")
    }
}

/// 子 conversation 现在的样子：`komo.live`、entry 与 `komo.task` 折出来的快照。一条 run 的范围是
/// 上一条最终回答（`stop`）之后，已收尾的 run 取它自己那一段。
async fn progress(
    cx: &Cx<'_>,
    child: ConversationId,
    doc: &TaskDoc,
) -> Result<TaskProgress, TaskError> {
    let live: LiveState = match cx.rt.find_document(&live_address(child)).await? {
        Some(record) => serde_json::from_value(record.value)
            .map_err(|e| TaskError::new(format!("komo.live is malformed: {e}")))?,
        None => LiveState::default(),
    };
    let entries = cx.rt.entries(child, None).await?;
    let now = cx.rt.now()?;
    let handle = child.to_string();
    let finals: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| is_final_answer(entry))
        .map(|(index, _)| index)
        .collect();
    let running = live.run.is_some();
    let finished_ok = !running
        && doc
            .last_run
            .as_ref()
            .is_some_and(|run| run.state == "completed");
    let span: &[EntryRecord] = match (finished_ok, finals.as_slice()) {
        (true, [.., prev, last]) => &entries[prev + 1..=*last],
        (true, [last]) => &entries[..=*last],
        (_, [.., last]) => &entries[last + 1..],
        _ => &entries[..],
    };
    let messages: Vec<&Message> = span.iter().flat_map(|e| e.model.iter().flatten()).collect();
    let first_user = messages.iter().find(|m| m.is_user());
    // 子 conversation 里还没有任何输入的 run：什么都没发生。
    let Some(first_user) = first_user else {
        return Ok(TaskProgress {
            short_id: handle,
            title: doc.title.clone(),
            run: None,
        });
    };
    let accepted = message_time(first_user);
    let last_at = messages
        .iter()
        .map(|m| message_time(m))
        .max()
        .unwrap_or(accepted);
    let calls = messages
        .iter()
        .filter(|m| matches!(m, Message::ToolResult(_)))
        .count() as u32;
    let note = span
        .iter()
        .rev()
        .filter_map(assistant_text)
        .next()
        .map(|text| clip(&text, 200));

    let (state, final_message, reason, waiting_on) = if running {
        let retrying = live
            .generation
            .as_ref()
            .and_then(|generation| generation.retry.as_ref());
        match retrying {
            Some(retry) => (
                RunState::Waiting,
                None,
                None,
                Some(format!("模型请求失败后的重试：{}", clip(&retry.error, 120))),
            ),
            None => (RunState::Running, None, None, None),
        }
    } else {
        match doc.last_run.as_ref() {
            Some(last) => {
                let state = match last.state.as_str() {
                    "completed" => RunState::Completed,
                    "cancelled" => RunState::Cancelled,
                    _ => RunState::Failed,
                };
                (state, last.final_message.clone(), last.reason.clone(), None)
            }
            // 有输入、没有 run、也没有收尾记录：排在收件箱里还没开跑。
            None => (RunState::Queued, None, None, None),
        }
    };
    let running_calls = live
        .tools
        .iter()
        .flatten()
        .filter(|slot| slot.status != SlotStatus::Done)
        .map(|slot| RunningCall {
            call: ToolCallId::from_raw(slot.call_id.clone()),
            attempt: None,
            tool: slot.name.clone(),
            summary: String::new(),
            started_at: None,
            output_tail: slot.output.as_deref().and_then(|text| tail_lines(text, 8)),
        })
        .collect();
    Ok(TaskProgress {
        short_id: handle,
        title: doc.title.clone(),
        run: Some(RunProgress {
            run: RunId::from_raw(child.to_string()),
            state,
            accepted_at: at(accepted),
            last_at: at(last_at.max(accepted).min(now.max(last_at))),
            rounds: live.run.as_ref().map_or(0, |run| run.rounds),
            calls,
            running: running_calls,
            note,
            final_message,
            reason,
            waiting_on,
        }),
    })
}

pub(super) async fn status(cx: &Cx<'_>, raw: &str) -> Result<(), TaskError> {
    let (child, doc) = match resolve(cx, raw).await? {
        Ok(found) => found,
        Err(reply) => {
            return cx
                .reply(&Reply::error(format!("查不到进度：{}", reply.text)))
                .await;
        }
    };
    let progress = progress(cx, child, &doc).await?;
    let text = progress.render(at(cx.rt.now()?));
    let result = serde_json::to_value(&progress).unwrap_or_else(|_| json!({}));
    cx.reply(&Reply::ok(text, result)).await
}
