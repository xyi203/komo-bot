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
//! [`end_run`] / [`end_answered_run`] / [`end_run_abnormal`]，所以任务无论怎么结束父都会收到一条
//! （标题是"已完成""受阻""交回了，没说做完没有"或"没有完成"，后面带工具调用轮数与用时）。
//!
//! - **状态行与自动续跑**（`docs/background-tasks.md` §2c）：任务的回答末行写
//!   [`TASK_STATUS_DONE`] / [`TASK_STATUS_BLOCKED`]，标题按它定——只看 `stopReason` 分不出"做完了"
//!   和"自己觉得差不多了"。没写状态行的回答先不交回：同一次提交里往任务会话放一条
//!   [`TASK_CONTINUE_ENTRY`] 输入、起后继 run（Claude Code 的 SubagentStop hook 返回 block 也是
//!   这样让子 agent 接着做）。以续跑输入开头的 run 不再续跑，所以每条派出 / 追问的输入最多续跑
//!   一次；带结果契约的回答只放 JSON，不看状态行。

use komo_kernel::durable::{
    Content, ConversationId, EntryId, EntryRecord, Message, SessionLifecycle, SubmissionId,
    SubmissionSettlement, TaskId,
};
use komo_kernel::types::chat::is_silent_reply;
use komo_kernel::types::delegate::{DelegateContract, Validation, validate};
use komo_kernel::types::handle::short_handle;
use komo_kernel::types::task::{TASK_STATUS_BLOCKED, TASK_STATUS_DONE, duration_words};
use serde_json::{Value, json};

use super::{
    LastRun, TASK_CONTINUE_ENTRY, TASK_CONTINUE_PROMPT, TASK_RESULT_ENTRY, TaskDoc, load_task_doc,
    store_task_doc,
};
use crate::durable::error::DurableError;
use crate::durable::harness::generation::GenerationTask;
use crate::durable::harness::inbox::{
    self, At, Boundary, InputEntry, SubmissionDraft, WhenBusy, WriteDraft,
};
use crate::durable::harness::live::{self, LiveState};
use crate::durable::tx::Tx;

/// 一条任务 run 怎么收尾的（交回消息的标题那一半）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Completed,
    NotCompleted,
}

/// 回答末行写的状态（模块文档的"状态行"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Done,
    Blocked,
    Unstated,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Done => "done",
            Status::Blocked => "blocked",
            Status::Unstated => "unstated",
        }
    }
}

/// 末行是不是 [`TASK_STATUS_DONE`] / [`TASK_STATUS_BLOCKED`]。模型常给它加粗、套引用或列表
/// 符号，冒号也会写成半角，这些都认；"状态：未完成"之类不认，按没写算。
fn status_of(text: &str) -> Status {
    let Some(line) = text.lines().map(str::trim).rfind(|line| !line.is_empty()) else {
        return Status::Unstated;
    };
    let line: String = line
        .chars()
        .filter(|c| !matches!(c, '*' | '`' | '_'))
        .collect();
    let line = line
        .trim_start_matches(['>', '-', '#', ' '])
        .replace(':', "：")
        .replace("： ", "：");
    if line.starts_with(TASK_STATUS_DONE) {
        Status::Done
    } else if line.starts_with(TASK_STATUS_BLOCKED) {
        Status::Blocked
    } else {
        Status::Unstated
    }
}

/// 标题里跟在结局后面的数字：工具调用轮数与用时（旧 `komo.live` 没有开始时刻就不报用时）。
fn facts(run: &live::RunControl, now: i64, continued: bool) -> String {
    let mut out = format!(" · {} 轮工具调用", run.rounds);
    if run.started_at > 0 {
        let span = time::Duration::milliseconds(now - run.started_at);
        out.push_str(&format!(" · {}", duration_words(span)));
    }
    if continued {
        out.push_str(" · 自动续跑过一次");
    }
    out
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

/// 这条 run 的输入全是监听交回的（请求键 `monitor:` / `monitor-end:`）。读 submission 不算表读。
async fn events_only(tx: &mut Tx, inputs: &[SubmissionId]) -> Result<bool, DurableError> {
    if inputs.is_empty() {
        return Ok(false);
    }
    for id in inputs {
        let key = tx
            .submission(*id)
            .await?
            .and_then(|record| record.request_id);
        let from_monitor = key
            .as_deref()
            .is_some_and(|key| super::monitor::is_monitor_key(key));
        if !from_monitor {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 回答是空的或只有 [`SILENT_REPLY`](komo_kernel::types::chat::SILENT_REPLY)。
fn is_blank_or_silent(text: &str) -> bool {
    text.trim().is_empty() || is_silent_reply(text)
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

/// 复验结论：与旧委派的父侧收尾同一份取 JSON + 校验（[`check_contract`]）。
fn contract_verdict(contract: &DelegateContract, text: &str) -> String {
    match check_contract(contract, text) {
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
    report(tx, generation, conversation, &run, settlement, false).await?;
    Ok(true)
}

/// 回答收尾用的 [`end_run`]：任务会话的回答没写状态行时不交回，同一次提交里放一条续跑输入、
/// 起后继 run（模块文档的"自动续跑"）。`idle`：这次 final boundary 没放任何用户项、收件箱也空了
/// ——否则后继 run 本来就要起（派它的会话追问过来了），续跑让给它，回答照常交回。
pub(crate) async fn end_answered_run(
    tx: &mut Tx,
    generation: &GenerationTask,
    conversation: ConversationId,
    live: &mut LiveState,
    task: TaskId,
    answer: EntryId,
    idle: bool,
) -> Result<(), DurableError> {
    let settlement = SubmissionSettlement::Done { answer };
    let Some(run) = live::end_run(tx, live, task, &settlement).await? else {
        return Ok(());
    };
    let Some(mut doc) = report(tx, generation, conversation, &run, &settlement, idle).await? else {
        return Ok(());
    };
    let now = tx.now();
    let entry = InputEntry::new(
        TASK_CONTINUE_ENTRY,
        Some(json!({ "after": run.inputs.first() })),
    );
    let id = inbox::place_idle_input(
        tx,
        generation,
        conversation,
        live,
        &Content::Text(TASK_CONTINUE_PROMPT.to_owned()),
        Some(&entry),
        None,
        now,
    )
    .await?;
    doc.continued = Some(id);
    store_task_doc(tx, conversation, &doc).await
}

/// run 结束了，往父交回结果（这不是后台任务的 conversation、父不在了、没有输入都什么都不做）。
/// `may_continue` 时没写状态行的回答不交回，返回那份 `komo.task` 交给调用方去续跑（lastRun
/// 不动、契约不取，都留给续跑之后那次收尾）。
async fn report(
    tx: &mut Tx,
    generation: &GenerationTask,
    conversation: ConversationId,
    run: &live::RunControl,
    settlement: &SubmissionSettlement,
    may_continue: bool,
) -> Result<Option<TaskDoc>, DurableError> {
    let Some(mut doc) = load_task_doc(tx, conversation).await? else {
        return Ok(None);
    };
    let Some(first) = run.inputs.first().copied() else {
        return Ok(None);
    };
    let continued = doc.continued == Some(first);
    let has_contract = doc.contracts.contains_key(&first.to_string());
    // 只由监听事件 / 结束消息起的 run（S2，`docs/background-tasks.md` §2e）：不自动续跑（事件那一轮
    // 不是派出的活，没有状态行可写）；回答是 NO_REPLY 时不交回、lastRun 也不动。
    let events_only = events_only(tx, &run.inputs).await?;
    // `idle`：这条回答之后没有后继 run（只在回答收尾时为真）。
    let idle = may_continue;
    // 已停下的任务（`docs/tasks.md` §4.14 F4）不再自动续跑。
    let may_continue = may_continue && !events_only && !doc.stopped;
    // 任务不许沉默（`docs/tasks.md` §4.9 B1）：分发派出 / 续上的任务里，由人的消息起的一轮（有一条输入
    // 不是监听交回的就算，混着事件的按用户消息算）回 `NO_REPLY` 或空，不当作完成。只由事件起的一轮不在
    // 此列，保持原样（上面的 `events_only` 静默）。
    let user_round = doc.reply.is_some() && !events_only && !has_contract;

    let (outcome, text, last) = match settlement {
        SubmissionSettlement::Done { answer } => {
            let text = tx
                .staged_entry(*answer)
                .map(answer_text)
                .unwrap_or_default();
            if events_only && is_silent_reply(&text) {
                return Ok(None);
            }
            if may_continue && !continued && !has_contract && status_of(&text) == Status::Unstated {
                return Ok(Some(doc));
            }
            let silent = user_round && is_blank_or_silent(&text);
            // 追问过一次（或没法追问）仍然是空：后面还有后继 run 就让给它回话，已停下的任务不再说话；
            // 都不是，就是真没做成，往下走直投那句"没做成"。
            if silent && (!idle || doc.stopped) {
                return Ok(None);
            }
            let last = if silent {
                LastRun {
                    state: "failed".into(),
                    ended_at: tx.now(),
                    final_message: None,
                    reason: Some("没有给出回复".into()),
                    status: None,
                    rounds: Some(run.rounds),
                }
            } else {
                LastRun {
                    state: "completed".into(),
                    ended_at: tx.now(),
                    final_message: Some(text.clone()),
                    reason: None,
                    status: Some(
                        if has_contract {
                            "done"
                        } else {
                            status_of(&text).as_str()
                        }
                        .to_owned(),
                    ),
                    rounds: Some(run.rounds),
                }
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
                status: None,
                rounds: Some(run.rounds),
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
        return Ok(None);
    };
    // 父所在的 Session 已经在删（`closing` / `deleted` / `purged`）：不再交回——交回的输入是内部
    // 输入，会在父里起后继 run，删除流程撤过一遍之后晚到的结果不能再让已删的会话动起来。
    if let Some(session) = parent_record.session.as_deref()
        && tx
            .session(session)
            .await?
            .is_some_and(|record| record.lifecycle != SessionLifecycle::Active)
    {
        return Ok(None);
    }
    // 带契约的回答只放 JSON，结局看契约复验，不看状态行。
    let status = (outcome == Outcome::Completed && !has_contract).then(|| status_of(&text));
    // 分发派出的任务：回复直接落成主线上的一条展示 entry，不交回给父起一轮转述（带契约的不走这条，
    // JSON 不外发；分发从不带契约，这只是兜住"追问时被别处塞了契约"的情形）。
    if contract.is_none()
        && let Some(reply) = doc.reply.clone()
    {
        direct_reply(
            tx,
            generation,
            Direct {
                parent,
                session: parent_record.session.clone(),
                conversation,
                first,
                title: &doc.title,
                reply: &reply,
            },
            outcome,
            &text,
        )
        .await?;
        return Ok(None);
    }
    let headline = match (outcome, status) {
        (Outcome::Completed, Some(Status::Blocked)) => "受阻",
        (Outcome::Completed, Some(Status::Unstated)) => "交回了，没说做完没有",
        (Outcome::Completed, _) => "已完成",
        (Outcome::NotCompleted, _) => "没有完成",
    };
    let facts = facts(run, tx.now(), continued);
    let verdict = match (outcome, &contract) {
        (Outcome::Completed, Some(contract)) => {
            format!("{}\n\n", contract_verdict(contract, &text))
        }
        _ => String::new(),
    };
    let input = format!(
        "[后台任务 #{handle}「{title}」{headline}{facts}]\n\n{text}\n\n{verdict}\
         （这是后台任务交回的结果，不是用户发来的消息；用户看不到上面这段，请把结论转告用户。）",
        handle = short_handle(conversation.get()),
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
                    "status": status.map(Status::as_str),
                    "rounds": run.rounds,
                }
            })),
        )),
        when_busy: WhenBusy::Steer,
        request_id: Some(format!("task-result:{conversation}:{first}")),
        // 交回的结果是内部输入：父被取消时不撤，非正常收尾后用它起后继 run。
        internal: true,
    };
    inbox::admit(tx, generation, parent, &draft, generation.modes()).await?;
    Ok(None)
}

/// [`direct_reply`] 的位置信息。
struct Direct<'a> {
    parent: ConversationId,
    /// 父 conversation 属于的 komo Session（Gateway 投递时要它）。
    session: Option<String>,
    conversation: ConversationId,
    first: SubmissionId,
    title: &'a str,
    reply: &'a super::ReplyTo,
}

/// 把回复里给程序看的末行状态行拆出来：`(正文, 状态, 受阻的原因)`。状态行不是末行（或没有）时正文原样。
fn split_status(text: &str) -> (String, Status, Option<String>) {
    let status = status_of(text);
    if status == Status::Unstated {
        return (text.trim().to_owned(), status, None);
    }
    let lines: Vec<&str> = text.lines().collect();
    let last = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .unwrap_or(0);
    let body = lines[..last].join("\n").trim().to_owned();
    // 与 `status_of` 同一套归一：去加粗 / 引用符号，冒号统一成全角。
    let line: String = lines[last]
        .chars()
        .filter(|c| !matches!(c, '*' | '`' | '_'))
        .collect();
    let line = line
        .trim()
        .trim_start_matches(['>', '-', '#', ' '])
        .replace(':', "：")
        .replace("： ", "：");
    let reason = line
        .strip_prefix(TASK_STATUS_BLOCKED)
        .map(|rest| rest.trim_start_matches('：').trim().to_owned())
        .filter(|reason| !reason.is_empty());
    (body, status, reason)
}

/// 直接投给用户的那句话（`docs/tasks.md` §4.12 U3）：去掉状态行，不加前缀、不带任务号。
/// 受阻时原因是一句普通的话；失败是一句自然的话，不带状态码；被取消的什么都不说（"停下"那句话由分发
/// 在主线上落，`docs/tasks.md` §4.14 F4）。
fn direct_text(outcome: Outcome, text: &str) -> Option<String> {
    match outcome {
        Outcome::NotCompleted => {
            (text != failure_text("aborted", None)).then(|| format!("这件事我没做成：{text}。"))
        }
        // 由人的消息起的一轮没有给出任何话（追问过一次仍空，见 `report`）：一句普通的话说明没做成，
        // 不带状态码。
        Outcome::Completed if is_blank_or_silent(text) => {
            Some("这件事我没做成，要不要再试一次？".to_owned())
        }
        Outcome::Completed => {
            let (body, status, reason) = split_status(text);
            Some(match (status, reason) {
                (Status::Blocked, Some(reason)) if body.is_empty() => reason,
                // 正文里多半已经写了原因，重复一遍只会啰嗦。
                (Status::Blocked, Some(reason)) if !body.contains(&reason) => {
                    format!("{body}\n\n{reason}")
                }
                _ if body.is_empty() => match status {
                    Status::Blocked => "这件事我没能做下去。".to_owned(),
                    _ => "做完了。".to_owned(),
                },
                _ => body,
            })
        }
    }
}

/// 分发派出的任务的回复：同一次提交里往父（主会话）受理一条展示 write（请求键
/// `task-reply:{子}:{首个输入}`，与交回同样原子、幂等）。**不是内部输入**：没有 `model`，父不起 run。
/// 渠道投递是 Gateway 的事，它读这条 entry 的 `data.peer`（先写 `deliveries` 后发，按 entry 去重）。
async fn direct_reply(
    tx: &mut Tx,
    generation: &GenerationTask,
    at: Direct<'_>,
    outcome: Outcome,
    text: &str,
) -> Result<(), DurableError> {
    let Some(said) = direct_text(outcome, text) else {
        return Ok(());
    };
    let data = json!({
        "text": said,
        "peer": at.reply.peer,
        "session": at.session,
        "origin": {
            "entry": at.reply.origin_entry,
            "submission": at.reply.origin_submission,
        },
        "task": {
            "conversation": at.conversation,
            "handle": short_handle(at.conversation.get()),
            "title": at.title,
        },
    });
    let draft = SubmissionDraft::Write {
        entry: WriteDraft::display(super::MAIN_REPLY_ENTRY, data),
        request_id: Some(format!("task-reply:{}:{}", at.conversation, at.first)),
    };
    inbox::admit(tx, generation, at.parent, &draft, generation.modes()).await?;
    Ok(())
}

/// 从最后一条回复里取 JSON 并按契约验一遍：`task` 交回之前的复验（`orchestration::report`）用它。
///
/// 校验器来自 kernel，**两头共用同一份代码**（§8.6）：两份实现会漂移，漂移的症状是
/// "子代理说成功、父侧说结果不合规"，而判断只能有一个出处。取 JSON 那一步与记忆提取
/// 共用（两边都容忍围栏与前后闲话），理由相同。
pub fn check_contract(
    contract: &DelegateContract,
    text: &str,
) -> Result<(serde_json::Value, Validation), String> {
    let Some(json) = komo_llm::parse_json_object(text) else {
        return Err("最后一条回复里找不到 JSON 对象".into());
    };
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| format!("最后一条回复里的 JSON 解不开：{error}"))?;
    let validation = validate(contract, &value);
    if validation.is_valid() {
        Ok((value, validation))
    } else {
        Err(validation.describe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_line_is_read_from_the_last_non_empty_line() {
        assert_eq!(
            status_of(&format!("改完了，测试全过。\n\n{TASK_STATUS_DONE}\n")),
            Status::Done
        );
        assert_eq!(
            status_of(&format!("{TASK_STATUS_BLOCKED}：方案的前提不成立")),
            Status::Blocked
        );
        // 加粗、引用、半角冒号都认。
        assert_eq!(status_of("结论……\n**状态**: 完成"), Status::Done);
        assert_eq!(status_of("> 状态：受阻：缺权限"), Status::Blocked);
    }

    /// 直投的回复去掉状态行、不加前缀；受阻的原因是一句普通的话；被取消的不说话。
    #[test]
    fn a_direct_reply_drops_the_status_line_and_speaks_plainly() {
        let said = |outcome, text: &str| direct_text(outcome, text);
        assert_eq!(
            said(
                Outcome::Completed,
                &format!("周报整理好了。\n\n{TASK_STATUS_DONE}")
            ),
            Some("周报整理好了。".into())
        );
        // 受阻：正文在前，原因补一句；原因正文里已有就不重复；没有正文就只有原因。
        assert_eq!(
            said(
                Outcome::Completed,
                &format!("试了两种办法。\n{TASK_STATUS_BLOCKED}：缺少仓库权限")
            ),
            Some("试了两种办法。\n\n缺少仓库权限".into())
        );
        assert_eq!(
            said(
                Outcome::Completed,
                &format!("我没有仓库权限，做不下去。\n{TASK_STATUS_BLOCKED}：没有仓库权限")
            ),
            Some("我没有仓库权限，做不下去。".into())
        );
        assert_eq!(
            said(
                Outcome::Completed,
                &format!("{TASK_STATUS_BLOCKED}：要你确认日期")
            ),
            Some("要你确认日期".into())
        );
        // 没写状态行：原样。
        assert_eq!(said(Outcome::Completed, "好的"), Some("好的".into()));
        assert_eq!(said(Outcome::NotCompleted, "已取消"), None);
        assert_eq!(
            said(Outcome::NotCompleted, "模型调用失败：503"),
            Some("这件事我没做成：模型调用失败：503。".into())
        );
    }

    /// fork 任务的说明教的写法就是这里认的写法（fresh 任务的在 komo-agent 的测试里核对）。
    #[test]
    fn the_fork_notice_teaches_both_status_lines() {
        for status in [TASK_STATUS_DONE, TASK_STATUS_BLOCKED] {
            assert!(super::super::FORK_NOTICE.contains(status), "{status}");
        }
    }

    #[test]
    fn a_missing_or_unknown_status_counts_as_unstated() {
        assert_eq!(status_of(""), Status::Unstated);
        assert_eq!(status_of("改了一半，还没跑测试。"), Status::Unstated);
        assert_eq!(status_of("状态：未完成"), Status::Unstated);
        // 状态行不在末行不算：后面又写了东西，说明它不是收尾。
        assert_eq!(status_of("状态：完成\n还有一件事没做"), Status::Unstated);
    }

    fn contract(mode: komo_kernel::types::delegate::SchemaMode) -> DelegateContract {
        DelegateContract {
            schema: json!({
                "type": "object",
                "required": ["count"],
                "properties": { "count": { "type": "integer" } }
            }),
            mode,
        }
    }

    /// 契约复验：围栏与前后闲话都容忍，不合规的报出原因。
    #[test]
    fn check_contract_takes_the_json_out_of_the_reply_and_validates_it() {
        let contract = contract(komo_kernel::types::delegate::SchemaMode::Strict);
        let (value, validation) =
            check_contract(&contract, "数完了：\n```json\n{\"count\": 3}\n```\n以上").unwrap();
        assert_eq!(value["count"], json!(3));
        assert!(validation.is_valid());

        assert!(check_contract(&contract, "没有 JSON").is_err());
        assert!(check_contract(&contract, "{\"files\": []}").is_err());
    }
}
